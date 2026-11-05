//! Manual FocusHalo overlay harness: draws the native halo without running
//! the full app (no input capture, no discovery, no single-instance lock), so
//! the overlay itself can be verified on a live desktop.
//!
//! Usage:
//!   cargo run --example halo_manual -- [x y] [idle|candidate|pulse|suspended]
//!        [diameter] [resize_to resize_after_ms]
//!   cargo run --example halo_manual -- border x y w h [clear_after_ms]
//!   cargo run --example halo_manual -- clear
//!   cargo run --example halo_manual -- windows
//!   cargo run --example halo_manual -- activate <window_id>
//!   cargo run --example halo_manual -- focus-receive <kind> <screen_id> <x> <y>
//!   cargo run --example halo_manual -- badges [clear_after_ms]
//!
//! Examples:
//!   halo_manual 756 491 idle 160            # static ring at screen center
//!   halo_manual 756 491 idle 160 180 4000   # hot-resize to 180 after 4s
//!   halo_manual 400 300 candidate 160       # dwell-progress arc
//!   halo_manual border 200 150 800 500      # breathing border around a rect
//!   halo_manual border 200 150 800 500 6000 # ...auto-clear after 6 s
//!   halo_manual windows                     # P2 window enumeration
//!   halo_manual activate 12345              # P3 activate the window with
//!                                           # id from `windows` output
//!   halo_manual focus-receive candidate screen-0 700 300
//!                                           # P4: simulate a remote halo
//!                                           # event through the real
//!                                           # receive path (no second
//!                                           # machine needed)
//!   halo_manual badges 3000 [tl|tr|br|bl]   # S2: number badges on every
//!                                           # display (no camera needed),
//!                                           # auto-cleared after 3 s; the
//!                                           # optional corner highlights
//!                                           # screen 1's bracket set
//!
//! The process just pumps the main run loop; Ctrl-C exits.

use mykvm_lib::halo::{
    apply_focus_event, create_activator, create_window_service, BadgeCorner, BadgeLayer,
    BadgeScreen, BorderFrame, FocusHaloSettings, FocusScreen, HaloFocusEvent, HaloRuntime,
    HaloVisual, OverlayScreenGeometry,
};

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
    // Appended subcommands: `halo_manual windows` exercises the P2 window
    // layer (enumerate + hit-test cache); `border`/`clear` exercise the
    // candidate-window border overlay. The positional overlay harness below
    // is untouched.
    match args.iter().nth(1).map(String::as_str) {
        Some("windows") => {
            run_windows_subcommand();
            return;
        }
        Some("activate") => {
            run_activate_subcommand(&args);
            return;
        }
        Some("border") => {
            run_border_subcommand(&args);
            return;
        }
        Some("clear") => {
            run_border_clear();
            return;
        }
        Some("focus-receive") => {
            run_focus_receive_subcommand(&args);
            return;
        }
        Some("badges") => {
            run_badges_subcommand(&args);
            return;
        }
        _ => {}
    }
    let x: f64 = args
        .iter()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(756.0);
    let y: f64 = args
        .iter()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(491.0);
    let visual = parse_visual(args.iter().nth(3).map(String::as_str).unwrap_or("idle"));
    let diameter: f64 = args
        .iter()
        .nth(4)
        .and_then(|v| v.parse().ok())
        .unwrap_or(160.0);
    let resize_to: Option<f64> = args.iter().nth(5).and_then(|v| v.parse().ok());
    let resize_after_ms: u64 = args
        .iter()
        .nth(6)
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000);

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

// ---------------------------------------------------------------------------
// `halo_manual border x y w h [clear_after_ms]` (appended): candidate-window
// border verification. Draws the breathing accent border around the given
// board-logical rectangle (screen/global coordinates on the single-board
// manual rig) without the halo ring; the 1.2 s opacity breathing runs inside
// the backend (Core Animation on macOS, the 60 Hz tick on Windows).
// `clear` smoke-tests the set_border(None) path.
// ---------------------------------------------------------------------------

fn run_border_subcommand(args: &[String]) {
    let parse = |index: usize| -> f64 {
        args.get(index)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                eprintln!("usage: halo_manual border x y w h [clear_after_ms]");
                std::process::exit(2);
            })
    };
    let border = BorderFrame {
        x: parse(2),
        y: parse(3),
        w: parse(4),
        h: parse(5),
    };
    let clear_after_ms: Option<u64> = args.get(6).and_then(|value| value.parse().ok());

    let Some(geometry) = main_display_geometry() else {
        eprintln!("no main display geometry on this platform");
        std::process::exit(2);
    };
    println!(
        "halo_manual border: rect=({},{} {}x{}) clear_after={:?}",
        border.x, border.y, border.w, border.h, clear_after_ms
    );
    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }
    let mut runtime = HaloRuntime::new();
    runtime.set_screen(geometry);
    runtime.show_test_border(Some(border));

    if let Some(delay_ms) = clear_after_ms {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            // The clear path runs through a second runtime instance (the
            // border window is a process-wide static, mirroring the app's
            // single overlay), and the instance is kept alive so dropping it
            // cannot race the hide with anything else.
            let mut runtime = HaloRuntime::new();
            runtime.set_screen(geometry);
            runtime.show_test_border(None);
            std::mem::forget(runtime);
            println!("halo_manual border: cleared after {delay_ms} ms");
        });
    }

    // Keep pumping so the border stays on screen until Ctrl-C (or until the
    // optional clear fires).
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

/// `halo_manual clear`: smoke-test the border-clear path. A fresh process has
/// no border to hide, so this only verifies the None path is harmless; the
/// in-process variant is `border ... <clear_after_ms>`.
fn run_border_clear() {
    let Some(geometry) = main_display_geometry() else {
        eprintln!("no main display geometry on this platform");
        std::process::exit(2);
    };
    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }
    let mut runtime = HaloRuntime::new();
    runtime.set_screen(geometry);
    runtime.show_test_border(None);
    println!("halo_manual clear: set_border(None) dispatched");
}

// ---------------------------------------------------------------------------
// `halo_manual windows` (appended): P2 window-layer verification harness.
// Enumerates the visible windows through the halo window service and prints
// them z-order top to bottom, for eyeballing enumeration, titles, foreground
// marking and frames on a live desktop.
// ---------------------------------------------------------------------------

fn run_windows_subcommand() {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        // NSWorkspace (macOS) needs the shared-application insurance here,
        // same as the overlay harness above.
        #[cfg(target_os = "macos")]
        unsafe {
            init_nsapp();
        }
        let screens = subcommand_screen_geometries();
        if screens.is_empty() {
            eprintln!("no screens detected; cannot enumerate windows");
            std::process::exit(2);
        }
        for screen in &screens {
            println!(
                "screen: board=({}, {}) {}x{} scale={} native=({}, {})",
                screen.x,
                screen.y,
                screen.width,
                screen.height,
                screen.scale,
                screen.native_x,
                screen.native_y
            );
        }
        let mut service = create_window_service(&screens);
        match service.enumerate() {
            Ok(windows) => {
                println!("{} windows (z-order top to bottom):", windows.len());
                for (index, window) in windows.iter().enumerate() {
                    println!(
                        "  [{:2}] id={:8} fg={:5} process={:?} frame=({:.0},{:.0} {:.0}x{:.0}) title={:?}",
                        index,
                        window.id,
                        window.is_foreground,
                        window.process,
                        window.frame.0,
                        window.frame.1,
                        window.frame.2,
                        window.frame.3,
                        window.title
                    );
                }
                let titled = windows.iter().filter(|w| !w.title.is_empty()).count();
                println!(
                    "titles: {}/{} windows resolved (CG/AX or platform title)",
                    titled,
                    windows.len()
                );
            }
            Err(error) => {
                eprintln!("enumeration failed: {error}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        eprintln!("window enumeration is not available on this platform yet");
        std::process::exit(2);
    }
}

/// `halo_manual focus-receive <kind> <screen_id> <x> <y>` (appended): P4
/// receive-side verification rig. Simulates one remote halo focus event and
/// runs it through the exact same processing function the app's receive hook
/// uses (apply_focus_event): screen selection, window hit-test, passive
/// border highlight (auto-cleared by the runtime's 5 s watcher) and, for
/// `activate`, window activation. Coordinates are board/global logical
/// points; screen ids here are the synthesized `screen-<index>` of the
/// locally attached displays, and a point inside a display also resolves by
/// containment, so any on-screen coordinate works.
fn run_focus_receive_subcommand(args: &[String]) {
    let kind = args.get(2).map(String::as_str).unwrap_or("");
    let screen_id = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "screen-0".to_string());
    let parse_point = |index: usize, fallback: f64| -> f64 {
        args.get(index)
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback)
    };
    let x = parse_point(4, 0.0);
    let y = parse_point(5, 0.0);
    let event = match kind {
        "edge" => HaloFocusEvent::Edge {
            screen_id: screen_id.clone(),
            x,
            y,
        },
        "candidate" => HaloFocusEvent::Candidate {
            screen_id: screen_id.clone(),
            x,
            y,
        },
        "activate" => HaloFocusEvent::Activate {
            screen_id: screen_id.clone(),
            x,
            y,
        },
        "clear" => HaloFocusEvent::Clear,
        _ => {
            eprintln!("usage: halo_manual focus-receive <edge|candidate|activate|clear> <screen_id> <x> <y>");
            std::process::exit(2);
        }
    };

    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }
    let screens: Vec<FocusScreen> = subcommand_screen_geometries()
        .into_iter()
        .enumerate()
        .map(|(index, geometry)| FocusScreen {
            id: format!("screen-{index}"),
            geometry,
        })
        .collect();
    if screens.is_empty() {
        eprintln!("no screens detected; cannot simulate focus receive");
        std::process::exit(2);
    }
    let geometries: Vec<OverlayScreenGeometry> =
        screens.iter().map(|screen| screen.geometry).collect();
    println!(
        "halo_manual focus-receive: kind={kind} screen={screen_id} point=({x},{y}) screens={}",
        screens.len()
    );
    let mut runtime = HaloRuntime::new();
    let mut windows = create_window_service(&geometries);
    let mut activator = create_activator();
    let outcome = apply_focus_event(
        &event,
        &mut runtime,
        &screens,
        &mut *windows,
        &mut *activator,
    );
    println!(
        "outcome: kind={} window={:?} highlighted={} activated={} {}",
        outcome.kind, outcome.window_title, outcome.highlighted, outcome.activated, outcome.detail
    );
    if kind != "clear" {
        println!(
            "highlight auto-clears after 5 s; keep the process running (Ctrl-C exits) to watch it"
        );
    }
    // Keep pumping so the overlay's async blocks execute and the runtime's
    // watcher thread can clear the highlight on schedule.
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

/// `halo_manual badges [clear_after_ms]` (appended): S2 badge-layer
/// verification rig. Shows the numbered badge (center label + four corner
/// brackets) on every attached display — exactly what the multi-screen
/// region wizard draws — without needing a camera, then optionally clears
/// after `clear_after_ms`. Run with two or more displays to check each
/// screen gets its own window and number.
fn run_badges_subcommand(args: &[String]) {
    let clear_after_ms: Option<u64> = args.get(2).and_then(|value| value.parse().ok());
    // Optional third arg: highlight this corner of screen 1 after showing
    // (exercises the set_active path: active corner brightens, others dim).
    let active_corner = args.get(3).and_then(|value| match value.as_str() {
        "tl" => Some(BadgeCorner::Tl),
        "tr" => Some(BadgeCorner::Tr),
        "br" => Some(BadgeCorner::Br),
        "bl" => Some(BadgeCorner::Bl),
        _ => None,
    });
    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }
    let geometries = subcommand_screen_geometries();
    if geometries.is_empty() {
        eprintln!("no screens detected; cannot show badges");
        std::process::exit(2);
    }
    let screens: Vec<BadgeScreen> = geometries
        .iter()
        .enumerate()
        .map(|(index, geometry)| BadgeScreen {
            geometry: *geometry,
            label: (index + 1).to_string(),
        })
        .collect();
    println!(
        "halo_manual badges: {} screen(s) clear_after={:?} active={:?}",
        screens.len(),
        clear_after_ms,
        active_corner
    );
    for screen in &screens {
        println!(
            "  badge {}: board=({}, {}) {}x{}",
            screen.label,
            screen.geometry.x,
            screen.geometry.y,
            screen.geometry.width,
            screen.geometry.height
        );
    }
    let mut badge = BadgeLayer::new();
    badge.show(&screens);
    if let Some(corner) = active_corner {
        // Small delay so the highlight lands after the reveal pass.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let mut badge = BadgeLayer::new();
            badge.set_active(0, corner);
            std::mem::forget(badge);
        });
    }
    if let Some(delay_ms) = clear_after_ms {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            let mut badge = BadgeLayer::new();
            badge.hide();
            std::mem::forget(badge);
            println!("halo_manual badges: cleared after {delay_ms} ms");
        });
    }
    // Keep pumping so the dispatch_async_f blocks execute and the badges
    // stay visible until Ctrl-C (or until the optional clear fires).
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

/// `halo_manual activate <id>` (appended): P3 activation verification rig.
/// Runs the platform activator once against the given window id (take ids
/// from the `windows` subcommand output) and prints the outcome. This steals
/// focus on purpose — it is the real-machine check for the dwell-switch path.
fn run_activate_subcommand(args: &[String]) {
    let Some(window_id) = args.get(2).and_then(|value| value.parse::<u64>().ok()) else {
        eprintln!("usage: halo_manual activate <window_id>");
        std::process::exit(2);
    };
    #[cfg(target_os = "macos")]
    unsafe {
        init_nsapp();
    }
    let mut activator = create_activator();
    let outcome = activator.activate(window_id);
    println!(
        "activate {window_id}: activated={} fell_back={} detail={}",
        outcome.activated, outcome.fell_back, outcome.detail
    );
    if !outcome.activated {
        std::process::exit(1);
    }
}

/// All attached macOS displays in CG global points — the identity mapping
/// into board space, matching the window layer's macOS convention.
#[cfg(target_os = "macos")]
fn subcommand_screen_geometries() -> Vec<OverlayScreenGeometry> {
    let mut ids = core_graphics::display::CGDisplay::active_displays().unwrap_or_default();
    if ids.is_empty() {
        ids.push(core_graphics::display::CGDisplay::main().id);
    }
    ids.iter()
        .map(|id| {
            let bounds = core_graphics::display::CGDisplay::new(*id).bounds();
            OverlayScreenGeometry {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                scale: 1.0,
                native_x: bounds.origin.x,
                native_y: bounds.origin.y,
            }
        })
        .collect()
}

/// Virtual-screen bounding box as one screen at scale 1.0. Good enough for
/// the manual rig at default DPI; the real app feeds per-monitor scales from
/// its Tauri monitor list.
#[cfg(target_os = "windows")]
fn subcommand_screen_geometries() -> Vec<OverlayScreenGeometry> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    vec![OverlayScreenGeometry {
        x: 0.0,
        y: 0.0,
        width: width as f64,
        height: height as f64,
        scale: 1.0,
        native_x: x as f64,
        native_y: y as f64,
    }]
}
