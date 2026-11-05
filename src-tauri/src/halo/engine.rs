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

use crate::halo::activator::WindowActivator;
use crate::halo::kalman::Kalman1D;
use crate::halo::smoothing::{ServoAxis, ServoTuning};
use crate::halo::input_probe::InputActivityProbe;
use crate::halo::mapping::{map_pose, CalibrationProfile};
use crate::halo::overlay::{
    BorderFrame, HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry,
};
use crate::halo::params::{CameraPosition, FocusHaloSettings};
use crate::halo::state_machine::{advance, DwellConfig, DwellInput, DwellPhase, DwellState};
use crate::halo::tracker::HeadSample;
use crate::halo::windows_layer::WindowService;

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
/// Emergency stop suspends the whole pipeline this long after the command
/// (product: the hotkey buys 5 s of guaranteed quiet).
pub(crate) const EMERGENCY_LOCK_MS: u64 = 5000;
/// Undo-stack depth cap: the 8 most recent switches stay undoable.
pub(crate) const UNDO_STACK_MAX: usize = 8;

/// One undoable switch: the foreground window the halo took the user away
/// from, and when the switch fired. `previous_foreground_id` is `None` when
/// the foreground window was not in the enumerable set (or there was none) —
/// undoing such an entry is a no-op pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UndoEntry {
    pub(crate) previous_foreground_id: Option<u64>,
    pub(crate) switched_at_ms: u64,
}

/// Whether the emergency lock is up at `now_ms`: `until` is the lock's
/// deadline (engine tick time), and a clock glitch backwards simply reads as
/// "deadline passed" (no panic, no permanent lock).
pub(crate) fn emergency_locked(until: Option<u64>, now_ms: u64) -> bool {
    until.is_some_and(|deadline| now_ms < deadline)
}

/// Push an undo entry with the depth cap: past [`UNDO_STACK_MAX`] the oldest
/// entries fall off the front.
pub(crate) fn undo_push(stack: &mut Vec<UndoEntry>, entry: UndoEntry) {
    stack.push(entry);
    if stack.len() > UNDO_STACK_MAX {
        let overflow = stack.len() - UNDO_STACK_MAX;
        stack.drain(0..overflow);
    }
}

/// Undo-stack top consumption: pops and discards expired tops first (each
/// older than `undo_ms` at `now_ms`), then pops the top when it is still
/// fresh. Returns the popped entry's `previous_foreground_id` (`Some(None)` =
/// recorded switch whose undo is a no-op), or `None` when nothing was
/// undoable (empty stack / every entry expired).
pub(crate) fn undo_take(
    stack: &mut Vec<UndoEntry>,
    now_ms: u64,
    undo_ms: u64,
) -> Option<Option<u64>> {
    while let Some(top) = stack.last() {
        if now_ms.saturating_sub(top.switched_at_ms) <= undo_ms {
            break;
        }
        stack.pop();
    }
    stack.pop().map(|entry| entry.previous_foreground_id)
}

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
    /// Window currently being settled or dwelled on (P2 dwell machine);
    /// `None` when idle/cooldown or when no window service is attached.
    pub candidate: Option<u64>,
    /// Dwell machine stage (`idle`/`settling`/`candidate`/`switched`/
    /// `cooldown`) for the debug panel. `switched` appears only on the tick a
    /// switch event fired.
    pub phase_str: &'static str,
    /// Border the overlay should show this tick (before deduplication);
    /// `Some` exactly while a candidate is being dwelled on.
    pub border: Option<BorderFrame>,
}

/// Engine wiring snapshot handed to [`spawn`]. Not `Clone`/`Debug` because it
/// owns the platform window service (a `Box<dyn WindowService>`).
pub struct EngineConfig {
    pub settings: FocusHaloSettings,
    pub calibration: CalibrationProfile,
    pub screen: OverlayScreenGeometry,
    /// Window enumeration/hit-test layer (P2). `None` bypasses the dwell
    /// machine entirely (pure-follow mode): used by halo_manual and by
    /// platforms without a window layer, where the halo just follows.
    pub window_service: Option<Box<dyn WindowService>>,
    /// Window activation backend (P3): consumes the dwell machine's switch
    /// events and undo requests. Always present — even a placeholder
    /// activator keeps the engine logic uniform.
    pub activator: Box<dyn WindowActivator>,
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
    /// Undo the most recent switch if it fired within `undo_ms` (P3-B undo
    /// hotkey lands here via HaloRuntime::undo_switch).
    UndoSwitch,
    /// Emergency stop: suspend the pipeline for [`EMERGENCY_LOCK_MS`] and
    /// clear the undo stack (P3-B emergency hotkey lands here via
    /// HaloRuntime::emergency_stop).
    EmergencyStop,
}

/// All mutable engine state, threaded through the pure [`step`].
pub(crate) struct EngineState {
    pub(crate) settings: FocusHaloSettings,
    pub(crate) calibration: CalibrationProfile,
    pub(crate) screen: OverlayScreenGeometry,
    pub(crate) yaw_kalman: Kalman1D,
    pub(crate) pitch_kalman: Kalman1D,
    /// ts of the last accepted sample, for Kalman dt.
    last_sample_ms: Option<u64>,
    /// ts of the previous engine tick, for the render servo dt.
    last_tick_ms: Option<u64>,
    /// Critically-damped render servos (60 Hz) toward the mapped target:
    /// they interpolate the 30 Hz Kalman output and kill sub-pixel shimmer.
    servo_x: ServoAxis,
    servo_y: ServoAxis,
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
    raw: (f64, f64),
    filtered: (f64, f64),
    /// P2 dwell/switch machine; advanced only when a window service exists.
    dwell: DwellState,
    /// Border currently pushed to the overlay, for set_border deduplication
    /// (unchanged states must not re-dispatch the call every tick).
    current_border: Option<BorderFrame>,
    /// `None` (halo_manual, unsupported platforms) keeps the engine in
    /// pure-follow mode: no hit-tests, dwell machine bypassed.
    window_service: Option<Box<dyn WindowService>>,
    /// P3 activation backend for switch events and undo requests.
    pub(crate) activator: Box<dyn WindowActivator>,
    /// Most recent switches, newest last (`undo_push` keeps at most
    /// [`UNDO_STACK_MAX`]).
    pub(crate) undo_stack: Vec<UndoEntry>,
    /// Emergency-lock deadline in tick time; `None`/past deadline = unlocked.
    pub(crate) emergency_until: Option<u64>,
}

impl EngineState {
    pub(crate) fn new(config: EngineConfig) -> Self {
        EngineState {
            settings: config.settings,
            calibration: config.calibration,
            screen: config.screen,
            yaw_kalman: Kalman1D::head_pose(),
            pitch_kalman: Kalman1D::head_pose(),
            last_sample_ms: None,
            last_tick_ms: None,
            servo_x: ServoAxis::new(),
            servo_y: ServoAxis::new(),
            face_detected: false,
            face_absent_since: None,
            last_input_active_ms: None,
            phase: EnginePhase::Idle,
            norm: (0.5, 0.5),
            speed_norm: 0.0,
            raw: (0.0, 0.0),
            filtered: (0.0, 0.0),
            dwell: DwellState::default(),
            current_border: None,
            window_service: config.window_service,
            activator: config.activator,
            undo_stack: Vec::new(),
            emergency_until: None,
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
    // Servo dt runs on tick time (60 Hz), independent of the ~30 Hz samples.
    let tick_dt = match state.last_tick_ms {
        Some(prev) => (now_ms.saturating_sub(prev) as f64 / 1000.0).clamp(MIN_DT, MAX_DT),
        None => 1.0 / 60.0,
    };
    state.last_tick_ms = Some(now_ms);

    if let Some(sample) = sample {
        // dt for the Kalman filters comes from sample timestamps (capture
        // rate, not tick rate); clamped so clock glitches cannot destabilize.
        let dt = match state.last_sample_ms {
            Some(prev) => (sample.ts_ms.saturating_sub(prev) as f64 / 1000.0).clamp(MIN_DT, MAX_DT),
            // First sample: any dt works, the filter passes through anyway.
            None => MIN_DT,
        };
        state.last_sample_ms = Some(sample.ts_ms);
        state.face_detected = sample.face_detected;

        if sample.face_detected {
            let (fy, _) = state.yaw_kalman.update(sample.yaw_deg, dt);
            let (fp, _) = state.pitch_kalman.update(sample.pitch_deg, dt);
            state.raw = (sample.yaw_deg, sample.pitch_deg);
            state.filtered = (fy, fp);
            state.norm = map_pose(
                (fy, fp),
                &state.calibration,
                state.settings.dead_zone_percent,
                state.settings.gain,
                state.settings.camera_position == CameraPosition::Bottom,
                state.settings.invert_pitch,
            );
        } else {
            // Face lost: reset the Kalman filters (a returning face must not
            // be dragged through stale velocity); the target becomes screen
            // center and the render servo glides there smoothly.
            state.yaw_kalman.reset();
            state.pitch_kalman.reset();
            state.raw = (0.0, 0.0);
            state.filtered = (0.0, 0.0);
            state.norm = (0.5, 0.5);
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

    // Render servo (P+D, critically damped): interpolates the 30 Hz Kalman
    // target into smooth 60 Hz motion and suppresses sub-pixel shimmer.
    // speed_norm comes from the servo velocity (screen-width units per
    // second, vertical scaled by aspect) — feeds enterSpeed.
    let tuning = ServoTuning::from_smooth_ms(state.settings.smooth_ms as f64);
    let target_x = state.norm.0 * state.screen.width;
    let target_y = state.norm.1 * state.screen.height;
    let local = (
        state.servo_x.step(target_x, tick_dt, &tuning),
        state.servo_y.step(target_y, tick_dt, &tuning),
    );
    let aspect = if state.screen.width > 0.0 {
        state.screen.height / state.screen.width
    } else {
        1.0
    };
    let vx = state.servo_x.velocity() / state.screen.width.max(1.0);
    let vy = state.servo_y.velocity() * aspect / state.screen.width.max(1.0);
    state.speed_norm = (vx * vx + vy * vy).sqrt();

    // P3 emergency lock: another suspend source next to face/input — the
    // dwell machine gets suspend=true (visual Suspended, no timing), so no
    // new candidates or switches can fire while the lock is up.
    let emergency = emergency_locked(state.emergency_until, now_ms);

    // P2 dwell machine: one hit-test per tick (the window service caches its
    // enumeration for 300 ms) feeds the pure state machine. `None` service
    // bypasses it entirely — pure-follow mode for halo_manual and platforms
    // without a window layer.
    let (dwell_phase, hit_frame, switch_event) = match state.window_service.as_mut() {
        None => (DwellPhase::Idle, None, None),
        Some(service) => {
            // WindowInfo frames and hit_test live in the layout board's
            // logical space, so the halo center is converted from
            // screen-local to board coordinates (identical when the tracked
            // screen sits at the board origin).
            let board = (state.screen.x + local.0, state.screen.y + local.1);
            let hit = service.hit_test(board.0, board.1).ok().flatten();
            let hit_frame = hit.as_ref().map(|window| window.frame);
            let cfg = DwellConfig::from_settings(&state.settings);
            let input = DwellInput {
                now_ms,
                halo_local: board,
                speed_norm: state.speed_norm,
                hit: hit.as_ref(),
                foreground_id: None,
                suspend: phase != EnginePhase::Idle || emergency,
            };
            let (phase_out, event) = advance(&mut state.dwell, &input, &cfg);
            (phase_out, hit_frame, event)
        }
    };

    // P3 switch consumption: capture the pre-switch foreground, push the
    // undo entry, then activate. Runs outside the window-service borrow
    // above so the activator call stays a disjoint field borrow.
    // Product note: after a switch the halo ring and the mouse stay exactly
    // where they are — no pointer move, no overlay relocation — which needs
    // no action here (nothing else in the tick touches them); activation is
    // the sole side effect.
    if let Some(event) = switch_event {
        log::info!("halo: dwell switch fired window={}", event.window_id);
        // Previous foreground, captured BEFORE activation (the enumerate is
        // served from the same 300 ms cache as the hit-test, so the mark may
        // lag reality by up to the TTL — accepted for undo purposes).
        // `None` = foreground not in the enumerable set: undo becomes a
        // no-op pop for this entry.
        let previous_foreground = match state.window_service.as_mut() {
            Some(service) => service.enumerate().ok().and_then(|windows| {
                windows
                    .iter()
                    .find(|window| window.is_foreground)
                    .map(|w| w.id)
            }),
            None => None,
        };
        undo_push(
            &mut state.undo_stack,
            UndoEntry {
                previous_foreground_id: previous_foreground,
                switched_at_ms: now_ms,
            },
        );
        let outcome = state.activator.activate(event.window_id);
        log::info!(
            "halo: switch activate window={} activated={} fell_back={} detail={}",
            event.window_id,
            outcome.activated,
            outcome.fell_back,
            outcome.detail
        );
    }

    // Candidate-window border: wanted exactly while dwelling on a candidate,
    // following the hit window's frame each tick (a moving/resized window
    // keeps the outline glued to it). run_loop deduplicates the set_border
    // calls via `border_command`.
    let wanted_border = border_for_phase(dwell_phase, hit_frame);

    let visual = if emergency {
        // Emergency lock renders as Suspended regardless of face/input state.
        HaloVisual::Suspended
    } else {
        match phase {
            EnginePhase::SuspendedFace | EnginePhase::SuspendedInput => HaloVisual::Suspended,
            EnginePhase::Idle => match dwell_phase {
                DwellPhase::Candidate { progress, .. } => HaloVisual::Candidate {
                    // show_progress=false keeps the candidate ring but hides
                    // the arc (progress 0 renders no sweep).
                    progress: if state.settings.show_progress {
                        progress
                    } else {
                        0.0
                    },
                },
                // Transient: the tick a switch fired flashes the ring.
                DwellPhase::Switched { .. } => HaloVisual::SwitchPulse,
                DwellPhase::Idle | DwellPhase::Cooldown => HaloVisual::Idle,
            },
        }
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
        candidate: match dwell_phase {
            DwellPhase::Candidate { window_id, .. } | DwellPhase::Switched { window_id } => {
                Some(window_id)
            }
            // A settling window counts as the pending candidate for the
            // debug panel ("why is nothing happening yet").
            DwellPhase::Idle | DwellPhase::Cooldown => state.dwell.candidate_window(),
        },
        phase_str: match dwell_phase {
            DwellPhase::Switched { .. } => "switched",
            _ => state.dwell.debug_phase_str(),
        },
        border: wanted_border,
    };
    (visual, local, snapshot)
}

/// Border decision: a border is wanted exactly while a candidate is being
/// dwelled on, wrapping the hit window's board frame (re-checked every tick
/// so a moving/resized candidate keeps the outline glued to it). Settling
/// does not draw anything yet; `Switched`/`Cooldown`/`Idle` clear it.
pub(crate) fn border_for_phase(
    phase: DwellPhase,
    hit_frame: Option<(f64, f64, f64, f64)>,
) -> Option<BorderFrame> {
    match phase {
        DwellPhase::Candidate { .. } => hit_frame.map(|(x, y, w, h)| BorderFrame { x, y, w, h }),
        DwellPhase::Idle | DwellPhase::Switched { .. } | DwellPhase::Cooldown => None,
    }
}

/// Deduplicated overlay border command for one tick: `None` when the wanted
/// border is unchanged from `current` (no set_border call), otherwise the
/// value to hand to `OverlayBackend::set_border` — `Some(None)` inside clears
/// it. `current` is updated in place.
fn border_command(
    current: &mut Option<BorderFrame>,
    wanted: Option<BorderFrame>,
) -> Option<Option<BorderFrame>> {
    if *current == wanted {
        return None;
    }
    *current = wanted;
    Some(wanted)
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
    let state = EngineState::new(config);
    let handle = std::thread::Builder::new()
        .name("focus-halo-engine".into())
        .spawn(move || run_loop(state, sample_rx, cmd_rx, overlay, probe, debug_tx))
        .expect("failed to spawn focus-halo engine thread");
    (handle, cmd_tx, debug_rx)
}

/// One command dispatch, extracted from [`run_loop`] so the command paths
/// (undo, emergency stop) are unit-testable without a live thread. Returns
/// `true` when the loop must shut down.
fn handle_command(
    state: &mut EngineState,
    cmd: EngineCommand,
    now_ms: u64,
    overlay: &mut dyn OverlayBackend,
    probe: &mut dyn InputActivityProbe,
) -> bool {
    match cmd {
        EngineCommand::Shutdown => true,
        EngineCommand::SetSettings(settings) => {
            state.settings = settings.sanitized();
            probe.set_keyboard_suspends(state.settings.keyboard_suspends);
            false
        }
        EngineCommand::SetCalibration(calibration) => {
            state.calibration = calibration;
            false
        }
        EngineCommand::SetScreen(screen) => {
            state.screen = screen;
            overlay.set_screen(screen);
            false
        }
        EngineCommand::UndoSwitch => {
            // During the emergency lock the product leaves undo undefined;
            // EmergencyStop already cleared the stack, this guard just keeps
            // a racing hotkey from acting on a re-built stack inside the
            // lock.
            if emergency_locked(state.emergency_until, now_ms) {
                return false;
            }
            let undo_ms = state.settings.undo_ms as u64;
            match undo_take(&mut state.undo_stack, now_ms, undo_ms) {
                Some(Some(previous)) => {
                    let outcome = state.activator.activate(previous);
                    log::info!(
                        "halo: undo switch -> window={} activated={} fell_back={} detail={}",
                        previous,
                        outcome.activated,
                        outcome.fell_back,
                        outcome.detail
                    );
                }
                Some(None) => {
                    log::info!("halo: undo switch: previous foreground not enumerable, no-op");
                }
                None => {
                    log::info!("halo: undo switch: nothing eligible");
                }
            }
            false
        }
        EngineCommand::EmergencyStop => {
            state.emergency_until = Some(now_ms.saturating_add(EMERGENCY_LOCK_MS));
            // Product note: undo-after-emergency is undefined by spec; the
            // chosen semantics are "emergency clears the undo stack" — an
            // undo racing a deliberate stop is more surprise than help, and
            // 5 s of no-response must also not resurrect old switches.
            state.undo_stack.clear();
            log::info!(
                "halo: emergency stop, pipeline suspended for {} ms, undo stack cleared",
                EMERGENCY_LOCK_MS
            );
            false
        }
    }
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
    // Engine-own monotonic clock: face/input timers, the emergency/undo
    // windows, and snapshot timestamps all use this, so wall-clock (NTP)
    // jumps cannot distort the suspension windows.
    let tick_epoch = Instant::now();
    let mut next_tick = Instant::now();

    loop {
        // 0. Tick time first: commands and the step share one timestamp so
        //    undo/emergency windows and dwell timers stay consistent.
        let now_ms = tick_epoch.elapsed().as_millis() as u64;

        // 1. Commands (drain).
        let mut shutdown = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if handle_command(&mut state, cmd, now_ms, overlay.as_mut(), probe.as_mut()) {
                shutdown = true;
                break;
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
        // Border calls are deduplicated: only real state changes reach the
        // overlay (entering Candidate, frame change, leaving).
        if let Some(border) = border_command(&mut state.current_border, snapshot.border) {
            overlay.set_border(border);
        }

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

    // Clear the border unconditionally: shutdown can happen mid-candidate,
    // and the border window must not outlive the pipeline.
    overlay.set_border(None);
    overlay.hide();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::halo::activator::ActivationOutcome;
    use crate::halo::params::FocusHaloSettings;
    use crate::halo::windows_layer::WindowInfo;
    use std::sync::{Arc, Mutex};

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

    /// Activation recorder: ids land in the shared log, so tests assert on
    /// the calls made through the `Box<dyn WindowActivator>` inside state.
    struct FakeActivator {
        calls: Arc<Mutex<Vec<u64>>>,
        succeed: bool,
    }

    impl WindowActivator for FakeActivator {
        fn activate(&mut self, window_id: u64) -> ActivationOutcome {
            self.calls.lock().unwrap().push(window_id);
            ActivationOutcome {
                activated: self.succeed,
                fell_back: false,
                detail: "fake".into(),
            }
        }
    }

    fn fake_activator() -> (Box<dyn WindowActivator>, Arc<Mutex<Vec<u64>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(FakeActivator {
                calls: Arc::clone(&calls),
                succeed: true,
            }),
            calls,
        )
    }

    fn test_state() -> EngineState {
        let (activator, _) = fake_activator();
        EngineState::new(EngineConfig {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            screen: test_screen(),
            window_service: None,
            activator,
        })
    }

    /// A window service that always "hits" the same window under any point,
    /// so dwell integration can be exercised without a live window layer.
    struct FixedWindowService {
        window: Option<WindowInfo>,
    }

    impl WindowService for FixedWindowService {
        fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
            Ok(self.window.iter().cloned().collect())
        }

        fn hit_test(&mut self, _x: f64, _y: f64) -> Result<Option<WindowInfo>, String> {
            Ok(self.window.clone())
        }

        fn invalidate(&mut self) {}
    }

    fn test_state_with_window(window: Option<WindowInfo>) -> EngineState {
        let (activator, _) = fake_activator();
        EngineState::new(EngineConfig {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            screen: test_screen(),
            window_service: Some(Box::new(FixedWindowService { window })),
            activator,
        })
    }

    /// State over a service whose hit-test target differs from the
    /// enumeration list: a candidate window to dwell on plus a separate
    /// foreground window the undo entry should record.
    struct CandidateAndForegroundService {
        candidate: WindowInfo,
        foreground: Option<WindowInfo>,
    }

    impl WindowService for CandidateAndForegroundService {
        fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
            Ok(self.foreground.iter().cloned().collect())
        }

        fn hit_test(&mut self, _x: f64, _y: f64) -> Result<Option<WindowInfo>, String> {
            Ok(Some(self.candidate.clone()))
        }

        fn invalidate(&mut self) {}
    }

    fn test_state_candidate_and_foreground(
        candidate: WindowInfo,
        foreground: Option<WindowInfo>,
    ) -> (EngineState, Arc<Mutex<Vec<u64>>>) {
        let (activator, calls) = fake_activator();
        let state = EngineState::new(EngineConfig {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            screen: test_screen(),
            window_service: Some(Box::new(CandidateAndForegroundService {
                candidate,
                foreground,
            })),
            activator,
        });
        (state, calls)
    }

    /// Drive a command through `handle_command` with inert overlay/probe
    /// backends (the real ones need platform windows).
    fn drive(state: &mut EngineState, cmd: EngineCommand, now_ms: u64) {
        let mut probe = crate::halo::input_probe::NullProbe;
        let mut overlay = NoopOverlay;
        handle_command(state, cmd, now_ms, &mut overlay, &mut probe);
    }

    /// Minimal OverlayBackend for command tests.
    struct NoopOverlay;

    impl OverlayBackend for NoopOverlay {
        fn show(&mut self) {}
        fn hide(&mut self) {}
        fn set_screen(&mut self, _geometry: OverlayScreenGeometry) {}
        fn update(&mut self, _local: (f64, f64), _frame: &HaloFrame) {}
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
    fn face_loss_glides_to_center_and_settles() {
        let mut s = test_state();
        step(&mut s, Some(sample(0, true, 18.0, 0.0)), 0, false);
        // First tick after the loss: the target is screen center and the
        // render servo starts gliding there (no teleport).
        let (_, local, snap) = step(&mut s, Some(sample(33, false, 0.0, 0.0)), 33, false);
        assert!(
            local.0 > 500.0 && local.0 < 1000.0,
            "glides toward center, got {local:?}"
        );
        assert!(!snap.face_detected);
        // After ~2 s of ticks the servo settles at center, speed ~0.
        let mut last = (local, snap);
        for i in 2..=130u64 {
            let t = 33 + i * 16;
            last = {
                let (_, l, sn) = step(&mut s, None, t, false);
                (l, sn)
            };
        }
        assert!(
            (last.0 .0 - 500.0).abs() < 2.0,
            "settled at center, got {:?}",
            last.0
        );
        assert!(last.1.speed_norm < 0.01, "speed {}", last.1.speed_norm);
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

    fn dwell_window(id: u64) -> WindowInfo {
        WindowInfo {
            id,
            title: format!("win{id}"),
            process: "app".into(),
            frame: (0.0, 0.0, 1000.0, 500.0),
            is_foreground: false,
        }
    }

    #[test]
    fn dwell_reaches_visual_and_debug_snapshot() {
        let mut s = test_state_with_window(Some(dwell_window(9)));
        // First sample: speed stays 0 (no prior position), the halo is over
        // the fake window, so settling starts at t=0.
        let (_, _, snap) = step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        assert_eq!(snap.candidate, Some(9), "settling counts as the candidate");
        assert_eq!(snap.phase_str, "settling");

        // Settle (150 ms) completes by t=200: candidate phase, progress 0.
        let (visual, _, snap) = step(&mut s, None, 200, false);
        assert_eq!(visual, HaloVisual::Candidate { progress: 0.0 });
        assert_eq!(snap.candidate, Some(9));
        assert_eq!(snap.phase_str, "candidate");

        // Dwell progress feeds the visual arc.
        let (visual, _, snap) = step(&mut s, None, 600, false);
        let progress = match visual {
            HaloVisual::Candidate { progress } => progress,
            other => panic!("expected Candidate, got {other:?}"),
        };
        assert!((progress - 400.0 / 900.0).abs() < 1e-6);
        assert_eq!(snap.phase_str, "candidate");

        // Dwell completes at 200 + 900 = 1100: pulse on exactly this tick.
        let (visual, _, snap) = step(&mut s, None, 1100, false);
        assert_eq!(visual, HaloVisual::SwitchPulse);
        assert_eq!(snap.candidate, Some(9));
        assert_eq!(snap.phase_str, "switched");

        // Cooldown ticks render idle again.
        let (visual, _, snap) = step(&mut s, None, 1120, false);
        assert_eq!(visual, HaloVisual::Idle);
        assert_eq!(snap.candidate, None);
        assert_eq!(snap.phase_str, "cooldown");
    }

    #[test]
    fn border_for_phase_only_fires_on_candidate() {
        let frame = (10.0, 20.0, 300.0, 200.0);
        let want = |phase| border_for_phase(phase, Some(frame));
        assert_eq!(
            want(DwellPhase::Candidate {
                window_id: 1,
                progress: 0.5
            }),
            Some(BorderFrame {
                x: 10.0,
                y: 20.0,
                w: 300.0,
                h: 200.0
            })
        );
        // Settling has no phase of its own; leaving the candidate clears.
        assert_eq!(want(DwellPhase::Idle), None);
        assert_eq!(want(DwellPhase::Switched { window_id: 1 }), None);
        assert_eq!(want(DwellPhase::Cooldown), None);
        // Candidate without a usable frame (defensive) draws nothing.
        assert_eq!(
            border_for_phase(
                DwellPhase::Candidate {
                    window_id: 1,
                    progress: 0.0
                },
                None
            ),
            None
        );
    }

    #[test]
    fn border_command_dedups_the_set_border_sequence() {
        let frame = |x: f64| BorderFrame {
            x,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let mut current: Option<BorderFrame> = None;
        let mut calls: Vec<Option<BorderFrame>> = Vec::new();
        let mut push = |current: &mut Option<BorderFrame>, wanted| {
            if let Some(command) = border_command(current, wanted) {
                calls.push(command);
            }
        };

        // Enter Candidate: one call with the frame.
        push(&mut current, Some(frame(1.0)));
        // Identical frame next tick: no call.
        push(&mut current, Some(frame(1.0)));
        // Frame changed (window moved): one call with the new frame.
        push(&mut current, Some(frame(2.0)));
        // Leave Candidate: one clearing call; staying out: no more calls.
        push(&mut current, None);
        push(&mut current, None);

        assert_eq!(
            calls,
            vec![Some(frame(1.0)), Some(frame(2.0)), None],
            "only real state changes dispatch set_border"
        );
    }

    #[test]
    fn border_appears_with_candidate_and_clears_on_switch() {
        let mut s = test_state_with_window(Some(dwell_window(9)));
        // Settling: no border yet.
        let (_, _, snap) = step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        assert_eq!(snap.border, None);
        // Candidate: the border follows the hit window's frame.
        let (_, _, snap) = step(&mut s, None, 200, false);
        assert_eq!(
            snap.border,
            Some(BorderFrame {
                x: 0.0,
                y: 0.0,
                w: 1000.0,
                h: 500.0
            })
        );
        // Switch tick and cooldown clear it.
        let (_, _, snap) = step(&mut s, None, 1100, false);
        assert_eq!(snap.border, None, "switch clears the border");
        let (_, _, snap) = step(&mut s, None, 1120, false);
        assert_eq!(snap.border, None, "cooldown keeps it cleared");
    }

    #[test]
    fn no_window_service_bypasses_the_dwell_machine() {
        let mut s = test_state();
        // Same timing as the integration test above, but with no window
        // service the engine must stay pure-follow: no candidate visuals, no
        // dwell telemetry.
        for (now, expect) in [
            (0u64, HaloVisual::Idle),
            (200, HaloVisual::Idle),
            (1100, HaloVisual::Idle),
        ] {
            let sample = if now == 0 {
                Some(sample(now, true, 0.0, 0.0))
            } else {
                None
            };
            let (visual, _, snap) = step(&mut s, sample, now, false);
            assert_eq!(visual, expect, "tick {now}");
            assert_eq!(snap.candidate, None);
            assert_eq!(snap.phase_str, "idle");
        }
    }

    #[test]
    fn foreground_hits_and_suspension_hold_the_dwell_machine() {
        let mut foreground = dwell_window(5);
        foreground.is_foreground = true;
        let mut s = test_state_with_window(Some(foreground));
        step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        // Foreground never becomes a candidate, even after settle time.
        let (visual, _, snap) = step(&mut s, None, 200, false);
        assert_eq!(visual, HaloVisual::Idle);
        assert_eq!(snap.candidate, None);
        assert_eq!(snap.phase_str, "idle");

        // Non-foreground + physical input: the engine's suspension clears the
        // dwell machine, so the visual is Suspended and timing restarts.
        let mut s = test_state_with_window(Some(dwell_window(6)));
        step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        let (visual, _, _) = step(&mut s, None, 100, true);
        assert_eq!(visual, HaloVisual::Suspended);
        // After the 1 s quiet window, the machine is back to a fresh settle.
        let (visual, _, snap) = step(&mut s, None, 1100, false);
        assert_eq!(visual, HaloVisual::Idle, "settling restarted, not resumed");
        assert_eq!(snap.phase_str, "settling");
        let (visual, _, _) = step(&mut s, None, 1250, false);
        assert_eq!(visual, HaloVisual::Candidate { progress: 0.0 });
    }

    #[test]
    fn show_progress_off_keeps_the_ring_but_hides_the_arc() {
        let mut s = test_state_with_window(Some(dwell_window(7)));
        s.settings.show_progress = false;
        step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        let (visual, _, _) = step(&mut s, None, 200, false);
        assert_eq!(
            visual,
            HaloVisual::Candidate { progress: 0.0 },
            "candidate ring shows, arc stays empty"
        );
        // Mid-dwell the arc still reports 0 (progress suppressed).
        let (visual, _, _) = step(&mut s, None, 600, false);
        assert_eq!(visual, HaloVisual::Candidate { progress: 0.0 });
    }

    #[test]
    fn hit_test_errors_are_tolerated_as_no_hit() {
        struct ExplodingService;
        impl WindowService for ExplodingService {
            fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
                Err("backend exploded".into())
            }
            fn hit_test(&mut self, _x: f64, _y: f64) -> Result<Option<WindowInfo>, String> {
                Err("backend exploded".into())
            }
            fn invalidate(&mut self) {}
        }
        let (activator, _) = fake_activator();
        let mut s = EngineState::new(EngineConfig {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            screen: test_screen(),
            window_service: Some(Box::new(ExplodingService)),
            activator,
        });
        let (visual, _, snap) = step(&mut s, Some(sample(0, true, 0.0, 0.0)), 0, false);
        assert_eq!(visual, HaloVisual::Idle, "hit-test failure == blank area");
        assert_eq!(snap.candidate, None);
        assert_eq!(snap.phase_str, "idle");
    }

    // ----- P3: emergency lock, undo stack, switch consumption -----

    fn undo_entry(previous: Option<u64>, switched_at_ms: u64) -> UndoEntry {
        UndoEntry {
            previous_foreground_id: previous,
            switched_at_ms,
        }
    }

    #[test]
    fn emergency_locked_covers_only_the_open_window() {
        assert!(!emergency_locked(None, 0));
        assert!(!emergency_locked(None, 9_999));
        // Deadline 1000: locked strictly before it, unlocked from it on.
        assert!(emergency_locked(Some(1000), 0));
        assert!(emergency_locked(Some(1000), 999));
        assert!(!emergency_locked(Some(1000), 1000));
        assert!(!emergency_locked(Some(1000), 1001));
        // Clock glitch (now after the deadline) reads as unlocked.
        assert!(!emergency_locked(Some(500), 5000));
    }

    #[test]
    fn undo_push_caps_depth_dropping_the_oldest() {
        let mut stack = Vec::new();
        for i in 0..(UNDO_STACK_MAX + 2) as u64 {
            undo_push(&mut stack, undo_entry(Some(i), i * 10));
        }
        assert_eq!(stack.len(), UNDO_STACK_MAX);
        // Oldest entries (0, 1) fell off; the newest (last id) is on top.
        assert_eq!(stack.first().unwrap().previous_foreground_id, Some(2));
        assert_eq!(
            stack.last().unwrap().previous_foreground_id,
            Some((UNDO_STACK_MAX + 1) as u64)
        );
    }

    #[test]
    fn undo_take_discards_expired_tops_then_pops_the_fresh_top() {
        let undo_ms = 5000;
        // Everything expired: nothing undoable, stack drained.
        let mut stack = vec![undo_entry(Some(1), 0), undo_entry(Some(2), 100)];
        assert_eq!(undo_take(&mut stack, 7000, undo_ms), None);
        assert!(stack.is_empty());

        // Top fresh, entries below expired: the top pops, stale ones stay
        // (they are only discarded when they reach the top).
        let mut stack = vec![undo_entry(Some(1), 0), undo_entry(Some(2), 4000)];
        assert_eq!(undo_take(&mut stack, 6000, undo_ms), Some(Some(2)));
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].previous_foreground_id, Some(1));

        // Exactly at the undo_ms boundary still counts as fresh (<=).
        let mut stack = vec![undo_entry(Some(3), 1000)];
        assert_eq!(
            undo_take(&mut stack, 1000 + undo_ms, undo_ms),
            Some(Some(3))
        );

        // One ms past it: expired.
        let mut stack = vec![undo_entry(Some(3), 1000)];
        assert_eq!(undo_take(&mut stack, 1000 + undo_ms + 1, undo_ms), None);

        // Fresh top with a None previous id: popped and reported as a no-op
        // undo.
        let mut stack = vec![undo_entry(None, 100)];
        assert_eq!(undo_take(&mut stack, 200, undo_ms), Some(None));
        assert!(stack.is_empty());

        // Empty stack: None.
        let mut stack = Vec::new();
        assert_eq!(undo_take(&mut stack, 100, undo_ms), None);

        // Clock rollback (now < switched_at) saturates to age 0: fresh.
        let mut stack = vec![undo_entry(Some(9), 10_000)];
        assert_eq!(undo_take(&mut stack, 5_000, undo_ms), Some(Some(9)));
    }

    fn candidate_window(id: u64) -> WindowInfo {
        WindowInfo {
            id,
            title: format!("cand{id}"),
            process: "app".into(),
            frame: (0.0, 0.0, 1000.0, 500.0),
            is_foreground: false,
        }
    }

    fn foreground_window(id: u64) -> WindowInfo {
        let mut window = candidate_window(id);
        window.is_foreground = true;
        window
    }

    /// Drive the full dwell chain (settle at t=0, dwell to the switch at
    /// t=1100) over the candidate/foreground service and return the tick-1100
    /// snapshot.
    fn dwell_to_switch(state: &mut EngineState) -> DebugSnapshot {
        step(state, Some(sample(0, true, 0.0, 0.0)), 0, false);
        step(state, None, 200, false);
        let (_, _, snap) = step(state, None, 1100, false);
        snap
    }

    #[test]
    fn switch_fires_activation_and_records_the_previous_foreground() {
        let (mut s, calls) =
            test_state_candidate_and_foreground(candidate_window(9), Some(foreground_window(5)));
        let snap = dwell_to_switch(&mut s);
        assert_eq!(snap.phase_str, "switched");
        // The switch target was activated…
        assert_eq!(*calls.lock().unwrap(), vec![9]);
        // …and the undo entry recorded the pre-switch foreground.
        assert_eq!(s.undo_stack.len(), 1);
        assert_eq!(s.undo_stack[0].previous_foreground_id, Some(5));
        assert_eq!(s.undo_stack[0].switched_at_ms, 1100);
    }

    #[test]
    fn undo_command_within_window_activates_the_previous_foreground() {
        let (mut s, calls) =
            test_state_candidate_and_foreground(candidate_window(9), Some(foreground_window(5)));
        dwell_to_switch(&mut s);
        assert_eq!(*calls.lock().unwrap(), vec![9]);

        // Undo 1 s after the switch: well inside the default undo_ms (5000).
        drive(&mut s, EngineCommand::UndoSwitch, 2100);
        assert_eq!(*calls.lock().unwrap(), vec![9, 5]);
        assert!(s.undo_stack.is_empty(), "the entry was popped");
    }

    #[test]
    fn undo_command_after_window_discards_and_reports_nothing() {
        let (mut s, calls) =
            test_state_candidate_and_foreground(candidate_window(9), Some(foreground_window(5)));
        dwell_to_switch(&mut s);
        // Default undo_ms is 5000: at 7000 the entry is expired.
        drive(&mut s, EngineCommand::UndoSwitch, 1100 + 5900);
        assert_eq!(*calls.lock().unwrap(), vec![9], "no undo activation");
        assert!(s.undo_stack.is_empty(), "expired entry was discarded");
    }

    #[test]
    fn undo_without_enumerable_foreground_is_a_no_op_pop() {
        // Foreground not enumerable (None): the switch still activates and
        // pushes, but undo pops without calling the activator again.
        let (mut s, calls) = test_state_candidate_and_foreground(candidate_window(9), None);
        dwell_to_switch(&mut s);
        assert_eq!(s.undo_stack[0].previous_foreground_id, None);
        drive(&mut s, EngineCommand::UndoSwitch, 1500);
        assert_eq!(*calls.lock().unwrap(), vec![9], "undo stayed a no-op");
        assert!(s.undo_stack.is_empty());
    }

    #[test]
    fn emergency_stop_suspends_clears_undo_and_expires_after_5s() {
        let (mut s, calls) =
            test_state_candidate_and_foreground(candidate_window(9), Some(foreground_window(5)));
        dwell_to_switch(&mut s);
        assert_eq!(s.undo_stack.len(), 1);

        // Emergency at 1200: lock until 6200, stack cleared.
        drive(&mut s, EngineCommand::EmergencyStop, 1200);
        assert_eq!(s.emergency_until, Some(1200 + EMERGENCY_LOCK_MS));
        assert!(s.undo_stack.is_empty(), "emergency clears the undo stack");

        // Undo during the lock: no-op even if an entry existed.
        drive(&mut s, EngineCommand::UndoSwitch, 1300);
        assert_eq!(*calls.lock().unwrap(), vec![9]);

        // While the lock is up the dwell machine is force-suspended: the
        // halo over the same candidate renders Suspended and never switches.
        let (visual, _, snap) = step(&mut s, Some(sample(1400, true, 0.0, 0.0)), 1400, false);
        assert_eq!(visual, HaloVisual::Suspended);
        assert_eq!(snap.candidate, None, "no candidate under the lock");
        let (visual, _, _) = step(&mut s, Some(sample(6000, true, 0.0, 0.0)), 6000, false);
        assert_eq!(visual, HaloVisual::Suspended, "still locked at 6000 < 6200");

        // After the deadline the machine is usable again: a fresh settle
        // starts (idle renders, no carried-over dwell).
        let (visual, _, _) = step(&mut s, Some(sample(6300, true, 0.0, 0.0)), 6300, false);
        assert_eq!(visual, HaloVisual::Idle);
        // …and needs the full settle+dwell again (switch only at 6300+150+900).
        let (visual, _, snap) = step(&mut s, None, 6300 + 150, false);
        assert_eq!(visual, HaloVisual::Candidate { progress: 0.0 });
        assert_eq!(snap.phase_str, "candidate");
        let (_, _, snap) = step(&mut s, None, 6300 + 1050, false);
        assert_eq!(snap.phase_str, "switched");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![9, 9],
            "second switch activated"
        );
    }

    #[test]
    fn constants_and_config_carry_the_p3_fields() {
        assert_eq!(EMERGENCY_LOCK_MS, 5000);
        assert_eq!(UNDO_STACK_MAX, 8);
    }
}
