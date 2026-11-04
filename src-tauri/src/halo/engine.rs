//! FocusHalo tracking engine thread: drains head-pose samples, filters and
//! maps them to screen coordinates, resolves the visual phase, and drives the
//! overlay at ~60 Hz.
//!
//! The per-tick logic lives in the pure [`step`] function (thread-free, fully
//! unit-testable); `spawn` only wires timers, channels, and the overlay.

use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::halo::filter::OneEuroFilter;
use crate::halo::input_probe::InputActivityProbe;
use crate::halo::mapping::{map_pose, CalibrationProfile};
use crate::halo::overlay::{HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry};
use crate::halo::params::{CameraPosition, FocusHaloSettings};
use crate::halo::tracker::HeadSample;

/// Suspend the halo after the face has been missing this long.
const FACE_LOST_SUSPEND_MS: u64 = 4000;
/// Resume from input-suspension after this much input silence.
const INPUT_QUIET_RESUME_MS: u64 = 1000;
/// Engine tick rate: 1/60 s (16.666667 ms), as nanos because
/// `Duration::from_secs_f64` is not const-callable.
const TICK_PERIOD: Duration = Duration::from_nanos(16_666_667);
/// dt guard rails for filter feeding (timestamps can glitch).
const MIN_DT: f64 = 0.001;
const MAX_DT: f64 = 1.0;

/// Coarse engine phase as consumed by the debug channel (P1-D). Both
/// suspension reasons render as `HaloVisual::Suspended`; the split exists so
/// telemetry can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnginePhase {
    Idle,
    /// No face for >= 4 s.
    SuspendedFace,
    /// Physical input seen within the last second.
    SuspendedInput,
}

/// Per-tick telemetry snapshot consumed by the P1-D debug stream. Sent on a
/// bounded channel; if the consumer stalls, snapshots are dropped, never
/// back-pressured into the engine loop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugSnapshot {
    pub ts_ms: u64,
    pub face_detected: bool,
    pub raw_yaw: f64,
    pub raw_pitch: f64,
    pub filtered_yaw: f64,
    pub filtered_pitch: f64,
    pub norm_x: f64,
    pub norm_y: f64,
    /// Normalized-coordinate displacement per second, measured in screen
    /// width units (y is scaled by the aspect ratio). Feeds enterSpeed (P2).
    pub speed_norm: f64,
    pub phase: EnginePhase,
}

/// Engine wiring snapshot handed to [`spawn`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub settings: FocusHaloSettings,
    pub calibration: CalibrationProfile,
    pub screen: OverlayScreenGeometry,
}

/// Commands the engine thread accepts while running.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    /// Stop the thread (overlay hidden on exit).
    Shutdown,
    /// Replace the sanitized settings snapshot (dead zone / gain / camera
    /// position / keyboard suspends take effect next tick).
    SetSettings(FocusHaloSettings),
    /// Replace the calibration profile.
    SetCalibration(CalibrationProfile),
    /// Screen moved/resized/scale changed.
    SetScreen(OverlayScreenGeometry),
}

/// All mutable engine state, threaded through the pure [`step`].
pub(crate) struct EngineState {
    pub(crate) settings: FocusHaloSettings,
    pub(crate) calibration: CalibrationProfile,
    pub(crate) screen: OverlayScreenGeometry,
    pub(crate) yaw_filter: OneEuroFilter,
    pub(crate) pitch_filter: OneEuroFilter,
    /// ts of the last accepted sample, for filter dt.
    last_sample_ms: Option<u64>,
    /// Face state as of the last sample; starts false so an engine without a
    /// camera feed honestly reports "no face" instead of faking an active ring.
    pub(crate) face_detected: bool,
    /// Tick time when the face was first observed missing (None = present).
    face_absent_since: Option<u64>,
    /// Tick time of the last physical input activity (None = none seen yet).
    last_input_active_ms: Option<u64>,
    pub(crate) phase: EnginePhase,
    /// Latest normalized screen position (0..=1).
    pub(crate) norm: (f64, f64),
    pub(crate) speed_norm: f64,
    prev_norm: Option<(f64, f64)>,
    prev_norm_ts: Option<u64>,
    raw: (f64, f64),
    filtered: (f64, f64),
}

impl EngineState {
    pub(crate) fn new(config: &EngineConfig) -> Self {
        EngineState {
            settings: config.settings.clone(),
            calibration: config.calibration,
            screen: config.screen,
            yaw_filter: OneEuroFilter::head_pose(),
            pitch_filter: OneEuroFilter::head_pose(),
            last_sample_ms: None,
            face_detected: false,
            face_absent_since: None,
            last_input_active_ms: None,
            phase: EnginePhase::Idle,
            norm: (0.5, 0.5),
            speed_norm: 0.0,
            prev_norm: None,
            prev_norm_ts: None,
            raw: (0.0, 0.0),
            filtered: (0.0, 0.0),
        }
    }
}

/// One engine tick. `sample` is the newest head-pose sample since the last
/// tick (or None); `now_ms` is monotonic tick time; `input_active` is the
/// probe reading for this tick. Returns the visual to render, the halo center
/// in screen-local logical coordinates, and the telemetry snapshot.
pub(crate) fn step(
    state: &mut EngineState,
    sample: Option<HeadSample>,
    now_ms: u64,
    input_active: bool,
) -> (HaloVisual, (f64, f64), DebugSnapshot) {
    if let Some(sample) = sample {
        // dt for the filters comes from sample timestamps (capture rate, not
        // tick rate); clamped so clock glitches cannot destabilize them.
        let dt = match state.last_sample_ms {
            Some(prev) => (sample.ts_ms.saturating_sub(prev) as f64 / 1000.0).clamp(MIN_DT, MAX_DT),
            // First sample: any dt works, the filter passes through anyway.
            None => MIN_DT,
        };
        state.last_sample_ms = Some(sample.ts_ms);
        state.face_detected = sample.face_detected;

        if sample.face_detected {
            let fy = state.yaw_filter.filter(sample.yaw_deg, dt);
            let fp = state.pitch_filter.filter(sample.pitch_deg, dt);
            state.raw = (sample.yaw_deg, sample.pitch_deg);
            state.filtered = (fy, fp);
            state.norm = map_pose(
                (fy, fp),
                &state.calibration,
                state.settings.dead_zone_percent,
                state.settings.gain,
                state.settings.camera_position == CameraPosition::Bottom,
            );
            // speed_norm: displacement per second in screen-width units
            // (vertical displacement scaled by aspect) — feeds enterSpeed.
            let aspect = if state.screen.width > 0.0 {
                state.screen.height / state.screen.width
            } else {
                1.0
            };
            if let (Some((px, py)), Some(pt)) = (state.prev_norm, state.prev_norm_ts) {
                let dt_s = (sample.ts_ms.saturating_sub(pt) as f64 / 1000.0).clamp(MIN_DT, MAX_DT);
                let dx = state.norm.0 - px;
                let dy = (state.norm.1 - py) * aspect;
                state.speed_norm = (dx * dx + dy * dy).sqrt() / dt_s;
            }
            state.prev_norm = Some(state.norm);
            state.prev_norm_ts = Some(sample.ts_ms);
        } else {
            // Face lost: pose is zero (maps to center). Freeze the filters
            // (feeding them zeros would drag their memory toward the center
            // and cause a snap when the face returns) and reset the speed
            // baseline so the return jump is not measured as velocity.
            state.raw = (0.0, 0.0);
            state.filtered = (0.0, 0.0);
            state.norm = (0.5, 0.5);
            state.speed_norm = 0.0;
            state.prev_norm = None;
            state.prev_norm_ts = None;
        }
    }

    // Face-absence timing runs on tick time.
    if state.face_detected {
        state.face_absent_since = None;
    } else {
        state.face_absent_since.get_or_insert(now_ms);
    }
    if input_active {
        state.last_input_active_ms = Some(now_ms);
    }

    let input_suspended = state
        .last_input_active_ms
        .is_some_and(|t| now_ms.saturating_sub(t) < INPUT_QUIET_RESUME_MS);
    let face_suspended = state
        .face_absent_since
        .is_some_and(|t| now_ms.saturating_sub(t) >= FACE_LOST_SUSPEND_MS);
    let phase = if input_suspended {
        EnginePhase::SuspendedInput
    } else if face_suspended {
        EnginePhase::SuspendedFace
    } else {
        EnginePhase::Idle
    };
    state.phase = phase;

    let local = (
        state.norm.0 * state.screen.width,
        state.norm.1 * state.screen.height,
    );
    let visual = match phase {
        EnginePhase::Idle => HaloVisual::Idle,
        EnginePhase::SuspendedFace | EnginePhase::SuspendedInput => HaloVisual::Suspended,
    };
    let snapshot = DebugSnapshot {
        ts_ms: now_ms,
        face_detected: state.face_detected,
        raw_yaw: state.raw.0,
        raw_pitch: state.raw.1,
        filtered_yaw: state.filtered.0,
        filtered_pitch: state.filtered.1,
        norm_x: state.norm.0,
        norm_y: state.norm.1,
        speed_norm: state.speed_norm,
        phase,
    };
    (visual, local, snapshot)
}

/// Spawn the engine thread. Returns its handle, the command sender, and the
/// debug snapshot receiver (bounded; snapshots are dropped while the consumer
/// lags behind, so the engine never blocks on telemetry).
pub fn spawn(
    config: EngineConfig,
    sample_rx: Receiver<HeadSample>,
    overlay: Box<dyn OverlayBackend>,
    probe: Box<dyn InputActivityProbe>,
) -> (
    JoinHandle<()>,
    Sender<EngineCommand>,
    Receiver<DebugSnapshot>,
) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (debug_tx, debug_rx) = std::sync::mpsc::sync_channel(64);
    let state = EngineState::new(&config);
    let handle = std::thread::Builder::new()
        .name("focus-halo-engine".into())
        .spawn(move || run_loop(state, sample_rx, cmd_rx, overlay, probe, debug_tx))
        .expect("failed to spawn focus-halo engine thread");
    (handle, cmd_tx, debug_rx)
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    mut state: EngineState,
    sample_rx: Receiver<HeadSample>,
    cmd_rx: Receiver<EngineCommand>,
    mut overlay: Box<dyn OverlayBackend>,
    mut probe: Box<dyn InputActivityProbe>,
    debug_tx: SyncSender<DebugSnapshot>,
) {
    overlay.set_screen(state.screen);
    probe.set_keyboard_suspends(state.settings.keyboard_suspends);
    let mut overlay_visible = false;
    // The frame is only rebuilt when the visual or opacity changes; otherwise
    // the same cached frame is reused so ticks just update the position.
    let mut frame = HaloFrame {
        visual: HaloVisual::Idle,
        opacity: state.settings.halo_opacity,
    };
    let mut debug_connected = true;
    // Engine-own monotonic clock: face/input timers and snapshot timestamps
    // all use this, so wall-clock (NTP) jumps cannot distort the 4 s / 1 s
    // suspension windows.
    let tick_epoch = Instant::now();
    let mut next_tick = Instant::now();

    loop {
        // 1. Commands (drain).
        let mut shutdown = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCommand::Shutdown => {
                    shutdown = true;
                }
                EngineCommand::SetSettings(settings) => {
                    state.settings = settings.sanitized();
                    probe.set_keyboard_suspends(state.settings.keyboard_suspends);
                }
                EngineCommand::SetCalibration(calibration) => {
                    state.calibration = calibration;
                }
                EngineCommand::SetScreen(screen) => {
                    state.screen = screen;
                    overlay.set_screen(screen);
                }
            }
        }
        if shutdown {
            break;
        }

        // 2. Drain samples, keep the newest (a stalled producer must not
        //    build a backlog; intermediate samples carry no extra information).
        let mut latest = None;
        loop {
            match sample_rx.try_recv() {
                Ok(s) => latest = Some(s),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Producer gone: nothing more to track, exit cleanly.
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }

        // 3. Probe + tick.
        let input_active = probe.physical_input_active();
        let now_ms = tick_epoch.elapsed().as_millis() as u64;
        let (visual, local, snapshot) = step(&mut state, latest, now_ms, input_active);

        // 4. Drive the overlay. The frame object is only rebuilt when the
        //    visual or opacity actually changed; unchanged ticks just push
        //    the new position with the same cached frame.
        if frame.visual != visual || frame.opacity != state.settings.halo_opacity {
            frame = HaloFrame {
                visual,
                opacity: state.settings.halo_opacity,
            };
        }
        if !overlay_visible {
            overlay.show();
            overlay_visible = true;
        }
        overlay.update(local, &frame);

        // 5. Telemetry: drop on lag, stop entirely when the consumer is gone.
        if debug_connected {
            match debug_tx.try_send(snapshot) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => debug_connected = false,
            }
        }

        // 6. Pace to 60 Hz (resync when a tick overran its budget).
        next_tick += TICK_PERIOD;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            next_tick = now;
        }
    }

    overlay.hide();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::halo::params::FocusHaloSettings;

    fn test_screen() -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
            scale: 1.0,
            native_x: 0.0,
            native_y: 0.0,
        }
    }

    fn test_state() -> EngineState {
        EngineState::new(&EngineConfig {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            screen: test_screen(),
        })
    }

    fn sample(ts_ms: u64, face: bool, yaw: f64, pitch: f64) -> HeadSample {
        HeadSample {
            ts_ms,
            face_detected: face,
            yaw_deg: yaw,
            pitch_deg: pitch,
            roll_deg: 0.0,
        }
    }

    #[test]
    fn face_loss_suspends_after_4s_and_recovers_immediately() {
        let mut s = test_state();
        // Face present at t=0.
        let (visual, _, _) = step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        assert_eq!(visual, HaloVisual::Idle);
        // Ticks with no sample while face was seen stay Idle.
        let (visual, _, _) = step(&mut s, None, 100, false);
        assert_eq!(visual, HaloVisual::Idle);
        // Face disappears at t=200.
        let (visual, _, _) = step(&mut s, Some(sample(200, false, 0.0, 0.0)), 200, false);
        assert_eq!(visual, HaloVisual::Idle, "absence only just started");
        // 3.9 s after disappearance: still Idle.
        let (visual, _, _) = step(&mut s, None, 200 + 3900, false);
        assert_eq!(visual, HaloVisual::Idle);
        // 4.1 s after disappearance: Suspended.
        let (visual, _, _) = step(&mut s, None, 200 + 4100, false);
        assert_eq!(visual, HaloVisual::Suspended);
        // Face returns: Idle immediately, timer cleared.
        let (visual, _, _) = step(&mut s, Some(sample(4400, true, 0.0, 0.0)), 4400, false);
        assert_eq!(visual, HaloVisual::Idle);
        // And it stays Idle afterwards (timer really reset, not pending).
        let (visual, _, _) = step(&mut s, None, 9000, false);
        assert_eq!(visual, HaloVisual::Idle);
    }

    #[test]
    fn input_activity_suspends_and_quiet_1s_resumes() {
        let mut s = test_state();
        let (visual, _, _) = step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, true);
        assert_eq!(
            visual,
            HaloVisual::Suspended,
            "physical input suspends at once"
        );
        let (visual, _, _) = step(&mut s, None, 900, false);
        assert_eq!(
            visual,
            HaloVisual::Suspended,
            "still within the 1s quiet window"
        );
        let (visual, _, _) = step(&mut s, None, 1100, false);
        assert_eq!(visual, HaloVisual::Idle, "quiet for 1.1s resumes");
    }

    #[test]
    fn input_beats_face_but_face_suspends_when_input_gone() {
        let mut s = test_state();
        step(&mut s, Some(sample(0, false, 0.0, 0.0)), 0, false);
        // Long absence + input activity: input wins (SuspendedInput semantics
        // still render Suspended; the debug snapshot carries the distinction).
        let (_, _, snap) = step(&mut s, None, 10_000, true);
        assert_eq!(snap.phase, EnginePhase::SuspendedInput);
        // Input goes quiet, face still absent > 4s: face suspension shows.
        let (_, _, snap) = step(&mut s, None, 11_200, false);
        assert_eq!(snap.phase, EnginePhase::SuspendedFace);
    }

    #[test]
    fn position_is_mapping_result_scaled_to_screen() {
        let mut s = test_state();
        // Right gaze extreme (yaw = 18 with default calibration) -> x = 1.0,
        // pitch centered -> y = 0.5, on a 1000x500 screen.
        let (_, local, snap) = step(&mut s, Some(sample(0, true, 18.0, 0.0)), 0, false);
        assert!((local.0 - 1000.0).abs() < 1e-9, "local {local:?}");
        assert!((local.1 - 250.0).abs() < 1e-9, "local {local:?}");
        assert!((snap.norm_x - 1.0).abs() < 1e-9);
        assert!((snap.norm_y - 0.5).abs() < 1e-9);
        assert_eq!(snap.face_detected, true);
        // First sample passes through the filter unchanged.
        assert_eq!(snap.filtered_yaw, 18.0);
        assert_eq!(snap.raw_yaw, 18.0);
    }

    #[test]
    fn face_loss_pulls_position_to_center_and_zeroes_speed() {
        let mut s = test_state();
        step(&mut s, Some(sample(0, true, 18.0, 0.0)), 0, false);
        let (_, local, snap) = step(&mut s, Some(sample(33, false, 0.0, 0.0)), 33, false);
        assert_eq!(local, (500.0, 250.0), "no face maps to screen center");
        assert_eq!(snap.speed_norm, 0.0);
        assert!(!snap.face_detected);
    }

    #[test]
    fn speed_norm_measures_screen_width_units_per_second() {
        let mut s = test_state();
        // Zero dead zone, linear gain, and a yaw ramp from center to the
        // right extreme over 2 s at 30 Hz: at steady state the filtered
        // position moves 0.5 width-units (norm 0.5 -> 1.0) per 2 s, i.e.
        // 0.25 width-units/s. Sampled mid-ramp (before the norm clamps).
        s.settings.dead_zone_percent = 0.0;
        s.settings.gain = 1.0;
        let dt_ms = 33;
        let mut snap = None;
        for i in 0..=60 {
            let ts = (i * dt_ms) as u64; // last sample at 1980 ms, still on ramp
            let yaw = 18.0 * (ts as f64 / 2000.0);
            let (_, _, s_snapshot) = step(&mut s, Some(sample(ts, true, yaw, 0.0)), ts, false);
            snap = Some(s_snapshot);
        }
        let snap = snap.unwrap();
        assert!(
            (snap.speed_norm - 0.25).abs() < 0.1,
            "speed {} should approach 0.25 width-units/s on the ramp",
            snap.speed_norm
        );
    }

    #[test]
    fn constants_match_design() {
        assert_eq!(FACE_LOST_SUSPEND_MS, 4000);
        assert_eq!(INPUT_QUIET_RESUME_MS, 1000);
        assert!((TICK_PERIOD.as_secs_f64() - 1.0 / 60.0).abs() < 1e-6);
    }
}
