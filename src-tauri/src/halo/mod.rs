//! FocusHalo: settings, the native overlay shell (P0), and the core tracking
//! pipeline (P1): head-pose inference -> One Euro smoothing -> pose-to-screen
//! mapping -> engine thread driving the overlay. `HaloRuntime` is the
//! process-wide owner: `enable` starts camera + tracker + engine threads, and
//! `disable` tears them all down. The dwell/switch logic arrives in a later
//! phase and will keep driving this runtime; today it also exposes the P0
//! manual `show_test_halo` hook, which is a no-op while tracking runs so the
//! debug harness cannot fight the engine for the overlay window.

// Nothing outside this tree constructs HaloRuntime's enabled state yet, so
// dead_code fires on some items until the P2 dwell engine wires them up.
#![allow(dead_code)]

pub mod activator;
pub mod engine;
pub mod filter;
pub mod focus_router;
pub mod input_probe;
pub mod kalman;
pub mod mapping;
pub mod overlay;
pub mod params;
pub mod protocol;
pub mod screen_map;
pub mod smoothing;
pub mod state_machine;
pub mod tracker;
pub mod warp;
pub mod windows_layer;

pub use activator::{create_activator, ActivationOutcome, WindowActivator};
pub use engine::{
    spawn as spawn_engine, DebugSnapshot, EngineCommand, EngineConfig, EnginePhase, RemoteSwitch,
    ScreenEntry,
};
pub use filter::OneEuroFilter;
pub use focus_router::{
    apply_focus_event, install_receive_hook, BoardScreen, FocusDisplayOutcome, FocusEmission,
    FocusForwarder, FocusPhase, FocusRouter, FocusScreen, Observation,
};
pub use input_probe::{InputActivityProbe, NullProbe, SystemInputProbe};
pub use mapping::{map_pose, CalibrationProfile};
pub use overlay::{
    badge::{BadgeCorner, BadgeLayer, BadgeScreen},
    BorderFrame, HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry,
};
pub use params::FocusHaloSettings;
pub use protocol::{
    decode_halo_focus_packet, FocusSender, HaloFocusEvent, HaloFocusPacket, HALO_FOCUS_PROTOCOL,
};
pub use state_machine::{
    advance as advance_dwell, DwellConfig, DwellPhase, DwellState, SwitchEvent,
};
pub use tracker::{default_models_dir, Frame, HeadSample, PoseEngine};
pub use warp::{create_cursor_warper, CursorWarper, WarpRecorder};
pub use windows_layer::{create_window_service, WindowInfo, WindowService};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::halo::screen_map::ScreenCalibration;
use tracker::camera::CameraControl;

/// How long a remote-focus highlight stays on screen without a refresh
/// heartbeat from the sender. The sender refreshes identical state at 1 Hz
/// (protocol::UNCHANGED_REFRESH), so the timeout is 5x that: a highlight
/// survives lost refresh packets, and a sender that dies mid-dwell still
/// clears within seconds.
const REMOTE_HIGHLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// Remote-highlight overlay state, shared with the auto-clear watcher
/// threads so a highlight can expire without holding the halo Mutex. Only
/// ever populated while the tracking pipeline is disabled: `enable` releases
/// it first, because both platform backends must not have two live instances
/// fighting for the same overlay window.
#[derive(Default)]
struct RemoteHighlightUi {
    overlay: Option<Box<dyn OverlayBackend>>,
    /// halo_diameter the overlay was built with (rebuilt on change).
    built_diameter: f64,
    /// Border currently shown; `None` after a clear or watcher expiry, which
    /// is what makes an identical re-show draw again.
    border: Option<BorderFrame>,
}

/// Cap of the sample channel between the tracker and the engine. The engine
/// drains it every tick (60 Hz), so the cap only guards against a short
/// inference burst; a full channel drops the *incoming* sample, which is safe
/// because intermediate samples carry no extra information (the engine keeps
/// only the newest per tick anyway).
const SAMPLE_CHANNEL_CAPACITY: usize = 4;
/// When no face was detected at the last inference, skip frames arriving
/// within this window: a face-less scene is inferred at >= 2 Hz instead of the
/// full camera rate, freeing the CPU while the user is away.
const NO_FACE_INFER_INTERVAL_MS: u64 = 500;
/// Frame-buffer depth between the camera callback and the tracker thread. The
/// tracker consumes continuously; a full buffer drops the frame (the camera
/// layer itself also discards late frames), never blocking the capture queue.
const FRAME_CHANNEL_CAPACITY: usize = 2;
/// Tracker thread poll cadence while no frames arrive, so the stop flag is
/// noticed promptly even when the camera went quiet.
const TRACKER_POLL_MS: u64 = 100;
/// Upper bound for joining a worker thread in [`HaloRuntime::disable`]; past
/// it the thread is detached (it will exit on its own Shutdown/stop signal).
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared gate state between the camera callback (writer: which frames may
/// pass) and the tracker thread (writer: last inference outcome).
#[derive(Debug, Default)]
struct FrameGate {
    /// `frame.ts_ms` of the last inference the tracker actually ran.
    last_infer_ms: Option<u64>,
    /// Whether that inference found a face.
    last_face: bool,
}

/// Pure decision for the camera callback: should this frame be inferred?
/// Frames are skipped only while no face has been seen recently; once a face
/// is present every frame goes through (the tracker keeps up or its own
/// channel backpressure drops frames).
fn frame_gate_passes(gate: &FrameGate, frame_ts_ms: u64) -> bool {
    match gate.last_infer_ms {
        Some(last) if !gate.last_face => {
            frame_ts_ms.saturating_sub(last) >= NO_FACE_INFER_INTERVAL_MS
        }
        _ => true,
    }
}

/// Runtime lifecycle. `Disabled` keeps the process at zero halo threads and
/// zero halo windows; `Running` owns the tracker and engine threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeState {
    Disabled,
    Running,
}

/// Owns the whole halo pipeline for the process: settings snapshot, the
/// overlay window (built lazily, and only held while disabled or by the
/// engine while running), and — while enabled — the camera, the tracker
/// thread, and the engine thread. Held behind a `Mutex` by the app runtime;
/// `enable`/`disable` may block for seconds (camera start, bounded joins), so
/// command layers must not hold other locks while calling them.
pub struct HaloRuntime {
    settings: FocusHaloSettings,
    calibration: CalibrationProfile,
    state: RuntimeState,
    /// Lazily-built overlay; `None` while running (the engine owns it) or
    /// before the first use.
    overlay: Option<Box<dyn OverlayBackend>>,
    /// halo_diameter the current overlay was built with; changing it rebuilds
    /// the window so its pixel size follows the setting.
    built_diameter: f64,
    geometry: Option<OverlayScreenGeometry>,
    /// S3: every local screen in board space, handed to the engine so a
    /// region commit can switch screens and warp the pointer. Kept on the
    /// runtime so the next enable picks up the current topology without the
    /// caller having to re-push it.
    screens: Vec<ScreenEntry>,
    /// Live while running: signals the tracker thread to stop (its loop owns
    /// the camera object, so all camera calls stay on one thread).
    tracker_stop: Option<Arc<AtomicBool>>,
    tracker_handle: Option<JoinHandle<()>>,
    engine_handle: Option<JoinHandle<()>>,
    engine_cmd_tx: Option<std::sync::mpsc::Sender<EngineCommand>>,
    engine_debug_rx: Option<Receiver<DebugSnapshot>>,
    /// Latest raw (yaw, pitch, u, v) from the tracker, for the calibration
    /// capture flows. `u`/`v` are the normalized face-center position (see
    /// `tracker::HeadSample`) and are consumed by the multi-screen region
    /// calibration's seat compensation. `None` whenever the last inference
    /// found no face.
    latest_raw_pose: Arc<Mutex<Option<(f64, f64, f64, f64)>>>,
    /// Camera health as of the last successful/failed enable, surfaced by
    /// `halo_get_status`.
    camera_ok: bool,
    /// Last enable failure reason, surfaced by `halo_get_status`.
    last_error: Option<String>,
    /// P4 passive peer display: the border highlight shown when a remote
    /// halo-sender points at this machine. Shared with watcher threads (see
    /// [`RemoteHighlightUi`]).
    remote_ui: Arc<Mutex<RemoteHighlightUi>>,
    /// Generation counter of remote highlights; watcher threads compare it
    /// before clearing so a newer highlight survives an older timer.
    remote_seq: Arc<AtomicU64>,
}

impl HaloRuntime {
    /// Empty runtime: no window, no thread, nothing platform-specific.
    pub fn new() -> Self {
        HaloRuntime {
            settings: FocusHaloSettings::default(),
            calibration: CalibrationProfile::default(),
            state: RuntimeState::Disabled,
            overlay: None,
            built_diameter: 0.0,
            geometry: None,
            screens: Vec::new(),
            tracker_stop: None,
            tracker_handle: None,
            engine_handle: None,
            engine_cmd_tx: None,
            engine_debug_rx: None,
            latest_raw_pose: Arc::new(Mutex::new(None)),
            camera_ok: false,
            last_error: None,
            remote_ui: Arc::new(Mutex::new(RemoteHighlightUi::default())),
            remote_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state == RuntimeState::Running
    }

    /// Whether the last enable reached a running camera. False while disabled
    /// and after an enable that failed at the camera step.
    pub(crate) fn camera_ok(&self) -> bool {
        self.is_running() && self.camera_ok
    }

    /// Last failure reason (missing/corrupt model, camera error, ...), for UI
    /// display via `halo_get_status`.
    pub(crate) fn status_reason(&self) -> Option<String> {
        self.last_error.clone()
    }

    /// Store a sanitized settings snapshot. Deliberately does not touch the
    /// platform: the overlay (and on Windows its thread) is only created by
    /// the first show, so a disabled halo keeps the process at zero cost.
    /// While running, use [`HaloRuntime::set_settings`] to reach the engine.
    pub fn configure(&mut self, settings: FocusHaloSettings) {
        self.settings = settings.sanitized();
    }

    /// Start the full tracking pipeline. On success the process owns a camera
    /// session, a tracker thread, and an engine thread; on failure nothing is
    /// left behind (all partial state is torn down) and the error message is
    /// meant for the UI.
    ///
    /// Already running: treated as "apply the new parameters", forwarded to
    /// the engine as commands — no restart, no camera flash. This keeps the
    /// `save_layout` enabled-changed hook idempotent when the UI enabled the
    /// halo first and then persisted the flag.
    pub(crate) fn enable(
        &mut self,
        settings: FocusHaloSettings,
        calibration: CalibrationProfile,
        screen: OverlayScreenGeometry,
    ) -> Result<(), String> {
        let settings = settings.sanitized();
        // A remote-focus highlight window (if any) must be gone before the
        // engine builds its own overlay: the platform backends share one
        // overlay window per process, and the engine needs exclusive
        // ownership of it.
        self.release_remote_overlay();
        if self.is_running() {
            // Idempotent re-enable: forward instead of restarting the camera.
            self.settings = settings;
            self.calibration = calibration;
            self.geometry = Some(screen);
            self.forward(EngineCommand::SetScreens(self.screens.clone()));
            self.forward(EngineCommand::SetSettings(self.settings.clone()));
            self.forward(EngineCommand::SetCalibration(calibration));
            self.forward(EngineCommand::SetScreen(screen));
            return Ok(());
        }

        self.last_error = None;
        match self.enable_inner(settings, calibration, screen) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Leave nothing half-started behind.
                self.teardown();
                self.last_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn enable_inner(
        &mut self,
        settings: FocusHaloSettings,
        calibration: CalibrationProfile,
        screen: OverlayScreenGeometry,
    ) -> Result<(), String> {
        // 1. Models first: a missing/corrupt ONNX file must fail before any
        //    thread or camera session exists.
        let pose_engine = Arc::new(PoseEngine::new(&default_models_dir())?);

        // 2. Camera session, started synchronously so a camera failure also
        //    surfaces from this call (the implementation marshals start/stop
        //    to its own worker and blocks until the session is up).
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(FRAME_CHANNEL_CAPACITY);
        let gate = Arc::new(Mutex::new(FrameGate::default()));
        let callback_gate = Arc::clone(&gate);
        let mut camera = tracker::camera::create_camera();
        camera.start(Box::new(move |frame: Frame| {
            // No-face downsample gate: skip frames while the scene has stayed
            // face-less for less than NO_FACE_INFER_INTERVAL_MS.
            let passes = callback_gate
                .lock()
                .map(|gate| frame_gate_passes(&gate, frame.ts_ms))
                .unwrap_or(true);
            if !passes {
                return;
            }
            // try_send: never block the capture queue; a full buffer drops
            // this frame and the tracker simply works on the next one.
            let _ = frame_tx.try_send(frame);
        }))?;

        // 3. Sample channel: the tracker holds the sender, the engine spawns
        //    with the receiver.
        let (sample_tx, sample_rx) =
            std::sync::mpsc::sync_channel::<HeadSample>(SAMPLE_CHANNEL_CAPACITY);

        // 4. Overlay: hand the lazily-built (or fresh) window to the engine.
        //    While running the runtime holds no overlay; the engine's exit
        //    path hides it, and dropping it closes the window again — a
        //    disabled halo therefore keeps zero threads AND zero windows.
        let overlay = self
            .overlay
            .take()
            .unwrap_or_else(|| overlay::create_overlay(&settings));
        self.built_diameter = 0.0;

        // 5. Engine. Its handle is registered on self immediately: if the
        //    tracker spawn below fails, the enable() error path runs
        //    teardown(), which needs the command sender to stop this engine —
        //    otherwise the thread (and its overlay window) would leak.
        let config = EngineConfig {
            settings: settings.clone(),
            calibration,
            // P2 dwell: a single-screen window list over the tracked screen's
            // board area is enough for now; multi-screen board layout stays a
            // later-phase refinement. The service caches enumeration for
            // 300 ms, so the engine can hit-test every tick cheaply. (This
            // field borrows `screen`, so it is initialized before `screen`
            // moves into the config.)
            window_service: Some(create_window_service(std::slice::from_ref(&screen))),
            // P3 activation: the engine raises dwell targets and undoes
            // switches through this backend.
            activator: activator::create_activator(),
            screen,
            // S3 region switching: the caller pushes the live screen list via
            // `set_screens` (kept on the runtime), the calibration travels
            // inside the settings snapshot, and the warper is the platform
            // cursor warp backend.
            screens: self.screens.clone(),
            screen_calibration: settings.screen_calibration.clone(),
            warper: warp::create_cursor_warper(),
        };
        let (engine_handle, engine_cmd_tx, engine_debug_rx) = engine::spawn(
            config,
            sample_rx,
            overlay,
            input_probe::create_input_probe(settings.keyboard_suspends),
        );
        self.engine_handle = Some(engine_handle);
        self.engine_cmd_tx = Some(engine_cmd_tx);
        self.engine_debug_rx = Some(engine_debug_rx);

        // 6. Tracker thread: infers on frames from the camera, publishes the
        //    latest raw pose for calibration, forwards samples to the engine.
        let stop = Arc::new(AtomicBool::new(false));
        let latest_raw_pose = Arc::clone(&self.latest_raw_pose);
        let tracker_stop = Arc::clone(&stop);
        let tracker_handle = std::thread::Builder::new()
            .name("focus-halo-tracker".into())
            .spawn(move || {
                run_tracker(
                    pose_engine,
                    camera,
                    frame_rx,
                    sample_tx,
                    gate,
                    latest_raw_pose,
                    tracker_stop,
                );
            })
            .map_err(|error| format!("failed to spawn focus-halo tracker thread: {error}"))?;

        self.settings = settings;
        self.calibration = calibration;
        self.geometry = Some(screen);
        self.camera_ok = true;
        self.tracker_stop = Some(stop);
        self.tracker_handle = Some(tracker_handle);
        self.state = RuntimeState::Running;
        Ok(())
    }

    /// Stop everything: camera, tracker thread, engine thread. The engine's
    /// exit path hides the overlay before dropping it, so the window closes
    /// with the pipeline (a disabled halo keeps zero threads and windows).
    /// Bounded: each join waits at most [`SHUTDOWN_JOIN_TIMEOUT`] before the
    /// thread is detached (it exits on its own stop signal regardless).
    pub(crate) fn disable(&mut self) {
        self.teardown();
        self.last_error = None;
    }

    /// Shared teardown for `disable` and failed `enable` cleanups.
    fn teardown(&mut self) {
        // Signal the tracker: its loop stops the camera and drops the sample
        // sender, which the engine also treats as end-of-stream.
        if let Some(stop) = self.tracker_stop.as_ref() {
            stop.store(true, Ordering::Relaxed);
        }
        // Engine shutdown command goes out first so it does not wait for the
        // sample channel to drain.
        if let Some(cmd_tx) = self.engine_cmd_tx.take() {
            let _ = cmd_tx.send(EngineCommand::Shutdown);
        }
        if let Some(handle) = self.tracker_handle.take() {
            join_bounded(handle, SHUTDOWN_JOIN_TIMEOUT);
        }
        if let Some(handle) = self.engine_handle.take() {
            join_bounded(handle, SHUTDOWN_JOIN_TIMEOUT);
        }
        self.tracker_stop = None;
        self.engine_debug_rx = None;
        self.overlay = None;
        self.built_diameter = 0.0;
        // Return to zero windows: the remote-highlight overlay (if any was
        // shown while running, which show_remote_highlight refuses) goes too.
        self.release_remote_overlay();
        if let Ok(mut latest) = self.latest_raw_pose.lock() {
            *latest = None;
        }
        self.camera_ok = false;
        self.state = RuntimeState::Disabled;
    }

    /// Forward a command to the running engine (no-op when disabled).
    fn forward(&self, command: EngineCommand) {
        if let Some(cmd_tx) = self.engine_cmd_tx.as_ref() {
            let _ = cmd_tx.send(command);
        }
    }

    /// Replace the settings snapshot; while running the engine is notified so
    /// dead zone / gain / camera position / keyboard suspends apply next tick.
    pub(crate) fn set_settings(&mut self, settings: FocusHaloSettings) {
        let settings = settings.sanitized();
        if self.is_running() {
            self.forward(EngineCommand::SetSettings(settings.clone()));
        }
        self.settings = settings;
    }

    /// Replace the calibration profile; forwarded to a running engine.
    pub(crate) fn set_calibration(&mut self, calibration: CalibrationProfile) {
        if self.is_running() {
            self.forward(EngineCommand::SetCalibration(calibration));
        }
        self.calibration = calibration;
    }

    /// Undo the most recent dwell switch (P3). Forwarded to a running engine
    /// as [`EngineCommand::UndoSwitch`]; a disabled runtime no-ops (and the
    /// engine itself rejects undos during the emergency lock). The P3-B undo
    /// hotkey calls this.
    pub(crate) fn undo_switch(&mut self) {
        if self.is_running() {
            self.forward(EngineCommand::UndoSwitch);
        }
    }

    /// Emergency stop (P3): suspend the pipeline for 5 s and clear the undo
    /// stack via [`EngineCommand::EmergencyStop`]; a disabled runtime no-ops.
    /// The P3-B emergency hotkey calls this.
    pub(crate) fn emergency_stop(&mut self) {
        if self.is_running() {
            self.forward(EngineCommand::EmergencyStop);
        }
    }

    /// Store the screen geometry; forwarded to a running engine and reused by
    /// the next enable / P0 test overlay.
    pub fn set_screen(&mut self, geometry: OverlayScreenGeometry) {
        self.geometry = Some(geometry);
        if self.is_running() {
            self.forward(EngineCommand::SetScreen(geometry));
            return;
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_screen(geometry);
        }
    }

    /// S3: replace the whole local-screen list (display topology changed).
    /// Stored for the next enable and forwarded to a running engine as
    /// [`EngineCommand::SetScreens`]; the engine's region path uses the list
    /// to re-host the overlay and to convert warp targets to native coords.
    pub fn set_screens(&mut self, screens: Vec<ScreenEntry>) {
        self.screens = screens;
        if self.is_running() {
            self.forward(EngineCommand::SetScreens(self.screens.clone()));
        }
    }

    /// S3: replace the multi-screen calibration; forwarded to a running
    /// engine (used by the recenter flow, which shifts every region's quad).
    pub(crate) fn set_screen_calibration(&mut self, calibration: Option<ScreenCalibration>) {
        if self.is_running() {
            self.forward(EngineCommand::SetScreenCalibration(calibration));
        }
    }

    /// Latest raw head pose plus face-center position for the calibration
    /// capture flows: `None` while disabled or whenever the last inference
    /// found no face.
    pub(crate) fn latest_pose(&self) -> Option<(f64, f64, f64, f64)> {
        self.latest_raw_pose.lock().ok().and_then(|latest| *latest)
    }

    /// Drain pending debug snapshots (bounded channel, dropped on lag by the
    /// engine side). Empty when disabled or once the engine has exited.
    pub(crate) fn drain_debug(&mut self) -> Vec<DebugSnapshot> {
        match self.engine_debug_rx.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        }
    }

    /// Create the backend on first use, or when the configured diameter
    /// changed (the window's pixel size must follow the setting). No-op while
    /// running: the engine owns the overlay window then.
    fn ensure_overlay(&mut self) {
        if !overlay::overlay_supported() || self.is_running() {
            return;
        }
        let diameter = self.settings.halo_diameter;
        let needs_build =
            self.overlay.is_none() || (self.built_diameter - diameter).abs() > f64::EPSILON;
        if !needs_build {
            return;
        }
        self.overlay = Some(overlay::create_overlay(&self.settings));
        self.built_diameter = diameter;
        // Carry the already-known screen over so the new window lands
        // correctly on the first update.
        if let Some(geometry) = self.geometry {
            if let Some(overlay) = self.overlay.as_mut() {
                overlay.set_screen(geometry);
            }
        }
    }

    /// P0 test hook: draw one halo frame at a screen-local logical point.
    /// No-op while the tracking pipeline is running — the engine owns the
    /// overlay then, and a manual frame would fight it at 60 Hz.
    pub fn show_test_halo(&mut self, local: (f64, f64), visual: HaloVisual) {
        if self.is_running() {
            return;
        }
        self.ensure_overlay();
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        overlay.show();
        overlay.update(
            local,
            &HaloFrame {
                visual,
                opacity: self.settings.halo_opacity,
            },
        );
    }

    /// P0 test hook: show or clear the candidate-window border (the breathing
    /// animation lives inside the backend). `border` coordinates are board
    /// logical, matching `WindowInfo::frame`. No-op while running.
    pub fn show_test_border(&mut self, border: Option<BorderFrame>) {
        if self.is_running() {
            return;
        }
        self.ensure_overlay();
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_border(border);
        }
    }

    /// Hide the overlay window. While running the engine owns the window (its
    /// own lifecycle controls visibility), so this is a deliberate no-op.
    pub fn hide(&mut self) {
        if self.is_running() {
            return;
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.hide();
        }
    }

    /// P4 passive peer display: show (or refresh) the border highlight a
    /// remote halo-sender pointed at. `frame` is in layout-board logical
    /// coordinates and `geometry` is the screen the highlight window should
    /// live on (the screen the sender aimed at, resolved by the caller).
    ///
    /// Works while the tracking pipeline is disabled — the normal state of a
    /// machine being controlled. While running it returns false (the engine
    /// owns the overlay visuals and a remote border would fight its 60 Hz
    /// updates). Returns true when the border was drawn/changed; false means
    /// "unchanged", letting callers skip redundant logging.
    ///
    /// A watcher thread auto-clears the border after
    /// [`REMOTE_HIGHLIGHT_TIMEOUT`] unless a newer highlight arrived first
    /// (generation check), so a sender that dies mid-dwell cannot leave a
    /// stale highlight behind. An identical refresh (the sender's 1 Hz
    /// heartbeat) does not redraw, but it does re-arm the watcher: a
    /// continuous dwell keeps the highlight alive instead of flickering off
    /// every timeout.
    pub fn show_remote_highlight(
        &mut self,
        frame: BorderFrame,
        geometry: OverlayScreenGeometry,
    ) -> bool {
        if self.is_running() || !overlay::overlay_supported() {
            return false;
        }
        let unchanged;
        let generation;
        {
            let Ok(mut ui) = self.remote_ui.lock() else {
                return false;
            };
            unchanged = ui.border == Some(frame);
            if !unchanged {
                let diameter = self.settings.halo_diameter;
                let needs_build =
                    ui.overlay.is_none() || (ui.built_diameter - diameter).abs() > f64::EPSILON;
                if needs_build {
                    ui.overlay = Some(overlay::create_overlay(&self.settings));
                    ui.built_diameter = diameter;
                }
                if let Some(overlay) = ui.overlay.as_mut() {
                    overlay.set_screen(geometry);
                    overlay.set_border(Some(frame));
                }
                ui.border = Some(frame);
            }
            // Bump the generation on every accepted refresh so the expiring
            // watcher is retired, and a fresh lease starts below.
            // `fetch_add` returns the previous value; +1 is this lease's
            // generation, read atomically with the bump.
            generation = self.remote_seq.fetch_add(1, Ordering::SeqCst) + 1;
        }
        let ui = Arc::clone(&self.remote_ui);
        let seq = Arc::clone(&self.remote_seq);
        let spawn_result = std::thread::Builder::new()
            .name("halo-remote-clear".into())
            .spawn(move || {
                std::thread::sleep(REMOTE_HIGHLIGHT_TIMEOUT);
                if seq.load(Ordering::SeqCst) != generation {
                    return;
                }
                if let Ok(mut ui) = ui.lock() {
                    // Recheck under the lock: a highlight that raced in just
                    // before we took it must survive this watcher.
                    if seq.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    if let Some(overlay) = ui.overlay.as_mut() {
                        overlay.set_border(None);
                    }
                    ui.border = None;
                    log::info!(
                        "halo focus: remote highlight expired after {REMOTE_HIGHLIGHT_TIMEOUT:?}"
                    );
                }
            });
        if spawn_result.is_err() {
            log::warn!("halo focus: failed to spawn remote-highlight watcher; highlight will not auto-clear");
        }
        !unchanged
    }

    /// P4 passive peer display: drop the remote border highlight immediately
    /// (a Clear arrived, or the caller is tearing the display down).
    /// Pending watcher threads become no-ops via the generation bump.
    pub fn clear_remote_highlight(&mut self) {
        // Invalidate any pending watcher first so it cannot resurrect the
        // border after we cleared it.
        self.remote_seq.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut ui) = self.remote_ui.lock() {
            if ui.border.take().is_some() {
                if let Some(overlay) = ui.overlay.as_mut() {
                    overlay.set_border(None);
                }
            }
        }
    }

    /// Drop the remote-highlight overlay window entirely (enable/teardown
    /// path): dropping the backend closes the window, and the freed platform
    /// singleton lets the engine build its own overlay.
    fn release_remote_overlay(&mut self) {
        self.clear_remote_highlight();
        if let Ok(mut ui) = self.remote_ui.lock() {
            ui.overlay = None;
            ui.built_diameter = 0.0;
        }
    }
}

impl Default for HaloRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Join a worker thread with a deadline; past the deadline the handle is
/// dropped (detaching the thread — it exits on its own stop signal anyway).
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
}

/// Tracker loop: inference worker between the camera frames and the engine
/// sample channel. Owns the camera (start happened in `enable`; stop happens
/// here when the stop flag fires) so all camera calls stay on one thread.
fn run_tracker(
    pose_engine: Arc<PoseEngine>,
    mut camera: Box<dyn CameraControl>,
    frame_rx: Receiver<Frame>,
    sample_tx: SyncSender<HeadSample>,
    gate: Arc<Mutex<FrameGate>>,
    latest_raw_pose: Arc<Mutex<Option<(f64, f64, f64, f64)>>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            camera.stop();
            break;
        }
        let frame = match frame_rx.recv_timeout(Duration::from_millis(TRACKER_POLL_MS)) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Camera gone (failed teardown): nothing more to infer.
                camera.stop();
                break;
            }
        };
        let sample = pose_engine.infer(&frame);
        if let Ok(mut latest) = latest_raw_pose.lock() {
            *latest = if sample.face_detected {
                Some((sample.yaw_deg, sample.pitch_deg, sample.u, sample.v))
            } else {
                None
            };
        }
        if let Ok(mut gate) = gate.lock() {
            gate.last_infer_ms = Some(frame.ts_ms);
            gate.last_face = sample.face_detected;
        }
        // Cap 4: on a full channel the sample is dropped. The engine drains
        // once per tick (~every TICK_PERIOD), keeps only the newest sample,
        // and telemetry never back-pressures tracking.
        if let Err(TrySendError::Disconnected(_)) = sample_tx.try_send(sample) {
            // Engine gone: stop tracking and release the camera.
            camera.stop();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_runtime_starts_empty_and_calls_are_safe() {
        let mut runtime = HaloRuntime::new();
        // No overlay yet: every path must be a no-op without panicking.
        runtime.hide();
        runtime.show_test_halo((10.0, 10.0), HaloVisual::Idle);
        assert!(runtime.geometry.is_none());
        assert!(!runtime.is_running());
        assert!(!runtime.camera_ok());
        assert_eq!(runtime.status_reason(), None);
        assert_eq!(runtime.latest_pose(), None);
        assert!(runtime.drain_debug().is_empty());
        // P3 command entries are no-ops while disabled (must not panic).
        runtime.undo_switch();
        runtime.emergency_stop();
        // Disabling an already-disabled runtime is a no-op.
        runtime.disable();
    }

    #[test]
    fn configure_sanitizes_the_snapshot() {
        let mut runtime = HaloRuntime::new();
        runtime.configure(FocusHaloSettings {
            halo_diameter: 999.0,
            halo_opacity: 5.0,
            ..FocusHaloSettings::default()
        });
        assert_eq!(runtime.settings.halo_diameter, 180.0);
        assert_eq!(runtime.settings.halo_opacity, 0.9);
    }

    #[test]
    fn disabled_runtime_stores_settings_calibration_and_screen() {
        let mut runtime = HaloRuntime::new();
        let screen = OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
            scale: 1.0,
            native_x: 0.0,
            native_y: 0.0,
        };
        let calibration = CalibrationProfile {
            center_yaw: -1.0,
            ..CalibrationProfile::default()
        };
        runtime.set_settings(FocusHaloSettings {
            dead_zone_percent: 4.0,
            ..FocusHaloSettings::default()
        });
        runtime.set_calibration(calibration);
        runtime.set_screen(screen);
        assert_eq!(runtime.settings.dead_zone_percent, 4.0);
        assert_eq!(runtime.calibration, calibration);
        // OverlayScreenGeometry has no PartialEq; asserting the width pins the
        // snapshot without adding a trait just for this test.
        assert_eq!(runtime.geometry.map(|g| g.width), Some(screen.width));
        assert!(
            !runtime.is_running(),
            "storing snapshots must not start anything"
        );
    }

    #[test]
    fn unsupported_platform_keeps_runtime_inert() {
        if overlay::overlay_supported() {
            return;
        }
        let mut runtime = HaloRuntime::new();
        runtime.configure(FocusHaloSettings::default());
        runtime.show_test_halo((0.0, 0.0), HaloVisual::SwitchPulse);
        runtime.hide();
        // Still inert: the factory returned a NoopBackend, nothing to assert
        // beyond "did not panic".
    }

    #[test]
    fn remote_highlight_draws_dedupes_and_clears() {
        let mut runtime = HaloRuntime::new();
        let geometry = OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
            scale: 1.0,
            native_x: 0.0,
            native_y: 0.0,
        };
        let frame = BorderFrame {
            x: 40.0,
            y: 30.0,
            w: 300.0,
            h: 200.0,
        };
        if !overlay::overlay_supported() {
            // Unsupported platforms: an honest no-op that reports false.
            assert!(!runtime.show_remote_highlight(frame, geometry));
            runtime.clear_remote_highlight();
            runtime.disable();
            return;
        }
        assert!(
            runtime.show_remote_highlight(frame, geometry),
            "first highlight draws"
        );
        assert!(
            !runtime.show_remote_highlight(frame, geometry),
            "identical refresh dedupes (sender heartbeat)"
        );
        let moved = BorderFrame {
            x: 40.0,
            y: 30.0,
            w: 320.0,
            h: 200.0,
        };
        assert!(
            runtime.show_remote_highlight(moved, geometry),
            "a changed frame redraws"
        );
        runtime.clear_remote_highlight();
        assert!(
            runtime.show_remote_highlight(frame, geometry),
            "after a clear the same frame draws again (watcher had reset the state)"
        );
        // disable() releases the remote overlay; showing again rebuilds it.
        runtime.disable();
        assert!(
            runtime.show_remote_highlight(frame, geometry),
            "highlight works after a disable/enable cycle"
        );
        runtime.clear_remote_highlight();
    }

    #[test]
    fn frame_gate_skips_only_recent_no_face_frames() {
        // Nothing inferred yet: first frame always passes.
        assert!(frame_gate_passes(&FrameGate::default(), 100));
        // Face present: every frame passes.
        let gate = FrameGate {
            last_infer_ms: Some(100),
            last_face: true,
        };
        assert!(frame_gate_passes(&gate, 120));
        assert!(frame_gate_passes(&gate, 10_000));
        // No face: frames within the window are skipped…
        let gate = FrameGate {
            last_infer_ms: Some(100),
            last_face: false,
        };
        assert!(!frame_gate_passes(&gate, 300));
        assert!(!frame_gate_passes(
            &gate,
            100 + NO_FACE_INFER_INTERVAL_MS - 1
        ));
        // …and the first frame after the window passes.
        assert!(frame_gate_passes(&gate, 100 + NO_FACE_INFER_INTERVAL_MS));
        // A clock glitch producing a backwards timestamp yields a saturating
        // diff of 0, i.e. inside the window: the frame is skipped and the
        // next (monotonic) frame passes.
        assert!(!frame_gate_passes(&gate, 50));
    }

    #[test]
    fn constants_match_design() {
        assert_eq!(SAMPLE_CHANNEL_CAPACITY, 4);
        assert_eq!(NO_FACE_INFER_INTERVAL_MS, 500);
        assert_eq!(TRACKER_POLL_MS, 100);
        assert_eq!(SHUTDOWN_JOIN_TIMEOUT, Duration::from_secs(2));
    }
}
