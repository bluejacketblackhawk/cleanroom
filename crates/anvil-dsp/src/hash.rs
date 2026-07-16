//! Content hashing for two deterministic needs: seeding the dither (ADR-003) and keying the
//! chain's stage cache. FNV-1a over the raw sample bytes — fast, stable, and reproducible
//! across runs and platforms (it is not, and does not need to be, cryptographic).

use anvil_media::AudioBuffer;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[inline]
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A stable 64-bit hash of an audio buffer's content (samples + rate + channel count).
pub fn hash_buffer(buffer: &AudioBuffer) -> u64 {
    let mut h = FNV_OFFSET;
    h = fnv1a(h, &buffer.sample_rate().to_le_bytes());
    h = fnv1a(h, &(buffer.channel_count() as u64).to_le_bytes());
    for channel in buffer.planar() {
        // Hash the bit patterns; f32::to_bits keeps this exact and deterministic.
        for &s in channel {
            h = fnv1a(h, &s.to_bits().to_le_bytes());
        }
    }
    h
}

/// A stable 64-bit hash of any `Debug`-formatted config (the stage-cache config key). Debug
/// output for our small `Copy` config structs is deterministic and captures every field.
pub fn hash_config<T: std::fmt::Debug>(config: &T) -> u64 {
    fnv1a(FNV_OFFSET, format!("{config:?}").as_bytes())
}
