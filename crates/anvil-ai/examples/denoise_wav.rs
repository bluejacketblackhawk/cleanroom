//! Dev-only harness: run a 48 kHz WAV through one denoise tier and write the result.
//!
//! This is what feeds the DNSMOS P.835 gate (06 §2) during development — score the input,
//! score the output, compare. Not shipped.
//!
//! ```text
//! cargo run --release -p anvil-ai --example denoise_wav -- in.wav out.wav standard 0.62
//! ```

use std::time::Instant;

use anvil_ai::{DenoiseConfig, DenoiseTier, Denoiser};
use anvil_media::AudioBuffer;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: denoise_wav <in.wav> <out.wav> <fast|standard|studio> [strength]");
        eprintln!("  env: CLEANROOM_PF=<beta> CLEANROOM_DEREVERB=<0..1> CLEANROOM_DEVICE=cpu  (tuning knobs)");
        std::process::exit(2);
    }
    let (input, output, tier_s) = (&args[1], &args[2], args[3].as_str());
    let strength: f32 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(0.62);
    let tier = match tier_s {
        "fast" => DenoiseTier::Fast,
        "standard" => DenoiseTier::Standard,
        "studio" => DenoiseTier::Studio,
        other => panic!("unknown tier {other}"),
    };

    let mut reader = hound::WavReader::open(input).expect("open input wav");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 48_000, "fixture must already be 48 kHz");
    let channels = spec.channels as usize;

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 * scale)
                .collect()
        }
    };
    let frames = interleaved.len() / channels;
    let planar: Vec<Vec<f32>> = (0..channels)
        .map(|c| (0..frames).map(|i| interleaved[i * channels + c]).collect())
        .collect();
    let mut buf = AudioBuffer::from_planar(planar, 48_000);

    let cfg = DenoiseConfig {
        strength,
        music_aware: false,
    };
    let mut denoiser = Denoiser::try_with_tier(channels, cfg, tier).expect("build denoiser");

    // Tuning knobs for the DNSMOS sweeps. Not part of the shipping API.
    let pf: Option<f32> = std::env::var("CLEANROOM_PF")
        .ok()
        .map(|v| v.parse().unwrap());
    let dr: Option<f32> = std::env::var("CLEANROOM_DEREVERB")
        .ok()
        .map(|v| v.parse().unwrap());
    if pf.is_some() || dr.is_some() {
        denoiser.tune(pf, dr);
    }

    let t0 = Instant::now();
    denoiser.try_process(&mut buf).expect("denoise");
    let elapsed = t0.elapsed().as_secs_f64();

    let audio_s = frames as f64 / 48_000.0;
    eprintln!(
        "tier={:?} device={:?} strength={strength} atten_lim={:.1} dB pf={:?} dereverb={:?}  |  {audio_s:.2} s audio in {elapsed:.2} s  =>  RTF {:.2}x",
        denoiser.tier(),
        denoiser.device(),
        cfg.max_attenuation_db(),
        pf,
        dr,
        audio_s / elapsed
    );

    let out_spec = hound::WavSpec {
        channels: spec.channels,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(output, out_spec).expect("create output wav");
    for i in 0..frames {
        for c in 0..channels {
            writer.write_sample(buf.channel(c)[i]).unwrap();
        }
    }
    writer.finalize().unwrap();
}
