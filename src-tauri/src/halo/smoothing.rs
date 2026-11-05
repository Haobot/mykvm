//! Critically-damped PD servo for render-side halo motion.
//!
//! The measurement Kalman runs on ~30 Hz pose samples; drawing those samples
//! directly at the 60 Hz tick would stair-step. This servo instead simulates a
//! spring-damper toward the latest filtered target every tick: a PD controller
//! (the sensible part of "PID" for a tracker — the integral term would wind up
//! and add lag, so it is deliberately omitted). `tau` (seconds) is the
//! exponential settle time: smaller = snappier, larger = calmer.
//!
//! A sub-pixel dead band kills the residual shimmer when the target is
//! effectively stationary.

use serde::{Deserialize, Serialize};

/// Servo state for one screen axis (position + velocity, in logical px).
#[derive(Debug, Clone, Copy)]
pub struct ServoAxis {
    pos: f64,
    vel: f64,
    has_state: bool,
}

/// Tunables shared by both axes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServoTuning {
    /// Settle time constant in seconds (critically damped).
    pub tau: f64,
    /// Dead-band radius in logical px: targets within this distance of the
    /// settled position (and with negligible velocity) do not move the halo.
    pub dead_band_px: f64,
}

impl ServoTuning {
    pub fn from_smooth_ms(smooth_ms: f64) -> Self {
        ServoTuning {
            tau: (smooth_ms / 1000.0).clamp(0.05, 0.5),
            dead_band_px: 1.5,
        }
    }
}

impl ServoAxis {
    pub fn new() -> Self {
        ServoAxis {
            pos: 0.0,
            vel: 0.0,
            has_state: false,
        }
    }

    /// Advance one `dt`-second tick toward `target`. The first call snaps to
    /// the target (nothing to animate from). When the residual error and the
    /// velocity are both inside the dead band the position pins to the target
    /// so a noisy stationary target cannot shimmer the halo.
    pub fn step(&mut self, target: f64, dt: f64, tuning: &ServoTuning) -> f64 {
        if !self.has_state {
            self.pos = target;
            self.vel = 0.0;
            self.has_state = true;
            return self.pos;
        }
        let dt = dt.max(1e-3);
        let tau = tuning.tau.max(1e-3);
        // Critical damping: kP = 1/tau^2, kD = 2/tau.
        let kp = 1.0 / (tau * tau);
        let kd = 2.0 / tau;
        // Semi-implicit Euler with sub-stepping for stability at any tau.
        let steps = ((dt / tau).ceil() as usize).clamp(1, 8);
        let h = dt / steps as f64;
        for _ in 0..steps {
            let acc = kp * (target - self.pos) - kd * self.vel;
            self.vel += acc * h;
            self.pos += self.vel * h;
        }
        // Anti-shimmer: inside the dead band the halo HOLDS its position
        // (does not track the noisy target). Snapping to the target would
        // just re-introduce the noise as sub-band jitter.
        let err = (target - self.pos).abs();
        if err < tuning.dead_band_px && self.vel.abs() < tuning.dead_band_px * 8.0 {
            self.vel = 0.0;
        }
        self.pos
    }

    /// Current servo position.
    pub fn position(&self) -> f64 {
        self.pos
    }

    /// Current servo velocity (logical px/s) — the engine's speed metric.
    pub fn velocity(&self) -> f64 {
        self.vel
    }

    /// Forget state (face lost / re-enable): the next call snaps again.
    pub fn reset(&mut self) {
        self.has_state = false;
        self.pos = 0.0;
        self.vel = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning() -> ServoTuning {
        ServoTuning::from_smooth_ms(140.0)
    }

    #[test]
    fn first_target_snaps() {
        let mut s = ServoAxis::new();
        assert_eq!(s.step(321.0, 1.0 / 60.0, &tuning()), 321.0);
    }

    #[test]
    fn step_target_settles_without_overshoot() {
        let mut s = ServoAxis::new();
        s.step(0.0, 1.0 / 60.0, &tuning());
        let mut peak = 0.0f64;
        for _ in 0..120 {
            let p = s.step(200.0, 1.0 / 60.0, &tuning());
            peak = peak.max(p);
        }
        // Critically damped: no meaningful overshoot past the target...
        assert!(peak <= 205.0, "overshoot peak {peak}");
        // ...and settled within the dead band of the target well under 2 s.
        assert!((s.position() - 200.0).abs() < 2.0);
    }

    #[test]
    fn settle_time_matches_tau_scale() {
        let mut s = ServoAxis::new();
        s.step(0.0, 1.0 / 60.0, &tuning());
        let mut t10 = None;
        let mut t90 = None;
        for i in 0..180 {
            let p = s.step(300.0, 1.0 / 60.0, &tuning());
            let frac = p / 300.0;
            if t10.is_none() && frac >= 0.1 {
                t10 = Some(i);
            }
            if t90.is_none() && frac >= 0.9 {
                t90 = Some(i);
            }
        }
        // tau = 140 ms, critically damped: t10 ~0.53*tau ~5 ticks, t90
        // ~3.9*tau ~33 ticks at 60 Hz.
        let t10 = t10.expect("reaches 10%");
        let t90 = t90.expect("reaches 90%");
        assert!(t10 >= 1 && t10 <= 8, "t10 tick {t10}");
        assert!(t90 <= 42, "t90 tick {t90}");
    }

    #[test]
    fn thirty_hz_staircase_becomes_smooth_sixty_hz() {
        // 30 Hz samples of a fast sweep; the servo output at 60 Hz must move
        // in bounded increments (no full-sample jumps).
        let mut s = ServoAxis::new();
        s.step(0.0, 1.0 / 60.0, &tuning());
        let dt = 1.0 / 60.0;
        let mut prev = 0.0;
        let mut max_jump = 0.0f64;
        for i in 0..120 {
            let target = if i % 2 == 0 { (i as f64 / 2.0) * 12.0 } else { ((i - 1) as f64 / 2.0) * 12.0 };
            let p = s.step(target, dt, &tuning());
            max_jump = max_jump.max((p - prev).abs());
            prev = p;
        }
        // Each 30 Hz step is 12 px; the servo must split it into smaller
        // 60 Hz increments instead of jumping the whole sample delta.
        assert!(max_jump < 10.0, "max per-tick jump {max_jump}");
    }

    #[test]
    fn stationary_noisy_target_does_not_shimmer() {
        let mut s = ServoAxis::new();
        s.step(100.0, 1.0 / 60.0, &tuning());
        let mut seed = 7u64;
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..200 {
            let noise = (((seed.wrapping_mul(6364136223846793005)) >> 33) as f64
                / (u64::MAX >> 33) as f64)
                * 1.6
                - 0.8;
            seed = seed.wrapping_add(1);
            let p = s.step(100.0 + noise, 1.0 / 60.0, &tuning());
            distinct.insert((p * 10.0).round() as i64);
        }
        // With sub-pixel noise the halo must settle essentially still.
        assert!(distinct.len() <= 3, "distinct positions: {}", distinct.len());
    }

    #[test]
    fn reset_forgets_then_snaps() {
        let mut s = ServoAxis::new();
        s.step(50.0, 1.0 / 60.0, &tuning());
        s.reset();
        assert_eq!(s.step(400.0, 1.0 / 60.0, &tuning()), 400.0);
    }
}
