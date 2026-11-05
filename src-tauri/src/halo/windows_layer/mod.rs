//! FocusHalo window layer (P2): visible-window enumeration, point hit-testing,
//! and the 300 ms enumerate cache. Query-only by design — activation/switching
//! is a later phase and never lives here.
//!
//! Layout: [`WindowInfo`]/[`WindowService`] are the platform-neutral model;
//! `macos.rs` / `win32.rs` gather raw platform facts; this module owns all the
//! shared machinery (cache, pure filtering, hit-testing, Windows physical→
//! logical coordinate math) so every decision rule is unit-tested here on any
//! host. Coordinates everywhere are the layout board's logical space —
//! identical to `OverlayScreenGeometry` (`x, y, width, height`, origin top
//! left); on macOS CG global points already match that space (identity), on
//! Windows [`native_rect_to_logical`] converts physical pixels per screen.

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod win32;

use crate::halo::overlay::OverlayScreenGeometry;

/// Freshness window of the enumerate cache. The engine can hit-test at its
/// tick rate while real enumeration happens at most ~3.3 times a second.
pub const CACHE_TTL_MS: u64 = 300;
/// Hard cap on returned windows (z-order top first) so a window farm cannot
/// blow up downstream per-window work.
pub const MAX_WINDOWS: usize = 128;

/// One hittable, visible, non-shell window in layout-board logical space.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    /// Platform window id: macOS `CGWindowNumber`, Windows HWND as u64.
    pub id: u64,
    /// Window title. May be empty when neither the platform nor the AX
    /// fallback could produce one (macOS without screen-recording permission).
    pub title: String,
    /// Process/bundle name, lowercased (blacklist matching input).
    pub process: String,
    /// Logical frame in layout-board space: `(x, y, w, h)`.
    pub frame: (f64, f64, f64, f64),
    /// Whether this window belongs to the current foreground application
    /// (macOS: any visible window of the frontmost pid; Windows: the one
    /// foreground HWND).
    pub is_foreground: bool,
}

/// Query-only window service. All calls may arrive from any thread; the cache
/// lives inside the implementation.
pub trait WindowService: Send {
    /// Enumerate the visible, hittable windows, z-order top to bottom.
    /// Served from the 300 ms cache when fresh.
    fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String>;
    /// Point hit-test in layout-board logical coordinates: the z-topmost
    /// window containing the point. Uses (and refreshes) the same cache as
    /// [`WindowService::enumerate`].
    fn hit_test(&mut self, x: f64, y: f64) -> Result<Option<WindowInfo>, String>;
    /// Drop the cached list; the next enumerate re-runs the platform pass.
    fn invalidate(&mut self);
}

pub fn create_window_service(screens: &[OverlayScreenGeometry]) -> Box<dyn WindowService> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOSWindowService::new(screens))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(win32::Win32WindowService::new(screens))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = screens;
        Box::new(UnsupportedService)
    }
}

/// Linux placeholder: the P2 rig only targets macOS and Windows.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct UnsupportedService;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl WindowService for UnsupportedService {
    fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
        Err("window enumeration is not available on this platform yet".into())
    }

    fn hit_test(&mut self, _x: f64, _y: f64) -> Result<Option<WindowInfo>, String> {
        Err("window enumeration is not available on this platform yet".into())
    }

    fn invalidate(&mut self) {}
}

// ---------------------------------------------------------------------------
// Raw facts + pure filtering
// ---------------------------------------------------------------------------

/// Platform-neutral facts one candidate window carries into the pure filter
/// pass. Platforms gather the cheap facts themselves (visibility, cloak,
/// tool/shell flags, process names, titles) so the filter is a pure,
/// host-independent transform over them.
pub(crate) struct RawWindow {
    pub id: u64,
    pub pid: u32,
    /// Logical frame `(x, y, w, h)` in layout-board space (converted by the
    /// platform backend already).
    pub frame: (f64, f64, f64, f64),
    pub title: String,
    /// Process/bundle name, already lowercased by the platform backend.
    pub process: String,
    /// Platform visibility verdict: false for minimized/cloaked/hidden/tool
    /// windows and (macOS) any non-zero CG window layer.
    pub visible: bool,
    /// Desktop-shell element (Dock, menu bar, Finder desktop, Windows
    /// Progman/WorkerW/tray): always dropped by the filter.
    pub shell: bool,
}

/// Which windows count as foreground, in the pure filter pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForegroundMatch {
    None,
    /// Windows: the single foreground HWND.
    Window(u64),
    /// macOS: every visible window of the frontmost application's pid.
    Process(u32),
}

impl ForegroundMatch {
    fn matches(self, id: u64, pid: u32) -> bool {
        match self {
            ForegroundMatch::None => false,
            ForegroundMatch::Window(fg_id) => fg_id != 0 && fg_id == id,
            ForegroundMatch::Process(fg_pid) => fg_pid != 0 && fg_pid == pid,
        }
    }
}

/// The pure filter: raw candidates (z-order preserved by the platform pass)
/// to the final visible-window list. Drops non-visible and shell windows,
/// windows owned by our own process, windows whose frame intersects no screen,
/// and truncates to [`MAX_WINDOWS`] keeping the z-order top.
pub(crate) fn filter_windows(
    raw: Vec<RawWindow>,
    own_pid: u32,
    screens: &[OverlayScreenGeometry],
    foreground: ForegroundMatch,
) -> Vec<WindowInfo> {
    let mut out = Vec::new();
    for window in raw {
        if !window.visible || window.shell {
            continue;
        }
        if window.pid == 0 || window.pid == own_pid {
            continue;
        }
        if !rect_intersects_screens(window.frame, screens) {
            continue;
        }
        if out.len() >= MAX_WINDOWS {
            break;
        }
        let is_foreground = foreground.matches(window.id, window.pid);
        out.push(WindowInfo {
            id: window.id,
            title: window.title,
            process: window.process,
            frame: window.frame,
            is_foreground,
        });
    }
    out
}

/// Pure hit-test: the first window in z-order (list order) whose frame
/// contains the point. Frame edges are half-open (right/bottom exclusive) so
/// adjacent windows never double-hit on their shared edge.
pub(crate) fn hit_test_windows(windows: &[WindowInfo], x: f64, y: f64) -> Option<WindowInfo> {
    windows
        .iter()
        .find(|window| rect_contains(window.frame, x, y))
        .cloned()
}

/// Half-open containment: `x` in `[x, x+w)` and `y` in `[y, y+h)`.
pub(crate) fn rect_contains(frame: (f64, f64, f64, f64), x: f64, y: f64) -> bool {
    let (rx, ry, rw, rh) = frame;
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Whether the frame has a non-empty intersection with any screen.
pub(crate) fn rect_intersects_screens(
    frame: (f64, f64, f64, f64),
    screens: &[OverlayScreenGeometry],
) -> bool {
    let (x, y, w, h) = frame;
    if !(w > 0.0 && h > 0.0) {
        return false;
    }
    screens
        .iter()
        .any(|screen| overlap_area(frame, (screen.x, screen.y, screen.width, screen.height)) > 0.0)
        && (x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite())
}

/// Intersection area of two `(x, y, w, h)` frames (0 when disjoint).
fn overlap_area(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> f64 {
    let width = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let height = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    if width > 0.0 && height > 0.0 {
        width * height
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Windows physical -> logical coordinate math
// ---------------------------------------------------------------------------

/// Convert a Windows physical-pixel rect `(left, top, right, bottom)` to the
/// layout board's logical `(x, y, w, h)` using the screen set. The screen
/// whose native bounds overlap the rect the most (first one wins on ties)
/// provides the DPI scale plus the native→board origin pair; when no screen
/// overlaps at all, the primary screen (`screens[0]`, the caller's board
/// order) provides them. Pure so it is unit-tested on any host.
pub(crate) fn native_rect_to_logical(
    rect: (f64, f64, f64, f64),
    screens: &[OverlayScreenGeometry],
) -> (f64, f64, f64, f64) {
    let chosen = native_rect_screen(rect, screens).or_else(|| screens.first());
    let (scale, board_x, board_y, origin_x, origin_y) = match chosen {
        Some(screen) => (
            safe_scale(screen.scale),
            screen.x,
            screen.y,
            screen.native_x,
            screen.native_y,
        ),
        // Empty screen set: identity conversion.
        None => (1.0, 0.0, 0.0, 0.0, 0.0),
    };
    let (left, top, right, bottom) = rect;
    (
        board_x + (left - origin_x) / scale,
        board_y + (top - origin_y) / scale,
        (right - left) / scale,
        (bottom - top) / scale,
    )
}

/// The screen with the strictly largest native-pixel overlap of the rect
/// (first screen wins ties), or `None` when nothing overlaps.
fn native_rect_screen(
    rect: (f64, f64, f64, f64),
    screens: &[OverlayScreenGeometry],
) -> Option<&OverlayScreenGeometry> {
    let mut best: Option<(f64, &OverlayScreenGeometry)> = None;
    for screen in screens {
        let overlap = native_overlap(rect, screen);
        if overlap <= 0.0 {
            continue;
        }
        match best {
            Some((best_overlap, _)) if best_overlap >= overlap => {}
            _ => best = Some((overlap, screen)),
        }
    }
    best.map(|(_, screen)| screen)
}

/// Overlap area between the physical `(left, top, right, bottom)` rect and a
/// screen's native bounds `(native_x, native_y, width*scale, height*scale)`.
fn native_overlap(rect: (f64, f64, f64, f64), screen: &OverlayScreenGeometry) -> f64 {
    let scale = safe_scale(screen.scale);
    let (left, top, right, bottom) = rect;
    let frame = (left, top, right - left, bottom - top);
    let screen_native = (
        screen.native_x,
        screen.native_y,
        screen.width * scale,
        screen.height * scale,
    );
    overlap_area(frame, screen_native)
}

pub(crate) fn safe_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// Enumerate cache
// ---------------------------------------------------------------------------

/// Injectable monotonic millisecond clock, so the TTL logic is unit-tested
/// without real sleeps.
pub(crate) type NowMs = Box<dyn Fn() -> u64 + Send>;

pub(crate) struct WindowCache {
    now_ms: NowMs,
    entry: Option<CachedList>,
}

struct CachedList {
    at_ms: u64,
    windows: Vec<WindowInfo>,
}

impl WindowCache {
    pub(crate) fn new(now_ms: NowMs) -> Self {
        WindowCache {
            now_ms,
            entry: None,
        }
    }

    /// Process-uptime milliseconds: the default clock. A static epoch keeps
    /// the values small and monotonic within the process.
    pub(crate) fn real() -> Self {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        WindowCache::new(Box::new(|| {
            let epoch = EPOCH.get_or_init(std::time::Instant::now);
            epoch.elapsed().as_millis() as u64
        }))
    }

    /// The cached list when still inside [`CACHE_TTL_MS`].
    pub(crate) fn get(&self) -> Option<&[WindowInfo]> {
        let entry = self.entry.as_ref()?;
        if (self.now_ms)().saturating_sub(entry.at_ms) < CACHE_TTL_MS {
            Some(&entry.windows)
        } else {
            None
        }
    }

    pub(crate) fn store(&mut self, windows: Vec<WindowInfo>) {
        let at_ms = (self.now_ms)();
        self.entry = Some(CachedList { at_ms, windows });
    }

    pub(crate) fn invalidate(&mut self) {
        self.entry = None;
    }
}

// ---------------------------------------------------------------------------
// Shared cached driver over a platform backend
// ---------------------------------------------------------------------------

/// The platform-specific half of a window service: raw candidate gathering.
/// Filtering, foreground marking, capping and caching all stay here, shared.
pub(crate) trait RawEnumerator: Send {
    fn enumerate_raw(
        &mut self,
        screens: &[OverlayScreenGeometry],
    ) -> Result<(Vec<RawWindow>, ForegroundMatch), String>;
}

/// Generic [`WindowService`]: one enumerate cache shared by enumerate and
/// hit-test, the pure filter, and the platform backend.
pub(crate) struct CachedService<B: RawEnumerator> {
    backend: B,
    screens: Vec<OverlayScreenGeometry>,
    cache: WindowCache,
}

impl<B: RawEnumerator> CachedService<B> {
    pub(crate) fn new(backend: B, screens: &[OverlayScreenGeometry], cache: WindowCache) -> Self {
        CachedService {
            backend,
            screens: screens.to_vec(),
            cache,
        }
    }
}

impl<B: RawEnumerator> WindowService for CachedService<B> {
    fn enumerate(&mut self) -> Result<Vec<WindowInfo>, String> {
        if let Some(windows) = self.cache.get() {
            return Ok(windows.to_vec());
        }
        let (raw, foreground) = self.backend.enumerate_raw(&self.screens)?;
        let windows = filter_windows(raw, std::process::id(), &self.screens, foreground);
        self.cache.store(windows.clone());
        Ok(windows)
    }

    fn hit_test(&mut self, x: f64, y: f64) -> Result<Option<WindowInfo>, String> {
        let windows = self.enumerate()?;
        Ok(hit_test_windows(&windows, x, y))
    }

    fn invalidate(&mut self) {
        self.cache.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;

    fn screen(x: f64, y: f64, width: f64, height: f64, scale: f64) -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x,
            y,
            width,
            height,
            scale,
            native_x: x,
            native_y: y,
        }
    }

    fn raw(
        id: u64,
        pid: u32,
        frame: (f64, f64, f64, f64),
        title: &str,
        visible: bool,
        shell: bool,
    ) -> RawWindow {
        RawWindow {
            id,
            pid,
            frame,
            title: title.into(),
            process: "app".into(),
            visible,
            shell,
        }
    }

    fn fake_clock(at_ms: u64) -> (Arc<AtomicU64>, WindowCache) {
        let clock = Arc::new(AtomicU64::new(at_ms));
        let cache = WindowCache::new(Box::new({
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::SeqCst)
        }));
        (clock, cache)
    }

    // ----- cache TTL / invalidation -----

    #[test]
    fn cache_serves_within_ttl_and_expires_after() {
        let (clock, mut cache) = fake_clock(1000);
        assert!(cache.get().is_none(), "empty cache never serves");

        cache.store(vec![WindowInfo {
            id: 1,
            title: "w".into(),
            process: "app".into(),
            frame: (0.0, 0.0, 10.0, 10.0),
            is_foreground: false,
        }]);
        assert_eq!(cache.get().map(|w| w.len()), Some(1));

        // Just inside the TTL: fresh.
        clock.store(1000 + CACHE_TTL_MS - 1, Ordering::SeqCst);
        assert!(cache.get().is_some());
        // At exactly TTL: stale.
        clock.store(1000 + CACHE_TTL_MS, Ordering::SeqCst);
        assert!(cache.get().is_none());
        // A clock glitch backwards saturates to age 0: the entry looks fresh
        // again. Harmless with the real monotonic process-uptime clock, and
        // an explicit invalidate always forces a refresh regardless.
        clock.store(500, Ordering::SeqCst);
        assert!(cache.get().is_some());

        cache.invalidate();
        assert!(cache.get().is_none(), "invalidate drops even fresh entries");
    }

    // ----- pure filter -----

    #[test]
    fn filter_drops_shell_invisible_own_pid_and_empty_intersections() {
        let screens = vec![screen(0.0, 0.0, 1000.0, 500.0, 1.0)];
        let own_pid = 4242;
        let list = filter_windows(
            vec![
                raw(1, 100, (0.0, 0.0, 100.0, 100.0), "ok", true, false),
                raw(2, 100, (0.0, 0.0, 100.0, 100.0), "shell", true, true),
                raw(3, 100, (0.0, 0.0, 100.0, 100.0), "minimized", false, false),
                raw(4, own_pid, (0.0, 0.0, 100.0, 100.0), "ours", true, false),
                // pid 0 (unknown owner) is never returned either.
                raw(5, 0, (0.0, 0.0, 100.0, 100.0), "pid0", true, false),
                raw(
                    6,
                    100,
                    (5000.0, 5000.0, 100.0, 100.0),
                    "offscreen",
                    true,
                    false,
                ),
                raw(7, 100, (0.0, 0.0, 0.0, 100.0), "degenerate", true, false),
            ],
            own_pid,
            &screens,
            ForegroundMatch::None,
        );
        let ids: Vec<u64> = list.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![1], "only the plain visible window survives");
    }

    #[test]
    fn filter_keeps_z_order_and_truncates_to_128() {
        let screens = vec![screen(0.0, 0.0, 1000.0, 500.0, 1.0)];
        let list: Vec<RawWindow> = (0..130)
            .map(|i| raw(i as u64, 100, (0.0, 0.0, 50.0, 50.0), "w", true, false))
            .collect();
        let out = filter_windows(list, 4242, &screens, ForegroundMatch::None);
        assert_eq!(out.len(), MAX_WINDOWS);
        // z-order: first entries are the top ones, kept as-is.
        assert_eq!(out.first().unwrap().id, 0);
        assert_eq!(out.last().unwrap().id, 127);
    }

    #[test]
    fn filter_marks_foreground_by_window_and_by_pid() {
        let screens = vec![screen(0.0, 0.0, 1000.0, 500.0, 1.0)];
        let windows = |fg: ForegroundMatch| {
            filter_windows(
                vec![
                    raw(10, 100, (0.0, 0.0, 50.0, 50.0), "a", true, false),
                    raw(11, 200, (10.0, 0.0, 50.0, 50.0), "b", true, false),
                ],
                4242,
                &screens,
                fg,
            )
        };
        let by_window = windows(ForegroundMatch::Window(11));
        assert_eq!(
            by_window
                .iter()
                .map(|w| (w.id, w.is_foreground))
                .collect::<Vec<_>>(),
            vec![(10, false), (11, true)]
        );
        // macOS style: every window of the frontmost pid.
        let by_pid = windows(ForegroundMatch::Process(200));
        assert_eq!(
            by_pid
                .iter()
                .map(|w| (w.id, w.is_foreground))
                .collect::<Vec<_>>(),
            vec![(10, false), (11, true)]
        );
        // pid 0 never matches, and foreground ids of 0 are ignored.
        assert!(windows(ForegroundMatch::Process(0))
            .iter()
            .all(|w| !w.is_foreground));
        assert!(windows(ForegroundMatch::Window(0))
            .iter()
            .all(|w| !w.is_foreground));
    }

    // ----- pure hit-test -----

    #[test]
    fn hit_test_returns_topmost_window_containing_point() {
        let windows = vec![
            WindowInfo {
                id: 1,
                title: "top".into(),
                process: "a".into(),
                frame: (0.0, 0.0, 100.0, 100.0),
                is_foreground: true,
            },
            WindowInfo {
                id: 2,
                title: "below".into(),
                process: "b".into(),
                frame: (50.0, 50.0, 100.0, 100.0),
                is_foreground: false,
            },
        ];
        // Inside both frames: z-order wins.
        assert_eq!(hit_test_windows(&windows, 60.0, 60.0).unwrap().id, 1);
        // Only the lower window contains it (half-open: (150,150) is outside).
        assert_eq!(hit_test_windows(&windows, 120.0, 140.0).unwrap().id, 2);
        // On window 1's right edge (x=100, y=20): half-open frames exclude it
        // from window 1, and it is above window 2's y range — neither window.
        assert!(hit_test_windows(&windows, 100.0, 20.0).is_none());
        // Outside everything.
        assert!(hit_test_windows(&windows, -1.0, 10.0).is_none());
        assert!(hit_test_windows(&windows, 0.0, 0.0).is_some());
    }

    // ----- Windows physical -> logical conversion -----

    #[test]
    fn native_rect_converts_on_the_overlapping_screen() {
        let screens = vec![
            screen(0.0, 0.0, 1000.0, 500.0, 1.0),
            // Second board screen at logical (1000, 0), native origin 2000 px,
            // 2x DPI: native bounds 2000..4000 x 0..1000.
            {
                let mut s = screen(1000.0, 0.0, 1000.0, 500.0, 2.0);
                s.native_x = 2000.0;
                s
            },
        ];
        // Physical rect on the second screen.
        let logical = native_rect_to_logical((3000.0, 100.0, 4000.0, 200.0), &screens);
        assert_eq!(
            logical,
            (1500.0, 50.0, 500.0, 50.0),
            "physical -> logical via screen 2 origin and 2x scale"
        );
        // Physical rect on the primary screen (scale 1, origin 0).
        let logical = native_rect_to_logical((10.0, 20.0, 110.0, 220.0), &screens);
        assert_eq!(logical, (10.0, 20.0, 100.0, 200.0));
        // A rect straddling both screens picks the one with the larger overlap
        // (first screen wins ties: here both overlaps are 100x100 px).
        let logical = native_rect_to_logical((900.0, 0.0, 2100.0, 100.0), &screens);
        assert_eq!(logical, (900.0, 0.0, 1200.0, 100.0));
        // Larger overlap on screen 2 wins: mostly on the 2x screen.
        let logical = native_rect_to_logical((2900.0, 0.0, 3950.0, 100.0), &screens);
        assert_eq!(logical, (1450.0, 0.0, 525.0, 50.0));
    }

    #[test]
    fn native_rect_falls_back_to_primary_screen_scale() {
        let screens = vec![
            screen(0.0, 0.0, 1000.0, 500.0, 2.0),
            screen(1000.0, 0.0, 1000.0, 500.0, 1.0),
        ];
        // No screen overlaps: primary screen's scale (2x) applies, origin 0.
        let logical = native_rect_to_logical((50000.0, 400.0, 51000.0, 600.0), &screens);
        assert_eq!(logical, (25000.0, 200.0, 500.0, 100.0));
        // Empty screen set: identity.
        assert_eq!(
            native_rect_to_logical((1.0, 2.0, 3.0, 4.0), &[]),
            (1.0, 2.0, 2.0, 2.0)
        );
    }

    #[test]
    fn native_rect_handles_degenerate_scales() {
        // A zero/NaN scale must not poison the math: safe_scale falls back 1.0.
        let screens = vec![
            screen(0.0, 0.0, 1000.0, 500.0, 0.0),
            screen(1000.0, 0.0, 1000.0, 500.0, f64::NAN),
        ];
        let logical = native_rect_to_logical((100.0, 100.0, 300.0, 400.0), &screens);
        assert_eq!(logical, (100.0, 100.0, 200.0, 300.0));
    }

    // ----- cached service over a fake backend -----

    struct FakeBackend {
        calls: u32,
        fail: bool,
    }

    impl FakeBackend {
        fn new() -> Self {
            FakeBackend {
                calls: 0,
                fail: false,
            }
        }
    }

    impl RawEnumerator for FakeBackend {
        fn enumerate_raw(
            &mut self,
            _screens: &[OverlayScreenGeometry],
        ) -> Result<(Vec<RawWindow>, ForegroundMatch), String> {
            self.calls += 1;
            if self.fail {
                return Err("backend exploded".into());
            }
            Ok((
                vec![raw(7, 100, (0.0, 0.0, 100.0, 100.0), "fake", true, false)],
                ForegroundMatch::Process(100),
            ))
        }
    }

    #[test]
    fn cached_service_reuses_the_list_and_invalidate_forces_a_refresh() {
        let (clock, cache) = fake_clock(0);
        let backend = FakeBackend::new();
        let screens = vec![screen(0.0, 0.0, 1000.0, 500.0, 1.0)];
        let mut service = CachedService::new(backend, &screens, cache);

        let first = service.enumerate().unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].is_foreground, "fake marks pid 100 foreground");
        let _ = service.enumerate().unwrap();
        assert_eq!(service.backend.calls, 1, "cache serves the second call");

        // Still fresh: no new backend call; hit_test shares the same cache.
        clock.store(CACHE_TTL_MS - 1, Ordering::SeqCst);
        assert_eq!(service.hit_test(50.0, 50.0).unwrap().unwrap().id, 7);
        assert_eq!(service.hit_test(200.0, 200.0).unwrap(), None);
        assert_eq!(service.backend.calls, 1);

        // Invalidate forces a fresh enumeration even inside the TTL.
        service.invalidate();
        let _ = service.enumerate().unwrap();
        assert_eq!(service.backend.calls, 2);
    }

    #[test]
    fn cached_service_propagates_backend_errors() {
        let (_, cache) = fake_clock(0);
        let screens = vec![screen(0.0, 0.0, 1000.0, 500.0, 1.0)];
        let mut service = CachedService::new(
            FakeBackend {
                calls: 0,
                fail: true,
            },
            &screens,
            cache,
        );
        assert_eq!(service.enumerate().unwrap_err(), "backend exploded");
        assert_eq!(
            service.hit_test(0.0, 0.0).unwrap_err(),
            "backend exploded",
            "hit_test surfaces the refresh failure too"
        );
    }

    #[test]
    fn constants_match_design() {
        assert_eq!(CACHE_TTL_MS, 300);
        assert_eq!(MAX_WINDOWS, 128);
    }

    // ----- Linux placeholder behavior (host-independent assertion) -----

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unsupported_platform_reports_placeholder_error() {
        let mut service = create_window_service(&[]);
        assert_eq!(
            service.enumerate().unwrap_err(),
            "window enumeration is not available on this platform yet"
        );
        service.invalidate();
    }
}
