//! End-to-end integration test for the public API (`analyze` / `master`) over a real file:
//! synthesize a noisy stereo WAV, decode it through `anvil-media`, and run the whole chain.

use std::path::PathBuf;

use anvil_dsp::{analyze, master};
use anvil_project::{Preset, Tier};

/// Write a noisy 4 s stereo sine to a temp WAV and return the path. Deterministic noise (LCG)
/// so the fixture is reproducible.
fn write_fixture() -> PathBuf {
    let sr = 48_000u32;
    let secs = 4;
    let n = sr as usize * secs;
    let mut seed = 0x1357_9BDFu32;

    // Unique per call, not just per process: cargo runs the tests in this file in parallel and
    // each one deletes its fixture when it finishes. Keying the path on the pid alone made both
    // tests share one file, so whoever finished first deleted it out from under the other —
    // a flaky "master() returned Err" that only shows up under the right interleaving.
    static FIXTURE_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "anvil_dsp_fixture_{}_{seq}.wav",
        std::process::id()
    ));

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for i in 0..n {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        let tone = (i as f32 * 200.0 * std::f32::consts::TAU / sr as f32).sin();
        let s = 0.2 * tone + 0.06 * noise;
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer.write_sample(v).unwrap();
        writer.write_sample(v).unwrap();
    }
    writer.finalize().unwrap();
    path
}

#[test]
fn analyze_and_master_over_a_real_file() {
    let path = write_fixture();

    // Analyze (streaming decode path).
    let report = analyze(&path).unwrap();
    assert_eq!(report.channels, 2);
    assert_eq!(report.chain_version, anvil_core::CHAIN_VERSION);
    assert!(report.integrated_lufs.is_finite());
    assert!((report.duration_secs - 4.0).abs() < 0.1);

    // Master and check the two hard gates: loudness within ±0.5 LU, TP ≤ ceiling.
    let preset = Preset::default(); // −16 LUFS, −1.0 dBTP
    let result = master(&path, &preset, Tier::Standard).unwrap();
    assert!(
        (result.report.after.integrated_lufs - preset.target_lufs as f64).abs() <= 0.5,
        "integrated {} not within 0.5 LU of {}",
        result.report.after.integrated_lufs,
        preset.target_lufs
    );
    assert!(
        result.report.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01,
        "true peak {} exceeded ceiling {}",
        result.report.after.true_peak_dbtp,
        preset.true_peak_ceiling_dbtp
    );

    // Report shape / contract. The M3 chain added the speech-repair modules (de-hum, mouth
    // de-click, breath, de-esser, AutoEQ); M4 adds de-clip and de-crackle, plus the per-speaker
    // leveling stage (§4.8) that sits between AutoEQ and the leveler — all in chain order.
    assert_eq!(result.report.chain_version, anvil_core::CHAIN_VERSION);
    assert_eq!(result.report.tier, "standard");
    let module_names: Vec<&str> = result
        .report
        .modules
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        module_names,
        vec![
            "dc_hpf",
            "dehum",
            "declip",
            "declick",
            "decrackle",
            "denoise",
            "breath",
            "deess",
            "autoeq",
            "speaker",
            "leveler",
            "loudness",
            "limiter",
            "dither",
        ],
        "modules must be reported in chain order (03 §3)"
    );
    // This file was not diarized, so the per-speaker stage must report itself as skipped — the
    // single-speaker path is exactly what it always was.
    assert!(result
        .report
        .modules
        .iter()
        .any(|m| m.name == "speaker" && !m.engaged));
    assert!(result
        .report
        .modules
        .iter()
        .any(|m| m.name == "limiter" && m.engaged));
    assert!(!result.report.health_card.is_empty());

    // The MasterReport JSON must carry the contract keys.
    let v = serde_json::to_value(&result.report).unwrap();
    for key in [
        "analysis",
        "before",
        "after",
        "preset",
        "tier",
        "chain_version",
        "modules",
        "health_card",
    ] {
        assert!(v.get(key).is_some(), "MasterReport missing key {key}");
    }
    let m0 = &v["modules"][0];
    for key in ["name", "engaged", "strength", "detail"] {
        assert!(m0.get(key).is_some(), "module entry missing {key}");
    }
    let h0 = &v["health_card"][0];
    for key in ["severity", "title", "detail", "fix"] {
        assert!(h0.get(key).is_some(), "health_card entry missing {key}");
    }

    let _ = std::fs::remove_file(&path);
}

/// Rough real-time-factor probe (run explicitly: `cargo test --release -- --ignored
/// --nocapture rtf`). Not a gate — just a number for the perf budget (06 §4).
#[test]
#[ignore]
fn rtf_probe() {
    // 60 s stereo fixture.
    let sr = 48_000usize;
    let n = sr * 60;
    let mut seed = 0x2468_ACE0u32;
    let s: Vec<f32> = (0..n)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            0.2 * (i as f32 * 200.0 * std::f32::consts::TAU / sr as f32).sin() + 0.05 * noise
        })
        .collect();
    let buf = anvil_media::AudioBuffer::from_planar(vec![s.clone(), s], sr as u32);
    let audio_secs = 60.0;

    let t0 = std::time::Instant::now();
    let report = anvil_dsp::analyze_buffer(&buf);
    let analyze_secs = t0.elapsed().as_secs_f64();

    let cfg = anvil_dsp::auto_configure(&report, &Preset::default(), Tier::Standard);
    let mut chain = anvil_dsp::Chain::new(buf.sample_rate());
    let t1 = std::time::Instant::now();
    let _ = chain.render(&buf, &cfg);
    let master_secs = t1.elapsed().as_secs_f64();

    println!("RTF analyze = {:.1}x", audio_secs / analyze_secs);
    println!("RTF master  = {:.1}x", audio_secs / master_secs);
}

#[test]
fn master_is_deterministic_over_a_real_file() {
    let path = write_fixture();
    let preset = Preset::default();
    let a = master(&path, &preset, Tier::Standard).unwrap();
    let b = master(&path, &preset, Tier::Standard).unwrap();
    assert_eq!(a.audio, b.audio, "double render must be bit-identical");
    let _ = std::fs::remove_file(&path);
}
