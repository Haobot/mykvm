//! FocusHalo halo overlay for macOS: a borderless, click-through NSWindow
//! hosting two CAShapeLayers (the ring and the dwell progress arc).
//!
//! The raw objc_msgSend / dispatch_async_f pattern is cloned from
//! `input.rs`'s `macos_drag_overlay` so the codebase keeps one battle-tested
//! style instead of pulling in a Cocoa bindings crate. As there, the window
//! is created lazily on the main queue and kept alive after `orderOut:` so a
//! later show reuses it. Single overlay per process (like the drag overlay):
//! the statics below are only safe because HaloRuntime owns one instance.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use core_foundation::base::CFRelease;
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};

use crate::halo::overlay::{HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry};
use crate::halo::params::FocusHaloSettings;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NsPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NsRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

// NSMainMenuWindowLevel-ish; above normal windows (same anchor as the drag
// overlay's macos_drag_overlay).
const WINDOW_LEVEL: i64 = 25;

// The ring stroke and the gap keeping it inside the window bounds (points).
const RING_WIDTH: f64 = 6.0;
const MARGIN: f64 = 8.0;

// Ring colors (r, g, b, a) — keep numerically in sync with the packed BGRA
// constants in win32.rs.
const NEUTRAL: (f64, f64, f64, f64) = (0.62, 0.66, 0.72, 0.85);
const NEUTRAL_SUSPENDED: (f64, f64, f64, f64) = (0.62, 0.66, 0.72, 0.30);
const PULSE: (f64, f64, f64, f64) = (0.20, 0.80, 0.95, 1.0);
const ACCENT: (f64, f64, f64, f64) = (0.20, 0.80, 0.95, 0.95);

// Candidate-window border: the window pads the board frame by this much on
// every side so the 3pt stroke lands fully inside; the outline hugs the
// original frame with rounded corners.
const BORDER_PAD: f64 = 6.0;
const BORDER_STROKE: f64 = 3.0;
const BORDER_CORNER_RADIUS: f64 = 12.0;
// Breathing opacity range and period (s); Core Animation modulates the
// layer's presentation opacity, no per-frame driving from this process.
const BORDER_OPACITY_MIN: f64 = 0.35;
const BORDER_OPACITY_MAX: f64 = 0.9;
const BORDER_BREATH_SECONDS: f64 = 1.2;

// NSWindow / layer pointers (as usize), created lazily on the main thread.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static RING_LAYER: AtomicUsize = AtomicUsize::new(0);
static ARC_LAYER: AtomicUsize = AtomicUsize::new(0);
// Border window and its stroke layer (same lazy main-thread pattern).
static BORDER_WINDOW: AtomicUsize = AtomicUsize::new(0);
static BORDER_LAYER: AtomicUsize = AtomicUsize::new(0);
// Diameter the existing window was built with (f64::to_bits; 0 = none yet).
// When a later configure() changes halo_diameter the same window is resized
// instead of rebuilt, so this is what makes hot diameter changes take effect.
static BUILT_DIAMETER: AtomicUsize = AtomicUsize::new(0);
// Whether the progress arc is wanted at all (settings.show_progress), set at
// construction because the main-thread frame callback cannot capture state.
static ARC_ENABLED: AtomicBool = AtomicBool::new(false);

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}
#[link(name = "System")]
extern "C" {
    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}
extern "C" {
    static _dispatch_main_q: c_void;
}

// CGPathCreateWithEllipseInRect lives in CoreGraphics, which the binary
// already links via AppKit; core-graphics 0.25 has no ellipse constructor of
// its own (only from_rect) and its CGColor::rgb is CGColorCreateGenericRGB,
// so declare the C entries directly. CoreFoundation is a direct dependency,
// which gives us CFRelease for balancing the +1 references.
extern "C" {
    fn CGPathCreateWithEllipseInRect(
        rect: CGRect,
        transform: *const CGAffineTransform,
    ) -> *mut c_void;
    fn CGPathCreateMutable() -> *mut c_void;
    fn CGPathMoveToPoint(path: *mut c_void, transform: *const CGAffineTransform, x: f64, y: f64);
    fn CGPathAddLineToPoint(path: *mut c_void, transform: *const CGAffineTransform, x: f64, y: f64);
    fn CGPathCloseSubpath(path: *mut c_void);
    fn CGPathAddRoundedRect(
        path: *mut c_void,
        transform: *const CGAffineTransform,
        rect: CGRect,
        corner_width: f64,
        corner_height: f64,
    );
    fn CGColorCreateGenericRGB(red: f64, green: f64, blue: f64, alpha: f64) -> *mut c_void;
}

/// Raw CGColorRef (r, g, b, a tuple). setStrokeColor: is a strong property,
/// so the returned +1 reference is handed over to the layer, not released.
pub(crate) fn cg_color(color: (f64, f64, f64, f64)) -> *mut c_void {
    unsafe { CGColorCreateGenericRGB(color.0, color.1, color.2, color.3) }
}

// Shared low-level objc helpers: also used by the sibling badge overlay
// (badge.rs) so the codebase keeps exactly one raw-msgSend style per file
// cluster.
pub(crate) unsafe fn cls(name: &[u8]) -> *mut c_void {
    objc_getClass(name.as_ptr() as *const c_char)
}
pub(crate) unsafe fn sel(name: &[u8]) -> *mut c_void {
    sel_registerName(name.as_ptr() as *const c_char)
}
pub(crate) unsafe fn send(obj: *mut c_void, s: *mut c_void) -> *mut c_void {
    let f: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, s)
}
pub(crate) unsafe fn send1(obj: *mut c_void, s: *mut c_void, a: *mut c_void) -> *mut c_void {
    let f: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
pub(crate) unsafe fn send2(
    obj: *mut c_void,
    s: *mut c_void,
    a: *mut c_void,
    b: *mut c_void,
) -> *mut c_void {
    let f: extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, s, a, b)
}
pub(crate) unsafe fn send_f64(obj: *mut c_void, selector: &[u8]) -> f64 {
    let f: extern "C" fn(*mut c_void, *mut c_void) -> f64 =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector))
}
pub(crate) unsafe fn set_bool(obj: *mut c_void, selector: &[u8], value: bool) {
    let f: extern "C" fn(*mut c_void, *mut c_void, bool) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector), value);
}
// CGFloat-sized setters: setLineWidth:, setStrokeEnd:, setContentsScale:,
// setAlphaValue: …
pub(crate) unsafe fn set_f64(obj: *mut c_void, selector: &[u8], value: f64) {
    let f: extern "C" fn(*mut c_void, *mut c_void, f64) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector), value);
}
pub(crate) unsafe fn set_rect(obj: *mut c_void, selector: &[u8], rect: NsRect) {
    let f: extern "C" fn(*mut c_void, *mut c_void, NsRect) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector), rect);
}
// NSWindow setFrame:display: (NSRect, BOOL).
unsafe fn set_frame_display(obj: *mut c_void, rect: NsRect, display: bool) {
    let f: extern "C" fn(*mut c_void, *mut c_void, NsRect, bool) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(b"setFrame:display:\0"), rect, display);
}

pub(crate) fn main_queue() -> *mut c_void {
    unsafe { &_dispatch_main_q as *const c_void as *mut c_void }
}

pub(crate) fn on_main(work: extern "C" fn(*mut c_void), ctx: *mut c_void) {
    unsafe { dispatch_async_f(main_queue(), ctx, work) }
}

/// CG global points (top-left origin) → NS window origin (bottom-left,
/// points), centered on the halo. Same flip anchor as `macos_drag_overlay`:
/// `ns_y = primary_height - cg_y - size`.
fn ns_window_origin(
    geometry: &OverlayScreenGeometry,
    local: (f64, f64),
    diameter: f64,
    primary_height: f64,
) -> (f64, f64) {
    let cg_x = geometry.native_x + local.0;
    let cg_y = geometry.native_y + local.1;
    (
        cg_x - diameter / 2.0,
        primary_height - cg_y - diameter / 2.0,
    )
}

/// Board-space border frame → the border window's NS frame (bottom-left
/// origin), padded `pad` points beyond the board rectangle on every side so
/// the stroke lands inside the window. Pure so the flip math stays testable.
fn border_ns_frame(
    geometry: &OverlayScreenGeometry,
    border: &crate::halo::overlay::BorderFrame,
    pad: f64,
    primary_height: f64,
) -> NsRect {
    // Board → screen-local → CG global (identity on single-board macs).
    let cg_x = geometry.native_x + (border.x - geometry.x);
    let cg_y = geometry.native_y + (border.y - geometry.y);
    // The CG top-left of the padded window is (cg_x - pad, cg_y - pad) and
    // its height is h + 2*pad; NS flips y around the primary display height.
    NsRect {
        x: cg_x - pad,
        y: primary_height - cg_y - border.h - pad,
        w: border.w + pad * 2.0,
        h: border.h + pad * 2.0,
    }
}

/// Ellipse path inset so the stroke never clips at the window edge. Returns a
/// raw +1 CGPathRef; the caller releases it after handing it to a layer.
unsafe fn ellipse_path(rect: CGRect) -> *mut c_void {
    CGPathCreateWithEllipseInRect(rect, std::ptr::null())
}

pub struct MacOSOverlay {
    diameter: f64,
    /// True once creation has been dispatched (window stays alive afterwards).
    active: bool,
    /// Whether a border is currently wanted (mirrors the last set_border).
    border_visible: bool,
    geometry: Option<OverlayScreenGeometry>,
    /// Cached `CGDisplay::main()` height for the y flip; refreshed on
    /// set_screen because the primary display can change.
    primary_height: Option<f64>,
}

impl MacOSOverlay {
    pub fn new(settings: &FocusHaloSettings) -> Self {
        let settings = settings.sanitized();
        ARC_ENABLED.store(settings.show_progress, Ordering::Relaxed);
        MacOSOverlay {
            diameter: settings.halo_diameter,
            active: false,
            border_visible: false,
            geometry: None,
            primary_height: None,
        }
    }

    fn ensure_window(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        on_main(
            create_and_show,
            Box::into_raw(Box::new(self.diameter)) as *mut c_void,
        );
    }

    fn primary_height(&mut self) -> f64 {
        if let Some(height) = self.primary_height {
            return height;
        }
        let height = CGDisplay::main().bounds().size.height;
        self.primary_height = Some(height);
        height
    }
}

impl OverlayBackend for MacOSOverlay {
    fn show(&mut self) {
        self.ensure_window();
    }

    fn hide(&mut self) {
        // The border window hides too: one overlay lifecycle owns both
        // windows (each stays alive for process-wide reuse).
        self.border_visible = false;
        on_main(hide_border, std::ptr::null_mut());
        if !self.active {
            return;
        }
        self.active = false;
        on_main(order_out, std::ptr::null_mut());
    }

    fn set_screen(&mut self, geometry: OverlayScreenGeometry) {
        self.geometry = Some(geometry);
        self.primary_height = None;
    }

    fn update(&mut self, local: (f64, f64), frame: &HaloFrame) {
        self.ensure_window();
        let Some(geometry) = self.geometry else {
            return;
        };
        let height = self.primary_height();
        let (x, y) = ns_window_origin(&geometry, local, self.diameter, height);
        // Main queue keeps FIFO order, so creation dispatched above (first
        // update) always precedes this frame application.
        on_main(
            move_window,
            Box::into_raw(Box::new(NsPoint { x, y })) as *mut c_void,
        );
        on_main(apply_frame, Box::into_raw(Box::new(*frame)) as *mut c_void);
    }

    fn set_border(&mut self, border: Option<crate::halo::overlay::BorderFrame>) {
        let Some(border) = border else {
            self.border_visible = false;
            on_main(hide_border, std::ptr::null_mut());
            return;
        };
        let Some(geometry) = self.geometry else {
            return;
        };
        self.border_visible = true;
        let height = self.primary_height();
        let rect = border_ns_frame(&geometry, &border, BORDER_PAD, height);
        // Same lazy main-thread pattern as the halo window: creation and the
        // frame application stay ordered on the main queue.
        on_main(show_border, Box::into_raw(Box::new(rect)) as *mut c_void);
        // macOS 27 compositing quirk (verified live 2026-11-05, see the
        // badge.rs module docs): a borderless non-opaque window whose
        // sublayer was un-hidden inside the creation block never composites
        // until a later frame touch — the halo ring survives only because
        // the engine moves it every tick. A follow-up block re-asserting the
        // frame with display:YES forces the window-server surface update.
        on_main(border_reveal, Box::into_raw(Box::new(rect)) as *mut c_void);
    }

    fn border_visible(&self) -> bool {
        self.border_visible
    }
}

impl Drop for MacOSOverlay {
    fn drop(&mut self) {
        // The NSWindow objects themselves are kept (like the drag overlay) so
        // a later show can reuse them; only visibility is dropped.
        self.hide();
    }
}

/// Point the existing layers at a new diameter: the window and its sublayers
/// get new frames, and each ring gets a fresh inset path (sublayers do not
/// auto-resize with the window). `setPath:` is a strong property, so it
/// releases the old path itself; we only release our own +1 on the new one.
unsafe fn resize_window(window: *mut c_void, diameter: f64) {
    let rect = NsRect {
        x: 0.0,
        y: 0.0,
        w: diameter,
        h: diameter,
    };
    set_frame_display(window, rect, false);
    let inset = CGRect {
        origin: CGPoint {
            x: MARGIN,
            y: MARGIN,
        },
        size: CGSize {
            width: rect.w - MARGIN * 2.0,
            height: rect.h - MARGIN * 2.0,
        },
    };
    for layer in [
        RING_LAYER.load(Ordering::Relaxed),
        ARC_LAYER.load(Ordering::Relaxed),
    ] {
        let layer = layer as *mut c_void;
        if layer.is_null() {
            continue;
        }
        set_rect(layer, b"setFrame:\0", rect);
        let path = ellipse_path(inset);
        send1(layer, sel(b"setPath:\0"), path);
        CFRelease(path);
    }
}

extern "C" fn create_and_show(ctx: *mut c_void) {
    let diameter = unsafe { Box::from_raw(ctx as *mut f64) };
    unsafe {
        let mut window = WINDOW.load(Ordering::Relaxed) as *mut c_void;
        let built = f64::from_bits(BUILT_DIAMETER.load(Ordering::Relaxed) as u64);
        if window.is_null() {
            window = build_window(*diameter);
            if window.is_null() {
                return;
            }
            WINDOW.store(window as usize, Ordering::Relaxed);
            BUILT_DIAMETER.store(diameter.to_bits() as usize, Ordering::Relaxed);
        } else if (built - *diameter).abs() > f64::EPSILON {
            resize_window(window, *diameter);
            BUILT_DIAMETER.store(diameter.to_bits() as usize, Ordering::Relaxed);
        }
        send(window, sel(b"orderFrontRegardless\0"));
    }
}

extern "C" fn order_out(_ctx: *mut c_void) {
    let window = WINDOW.load(Ordering::Relaxed) as *mut c_void;
    if !window.is_null() {
        unsafe {
            send1(window, sel(b"orderOut:\0"), std::ptr::null_mut());
        }
    }
}

extern "C" fn move_window(ctx: *mut c_void) {
    let origin = unsafe { Box::from_raw(ctx as *mut NsPoint) };
    let window = WINDOW.load(Ordering::Relaxed) as *mut c_void;
    if window.is_null() {
        return;
    }
    unsafe {
        let f: extern "C" fn(*mut c_void, *mut c_void, NsPoint) =
            std::mem::transmute(objc_msgSend as *const ());
        f(window, sel(b"setFrameOrigin:\0"), *origin);
    }
}

extern "C" fn apply_frame(ctx: *mut c_void) {
    let frame = unsafe { Box::from_raw(ctx as *mut HaloFrame) };
    let ring = RING_LAYER.load(Ordering::Relaxed) as *mut c_void;
    let arc = ARC_LAYER.load(Ordering::Relaxed) as *mut c_void;
    let window = WINDOW.load(Ordering::Relaxed) as *mut c_void;
    if ring.is_null() {
        return;
    }
    unsafe {
        let (r, g, b, a) = match frame.visual {
            HaloVisual::SwitchPulse => PULSE,
            HaloVisual::Suspended => NEUTRAL_SUSPENDED,
            HaloVisual::Idle | HaloVisual::Candidate { .. } => NEUTRAL,
        };
        // setStrokeColor: is a strong (retaining) property, so hand over the
        // +1 reference and never release it here.
        let color = cg_color((r, g, b, a));
        send1(ring, sel(b"setStrokeColor:\0"), color);
        set_f64(ring, b"setLineWidth:\0", RING_WIDTH);
        set_bool(ring, b"setHidden:\0", false);

        let arc_visible = matches!(frame.visual, HaloVisual::Candidate { .. })
            && ARC_ENABLED.load(Ordering::Relaxed);
        if !arc.is_null() {
            if arc_visible {
                if let HaloVisual::Candidate { progress } = frame.visual {
                    let accent = cg_color(ACCENT);
                    send1(arc, sel(b"setStrokeColor:\0"), accent);
                    set_f64(arc, b"setStrokeStart:\0", 0.0);
                    set_f64(arc, b"setStrokeEnd:\0", progress.clamp(0.0, 1.0) as f64);
                    set_f64(arc, b"setLineWidth:\0", RING_WIDTH);
                }
            }
            set_bool(arc, b"setHidden:\0", !arc_visible);
        }
        if !window.is_null() {
            set_f64(window, b"setAlphaValue:\0", frame.opacity.clamp(0.0, 1.0));
        }
    }
}

/// Shared NSWindow boilerplate for the halo, border and badge overlay
/// windows: borderless, transparent, shadowless, click-through, level-25,
/// all-spaces window with a layer-backed content view. Returns (window, host
/// layer); the host is null when the view could not be created (callers
/// degrade to a plain window).
pub(crate) unsafe fn make_click_through_window(rect: NsRect) -> (*mut c_void, *mut c_void) {
    let alloc = send(cls(b"NSWindow\0"), sel(b"alloc\0"));
    // initWithContentRect:styleMask:backing:defer: (borderless=0, buffered=2)
    let init: extern "C" fn(*mut c_void, *mut c_void, NsRect, u64, u64, bool) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    let window = init(
        alloc,
        sel(b"initWithContentRect:styleMask:backing:defer:\0"),
        rect,
        0,
        2,
        false,
    );
    if window.is_null() {
        return (std::ptr::null_mut(), std::ptr::null_mut());
    }

    // Transparent, shadowless, click-through, top-most, all-spaces.
    let clear = send(cls(b"NSColor\0"), sel(b"clearColor\0"));
    send1(window, sel(b"setBackgroundColor:\0"), clear);
    set_bool(window, b"setOpaque:\0", false);
    set_bool(window, b"setHasShadow:\0", false);
    set_bool(window, b"setIgnoresMouseEvents:\0", true);
    let set_level: extern "C" fn(*mut c_void, *mut c_void, i64) =
        std::mem::transmute(objc_msgSend as *const ());
    set_level(window, sel(b"setLevel:\0"), WINDOW_LEVEL);
    // canJoinAllSpaces(1) | stationary(16)
    let set_behavior: extern "C" fn(*mut c_void, *mut c_void, u64) =
        std::mem::transmute(objc_msgSend as *const ());
    set_behavior(window, sel(b"setCollectionBehavior:\0"), 1 | 16);

    // Layer-backed content view.
    let view_alloc = send(cls(b"NSView\0"), sel(b"alloc\0"));
    let view_init: extern "C" fn(*mut c_void, *mut c_void, NsRect) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    let view = view_init(view_alloc, sel(b"initWithFrame:\0"), rect);
    if view.is_null() {
        return (window, std::ptr::null_mut());
    }
    set_bool(view, b"setWantsLayer:\0", true);
    send1(window, sel(b"setContentView:\0"), view);
    let host = send(view, sel(b"layer\0"));
    (window, host)
}

unsafe fn build_window(diameter: f64) -> *mut c_void {
    let rect = NsRect {
        x: 0.0,
        y: 0.0,
        w: diameter,
        h: diameter,
    };
    let (window, host) = make_click_through_window(rect);
    if window.is_null() {
        return std::ptr::null_mut();
    }
    if host.is_null() {
        return window;
    }
    let scale = send_f64(window, b"backingScaleFactor\0");
    let ring = make_shape_layer(rect, scale);
    let arc = make_shape_layer(rect, scale);
    if !ring.is_null() {
        send1(host, sel(b"addSublayer:\0"), ring);
        RING_LAYER.store(ring as usize, Ordering::Relaxed);
    }
    if !arc.is_null() {
        send1(host, sel(b"addSublayer:\0"), arc);
        ARC_LAYER.store(arc as usize, Ordering::Relaxed);
    }
    window
}

// ---------------------------------------------------------------------------
// Candidate-window border: a second click-through window hosting one
// CAShapeLayer stroked rounded rect. The breathing (opacity 0.35↔0.9 over a
// 1.2 s autoreverse loop) runs entirely in Core Animation on the GPU
// compositor, so nothing here drives it per frame; set_border only
// moves/resizes the window and refreshes the outline path.
// ---------------------------------------------------------------------------

/// Rounded-rect outline path for the border, positioned ABSOLUTELY inside
/// the full-screen border window (see `build_border_window`): `rect` is the
/// target frame in main-display NS coordinates, padded by `BORDER_PAD` so
/// the centered stroke stays inside. Built with `CGPathAddRoundedRect` —
/// `CGPathCreateWithRoundedCGRect` is no longer exported by the macOS 27
/// SDK. Returns a raw +1 CGMutablePathRef; the caller releases it after
/// handing it to a layer.
unsafe fn border_path(rect: NsRect) -> *mut c_void {
    let mutable = CGPathCreateMutable();
    if mutable.is_null() {
        return std::ptr::null_mut();
    }
    // Sharp corners via explicit MoveTo/AddLine segments (the same
    // primitives badge.rs brackets use — verified live against the macOS 27
    // runtime; keep the path API surface minimal).
    let x0 = rect.x + BORDER_PAD;
    let y0 = rect.y + BORDER_PAD;
    let x1 = rect.x + rect.w - BORDER_PAD;
    let y1 = rect.y + rect.h - BORDER_PAD;
    CGPathMoveToPoint(mutable, std::ptr::null(), x0, y0);
    CGPathAddLineToPoint(mutable, std::ptr::null(), x1, y0);
    CGPathAddLineToPoint(mutable, std::ptr::null(), x1, y1);
    CGPathAddLineToPoint(mutable, std::ptr::null(), x0, y1);
    CGPathCloseSubpath(mutable);
    mutable
}

/// The border window spans the whole MAIN display (origin-anchored, like the
/// badge windows) instead of hugging the candidate frame. Two reasons,
/// both verified live on macOS 27 (2026-11-05):
///
/// 1. Origin-anchored full-coverage overlay windows composite reliably; a
///    borderless window born at an arbitrary position never got its layer
///    geometry picked up, which kept the candidate border pixel-invisible
///    through several reveal-pattern attempts.
/// 2. The outline path carries the position instead, so "moving" the border
///    between candidates is a single setPath: — no window resize churn.
unsafe fn border_window_frame() -> NsRect {
    let bounds = CGDisplay::main().bounds();
    NsRect {
        x: bounds.origin.x,
        y: bounds.origin.y,
        w: bounds.size.width,
        h: bounds.size.height,
    }
}

unsafe fn build_border_window(target: NsRect) -> *mut c_void {
    let full = border_window_frame();
    let (window, host) = make_click_through_window(full);
    if window.is_null() {
        return std::ptr::null_mut();
    }
    if host.is_null() {
        return window;
    }
    // Build the layer from scratch (badge.rs's bracket construction), NOT
    // via make_shape_layer: that helper installs its ellipse as the initial
    // `path`, and REPLACING an animatable property (path) inside the
    // creation block leaves an implicit transition action stranded on a
    // never-composited layer — the outline never appears. The rect path
    // here is the layer's first and only path. Frame covers the whole
    // window at (0,0); the path carries the candidate's absolute position.
    let layer = send(
        send(cls(b"CAShapeLayer\0"), sel(b"alloc\0")),
        sel(b"init\0"),
    );
    if layer.is_null() {
        return window;
    }
    let path = border_path(target);
    send1(layer, sel(b"setPath:\0"), path);
    CFRelease(path);
    send1(
        layer,
        sel(b"setStrokeColor:\0"),
        cg_color((ACCENT.0, ACCENT.1, ACCENT.2, BORDER_OPACITY_MAX)),
    );
    send1(layer, sel(b"setFillColor:\0"), std::ptr::null_mut());
    set_f64(layer, b"setLineWidth:\0", BORDER_STROKE);
    set_f64(
        layer,
        b"setContentsScale:\0",
        send_f64(window, b"backingScaleFactor\0"),
    );
    set_rect(layer, b"setFrame:\0", full);
    set_bool(layer, b"setHidden:\0", true);
    send1(host, sel(b"addSublayer:\0"), layer);
    BORDER_LAYER.store(layer as usize, Ordering::Relaxed);
    window
}

/// Attach the breathing CABasicAnimation (opacity 0.35↔0.9, 1.2 s,
/// autoreverse, repeat forever). Called once at window creation; later
/// path/frame updates leave the animation untouched.
unsafe fn start_border_breathing(layer: *mut c_void) {
    let key_path = ns_string("opacity");
    // `alloc` + `initWithKeyPath:` was removed from the macOS 27 runtime
    // (verified live: instancesRespond = false, and the app crashed with
    // "-[CABasicAnimation initWithKeyPath:]: unrecognized selector"); the
    // supported creation path is the `animationWithKeyPath:` class factory.
    // It returns an autoreleased (+0) instance, so we never release `anim`
    // ourselves: the layer retains it inside addAnimation:forKey: and the
    // main-queue autorelease pool drains the remainder.
    let anim = send1(
        cls(b"CABasicAnimation\0"),
        sel(b"animationWithKeyPath:\0"),
        key_path,
    );
    send1(key_path, sel(b"release\0"), std::ptr::null_mut());
    if anim.is_null() {
        return;
    }
    let from = ns_number(BORDER_OPACITY_MIN);
    let to = ns_number(BORDER_OPACITY_MAX);
    send1(anim, sel(b"setFromValue:\0"), from);
    send1(anim, sel(b"setToValue:\0"), to);
    send1(from, sel(b"release\0"), std::ptr::null_mut());
    send1(to, sel(b"release\0"), std::ptr::null_mut());
    set_f64(anim, b"setDuration:\0", BORDER_BREATH_SECONDS);
    set_bool(anim, b"setAutoreverses:\0", true);
    // CAMediaTiming.repeatCount: HUGE_VAL-style "forever".
    set_f64(anim, b"setRepeatCount:\0", 1e100);
    set_bool(anim, b"setRemovedOnCompletion:\0", false);
    send2(
        layer,
        sel(b"addAnimation:forKey:\0"),
        anim,
        std::ptr::null_mut(),
    );
}

/// NSString for the few selector-key strings needed here. Uses alloc/initWith
/// (not the autoreleased class factories) so the marshaled callback needs no
/// autorelease pool: the caller owns the +1 object and releases it.
unsafe fn ns_string(value: &str) -> *mut c_void {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    let alloc = send(cls(b"NSString\0"), sel(b"alloc\0"));
    send1(
        alloc,
        sel(b"initWithUTF8String:\0"),
        bytes.as_ptr() as *mut c_void,
    )
}

/// Boxed CGFloat for CABasicAnimation's from/to values; +1 owned, caller
/// releases.
unsafe fn ns_number(value: f64) -> *mut c_void {
    let alloc = send(cls(b"NSNumber\0"), sel(b"alloc\0"));
    let f: extern "C" fn(*mut c_void, *mut c_void, f64) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(alloc, sel(b"initWithDouble:\0"), value)
}

extern "C" fn show_border(ctx: *mut c_void) {
    let rect = unsafe { Box::from_raw(ctx as *mut NsRect) };
    let mut window = BORDER_WINDOW.load(Ordering::Relaxed) as *mut c_void;
    unsafe {
        if window.is_null() {
            window = build_border_window(*rect);
            if window.is_null() {
                return;
            }
            BORDER_WINDOW.store(window as usize, Ordering::Relaxed);
            // The breathing animation is deliberately NOT attached here: on
            // macOS 27 an animation added inside the creation block (to a
            // hidden, never-yet-composited layer) strands the presentation
            // — `border_reveal` attaches it once the layer is visible, the
            // same ordering badge.rs uses for its corner breathing.
        } else {
            // Reuse: the full-screen window never moves; only the outline
            // path carries the new candidate position.
            let layer = BORDER_LAYER.load(Ordering::Relaxed) as *mut c_void;
            if !layer.is_null() {
                let path = border_path(*rect);
                send1(layer, sel(b"setPath:\0"), path);
                CFRelease(path);
            }
        }
        send(window, sel(b"orderFrontRegardless\0"));
    }
}

/// Second main-queue block after [`show_border`]: re-assert the same frame
/// with display:YES and the layer visibility so the freshly created border
/// actually composites (macOS 27 quirk; the opacity breathing animation is
/// deliberately untouched — re-asserting layer opacity strands the
/// presentation, see badge.rs).
extern "C" fn border_reveal(ctx: *mut c_void) {
    let rect = unsafe { Box::from_raw(ctx as *mut NsRect) };
    let _ = rect; // target position lives in the layer path; window is full-screen
    let window = BORDER_WINDOW.load(Ordering::Relaxed) as *mut c_void;
    if window.is_null() {
        return;
    }
    unsafe {
        let layer = BORDER_LAYER.load(Ordering::Relaxed) as *mut c_void;
        if !layer.is_null() {
            // Mirror badge.rs's reveal writes exactly: re-asserting the
            // stroke properties in THIS block (not just hidden:false) is
            // what marks the layer for the window server on macOS 27.
            send1(
                layer,
                sel(b"setStrokeColor:\0"),
                cg_color((ACCENT.0, ACCENT.1, ACCENT.2, BORDER_OPACITY_MAX)),
            );
            send1(layer, sel(b"setFillColor:\0"), std::ptr::null_mut());
            set_f64(layer, b"setLineWidth:\0", BORDER_STROKE);
            set_bool(layer, b"setHidden:\0", false);
            // Attach the breathing only after the visible transition (same
            // ordering as badge.rs's corner breathing: an animation added to
            // a never-composited layer inside the creation block strands the
            // presentation on macOS 27).
            start_border_breathing(layer);
        }
        set_frame_display(window, border_window_frame(), true);
        set_f64(window, b"setAlphaValue:\0", 1.0);
    }
}

extern "C" fn hide_border(_ctx: *mut c_void) {
    let window = BORDER_WINDOW.load(Ordering::Relaxed) as *mut c_void;
    if !window.is_null() {
        unsafe {
            send1(window, sel(b"orderOut:\0"), std::ptr::null_mut());
        }
    }
}

unsafe fn make_shape_layer(rect: NsRect, scale: f64) -> *mut c_void {
    let alloc = send(cls(b"CAShapeLayer\0"), sel(b"alloc\0"));
    let layer = send(alloc, sel(b"init\0"));
    if layer.is_null() {
        return std::ptr::null_mut();
    }
    let inset = CGRect {
        origin: CGPoint {
            x: MARGIN,
            y: MARGIN,
        },
        size: CGSize {
            width: rect.w - MARGIN * 2.0,
            height: rect.h - MARGIN * 2.0,
        },
    };
    let path = ellipse_path(inset);
    // CAShapeLayer.path is a strong property: hand over the +1 reference.
    send1(layer, sel(b"setPath:\0"), path);
    CFRelease(path);
    // Stroke only: no fill keeps the halo interior fully transparent.
    send1(layer, sel(b"setFillColor:\0"), std::ptr::null_mut());
    set_f64(layer, b"setLineWidth:\0", RING_WIDTH);
    set_f64(layer, b"setContentsScale:\0", scale);
    set_bool(layer, b"setHidden:\0", true);
    set_rect(layer, b"setFrame:\0", rect);
    layer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(native_x: f64, native_y: f64) -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            scale: 1.0,
            native_x,
            native_y,
        }
    }

    #[test]
    fn ns_origin_flips_y_and_centers_on_the_halo() {
        let g = geometry(0.0, 0.0);
        let (x, y) = ns_window_origin(&g, (500.0, 400.0), 160.0, 1080.0);
        assert_eq!(x, 420.0);
        assert_eq!(y, 1080.0 - 400.0 - 80.0);
    }

    #[test]
    fn ns_origin_offsets_by_the_screen_native_origin() {
        let g = geometry(100.0, 200.0);
        let (x, y) = ns_window_origin(&g, (60.0, 40.0), 160.0, 1000.0);
        assert_eq!(x, 100.0 + 60.0 - 80.0);
        assert_eq!(y, 1000.0 - (200.0 + 40.0) - 80.0);
    }

    #[test]
    fn border_ns_frame_flips_y_and_pads_the_board_rect() {
        let g = geometry(0.0, 0.0);
        let border = crate::halo::overlay::BorderFrame {
            x: 100.0,
            y: 200.0,
            w: 400.0,
            h: 300.0,
        };
        let rect = border_ns_frame(&g, &border, 6.0, 1080.0);
        // Window top-left in CG: (94, 194); NS y = 1080 - 194 - 312 = 574.
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (94.0, 574.0, 412.0, 312.0)
        );
    }

    #[test]
    fn border_ns_frame_subtracts_the_screen_board_origin() {
        // Screen sits at board (50, 25) with native origin (100, 200).
        let g = OverlayScreenGeometry {
            x: 50.0,
            y: 25.0,
            ..geometry(100.0, 200.0)
        };
        let border = crate::halo::overlay::BorderFrame {
            x: 60.0,
            y: 40.0,
            w: 200.0,
            h: 100.0,
        };
        let rect = border_ns_frame(&g, &border, 6.0, 1000.0);
        // Board (60,40) → CG (100+10, 200+15) = (110, 215).
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (104.0, 679.0, 212.0, 112.0)
        );
    }
}
