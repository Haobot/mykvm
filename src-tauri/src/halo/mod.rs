//! FocusHalo P0: settings plus the native overlay shell. The dwell/switch
//! engine arrives in a later phase and will drive `HaloRuntime` at 60 fps;
//! today the runtime only exposes enough surface for manual testing of the
//! overlay backends.

// Nothing outside this tree constructs HaloRuntime yet, so dead_code fires
// on every item until the P1 engine wires it up.
#![allow(dead_code)]

pub mod overlay;
pub mod params;

pub use overlay::{HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry};
pub use params::FocusHaloSettings;

/// Owns the one halo overlay window for the process. P0 is deliberately
/// thread-free: every call just forwards to the platform backend, which
/// marshals to its own UI thread.
pub struct HaloRuntime {
    settings: FocusHaloSettings,
    overlay: Option<Box<dyn OverlayBackend>>,
    /// halo_diameter the current overlay was built with; changing it rebuilds
    /// the window so its pixel size follows the setting.
    built_diameter: f64,
    geometry: Option<OverlayScreenGeometry>,
}

impl HaloRuntime {
    /// Empty runtime: no window, no thread, nothing platform-specific.
    pub fn new() -> Self {
        HaloRuntime {
            settings: FocusHaloSettings::default(),
            overlay: None,
            built_diameter: 0.0,
            geometry: None,
        }
    }

    /// Store a sanitized settings snapshot. Deliberately does not touch the
    /// platform: the overlay (and on Windows its thread) is only created by
    /// the first show, so a disabled halo keeps the process at zero cost.
    pub fn configure(&mut self, settings: FocusHaloSettings) {
        self.settings = settings.sanitized();
    }

    /// Create the backend on first use, or when the configured diameter
    /// changed (the window's pixel size must follow the setting).
    fn ensure_overlay(&mut self) {
        if !overlay::overlay_supported() {
            return;
        }
        let diameter = self.settings.halo_diameter;
        let needs_build =
            self.overlay.is_none() || (self.built_diameter - diameter).abs() > f64::EPSILON;
        if !needs_build {
            return;
        }
        self.overlay = Some(overlay::create_overlay(&self.settings));
        self.built_diameter = diameter;
        // Carry the already-known screen over so the new window lands
        // correctly on the first update.
        if let Some(geometry) = self.geometry {
            if let Some(overlay) = self.overlay.as_mut() {
                overlay.set_screen(geometry);
            }
        }
    }

    pub fn set_screen(&mut self, geometry: OverlayScreenGeometry) {
        self.geometry = Some(geometry);
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_screen(geometry);
        }
    }

    /// P0 test hook: draw one halo frame at a screen-local logical point.
    pub fn show_test_halo(&mut self, local: (f64, f64), visual: HaloVisual) {
        self.ensure_overlay();
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        overlay.show();
        overlay.update(
            local,
            &HaloFrame {
                visual,
                opacity: self.settings.halo_opacity,
            },
        );
    }

    pub fn hide(&mut self) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.hide();
        }
    }
}

impl Default for HaloRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_runtime_starts_empty_and_calls_are_safe() {
        let mut runtime = HaloRuntime::new();
        // No overlay yet: every path must be a no-op without panicking.
        runtime.hide();
        runtime.show_test_halo((10.0, 10.0), HaloVisual::Idle);
        assert!(runtime.geometry.is_none());
    }

    #[test]
    fn configure_sanitizes_the_snapshot() {
        let mut runtime = HaloRuntime::new();
        runtime.configure(FocusHaloSettings {
            halo_diameter: 999.0,
            halo_opacity: 5.0,
            ..FocusHaloSettings::default()
        });
        assert_eq!(runtime.settings.halo_diameter, 180.0);
        assert_eq!(runtime.settings.halo_opacity, 0.9);
    }

    #[test]
    fn unsupported_platform_keeps_runtime_inert() {
        if overlay::overlay_supported() {
            return;
        }
        let mut runtime = HaloRuntime::new();
        runtime.configure(FocusHaloSettings::default());
        runtime.show_test_halo((0.0, 0.0), HaloVisual::SwitchPulse);
        runtime.hide();
        // Still inert: the factory returned a NoopBackend, nothing to assert
        // beyond "did not panic".
    }
}
