//! Head pose -> screen mapping: converts (yaw, pitch) degrees into normalized
//! screen coordinates in 0..=1, applying calibration ranges, a dead zone, and
//! an edge-gain shaping curve.

use serde::{Deserialize, Serialize};

/// Axis half-range values small enough to be a calibration typo rather than a
/// usable range; raw offsets beyond such a range clamp to the edge.
const MIN_HALF_RANGE: f64 = 1e-3;
/// Degree margin required between the captured center and the left/right
/// extremes (and for the up/down magnitudes) so float noise from a lazy
/// capture cannot pass as a monotonic calibration.
const CAPTURE_MIN_EXTENT_DEG: f64 = 0.05;

/// Per-user head pose calibration, in degrees, matching the 6DRepNet Euler
/// convention used by `crate::halo::tracker` (positive pitch = looking up).
///
/// Sign conventions: `center_*` are pose readings; `right_yaw` and
/// `left_yaw` are absolute yaw readings at the gaze extremes (`left_yaw` is
/// negative by convention); `up_pitch` / `down_pitch` are positive magnitudes
/// of the up/down ranges relative to the center reading (up limit = center +
/// up_pitch, down limit = center - down_pitch).
///
/// The defaults are typical seated ranges measured from front-facing-camera
/// usage; they are overwritten by the user calibration flow (P2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CalibrationProfile {
    /// Pose reading that maps to screen center.
    pub center_yaw: f64,
    pub center_pitch: f64,
    /// Yaw reading at the user's extreme left gaze (negative by convention).
    pub left_yaw: f64,
    /// Yaw reading at the user's extreme right gaze.
    pub right_yaw: f64,
    /// Upward range magnitude above the center reading.
    pub up_pitch: f64,
    /// Downward range magnitude below the center reading.
    pub down_pitch: f64,
}

impl Default for CalibrationProfile {
    fn default() -> Self {
        // Typical human head-turn ranges when driving a whole desktop with
        // the head; to be replaced by per-user calibration (P2).
        CalibrationProfile {
            center_yaw: 0.0,
            center_pitch: 0.0,
            left_yaw: -18.0,
            right_yaw: 18.0,
            up_pitch: 12.0,
            down_pitch: 12.0,
        }
    }
}

/// Quadrant-aware linear normalization of a centered axis offset to -1..=1.
///
/// `half_pos` is the magnitude of the offset that means "+1" (right / up) and
/// `half_neg` the magnitude that means "-1" (left / down). Mis-calibrated
/// non-positive half-ranges clamp to the edge instead of dividing by zero.
fn normalize_axis(raw: f64, half_pos: f64, half_neg: f64) -> f64 {
    if raw >= 0.0 {
        if half_pos > MIN_HALF_RANGE {
            (raw / half_pos).clamp(0.0, 1.0)
        } else if raw > 0.0 {
            1.0
        } else {
            0.0
        }
    } else if half_neg > MIN_HALF_RANGE {
        (raw / half_neg).clamp(-1.0, 0.0)
    } else {
        -1.0
    }
}

/// Dead zone + edge gain for one normalized axis value (`n` in -1..=1).
///
/// - `dz` (0..1) is the dead-zone radius as a fraction of the half-range:
///   offsets within it output 0 (the halo stays glued to center). From the
///   dead-zone edge to the calibrated extreme the response ramps linearly
///   from 0 to 1, so there is no jump at the boundary.
/// - `gain` reshapes that ramp: >1 accelerates toward the edges (most screen
///   distance is covered near the calibrated extremes, center stays stable),
///   <1 favors precision near center, =1 linear.
pub(crate) fn shape_axis(n: f64, dz: f64, gain: f64) -> f64 {
    let dz = dz.clamp(0.0, 0.99);
    // gain <= 0 would flip the curve; fall back to linear.
    let gain = if gain <= 0.0 { 1.0 } else { gain };
    let mag = n.abs();
    if mag <= dz {
        return 0.0;
    }
    let t = ((mag - dz) / (1.0 - dz)).clamp(0.0, 1.0);
    t.powf(gain) * n.signum()
}

/// Map a (yaw, pitch) head pose in degrees to normalized screen coordinates
/// in 0..=1 (0 = left/top edge, 1 = right/bottom edge).
///
/// `dead_zone_percent` is expressed in percent of each axis half-range;
/// `gain > 1` accelerates near the screen edges, `< 1` favors center
/// precision, `= 1` linear. When `camera_bottom` is set (camera mounted below
/// the screen, e.g. a laptop bottom bar), pitch is inverted first because a
/// camera below the screen sees "looking up" with flipped geometry.
pub fn map_pose(
    pose: (f64, f64),
    cal: &CalibrationProfile,
    dead_zone_percent: f64,
    gain: f64,
    camera_bottom: bool,
) -> (f64, f64) {
    let (yaw, pitch) = pose;
    let raw_yaw = yaw - cal.center_yaw;
    let mut raw_pitch = pitch - cal.center_pitch;
    if camera_bottom {
        raw_pitch = -raw_pitch;
    }

    let yaw_n = normalize_axis(
        raw_yaw,
        cal.right_yaw - cal.center_yaw,
        cal.center_yaw - cal.left_yaw,
    );
    // Positive pitch = looking up: the up half-range is `up_pitch`, the down
    // half-range is `down_pitch` (both positive magnitudes; the down limit as
    // a pose reading is `center - down_pitch`).
    let pitch_n = normalize_axis(raw_pitch, cal.up_pitch, cal.down_pitch);

    let dz = dead_zone_percent / 100.0;
    let sx = shape_axis(yaw_n, dz, gain);
    let sy = shape_axis(pitch_n, dz, gain);

    let x = (0.5 + 0.5 * sx).clamp(0.0, 1.0);
    // Looking up (positive pitch) must move the halo up the screen, i.e. to
    // smaller y: `up_pitch` maps to y = 0.
    let y = (0.5 - 0.5 * sy).clamp(0.0, 1.0);
    (x, y)
}

/// Validate a freshly captured calibration profile: the yaw extremes must
/// straddle the center monotonically (left < center < right, each by at least
/// [`CAPTURE_MIN_EXTENT_DEG`]) and both pitch magnitudes must be positive by
/// the same margin. Error strings are user-facing (the calibration UI shows
/// them verbatim).
pub fn validate_calibration(cal: &CalibrationProfile) -> Result<(), String> {
    let margin = CAPTURE_MIN_EXTENT_DEG;
    if cal.left_yaw >= cal.center_yaw - margin {
        return Err("校准不完整：左转角度必须明显小于中心角度".into());
    }
    if cal.right_yaw <= cal.center_yaw + margin {
        return Err("校准不完整：右转角度必须明显大于中心角度".into());
    }
    if cal.up_pitch <= margin {
        return Err("校准不完整：抬头幅度必须大于 0".into());
    }
    if cal.down_pitch <= margin {
        return Err("校准不完整：低头幅度必须大于 0".into());
    }
    Ok(())
}

/// Shift a captured profile so the current head pose maps to screen center:
/// every absolute yaw/pitch reading moves by the same offset; the up/down
/// magnitudes are relative and stay untouched. This is the "recenter" flow.
pub fn recenter_calibration(cal: &CalibrationProfile, pose: (f64, f64)) -> CalibrationProfile {
    let (yaw, pitch) = pose;
    CalibrationProfile {
        center_yaw: yaw,
        center_pitch: pitch,
        left_yaw: cal.left_yaw + (yaw - cal.center_yaw),
        right_yaw: cal.right_yaw + (yaw - cal.center_yaw),
        up_pitch: cal.up_pitch,
        down_pitch: cal.down_pitch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cal() -> CalibrationProfile {
        CalibrationProfile::default()
    }

    #[test]
    fn center_maps_to_screen_center() {
        let (x, y) = map_pose((0.0, 0.0), &default_cal(), 5.0, 1.0, false);
        assert_eq!(x, 0.5);
        assert_eq!(y, 0.5);
    }

    #[test]
    fn four_calibrated_extremes_map_to_four_edges() {
        let cal = default_cal();
        assert_eq!(map_pose((cal.right_yaw, 0.0), &cal, 0.0, 1.0, false).0, 1.0);
        assert_eq!(map_pose((cal.left_yaw, 0.0), &cal, 0.0, 1.0, false).0, 0.0);
        assert_eq!(map_pose((0.0, cal.up_pitch), &cal, 0.0, 1.0, false).1, 0.0);
        assert_eq!(
            map_pose((0.0, -cal.down_pitch), &cal, 0.0, 1.0, false).1,
            1.0
        );
        // Extremes leave the other axis at center.
        assert_eq!(map_pose((cal.right_yaw, 0.0), &cal, 0.0, 1.0, false).1, 0.5);
        assert_eq!(map_pose((0.0, cal.up_pitch), &cal, 0.0, 1.0, false).0, 0.5);
    }

    #[test]
    fn dead_zone_glues_small_offsets_to_center() {
        let cal = default_cal();
        // 5% of the 18 deg half-range = 0.9 deg dead-zone radius.
        let (x, y) = map_pose((0.4, 0.0), &cal, 5.0, 1.0, false);
        assert_eq!(x, 0.5);
        assert_eq!(y, 0.5);
        let (x, y) = map_pose((-0.4, -0.3), &cal, 5.0, 1.0, false);
        assert_eq!(x, 0.5);
        assert_eq!(y, 0.5);
    }

    #[test]
    fn dead_zone_boundary_is_continuous() {
        let cal = default_cal();
        let dz_deg = 0.05 * 18.0; // 0.9 deg
        let eps = 1e-4;
        let below = map_pose((dz_deg - eps, 0.0), &cal, 5.0, 1.0, false).0;
        let above = map_pose((dz_deg + eps, 0.0), &cal, 5.0, 1.0, false).0;
        // Both sides of the boundary sit at the boundary value (0.5) and agree
        // to within the epsilon-induced slope.
        assert!((below - 0.5).abs() < 1e-5, "below boundary: {below}");
        assert!((above - 0.5).abs() < 1e-5, "above boundary: {above}");
        // And the response is strictly monotone just past the boundary.
        let near = map_pose((dz_deg + 0.05, 0.0), &cal, 5.0, 1.0, false).0;
        assert!(near > above);
    }

    #[test]
    fn gain_two_halves_midpoint_response() {
        // At normalized n = 0.5 (dz = 0), gain 2 must output 0.25: the edge
        // acceleration curve. Exercised through the full mapping: yaw = 9 deg
        // is exactly half of the 18 deg half-range -> x = 0.5 + 0.5 * 0.25.
        let cal = default_cal();
        let (x, _) = map_pose((cal.right_yaw * 0.5, 0.0), &cal, 0.0, 2.0, false);
        assert!(
            (x - 0.625).abs() < 1e-9,
            "x = {x}, expected 0.625 (sx = 0.25)"
        );
        // Direct check of the shaping helper for the exact 0.25 figure.
        assert_eq!(shape_axis(0.5, 0.0, 2.0), 0.25);
    }

    #[test]
    fn camera_bottom_flips_pitch_mapping() {
        let cal = default_cal();
        let top = map_pose((0.0, cal.up_pitch), &cal, 0.0, 1.0, false).1;
        let bottom = map_pose((0.0, cal.up_pitch), &cal, 0.0, 1.0, true).1;
        assert_eq!(top, 0.0);
        assert_eq!(bottom, 1.0);
    }

    #[test]
    fn calibration_json_round_trip_is_camel_case() {
        let cal = CalibrationProfile {
            center_yaw: -2.0,
            center_pitch: 1.5,
            left_yaw: -20.0,
            right_yaw: 16.0,
            up_pitch: 10.0,
            down_pitch: 14.0,
        };
        let json = serde_json::to_string(&cal).unwrap();
        for key in [
            "centerYaw",
            "centerPitch",
            "leftYaw",
            "rightYaw",
            "upPitch",
            "downPitch",
        ] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing {key} in {json}"
            );
        }
        let back: CalibrationProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cal);
    }

    #[test]
    fn empty_json_uses_defaults() {
        let cal: CalibrationProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(cal, CalibrationProfile::default());
    }

    #[test]
    fn default_profile_passes_validation_and_degenerate_profiles_fail() {
        assert_eq!(validate_calibration(&default_cal()), Ok(()));
        // Left not clearly left of center.
        let mut cal = default_cal();
        cal.left_yaw = cal.center_yaw;
        assert!(validate_calibration(&cal).is_err());
        // Right not clearly right of center.
        let mut cal = default_cal();
        cal.right_yaw = cal.center_yaw + 0.01;
        assert!(validate_calibration(&cal).is_err());
        // Non-positive up/down magnitudes.
        let mut cal = default_cal();
        cal.up_pitch = 0.0;
        assert!(validate_calibration(&cal).is_err());
        let mut cal = default_cal();
        cal.down_pitch = -1.0;
        assert!(validate_calibration(&cal).is_err());
    }

    #[test]
    fn recenter_shifts_every_absolute_reading() {
        let cal = CalibrationProfile {
            center_yaw: -2.0,
            center_pitch: 1.0,
            left_yaw: -20.0,
            right_yaw: 16.0,
            up_pitch: 10.0,
            down_pitch: 14.0,
        };
        // User now sits 3 deg to the left and 2 deg lower than when captured.
        let shifted = recenter_calibration(&cal, (-5.0, -1.0));
        assert_eq!(shifted.center_yaw, -5.0);
        assert_eq!(shifted.center_pitch, -1.0);
        assert_eq!(shifted.left_yaw, -23.0);
        assert_eq!(shifted.right_yaw, 13.0);
        // Magnitudes are relative and must not move.
        assert_eq!(shifted.up_pitch, 10.0);
        assert_eq!(shifted.down_pitch, 14.0);
        // The shifted profile keeps the same half-ranges, so the mapping still
        // reaches every edge.
        assert_eq!(validate_calibration(&shifted), Ok(()));
        let (x, y) = map_pose(
            (shifted.left_yaw, shifted.center_pitch),
            &shifted,
            0.0,
            1.0,
            false,
        );
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.5);
    }
}
