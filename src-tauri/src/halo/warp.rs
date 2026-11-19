//! Cursor warp executor for the "turn your head to switch screen" feature
//! (S3). When a head-pose region commit lands, the engine moves the OS
//! pointer to the newly selected screen's board center so the user arrives
//! where their gaze is already pointing.
//!
//! Layout-board to native conversion lives here too: the engine tracks
//! screens in layout-board logical coordinates, while every platform warp
//! API takes its own global native coordinates. On macOS CG global points
//! are already native (scale is identity); on Windows the same logical rect
//! maps to physical pixels through the screen's DPI factor.

use crate::halo::overlay::OverlayScreenGeometry;

/// Moves the OS cursor to platform-native global coordinates. Implementations
/// must be callable from the engine thread; `warp` returns whether the
/// platform accepted the move (best-effort: a failed warp only skips the
/// pointer move, the visual screen switch still happened).
pub trait CursorWarper: Send {
    fn warp(&mut self, native_x: f64, native_y: f64) -> bool;
}

/// Convert a layout-board logical point into platform-native global
/// coordinates. `screens` pairs each stable screen id with its board-space
/// geometry; the id is currently informational (containment is geometric)
/// and keeps the call sites free of an extra entry type.
///
/// Resolution order:
/// 1. A screen whose board rect contains the point wins (first match keeps
///    the result deterministic for degenerate overlapping layouts).
/// 2. Otherwise the screen whose *center* is nearest to the point is chosen
///    (ties keep the earlier entry) and the point is projected (clamped)
///    onto that screen's rect, so an off-screen target still lands on a
///    real pixel instead of nowhere.
///
/// Native mapping: `native = screen.native_origin + (board - screen.origin)
/// * scale`. On macOS scale is 1 and native == board (CG global points);
/// on Windows scale converts logical points to physical pixels.
/// Returns `None` only when the list is empty or has no usable screen.
pub fn board_to_native(
    screens: &[(String, OverlayScreenGeometry)],
    board_x: f64,
    board_y: f64,
) -> Option<(f64, f64)> {
    let effective_scale = |g: &OverlayScreenGeometry| {
        if g.scale.is_finite() && g.scale > 0.0 {
            g.scale
        } else {
            1.0
        }
    };
    let usable = |g: &OverlayScreenGeometry| {
        g.width.is_finite() && g.width > 0.0 && g.height.is_finite() && g.height > 0.0
    };

    let mut nearest: Option<(usize, f64)> = None;
    for (index, (_, geometry)) in screens.iter().enumerate() {
        if !usable(geometry) {
            continue;
        }
        let inside = board_x >= geometry.x
            && board_x <= geometry.x + geometry.width
            && board_y >= geometry.y
            && board_y <= geometry.y + geometry.height;
        if inside {
            let scale = effective_scale(geometry);
            return Some((
                geometry.native_x + (board_x - geometry.x) * scale,
                geometry.native_y + (board_y - geometry.y) * scale,
            ));
        }
        let cx = geometry.x + geometry.width / 2.0;
        let cy = geometry.y + geometry.height / 2.0;
        let dist = (cx - board_x).hypot(cy - board_y);
        // Strict `<` keeps the first screen on ties (deterministic order).
        if nearest.map(|(_, best)| dist < best).unwrap_or(true) {
            nearest = Some((index, dist));
        }
    }
    let (index, _) = nearest?;
    let geometry = &screens[index].1;
    let scale = effective_scale(geometry);
    let px = board_x.clamp(geometry.x, geometry.x + geometry.width);
    let py = board_y.clamp(geometry.y, geometry.y + geometry.height);
    Some((
        geometry.native_x + (px - geometry.x) * scale,
        geometry.native_y + (py - geometry.y) * scale,
    ))
}

/// Test fake: records every warp target instead of touching the OS. `Clone`
/// shares the call log, so a test keeps a handle while the engine owns the
/// `Box<dyn CursorWarper>`.
#[derive(Debug, Clone, Default)]
pub struct WarpRecorder {
    calls: std::sync::Arc<std::sync::Mutex<Vec<(f64, f64)>>>,
    succeed: bool,
}

impl WarpRecorder {
    pub fn new(succeed: bool) -> Self {
        WarpRecorder {
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            succeed,
        }
    }

    /// Every warp call so far, oldest first.
    pub fn calls(&self) -> Vec<(f64, f64)> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }

    /// Number of warp calls so far.
    pub fn warp_count(&self) -> usize {
        self.calls.lock().map(|calls| calls.len()).unwrap_or(0)
    }
}

impl CursorWarper for WarpRecorder {
    fn warp(&mut self, native_x: f64, native_y: f64) -> bool {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((native_x, native_y));
        }
        self.succeed
    }
}

/// Build the platform cursor warper for the engine.
pub fn create_cursor_warper() -> Box<dyn CursorWarper> {
    #[cfg(target_os = "macos")]
    return Box::new(MacCursorWarper);
    #[cfg(target_os = "windows")]
    return Box::new(WinCursorWarper);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Box::new(NoopCursorWarper)
}

// ---------------------------------------------------------------------------
// macOS: CGWarpMouseCursorPosition in CG global points (identity scale).
//
// Deliberately does NOT call CGSetLocalEventsSuppressionInterval: macOS
// suppresses local hardware mouse events for 0.25 s after a warp by default,
// and that suppression is exactly what we want here — it swallows the burst
// of hardware deltas the user's still-moving physical mouse would generate
// right after the warp, so the cursor does not immediately drift back off
// the target screen. input.rs only zeroes that interval while remote control
// is active and restores the default on exit, so the two features never
// fight over the process-wide setting.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[derive(Debug, Default, Clone, Copy)]
pub struct MacCursorWarper;

#[cfg(target_os = "macos")]
impl CursorWarper for MacCursorWarper {
    fn warp(&mut self, native_x: f64, native_y: f64) -> bool {
        core_graphics::display::CGDisplay::warp_mouse_cursor_position(
            core_graphics::geometry::CGPoint::new(native_x, native_y),
        )
        .is_ok()
    }
}

// ---------------------------------------------------------------------------
// Windows: SetCursorPos in physical pixels (the process is per-monitor DPI
// aware, mirroring input.rs's cursor handling).
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[derive(Debug, Default, Clone, Copy)]
pub struct WinCursorWarper;

#[cfg(target_os = "windows")]
impl CursorWarper for WinCursorWarper {
    fn warp(&mut self, native_x: f64, native_y: f64) -> bool {
        let x = native_x.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        let y = native_y.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) != 0 }
    }
}

// ---------------------------------------------------------------------------
// Other platforms: the halo feature itself is macOS/Windows only.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct NoopCursorWarper;

impl CursorWarper for NoopCursorWarper {
    fn warp(&mut self, _native_x: f64, _native_y: f64) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(
        id: &str,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> (String, OverlayScreenGeometry) {
        (
            id.to_string(),
            OverlayScreenGeometry {
                x,
                y,
                width,
                height,
                scale: 1.0,
                native_x: x,
                native_y: y,
            },
        )
    }

    #[test]
    fn board_to_native_inside_single_screen_is_identity() {
        let screens = vec![screen("a", 0.0, 0.0, 1000.0, 500.0)];
        assert_eq!(
            board_to_native(&screens, 500.0, 250.0),
            Some((500.0, 250.0))
        );
        // Edges count as contained.
        assert_eq!(board_to_native(&screens, 0.0, 0.0), Some((0.0, 0.0)));
        assert_eq!(
            board_to_native(&screens, 1000.0, 500.0),
            Some((1000.0, 500.0))
        );
    }

    #[test]
    fn board_to_native_multi_screen_picks_containing_screen() {
        let screens = vec![
            screen("a", 0.0, 0.0, 1000.0, 500.0),
            screen("b", 1000.0, 0.0, 1000.0, 500.0),
        ];
        assert_eq!(
            board_to_native(&screens, 1500.0, 250.0),
            Some((1500.0, 250.0))
        );
        assert_eq!(
            board_to_native(&screens, 999.0, 250.0),
            Some((999.0, 250.0))
        );
        assert_eq!(board_to_native(&screens, 1000.0, 0.0), Some((1000.0, 0.0)));
    }

    #[test]
    fn board_to_native_outside_projects_onto_nearest_screen_center() {
        let screens = vec![
            screen("a", 0.0, 0.0, 1000.0, 500.0),
            screen("b", 1000.0, 0.0, 1000.0, 500.0),
        ];
        // Below screen b: nearest center is b's (1500, 250); the point is
        // clamped onto b's bottom edge.
        assert_eq!(
            board_to_native(&screens, 1500.0, 900.0),
            Some((1500.0, 500.0))
        );
        // Left of screen a, vertically between the two: both centers are 500
        // away horizontally... a's center (500, 250) is 500.25 away, b's
        // (1500, 250) is 1500.75: a wins and the point clamps to a's left
        // edge.
        assert_eq!(board_to_native(&screens, -50.0, 249.0), Some((0.0, 249.0)));
    }

    #[test]
    fn board_to_native_applies_scale_to_native_origin() {
        // Windows-style: logical 1000x500 board rect, native physical pixels
        // at 2x with a non-zero native origin.
        let screens = vec![(
            "hidpi".to_string(),
            OverlayScreenGeometry {
                x: 0.0,
                y: 0.0,
                width: 1000.0,
                height: 500.0,
                scale: 2.0,
                native_x: 100.0,
                native_y: 60.0,
            },
        )];
        assert_eq!(
            board_to_native(&screens, 500.0, 250.0),
            Some((1100.0, 560.0))
        );
        assert_eq!(board_to_native(&screens, 0.0, 0.0), Some((100.0, 60.0)));
    }

    #[test]
    fn board_to_native_guards_degenerate_geometry() {
        assert_eq!(board_to_native(&[], 0.0, 0.0), None, "empty list");
        // Zero-sized screens are unusable.
        let screens = vec![screen("dot", 0.0, 0.0, 0.0, 0.0)];
        assert_eq!(board_to_native(&screens, 0.0, 0.0), None);
        // Non-positive or non-finite scale falls back to identity.
        let screens = vec![(
            "bad-scale".to_string(),
            OverlayScreenGeometry {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
                scale: 0.0,
                native_x: 0.0,
                native_y: 0.0,
            },
        )];
        assert_eq!(board_to_native(&screens, 50.0, 50.0), Some((50.0, 50.0)));
    }

    #[test]
    fn warp_recorder_records_every_call() {
        let recorder = WarpRecorder::new(true);
        let handle = recorder.clone();
        let mut warper: Box<dyn CursorWarper> = Box::new(recorder);
        assert!(warper.warp(1.0, 2.0));
        assert!(warper.warp(3.0, 4.0));
        assert_eq!(handle.calls(), vec![(1.0, 2.0), (3.0, 4.0)]);
        assert_eq!(handle.warp_count(), 2);
    }

    #[test]
    fn warp_recorder_can_report_failure() {
        let mut warper: Box<dyn CursorWarper> = Box::new(WarpRecorder::new(false));
        assert!(!warper.warp(5.0, 6.0));
    }
}
