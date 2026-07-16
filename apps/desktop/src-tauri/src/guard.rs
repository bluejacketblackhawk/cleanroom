//! Recording Guard (04 §S10): a zero-persistence pre-flight check, not a recorder. Live
//! input meter with plain-word headroom guidance, a continuously-tracked noise-floor
//! reading, a "clap test" room-echo (RT60) estimate, and the device/sample-rate in use.
//!
//! This is a genuine `cpal` **input** capture, added directly in the desktop crate rather
//! than in `anvil-audio` (which today only does output) — see the M4 brief: "if adding an
//! input capture to the desktop crate is clean, do it". It is: the shape mirrors
//! `anvil_audio::engine`'s output engine (a dedicated thread owns the non-`Send` `Stream`;
//! the Tauri commands only touch small `Arc`s of atomics/mutexes), just simpler because a
//! meter has no transport (no seek, no A/B).
//!
//! Nothing here is an `// INTEGRATION SEAM` — the meter, the noise floor, and the RT60
//! estimate are all computed from real captured audio, not placeholders. The RT60 estimate
//! is deliberately its own small algorithm (impulse decay from the loudest point in the
//! capture) rather than reusing `anvil_dsp`'s reverb estimator, which is speech-offset
//! based (it looks for *speech→silence* transitions) and would almost never fire on a
//! single clap — reusing it here would have "worked" in the sense of compiling and mostly
//! returning an honest `None`, but would have shipped a clap test that (almost) never
//! produces a reading, which is a failure to deliver the feature even though it isn't
//! technically a fake result.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
// `Sample` brings the `from_sample` inherent-looking method into scope (it's defined on
// `Sample` itself, gated by a `Self: FromSample<S>` where-bound — `FromSample`'s own
// method is `from_sample_`, not `from_sample`); `FromSample` is what the `where f32:
// FromSample<T>` bound below names.
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use serde::Serialize;
use tauri::State;

// ---- level tracking (lock-free; written by the audio callback, read by polling) --------

/// Smoothed peak/RMS/noise-floor levels, published by the audio callback as `f32` bits so
/// the polling `guard_meter` command never blocks the realtime thread.
struct Levels {
    peak: AtomicU32,
    rms: AtomicU32,
    noise_floor: AtomicU32,
}

impl Levels {
    fn new() -> Self {
        Self {
            peak: AtomicU32::new(0f32.to_bits()),
            rms: AtomicU32::new(0f32.to_bits()),
            // Starts "high" (0 dBFS-ish) so a single loud moment before any quiet audio
            // arrives doesn't read as a suspiciously perfect noise floor.
            noise_floor: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    fn set_peak(&self, v: f32) {
        self.peak.store(v.to_bits(), Ordering::Release);
    }
    fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Acquire))
    }
    fn set_rms(&self, v: f32) {
        self.rms.store(v.to_bits(), Ordering::Release);
    }
    fn rms(&self) -> f32 {
        f32::from_bits(self.rms.load(Ordering::Acquire))
    }
    fn set_noise_floor(&self, v: f32) {
        self.noise_floor.store(v.to_bits(), Ordering::Release);
    }
    fn noise_floor(&self) -> f32 {
        f32::from_bits(self.noise_floor.load(Ordering::Acquire))
    }
}

/// Raw-sample scratch buffer for the "clap test": the audio callback only pushes samples
/// into this while `capturing` is set, so the meter's steady-state hot path never
/// allocates or copies full-rate audio.
struct ClapCapture {
    capturing: AtomicBool,
    samples: Mutex<Vec<f32>>,
}

impl ClapCapture {
    fn new() -> Self {
        Self {
            capturing: AtomicBool::new(false),
            samples: Mutex::new(Vec::new()),
        }
    }
}

/// Bound on how much a clap-test capture can grow, regardless of the requested duration
/// (belt-and-braces against a caller passing an unreasonable duration).
const MAX_CLAP_CAPTURE_SECS: f64 = 8.0;

/// Live audio-thread machinery for one running input stream. There is only ever one
/// "command" — shut down — so the channel just carries `()` rather than a one-variant enum.
struct Live {
    device_name: String,
    sample_rate: u32,
    channels: u16,
    levels: Arc<Levels>,
    clap: Arc<ClapCapture>,
    shutdown_tx: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Holds at most one running input stream. `Mutex` (not `RwLock`) because every command
/// here is a rare, human-paced action (start/stop/poll/clap-test), never a hot path.
#[derive(Default)]
pub struct GuardState {
    live: Mutex<Option<Live>>,
}

impl GuardState {
    pub fn new() -> Self {
        Self::default()
    }
}

// ---- wire types --------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct GuardDevice {
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardMeter {
    pub running: bool,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
    /// "hot" | "good" | "quiet" | "silent".
    pub headroom_level: String,
    pub headroom_message: String,
    pub noise_floor_dbfs: f32,
    /// "quiet" | "some_hiss" | "noisy".
    pub noise_rating: String,
    pub noise_message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClapResult {
    pub ok: bool,
    pub rt60_secs: Option<f32>,
    /// "dry" | "ok" | "noticeable" | "bad" — present only when `rt60_secs` is.
    pub reverb_bucket: Option<String>,
    pub message: String,
}

// ---- commands -----------------------------------------------------------------------

/// List available input devices (04 §S10 "device… display" needs somewhere to pick from).
#[tauri::command]
pub fn guard_list_devices() -> Vec<GuardDevice> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    match host.input_devices() {
        Ok(devices) => devices
            .filter_map(|d| d.name().ok())
            .map(|name| {
                let is_default = default_name.as_deref() == Some(name.as_str());
                GuardDevice { name, is_default }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Start (or, if already running, just report) the live input meter on `device_name` (or
/// the system default input when `None`).
#[tauri::command]
pub fn guard_start(
    device_name: Option<String>,
    state: State<'_, GuardState>,
) -> Result<GuardMeter, String> {
    let mut guard = state.live.lock().map_err(|_| "guard lock poisoned")?;
    if let Some(live) = guard.as_ref() {
        return Ok(build_meter(live));
    }
    let live = spawn_live(device_name.as_deref())?;
    let meter = build_meter(&live);
    *guard = Some(live);
    Ok(meter)
}

/// Stop the live input meter, releasing the device. A no-op if nothing is running.
#[tauri::command]
pub fn guard_stop(state: State<'_, GuardState>) -> Result<(), String> {
    let mut guard = state.live.lock().map_err(|_| "guard lock poisoned")?;
    *guard = None; // `Live::drop` sends Shutdown and joins the thread.
    Ok(())
}

/// Poll the current meter reading. Errors if the meter isn't running (the UI should call
/// `guard_start` first — same "open a file before…" shape as the Master tab's commands).
#[tauri::command]
pub fn guard_meter(state: State<'_, GuardState>) -> Result<GuardMeter, String> {
    let guard = state.live.lock().map_err(|_| "guard lock poisoned")?;
    let live = guard
        .as_ref()
        .ok_or_else(|| "start the input meter first".to_string())?;
    Ok(build_meter(live))
}

/// Capture `duration_secs` of audio (clamped to a sane range) and estimate room echo from
/// its decay after the loudest moment (the "clap"). Blocks the calling thread for the
/// capture window — deliberate: this is a short, foreground, user-initiated test, the same
/// shape as `transcribe` blocking on the whisper sidecar.
#[tauri::command]
pub fn guard_clap_test(
    duration_secs: f64,
    state: State<'_, GuardState>,
) -> Result<ClapResult, String> {
    let (clap, sample_rate) = {
        let guard = state.live.lock().map_err(|_| "guard lock poisoned")?;
        let live = guard
            .as_ref()
            .ok_or_else(|| "start the input meter before running the clap test".to_string())?;
        (Arc::clone(&live.clap), live.sample_rate)
    };

    let secs = duration_secs.clamp(1.0, MAX_CLAP_CAPTURE_SECS);
    {
        let mut buf = clap
            .samples
            .lock()
            .map_err(|_| "clap buffer lock poisoned")?;
        buf.clear();
    }
    clap.capturing.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_secs_f64(secs));
    clap.capturing.store(false, Ordering::Release);

    let samples = {
        let mut buf = clap
            .samples
            .lock()
            .map_err(|_| "clap buffer lock poisoned")?;
        std::mem::take(&mut *buf)
    };

    if samples.len() < sample_rate as usize / 2 {
        return Ok(ClapResult {
            ok: false,
            rt60_secs: None,
            reverb_bucket: None,
            message: "Didn't catch enough audio — make sure the meter is running and try again."
                .to_string(),
        });
    }

    match estimate_rt60(&samples, sample_rate) {
        Some(rt60) => {
            let bucket = reverb_bucket_for(rt60);
            Ok(ClapResult {
                ok: true,
                rt60_secs: Some(rt60),
                reverb_bucket: Some(bucket.to_string()),
                message: echo_message(rt60, bucket),
            })
        }
        None => Ok(ClapResult {
            ok: false,
            rt60_secs: None,
            reverb_bucket: None,
            message: "Couldn't hear a clear decay — try a sharper, louder clap in a quiet moment."
                .to_string(),
        }),
    }
}

// ---- meter interpretation (plain words, never a bare number — 04 §Microcopy rules) -----

fn dbfs(linear: f32) -> f32 {
    20.0 * linear.max(1e-7).log10()
}

fn build_meter(live: &Live) -> GuardMeter {
    let peak_dbfs = dbfs(live.levels.peak());
    let rms_dbfs = dbfs(live.levels.rms());
    let noise_floor_dbfs = dbfs(live.levels.noise_floor());
    let (headroom_level, headroom_message) = headroom_for(peak_dbfs);
    let (noise_rating, noise_message) = noise_rating_for(noise_floor_dbfs);
    GuardMeter {
        running: true,
        device_name: live.device_name.clone(),
        sample_rate: live.sample_rate,
        channels: live.channels,
        peak_dbfs,
        rms_dbfs,
        headroom_level: headroom_level.to_string(),
        headroom_message,
        noise_floor_dbfs,
        noise_rating: noise_rating.to_string(),
        noise_message,
    }
}

fn headroom_for(peak_dbfs: f32) -> (&'static str, String) {
    if peak_dbfs > -3.0 {
        (
            "hot",
            "Too hot — turn the input gain down, peaks are right at clipping.".to_string(),
        )
    } else if peak_dbfs > -9.0 {
        (
            "hot",
            "A bit hot — lower gain until the bar stays green while you speak loudly.".to_string(),
        )
    } else if peak_dbfs > -24.0 {
        ("good", "Good headroom for recording.".to_string())
    } else if peak_dbfs > -50.0 {
        (
            "quiet",
            "Quiet — you can raise the gain a bit for a stronger signal.".to_string(),
        )
    } else {
        (
            "silent",
            "No signal — check the microphone is connected and not muted.".to_string(),
        )
    }
}

fn noise_rating_for(noise_floor_dbfs: f32) -> (&'static str, String) {
    if noise_floor_dbfs < -55.0 {
        (
            "quiet",
            "Very quiet background — nice and clean.".to_string(),
        )
    } else if noise_floor_dbfs < -40.0 {
        (
            "some_hiss",
            "A little background hiss — usually fine after mastering.".to_string(),
        )
    } else {
        (
            "noisy",
            "Noticeable background noise, like a fan or AC hum — worth finding the source."
                .to_string(),
        )
    }
}

fn reverb_bucket_for(rt60: f32) -> &'static str {
    if rt60 < 0.3 {
        "dry"
    } else if rt60 < 0.5 {
        "ok"
    } else if rt60 < 0.8 {
        "noticeable"
    } else {
        "bad"
    }
}

fn echo_message(rt60: f32, bucket: &str) -> String {
    match bucket {
        "dry" => format!(
            "Room sounds tight and controlled — about {rt60:.1}s of decay. No echo problem here."
        ),
        "ok" => format!(
            "A little room echo — about {rt60:.1}s of decay. Standard tier will clean up the rest."
        ),
        "noticeable" => format!(
            "Noticeable room echo — about {rt60:.1}s of decay. The Studio tier will help most; soft furnishings help more."
        ),
        _ => format!(
            "Strong room echo — about {rt60:.1}s of decay. Try a smaller or softer-furnished room if you can."
        ),
    }
}

/// A real, if deliberately simple, impulse-decay RT60 estimate: short-time energy in dB
/// across the capture, find the loudest window (the clap), then time how long the level
/// takes to fall from 5 dB to 25 dB below that peak (the standard "T20" span) and
/// extrapolate ×3 for a full 60 dB decay. `None` when the capture never shows a clear
/// decay (too quiet a clap, or a noise floor that swallows it) — a one-click pre-flight
/// check should say "couldn't tell" rather than guess.
fn estimate_rt60(samples: &[f32], sample_rate: u32) -> Option<f32> {
    const WINDOW_SECS: f32 = 0.005; // 5 ms energy windows
    let window = ((sample_rate as f32 * WINDOW_SECS) as usize).max(8);
    if samples.len() < window * 8 {
        return None;
    }

    let energy_db: Vec<f32> = samples
        .chunks(window)
        .map(|chunk| {
            let sum_sq: f64 = chunk.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            let rms = (sum_sq / chunk.len() as f64).sqrt() as f32;
            dbfs(rms)
        })
        .collect();

    let (peak_idx, &peak_db) = energy_db
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    let hop_secs = window as f32 / sample_rate as f32;
    let mut minus5: Option<usize> = None;
    let mut minus25: Option<usize> = None;
    for (offset, &level) in energy_db[peak_idx..].iter().enumerate() {
        let drop = peak_db - level;
        if minus5.is_none() && drop >= 5.0 {
            minus5 = Some(offset);
        }
        if drop >= 25.0 {
            minus25 = Some(offset);
            break;
        }
    }

    let start = minus5?;
    let end = minus25?;
    if end <= start {
        return None;
    }
    let t20_secs = (end - start) as f32 * hop_secs;
    Some((t20_secs * 3.0).clamp(0.05, 3.0))
}

// ---- cpal plumbing --------------------------------------------------------------------

fn spawn_live(device_name: Option<&str>) -> Result<Live, String> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| format!("input device \"{name}\" not found"))?,
        None => host.default_input_device().ok_or_else(|| {
            "no input device found — plug in a microphone and try again".to_string()
        })?,
    };
    let device_label = device
        .name()
        .unwrap_or_else(|_| "Unknown input device".to_string());
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let levels = Arc::new(Levels::new());
    let clap = Arc::new(ClapCapture::new());
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let thread_levels = Arc::clone(&levels);
    let thread_clap = Arc::clone(&clap);
    let handle = std::thread::Builder::new()
        .name("anvil-guard-input".into())
        .spawn(move || {
            guard_audio_thread(
                device,
                config,
                sample_format,
                thread_levels,
                thread_clap,
                shutdown_rx,
            );
        })
        .map_err(|e| e.to_string())?;

    Ok(Live {
        device_name: device_label,
        sample_rate,
        channels,
        levels,
        clap,
        shutdown_tx,
        handle: Some(handle),
    })
}

fn guard_audio_thread(
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    levels: Arc<Levels>,
    clap: Arc<ClapCapture>,
    shutdown_rx: mpsc::Receiver<()>,
) {
    let stream = match build_input_stream(&device, &config, sample_format, levels, clap) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to build guard input stream: {e}");
            return;
        }
    };
    if let Err(e) = stream.play() {
        tracing::error!("failed to start guard input stream: {e}");
        return;
    }
    // Block until told to shut down (or the sender drops) — `stream` keeps calling its
    // callback the whole time. There's only ever one signal, so no loop is needed.
    let _ = shutdown_rx.recv();
    // `stream` drops here, releasing the device.
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    levels: Arc<Levels>,
    clap: Arc<ClapCapture>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    match sample_format {
        SampleFormat::F32 => build_input_stream_t::<f32>(device, config, levels, clap),
        SampleFormat::I16 => build_input_stream_t::<i16>(device, config, levels, clap),
        SampleFormat::U16 => build_input_stream_t::<u16>(device, config, levels, clap),
        other => {
            tracing::warn!("unsupported input sample format {other:?}; defaulting to f32");
            build_input_stream_t::<f32>(device, config, levels, clap)
        }
    }
}

fn one_pole_coeff(tau_secs: f32, frames: usize, sample_rate: u32) -> f32 {
    if tau_secs <= 0.0 || sample_rate == 0 {
        return 0.0;
    }
    (-(frames as f32) / (tau_secs * sample_rate as f32)).exp()
}

fn smooth(prev: f32, target: f32, attack_coeff: f32, release_coeff: f32) -> f32 {
    let coeff = if target > prev {
        attack_coeff
    } else {
        release_coeff
    };
    coeff * prev + (1.0 - coeff) * target
}

fn build_input_stream_t<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    levels: Arc<Levels>,
    clap: Arc<ClapCapture>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let sample_rate = config.sample_rate.0;

    // Envelope state lives only in this closure's captures (not shared/atomic) — the
    // callback is the sole writer, `Levels` is just where it publishes the result.
    let mut smoothed_peak = 0f32;
    let mut smoothed_rms = 0f32;
    let mut noise_floor = 1.0f32;
    let max_clap_samples = (sample_rate as f64 * MAX_CLAP_CAPTURE_SECS) as usize;

    let err_fn = |e| tracing::error!("guard input stream error: {e}");

    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if channels == 0 || data.is_empty() {
                return;
            }
            let frames = data.len() / channels;
            if frames == 0 {
                return;
            }
            let inv_channels = 1.0f32 / channels as f32;

            let capturing = clap.capturing.load(Ordering::Acquire);
            let mut capture_chunk = if capturing {
                Vec::with_capacity(frames)
            } else {
                Vec::new()
            };

            let mut chunk_peak = 0f32;
            let mut sum_sq = 0f64;
            for f in 0..frames {
                let mut mono_sum = 0f32;
                let mut abs_max = 0f32;
                for c in 0..channels {
                    let s: f32 = f32::from_sample(data[f * channels + c]);
                    mono_sum += s;
                    abs_max = abs_max.max(s.abs());
                }
                let mono = mono_sum * inv_channels;
                chunk_peak = chunk_peak.max(abs_max);
                sum_sq += f64::from(mono) * f64::from(mono);
                if capturing {
                    capture_chunk.push(mono);
                }
            }
            let chunk_rms = ((sum_sq / frames as f64).sqrt()) as f32;

            let attack = one_pole_coeff(0.01, frames, sample_rate);
            let release = one_pole_coeff(0.30, frames, sample_rate);
            smoothed_peak = smooth(smoothed_peak, chunk_peak, attack, release);
            smoothed_rms = smooth(smoothed_rms, chunk_rms, attack, release);

            // Noise floor: snap down fast to a quieter reading, drift back up slowly so a
            // room that gets noisier later isn't stuck reporting an old quiet number.
            if smoothed_rms < noise_floor {
                noise_floor = smoothed_rms;
            } else {
                let drift = one_pole_coeff(4.0, frames, sample_rate);
                noise_floor = drift * noise_floor + (1.0 - drift) * smoothed_rms;
            }

            levels.set_peak(smoothed_peak);
            levels.set_rms(smoothed_rms);
            levels.set_noise_floor(noise_floor.max(1e-7));

            if capturing {
                if let Ok(mut buf) = clap.samples.lock() {
                    if buf.len() < max_clap_samples {
                        let remaining = max_clap_samples - buf.len();
                        buf.extend(capture_chunk.into_iter().take(remaining));
                    }
                }
            }
        },
        err_fn,
        Some(Duration::from_millis(200)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_of_full_scale_is_zero() {
        assert!((dbfs(1.0) - 0.0).abs() < 1e-4);
    }

    #[test]
    fn dbfs_of_silence_is_very_negative() {
        assert!(dbfs(0.0) < -100.0);
    }

    #[test]
    fn headroom_buckets_match_expected_ranges() {
        assert_eq!(headroom_for(-1.0).0, "hot");
        assert_eq!(headroom_for(-6.0).0, "hot");
        assert_eq!(headroom_for(-15.0).0, "good");
        assert_eq!(headroom_for(-35.0).0, "quiet");
        assert_eq!(headroom_for(-70.0).0, "silent");
    }

    #[test]
    fn noise_rating_buckets_match_expected_ranges() {
        assert_eq!(noise_rating_for(-60.0).0, "quiet");
        assert_eq!(noise_rating_for(-45.0).0, "some_hiss");
        assert_eq!(noise_rating_for(-20.0).0, "noisy");
    }

    #[test]
    fn reverb_bucket_matches_expected_ranges() {
        assert_eq!(reverb_bucket_for(0.1), "dry");
        assert_eq!(reverb_bucket_for(0.4), "ok");
        assert_eq!(reverb_bucket_for(0.6), "noticeable");
        assert_eq!(reverb_bucket_for(1.2), "bad");
    }

    /// A synthetic impulse (loud burst, then near-silence) should yield a short but
    /// nonzero RT60 estimate — proves the decay walk actually finds the -5/-25 dB points.
    #[test]
    fn estimate_rt60_finds_a_decay_in_a_synthetic_impulse() {
        let sr = 48_000u32;
        let mut samples = Vec::new();
        // 100 ms loud burst at full scale.
        samples.extend(std::iter::repeat_n(0.9f32, (sr as usize) / 10));
        // 400 ms of exponential decay down toward the noise floor.
        let decay_len = (sr as usize) * 4 / 10;
        for i in 0..decay_len {
            let t = i as f32 / sr as f32;
            // ~ -40 dB/s decay: rt60 should land well under a second.
            let level = 0.9 * 10f32.powf(-40.0 * t / 20.0);
            samples.push(level.max(0.0001));
        }
        // A little quiet tail so the window has something after full decay.
        samples.extend(std::iter::repeat_n(0.0001f32, (sr as usize) / 10));

        let rt60 = estimate_rt60(&samples, sr);
        assert!(rt60.is_some(), "expected a decay to be found");
        let rt60 = rt60.unwrap();
        assert!(rt60 > 0.0 && rt60 < 3.0, "unexpected rt60: {rt60}");
    }

    #[test]
    fn estimate_rt60_is_none_for_flat_silence() {
        let sr = 48_000u32;
        let samples = vec![0.0f32; sr as usize];
        assert!(estimate_rt60(&samples, sr).is_none());
    }

    #[test]
    fn estimate_rt60_is_none_for_too_short_a_capture() {
        let sr = 48_000u32;
        let samples = vec![0.5f32; 100];
        assert!(estimate_rt60(&samples, sr).is_none());
    }
}
