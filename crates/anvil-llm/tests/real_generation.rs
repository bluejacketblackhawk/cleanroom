//! End-to-end generation against a **real** llama.cpp + Qwen2.5 gguf.
//!
//! Gated on the environment so `cargo test` stays green (and offline, and fast) on machines
//! without the model pack — which is every CI runner and most dev machines, since the pack is
//! a 4.7 GB optional download. It runs only when both are set and point at real files:
//! - `ANVIL_LLAMA`     — path to `llama-cli`(`.exe`) from a recent llama.cpp build,
//! - `ANVIL_LLM_MODEL` — path to a Qwen2.5-Instruct `.gguf`.
//!
//! When either is missing the test prints a skip note and passes. This mirrors
//! `anvil-asr/tests/transcribe.rs`.
//!
//! What it checks is the rubric ([`anvil_llm::rubric`]), not the prose: on a canned
//! three-topic transcript the model must produce structurally publishable show notes —
//! valid JSON, a summary of sane length, real bullets, and chapters that are strictly
//! increasing, snapped to segment boundaries and inside the episode. Whether the prose is any
//! *good* is the human half of the rubric, run on the 20-episode set.

use std::path::PathBuf;

use anvil_llm::rubric::{self, RubricContext};
use anvil_llm::{generate, GenerateOptions, TranscriptInput, TranscriptSegment, PROMPT_VERSION};

fn env_file(key: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(key)?);
    path.is_file().then_some(path)
}

#[test]
fn real_shownotes_when_the_model_pack_is_installed() {
    let (Some(llama), Some(model)) = (env_file("ANVIL_LLAMA"), env_file("ANVIL_LLM_MODEL")) else {
        eprintln!(
            "skipping real generation: set ANVIL_LLAMA (llama-cli) and ANVIL_LLM_MODEL (a \
             Qwen2.5-Instruct gguf), both pointing at real files, to run it"
        );
        return;
    };
    eprintln!(
        "generating with {} + {} (prompts {PROMPT_VERSION})",
        llama.display(),
        model.display()
    );

    let input = canned_episode();
    let opts = GenerateOptions {
        model: Some(model),
        ..Default::default()
    };

    let started = std::time::Instant::now();
    let notes = generate(&input, &opts).expect("generation should succeed");
    eprintln!(
        "generated in {:.1} s: {notes:#?}",
        started.elapsed().as_secs_f64()
    );

    let report = rubric::score(&notes, &RubricContext::from_input(&input));
    assert!(
        report.usable,
        "the model's show notes are structurally unusable ({:.2}): {:?}\n{report:#?}",
        report.score,
        report.failures()
    );

    // The contract the chapter writer depends on, restated here so a regression in the
    // sanitizer cannot hide behind a passing rubric.
    let boundaries: Vec<f64> = input.segments.iter().map(|s| s.start).collect();
    for pair in notes.chapters.windows(2) {
        assert!(pair[1].start > pair[0].start, "chapters must increase");
    }
    for chapter in &notes.chapters {
        assert!(
            boundaries.contains(&chapter.start),
            "chapter at {} is not on a segment boundary",
            chapter.start
        );
        assert!(chapter.start <= input.duration_secs());
    }
}

/// A ~25-minute, three-topic conversation (microphones → the room → editing). Canned rather
/// than transcribed so the test needs no audio and no whisper: this lane's input is a
/// transcript, and where the transcript came from is not its problem.
fn canned_episode() -> TranscriptInput {
    let lines = [
        "Welcome back to the show, today we are finally doing the episode about recording gear that everybody keeps asking for.",
        "Let's start with microphones, because that is where most people spend money they did not need to spend.",
        "A dynamic microphone rejects most of the room around it, which is exactly what you want in an untreated spare bedroom.",
        "A condenser microphone hears everything, including the fridge, the street and the reflection off the desk.",
        "So the expensive condenser everyone recommends online is often the worst possible choice for a first podcast setup.",
        "Microphone technique matters more than the price tag anyway: get close, stay off axis, and stop shouting.",
        "If you are two inches from a dynamic microphone, the room is thirty decibels quieter than your voice and nobody hears it.",
        "The preamp matters too, but only in the sense that a noisy cheap one adds hiss you can never remove afterwards.",
        "Right, so that is microphones. The other half of the problem is the room itself, and no microphone fixes a bad room.",
        "The room is what makes home recordings sound like home recordings: it is the reflections, not the noise.",
        "Every hard surface sends a copy of your voice back at the microphone a few milliseconds late, and that smears the sound.",
        "That is why an empty square bedroom sounds boxy, and why the same voice in a wardrobe full of coats sounds fine.",
        "Treat the wall behind you first, then the wall in front of you, then the ceiling above the desk if you still care.",
        "Acoustic foam is mostly decoration: it is too thin to absorb anything below a couple of kilohertz.",
        "Thick panels, a bookshelf, a heavy curtain, a rug, a sofa. Mass and depth, not egg boxes and foam tiles.",
        "You can measure the reverb tail with a clap and a free analyzer, and you will be depressed by the result.",
        "Okay, so we have a microphone and we have a room that no longer fights us. The last part is the editing.",
        "Editing is where a good conversation becomes a good episode, and where most people waste their entire weekend.",
        "Modern editing software shows you the transcript as text, so you cut a sentence by deleting the words.",
        "Cutting filler words from the transcript view is ten times faster than hunting for them in the waveform.",
        "Do not over-edit though: strip every breath and every pause and the conversation stops sounding like people.",
        "Render the export once the cuts are approved, and keep the project file, because you will want that audio again.",
        "Back up the project before the final render, because the one time you do not is the time the drive dies.",
        "That is the episode: a dynamic microphone, a treated wall, and an editor that works on words instead of waveforms.",
    ];
    TranscriptInput {
        language: "en".into(),
        segments: lines
            .iter()
            .enumerate()
            .map(|(i, text)| TranscriptSegment {
                text: (*text).to_string(),
                start: i as f64 * 65.0,
                end: i as f64 * 65.0 + 60.0,
            })
            .collect(),
    }
}
