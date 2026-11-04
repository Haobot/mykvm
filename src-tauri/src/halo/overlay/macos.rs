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
struct NsPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct NsRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
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

// NSWindow / layer pointers (as usize), created lazily on the main thread.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static RING_LAYER: AtomicUsize = AtomicUsize::new(0);
static ARC_LAYER: AtomicUsize = AtomicUsize::new(0);
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
    fn CGColorCreateGenericRGB(red: f64, green: f64, blue: f64, alpha: f64) -> *mut c_void;
}

/// Raw CGColorRef (r, g, b, a tuple). setStrokeColor: is a strong property,
/// so the returned +1 reference is handed over to the layer, not released.
fn cg_color(color: (f64, f64, f64, f64)) -> *mut c_void {
    unsafe { CGColorCreateGenericRGB(color.0, color.1, color.2, color.3) }
}

unsafe fn cls(name: &[u8]) -> *mut c_void {
    objc_getClass(name.as_ptr() as *const c_char)
}
unsafe fn sel(name: &[u8]) -> *mut c_void {
    sel_registerName(name.as_ptr() as *const c_char)
}
unsafe fn send(obj: *mut c_void, s: *mut c_void) -> *mut c_void {
    let f: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, s)
}
unsafe fn send1(obj: *mut c_void, s: *mut c_void, a: *mut c_void) -> *mut c_void {
    let f: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
unsafe fn send_f64(obj: *mut c_void, selector: &[u8]) -> f64 {
    let f: extern "C" fn(*mut c_void, *mut c_void) -> f64 =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector))
}
unsafe fn set_bool(obj: *mut c_void, selector: &[u8], value: bool) {
    let f: extern "C" fn(*mut c_void, *mut c_void, bool) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector), value);
}
// CGFloat-sized setters: setLineWidth:, setStrokeEnd:, setContentsScale:,
// setAlphaValue: …
unsafe fn set_f64(obj: *mut c_void, selector: &[u8], value: f64) {
    let f: extern "C" fn(*mut c_void, *mut c_void, f64) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel(selector), value);
}
unsafe fn set_rect(obj: *mut c_void, selector: &[u8], rect: NsRect) {
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

fn main_queue() -> *mut c_void {
    unsafe { &_dispatch_main_q as *const c_void as *mut c_void }
}

fn on_main(work: extern "C" fn(*mut c_void), ctx: *mut c_void) {
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

/// Ellipse path inset so the stroke never clips at the window edge. Returns a
/// raw +1 CGPathRef; the caller releases it after handing it to a layer.
unsafe fn ellipse_path(rect: CGRect) -> *mut c_void {
    CGPathCreateWithEllipseInRect(rect, std::ptr::null())
}

pub struct MacOSOverlay {
    diameter: f64,
    /// True once creation has been dispatched (window stays alive afterwards).
    active: bool,
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
}

impl Drop for MacOSOverlay {
    fn drop(&mut self) {
        // The NSWindow object itself is kept (like the drag overlay) so a
        // later show can reuse it; only visibility is dropped.
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

unsafe fn build_window(diameter: f64) -> *mut c_void {
    let rect = NsRect {
        x: 0.0,
        y: 0.0,
        w: diameter,
        h: diameter,
    };
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
        return std::ptr::null_mut();
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

    // Layer-backed content view hosting the ring and progress-arc sublayers.
    let view_alloc = send(cls(b"NSView\0"), sel(b"alloc\0"));
    let view_init: extern "C" fn(*mut c_void, *mut c_void, NsRect) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    let view = view_init(view_alloc, sel(b"initWithFrame:\0"), rect);
    if view.is_null() {
        return window;
    }
    set_bool(view, b"setWantsLayer:\0", true);
    send1(window, sel(b"setContentView:\0"), view);
    let host = send(view, sel(b"layer\0"));
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
}
