//! FocusHalo settings. Old configs lack these keys, so every field carries a
//! `#[serde(default)]`; `sanitized()` clamps values back into the documented
//! ranges so a hand-edited config can never push the engine out of bounds.

use serde::{Deserialize, Serialize};

use crate::halo::mapping::CalibrationProfile;
use crate::halo::screen_map;

const DWELL_MS: (u32, u32) = (600, 2000);
const SETTLE_MS: (u32, u32) = (80, 400);
const ENTER_SPEED: (f64, f64) = (0.05, 0.4);
const COOLDOWN_MS: (u32, u32) = (300, 1500);
const UNDO_MS: (u32, u32) = (1000, 10000);
const DEAD_ZONE_PERCENT: (f64, f64) = (3.0, 8.0);
const GAIN: (f64, f64) = (0.5, 2.0);
const HALO_DIAMETER: (f64, f64) = (140.0, 180.0);
const HALO_OPACITY: (f64, f64) = (0.35, 0.9);
const SMOOTH_MS: (u32, u32) = (80, 400);
/// Hysteresis window for the turn-head region switch: too small chatters on
/// borderline poses, too large feels laggy.
const REGION_SETTLE_MS: (u32, u32) = (150, 800);
/// Dilatation factor applied around a region's centroid when the pose misses
/// every region exactly (1.0 = no tolerance growth).
const REGION_MARGIN: (f64, f64) = (1.0, 1.5);
/// Seat-translation compensation, deg per normalized unit of face-center
/// shift; 0 disables, ~120 is already an extreme wide FOV.
const TRANS_ALPHA: (f64, f64) = (0.0, 120.0);

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
    /// Render-servo settle time (ms): smaller = snappier, larger = calmer.
    #[serde(default = "d_smooth_ms")]
    pub smooth_ms: u32,
    #[serde(default = "d_show_progress")]
    pub show_progress: bool,
    #[serde(default)]
    pub switch_zone: SwitchZone,
    #[serde(default)]
    pub camera_position: CameraPosition,
    /// User-facing vertical correction: flip the pitch axis when the halo
    /// still moves opposite to the gaze (unusual cameras/mounts). Composes
    /// with camera_position as an XOR in the mapping layer.
    #[serde(default)]
    pub invert_pitch: bool,
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
    /// Per-user head-pose calibration captured by the settings UI. `None`
    /// means "not calibrated yet": the mapping falls back to the default
    /// seated-range profile, so an uncalibrated halo still renders (it just
    /// does not track the user's personal ranges).
    #[serde(default)]
    pub calibration: Option<CalibrationProfile>,
    /// Turn-head-to-switch-screen master switch ("转头切屏"). Stored from S1
    /// on so the settings schema is stable; the engine consumes it in S2.
    #[serde(default)]
    pub screen_warp_enabled: bool,
    /// How long a new region attribution must persist before it commits
    /// (hysteresis against borderline poses), in ms.
    #[serde(default = "d_region_settle_ms")]
    pub region_settle_ms: u32,
    /// Dilatation factor applied around a region's centroid when the pose
    /// misses every region exactly (1.0 = no tolerance growth).
    #[serde(default = "d_region_margin")]
    pub region_margin: f64,
    /// Seat-translation compensation coefficients (deg per normalized unit of
    /// face-center shift). Fitted by the optional lean-capture step; the
    /// default is the geometric prior for a typical webcam FOV.
    #[serde(default = "d_trans_alpha_yaw")]
    pub trans_alpha_yaw: f64,
    #[serde(default = "d_trans_alpha_pitch")]
    pub trans_alpha_pitch: f64,
    /// Per-screen pose quads captured by the settings UI; `None` until the
    /// first multi-screen calibration completes. Passed through sanitization
    /// untouched (same reasoning as `calibration`).
    #[serde(default)]
    pub screen_calibration: Option<screen_map::ScreenCalibration>,
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
fn d_smooth_ms() -> u32 {
    140
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
fn d_region_settle_ms() -> u32 {
    300
}
fn d_region_margin() -> f64 {
    1.15
}
fn d_trans_alpha_yaw() -> f64 {
    60.0
}
fn d_trans_alpha_pitch() -> f64 {
    60.0
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
            smooth_ms: d_smooth_ms(),
            show_progress: d_show_progress(),
            switch_zone: SwitchZone::default(),
            camera_position: CameraPosition::default(),
            invert_pitch: false,
            keyboard_suspends: d_keyboard_suspends(),
            title_blacklist: Vec::new(),
            process_blacklist: Vec::new(),
            emergency_stop_hotkey: d_emergency_stop_hotkey(),
            undo_hotkey: d_undo_hotkey(),
            recenter_hotkey: d_recenter_hotkey(),
            calibration: None,
            screen_warp_enabled: false,
            region_settle_ms: d_region_settle_ms(),
            region_margin: d_region_margin(),
            trans_alpha_yaw: d_trans_alpha_yaw(),
            trans_alpha_pitch: d_trans_alpha_pitch(),
            screen_calibration: None,
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
            smooth_ms: self.smooth_ms.clamp(SMOOTH_MS.0, SMOOTH_MS.1),
            show_progress: self.show_progress,
            switch_zone: self.switch_zone,
            camera_position: self.camera_position,
            invert_pitch: self.invert_pitch,
            keyboard_suspends: self.keyboard_suspends,
            title_blacklist: self.title_blacklist.clone(),
            process_blacklist: self.process_blacklist.clone(),
            emergency_stop_hotkey: self.emergency_stop_hotkey.clone(),
            undo_hotkey: self.undo_hotkey.clone(),
            recenter_hotkey: self.recenter_hotkey.clone(),
            // Calibration values are degree readings written only by the
            // capture flow, which validates monotonicity on finish; there is
            // nothing sane to clamp them against here, so pass through.
            calibration: self.calibration,
            screen_warp_enabled: self.screen_warp_enabled,
            region_settle_ms: self
                .region_settle_ms
                .clamp(REGION_SETTLE_MS.0, REGION_SETTLE_MS.1),
            region_margin: self.region_margin.clamp(REGION_MARGIN.0, REGION_MARGIN.1),
            trans_alpha_yaw: self.trans_alpha_yaw.clamp(TRANS_ALPHA.0, TRANS_ALPHA.1),
            trans_alpha_pitch: self.trans_alpha_pitch.clamp(TRANS_ALPHA.0, TRANS_ALPHA.1),
            // Same reasoning as `calibration`: captured pose quads are
            // validated at capture time and have no sane clamp here.
            screen_calibration: self.screen_calibration.clone(),
        }
    }

    /// The calibration profile the mapping layer should use: the captured one
    /// when present, the default seated-range profile otherwise.
    pub fn effective_calibration(&self) -> CalibrationProfile {
        self.calibration.unwrap_or_default()
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
        assert_eq!(s.calibration, None);
        assert!(!s.screen_warp_enabled);
        assert_eq!(s.region_settle_ms, 300);
        assert_eq!(s.region_margin, 1.15);
        assert_eq!(s.trans_alpha_yaw, 60.0);
        assert_eq!(s.trans_alpha_pitch, 60.0);
        assert_eq!(s.screen_calibration, None);
        assert_eq!(
            s.effective_calibration(),
            CalibrationProfile::default(),
            "uncalibrated settings fall back to the default profile"
        );
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
            region_settle_ms: 0,
            region_margin: 9.9,
            trans_alpha_yaw: -5.0,
            trans_alpha_pitch: 999.0,
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
        assert_eq!(s.region_settle_ms, 150);
        assert_eq!(s.region_margin, 1.5);
        assert_eq!(s.trans_alpha_yaw, 0.0);
        assert_eq!(s.trans_alpha_pitch, 120.0);
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
            "calibration",
            "screenWarpEnabled",
            "regionSettleMs",
            "regionMargin",
            "transAlphaYaw",
            "transAlphaPitch",
            "screenCalibration",
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

    #[test]
    fn calibration_field_round_trips_and_survives_sanitize() {
        let cal = CalibrationProfile {
            center_yaw: -1.5,
            center_pitch: 0.5,
            left_yaw: -16.0,
            right_yaw: 14.0,
            up_pitch: 9.0,
            down_pitch: 11.0,
        };
        let s = FocusHaloSettings {
            calibration: Some(cal),
            ..FocusHaloSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"centerYaw\":-1.5"),
            "calibration must serialize nested camelCase: {json}"
        );
        let back: FocusHaloSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.calibration, Some(cal));
        // sanitize() is applied on every save_layout; calibration passes
        // through unchanged.
        assert_eq!(s.sanitized().calibration, Some(cal));
        assert_eq!(s.effective_calibration(), cal);
    }

    #[test]
    fn screen_calibration_field_round_trips_and_survives_sanitize() {
        use crate::halo::screen_map::{BoardRect, PosePoint, ScreenRegion};

        let cal = screen_map::ScreenCalibration {
            captured_at_ms: 1_700_000_000_000,
            regions: vec![ScreenRegion {
                screen_id: "hdmi-1".into(),
                board: BoardRect {
                    screen_id: "hdmi-1".into(),
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                tl: PosePoint {
                    yaw: -12.0,
                    pitch: 9.0,
                    u: 0.3,
                    v: 0.2,
                },
                tr: PosePoint {
                    yaw: 13.0,
                    pitch: 8.0,
                    u: 0.7,
                    v: 0.2,
                },
                br: PosePoint {
                    yaw: 11.0,
                    pitch: -7.0,
                    u: 0.7,
                    v: 0.8,
                },
                bl: PosePoint {
                    yaw: -10.0,
                    pitch: -6.0,
                    u: 0.3,
                    v: 0.8,
                },
            }],
            ref_u: 0.5,
            ref_v: 0.5,
            trans_alpha_yaw: 88.0,
            trans_alpha_pitch: 72.0,
            ..screen_map::ScreenCalibration::default()
        };
        let s = FocusHaloSettings {
            screen_calibration: Some(cal.clone()),
            ..FocusHaloSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"screenCalibration\""),
            "settings must serialize the calibration payload: {json}"
        );
        assert!(
            json.contains("\"transAlphaYaw\":88.0"),
            "nested calibration must serialize camelCase: {json}"
        );
        let back: FocusHaloSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.screen_calibration, Some(cal.clone()));
        // sanitize() is applied on every save_layout; the payload passes
        // through unchanged (validated at capture time, nothing to clamp).
        assert_eq!(s.sanitized().screen_calibration, Some(cal));
    }
}
