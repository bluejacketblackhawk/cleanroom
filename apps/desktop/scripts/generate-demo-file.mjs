#!/usr/bin/env node
// Generates the bundled onboarding demo file (04 §S9: "offers a bundled demo file").
//
// Entirely synthetic (procedurally generated here, not a recording of anyone, not any
// copyrighted material) — a deliberately BAD "podcast take": quiet voice-like syllable
// bursts buried in broadband hiss, so dropping it and pressing Master visibly fixes
// something real (the leveler brings it up to loudness target, DFN3 denoise pulls the
// noise floor down). Small on purpose (~8 s mono) to keep the installer light.
//
// Run: `node scripts/generate-demo-file.mjs` from `apps/desktop/`. Writes
// `src-tauri/resources/demo/bad-recording-example.wav`, which `tauri.conf.json`'s
// `bundle.resources` ships inside the installer/portable zip; the frontend resolves it at
// runtime via `@tauri-apps/api/path`'s `resolveResource`.

import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const OUT_PATH = path.join(
  __dirname,
  "..",
  "src-tauri",
  "resources",
  "demo",
  "bad-recording-example.wav",
);

const SAMPLE_RATE = 48_000;
const DURATION_SECS = 8;
const TOTAL_SAMPLES = SAMPLE_RATE * DURATION_SECS;

// Deterministic PRNG (mulberry32) so the file is byte-identical across regenerations —
// no reason for git churn on every re-run of this script.
function mulberry32(seed) {
  return function () {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rand = mulberry32(0xa1);

function raisedCosineEnvelope(t, attack, hold, release) {
  if (t < attack) return 0.5 - 0.5 * Math.cos((Math.PI * t) / attack);
  if (t < attack + hold) return 1;
  const r = t - attack - hold;
  if (r < release) return 0.5 + 0.5 * Math.cos((Math.PI * r) / release);
  return 0;
}

// "Syllable" bursts: short voice-like tone clusters separated by pauses, mimicking the
// rhythm of speech without being speech (no words, no likeness of any person).
const bursts = [];
{
  let t = 0.3;
  while (t < DURATION_SECS - 0.4) {
    const len = 0.12 + rand() * 0.22;
    bursts.push({ start: t, len });
    t += len + 0.06 + rand() * 0.18;
  }
}

const samples = new Float32Array(TOTAL_SAMPLES);

// Quiet, noisy "voice": a handful of low-order harmonics around a speech-ish fundamental,
// peaking well below full scale (the "quiet" half of "noisy/quiet").
const VOICE_PEAK = 0.09; // ~ -21 dBFS peak
for (const b of bursts) {
  const f0 = 110 + rand() * 40;
  const startSample = Math.floor(b.start * SAMPLE_RATE);
  const lenSamples = Math.floor(b.len * SAMPLE_RATE);
  const attack = Math.min(0.02, b.len * 0.25) * SAMPLE_RATE;
  const release = Math.min(0.05, b.len * 0.35) * SAMPLE_RATE;
  const hold = Math.max(0, lenSamples - attack - release);
  for (let i = 0; i < lenSamples; i++) {
    const idx = startSample + i;
    if (idx >= TOTAL_SAMPLES) break;
    const env = raisedCosineEnvelope(i, attack, hold, release);
    const time = i / SAMPLE_RATE;
    const voice =
      0.55 * Math.sin(2 * Math.PI * f0 * time) +
      0.28 * Math.sin(2 * Math.PI * f0 * 2.6 * time) +
      0.12 * Math.sin(2 * Math.PI * f0 * 4.3 * time);
    samples[idx] += VOICE_PEAK * env * voice;
  }
}

// Broadband hiss across the whole clip — the "noisy" half. Kept below the voice peak so
// there's real signal for the denoiser to separate from, not just noise.
const NOISE_RMS = 0.02; // roughly -34 dBFS RMS
for (let i = 0; i < TOTAL_SAMPLES; i++) {
  samples[i] += (rand() * 2 - 1) * NOISE_RMS;
}

// Safety clamp (headroom check only — nothing above should approach 0 dBFS).
let peak = 0;
for (let i = 0; i < TOTAL_SAMPLES; i++) peak = Math.max(peak, Math.abs(samples[i]));
if (peak > 0.5) {
  const scale = 0.5 / peak;
  for (let i = 0; i < TOTAL_SAMPLES; i++) samples[i] *= scale;
}

// --- WAV encode: mono 16-bit PCM ---
const bytesPerSample = 2;
const dataSize = TOTAL_SAMPLES * bytesPerSample;
const buffer = Buffer.alloc(44 + dataSize);
buffer.write("RIFF", 0, "ascii");
buffer.writeUInt32LE(36 + dataSize, 4);
buffer.write("WAVE", 8, "ascii");
buffer.write("fmt ", 12, "ascii");
buffer.writeUInt32LE(16, 16); // fmt chunk size
buffer.writeUInt16LE(1, 20); // PCM
buffer.writeUInt16LE(1, 22); // mono
buffer.writeUInt32LE(SAMPLE_RATE, 24);
buffer.writeUInt32LE(SAMPLE_RATE * bytesPerSample, 28); // byte rate
buffer.writeUInt16LE(bytesPerSample, 32); // block align
buffer.writeUInt16LE(16, 34); // bits per sample
buffer.write("data", 36, "ascii");
buffer.writeUInt32LE(dataSize, 40);
for (let i = 0; i < TOTAL_SAMPLES; i++) {
  const s = Math.max(-1, Math.min(1, samples[i]));
  buffer.writeInt16LE(Math.round(s * 32767), 44 + i * bytesPerSample);
}

writeFileSync(OUT_PATH, buffer);
const kb = (buffer.length / 1024).toFixed(0);
console.log(`Wrote ${OUT_PATH} (${kb} KB, ${DURATION_SECS}s mono @ ${SAMPLE_RATE} Hz)`);
