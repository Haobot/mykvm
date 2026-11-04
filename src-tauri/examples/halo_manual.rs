//! Manual FocusHalo overlay harness: draws the native halo without running
//! the full app (no input capture, no discovery, no single-instance lock), so
//! the overlay itself can be verified on a live desktop.
//!
//! Usage:
//!   cargo run --example halo_manual -- [x y] [idle|candidate|pulse|suspended]
//!        [diameter] [resize_to resize_after_ms]
//!
//! Examples:
//!   halo_manual 756 491 idle 160            # static ring at screen center
//!   halo_manual 756 491 idle 160 180 4000   # hot-resize to 180 after 4s
//!   halo_manual 400 300 candidate 160       # dwell-progress arc
//!
//! The process just pumps the main run loop; Ctrl-C exits.

use mykvm_lib::halo::{FocusHaloSettings, HaloRuntime, HaloVisual, OverlayScreenGeometry};

#[cfg(target_os = "macos")]
fn main_display_geometry() -> Option<OverlayScreenGeometry> {
    let bounds = core_graphics::display::CGDisplay::main().bounds();
    Some(OverlayScreenGeometry {
        x: bounds.origin.x,
        y: bounds.origin.y,
        width: bounds.size.width,
        height: bounds.size.height,
        // macOS overlay math is all in points; scale stays identity.
        scale: 1.0,
        native_x: bounds.origin.x,
        native_y: bounds.origin.y,
    })
}

#[cfg(not(target_os = "macos"))]
fn main_display_geometry() -> Option<OverlayScreenGeometry> {
    // The P0 verification rig is macOS-only for now; Windows gets the same
    // harness when its overlay path gains a real-machine test pass.
    None
}

fn parse_visual(value: &str) -> HaloVisual {
    match value {
        "candidate" => HaloVisual::Candidate { progress: 0.5 },
        "pulse" => HaloVisual::SwitchPulse,
        "suspended" => HaloVisual::Suspended,
        _ => HaloVisual::Idle,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let x: f64 = args.iter().nth(1).and_then(|v| v.parse().ok()).unwrap_or(756.0);
    let y: f64 = args.iter().nth(2).and_then(|v| v.parse().ok()).unwrap_or(491.0);
    let visual = parse_visual(args.iter().nth(3).map(String::as_str).unwrap_or("idle"));
    let diameter: f64 = args.iter().nth(4).and_then(|v| v.parse().ok()).unwrap_or(160.0);
    let resize_to: Option<f64> = args.iter().nth(5).and_then(|v| v.parse().ok());
    let resize_after_ms: u64 = args.iter().nth(6).and_then(|v| v.parse().ok()).unwrap_or(4000);

    let Some(geometry) = main_display_geometry() else {
        eprintln!("no main display geometry on this platform");
        std::process::exit(2);
    };
    println!(
        "halo_manual: screen {}x{} at ({},{}), halo at ({},{}) visual={:?} diameter={} resize={:?} after {}ms",
        geometry.width,
        geometry.height,
        geometry.native_x,
        geometry.native_y,
        x,
        y,
        visual,
        diameter,
        resize_to,
        resize_after_ms
    );

    // AppKit insurance: instantiate NSApplication so a borderless NSWindow
    // created before any app UI still behaves (activation policy stays
    // accessory-like because we never activate).
    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }

    let mut settings = FocusHaloSettings::default();
    settings.halo_diameter = diameter;
    let mut runtime = HaloRuntime::new();
    runtime.configure(settings.clone());
    runtime.set_screen(geometry);
    runtime.show_test_halo((x, y), visual);

    if let Some(new_diameter) = resize_to {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(resize_after_ms));
            // HaloRuntime is !Send-safe only via this single owning thread; the
            // resize path is exercised through a second runtime instance that
            // reuses the same overlay window (statics make it a process-wide
            // singleton on macOS, mirroring the app's behavior).
            let mut next = FocusHaloSettings::default();
            next.halo_diameter = new_diameter;
            let mut runtime = HaloRuntime::new();
            runtime.configure(next);
            runtime.set_screen(geometry);
            runtime.show_test_halo((x, y), visual);
            // Keep this runtime alive for the process lifetime: dropping a
            // MacOSOverlay orders the window out, which would hide the halo
            // the moment this thread ends.
            std::mem::forget(runtime);
            println!("halo_manual: hot-resized to diameter {new_diameter}");
        });
    }

    // Drain the main queue so the overlay's dispatch_async_f blocks execute.
    #[cfg(target_os = "macos")]
    unsafe {
        run_main_loop();
    }
    #[cfg(not(target_os = "macos"))]
    {
        loop {
            std::thread::park();
        }
    }
}

#[cfg(target_os = "macos")]
mod appkit_shim {
    use std::ffi::c_void;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const i8) -> *mut c_void;
        fn sel_registerName(name: *const i8) -> *mut c_void;
        fn objc_msgSend();
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopRun();
    }

    unsafe fn cls(name: &[u8]) -> *mut c_void {
        objc_getClass(name.as_ptr() as *const i8)
    }
    unsafe fn sel(name: &[u8]) -> *mut c_void {
        sel_registerName(name.as_ptr() as *const i8)
    }

    pub unsafe fn init_nsapp() {
        let send0: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        // [NSApplication sharedApplication]
        send0(cls(b"NSApplication\0"), sel(b"sharedApplication\0"));
    }

    pub unsafe fn run_main_loop() {
        CFRunLoopRun()
    }
}

#[cfg(target_os = "macos")]
use appkit_shim::{init_nsapp, run_main_loop};
