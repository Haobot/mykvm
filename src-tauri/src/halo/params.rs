//! FocusHalo settings. Old configs lack these keys, so every field carries a
//! `#[serde(default)]`; `sanitized()` clamps values back into the documented
//! ranges so a hand-edited config can never push the engine out of bounds.

use serde::{Deserialize, Serialize};

const DWELL_MS: (u32, u32) = (600, 2000);
const SETTLE_MS: (u32, u32) = (80, 400);
const ENTER_SPEED: (f64, f64) = (0.05, 0.4);
const COOLDOWN_MS: (u32, u32) = (300, 1500);
const UNDO_MS: (u32, u32) = (1000, 10000);
const DEAD_ZONE_PERCENT: (f64, f64) = (3.0, 8.0);
const GAIN: (f64, f64) = (0.5, 2.0);
const HALO_DIAMETER: (f64, f64) = (140.0, 180.0);
const HALO_OPACITY: (f64, f64) = (0.35, 0.9);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchZone {
    Anywhere,
    TitleBar,
}

impl Default for SwitchZone {
    fn default() -> Self {
        SwitchZone::Anywhere
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraPosition {
    Top,
    Bottom,
}

impl Default for CameraPosition {
    fn default() -> Self {
        CameraPosition::Top
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusHaloSettings {
    #[serde(default = "d_enabled")]
    pub enabled: bool,
    #[serde(default = "d_dwell_ms")]
    pub dwell_ms: u32,
    #[serde(default = "d_settle_ms")]
    pub settle_ms: u32,
    #[serde(default = "d_enter_speed")]
    pub enter_speed: f64,
    #[serde(default = "d_cooldown_ms")]
    pub cooldown_ms: u32,
    #[serde(default = "d_undo_ms")]
    pub undo_ms: u32,
    #[serde(default = "d_dead_zone_percent")]
    pub dead_zone_percent: f64,
    #[serde(default = "d_gain")]
    pub gain: f64,
    #[serde(default = "d_halo_diameter")]
    pub halo_diameter: f64,
    #[serde(default = "d_halo_opacity")]
    pub halo_opacity: f64,
    #[serde(default = "d_show_progress")]
    pub show_progress: bool,
    #[serde(default)]
    pub switch_zone: SwitchZone,
    #[serde(default)]
    pub camera_position: CameraPosition,
    #[serde(default = "d_keyboard_suspends")]
    pub keyboard_suspends: bool,
    #[serde(default)]
    pub title_blacklist: Vec<String>,
    #[serde(default)]
    pub process_blacklist: Vec<String>,
    #[serde(default = "d_emergency_stop_hotkey")]
    pub emergency_stop_hotkey: String,
    #[serde(default = "d_undo_hotkey")]
    pub undo_hotkey: String,
    #[serde(default = "d_recenter_hotkey")]
    pub recenter_hotkey: String,
}

fn d_enabled() -> bool {
    false
}
fn d_dwell_ms() -> u32 {
    900
}
fn d_settle_ms() -> u32 {
    150
}
fn d_enter_speed() -> f64 {
    0.15
}
fn d_cooldown_ms() -> u32 {
    500
}
fn d_undo_ms() -> u32 {
    5000
}
fn d_dead_zone_percent() -> f64 {
    5.0
}
fn d_gain() -> f64 {
    1.0
}
fn d_halo_diameter() -> f64 {
    160.0
}
fn d_halo_opacity() -> f64 {
    0.55
}
fn d_show_progress() -> bool {
    true
}
fn d_keyboard_suspends() -> bool {
    true
}
fn d_emergency_stop_hotkey() -> String {
    "disabled".into()
}
fn d_undo_hotkey() -> String {
    "disabled".into()
}
fn d_recenter_hotkey() -> String {
    "disabled".into()
}

impl Default for FocusHaloSettings {
    fn default() -> Self {
        FocusHaloSettings {
            enabled: d_enabled(),
            dwell_ms: d_dwell_ms(),
            settle_ms: d_settle_ms(),
            enter_speed: d_enter_speed(),
            cooldown_ms: d_cooldown_ms(),
            undo_ms: d_undo_ms(),
            dead_zone_percent: d_dead_zone_percent(),
            gain: d_gain(),
            halo_diameter: d_halo_diameter(),
            halo_opacity: d_halo_opacity(),
            show_progress: d_show_progress(),
            switch_zone: SwitchZone::default(),
            camera_position: CameraPosition::default(),
            keyboard_suspends: d_keyboard_suspends(),
            title_blacklist: Vec::new(),
            process_blacklist: Vec::new(),
            emergency_stop_hotkey: d_emergency_stop_hotkey(),
            undo_hotkey: d_undo_hotkey(),
            recenter_hotkey: d_recenter_hotkey(),
        }
    }
}

impl FocusHaloSettings {
    /// Clamp every numeric field into its documented range so stale or
    /// hand-edited configs cannot produce nonsensical engine timing or a
    /// broken overlay size.
    pub fn sanitized(&self) -> Self {
        FocusHaloSettings {
            enabled: self.enabled,
            dwell_ms: self.dwell_ms.clamp(DWELL_MS.0, DWELL_MS.1),
            settle_ms: self.settle_ms.clamp(SETTLE_MS.0, SETTLE_MS.1),
            enter_speed: self.enter_speed.clamp(ENTER_SPEED.0, ENTER_SPEED.1),
            cooldown_ms: self.cooldown_ms.clamp(COOLDOWN_MS.0, COOLDOWN_MS.1),
            undo_ms: self.undo_ms.clamp(UNDO_MS.0, UNDO_MS.1),
            dead_zone_percent: self
                .dead_zone_percent
                .clamp(DEAD_ZONE_PERCENT.0, DEAD_ZONE_PERCENT.1),
            gain: self.gain.clamp(GAIN.0, GAIN.1),
            halo_diameter: self.halo_diameter.clamp(HALO_DIAMETER.0, HALO_DIAMETER.1),
            halo_opacity: self.halo_opacity.clamp(HALO_OPACITY.0, HALO_OPACITY.1),
            show_progress: self.show_progress,
            switch_zone: self.switch_zone,
            camera_position: self.camera_position,
            keyboard_suspends: self.keyboard_suspends,
            title_blacklist: self.title_blacklist.clone(),
            process_blacklist: self.process_blacklist.clone(),
            emergency_stop_hotkey: self.emergency_stop_hotkey.clone(),
            undo_hotkey: self.undo_hotkey.clone(),
            recenter_hotkey: self.recenter_hotkey.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let s = FocusHaloSettings::default();
        assert!(!s.enabled);
        assert_eq!(s.dwell_ms, 900);
        assert_eq!(s.settle_ms, 150);
        assert_eq!(s.enter_speed, 0.15);
        assert_eq!(s.cooldown_ms, 500);
        assert_eq!(s.undo_ms, 5000);
        assert_eq!(s.dead_zone_percent, 5.0);
        assert_eq!(s.gain, 1.0);
        assert_eq!(s.halo_diameter, 160.0);
        assert_eq!(s.halo_opacity, 0.55);
        assert!(s.show_progress);
        assert_eq!(s.switch_zone, SwitchZone::Anywhere);
        assert_eq!(s.camera_position, CameraPosition::Top);
        assert!(s.keyboard_suspends);
        assert!(s.title_blacklist.is_empty());
        assert!(s.process_blacklist.is_empty());
        assert_eq!(s.emergency_stop_hotkey, "disabled");
        assert_eq!(s.undo_hotkey, "disabled");
        assert_eq!(s.recenter_hotkey, "disabled");
    }

    #[test]
    fn sanitized_clamps_every_numeric_field() {
        let s = FocusHaloSettings {
            dwell_ms: 999_999,
            settle_ms: 0,
            enter_speed: 5.0,
            cooldown_ms: 0,
            undo_ms: 999_999,
            dead_zone_percent: 99.0,
            gain: 10.0,
            halo_diameter: 500.0,
            halo_opacity: 2.0,
            ..FocusHaloSettings::default()
        };
        let s = s.sanitized();
        assert_eq!(s.dwell_ms, 2000);
        assert_eq!(s.settle_ms, 80);
        assert_eq!(s.enter_speed, 0.4);
        assert_eq!(s.cooldown_ms, 300);
        assert_eq!(s.undo_ms, 10000);
        assert_eq!(s.dead_zone_percent, 8.0);
        assert_eq!(s.gain, 2.0);
        assert_eq!(s.halo_diameter, 180.0);
        assert_eq!(s.halo_opacity, 0.9);
    }

    #[test]
    fn sanitized_keeps_in_range_values_untouched() {
        let s = FocusHaloSettings {
            dwell_ms: 600,
            settle_ms: 400,
            enter_speed: 0.05,
            gain: 0.5,
            halo_opacity: 0.35,
            ..FocusHaloSettings::default()
        };
        let s = s.sanitized();
        assert_eq!(s.dwell_ms, 600);
        assert_eq!(s.settle_ms, 400);
        assert_eq!(s.enter_speed, 0.05);
        assert_eq!(s.gain, 0.5);
        assert_eq!(s.halo_opacity, 0.35);
    }

    #[test]
    fn json_round_trip_uses_camel_case_keys() {
        let s = FocusHaloSettings {
            enabled: true,
            dwell_ms: 1200,
            switch_zone: SwitchZone::TitleBar,
            camera_position: CameraPosition::Bottom,
            ..FocusHaloSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        for key in [
            "enabled",
            "dwellMs",
            "settleMs",
            "enterSpeed",
            "cooldownMs",
            "undoMs",
            "deadZonePercent",
            "gain",
            "haloDiameter",
            "haloOpacity",
            "showProgress",
            "switchZone",
            "cameraPosition",
            "keyboardSuspends",
            "titleBlacklist",
            "processBlacklist",
            "emergencyStopHotkey",
            "undoHotkey",
            "recenterHotkey",
        ] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing key {key} in {json}"
            );
        }
        assert!(json.contains("\"titlebar\""));
        assert!(json.contains("\"bottom\""));
        let back: FocusHaloSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn legacy_config_without_new_fields_equals_default() {
        let legacy: FocusHaloSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, FocusHaloSettings::default());
    }
}
