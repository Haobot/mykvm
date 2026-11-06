//! FocusHalo native overlay shell: the visual state model handed to the
//! platform backends, the screen geometry conversion contract, and the
//! backend factory. The dwell/switch engine (P1+) drives these backends at
//! 60 fps; P0 only needs show/hide/update to work for manual testing.

pub use crate::halo::params::FocusHaloSettings;

pub mod badge;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod win32;

pub use badge::{BadgeCorner, BadgeLayer, BadgeScreen};

/// What the halo ring should currently look like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HaloVisual {
    /// Cursor resting inside the dead zone: neutral thin ring.
    Idle,
    /// Dwell in progress: ring plus a progress arc swept `progress` (0.0..=1.0).
    Candidate { progress: f32 },
    /// Switch just fired: the ring flashes bright for a beat.
    SwitchPulse,
    /// Keyboard activity suspended the feature: neutral ring, dimmed.
    Suspended,
}

/// One frame for the halo. `opacity` is the final value after the engine has
/// adjusted the configured `halo_opacity` for the current state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HaloFrame {
    pub visual: HaloVisual,
    pub opacity: f64,
}

/// Candidate-window border in layout-board logical coordinates (the same
/// space as [`crate::halo::windows_layer::WindowInfo::frame`]). The breathing
/// phase itself is a per-backend concern: macOS runs a Core Animation
/// autoreverse loop, Windows modulates alpha in its 60 Hz timer tick.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct BorderFrame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Screen geometry in the engine's layout-board space plus the platform's
/// global native origin, so the backend never needs layout knowledge.
/// `PartialEq` (exact float equality) drives the engine's overlay re-host
/// deduplication: two calls with identical geometry are considered the same
/// screen, so only real topology changes re-host the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayScreenGeometry {
    /// Logical origin of this screen in layout-board space.
    pub x: f64,
    pub y: f64,
    /// Logical size of this screen.
    pub width: f64,
    pub height: f64,
    /// DPI factor (always 1.0-equivalent on macOS points).
    pub scale: f64,
    /// Platform global native origin of this screen:
    /// Windows physical pixels; macOS CG global points.
    pub native_x: f64,
    pub native_y: f64,
}

/// Backend owning one halo window. All calls may arrive from any thread;
/// backends marshal to their platform's UI thread themselves.
pub trait OverlayBackend: Send {
    fn show(&mut self);
    fn hide(&mut self);
    fn set_screen(&mut self, geometry: OverlayScreenGeometry);
    /// `local` is this screen's local logical coordinate (halo center point).
    fn update(&mut self, local: (f64, f64), frame: &HaloFrame);
    /// Candidate-window border: `Some(frame)` draws the breathing outline
    /// around that board-space rectangle, `None` hides it. Default no-op so
    /// minimal backends (NoopBackend) stay unchanged.
    fn set_border(&mut self, _border: Option<BorderFrame>) {}
    /// Show/hide the halo RING only (user preference; tracking and switching
    /// keep running). Border/highlight visuals are unaffected. Default no-op
    /// for minimal backends.
    fn set_ring_visible(&mut self, _visible: bool) {}
    /// Whether a border is currently wanted by the backend (test hook).
    fn border_visible(&self) -> bool {
        false
    }
}

/// macOS and Windows both have the primitives P0 needs; Linux has no
/// click-through per-pixel-alpha path wired up yet.
pub fn overlay_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

pub fn overlay_unsupported_reason() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("FocusHalo overlay is not available on this platform yet")
    } else {
        None
    }
}

/// P0 placeholder so Linux builds keep compiling and every runtime call is a
/// harmless no-op.
struct NoopBackend;

impl OverlayBackend for NoopBackend {
    fn show(&mut self) {}
    fn hide(&mut self) {}
    fn set_screen(&mut self, _geometry: OverlayScreenGeometry) {}
    fn update(&mut self, _local: (f64, f64), _frame: &HaloFrame) {}
}

pub fn create_overlay(settings: &FocusHaloSettings) -> Box<dyn OverlayBackend> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacOSOverlay::new(settings));
    #[cfg(target_os = "windows")]
    return Box::new(win32::Win32Overlay::start(settings));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = settings;
        Box::new(NoopBackend)
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn supported_platforms_report_support() {
        assert!(overlay_supported());
        assert_eq!(overlay_unsupported_reason(), None);
    }
}
