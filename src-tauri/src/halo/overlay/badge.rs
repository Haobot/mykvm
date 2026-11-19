//! Screen-calibration badge overlay (S2): one click-through full-screen
//! window per display showing a large numbered label in the middle and L
//! brackets at the four physical corners, so the multi-screen region wizard
//! can point at "screen 3, top-right corner".
//!
//! The implementation follows the same battle-tested pattern as `macos.rs`
//! (raw objc_msgSend + `dispatch_async_f` marshaling to the main queue,
//! windows kept alive for reuse). One macOS 27 quirk is load-bearing: like
//! the halo ring (`start_border_breathing` / `apply_frame`), sublayers must
//! be revealed in a SECOND main-queue block after the window has been
//! ordered front — layers un-hidden inside the creation block before the
//! window's first display never composite (verified on macOS 27: the border
//! overlay built the same way shows nothing). `badge_show` therefore builds
//! everything hidden, orders the windows front, and a follow-up
//! `badge_reveal` block flips visibility on.
//!
//! Windows live in one main-thread-owned state struct guarded by a
//! process-wide mutex; the lock is only ever taken briefly (bookkeeping),
//! never around blocking calls, so it cannot observe or violate the
//! halo/layout lock-order contract.
//!
//! Platform note: only macOS is implemented. Windows (win32) reuses the
//! overlay machinery for the halo but has no badge path yet — `BadgeLayer`
//! degrades to an inert no-op there (TODO, see win32.rs).

#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::Mutex;

use crate::halo::overlay::OverlayScreenGeometry;

#[cfg(target_os = "macos")]
use super::macos::{
    cg_color, cls, make_click_through_window, on_main, sel, send, send1, send2, send_f64, set_bool,
    set_f64, set_rect, CGPathAddLineToPoint, CGPathCreateMutable, CGPathMoveToPoint, CGPathRelease,
    NsRect,
};

// Same extern surface as macos.rs: the objc runtime entries plus the three
// CGPath functions this module needs (core-graphics 0.25 does not export
// the mutable-path C API directly).
#[cfg(target_os = "macos")]
extern "C" {
    fn objc_msgSend();
}

/// NSWindow setFrame:display: (NSRect, BOOL); re-applied in the reveal step
/// to force a window-server surface update.
#[cfg(target_os = "macos")]
unsafe fn set_frame_display(obj: *mut c_void, rect: NsRect, display: bool) {
    let f: extern "C" fn(*mut c_void, *mut c_void, NsRect, bool) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(b"setFrame:display:\0"), rect, display);
}

/// One screen in the badge layout: geometry plus the human-readable number
/// ("1".."N") the wizard assigned in board order.
#[derive(Debug, Clone)]
pub struct BadgeScreen {
    pub geometry: OverlayScreenGeometry,
    pub label: String,
}

/// Which physical corner of a screen is currently being captured. Names are
/// from the user's viewpoint looking AT the screen, matching
/// `screen_map::ScreenRegion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeCorner {
    Tl,
    Tr,
    Br,
    Bl,
}

impl BadgeCorner {
    /// Index into the per-screen corner-layer array ([Tl, Tr, Br, Bl]).
    fn slot(self) -> usize {
        match self {
            BadgeCorner::Tl => 0,
            BadgeCorner::Tr => 1,
            BadgeCorner::Br => 2,
            BadgeCorner::Bl => 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Visual constants (points). Keep in sync with the bracket geometry tests.
// ---------------------------------------------------------------------------

/// Bracket inset from the screen's physical corners.
pub(crate) const BADGE_INSET: f64 = 90.0;
/// Arm length of each L bracket, measured from the corner point along each
/// screen edge.
pub(crate) const BADGE_ARM: f64 = 64.0;
/// Bracket stroke width.
pub(crate) const BADGE_LINE_WIDTH: f64 = 8.0;
/// Number label font size (points).
pub(crate) const BADGE_FONT_SIZE: f64 = 220.0;
/// Line-height factor used to size the CATextLayer so a single line centers
/// on the screen middle.
const BADGE_LINE_HEIGHT_FACTOR: f64 = 1.2;
/// Label white alpha (baked into the stroke color; see the macOS 27 note in
/// `make_bracket_layer` — the layer `opacity` property is never touched).
pub(crate) const BADGE_LABEL_ALPHA: f64 = 0.85;
/// Idle bracket alpha (all four corners, uniform).
pub(crate) const BADGE_BRACKET_ALPHA: f64 = 0.8;
/// Non-active corners dim to this while one corner is being captured.
pub(crate) const BADGE_DIM_ALPHA: f64 = 0.35;
/// The active corner sits at full white plus the breathing animation below.
pub(crate) const BADGE_ACTIVE_ALPHA: f64 = 1.0;
/// Active-corner breathing range and period (s): the same Core Animation
/// autoreverse pattern as the candidate border. The animation drives only
/// the presentation layer; the model color stays at BADGE_ACTIVE_ALPHA so a
/// stuck animation degrades to a steady highlight.
const BADGE_BREATH_MIN: f64 = 0.55;
const BADGE_BREATH_MAX: f64 = 1.0;
const BADGE_BREATH_SECONDS: f64 = 0.9;

/// Bracket white (r, g, b) at a given alpha; the alpha is baked into the
/// color because layer `opacity` must stay untouched on macOS 27.
fn badge_white(alpha: f64) -> (f64, f64, f64, f64) {
    (1.0, 1.0, 1.0, alpha)
}

// ---------------------------------------------------------------------------
// Pure geometry (unit-tested; no objc involved).
// ---------------------------------------------------------------------------

/// One corner's L bracket in layer coordinates (bottom-left origin): the
/// shared corner point plus the endpoints of the horizontal and vertical
/// arms. `horizontal` runs along the top/bottom edge, `vertical` along the
/// left/right edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BracketArms {
    pub corner: (f64, f64),
    pub horizontal: (f64, f64),
    pub vertical: (f64, f64),
}

/// The four L brackets for a `width x height` screen, ordered [TL, TR, BR,
/// BL] in layer coordinates (bottom-left origin): the screen's top-left is
/// `(inset, height - inset)`. Inset/arm clamp to sane ranges so a degenerate
/// geometry can never produce an inverted or negative path.
pub fn bracket_geometry(width: f64, height: f64, inset: f64, arm: f64) -> [BracketArms; 4] {
    let inset = inset.max(1.0);
    let arm = arm.max(1.0);
    let left = inset;
    let right = (width - inset).max(left + 1.0);
    let bottom = inset;
    let top = (height - inset).max(bottom + 1.0);
    [
        // Top-left: arms point right (+x) and down (-y from the top edge).
        BracketArms {
            corner: (left, top),
            horizontal: (left + arm, top),
            vertical: (left, top - arm),
        },
        // Top-right: arms point left and down.
        BracketArms {
            corner: (right, top),
            horizontal: (right - arm, top),
            vertical: (right, top - arm),
        },
        // Bottom-right: arms point left and up.
        BracketArms {
            corner: (right, bottom),
            horizontal: (right - arm, bottom),
            vertical: (right, bottom + arm),
        },
        // Bottom-left: arms point right and up.
        BracketArms {
            corner: (left, bottom),
            horizontal: (left + arm, bottom),
            vertical: (left, bottom + arm),
        },
    ]
}

/// Frame of the number label so its single text line centers on the screen
/// middle. CATextLayer draws from the top of its bounds, so the layer is
/// sized to exactly one line box and positioned at (center - line/2).
#[cfg(target_os = "macos")]
pub(crate) fn label_frame(width: f64, height: f64, font_size: f64) -> NsRect {
    let line_height = font_size * BADGE_LINE_HEIGHT_FACTOR;
    let label_width = (width * 0.5).max(line_height * 3.0);
    NsRect {
        x: (width - label_width) / 2.0,
        y: (height - line_height) / 2.0,
        w: label_width,
        h: line_height,
    }
}

/// CG global (top-left origin) screen rect -> NS window frame (bottom-left
/// origin), same flip anchor as macos.rs: `ns_y = primary_height - cg_y -
/// size`.
#[cfg(target_os = "macos")]
pub(crate) fn badge_ns_frame(geometry: &OverlayScreenGeometry, primary_height: f64) -> NsRect {
    NsRect {
        x: geometry.native_x,
        y: primary_height - geometry.native_y - geometry.height,
        w: geometry.width,
        h: geometry.height,
    }
}

// ---------------------------------------------------------------------------
// Main-thread window state. Windows and layers are stored as raw usize
// pointers; all mutation happens inside dispatch callbacks on the main
// queue, and non-main callers only dispatch boxes into the queue.
// ---------------------------------------------------------------------------

/// Per shown screen: window pointer, its NS frame, label layer, and the four
/// corner bracket layers in [Tl, Tr, Br, Bl] order.
#[derive(Clone, Copy)]
#[cfg(target_os = "macos")]
struct BadgeWindow {
    window: usize,
    frame: NsRect,
    label: usize,
    corners: [usize; 4],
}

#[derive(Default)]
#[cfg(target_os = "macos")]
struct BadgeWindowState {
    screens: Vec<BadgeWindow>,
}

#[cfg(target_os = "macos")]
static BADGE_STATE: Mutex<Option<BadgeWindowState>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn with_badge_state<T>(f: impl FnOnce(&mut BadgeWindowState) -> T) -> Option<T> {
    let mut guard = BADGE_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_mut().map(f)
}

/// Payloads for the dispatch callbacks below; each callback takes ownership
/// via `Box::from_raw`.
#[cfg(target_os = "macos")]
struct ShowPayload {
    screens: Vec<BadgeScreen>,
}
#[cfg(target_os = "macos")]
struct ActivePayload {
    screen_index: usize,
    corner: Option<BadgeCorner>,
}

/// Build every badge window (contents hidden) and order them front.
#[cfg(target_os = "macos")]
extern "C" fn badge_show(ctx: *mut c_void) {
    let payload = unsafe { Box::from_raw(ctx as *mut ShowPayload) };
    unsafe {
        let mut built: Vec<BadgeWindow> = Vec::with_capacity(payload.screens.len());
        let primary_height = core_graphics::display::CGDisplay::main()
            .bounds()
            .size
            .height;
        for screen in &payload.screens {
            let frame = badge_ns_frame(&screen.geometry, primary_height);
            let (window, host) = make_click_through_window(frame);
            if window.is_null() {
                continue;
            }
            let mut corners = [0_usize; 4];
            let mut label = 0_usize;
            if !host.is_null() {
                let scale = send_f64(window, b"backingScaleFactor\0");
                let brackets = bracket_geometry(frame.w, frame.h, BADGE_INSET, BADGE_ARM);
                for (slot, arms) in brackets.iter().enumerate() {
                    let layer = make_bracket_layer(arms, frame, scale);
                    if layer.is_null() {
                        continue;
                    }
                    send1(host, sel(b"addSublayer:\0"), layer);
                    corners[slot] = layer as usize;
                }
                let label_layer = make_label_layer(&screen.label, frame, scale);
                if !label_layer.is_null() {
                    send1(host, sel(b"addSublayer:\0"), label_layer);
                    label = label_layer as usize;
                }
            }
            send(window, sel(b"orderFrontRegardless\0"));
            built.push(BadgeWindow {
                window: window as usize,
                frame,
                label,
                corners,
            });
        }
        if let Ok(mut guard) = BADGE_STATE.lock() {
            // Replace any previous set: show() always rebuilds so a topology
            // change between wizard runs cannot leave stale windows. Old
            // windows are ordered out first.
            if let Some(old) = guard.as_ref() {
                for screen in &old.screens {
                    send1(
                        screen.window as *mut c_void,
                        sel(b"orderOut:\0"),
                        std::ptr::null_mut(),
                    );
                }
            }
            *guard = Some(BadgeWindowState { screens: built });
        }
        // Reveal in a follow-up main-queue block (see module docs: layers
        // un-hidden before the window's first display never composite on
        // macOS 27).
        on_main(badge_reveal, std::ptr::null_mut());
    }
}

/// Flip every badge window's sublayers visible and force the window server
/// to recomposite. Mirrors the halo ring's deferred `apply_frame` +
/// `move_window` sequence: on macOS 27, a borderless non-opaque window that
/// is ordered front without a subsequent frame touch never composites its
/// layer-backed subviews (verified live: the candidate border built in one
/// block shows nothing; the ring, which is moved after ordering, renders).
#[cfg(target_os = "macos")]
extern "C" fn badge_reveal(_ctx: *mut c_void) {
    let Some(screens) = with_badge_state(|state| state.screens.clone()) else {
        return;
    };
    unsafe {
        for screen in &screens {
            for layer in screen.corners.iter() {
                let layer = *layer as usize as *mut c_void;
                if layer.is_null() {
                    continue;
                }
                send1(
                    layer,
                    sel(b"setStrokeColor:\0"),
                    cg_color(badge_white(BADGE_BRACKET_ALPHA)),
                );
                send1(layer, sel(b"setFillColor:\0"), std::ptr::null_mut());
                set_f64(layer, b"setLineWidth:\0", BADGE_LINE_WIDTH);
                set_bool(layer, b"setHidden:\0", false);
            }
            let label = screen.label as usize as *mut c_void;
            if !label.is_null() {
                set_bool(label, b"setHidden:\0", false);
            }
            let window = screen.window as *mut c_void;
            if !window.is_null() {
                // Re-apply the same frame with display:YES: the window-server
                // surface update this forces is what makes the freshly
                // un-hidden layers actually composite (ring parity).
                set_frame_display(window, screen.frame, true);
                set_f64(window, b"setAlphaValue:\0", 1.0);
            }
        }
    }
}

/// One L-bracket stroke layer from pure geometry. `screen` is the full
/// window rect: the bracket layer's frame must cover the whole screen or its
/// zero bounds would clip the path away. Created hidden; `badge_reveal`
/// flips visibility. Path is handed to the strong `setPath:` property (+1
/// transferred); color ownership likewise.
#[cfg(target_os = "macos")]
unsafe fn make_bracket_layer(arms: &BracketArms, screen: NsRect, scale: f64) -> *mut c_void {
    let alloc = send(cls(b"CAShapeLayer\0"), sel(b"alloc\0"));
    let layer = send(alloc, sel(b"init\0"));
    if layer.is_null() {
        return std::ptr::null_mut();
    }
    // macOS 27 note: never touch the layer `opacity` property — a direct
    // setOpacity: strands the layer at a ~0 presentation opacity (implicit
    // fade action stuck; verified live). All brightness states are baked
    // into the stroke color's alpha instead.
    let path = CGPathCreateMutable();
    if path.is_null() {
        return std::ptr::null_mut();
    }
    CGPathMoveToPoint(path, std::ptr::null(), arms.horizontal.0, arms.horizontal.1);
    CGPathAddLineToPoint(path, std::ptr::null(), arms.corner.0, arms.corner.1);
    CGPathAddLineToPoint(path, std::ptr::null(), arms.vertical.0, arms.vertical.1);
    send1(layer, sel(b"setPath:\0"), path);
    CGPathRelease(path);
    send1(
        layer,
        sel(b"setStrokeColor:\0"),
        cg_color(badge_white(BADGE_BRACKET_ALPHA)),
    );
    send1(layer, sel(b"setFillColor:\0"), std::ptr::null_mut());
    set_f64(layer, b"setLineWidth:\0", BADGE_LINE_WIDTH);
    set_f64(layer, b"setContentsScale:\0", scale);
    set_rect(layer, b"setFrame:\0", screen);
    set_bool(layer, b"setHidden:\0", true);
    layer
}

/// One CATextLayer showing `label`, centered on the screen. Created hidden
/// (revealed with the rest). The string is built with alloc/initWith so the
/// layer's strong string property owns it and this code never touches the
/// autorelease pool for it.
#[cfg(target_os = "macos")]
unsafe fn make_label_layer(label: &str, frame: NsRect, scale: f64) -> *mut c_void {
    let alloc = send(cls(b"CATextLayer\0"), sel(b"alloc\0"));
    let layer = send(alloc, sel(b"init\0"));
    if layer.is_null() {
        return std::ptr::null_mut();
    }
    let mut bytes = label.as_bytes().to_vec();
    bytes.push(0);
    let ns_alloc = send(cls(b"NSString\0"), sel(b"alloc\0"));
    let string = send1(
        ns_alloc,
        sel(b"initWithUTF8String:\0"),
        bytes.as_ptr() as *mut c_void,
    );
    if !string.is_null() {
        send1(layer, sel(b"setString:\0"), string);
        // setString: retains; release our +1.
        send1(string, sel(b"release\0"), std::ptr::null_mut());
    }
    send1(
        layer,
        sel(b"setForegroundColor:\0"),
        cg_color(badge_white(BADGE_LABEL_ALPHA)),
    );
    send1(
        layer,
        sel(b"setAlignmentMode:\0"),
        ns_string_autoreleased("center"),
    );
    set_f64(layer, b"setFontSize:\0", BADGE_FONT_SIZE);
    set_f64(layer, b"setContentsScale:\0", scale);
    set_rect(
        layer,
        b"setFrame:\0",
        label_frame(frame.w, frame.h, BADGE_FONT_SIZE),
    );
    set_bool(layer, b"setHidden:\0", true);
    layer
}

/// Autoreleased NSString for the few selector-key strings needed here; the
/// main-queue autorelease pool drains it (same reasoning as the animation
/// factory in macos.rs).
#[cfg(target_os = "macos")]
unsafe fn ns_string_autoreleased(value: &str) -> *mut c_void {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    send1(
        cls(b"NSString\0"),
        sel(b"stringWithUTF8String:\0"),
        bytes.as_ptr() as *mut c_void,
    )
}

#[cfg(target_os = "macos")]
extern "C" fn badge_set_active(ctx: *mut c_void) {
    let payload = unsafe { Box::from_raw(ctx as *mut ActivePayload) };
    let corners = with_badge_state(|state| {
        state
            .screens
            .get(payload.screen_index)
            .map(|screen| screen.corners)
    });
    // Double unwrap: the mutex lock itself may fail (outer Option), and the
    // screen index may be out of range after a re-show with fewer screens.
    let Some(Some(corners)) = corners else {
        return;
    };
    unsafe {
        for (slot, layer) in corners.iter().enumerate() {
            let layer = *layer as usize as *mut c_void;
            if layer.is_null() {
                continue;
            }
            // Rebuild animation state on every transition: drop the previous
            // breathing animation first so a re-targeted corner starts a
            // fresh phase instead of keeping the stale loop. Both strings are
            // autoreleased (+0) — never hand-released; the main-queue pool
            // drains them.
            send2(
                layer,
                sel(b"removeAnimationForKey:\0"),
                ns_string_autoreleased("badge-breath"),
                ns_string_autoreleased("badge-breath"),
            );
            let active = payload.corner.map(|c| c.slot()) == Some(slot);
            // Brightness states are baked into the stroke color's alpha (see
            // make_bracket_layer); the breathing animation below drives only
            // the presentation layer of the active corner.
            let alpha = if active {
                BADGE_ACTIVE_ALPHA
            } else if payload.corner.is_some() {
                BADGE_DIM_ALPHA
            } else {
                BADGE_BRACKET_ALPHA
            };
            send1(
                layer,
                sel(b"setStrokeColor:\0"),
                cg_color(badge_white(alpha)),
            );
            if active {
                start_badge_breathing(layer);
            }
        }
    }
}

/// Attach the active-corner breathing animation via the
/// `animationWithKeyPath:` class factory — `alloc` + `initWithKeyPath:` was
/// removed from the macOS 27 runtime and crashes (see macos.rs
/// `start_border_breathing` for the verified pattern).
#[cfg(target_os = "macos")]
unsafe fn start_badge_breathing(layer: *mut c_void) {
    let key_path = ns_string_autoreleased("opacity");
    let anim = send1(
        cls(b"CABasicAnimation\0"),
        sel(b"animationWithKeyPath:\0"),
        key_path,
    );
    if anim.is_null() {
        return;
    }
    let from = ns_number_owned(BADGE_BREATH_MIN);
    let to = ns_number_owned(BADGE_BREATH_MAX);
    send1(anim, sel(b"setFromValue:\0"), from);
    send1(anim, sel(b"setToValue:\0"), to);
    send1(from, sel(b"release\0"), std::ptr::null_mut());
    send1(to, sel(b"release\0"), std::ptr::null_mut());
    set_f64(anim, b"setDuration:\0", BADGE_BREATH_SECONDS);
    set_bool(anim, b"setAutoreverses:\0", true);
    // CAMediaTiming.repeatCount: HUGE_VAL-style "forever".
    set_f64(anim, b"setRepeatCount:\0", 1e100);
    set_bool(anim, b"setRemovedOnCompletion:\0", false);
    send2(
        layer,
        sel(b"addAnimation:forKey:\0"),
        anim,
        ns_string_autoreleased("badge-breath"),
    );
}

/// Owned (+1) NSNumber for the animation endpoints; caller releases.
#[cfg(target_os = "macos")]
unsafe fn ns_number_owned(value: f64) -> *mut c_void {
    let alloc = send(cls(b"NSNumber\0"), sel(b"alloc\0"));
    let f: extern "C" fn(*mut c_void, *mut c_void, f64) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(alloc, sel(b"initWithDouble:\0"), value)
}

#[cfg(target_os = "macos")]
extern "C" fn badge_hide(_ctx: *mut c_void) {
    if let Some(screens) = with_badge_state(|state| std::mem::take(&mut state.screens)) {
        unsafe {
            for screen in &screens {
                send1(
                    screen.window as usize as *mut c_void,
                    sel(b"orderOut:\0"),
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public BadgeLayer (thread-safe façade; real windows on macOS, no-op
// elsewhere).
// ---------------------------------------------------------------------------

/// Owns the badge overlay lifecycle. `Drop` hides the windows so a wizard
/// cancel/finish or an app exit can never leave badges behind. All state
/// lives in the process-wide main-thread badge state, so the handle itself
/// carries nothing platform-specific.
pub struct BadgeLayer;

impl BadgeLayer {
    pub fn new() -> Self {
        BadgeLayer
    }

    /// Create one full-screen badge window per screen and bring them up.
    /// Replaces any previously shown badge set.
    pub fn show(&mut self, screens: &[BadgeScreen]) {
        if screens.is_empty() {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let payload = Box::into_raw(Box::new(ShowPayload {
                screens: screens.to_vec(),
            }));
            on_main(badge_show, payload as *mut c_void);
        }
        #[cfg(not(target_os = "macos"))]
        {
            // TODO(win32): mirror the halo overlay's Win32 path for badge
            // windows; until then the wizard runs with visual guidance from
            // the UI only.
            let _ = screens;
        }
    }

    /// Highlight the corner currently being captured: it brightens and
    /// breathes while the other three dim. Rebuilds the animation state on
    /// every transition. A no-op when no badge set is on screen (the
    /// main-queue handler tolerates a missing/short screen list).
    pub fn set_active(&mut self, screen_index: usize, corner: BadgeCorner) {
        #[cfg(target_os = "macos")]
        {
            let payload = Box::into_raw(Box::new(ActivePayload {
                screen_index,
                corner: Some(corner),
            }));
            on_main(badge_set_active, payload as *mut c_void);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (screen_index, corner);
        }
    }

    /// Back to uniform brackets (between screens / after the last corner).
    pub fn clear_active(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let payload = Box::into_raw(Box::new(ActivePayload {
                screen_index: usize::MAX,
                corner: None,
            }));
            on_main(badge_set_active, payload as *mut c_void);
        }
    }

    /// Order out every badge window and drop the state.
    pub fn hide(&mut self) {
        #[cfg(target_os = "macos")]
        on_main(badge_hide, std::ptr::null_mut());
        #[cfg(not(target_os = "macos"))]
        {}
    }
}

impl Default for BadgeLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BadgeLayer {
    fn drop(&mut self) {
        self.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(w: f64, h: f64) -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: w,
            height: h,
            scale: 1.0,
            native_x: 0.0,
            native_y: 0.0,
        }
    }

    // ------------------------------------------------------------------
    // bracket_geometry
    // ------------------------------------------------------------------

    #[test]
    fn brackets_hug_the_four_corners_in_layer_space() {
        let [tl, tr, br, bl] = bracket_geometry(1920.0, 1080.0, BADGE_INSET, BADGE_ARM);
        assert_eq!(tl.corner, (90.0, 990.0), "top-left in bottom-left coords");
        assert_eq!(tr.corner, (1830.0, 990.0));
        assert_eq!(br.corner, (1830.0, 90.0));
        assert_eq!(bl.corner, (90.0, 90.0));
    }

    #[test]
    fn bracket_arms_point_inward_along_the_edges() {
        let [tl, tr, br, bl] = bracket_geometry(1000.0, 800.0, BADGE_INSET, BADGE_ARM);
        // TL: horizontal grows +x, vertical shrinks y.
        assert_eq!(tl.horizontal, (90.0 + BADGE_ARM, 710.0));
        assert_eq!(tl.vertical, (90.0, 710.0 - BADGE_ARM));
        // TR: horizontal shrinks x, vertical shrinks y.
        assert_eq!(tr.horizontal, (910.0 - BADGE_ARM, 710.0));
        assert_eq!(tr.vertical, (910.0, 710.0 - BADGE_ARM));
        // BR: horizontal shrinks x, vertical grows y.
        assert_eq!(br.horizontal, (910.0 - BADGE_ARM, 90.0));
        assert_eq!(br.vertical, (910.0, 90.0 + BADGE_ARM));
        // BL: horizontal grows x, vertical grows y.
        assert_eq!(bl.horizontal, (90.0 + BADGE_ARM, 90.0));
        assert_eq!(bl.vertical, (90.0, 90.0 + BADGE_ARM));
    }

    #[test]
    fn bracket_geometry_clamps_degenerate_screens() {
        let [tl, _, _, bl] = bracket_geometry(10.0, 0.0, BADGE_INSET, BADGE_ARM);
        // Zero-height screen: top/bottom collapse to a sane, ordered pair
        // instead of producing negative coordinates.
        assert_eq!(tl.corner.1, bl.corner.1 + 1.0);
        assert!(tl.corner.1 > bl.corner.1);
    }

    // ------------------------------------------------------------------
    // label_frame
    // ------------------------------------------------------------------

    #[test]
    #[cfg(target_os = "macos")]
    fn label_frame_centers_one_line_box_on_the_screen() {
        let frame = label_frame(1920.0, 1080.0, BADGE_FONT_SIZE);
        let line = BADGE_FONT_SIZE * BADGE_LINE_HEIGHT_FACTOR;
        assert!((frame.x + frame.w / 2.0 - 960.0).abs() < 1e-9);
        assert!((frame.y + frame.h / 2.0 - 540.0).abs() < 1e-9);
        assert_eq!(frame.h, line);
    }

    // ------------------------------------------------------------------
    // badge_ns_frame
    // ------------------------------------------------------------------

    #[test]
    #[cfg(target_os = "macos")]
    fn ns_frame_flips_y_and_keeps_the_full_screen_rect() {
        let g = OverlayScreenGeometry {
            x: 1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            scale: 1.0,
            native_x: 1920.0,
            native_y: 0.0,
        };
        let rect = badge_ns_frame(&g, 2160.0);
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (1920.0, 2160.0 - 1080.0, 1920.0, 1080.0)
        );
        // Secondary screen stacked below the primary in CG space.
        let below = OverlayScreenGeometry {
            native_y: 1080.0,
            ..screen(1000.0, 700.0)
        };
        let rect = badge_ns_frame(&below, 2160.0);
        assert_eq!(rect.y, 2160.0 - 1080.0 - 700.0);
    }

    // ------------------------------------------------------------------
    // BadgeCorner slot order matches ScreenRegion field order
    // ------------------------------------------------------------------

    #[test]
    fn corner_slots_are_in_region_field_order() {
        assert_eq!(BadgeCorner::Tl.slot(), 0);
        assert_eq!(BadgeCorner::Tr.slot(), 1);
        assert_eq!(BadgeCorner::Br.slot(), 2);
        assert_eq!(BadgeCorner::Bl.slot(), 3);
    }
}
