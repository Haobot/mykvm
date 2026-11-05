//! Per-axis Kalman filter (constant-velocity model) for head-pose angles.
//!
//! Replaces the One Euro low-pass as the measurement-stage filter: a 2-state
//! (angle, angular-velocity) Kalman both smooths the noisy 6DRepNet output
//! and yields a clean velocity estimate (which feeds `speed_norm` /
//! `enterSpeed`). Tuned for 30 Hz pose samples in degrees; `dt` comes from
//! frame timestamps and is clamped by the caller.

/// 2-state (position, velocity) scalar Kalman filter with a white-noise
/// acceleration process model. All math is plain f64; no external deps.
#[derive(Debug, Clone)]
pub struct Kalman1D {
    /// Process noise intensity (deg^2/s^3): how freely the true angle may
    /// accelerate. Higher tracks fast head turns at the cost of noise.
    q: f64,
    /// Measurement noise variance (deg^2): the pose model's output jitter.
    r: f64,
    // State: [angle, angle_velocity]
    x: (f64, f64),
    // Covariance 2x2.
    p: [[f64; 2]; 2],
    initialized: bool,
}

impl Kalman1D {
    /// `process_accel_noise` (deg^2/s^3) and `measurement_noise_var` (deg^2).
    pub fn new(process_accel_noise: f64, measurement_noise_var: f64) -> Self {
        Kalman1D {
            q: process_accel_noise.max(1e-6),
            r: measurement_noise_var.max(1e-6),
            x: (0.0, 0.0),
            p: [[1.0, 0.0], [0.0, 1.0]],
            initialized: false,
        }
    }

    /// Head-pose defaults: the pose model's static jitter on live video is
    /// roughly sigma 0.5 deg on a face crop (R = 0.25). Q is tuned
    /// stability-first (jitter was the top user complaint); fast-turn
    /// responsiveness is recovered by the render servo rather than by a hot
    /// Kalman — the velocity state still converges on ramps (see tests).
    pub fn head_pose() -> Self {
        Kalman1D::new(100.0, 0.25)
    }

    /// Feed one measurement at `dt` seconds; returns the updated (angle,
    /// velocity) estimate. The first measurement initializes the state.
    pub fn update(&mut self, measurement: f64, dt: f64) -> (f64, f64) {
        if !self.initialized {
            self.x = (measurement, 0.0);
            // Position uncertainty at the measurement noise level; velocity
            // unknown, keep it loose so the filter converges quickly.
            self.p = [[self.r, 0.0], [0.0, 100.0]];
            self.initialized = true;
            return self.x;
        }
        let dt = dt.max(1e-3);

        // ---- Predict (constant velocity + white-noise acceleration) ----
        let (mut pos, mut vel) = self.x;
        pos += vel * dt;
        let q = self.q;
        let dt2 = dt * dt;
        let dt3 = dt2 * dt;
        // Exact discrete Q for continuous white-noise acceleration.
        let q00 = q * dt3 / 3.0;
        let q01 = q * dt2 / 2.0;
        let q11 = q * dt;
        let (p00, p01, p11) = (self.p[0][0], self.p[0][1], self.p[1][1]);
        self.p = [
            [p00 + dt * (p01 + p01) + dt2 * p11 + q00, p01 + dt * p11 + q01],
            [p01 + dt * p11 + q01, p11 + q11],
        ];

        // ---- Update (scalar measurement of position) ----
        let y = measurement - pos;
        let s = self.p[0][0] + self.r;
        let k0 = self.p[0][0] / s;
        let k1 = self.p[1][0] / s;
        pos += k0 * y;
        vel += k1 * y;
        self.x = (pos, vel);
        // (I - K*H) * P with H = [1, 0]; P is symmetric (p10 == p01).
        let (p00, p01, p11) = (self.p[0][0], self.p[0][1], self.p[1][1]);
        self.p = [
            [(1.0 - k0) * p00, (1.0 - k0) * p01],
            [p01 - k1 * p00, p11 - k1 * p01],
        ];
        self.x
    }

    /// Current estimate without feeding a measurement.
    pub fn estimate(&self) -> (f64, f64) {
        self.x
    }

    /// Forget history (face lost): the next measurement re-initializes, so a
    /// returning face is not dragged through stale velocity.
    pub fn reset(&mut self) {
        self.initialized = false;
        self.x = (0.0, 0.0);
        self.p = [[1.0, 0.0], [0.0, 1.0]];
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-noise so assertions are stable.
    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f64 / (u64::MAX >> 33) as f64) * 2.0 - 1.0
    }

    #[test]
    fn first_measurement_initializes_state() {
        let mut k = Kalman1D::head_pose();
        let (p, v) = k.update(7.5, 0.033);
        assert!((p - 7.5).abs() < 1e-9);
        assert!(v.abs() < 1e-9);
    }

    #[test]
    fn static_noise_is_heavily_attenuated() {
        let mut k = Kalman1D::head_pose();
        let mut seed = 42u64;
        let sigma = 0.4;
        let mut outputs = Vec::new();
        for _ in 0..150 {
            let (p, _) = k.update(5.0 + sigma * lcg(&mut seed), 1.0 / 30.0);
            outputs.push(p);
        }
        let steady = &outputs[50..];
        let mean = steady.iter().sum::<f64>() / steady.len() as f64;
        let var = steady
            .iter()
            .map(|v| (v - mean) * (v - mean))
            .sum::<f64>()
            / steady.len() as f64;
        let out_sigma = var.sqrt();
        assert!(
            out_sigma < sigma * 0.35,
            "output sigma {out_sigma:.4} should be well under input {sigma}"
        );
        assert!((mean - 5.0).abs() < 0.15, "mean {mean:.3} tracks the truth");
    }

    #[test]
    fn ramp_is_tracked_with_bounded_lag_and_velocity_estimate() {
        let mut k = Kalman1D::head_pose();
        let dt = 1.0 / 30.0;
        let rate = 60.0; // deg/s head turn
        let mut last = (0.0, 0.0);
        for i in 0..90 {
            let t = i as f64 * dt;
            last = k.update(rate * t, dt);
        }
        let truth = rate * 89.0 * dt;
        let lag = truth - last.0;
        // At 60 deg/s the constant-velocity model lags well under 100 ms of
        // travel (< 6 deg); the velocity estimate converges near the rate.
        assert!(lag.abs() < 6.0, "lag {lag:.2} deg too large");
        assert!(
            (last.1 - rate).abs() < 12.0,
            "velocity estimate {} vs {rate}",
            last.1
        );
    }

    #[test]
    fn reset_forgets_state_after_face_loss() {
        let mut k = Kalman1D::head_pose();
        for i in 0..30 {
            k.update(i as f64, 1.0 / 30.0);
        }
        k.reset();
        assert!(!k.is_initialized());
        let (p, v) = k.update(-3.0, 0.033);
        assert!((p - -3.0).abs() < 1e-9);
        assert!(v.abs() < 1e-9);
    }

    #[test]
    fn dt_clamp_prevents_runaway_on_clock_glitch() {
        let mut k = Kalman1D::head_pose();
        k.update(0.0, 0.033);
        // dt of zero (duplicate timestamp) must not divide by zero or NaN.
        let (p, v) = k.update(1.0, 0.0);
        assert!(p.is_finite() && v.is_finite());
    }
}
