//! Autoregressive gap interpolation (03 §4.3, "AR-model interpolation") — the shared
//! reconstruction kernel behind de-crackle and (as a fallback) the de-click family.
//!
//! A short damaged run is bridged by *predicting* it from the clean audio on either side
//! rather than by drawing a straight line through it: we fit an AR(p) model to the clean
//! left context (Levinson-Durbin on its autocorrelation), extrapolate forward across the gap,
//! fit a second model to the time-reversed right context, extrapolate backward, and crossfade
//! the two. Speech is locally near-stationary and strongly autoregressive (that is what a
//! vocal-tract filter *is*), so the bridge keeps the formant structure and pitch pulse instead
//! of punching a spectral hole — which is exactly what "does not smear speech" means.
//!
//! Guard rails: a ridge on the zero-lag autocorrelation keeps Levinson well-conditioned on
//! near-deterministic input, and any extrapolation that runs away (non-finite, or beyond
//! [`RUNAWAY_LIMIT`]× the context peak) is rejected so the caller can fall back to the linear
//! bridge. Deterministic (ADR-003): sequential f64 math, no entropy, no threading.

/// Shortest usable one-sided context, in samples, regardless of model order.
const MIN_CONTEXT: usize = 32;
/// Reject an extrapolation whose magnitude exceeds this multiple of the context peak.
const RUNAWAY_LIMIT: f32 = 4.0;
/// A context below this energy is silence — there is no model to fit.
const MIN_ENERGY: f64 = 1e-12;

/// Fit an AR(`order`) prediction-error filter to `x` by **Burg's method**.
///
/// Returns the coefficients `a[0..order]` = a₁..a_p of `A(z) = 1 + Σ aⱼ z⁻ʲ`, i.e. the
/// predictor is `x̂[n] = −Σ aⱼ·x[n−j]`. `None` when the context is too short or silent.
///
/// Burg, not the textbook autocorrelation/Levinson method, and the difference is not
/// academic: the autocorrelation estimator implicitly tapers lag ℓ by (N−ℓ)/N, which on a
/// short context broadens every spectral peak, pulls the model's poles inside the unit circle
/// and makes the extrapolation *decay* across the gap — a 512-sample fit to a clean sine came
/// back with a₁ ≈ −1.05 where the true value is −2·cos ω ≈ −2.0. Burg minimizes the forward
/// and backward prediction error directly, with no windowing bias, and its reflection
/// coefficients are bounded by construction, so the filter is always stable.
///
/// Kept in f64 end to end: on near-tonal material the high-order coefficients are large and
/// cancelling, and rounding them to f32 is enough to visibly bend a 20-sample extrapolation.
pub(crate) fn fit(x: &[f32], order: usize) -> Option<Vec<f64>> {
    let n = x.len();
    if order == 0 || n <= order {
        return None;
    }
    let energy: f64 = x.iter().map(|&s| (s as f64) * (s as f64)).sum();
    if energy <= MIN_ENERGY {
        return None; // silent context: nothing to model
    }

    // Forward and backward prediction errors, initialized to the signal itself (order 0).
    let mut f: Vec<f64> = x.iter().map(|&s| s as f64).collect();
    let mut b = f.clone();
    let mut a: Vec<f64> = Vec::with_capacity(order);

    for m in 1..=order {
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for i in m..n {
            num += f[i] * b[i - 1];
            den += f[i] * f[i] + b[i - 1] * b[i - 1];
        }
        if den <= MIN_ENERGY {
            break; // the residual is exhausted — the remaining coefficients stay zero
        }
        let k = -2.0 * num / den;
        if !k.is_finite() {
            break;
        }

        // Coefficient recursion: aⱼ⁽ᵐ⁾ = aⱼ⁽ᵐ⁻¹⁾ + k·a₍ₘ₋ⱼ₎⁽ᵐ⁻¹⁾, with a_m⁽ᵐ⁾ = k.
        let prev = a.clone();
        a.push(k);
        for j in 0..m - 1 {
            a[j] = prev[j] + k * prev[m - 2 - j];
        }

        // Error recursion (descending, so each step reads the previous order's values).
        for i in (m..n).rev() {
            let f_old = f[i];
            let b_old = b[i - 1];
            f[i] = f_old + k * b_old;
            b[i] = b_old + k * f_old;
        }
    }

    if a.is_empty() || a.iter().any(|v| !v.is_finite()) {
        return None;
    }
    a.resize(order, 0.0);
    Some(a)
}

/// Run the predictor `count` steps past the end of `history` (which must hold ≥ p samples).
fn extrapolate(history: &[f32], coeffs: &[f64], count: usize) -> Vec<f64> {
    let p = coeffs.len();
    let mut state: Vec<f64> = history[history.len() - p..]
        .iter()
        .map(|&s| s as f64)
        .collect();
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut y = 0.0f64;
        for (j, &a) in coeffs.iter().enumerate() {
            // `state` is chronological: state[len−1−j] is x[n−1−j].
            y -= a * state[state.len() - 1 - j];
        }
        out.push(y);
        state.push(y);
    }
    out
}

/// Reconstruct `x[start..end)` from the clean audio around it.
///
/// `order` is the AR model order and `context` the number of samples taken from each side.
/// Returns the replacement samples, or `None` when there is not enough clean context or the
/// model runs away (caller should fall back to [`linear_bridge`]).
pub(crate) fn interpolate_gap(
    x: &[f32],
    start: usize,
    end: usize,
    order: usize,
    context: usize,
) -> Option<Vec<f32>> {
    if order == 0 || end <= start || end > x.len() {
        return None;
    }
    let gap = end - start;
    let min_ctx = (2 * order).max(MIN_CONTEXT);

    let left = &x[start.saturating_sub(context)..start];
    let right = &x[end..(end + context).min(x.len())];
    if left.len() < min_ctx || right.len() < min_ctx {
        return None;
    }

    let fwd = extrapolate(left, &fit(left, order)?, gap);

    // Backward pass: an AR model is fitted to the time-reversed right context and run forward,
    // which predicts the gap from `end` back to `start`; flip it to chronological order.
    let reversed: Vec<f32> = right.iter().rev().copied().collect();
    let mut bwd = extrapolate(&reversed, &fit(&reversed, order)?, gap);
    bwd.reverse();

    let peak = left
        .iter()
        .chain(right.iter())
        .fold(0.0f32, |m, &s| m.max(s.abs()));
    let limit = (peak * RUNAWAY_LIMIT).max(1e-6) as f64;

    let mut out = Vec::with_capacity(gap);
    for i in 0..gap {
        // Crossfade: trust the forward model at the left edge, the backward model at the right.
        let t = (i + 1) as f64 / (gap + 1) as f64;
        let v = fwd[i] * (1.0 - t) + bwd[i] * t;
        if !v.is_finite() || v.abs() > limit {
            return None; // unstable model — the caller bridges linearly instead
        }
        out.push(v as f32);
    }
    Some(out)
}

/// Straight-line bridge between the clean anchors `left` and `right` (exclusive interior).
/// The M3 de-click repair, factored out so de-crackle can share it as its fallback.
pub(crate) fn linear_bridge(channel: &mut [f32], left: usize, right: usize) {
    if right <= left || right >= channel.len() {
        return;
    }
    let span = (right - left) as f32;
    let a = channel[left];
    let b = channel[right];
    for (offset, idx) in ((left + 1)..right).enumerate() {
        let t = (offset + 1) as f32 / span;
        channel[idx] = a + (b - a) * t;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.4 * (i as f32 * freq * TAU / 48_000.0).sin())
            .collect()
    }

    #[test]
    fn ar_bridge_reconstructs_a_tone_better_than_a_straight_line() {
        let clean = tone(300.0, 4_000);
        let (start, end) = (2_000usize, 2_020usize); // 20-sample gap ≈ 0.4 ms

        let ar = interpolate_gap(&clean, start, end, 24, 512).expect("AR bridge");
        let ar_err = ar
            .iter()
            .zip(&clean[start..end])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);

        let mut lin = clean.clone();
        linear_bridge(&mut lin, start - 1, end);
        let lin_err = lin[start..end]
            .iter()
            .zip(&clean[start..end])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);

        assert!(
            ar_err < lin_err * 0.5,
            "AR bridge should beat linear: ar={ar_err}, linear={lin_err}"
        );
    }

    #[test]
    fn degenerate_contexts_are_rejected_not_panicked() {
        let silence = vec![0.0f32; 2_000];
        assert!(interpolate_gap(&silence, 1_000, 1_010, 16, 256).is_none());
        // Not enough context on the left.
        let s = tone(300.0, 2_000);
        assert!(interpolate_gap(&s, 4, 8, 16, 256).is_none());
        // Empty / inverted ranges.
        assert!(interpolate_gap(&s, 100, 100, 16, 256).is_none());
        assert!(interpolate_gap(&s, 100, 99, 16, 256).is_none());
    }

    #[test]
    fn deterministic() {
        let s = tone(220.0, 4_000);
        let a = interpolate_gap(&s, 2_000, 2_016, 24, 512).unwrap();
        let b = interpolate_gap(&s, 2_000, 2_016, 24, 512).unwrap();
        assert_eq!(a, b);
    }
}
