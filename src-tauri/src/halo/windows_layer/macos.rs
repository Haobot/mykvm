//! FocusHalo window layer for macOS: `CGWindowListCopyWindowInfo` enumeration
//! (no screen-recording permission needed for id/bounds/pid/layer/owner), an
//! Accessibility fallback for titles (`AXWindows` matched by frame), and
//! NSWorkspace for the frontmost pid plus pid-to-app-name resolution.
//!
//! CG global points already are the layout board's logical coordinates (same
//! top-left origin, 1 pt units), so frames pass through unchanged — the same
//! identity convention the P0 overlay uses. The raw objc_msgSend style is the
//! `input_guard_macos.rs` / overlay precedent; a live desktop integration
//! test sits at the bottom (`cargo test --lib -- --ignored`).

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};

use core_foundation::base::{CFRelease, CFRetain, CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

use super::{CachedService, ForegroundMatch, RawEnumerator, RawWindow, WindowCache};
use crate::halo::overlay::OverlayScreenGeometry;

/// Collect at most this many on-screen candidates (z-order top first) before
/// the pure filter; the filter's own 128 cap still applies after filtering,
/// this one just bounds per-window work (AX lookups).
const RAW_CANDIDATE_CAP: usize = 256;
/// AX messages wait at most this long per element (seconds), so a hung app
/// delays enumeration by at most this much, not forever.
const AX_TIMEOUT_SECONDS: f32 = 0.05;
/// Frame match tolerance (points) when pairing a CG window with an AX window.
const AX_FRAME_EPSILON: f64 = 1.0;
/// AXValueGetType discriminators (Accessibility headers).
const KAXVALUE_POINT_TYPE: u32 = 1;
const KAXVALUE_SIZE_TYPE: u32 = 2;

type AxElement = *const c_void;
type CfArrayRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AxElement;
    // Same signature as input_guard_macos.rs declares; keeping the two in
    // sync avoids a clashing-extern-declarations warning when both compile.
    fn AXUIElementCopyAttributeValue(
        element: AxElement,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AxElement, timeout: f32) -> i32;
    fn AXValueGetValue(value: *const c_void, value_type: u32, out: *mut c_void) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> *const c_void;
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe fn object(receiver: *mut c_void, selector: &'static [u8]) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel_registerName(selector.as_ptr().cast()))
}

unsafe fn string(receiver: *mut c_void) -> Option<String> {
    if receiver.is_null() {
        return None;
    }
    let pointer = object(receiver, b"UTF8String\0").cast::<c_char>();
    if pointer.is_null() {
        None
    } else {
        Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
    }
}

unsafe fn int(receiver: *mut c_void, selector: &'static [u8]) -> i32 {
    if receiver.is_null() {
        return 0;
    }
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel_registerName(selector.as_ptr().cast()))
}

unsafe fn uint(receiver: *mut c_void, selector: &'static [u8]) -> u64 {
    if receiver.is_null() {
        return 0;
    }
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64 =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel_registerName(selector.as_ptr().cast()))
}

// ---------------------------------------------------------------------------
// NSWorkspace: frontmost pid + app names
// ---------------------------------------------------------------------------

/// NSApplication insurance for contexts without app UI (tests, CLI tools):
/// `NSWorkspace.frontmostApplication` only reports meaningfully once a
/// shared application exists. Mirrors `examples/halo_manual.rs`'s
/// `init_nsapp`; idempotent inside the real app.
unsafe fn init_nsapp() {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(
        objc_getClass(b"NSApplication\0".as_ptr().cast()),
        sel_registerName(b"sharedApplication\0".as_ptr().cast()),
    );
}

// ---------------------------------------------------------------------------
// Process names via libproc (thread-safe replacement for NSWorkspace)
//
// NSWorkspace / NSRunningApplication are NOT safe off the main thread:
// `localizedName` (and even `processIdentifier` when it lazily fetches
// dynamic properties) resolves through LaunchServices, and that XPC can
// block indefinitely when driven from a background thread. Observed live on
// 2026-11-05: the halo engine thread (which calls enumerate at ~3 Hz via
// the 300 ms cache) wedged inside
// `-[NSRunningApplication _fetchDynamicProperties] ->
//  _LSCopyApplicationInformation`, freezing the whole tracking pipeline and
// the halo ring. libproc's sysctl-backed calls have no such hazard, so the
// enumerate path resolves pid -> app name through proc_pidpath instead.
// ---------------------------------------------------------------------------

#[link(name = "proc")]
extern "C" {
    fn proc_listallpids(buffer: *mut i32, buffersize: i32) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

/// pid -> friendly app name for every running process: the `.app` bundle
/// component of the executable path when there is one, else the binary
/// name. Replaces the old NSWorkspace map (used for blacklist matching and
/// telemetry); CGWindowList's `kCGWindowOwnerName` stays the fallback for
/// pids the list misses.
unsafe fn proc_app_names_by_pid() -> HashMap<u32, String> {
    let mut pids = [0i32; 2048];
    let count = proc_listallpids(
        pids.as_mut_ptr(),
        (pids.len() * std::mem::size_of::<i32>()) as i32,
    );
    if count <= 0 {
        return HashMap::new();
    }
    let mut names = HashMap::new();
    for &pid in &pids[..count as usize] {
        if pid <= 0 {
            continue;
        }
        let mut buffer = [0u8; 4096];
        let len = proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32);
        if len <= 0 {
            continue;
        }
        let path =
            std::str::from_utf8(&buffer[..(len as usize).min(buffer.len())]).unwrap_or_default();
        let name = app_name_from_path(path);
        if !name.is_empty() {
            names.insert(pid as u32, name);
        }
    }
    names
}

/// `"/Applications/Foo.app/Contents/MacOS/Foo"` -> `"Foo"` (the bundle name
/// matches what NSWorkspace's `localizedName` reported for bundle apps);
/// any other path -> its last component.
fn app_name_from_path(path: &str) -> String {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() >= 4 {
        if let Some(stripped) = components[components.len() - 4].strip_suffix(".app") {
            return stripped.to_string();
        }
    }
    components.last().copied().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------------------
// Accessibility title fallback
// ---------------------------------------------------------------------------

/// One AX window: global frame, its title, and the AX element (+1 retained —
/// every caller must `CFRelease` it, including the enumerate path that only
/// reads frame/title). Retaining here lets the activator keep a matched
/// element alive past this list's lifetime to perform `AXRaise` on it.
pub(crate) struct AxWindow {
    pub(crate) frame: CGRect,
    pub(crate) title: String,
    pub(crate) element: AxElement,
}

/// Copy one AX attribute value (+1, caller must `CFRelease`), `None` on
/// error / missing attribute / null element.
fn ax_attribute(element: AxElement, name: &str) -> Option<*const c_void> {
    unsafe {
        if element.is_null() {
            return None;
        }
        let attribute = CFString::new(name);
        let mut value = std::ptr::null();
        if AXUIElementCopyAttributeValue(
            element,
            attribute.as_concrete_TypeRef().cast(),
            &mut value,
        ) == 0
            && !value.is_null()
        {
            Some(value)
        } else {
            None
        }
    }
}

fn ax_string_attribute(element: AxElement, name: &str) -> Option<String> {
    let value = ax_attribute(element, name)?;
    unsafe {
        // Copy rule: the wrapper owns the +1 reference and releases on drop.
        Some(CFString::wrap_under_create_rule(value.cast()).to_string())
    }
}

/// `AXPosition` of an AX element as global points.
fn ax_position(element: AxElement) -> Option<CGPoint> {
    let value = ax_attribute(element, "AXPosition")?;
    unsafe {
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        let ok = AXValueGetValue(
            value,
            KAXVALUE_POINT_TYPE,
            &mut point as *mut CGPoint as *mut c_void,
        ) != 0;
        CFRelease(value);
        ok.then_some(point)
    }
}

/// `AXSize` of an AX element in points.
fn ax_size(element: AxElement) -> Option<CGSize> {
    let value = ax_attribute(element, "AXSize")?;
    unsafe {
        let mut size = CGSize {
            width: 0.0,
            height: 0.0,
        };
        let ok = AXValueGetValue(
            value,
            KAXVALUE_SIZE_TYPE,
            &mut size as *mut CGSize as *mut c_void,
        ) != 0;
        CFRelease(value);
        ok.then_some(size)
    }
}

/// Every AX window of one application (pid), or an empty list when the app
/// exposes no accessibility windows (or hangs past the messaging timeout).
/// Each returned window carries a +1 element reference (see [`AxWindow`]).
pub(crate) fn ax_windows_for_pid(pid: u32) -> Vec<AxWindow> {
    unsafe {
        let application = AXUIElementCreateApplication(pid as i32);
        if application.is_null() {
            return Vec::new();
        }
        AXUIElementSetMessagingTimeout(application, AX_TIMEOUT_SECONDS);
        let mut windows = Vec::new();
        if let Some(list) = ax_attribute(application, "AXWindows") {
            let count = CFArrayGetCount(list);
            for index in 0..count {
                let element = CFArrayGetValueAtIndex(list, index);
                if element.is_null() {
                    continue;
                }
                AXUIElementSetMessagingTimeout(element, AX_TIMEOUT_SECONDS);
                let (Some(position), Some(size)) = (ax_position(element), ax_size(element)) else {
                    continue;
                };
                let title = ax_string_attribute(element, "AXTitle").unwrap_or_default();
                // Get-rule element from the array: retain it so the +1 field
                // contract holds after `CFRelease(list)` below.
                CFRetain(element);
                windows.push(AxWindow {
                    frame: CGRect {
                        origin: position,
                        size,
                    },
                    title,
                    element,
                });
            }
            // AXUIElementCopyAttributeValue handed us a +1 array.
            CFRelease(list);
        }
        CFRelease(application);
        windows
    }
}

/// (CGWindowNumber, owner pid, bounds) for every on-screen window, no
/// filtering: the activator looks window ids up by exact number and only
/// needs pid + frame from this raw pass.
pub(crate) fn cg_window_table() -> Result<Vec<(u64, u32, CGRect)>, String> {
    let array = copy_window_info(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)
        .ok_or_else(|| "CGWindowListCopyWindowInfo returned no array".to_string())?;
    let mut out = Vec::new();
    unsafe {
        let array_ref = array.as_concrete_TypeRef();
        let count = CFArrayGetCount(array_ref as CfArrayRef).max(0);
        for index in 0..(count as usize).min(RAW_CANDIDATE_CAP) {
            let pointer = CFArrayGetValueAtIndex(array_ref as CfArrayRef, index as isize);
            if pointer.is_null() {
                continue;
            }
            let dict = WindowDict::wrap_under_get_rule(pointer.cast());
            let Some(id) = cf_number_u32(&dict, kCGWindowNumber) else {
                continue;
            };
            let Some(pid) = cf_number_i32(&dict, kCGWindowOwnerPID) else {
                continue;
            };
            if pid <= 0 {
                continue;
            }
            let Some(frame) = cf_bounds(&dict) else {
                continue;
            };
            out.push((id as u64, pid as u32, frame));
        }
    }
    Ok(out)
}

pub(crate) fn frames_match(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() <= AX_FRAME_EPSILON
        && (a.origin.y - b.origin.y).abs() <= AX_FRAME_EPSILON
        && (a.size.width - b.size.width).abs() <= AX_FRAME_EPSILON
        && (a.size.height - b.size.height).abs() <= AX_FRAME_EPSILON
}

// ---------------------------------------------------------------------------
// CG window info parsing
// ---------------------------------------------------------------------------

type WindowDict = CFDictionary<CFString, CFType>;

fn cf_number_u32(dict: &WindowDict, key: CFStringRef) -> Option<u32> {
    dict.find(key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|number| number.to_i64())
        .map(|value| value as u32)
}

fn cf_number_i32(dict: &WindowDict, key: CFStringRef) -> Option<i32> {
    dict.find(key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|number| number.to_i32())
}

fn cf_string(dict: &WindowDict, key: CFStringRef) -> Option<String> {
    dict.find(key)
        .and_then(|value| value.downcast::<CFString>())
        .map(|string| string.to_string())
}

/// `kCGWindowBounds` as a CGRect via
/// `CGRectMakeWithDictionaryRepresentation` (the value CG itself put in the
/// window info is a CFDictionary; no typed wrapper needed to hand it over).
fn cf_bounds(dict: &WindowDict) -> Option<CGRect> {
    // Reading the extern static is itself unsafe.
    let bounds = dict.find(unsafe { kCGWindowBounds })?;
    let mut rect = CGRect::default();
    unsafe {
        if CGRectMakeWithDictionaryRepresentation(bounds.as_concrete_TypeRef().cast(), &mut rect) {
            Some(rect)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

pub(crate) struct MacOSWindowService;

impl MacOSWindowService {
    /// Build the cached macOS window service.
    pub(crate) fn new(screens: &[OverlayScreenGeometry]) -> CachedService<Self> {
        CachedService::new(MacOSWindowService, screens, WindowCache::real())
    }
}

impl RawEnumerator for MacOSWindowService {
    fn enumerate_raw(
        &mut self,
        _screens: &[OverlayScreenGeometry],
    ) -> Result<(Vec<RawWindow>, ForegroundMatch), String> {
        // macOS frames pass through: CG global points == layout-board logical
        // coordinates, so the screen set is only used by the pure filter.
        let array = copy_window_info(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)
            .ok_or_else(|| "CGWindowListCopyWindowInfo returned no array".to_string())?;
        unsafe {
            let pool = objc_autoreleasePoolPush();
            // Thread-safe replacements for the NSWorkspace calls this used to
            // make (see the libproc section above): app names come from
            // proc_pidpath, and the frontmost pid is derived from the very
            // list being iterated — CGWindowList returns z-order front to
            // back, so the first layer-0 window that is not our own process
            // belongs to the frontmost application.
            let app_names = proc_app_names_by_pid();
            let own_pid = std::process::id();
            let mut frontmost_pid: u32 = 0;
            let array_ref = array.as_concrete_TypeRef();

            let mut raw = Vec::new();
            let count = CFArrayGetCount(array_ref as CfArrayRef).max(0);
            for index in 0..(count as usize).min(RAW_CANDIDATE_CAP) {
                let pointer = CFArrayGetValueAtIndex(array_ref as CfArrayRef, index as isize);
                if pointer.is_null() {
                    continue;
                }
                let dict = WindowDict::wrap_under_get_rule(pointer.cast());
                let Some(id) = cf_number_u32(&dict, kCGWindowNumber) else {
                    continue;
                };
                let Some(pid) = cf_number_i32(&dict, kCGWindowOwnerPID) else {
                    continue;
                };
                // Non-zero layers are shell elements (Dock, menu bar, status
                // windows) and Finder's desktop backdrop: not hittable targets.
                if cf_number_i32(&dict, kCGWindowLayer).unwrap_or(-1) != 0 {
                    continue;
                }
                let Some(frame) = cf_bounds(&dict) else {
                    continue;
                };
                if frontmost_pid == 0 && pid > 0 && pid as u32 != own_pid {
                    frontmost_pid = pid as u32;
                }
                let owner_name = cf_string(&dict, kCGWindowOwnerName).unwrap_or_default();
                let process = app_names
                    .get(&(pid as u32))
                    .cloned()
                    .unwrap_or(owner_name)
                    .to_lowercase();
                let title = cf_string(&dict, kCGWindowName).unwrap_or_default();
                raw.push(RawWindow {
                    id: id as u64,
                    pid: pid as u32,
                    frame: (
                        frame.origin.x,
                        frame.origin.y,
                        frame.size.width,
                        frame.size.height,
                    ),
                    title,
                    process,
                    visible: true,
                    shell: false,
                });
            }

            // Title fallback: group every still-untitled window by pid and ask
            // each app once for its AX windows, matching frames to fill titles
            // in. Skipped when a screen-recording permission already provided
            // kCGWindowName values.
            let mut pending: HashMap<u32, Vec<usize>> = HashMap::new();
            for (index, window) in raw.iter().enumerate() {
                if window.title.is_empty() && window.pid != 0 && window.pid != own_pid {
                    pending.entry(window.pid).or_default().push(index);
                }
            }
            for (pid, indices) in pending {
                let ax_windows = ax_windows_for_pid(pid);
                for index in indices {
                    let frame = raw[index].frame;
                    let cg = CGRect {
                        origin: CGPoint {
                            x: frame.0,
                            y: frame.1,
                        },
                        size: CGSize {
                            width: frame.2,
                            height: frame.3,
                        },
                    };
                    if let Some(ax) = ax_windows
                        .iter()
                        .find(|ax| frames_match(ax.frame, cg) && !ax.title.is_empty())
                    {
                        raw[index].title = ax.title.clone();
                    }
                }
                // The enumerate path only read frame/title: release every
                // retained element (see AxWindow's +1 contract).
                for ax in &ax_windows {
                    CFRelease(ax.element);
                }
            }

            objc_autoreleasePoolPop(pool);
            Ok((raw, ForegroundMatch::Process(frontmost_pid)))
        }
    }
}

/// Screen geometries for the whole attached display set, in CG global points
/// (the identity mapping into board space). Used by the live test; mirrors
/// what `lib.rs` feeds the halo from the Tauri monitor list.
fn live_screen_geometries() -> Vec<OverlayScreenGeometry> {
    let mut ids = CGDisplay::active_displays().unwrap_or_default();
    if ids.is_empty() {
        ids.push(CGDisplay::main().id);
    }
    ids.iter()
        .map(|id| {
            let bounds = CGDisplay::new(*id).bounds();
            OverlayScreenGeometry {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                // macOS overlay math is all in points; scale stays identity.
                scale: 1.0,
                native_x: bounds.origin.x,
                native_y: bounds.origin.y,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::halo::windows_layer::WindowService;

    #[test]
    fn app_name_from_path_extracts_the_bundle() {
        // Bundle apps: the ".app" component three levels above the binary.
        assert_eq!(
            app_name_from_path("/Applications/Safari.app/Contents/MacOS/Safari"),
            "Safari"
        );
        assert_eq!(
            app_name_from_path("/Users/x/Applications/My Tool.app/Contents/MacOS/My Tool"),
            "My Tool"
        );
        // Helpers and bare binaries: last component.
        assert_eq!(app_name_from_path("/usr/libexec/spotlight"), "spotlight");
        assert_eq!(app_name_from_path("/bin/zsh"), "zsh");
        // Degenerate inputs: empty, not a path.
        assert_eq!(app_name_from_path(""), "");
        assert_eq!(app_name_from_path("/"), "");
    }

    /// libproc resolution must produce a non-empty map on any live session
    /// (this test binary itself is running) and must never block - a hang
    /// here would reproduce the LaunchServices wedge it replaced.
    #[test]
    fn proc_app_names_resolves_live_pids() {
        let names = unsafe { proc_app_names_by_pid() };
        assert!(!names.is_empty(), "at least this process should resolve");
        assert!(names.contains_key(&std::process::id()));
    }

    /// Live-desktop integration test: real window server, real AX, real
    /// NSWorkspace. Not part of the default run.
    #[test]
    #[ignore = "needs a live macOS desktop session; run with `cargo test --lib -- --ignored`"]
    fn live_macos_enumerate_and_hit_test() {
        let screens = live_screen_geometries();
        assert!(!screens.is_empty(), "at least one display must be attached");
        // The test binary has no app UI; NSWorkspace needs the insurance.
        unsafe { init_nsapp() };

        // Raw pass first: no candidate may belong to our own process.
        let own_pid = std::process::id();
        unsafe {
            let names = proc_app_names_by_pid();
            println!(
                "diagnostics: own pid={own_pid}, libproc knows {} app names",
                names.len()
            );
        }
        let mut backend = MacOSWindowService;
        let (raw, _) = backend.enumerate_raw(&screens).expect("raw enumerate");
        assert!(
            !raw.is_empty(),
            "a live desktop always has on-screen layer-0 windows"
        );
        assert!(
            raw.iter().all(|w| w.pid != own_pid),
            "own process windows must never be enumerated"
        );
        assert!(
            raw.iter().all(|w| w.frame.2 > 0.0 && w.frame.3 > 0.0),
            "all frames must be non-degenerate"
        );

        let mut service = MacOSWindowService::new(&screens);
        let started = std::time::Instant::now();
        let windows = service.enumerate().expect("enumerate");
        println!(
            "live enumerate: {} windows (of {} raw candidates) in {:?}",
            windows.len(),
            raw.len(),
            started.elapsed()
        );

        for (index, window) in windows.iter().enumerate() {
            assert_ne!(window.id, 0, "every window carries a CG window number");
            assert!(window.frame.2 > 0.0 && window.frame.3 > 0.0);
            // The service caches: the second enumerate must return identical
            // data without re-asking the window server.
            if index == 0 {
                let again = service.enumerate().expect("cached enumerate");
                assert_eq!(windows, again, "cache must serve an identical list");
            }
            println!(
                "  [{:2}] id={:6} fg={:5} process={:?} frame=({:.0},{:.0} {:.0}x{:.0}) title={:?}",
                index,
                window.id,
                window.is_foreground,
                window.process,
                window.frame.0,
                window.frame.1,
                window.frame.2,
                window.frame.3,
                window.title,
            );
        }
        let titled = windows.iter().filter(|w| !w.title.is_empty()).count();
        println!(
            "titles: {}/{} windows ({:.0}%) resolved via CG or the AX fallback",
            titled,
            windows.len(),
            100.0 * titled as f64 / windows.len().max(1) as f64
        );

        // Hit-testing: the center of a foreground window must hit a window of
        // the frontmost application (the same window unless a floating panel
        // of the frontmost app covers that exact point). On a locked/login
        // session NSWorkspace reports loginwindow as frontmost, which owns no
        // hittable windows — a legitimate state, not a failure.
        let Some(fg) = windows.iter().find(|w| w.is_foreground) else {
            println!(
                "note: no enumerated window belongs to the frontmost pid (locked/login session?); foreground hit-test skipped"
            );
            return;
        };
        let center = (fg.frame.0 + fg.frame.2 / 2.0, fg.frame.1 + fg.frame.3 / 2.0);
        let hit = service
            .hit_test(center.0, center.1)
            .expect("hit_test")
            .expect("the foreground window's own center must hit something");
        if hit.id != fg.id {
            println!(
                "note: center of id {} hit id {} ({:?}) instead",
                fg.id, hit.id, hit.title
            );
        }
        assert!(
            hit.is_foreground,
            "z-top window at the frontmost center belongs to the frontmost app"
        );
    }
}
