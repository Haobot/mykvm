//! One Euro Filter (Casiez et al., CHI 2012) for smoothing head-pose angles.
//!
//! Adaptive low-pass: the cutoff frequency rises with the estimated signal
//! speed, so slow head motion is heavily smoothed (stable halo) while fast
//! turns pass through almost unfiltered (responsive halo). One instance per
//! axis (yaw / pitch); state must not be shared between axes.

use std::f64::consts::PI;

/// Guard rails for caller-supplied time deltas: capture timestamps can glitch
/// (clock jumps, first sample) and a zero/negative `dt` would divide by zero.
const MIN_DT: f64 = 0.001;
const MAX_DT: f64 = 1.0;
/// Keep the adaptive cutoff positive even with extreme `beta` tuning.
const MIN_CUTOFF: f64 = 1e-3;

/// First-order exponential low-pass that stores its previous estimate.
#[derive(Debug, Clone)]
struct LowPass {
    hat: Option<f64>,
}

impl LowPass {
    fn new() -> Self {
        LowPass { hat: None }
    }

    fn filter(&mut self, value: f64, alpha: f64) -> f64 {
        match self.hat {
            None => self.hat = Some(value),
            Some(hat) => self.hat = Some(alpha * value + (1.0 - alpha) * hat),
        }
        self.hat.unwrap_or(value)
    }

    fn last(&self) -> Option<f64> {
        self.hat
    }
}

/// One Euro filter over a scalar signal, fed at (possibly varying) sample rate.
///
/// Defaults (see [`OneEuroFilter::head_pose`]): tracking lag < 100 ms on 30 Hz
/// ramps while static jitter stays well under 10 px at typical screen sizes.
/// Both figures are provisional until calibrated against real capture data
/// (P1-D telemetry task).
#[derive(Debug, Clone)]
pub struct OneEuroFilter {
    min_cutoff: f64,
    beta: f64,
    d_cutoff: f64,
    x: LowPass,
    dx: LowPass,
}

impl OneEuroFilter {
    pub fn new(min_cutoff: f64, beta: f64, d_cutoff: f64) -> Self {
        OneEuroFilter {
            min_cutoff: min_cutoff.max(MIN_CUTOFF),
            beta: beta.max(0.0),
            d_cutoff: d_cutoff.max(MIN_CUTOFF),
            x: LowPass::new(),
            dx: LowPass::new(),
        }
    }

    /// Default tuning for head pose (degrees, one instance per axis):
    /// `min_cutoff = 1.7`, `beta = 0.007`, `d_cutoff = 1.0`.
    /// Tracking lag stays under 100 ms on 30 Hz ramps and static noise is
    /// heavily damped; recalibrate against P1-D telemetry before shipping.
    pub fn head_pose() -> Self {
        OneEuroFilter::new(1.7, 0.007, 1.0)
    }

    /// Standard Casiez alpha: blending weight for a low-pass with `cutoff` Hz
    /// running at period `dt` seconds.
    fn alpha(&self, cutoff: f64, dt: f64) -> f64 {
        let tau = 1.0 / (2.0 * PI * cutoff.max(MIN_CUTOFF));
        1.0 / (1.0 + tau / dt)
    }

    /// Feed one sample taken `dt` seconds after the previous one. The first
    /// sample passes through unchanged (no history to smooth against).
    pub fn filter(&mut self, value: f64, dt: f64) -> f64 {
        let dt = dt.clamp(MIN_DT, MAX_DT);
        // Estimate speed in units/second; zero on the very first sample.
        let dvalue = match self.x.last() {
            Some(prev) => (value - prev) / dt,
            None => 0.0,
        };
        let edvalue = self.dx.filter(dvalue, self.alpha(self.d_cutoff, dt));
        let cutoff = self.min_cutoff + self.beta * edvalue.abs();
        self.x.filter(value, self.alpha(cutoff, dt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG so noise-based tests never flake between runs.
    struct Lcg(u64);

    impl Lcg {
        fn uniform(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }

        /// Standard normal via Box-Muller.
        fn normal(&mut self) -> f64 {
            let u1 = self.uniform().max(1e-12);
            let u2 = self.uniform();
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    #[test]
    fn first_sample_passes_through_unchanged() {
        let mut f = OneEuroFilter::head_pose();
        assert_eq!(f.filter(5.25, 1.0 / 30.0), 5.25);
    }

    #[test]
    fn ramp_lag_stays_under_100ms_at_30hz() {
        // 30 units/s ramp at 30 Hz: after warm-up the filter output should
        // align best with the input shifted by well under 100 ms (3 samples).
        let dt = 1.0 / 30.0;
        let mut f = OneEuroFilter::head_pose();
        let n = 150; // 5 s
        let input: Vec<f64> = (0..n).map(|i| 30.0 * (i as f64) * dt).collect();
        let output: Vec<f64> = input.iter().map(|&v| f.filter(v, dt)).collect();

        let start = 90; // skip 3 s warm-up
        let mut best_lag = 0;
        let mut best_err = f64::INFINITY;
        for lag in 0..6usize {
            let mut err = 0.0;
            for i in start..n {
                // input sample that would be "current" if the output lagged
                // exactly `lag` samples.
                err += (output[i] - input[i - lag]).abs();
            }
            if err < best_err {
                best_err = err;
                best_lag = lag;
            }
        }
        let lag_ms = best_lag as f64 * dt * 1000.0;
        assert!(
            lag_ms < 100.0,
            "steady-state ramp lag {lag_ms}ms (best_lag={best_lag} samples) should be < 100ms"
        );
    }

    #[test]
    fn static_noise_attenuated_below_40_percent() {
        // Constant position with sigma = 0.3 deg white noise for 6 s at 30 Hz;
        // the filter output's std must drop below 40% of the input's.
        let dt = 1.0 / 30.0;
        let mut f = OneEuroFilter::head_pose();
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        let sigma_in = 0.3;
        let n = 180;
        let mut out_sum = 0.0;
        let mut out_sq_sum = 0.0;
        let mut in_sq_sum = 0.0;
        let mut in_sum = 0.0;
        let mut count = 0.0;
        for i in 0..n {
            let noise = rng.normal() * sigma_in;
            let out = f.filter(noise, dt);
            if i >= 30 {
                // skip first second of warm-up
                in_sum += noise;
                in_sq_sum += noise * noise;
                out_sum += out;
                out_sq_sum += out * out;
                count += 1.0;
            }
        }
        let in_var = in_sq_sum / count - (in_sum / count).powi(2);
        let out_var = out_sq_sum / count - (out_sum / count).powi(2);
        let ratio = (out_var / in_var).sqrt();
        assert!(
            ratio < 0.4,
            "output std ratio {ratio:.3} should be < 0.40 of input std"
        );
    }
}
