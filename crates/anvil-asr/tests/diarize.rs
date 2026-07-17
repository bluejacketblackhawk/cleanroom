//! End-to-end diarization against the real sherpa-onnx sidecar, plus the **DER quality gate**.
//!
//! `handoff/06-QUALITY-EVAL.md` §2 sets the bar at **DER ≤ 20%**, measured on an AMI subset.
//! AMI is not redistributable and is not on this machine, so the offline gate runs against a
//! *synthetic* 3-speaker conversation whose ground truth is exact by construction — see
//! `tests/fixtures/make-synthetic-diarization-fixture.ps1`, which renders each turn with a
//! different Windows TTS voice (Microsoft David and Mark, both male, plus Zira) and writes the
//! matching RTTM. **This is a synthetic corpus, not AMI**; the number it produces is reported
//! as such and is not a claim about AMI.
//!
//! Everything here is **gated on the environment** so `cargo test` stays green (and offline) on
//! a machine with no sidecar and no models. The gate runs only when all of these point at real
//! files:
//! - `CLEANROOM_DIARIZE`             — `sherpa-onnx-offline-speaker-diarization`(`.exe`),
//! - `CLEANROOM_DIARIZE_SEG_MODEL`   — the pyannote segmentation `.onnx`,
//! - `CLEANROOM_DIARIZE_EMB_MODEL`   — the speaker-embedding `.onnx`,
//! - `CLEANROOM_DIAR_TEST_AUDIO`     — the fixture `.wav`,
//! - `CLEANROOM_DIAR_TEST_RTTM`      — its ground-truth `.rttm`.
//!
//! The DER metric itself is pure and is unit-tested below without any of that.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anvil_asr::{
    assign_speakers, diarize, Diarization, DiarizeOptions, DiarizeSidecar, Segment, Transcript,
    Word,
};

fn env_file(key: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(key)?);
    path.is_file().then_some(path)
}

// --- ground truth + DER --------------------------------------------------------------------

/// One reference turn read from an RTTM file.
#[derive(Debug, Clone)]
struct RefTurn {
    speaker: String,
    start: f64,
    end: f64,
}

/// Parse NIST RTTM: `SPEAKER <file> <chan> <start> <dur> <NA> <NA> <speaker> <NA> <NA>`.
fn parse_rttm(text: &str) -> Vec<RefTurn> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[0] != "SPEAKER" {
            continue;
        }
        let (Ok(start), Ok(dur)) = (f[3].parse::<f64>(), f[4].parse::<f64>()) else {
            continue;
        };
        out.push(RefTurn {
            speaker: f[7].to_string(),
            start,
            end: start + dur,
        });
    }
    out
}

/// Diarization Error Rate, the standard NIST definition:
///
/// ```text
/// DER = (missed speech + false alarm + speaker confusion) / total reference speech
/// ```
///
/// Computed on a fine time grid (10 ms, matching pyannote.metrics' resolution) rather than
/// analytically, because the fixture has no overlapped speech and a grid is far easier to
/// verify by eye than an interval-algebra implementation of the same thing.
///
/// The hypothesis's speaker ids are arbitrary, so we take the **best** mapping from hypothesis
/// ids to reference names — brute-forced over all permutations, which is what the Hungarian
/// algorithm would find, and is trivially cheap for the ≤ 8 speakers a podcast has.
///
/// `collar` (seconds) is the NIST/DIHARD forgiveness zone around every reference **boundary**:
/// frames within `±collar` of a turn's start or end are **excluded from scoring altogether** —
/// dropped from both the error count and the denominator. That is the part people get wrong.
/// Merely trimming the reference turn instead would leave the hypothesis still speaking in the
/// collar with the reference now silent, and charge the model a *false alarm* for the very
/// jitter the collar exists to forgive. 250 ms is the convention; pass `0.0` for the
/// unforgiving number, which this test also reports.
fn der(reference: &[RefTurn], hyp: &Diarization, collar: f64, grid: f64) -> f64 {
    let end = reference
        .iter()
        .map(|t| t.end)
        .chain(hyp.segments.iter().map(|s| s.end))
        .fold(0.0f64, f64::max);
    let frames = (end / grid).ceil() as usize + 1;

    // Reference names → dense indices.
    let mut names: Vec<&str> = reference.iter().map(|t| t.speaker.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    // ref_at[i] / hyp_at[i]: who is talking in frame i (None = silence). The fixture has no
    // overlap, so one speaker per frame is a faithful model of it.
    let mut ref_at: Vec<Option<usize>> = vec![None; frames];
    let mut hyp_at: Vec<Option<usize>> = vec![None; frames];
    // scored[i]: is frame i inside the forgiveness zone around a reference boundary?
    let mut scored: Vec<bool> = vec![true; frames];

    /// Stamp `who` across the frames covered by `[start, end)`.
    fn fill(track: &mut [Option<usize>], start: f64, end: f64, grid: f64, who: usize) {
        let lo = (start / grid).ceil().max(0.0) as usize;
        let hi = ((end / grid).floor().max(0.0) as usize).min(track.len());
        for slot in track.iter_mut().take(hi).skip(lo) {
            *slot = Some(who);
        }
    }

    /// Blank out `[start, end)` from scoring.
    fn exclude(scored: &mut [bool], start: f64, end: f64, grid: f64) {
        let lo = (start.max(0.0) / grid).floor() as usize;
        let hi = ((end.max(0.0) / grid).ceil() as usize).min(scored.len());
        for slot in scored.iter_mut().take(hi).skip(lo) {
            *slot = false;
        }
    }

    for turn in reference {
        let idx = names.iter().position(|n| *n == turn.speaker).expect("name");
        fill(&mut ref_at, turn.start, turn.end, grid, idx);
        if collar > 0.0 {
            exclude(&mut scored, turn.start - collar, turn.start + collar, grid);
            exclude(&mut scored, turn.end - collar, turn.end + collar, grid);
        }
    }
    for seg in &hyp.segments {
        fill(&mut hyp_at, seg.start, seg.end, grid, seg.speaker as usize);
    }

    let total_ref = (0..frames)
        .filter(|&f| scored[f] && ref_at[f].is_some())
        .count();
    if total_ref == 0 {
        return 0.0;
    }

    let num_hyp = hyp.speakers.len();
    let best_errors = permutations(num_hyp, names.len())
        .into_iter()
        .map(|map| {
            // map[h] = the reference index hypothesis speaker `h` is claimed to be.
            (0..frames)
                .filter(|&f| scored[f])
                .filter(|&f| {
                    let h = hyp_at[f].and_then(|h| map.get(h).copied()).flatten();
                    match (ref_at[f], h) {
                        (None, None) => false,        // agreed silence
                        (None, Some(_)) => true,      // false alarm
                        (Some(_), None) => true,      // missed speech
                        (Some(r), Some(h)) => r != h, // confusion (or not)
                    }
                })
                .count()
        })
        .min()
        // No hypothesis speakers at all: everything is missed speech.
        .unwrap_or(total_ref);

    best_errors as f64 / total_ref as f64
}

/// Every injective mapping of `n_hyp` hypothesis speakers onto `n_ref` reference speakers
/// (`None` where a hypothesis speaker maps to nobody, which happens when the model invents an
/// extra speaker). Brute force: podcasts have a handful of speakers, not hundreds.
fn permutations(n_hyp: usize, n_ref: usize) -> Vec<Vec<Option<usize>>> {
    fn go(
        pos: usize,
        n_hyp: usize,
        n_ref: usize,
        used: &mut Vec<bool>,
        cur: &mut Vec<Option<usize>>,
        out: &mut Vec<Vec<Option<usize>>>,
    ) {
        if pos == n_hyp {
            out.push(cur.clone());
            return;
        }
        for r in 0..n_ref {
            if !used[r] {
                used[r] = true;
                cur.push(Some(r));
                go(pos + 1, n_hyp, n_ref, used, cur, out);
                cur.pop();
                used[r] = false;
            }
        }
        // This hypothesis speaker maps to nobody — a spurious speaker.
        cur.push(None);
        go(pos + 1, n_hyp, n_ref, used, cur, out);
        cur.pop();
    }
    let mut out = Vec::new();
    go(
        0,
        n_hyp,
        n_ref,
        &mut vec![false; n_ref],
        &mut Vec::new(),
        &mut out,
    );
    out
}

// --- the gate ------------------------------------------------------------------------------

/// The DER quality gate (06-QUALITY-EVAL.md §2: **≤ 20%**), on the synthetic 3-speaker fixture.
#[test]
fn der_meets_the_quality_gate() {
    let (Some(sidecar), Some(seg), Some(emb), Some(audio), Some(rttm)) = (
        env_file("CLEANROOM_DIARIZE"),
        env_file("CLEANROOM_DIARIZE_SEG_MODEL"),
        env_file("CLEANROOM_DIARIZE_EMB_MODEL"),
        env_file("CLEANROOM_DIAR_TEST_AUDIO"),
        env_file("CLEANROOM_DIAR_TEST_RTTM"),
    ) else {
        eprintln!(
            "skipping DER gate: set CLEANROOM_DIARIZE, CLEANROOM_DIARIZE_SEG_MODEL, \
             CLEANROOM_DIARIZE_EMB_MODEL, CLEANROOM_DIAR_TEST_AUDIO and CLEANROOM_DIAR_TEST_RTTM \
             (see tests/fixtures/make-synthetic-diarization-fixture.ps1)"
        );
        return;
    };

    // The locator must resolve the same binary CLEANROOM_DIARIZE names.
    let located = DiarizeSidecar::locate().expect("locate sidecar via CLEANROOM_DIARIZE");
    assert_eq!(located.binary(), sidecar.as_path());

    let reference = parse_rttm(&std::fs::read_to_string(&rttm).expect("read rttm"));
    assert!(!reference.is_empty(), "ground truth must not be empty");
    let expected_speakers = reference
        .iter()
        .map(|t| t.speaker.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    let opts = DiarizeOptions {
        // Auto-detect: the harder, honest setting. `num_speakers: Some(n)` would be the easy
        // mode, and a podcast host does usually know how many people are in the room — but a
        // gate that only passes when you are told the answer is not much of a gate.
        num_speakers: None,
        segmentation_model: Some(seg),
        embedding_model: Some(emb),
        threads: Some(4),
        ..Default::default()
    };
    let result = diarize(&audio, &opts).expect("diarize should succeed");

    assert!(!result.speakers.is_empty(), "expected at least one speaker");
    assert!(!result.segments.is_empty(), "expected at least one turn");

    // Contract invariants: dense ids, sorted turns, non-negative spans.
    for (i, s) in result.speakers.iter().enumerate() {
        assert_eq!(s.id, i as u32, "speaker ids must be dense and in order");
        assert_eq!(s.label, format!("Speaker {}", i + 1));
    }
    for seg in &result.segments {
        assert!(seg.end >= seg.start, "{seg:?}");
        assert!(
            (seg.speaker as usize) < result.speakers.len(),
            "turn references an unknown speaker: {seg:?}"
        );
    }
    for pair in result.segments.windows(2) {
        assert!(pair[1].start >= pair[0].start, "turns must be time-ordered");
    }

    let der_collar = der(&reference, &result, 0.25, 0.01);
    let der_strict = der(&reference, &result, 0.0, 0.01);

    let mut talk: BTreeMap<u32, f64> = BTreeMap::new();
    for s in &result.speakers {
        talk.insert(s.id, result.speaking_time(s.id));
    }

    eprintln!("--- DER gate (synthetic 3-speaker TTS fixture, NOT AMI) ---");
    eprintln!(
        "  reference : {expected_speakers} speakers, {} turns",
        reference.len()
    );
    eprintln!(
        "  hypothesis: {} speakers, {} turns",
        result.speakers.len(),
        result.segments.len()
    );
    eprintln!("  talk time : {talk:?}");
    eprintln!("  DER (250 ms collar): {:.2}%", der_collar * 100.0);
    eprintln!("  DER (no collar)    : {:.2}%", der_strict * 100.0);

    assert!(
        der_collar <= 0.20,
        "DER {:.2}% exceeds the 20% gate (06-QUALITY-EVAL.md §2)",
        der_collar * 100.0
    );
}

/// `assign_speakers` against a real diarization: every word inside a speaker turn must come out
/// with a speaker, and the fixture's turns are long enough that essentially all of them do.
#[test]
fn real_assign_speakers_when_sidecar_available() {
    let (Some(_), Some(seg), Some(emb), Some(audio), Some(rttm)) = (
        env_file("CLEANROOM_DIARIZE"),
        env_file("CLEANROOM_DIARIZE_SEG_MODEL"),
        env_file("CLEANROOM_DIARIZE_EMB_MODEL"),
        env_file("CLEANROOM_DIAR_TEST_AUDIO"),
        env_file("CLEANROOM_DIAR_TEST_RTTM"),
    ) else {
        eprintln!("skipping assign_speakers e2e: diarization env not set");
        return;
    };

    let opts = DiarizeOptions {
        segmentation_model: Some(seg),
        embedding_model: Some(emb),
        threads: Some(4),
        ..Default::default()
    };
    let result = diarize(&audio, &opts).expect("diarize");

    // Rather than pay for a whisper run here (that is `tests/transcribe.rs`'s job), synthesise
    // a word stream from the ground truth: one word per second of every reference turn. The
    // point of this test is the *merge*, not the ASR.
    let reference = parse_rttm(&std::fs::read_to_string(&rttm).expect("read rttm"));
    let mut words = Vec::new();
    for turn in &reference {
        let mut t = turn.start;
        while t + 0.5 < turn.end {
            words.push(Word {
                text: "word".into(),
                start: t,
                end: t + 0.5,
                confidence: 0.9,
                speaker: None,
            });
            t += 1.0;
        }
    }
    let segments: Vec<Segment> = reference
        .iter()
        .map(|t| Segment {
            text: "turn".into(),
            start: t.start,
            end: t.end,
            speaker: None,
        })
        .collect();

    let mut transcript = Transcript {
        language: "en".into(),
        words,
        segments,
    };
    let total_words = transcript.words.len();
    assign_speakers(&mut transcript, &result);

    let labelled = transcript
        .words
        .iter()
        .filter(|w| w.speaker.is_some())
        .count();
    eprintln!("assigned speakers to {labelled}/{total_words} words");
    assert!(
        labelled * 100 / total_words >= 95,
        "expected ≥95% of in-turn words to get a speaker, got {labelled}/{total_words}"
    );

    // Every reference turn maps to exactly one *hypothesis* speaker if diarization is any good,
    // so the segment-level dominant-speaker vote must produce a stable label per turn.
    let unlabelled = transcript
        .segments
        .iter()
        .filter(|s| s.speaker.is_none())
        .count();
    assert_eq!(
        unlabelled, 0,
        "every reference turn should get a dominant speaker"
    );

    for w in &transcript.words {
        if let Some(id) = w.speaker {
            assert!(
                (id as usize) < result.speakers.len(),
                "unknown speaker id {id}"
            );
        }
    }
}

// --- DER metric unit tests (no models, no sidecar, always run) ------------------------------

fn diar(turns: &[(u32, f64, f64)]) -> Diarization {
    let text: String = turns
        .iter()
        .map(|(s, a, b)| format!("{a:.3} -- {b:.3} speaker_{s:02}\n"))
        .collect();
    anvil_asr::parse_diarization_output(&text).expect("parse")
}

#[test]
fn der_of_a_perfect_hypothesis_is_zero() {
    let reference = vec![
        RefTurn {
            speaker: "A".into(),
            start: 0.0,
            end: 10.0,
        },
        RefTurn {
            speaker: "B".into(),
            start: 10.0,
            end: 20.0,
        },
    ];
    // Same turns, arbitrary (and here, reversed-looking) cluster ids.
    let hyp = diar(&[(0, 0.0, 10.0), (1, 10.0, 20.0)]);
    assert!(der(&reference, &hyp, 0.0, 0.01) < 1e-9);
}

#[test]
fn der_finds_the_best_speaker_mapping() {
    let reference = vec![
        RefTurn {
            speaker: "A".into(),
            start: 0.0,
            end: 10.0,
        },
        RefTurn {
            speaker: "B".into(),
            start: 10.0,
            end: 20.0,
        },
    ];
    // Correct turns, but the ids are swapped relative to alphabetical order. A DER that did
    // not search the mapping would call this 100% confusion.
    let hyp = diar(&[(1, 0.0, 10.0), (0, 10.0, 20.0)]);
    assert!(der(&reference, &hyp, 0.0, 0.01) < 1e-9);
}

#[test]
fn der_charges_confusion_missed_speech_and_false_alarm() {
    let reference = vec![
        RefTurn {
            speaker: "A".into(),
            start: 0.0,
            end: 10.0,
        },
        RefTurn {
            speaker: "B".into(),
            start: 10.0,
            end: 20.0,
        },
    ];

    // All of B's 10 s attributed to A: 10 s confused out of 20 s of reference speech.
    let confused = diar(&[(0, 0.0, 20.0)]);
    let d = der(&reference, &confused, 0.0, 0.01);
    assert!((d - 0.5).abs() < 0.02, "expected ~50% confusion, got {d}");

    // Only the first half found: 10 s missed out of 20 s.
    let missed = diar(&[(0, 0.0, 10.0)]);
    let d = der(&reference, &missed, 0.0, 0.01);
    assert!((d - 0.5).abs() < 0.02, "expected ~50% missed, got {d}");

    // Correct, plus 10 s of speech invented in silence: 10 s false alarm over 20 s reference.
    let false_alarm = diar(&[(0, 0.0, 10.0), (1, 10.0, 20.0), (0, 25.0, 35.0)]);
    let d = der(&reference, &false_alarm, 0.0, 0.01);
    assert!((d - 0.5).abs() < 0.02, "expected ~50% false alarm, got {d}");
}

#[test]
fn der_collar_forgives_boundary_jitter() {
    let reference = vec![
        RefTurn {
            speaker: "A".into(),
            start: 0.0,
            end: 10.0,
        },
        RefTurn {
            speaker: "B".into(),
            start: 10.0,
            end: 20.0,
        },
    ];
    // The boundary is 200 ms late — inside the 250 ms collar, so it should score clean.
    let jittery = diar(&[(0, 0.0, 10.2), (1, 10.2, 20.0)]);
    assert!(der(&reference, &jittery, 0.25, 0.01) < 1e-9);
    // Without a collar the same jitter is charged as confusion.
    assert!(der(&reference, &jittery, 0.0, 0.01) > 0.0);
}

#[test]
fn rttm_parses_the_fixture_format() {
    let text = "SPEAKER diarization-3spk 1 0.000 9.375 <NA> <NA> Microsoft_David <NA> <NA>\n\
                SPEAKER diarization-3spk 1 9.775 9.425 <NA> <NA> Microsoft_Zira <NA> <NA>\n\
                # a comment line that must be ignored\n";
    let turns = parse_rttm(text);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].speaker, "Microsoft_David");
    assert!((turns[0].end - 9.375).abs() < 1e-9);
    assert!((turns[1].start - 9.775).abs() < 1e-9);
    assert!((turns[1].end - 19.2).abs() < 1e-9);
}
