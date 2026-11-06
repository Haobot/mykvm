//! P4 cross-machine focus routing.
//!
//! Send side ([`FocusRouter`] + [`FocusForwarder`]): consumes the halo
//! engine's per-tick snapshot (normalized halo position, dwell phase) plus a
//! snapshot of the layout board (this machine's tracked screen + the online
//! peers' screens) and decides whether a [`HaloFocusEvent`](super::protocol::HaloFocusEvent)
//! must go out: the halo entering the edge band toward a screen that belongs
//! to a peer yields Edge/Candidate/Activate aimed at that peer; leaving the
//! band (or losing the neighbor) yields Clear once.
//!
//! Receive side ([`try_dispatch_received_focus`] + [`apply_focus_event`]):
//! decodes and authorizes an inbound datagram from the input chain, then
//! drives the passive peer display (border highlight via the halo overlay,
//! optional window activation). The hook indirection exists because the
//! input datagram chain has no `AppHandle`; the app registers one closure at
//! startup that performs the actual UI work.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::activator::{ActivationOutcome, WindowActivator};
use super::overlay::BorderFrame;
use super::protocol::{decode_halo_focus_packet, FocusSender, HaloFocusEvent, HaloFocusPacket};
use super::warp::CursorWarper;
use super::windows_layer::{safe_scale, WindowService};
use super::{HaloRuntime, OverlayScreenGeometry};
use crate::LayoutState;

/// Edge bandwidth as a fraction of the tracked screen: the halo is "at the
/// edge" once norm_x/norm_y is within this distance of a border.
pub const EDGE_BAND: f64 = 0.08;
/// Fraction of the target screen's dimension the landing point is
/// extrapolated into the peer's screen from the shared edge.
pub const ENTRY_INSET: f64 = 0.05;
/// Slack (board logical pixels) when deciding whether a peer screen is
/// flush-adjacent to the local screen; layout boards store integer-ish
/// origins but announced geometry may wobble by a pixel.
const ADJACENCY_SLACK: f64 = 2.0;

// ---------------------------------------------------------------------------
// Send side: geometry + event selection (pure, unit-tested below)
// ---------------------------------------------------------------------------

/// One screen of the layout board relevant to focus routing.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardScreen {
    pub id: String,
    /// Owning device id; peers only (the local device's screens never
    /// become focus targets).
    pub device_id: String,
    /// Layout-board logical rect.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl BoardScreen {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

/// Which border of the tracked screen the halo is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// Dwell stage relevant to cross-machine focus, mapped from the engine's
/// `phase_str` telemetry ("candidate" / "switched" / everything else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPhase {
    Idle,
    Candidate,
    Switched,
}

impl FocusPhase {
    /// Map the engine debug snapshot's dwell stage string.
    pub fn from_phase_str(phase_str: &str) -> Self {
        match phase_str {
            "candidate" => FocusPhase::Candidate,
            "switched" => FocusPhase::Switched,
            _ => FocusPhase::Idle,
        }
    }
}

/// One observation tick fed into [`FocusRouter::observe`].
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    /// Halo position normalized to the tracked (local) screen, 0..=1.
    pub norm: (f64, f64),
    pub phase: FocusPhase,
    /// The screen the engine tracks (the local primary).
    pub local_screen: BoardScreen,
    /// Online peers' screens from the same layout board.
    pub peer_screens: &'a [BoardScreen],
}

/// An event the sender should transmit, with the peer device it is aimed at
/// (Clear repeats the last target so the peer that was highlighted is the
/// one that gets told to clear).
#[derive(Debug, Clone, PartialEq)]
pub struct FocusEmission {
    pub event: HaloFocusEvent,
    pub target_device_id: String,
}

/// Pure decision state for the send side: whether the halo is currently
/// "pointing" at a peer, and which one.
#[derive(Debug, Default)]
struct RouterState {
    active_target: Option<String>,
}

/// Classify the normalized position into an edge side, or `None` when the
/// halo is away from every border. At a corner both axes qualify; the axis
/// closer to its border wins (the deeper excursion into the band).
fn edge_band(norm: (f64, f64)) -> Option<EdgeSide> {
    let (x, y) = (norm.0.clamp(0.0, 1.0), norm.1.clamp(0.0, 1.0));
    let candidates = [
        (EdgeSide::Left, x, x < EDGE_BAND),
        (EdgeSide::Right, 1.0 - x, x > 1.0 - EDGE_BAND),
        (EdgeSide::Top, y, y < EDGE_BAND),
        (EdgeSide::Bottom, 1.0 - y, y > 1.0 - EDGE_BAND),
    ];
    candidates
        .into_iter()
        .filter(|(_, _, at_edge)| *at_edge)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(side, _, _)| side)
}

/// Find the peer screen adjacent to `local` beyond `side`: a screen that
/// ends flush against that border (within [`ADJACENCY_SLACK`]) and overlaps
/// the local screen along the cross axis. The closest by gap wins.
fn neighbor_for_side<'a>(
    local: &BoardScreen,
    peers: &'a [BoardScreen],
    side: EdgeSide,
) -> Option<&'a BoardScreen> {
    let vertical_overlap = |peer: &BoardScreen| -> f64 {
        let top = local.y.max(peer.y);
        let bottom = (local.y + local.height).min(peer.y + peer.height);
        bottom - top
    };
    let horizontal_overlap = |peer: &BoardScreen| -> f64 {
        let left = local.x.max(peer.x);
        let right = (local.x + local.width).min(peer.x + peer.width);
        right - left
    };
    let usable = |peer: &BoardScreen| peer.width > 0.0 && peer.height > 0.0;
    let best = |mut candidates: Vec<&'a BoardScreen>| -> Option<&'a BoardScreen> {
        candidates.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic tie-break
        candidates.first().copied()
    };
    match side {
        EdgeSide::Left => best(
            peers
                .iter()
                .filter(|peer| {
                    usable(peer)
                        && peer.x + peer.width <= local.x + ADJACENCY_SLACK
                        && vertical_overlap(peer) > 0.0
                })
                .collect(),
        ),
        EdgeSide::Right => best(
            peers
                .iter()
                .filter(|peer| {
                    usable(peer)
                        && peer.x >= local.x + local.width - ADJACENCY_SLACK
                        && vertical_overlap(peer) > 0.0
                })
                .collect(),
        ),
        EdgeSide::Top => best(
            peers
                .iter()
                .filter(|peer| {
                    usable(peer)
                        && peer.y + peer.height <= local.y + ADJACENCY_SLACK
                        && horizontal_overlap(peer) > 0.0
                })
                .collect(),
        ),
        EdgeSide::Bottom => best(
            peers
                .iter()
                .filter(|peer| {
                    usable(peer)
                        && peer.y >= local.y + local.height - ADJACENCY_SLACK
                        && horizontal_overlap(peer) > 0.0
                })
                .collect(),
        ),
    }
}

/// Landing point of the halo in layout-board coordinates: the edge point
/// extrapolated `ENTRY_INSET` of the target screen's dimension into the
/// target screen, cross axis clamped into the target's bounds.
fn landing_point(
    local: &BoardScreen,
    target: &BoardScreen,
    side: EdgeSide,
    norm: (f64, f64),
) -> (f64, f64) {
    let clamp_into = |value: f64, screen: &BoardScreen| -> f64 {
        value.clamp(screen.y, screen.y + screen.height)
    };
    match side {
        EdgeSide::Left => (
            target.x + target.width * (1.0 - ENTRY_INSET),
            clamp_into(local.y + norm.1 * local.height, target),
        ),
        EdgeSide::Right => (
            target.x + target.width * ENTRY_INSET,
            clamp_into(local.y + norm.1 * local.height, target),
        ),
        EdgeSide::Top => (
            (local.x + norm.0 * local.width).clamp(target.x, target.x + target.width),
            target.y + target.height * (1.0 - ENTRY_INSET),
        ),
        EdgeSide::Bottom => (
            (local.x + norm.0 * local.width).clamp(target.x, target.x + target.width),
            target.y + target.height * ENTRY_INSET,
        ),
    }
}

/// State-change decision layer: turns observation ticks into at most one
/// pending emission, and emits exactly one Clear when the halo leaves the
/// edge context.
#[derive(Debug, Default)]
pub struct FocusRouter {
    state: RouterState,
}

impl FocusRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one tick. Returns the emission to hand to the sender (which
    /// applies its own throttle), or `None` when this tick changes nothing.
    pub fn observe(&mut self, observation: &Observation) -> Option<FocusEmission> {
        let at_edge = edge_band(observation.norm).and_then(|side| {
            neighbor_for_side(&observation.local_screen, observation.peer_screens, side)
                .map(|neighbor| (side, neighbor))
        });
        let Some((side, neighbor)) = at_edge else {
            // Away from the band, or the band faces another local screen (or
            // nothing at all): retire any active pointer with one Clear.
            let target = self.state.active_target.take()?;
            return Some(FocusEmission {
                event: HaloFocusEvent::Clear,
                target_device_id: target,
            });
        };
        let (x, y) = landing_point(&observation.local_screen, neighbor, side, observation.norm);
        // Wire semantics: the event carries the landing point in the PEER
        // screen's own logical coordinates (origin = that screen's top-left
        // corner), not board coordinates — each machine localizes the shared
        // layout board differently, so board values are meaningless to the
        // receiver. Internal tracking above stays in board space; only the
        // wire fields convert.
        let local = (x - neighbor.x, y - neighbor.y);
        let event = match observation.phase {
            FocusPhase::Switched => HaloFocusEvent::Activate {
                screen_id: neighbor.id.clone(),
                x: local.0,
                y: local.1,
            },
            FocusPhase::Candidate => HaloFocusEvent::Candidate {
                screen_id: neighbor.id.clone(),
                x: local.0,
                y: local.1,
            },
            FocusPhase::Idle => HaloFocusEvent::Edge {
                screen_id: neighbor.id.clone(),
                x: local.0,
                y: local.1,
            },
        };
        self.state.active_target = Some(neighbor.device_id.clone());
        Some(FocusEmission {
            event,
            target_device_id: neighbor.device_id.clone(),
        })
    }

    /// Whether the router is currently pointing at a peer (telemetry).
    pub fn active_target(&self) -> Option<&str> {
        self.state.active_target.as_deref()
    }
}

/// Router + throttle combined: the exact pipeline the debug pump drives.
#[derive(Debug, Default)]
pub struct FocusForwarder {
    router: FocusRouter,
    sender: FocusSender,
}

impl FocusForwarder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one tick and, if a state change survives the throttle, return
    /// `(event, seq, target_device_id)` for envelope construction.
    pub fn observe(&mut self, observation: &Observation) -> Option<(HaloFocusEvent, u64, String)> {
        let emission = self.router.observe(observation)?;
        let seq = self.sender.should_send(&emission.event)?;
        Some((emission.event, seq, emission.target_device_id))
    }
}

// ---------------------------------------------------------------------------
// Receive side: static dispatch + passive peer display
// ---------------------------------------------------------------------------

/// A receiving machine's screen, in the geometry the overlay and the window
/// service need. `id` matches the layout board screen id when available so
/// the landing `screen_id` can select it.
#[derive(Debug, Clone)]
pub struct FocusScreen {
    pub id: String,
    pub geometry: OverlayScreenGeometry,
}

/// Result of applying one remote event, for logging/telemetry.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusDisplayOutcome {
    pub kind: &'static str,
    pub window_title: Option<String>,
    pub highlighted: bool,
    pub activated: bool,
    pub detail: String,
}

/// Callback the app installs at startup; runs the actual UI work (window
/// hit-test, overlay highlight, activation). Called from the QUIC receive
/// thread with no lock held.
pub type FocusReceiveHook = Box<dyn Fn(&HaloFocusPacket) + Send + Sync>;

static RECEIVE_HOOK: OnceLock<FocusReceiveHook> = OnceLock::new();

/// Register the app-level receive hook. Returns false when one was already
/// installed (the app installs it once at transport startup).
pub fn install_receive_hook(hook: FocusReceiveHook) -> bool {
    RECEIVE_HOOK.set(hook).is_ok()
}

/// Authorize an inbound halo focus packet against the layout. Mirrors the
/// semantics of `input.rs::packet_authorized_fields` (constant-time secret
/// compare, credential block must match the local pairing, and the source
/// device must be a paired controller or the legacy client-role origin) so
/// halo traffic can never authenticate more loosely than input traffic.
/// Replicated rather than shared because input.rs only accepts the one
/// decode-branch call from this feature.
fn halo_packet_authorized(layout: &LayoutState, packet: &HaloFocusPacket) -> bool {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return false;
    }
    if packet.cluster_id != layout.cluster_id
        || !crate::consttime_eq(packet.pair_secret.as_bytes(), layout.pair_secret.as_bytes())
    {
        return false;
    }
    if layout.paired_controllers.iter().any(|controller| {
        !packet.source_device_id.trim().is_empty() && controller.id == packet.source_device_id
    }) {
        return true;
    }
    // Legacy client-role origin: a controller paired before ids were
    // stabilized sent "local-device"; accept it exactly as the input path
    // does (the input transport key check has no halo counterpart because
    // the halo envelope carries no transport key — the credential block is
    // the only proof, same strength as a credential-less input packet that
    // just refreshed the authorization cache).
    layout.machine_role == "client"
        && layout.paired_controllers.len() == 1
        && packet.source_device_id == "local-device"
}

static LAST_UNAUTHORIZED_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();

fn warn_unauthorized_halo_packet(source_device_id: &str) {
    let cell = LAST_UNAUTHORIZED_WARN
        .get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(3600)));
    let mut last = match cell.lock() {
        Ok(last) => last,
        Err(_) => return,
    };
    if last.elapsed() < Duration::from_secs(5) {
        return;
    }
    *last = Instant::now();
    log::warn!(
        "halo focus packet rejected: source_device_id={source_device_id} failed pairing authorization"
    );
}

/// Input-chain decode branch: try the halo focus schema on a payload the
/// input and input-control decoders already rejected. Returns `true` when
/// the payload belonged to this protocol (consumed, whether or not it was
/// authorized), `false` when it is something else entirely.
///
/// The layout lock covers only decode + authorization; the hook runs after
/// the guard is dropped so it can take the layout lock itself for the screen
/// snapshot without deadlocking, and slow UI work never holds the lock.
/// Crate-only: the signature exposes the crate-private `LayoutState`.
pub(crate) fn try_dispatch_received_focus(
    layout_state: &Arc<Mutex<LayoutState>>,
    payload: &[u8],
) -> bool {
    let Some(packet) = decode_halo_focus_packet(payload) else {
        return false;
    };
    let authorized = match layout_state.lock() {
        Ok(layout) => halo_packet_authorized(&layout, &packet),
        Err(_) => {
            log::warn!("halo focus packet dropped: layout state lock poisoned");
            return true;
        }
    };
    if !authorized {
        warn_unauthorized_halo_packet(&packet.source_device_id);
        return true;
    }
    match RECEIVE_HOOK.get() {
        Some(hook) => hook(&packet),
        None => log::warn!(
            "halo focus packet from {} dropped: no receive hook installed",
            packet.source_device_id
        ),
    }
    true
}

/// Apply one decoded remote event to this machine's passive display: find
/// the target screen, hit-test the landing point, drive the halo overlay
/// border and (for Activate) the window activator. A `Land` additionally
/// warps this machine's own cursor to the landing point first (S4 takeover:
/// the sender's engine committed a region on one of our screens). The wire
/// point is screen-local logical (origin = the target screen's top-left);
/// everything below converts it through the selected screen's geometry.
/// Shared by the app hook and the `halo_manual focus-receive` rig, so both
/// exercise identical behavior.
pub fn apply_focus_event(
    event: &HaloFocusEvent,
    runtime: &mut HaloRuntime,
    screens: &[FocusScreen],
    windows: &mut dyn WindowService,
    activator: &mut dyn WindowActivator,
    warper: &mut dyn CursorWarper,
) -> FocusDisplayOutcome {
    let kind = event_kind(event);
    let Some((screen_id, x, y)) = event_point(event) else {
        // Clear is the only point-less variant: drop the highlight.
        runtime.clear_remote_highlight();
        log::info!("halo focus: clear remote highlight");
        return FocusDisplayOutcome {
            kind,
            window_title: None,
            highlighted: false,
            activated: false,
            detail: String::new(),
        };
    };
    let Some(geometry) = select_screen(screens, screen_id) else {
        log::warn!("halo focus: landing screen {screen_id} not found locally; ignoring");
        return FocusDisplayOutcome {
            kind,
            window_title: None,
            highlighted: false,
            activated: false,
            detail: format!("unknown screen {screen_id}"),
        };
    };
    // S4 Land: move this machine's own cursor onto the landing point before
    // the focus work below. Screen-local -> native through the selected
    // screen's geometry (`native = native_origin + local * scale`), clamped
    // into the screen so a stale id's out-of-range point still lands on a
    // real pixel of the chosen screen instead of a wild desktop coordinate
    // (the guarantee board_to_native's projection used to provide).
    // Best-effort: a failed warp logs and continues — window activation and
    // the highlight still land where the sender aimed.
    let mut detail_prefix = String::new();
    if matches!(event, HaloFocusEvent::Land { .. }) {
        if geometry.width > 0.0 && geometry.height > 0.0 {
            let scale = safe_scale(geometry.scale);
            let local_x = x.clamp(0.0, geometry.width);
            let local_y = y.clamp(0.0, geometry.height);
            let native_x = geometry.native_x + local_x * scale;
            let native_y = geometry.native_y + local_y * scale;
            let moved = warper.warp(native_x, native_y);
            detail_prefix = format!("warped=({native_x},{native_y}) moved={moved} ");
            log::info!("halo focus: land warp to ({native_x}, {native_y}) accepted={moved}");
        } else {
            detail_prefix = "warp skipped (no usable screen) ".into();
            log::warn!("halo focus: land warp skipped: no usable screen for ({x},{y})");
        }
    }
    // Window enumeration and hit_test live in THIS machine's own layout-board
    // logical space (macOS CG global points pass through with an identity
    // board mapping; Windows native pixels are converted per screen at
    // enumerate time), so the wire's screen-local point is converted through
    // the selected screen's board origin.
    let board_x = geometry.x + x;
    let board_y = geometry.y + y;
    let hit = match windows.hit_test(board_x, board_y) {
        Ok(hit) => hit,
        Err(error) => {
            log::warn!("halo focus: hit-test failed at board ({board_x},{board_y}): {error}");
            runtime.clear_remote_highlight();
            return FocusDisplayOutcome {
                kind,
                window_title: None,
                highlighted: false,
                activated: false,
                detail: format!("hit-test failed: {error}"),
            };
        }
    };
    let Some(window) = hit else {
        // Nothing under the point: clear any stale highlight so the border
        // cannot keep pointing at a window that already moved away.
        runtime.clear_remote_highlight();
        log::info!(
            "halo focus: {kind} at screen-local ({x},{y}) on {screen_id} hit no window; highlight cleared"
        );
        return FocusDisplayOutcome {
            kind,
            window_title: None,
            highlighted: false,
            activated: false,
            detail: "no window under point".into(),
        };
    };
    let frame = BorderFrame {
        x: window.frame.0,
        y: window.frame.1,
        w: window.frame.2,
        h: window.frame.3,
    };
    let highlighted = runtime.show_remote_highlight(frame, geometry);
    let mut activated = false;
    let mut detail = detail_prefix;
    if matches!(
        event,
        HaloFocusEvent::Activate { .. } | HaloFocusEvent::Land { .. }
    ) {
        // Land reuses the exact Activate activation path: raise the window
        // under the landing point so the takeover arrives on the window the
        // sender was looking at.
        let activation = activator.activate(window.id);
        activated = activation.activated;
        detail.push_str(&format!(
            "activated={} fell_back={} {}",
            activation.activated, activation.fell_back, activation.detail
        ));
    }
    log::info!(
        "halo focus: {kind} at screen-local ({x},{y}) screen={screen_id} highlight={highlighted} window={:?} activated={activated} {detail}",
        window.title
    );
    FocusDisplayOutcome {
        kind,
        window_title: Some(window.title.clone()),
        highlighted,
        activated,
        detail,
    }
}

fn event_kind(event: &HaloFocusEvent) -> &'static str {
    match event {
        HaloFocusEvent::Edge { .. } => "edge",
        HaloFocusEvent::Candidate { .. } => "candidate",
        HaloFocusEvent::Activate { .. } => "activate",
        HaloFocusEvent::Land { .. } => "land",
        HaloFocusEvent::Clear => "clear",
    }
}

fn event_point(event: &HaloFocusEvent) -> Option<(&str, f64, f64)> {
    match event {
        HaloFocusEvent::Edge { screen_id, x, y }
        | HaloFocusEvent::Candidate { screen_id, x, y }
        | HaloFocusEvent::Activate { screen_id, x, y }
        | HaloFocusEvent::Land { screen_id, x, y } => Some((screen_id, *x, *y)),
        HaloFocusEvent::Clear => None,
    }
}

fn activation_detail(activation: &ActivationOutcome) -> String {
    format!(
        "activated={} fell_back={} {}",
        activation.activated, activation.fell_back, activation.detail
    )
}

/// Pick the overlay host screen for a landing event: the screen whose id
/// matches the event exactly, then a suffix-normalized match, then the first
/// screen as a last resort (single-screen rigs). The old board-containment
/// fallback is gone on purpose: wire points are screen-local, so a point no
/// longer identifies a screen. The last-resort fallback warns so a silently
/// mis-aimed event stays visible in the logs.
fn select_screen(screens: &[FocusScreen], screen_id: &str) -> Option<OverlayScreenGeometry> {
    if screens.is_empty() {
        return None;
    }
    screens
        .iter()
        .find(|screen| screen.id == screen_id)
        .or_else(|| {
            normalized_screen_id(screen_id)
                .and_then(|normalized| screens.iter().find(|screen| screen.id == normalized))
        })
        .map(|screen| screen.geometry)
        .or_else(|| {
            log::warn!(
                "halo focus: landing screen {screen_id} matched no local screen; falling back to the first screen"
            );
            screens.first().map(|screen| screen.geometry)
        })
}

/// Normalize an inbound screen id against this machine's own naming: the
/// sender sees our screens as "{device-id}-local-display-N" (its layout
/// prefixes them with our device id) while we name them "local-display-N".
/// `rsplit_once` keys on the LAST "-local-display-" separator, so a device id
/// that itself contains hyphens ("peer-my-host-10.0.0.9-local-display-2")
/// still resolves to the trailing display suffix. Ids without the marker
/// need no normalization.
fn normalized_screen_id(screen_id: &str) -> Option<String> {
    let (_, suffix) = screen_id.rsplit_once("-local-display-")?;
    (!suffix.is_empty()).then(|| format!("local-display-{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::halo::windows_layer::hit_test_windows;
    use crate::halo::{warp::WarpRecorder, WindowInfo};

    fn screen(id: &str, device: &str, x: f64, y: f64, w: f64, h: f64) -> BoardScreen {
        BoardScreen {
            id: id.into(),
            device_id: device.into(),
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn local() -> BoardScreen {
        screen("local-1", "dev-local", 0.0, 0.0, 1000.0, 500.0)
    }

    fn obs<'a>(norm: (f64, f64), phase: FocusPhase, peers: &'a [BoardScreen]) -> Observation<'a> {
        Observation {
            norm,
            phase,
            local_screen: local(),
            peer_screens: peers,
        }
    }

    #[test]
    fn edge_band_matches_eight_percent_of_each_axis() {
        assert_eq!(edge_band((0.5, 0.5)), None);
        assert_eq!(edge_band((0.079, 0.5)), Some(EdgeSide::Left));
        assert_eq!(edge_band((0.921, 0.5)), Some(EdgeSide::Right));
        assert_eq!(edge_band((0.5, 0.079)), Some(EdgeSide::Top));
        assert_eq!(edge_band((0.5, 0.921)), Some(EdgeSide::Bottom));
        // Just outside the band on every axis.
        assert_eq!(edge_band((0.09, 0.09)), None);
        // Corner: the deeper excursion wins.
        assert_eq!(edge_band((0.01, 0.07)), Some(EdgeSide::Left));
        assert_eq!(edge_band((0.07, 0.01)), Some(EdgeSide::Top));
        assert_eq!(edge_band((0.99, 0.93)), Some(EdgeSide::Right));
    }

    #[test]
    fn phase_str_maps_to_focus_phase() {
        assert_eq!(
            FocusPhase::from_phase_str("candidate"),
            FocusPhase::Candidate
        );
        assert_eq!(FocusPhase::from_phase_str("switched"), FocusPhase::Switched);
        assert_eq!(FocusPhase::from_phase_str("idle"), FocusPhase::Idle);
        assert_eq!(FocusPhase::from_phase_str("settling"), FocusPhase::Idle);
        assert_eq!(FocusPhase::from_phase_str("cooldown"), FocusPhase::Idle);
    }

    #[test]
    fn left_neighbor_triggers_edge_with_extrapolated_landing() {
        // Peer screen sits flush left of the local screen.
        let peer = screen("peer-s", "dev-peer", -1200.0, 0.0, 1200.0, 600.0);
        let peers = vec![peer.clone()];
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &peers))
            .expect("edge arrival emits");
        assert_eq!(emission.target_device_id, "dev-peer");
        match emission.event {
            HaloFocusEvent::Edge { screen_id, x, y } => {
                assert_eq!(screen_id, "peer-s");
                // Peer-screen-local: 5% of the peer's width in from its
                // right edge (board -60 minus the peer's board origin).
                assert!((x - (1200.0 * 0.95)).abs() < 1e-9);
                // Cross axis: (local.y + 0.5 * local height) - peer.y.
                assert!((y - 250.0).abs() < 1e-9);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    #[test]
    fn right_top_bottom_layouts_all_resolve_neighbors() {
        let right_peer = screen("p-r", "dev-r", 1000.0, 0.0, 800.0, 500.0);
        let top_peer = screen("p-t", "dev-t", 0.0, -400.0, 1000.0, 400.0);
        let bottom_peer = screen("p-b", "dev-b", 0.0, 500.0, 1000.0, 400.0);
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.95, 0.5), FocusPhase::Idle, &[right_peer]))
            .expect("right neighbor");
        assert_eq!(emission.target_device_id, "dev-r");
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.5, 0.03), FocusPhase::Idle, &[top_peer]))
            .expect("top neighbor");
        assert_eq!(emission.target_device_id, "dev-t");
        match emission.event {
            HaloFocusEvent::Edge { y, .. } => {
                // Peer-screen-local y: 5% of the peer's height in from its
                // bottom edge (board -20 minus the peer's board origin).
                assert!((y - (400.0 * 0.95)).abs() < 1e-9)
            }
            other => panic!("expected Edge, got {other:?}"),
        }
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.5, 0.97), FocusPhase::Idle, &[bottom_peer]))
            .expect("bottom neighbor");
        assert_eq!(emission.target_device_id, "dev-b");
    }

    #[test]
    fn forwarder_events_carry_peer_screen_local_coordinates() {
        // Both axes offset: a peer sitting right of AND below the local
        // screen. The wire x/y must be that peer screen's own logical
        // coordinates (origin at its top-left), not board coordinates —
        // the two machines' boards are not synchronized.
        let peer = screen("peer-s", "dev-peer", 15624.0, 5120.0, 3840.0, 2160.0);
        // Adjacency for the router's board math: local must actually touch
        // the peer, so shift the peer flush right of the local screen for
        // the observation itself.
        let peers = vec![screen("peer-s", "dev-peer", 1000.0, 0.0, 800.0, 500.0)];
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.95, 0.5), FocusPhase::Idle, &peers))
            .expect("right neighbor");
        match emission.event {
            HaloFocusEvent::Edge { x, y, .. } => {
                // Board (1040, 250) minus the peer board origin (1000, 0).
                assert!((x - (800.0 * 0.05)).abs() < 1e-9, "local x {x}");
                assert!((y - 250.0).abs() < 1e-9, "local y {y}");
                assert!(x >= 0.0 && x <= 800.0 && y >= 0.0 && y <= 500.0);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
        // The far-offset board above stays valid input: with the peer
        // non-adjacent the router emits nothing (no neighbor), proving the
        // conversion is anchored to the neighbor's own origin, not the board.
        let mut router = FocusRouter::new();
        assert!(router
            .observe(&obs((0.95, 0.5), FocusPhase::Idle, &[peer]))
            .is_none());
    }

    #[test]
    fn no_peer_screen_in_band_direction_emits_nothing() {
        let mut router = FocusRouter::new();
        // Only a screen far below with no vertical overlap: not a neighbor.
        let distant = screen("far", "dev-far", 2000.0, 900.0, 500.0, 300.0);
        assert!(router
            .observe(&obs((0.04, 0.5), FocusPhase::Idle, &[distant]))
            .is_none());
        // Empty peer list: same.
        assert!(router
            .observe(&obs((0.04, 0.5), FocusPhase::Idle, &[]))
            .is_none());
        assert!(router.active_target().is_none());
    }

    #[test]
    fn leaving_the_band_emits_one_clear_for_the_last_target() {
        let peer = screen("peer-s", "dev-peer", -1200.0, 0.0, 1200.0, 600.0);
        let peers = vec![peer];
        let mut router = FocusRouter::new();
        router
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &peers))
            .expect("arrival");
        // Staying at the edge keeps emitting (the throttle, not the router,
        // collapses repeats).
        assert!(router
            .observe(&obs((0.04, 0.5), FocusPhase::Idle, &peers))
            .is_some());
        // Leaving the band: exactly one Clear aimed at the same device.
        let clear = router
            .observe(&obs((0.5, 0.5), FocusPhase::Idle, &peers))
            .expect("departure clears");
        assert_eq!(clear.event, HaloFocusEvent::Clear);
        assert_eq!(clear.target_device_id, "dev-peer");
        // And nothing afterwards.
        assert!(router
            .observe(&obs((0.5, 0.5), FocusPhase::Idle, &peers))
            .is_none());
    }

    #[test]
    fn losing_the_neighbor_also_clears() {
        let peer = screen("peer-s", "dev-peer", -1200.0, 0.0, 1200.0, 600.0);
        let mut router = FocusRouter::new();
        let moved = vec![screen("peer-s", "dev-peer", -1200.0, 700.0, 1200.0, 600.0)];
        router
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &[peer]))
            .expect("arrival");
        // Peer re-arranged so it no longer overlaps: the pointer retires.
        let clear = router
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &moved))
            .expect("neighbor loss clears");
        assert_eq!(clear.event, HaloFocusEvent::Clear);
    }

    #[test]
    fn dwell_phase_selects_candidate_and_activate_events() {
        let peer = screen("peer-s", "dev-peer", 1000.0, 0.0, 800.0, 500.0);
        let peers = vec![peer];
        let mut router = FocusRouter::new();
        let emission = router
            .observe(&obs((0.95, 0.5), FocusPhase::Candidate, &peers))
            .expect("candidate");
        assert!(matches!(emission.event, HaloFocusEvent::Candidate { .. }));
        let emission = router
            .observe(&obs((0.95, 0.5), FocusPhase::Switched, &peers))
            .expect("activate");
        assert!(matches!(emission.event, HaloFocusEvent::Activate { .. }));
    }

    #[test]
    fn forwarder_applies_sender_throttle_and_returns_seq_and_target() {
        let peer = screen("peer-s", "dev-peer", -1200.0, 0.0, 1200.0, 600.0);
        let peers = vec![peer];
        let mut forwarder = FocusForwarder::new();
        // First tick sends.
        let (_, seq, target) = forwarder
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &peers))
            .expect("first tick sends");
        assert_eq!(seq, 1);
        assert_eq!(target, "dev-peer");
        // The tick right after is the same state: router emits again but the
        // 150 ms throttle collapses it.
        assert!(forwarder
            .observe(&obs((0.05, 0.5), FocusPhase::Idle, &peers))
            .is_none());
        // Departure is a state change: goes out immediately.
        let (event, seq, target) = forwarder
            .observe(&obs((0.5, 0.5), FocusPhase::Idle, &peers))
            .expect("departure sends");
        assert_eq!(event, HaloFocusEvent::Clear);
        assert_eq!(seq, 2);
        assert_eq!(target, "dev-peer");
    }

    #[test]
    fn vertical_overlap_required_for_horizontal_neighbors() {
        // Peer left of local but its vertical range barely misses: no handoff.
        let almost = screen("p", "dev-p", -800.0, 505.0, 800.0, 300.0);
        let mut router = FocusRouter::new();
        assert!(router
            .observe(&obs((0.04, 0.5), FocusPhase::Idle, &[almost]))
            .is_none());
    }

    // ----- receive side: screen selection + screen-local point handling -----

    fn geometry(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        scale: f64,
        native_x: f64,
        native_y: f64,
    ) -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x,
            y,
            width,
            height,
            scale,
            native_x,
            native_y,
        }
    }

    fn focus_screen(id: &str, geometry: OverlayScreenGeometry) -> FocusScreen {
        FocusScreen {
            id: id.into(),
            geometry,
        }
    }

    fn window(id: u64, frame: (f64, f64, f64, f64)) -> WindowInfo {
        WindowInfo {
            id,
            title: "w".into(),
            process: "app".into(),
            frame,
            is_foreground: true,
        }
    }

    /// Window-layer double: records every hit-test point (pinning the
    /// board-space conversion) and answers with the configured list through
    /// the real pure hit-test.
    struct FakeWindows {
        windows: Vec<WindowInfo>,
        hit_points: Vec<(f64, f64)>,
    }

    impl FakeWindows {
        fn with_window(frame: (f64, f64, f64, f64)) -> Self {
            FakeWindows {
                windows: vec![window(7, frame)],
                hit_points: Vec::new(),
            }
        }

        fn hit_points(&self) -> &[(f64, f64)] {
            &self.hit_points
        }
    }

    impl WindowService for FakeWindows {
        fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
            Ok(self.windows.clone())
        }

        fn hit_test(&mut self, x: f64, y: f64) -> Result<Option<WindowInfo>, String> {
            self.hit_points.push((x, y));
            Ok(hit_test_windows(&self.windows, x, y))
        }

        fn invalidate(&mut self) {}
    }

    /// Activator double: records the activated window ids.
    struct FakeActivator {
        activated: Vec<u64>,
    }

    impl WindowActivator for FakeActivator {
        fn activate(&mut self, window_id: u64) -> ActivationOutcome {
            self.activated.push(window_id);
            ActivationOutcome {
                activated: true,
                fell_back: false,
                detail: "fake".into(),
            }
        }
    }

    fn receiver_screens() -> Vec<FocusScreen> {
        // This machine's own screens in its own board space: display-2 sits
        // right of display-1 at a 2x DPI with a non-zero native origin.
        vec![
            focus_screen(
                "local-display-1",
                geometry(0.0, 0.0, 1000.0, 500.0, 1.0, 0.0, 0.0),
            ),
            focus_screen(
                "local-display-2",
                geometry(1000.0, 0.0, 800.0, 500.0, 2.0, 2000.0, 0.0),
            ),
        ]
    }

    #[test]
    fn normalized_screen_id_strips_the_device_prefix() {
        // The sender prefixes our screen ids with our device id
        // (peer-{hostname}-{ip}); a hyphenated hostname must not confuse the
        // extraction (rsplit_once keys on the last marker).
        assert_eq!(
            normalized_screen_id("peer-my-host-10.0.0.9-local-display-2").as_deref(),
            Some("local-display-2")
        );
        // An id with no marker carries nothing to strip: it resolves through
        // the exact-match step instead (an already-local id, or a foreign
        // naming scheme that then falls back with a warning).
        assert_eq!(normalized_screen_id("local-display-1"), None);
        assert_eq!(normalized_screen_id("screen-9"), None);
        // A marker with nothing after it produces no needle.
        assert_eq!(normalized_screen_id("peer-x-local-display-"), None);
    }

    #[test]
    fn select_screen_matches_exact_then_suffix_then_warns_to_first() {
        let screens = receiver_screens();
        // Exact id match.
        assert_eq!(
            select_screen(&screens, "local-display-2").map(|g| g.x),
            Some(1000.0)
        );
        // Suffix-normalized match: the sender's view of our display-2.
        assert_eq!(
            select_screen(&screens, "peer-my-host-10.0.0.9-local-display-2").map(|g| g.x),
            Some(1000.0)
        );
        // Unmatched ids fall back to the first screen (the warn path —
        // logged, never silent).
        assert_eq!(select_screen(&screens, "screen-9").map(|g| g.x), Some(0.0));
        // Empty screen list stays None.
        assert!(select_screen(&[], "local-display-1").is_none());
    }

    #[test]
    fn land_warps_screen_local_to_native_and_hit_tests_in_board_space() {
        // The inbound id is the sender's name for our display-2 (device
        // prefix + hyphenated host); the point is screen-local logical.
        let event = HaloFocusEvent::Land {
            screen_id: "peer-my-host-10.0.0.9-local-display-2".into(),
            x: 40.0,
            y: 200.0,
        };
        let mut runtime = HaloRuntime::new();
        let mut windows = FakeWindows::with_window((1000.0, 150.0, 300.0, 100.0));
        let mut activator = FakeActivator {
            activated: Vec::new(),
        };
        let recorder = WarpRecorder::new(true);
        let mut warper: Box<dyn CursorWarper> = Box::new(recorder.clone());
        let outcome = apply_focus_event(
            &event,
            &mut runtime,
            &receiver_screens(),
            &mut windows,
            &mut activator,
            warper.as_mut(),
        );
        // Land warp: native = native_origin + local * scale on the selected
        // screen (display-2: native (2000,0), 2x) -> (2080, 400).
        assert_eq!(recorder.calls(), vec![(2080.0, 400.0)]);
        // hit_test ran in this machine's board space (display-2's board
        // origin (1000,0) added back onto the local point).
        assert_eq!(windows.hit_points(), &[(1040.0, 200.0)]);
        assert!(
            outcome.activated,
            "land activates the window under the point"
        );
        assert_eq!(outcome.window_title.as_deref(), Some("w"));
        assert_eq!(activator.activated, vec![7]);
        assert!(outcome.detail.contains("warped=(2080,400)"));
        runtime.clear_remote_highlight();
    }

    #[test]
    fn land_clamps_out_of_range_local_points_into_the_screen() {
        let event = HaloFocusEvent::Land {
            screen_id: "local-display-2".into(),
            x: -50.0,
            y: 600.0,
        };
        let mut runtime = HaloRuntime::new();
        let mut windows = FakeWindows::with_window((1000.0, 150.0, 300.0, 100.0));
        let mut activator = FakeActivator {
            activated: Vec::new(),
        };
        let recorder = WarpRecorder::new(true);
        let mut warper: Box<dyn CursorWarper> = Box::new(recorder.clone());
        let _outcome = apply_focus_event(
            &event,
            &mut runtime,
            &receiver_screens(),
            &mut windows,
            &mut activator,
            warper.as_mut(),
        );
        // Out-of-range local coords clamp into the selected screen before
        // the scale conversion: (-50,600) -> (0,500) -> native (2000,1000).
        assert_eq!(recorder.calls(), vec![(2000.0, 1000.0)]);
        runtime.clear_remote_highlight();
    }
}
