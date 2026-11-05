//! Multi-screen pose regions for the "turn your head to switch screen"
//! feature (S1: data model + pure math only; no engine, no UI).
//!
//! Each display is calibrated as a quadrilateral in head-pose space: the user
//! looks at a screen's four corners and the captured (yaw, pitch) pairs become
//! the quad corners. At runtime a pose is attributed to the screen whose quad
//! contains it (with a dilated fallback for borderline poses and a hysteresis
//! gate for stability), and an inverse-bilinear map turns the pose into a
//! position inside that screen. A first-order face-position term compensates
//! for the user shifting seat between calibration and use.
//!
//! # Coordinate conventions (the whole module depends on them)
//!
//! - Pose plane: `x = yaw` degrees, `y = pitch` degrees. **Positive yaw =
//!   looking right**, **positive pitch = looking up**, matching the
//!   conventions already verified in `mapping.rs`.
//! - Corner names are from the user's viewpoint looking AT the screen: `tl`
//!   is the screen's top-left corner. A valid quad therefore satisfies
//!   `tl.yaw < tr.yaw` and `bl.yaw < br.yaw` (the left corners sit at more
//!   negative yaw) plus `tl.pitch > bl.pitch` and `tr.pitch > br.pitch` (the
//!   top corners sit at more positive pitch). In this x-right / y-up plane
//!   the corner sequence TL -> TR -> BR -> BL runs clockwise, which the
//!   cross-product side tests below rely on (they are sign-agnostic, but the
//!   validity checks assume this ordering).
//! - Angles are stored post-flip: the capture flow applies the same
//!   `camera_position` / `invert_pitch` flip the runtime mapping uses before
//!   storing, so this module never re-applies sign handling.
//! - Face-center `u` / `v` are normalized 0..1 with the origin at the
//!   top-left of the camera image (v grows downward), as the YuNet detector
//!   reports them.
//!
//! Pure by design: no clock, no camera, no engine and no persistence live
//! here. The integration layer (S2) owns capture and per-tick driving.

use serde::{Deserialize, Serialize};

/// Smallest pose-space width/height a calibrated region may have, in degrees.
/// Anything narrower is a mis-capture (two corners taken at nearly the same
/// head pose) and cannot discriminate one screen from another.
pub const MIN_REGION_EXTENT_DEG: f64 = 1.0;

/// How far a stored board rect may drift from the live layout (logical px,
/// per coordinate) before the display topology counts as changed and the
/// pose calibration must be re-captured.
pub const TOPOLOGY_TOLERANCE_PX: f64 = 2.0;

/// Side-test tolerance for [`point_in_quad`]: a cross product this close to
/// zero means the point lies on the edge line, and that edge cannot vote for
/// either side.
const SIDE_EPS: f64 = 1e-9;

/// Convexity strictness for [`validate_region`]: edge crosses must be clearly
/// one-signed; a ~zero cross means collinear/degenerate corners.
const CONVEX_EPS: f64 = 1e-9;

/// Overlap gate for [`validate_region`]: a new region sampling more than this
/// fraction of its grid points inside an existing region means (part of) the
/// same screen was calibrated twice.
const OVERLAP_MAX_RATIO: f64 = 0.15;

/// Bilinear sampling grid resolution for the overlap ratio (n x n, corners
/// included, fully deterministic).
const OVERLAP_GRID: usize = 9;

/// Fixed-point iteration cap for the inverse bilinear map. Mild convex quads
/// converge in a handful of steps; the cap only bounds the worst case, and
/// the result is clamped to the unit square regardless.
const BILINEAR_MAX_ITERS: usize = 16;

/// One calibration capture point: filtered head-pose angles plus the
/// face-center position in the camera frame, normalized 0..1 with origin at
/// the top-left of the image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PosePoint {
    /// Head yaw in degrees (positive = looking right), post-flip convention.
    pub yaw: f64,
    /// Head pitch in degrees (positive = looking up), post-flip convention.
    pub pitch: f64,
    /// Face-center x in the camera frame, normalized 0..1 (left = 0).
    pub u: f64,
    /// Face-center y in the camera frame, normalized 0..1 (top = 0).
    pub v: f64,
}

/// Layout-board rect snapshot (logical px) taken at capture time; used to
/// detect display-topology changes before trusting the calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardRect {
    /// Stable screen identity so a moved-but-unchanged display still matches
    /// its captured region by id before the exact-board fallback applies.
    pub screen_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One screen's calibrated pose-space quad. Corner naming is from the user's
/// viewpoint looking AT the screen: `tl` = the screen's top-left corner.
/// Angles are in the post-flip convention (the capture flow applies the same
/// camera_position/invert_pitch flip before storing, so runtime does no extra
/// sign handling).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRegion {
    /// Identity carried through from the layout so callers can resolve a
    /// region index back to a display even after reordering.
    pub screen_id: String,
    /// Board rect snapshot at capture time (see [`BoardRect`]).
    pub board: BoardRect,
    pub tl: PosePoint,
    pub tr: PosePoint,
    pub br: PosePoint,
    pub bl: PosePoint,
}

/// Full multi-screen calibration, persisted inside FocusHaloSettings.
///
/// `#[serde(default)]` fills any missing key from [`Default`], so a config
/// written by an older build (or with a partially-written struct) still
/// deserializes instead of failing the whole settings load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScreenCalibration {
    /// Schema version; bumped only when the semantics of the fields change.
    pub version: u32,
    /// Wall clock (ms) at calibration completion, for UI display.
    pub captured_at_ms: u64,
    /// One region per calibrated screen. The caller keeps them ordered
    /// (board x then y) so region indices are stable across ticks.
    pub regions: Vec<ScreenRegion>,
    /// Reference seat position: the face center captured while calibrating.
    /// Deviations from it drive the translation compensation.
    pub ref_u: f64,
    pub ref_v: f64,
    /// Translation compensation coefficients (deg per unit u / v). Fitted by
    /// the optional lean-capture step; until then the geometric prior (~60, a
    /// typical webcam FOV) applies.
    pub trans_alpha_yaw: f64,
    pub trans_alpha_pitch: f64,
}

fn default_calibration_version() -> u32 {
    1
}

fn default_trans_alpha() -> f64 {
    60.0
}

impl Default for ScreenCalibration {
    fn default() -> Self {
        ScreenCalibration {
            version: default_calibration_version(),
            captured_at_ms: 0,
            regions: Vec::new(),
            ref_u: 0.5,
            ref_v: 0.5,
            trans_alpha_yaw: default_trans_alpha(),
            trans_alpha_pitch: default_trans_alpha(),
        }
    }
}

/// The four quad corners in TL -> TR -> BR -> BL order, as plain (yaw, pitch)
/// pairs (the pose plane this module's math operates in).
fn corners(r: &ScreenRegion) -> [(f64, f64); 4] {
    [
        (r.tl.yaw, r.tl.pitch),
        (r.tr.yaw, r.tr.pitch),
        (r.br.yaw, r.br.pitch),
        (r.bl.yaw, r.bl.pitch),
    ]
}

/// Convex-quad containment test. Walks the four edges in TL->TR->BR->BL
/// order and takes the cross product of (edge vector) x (corner->point):
/// for a convex quad every edge puts an interior point on the same side, so
/// conflicting signs mean outside. A cross product within [`SIDE_EPS`] of
/// zero means the point sits on the edge line — the edge cannot vote, which
/// is exactly what makes boundary points (edges and corners) count as
/// contained.
pub fn point_in_quad(p: (f64, f64), r: &ScreenRegion) -> bool {
    let c = corners(r);
    let mut saw_pos = false;
    let mut saw_neg = false;
    for i in 0..4 {
        let a = c[i];
        let b = c[(i + 1) % 4];
        let ex = b.0 - a.0;
        let ey = b.1 - a.1;
        let px = p.0 - a.0;
        let py = p.1 - a.1;
        let cross = ex * py - ey * px;
        if cross > SIDE_EPS {
            saw_pos = true;
        } else if cross < -SIDE_EPS {
            saw_neg = true;
        }
    }
    !(saw_pos && saw_neg)
}

/// Scale all four corners around the quad's centroid by `margin`
/// (corner = centroid + (corner - centroid) * margin). Only ever used for
/// region-attribution tolerance (see [`region_at`]), never for mapping, and
/// the u/v fields are carried through untouched because they do not
/// participate in pose-space geometry.
pub fn dilate_region(r: &ScreenRegion, margin: f64) -> ScreenRegion {
    let cy = (r.tl.yaw + r.tr.yaw + r.br.yaw + r.bl.yaw) / 4.0;
    let cp = (r.tl.pitch + r.tr.pitch + r.br.pitch + r.bl.pitch) / 4.0;
    let scale = |p: &PosePoint| PosePoint {
        yaw: cy + (p.yaw - cy) * margin,
        pitch: cp + (p.pitch - cp) * margin,
        u: p.u,
        v: p.v,
    };
    ScreenRegion {
        screen_id: r.screen_id.clone(),
        board: r.board.clone(),
        tl: scale(&r.tl),
        tr: scale(&r.tr),
        br: scale(&r.br),
        bl: scale(&r.bl),
    }
}

/// Forward bilinear interpolation: corner positions blended by (s, t) in the
/// unit square, with tl -> (0,0), tr -> (1,0), br -> (1,1), bl -> (0,1).
/// Exact by construction; used both by the residual test of [`bilinear_map`]
/// and the deterministic overlap sampling of [`validate_region`].
fn bilinear_forward(r: &ScreenRegion, s: f64, t: f64) -> (f64, f64) {
    let lerp = |a: f64, b: f64, k: f64| a + (b - a) * k;
    let top = (lerp(r.tl.yaw, r.tr.yaw, s), lerp(r.tl.pitch, r.tr.pitch, s));
    let bottom = (lerp(r.bl.yaw, r.br.yaw, s), lerp(r.bl.pitch, r.br.pitch, s));
    (lerp(top.0, bottom.0, t), lerp(top.1, bottom.1, t))
}

/// Inverse bilinear map: solve for (s, t) in [0, 1]^2 with
/// `bilinear_forward(r, s, t) == p`, returned as normalized screen
/// coordinates (tl -> (0,0), tr -> (1,0), br -> (1,1), bl -> (0,1)).
///
/// The inverse has no closed form for a general quad, so this uses fixed-
/// point iteration, alternating an exact one-variable solve per round:
/// given the current `t`, the horizontal blend between the top and bottom
/// edges is linear in `s`, so `s` is solved exactly (and symmetrically `t`
/// from the vertical blend between the left and right edges at the current
/// `s`). On a convex quad both edge spans share a sign, the blended span
/// never degenerates, and the alternation contracts to the true solution; on
/// a rectangle it is exact after a single round. Iterates are deliberately
/// left unclamped (clamping mid-iteration would freeze the solution just
/// outside a boundary and stall there); only the final result is clamped to
/// the unit square, which is also what maps outside poses onto the quad
/// border.
pub fn bilinear_map(p: (f64, f64), r: &ScreenRegion) -> (f64, f64) {
    let mut s = 0.5;
    let mut t = 0.5;
    for _ in 0..BILINEAR_MAX_ITERS {
        // Horizontal blend at parameter t: the segment from the point at s=0
        // (lerp of tl and bl) to the point at s=1 (lerp of tr and br). Solve
        // where p's yaw sits along it.
        let base_x = (1.0 - t) * r.tl.yaw + t * r.bl.yaw;
        let span_x = (1.0 - t) * (r.tr.yaw - r.tl.yaw) + t * (r.br.yaw - r.bl.yaw);
        if span_x.abs() < 1e-12 {
            s = 0.5;
        } else {
            s = (p.0 - base_x) / span_x;
        }
        // Vertical blend at parameter s: from the point at t=0 (lerp of tl
        // and tr) to the point at t=1 (lerp of bl and br). Solve where p's
        // pitch sits along it.
        let base_y = (1.0 - s) * r.tl.pitch + s * r.tr.pitch;
        let span_y = (1.0 - s) * (r.bl.pitch - r.tl.pitch) + s * (r.br.pitch - r.tr.pitch);
        if span_y.abs() < 1e-12 {
            t = 0.5;
        } else {
            t = (p.1 - base_y) / span_y;
        }
    }
    (s.clamp(0.0, 1.0), t.clamp(0.0, 1.0))
}

/// Distance from `p` to the closest point of the segment `a`-`b`.
fn point_segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let ex = b.0 - a.0;
    let ey = b.1 - a.1;
    let px = p.0 - a.0;
    let py = p.1 - a.1;
    let len2 = ex * ex + ey * ey;
    let t = if len2 < 1e-18 {
        0.0
    } else {
        ((px * ex + py * ey) / len2).clamp(0.0, 1.0)
    };
    let dx = px - t * ex;
    let dy = py - t * ey;
    (dx * dx + dy * dy).sqrt()
}

/// Distance from a pose to a region's boundary: 0 inside, otherwise the
/// minimum point-to-edge (point-to-segment) distance over the four edges.
pub fn dist_to_region(p: (f64, f64), r: &ScreenRegion) -> f64 {
    if point_in_quad(p, r) {
        return 0.0;
    }
    let c = corners(r);
    let mut best = f64::INFINITY;
    for i in 0..4 {
        let d = point_segment_distance(p, c[i], c[(i + 1) % 4]);
        if d < best {
            best = d;
        }
    }
    best
}

/// Attribute a pose to one of the calibrated regions.
///
/// 1. Exactly one un-dilated quad contains the pose -> that region.
/// 2. Several contain it (slightly overlapping calibrations) -> the one with
///    the smallest [`dist_to_region`] (ties keep the lower index, keeping the
///    result deterministic).
/// 3. None contains it -> retry against the dilated quads (only when
///    `margin > 1.0`): exactly one dilated containment wins outright;
///    several fall back to the smallest un-dilated distance; none (the pose
///    is outside every screen even with tolerance) returns `None`, which the
///    caller interprets as "keep the current screen".
pub fn region_at(p: (f64, f64), regions: &[ScreenRegion], margin: f64) -> Option<usize> {
    let nearest_by = |candidates: &[usize]| -> usize {
        let dist = |i: usize| dist_to_region(p, &regions[i]);
        *candidates
            .iter()
            .min_by(|a, b| dist(**a).total_cmp(&dist(**b)))
            .expect("candidates are never empty at call sites")
    };

    let containing: Vec<usize> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| point_in_quad(p, r))
        .map(|(i, _)| i)
        .collect();
    if containing.len() == 1 {
        return Some(containing[0]);
    }
    if containing.len() > 1 {
        return Some(nearest_by(&containing));
    }

    if margin > 1.0 {
        let dilated: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| point_in_quad(p, &dilate_region(r, margin)))
            .map(|(i, _)| i)
            .collect();
        if dilated.len() == 1 {
            return Some(dilated[0]);
        }
        if dilated.len() > 1 {
            // Both dilated quads claim the pose: fall back to the raw
            // geometry, which is still > 0 here because step 1 found no
            // un-dilated containment.
            return Some(nearest_by(&dilated));
        }
    }
    None
}

/// Remove the first-order effect of the user shifting seat between
/// calibration and runtime, using the face-center position the tracker
/// already reports.
///
/// Derivation: with the head a distance `D` from the watched screen point,
/// translating sideways by `dx` rotates the head-to-point direction by
/// `atan(dx / D)`. Moving right grows the face-center `u` (image x grows
/// rightward) and makes the yaw needed to hold the same point *smaller*, so
/// the captured reading is `yaw_ref - atan(dx/D)`. Dividing the angular term
/// by the normalized seat shift gives `alpha_u` (deg per unit u), and the
/// inverse correction is `yaw_eff = yaw + alpha_u * (u - ref_u)`.
///
/// Vertically: moving down grows `v` (image y grows downward) and the head
/// must pitch *up* more to hold the same point, so the captured reading is
/// `pitch_ref + atan(dy/D)` and the inverse is
/// `pitch_eff = pitch - alpha_v * (v - ref_v)`. Both alphas are stored
/// positive; the sign difference lives in this formula.
pub fn compensate(pose: (f64, f64), uv: (f64, f64), cal: &ScreenCalibration) -> (f64, f64) {
    let du = uv.0 - cal.ref_u;
    let dv = uv.1 - cal.ref_v;
    (
        pose.0 + cal.trans_alpha_yaw * du,
        pose.1 - cal.trans_alpha_pitch * dv,
    )
}

/// Why a captured region was rejected. Variants are checked (and returned)
/// in this order: shape, corner ordering, size, overlap with an existing
/// region.
#[derive(Debug, Clone, PartialEq)]
pub enum RegionError {
    /// The four corners do not form a convex quad (self-intersecting
    /// "bowtie", collinear corners, or duplicated points).
    NonConvex,
    /// Corner ordering violates the viewpoint convention: a "left" corner is
    /// not to the left of its partner, or a "top" corner is not above its
    /// partner (see the module coordinate conventions).
    CornerOrder,
    /// The quad is narrower or shorter than [`MIN_REGION_EXTENT_DEG`] on at
    /// least one side, so it cannot separate this screen from its neighbors.
    TooSmall,
    /// More than the tolerated fraction of this region's bilinear grid
    /// samples falls inside an already-calibrated region (`other` is that
    /// region's index, `ratio` the sampled fraction).
    Overlap { other: usize, ratio: f64 },
}

/// Validate a freshly captured region against the already-accepted ones, in
/// check order: corner ordering, convexity, per-side extent, then the
/// deterministic overlap ratio (9x9 bilinear grid of this region sampled
/// against each existing quad, corners included).
///
/// Corner ordering is checked *before* convexity on purpose: swapping two
/// adjacent corners of a valid quad always produces a self-intersecting
/// polygon, so a shape-first order would make [`RegionError::CornerOrder`]
/// unreachable and misreport the common "corners captured in the wrong
/// order" mistake as a shape error.
pub fn validate_region(r: &ScreenRegion, existing: &[ScreenRegion]) -> Result<(), RegionError> {
    // 1. Corner ordering from the module conventions (left corners more
    //    negative in yaw, top corners more positive in pitch).
    if !(r.tl.yaw < r.tr.yaw && r.bl.yaw < r.br.yaw)
        || !(r.tl.pitch > r.bl.pitch && r.tr.pitch > r.br.pitch)
    {
        return Err(RegionError::CornerOrder);
    }

    // 2. Convexity: the loop TL->TR->BR->BL must turn the same way at every
    //    corner. A ~zero cross means collinear corners; a sign change means
    //    the polygon self-intersects.
    let c = corners(r);
    let mut any_pos = false;
    let mut any_neg = false;
    for i in 0..4 {
        let a = c[i];
        let b = c[(i + 1) % 4];
        let d = c[(i + 2) % 4];
        let cross = (b.0 - a.0) * (d.1 - b.1) - (b.1 - a.1) * (d.0 - b.0);
        if cross > CONVEX_EPS {
            any_pos = true;
        } else if cross < -CONVEX_EPS {
            any_neg = true;
        } else {
            // Collinear corner: degenerate quad, never a usable region.
            return Err(RegionError::NonConvex);
        }
    }
    if any_pos && any_neg {
        return Err(RegionError::NonConvex);
    }

    // 3. Extents: all four sides must be wide/tall enough to discriminate.
    let top_w = r.tr.yaw - r.tl.yaw;
    let bottom_w = r.br.yaw - r.bl.yaw;
    let left_h = r.tl.pitch - r.bl.pitch;
    let right_h = r.tr.pitch - r.br.pitch;
    if top_w < MIN_REGION_EXTENT_DEG
        || bottom_w < MIN_REGION_EXTENT_DEG
        || left_h < MIN_REGION_EXTENT_DEG
        || right_h < MIN_REGION_EXTENT_DEG
    {
        return Err(RegionError::TooSmall);
    }

    // 4. Overlap against each existing region, one direction only (the new
    //    region sampled into the existing one): the capture flow validates
    //    regions one at a time in board order, so this direction is the one
    //    that catches "I just re-calibrated the screen I already did".
    for (idx, other) in existing.iter().enumerate() {
        let mut hits = 0usize;
        let total = OVERLAP_GRID * OVERLAP_GRID;
        for i in 0..OVERLAP_GRID {
            let s = i as f64 / (OVERLAP_GRID - 1) as f64;
            for j in 0..OVERLAP_GRID {
                let t = j as f64 / (OVERLAP_GRID - 1) as f64;
                if point_in_quad(bilinear_forward(r, s, t), other) {
                    hits += 1;
                }
            }
        }
        let ratio = hits as f64 / total as f64;
        if ratio > OVERLAP_MAX_RATIO {
            return Err(RegionError::Overlap { other: idx, ratio });
        }
    }
    Ok(())
}

/// Whether the captured calibration still describes the live display layout.
/// Screen counts must match, each captured region must find its screen
/// (by `screen_id` first, falling back to an exact geometry match for setups
/// whose ids changed without a topology change), and every coordinate must
/// agree within [`TOPOLOGY_TOLERANCE_PX`] logical px.
pub fn topology_matches(cal: &ScreenCalibration, current: &[BoardRect]) -> bool {
    if cal.regions.len() != current.len() {
        return false;
    }
    // Exact geometry equality for the id-less fallback: the id branch has
    // already failed by definition here, so comparing ids again would make
    // the fallback unreachable. Coordinates compare exactly (==), not within
    // the tolerance — the tolerance pass below judges the matched rect.
    let same_geometry = |a: &BoardRect, b: &BoardRect| {
        a.x == b.x && a.y == b.y && a.width == b.width && a.height == b.height
    };
    for region in &cal.regions {
        let matched = current
            .iter()
            .find(|b| b.screen_id == region.screen_id)
            .or_else(|| current.iter().find(|b| same_geometry(b, &region.board)));
        let Some(rect) = matched else {
            return false;
        };
        let same = |a: f64, b: f64| (a - b).abs() <= TOPOLOGY_TOLERANCE_PX;
        if !same(rect.x, region.board.x)
            || !same(rect.y, region.board.y)
            || !same(rect.width, region.board.width)
            || !same(rect.height, region.board.height)
        {
            return false;
        }
    }
    true
}

/// Which region to render for one tick of the region hysteresis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionDecision {
    /// The region to use this tick: the just-committed one when `committed`,
    /// otherwise the held current region.
    pub region: Option<usize>,
    /// True exactly on the tick a pending transition commits.
    pub committed: bool,
}

/// Hysteresis gate between raw pose attribution and the committed "current
/// screen". A new attribution must persist `settle_ms` before it commits,
/// so a borderline pose (or a glance swept across two screens) cannot churn
/// the switch target. `None` attributions never become pending targets:
/// losing the face or leaving all regions holds the current screen and
/// drops any pending transition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegionSelector {
    current: Option<usize>,
    pending: Option<usize>,
    pending_since_ms: Option<u64>,
}

impl RegionSelector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed region (read-only view for callers).
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// One tick. `target` is the [`region_at`] output; `now_ms` is any
    /// monotonic millisecond clock; `settle_ms` is how long a new target
    /// must persist before committing (caller guarantees > 0; a 0 would
    /// simply commit on the very tick the target appears, which the
    /// u64 type already makes well-defined).
    pub fn observe(
        &mut self,
        target: Option<usize>,
        now_ms: u64,
        settle_ms: u64,
    ) -> RegionDecision {
        // Holding semantics: no attribution, or the same one as now — clear
        // any pending transition and keep rendering the current region.
        if target.is_none() || target == self.current {
            self.pending = None;
            self.pending_since_ms = None;
            return RegionDecision {
                region: self.current,
                committed: false,
            };
        }

        // A concrete new target. Continuing an existing pending candidacy
        // keeps its start time; anything else restarts the settle window.
        if self.pending != target {
            self.pending = target;
            self.pending_since_ms = Some(now_ms);
        }
        let since = self.pending_since_ms.unwrap_or(now_ms);
        if now_ms.saturating_sub(since) >= settle_ms {
            self.current = target;
            self.pending = None;
            self.pending_since_ms = None;
            return RegionDecision {
                region: target,
                committed: true,
            };
        }
        RegionDecision {
            region: self.current,
            committed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rectangle quad in pose space: yaw in [-10, 10], pitch in [0, 10].
    fn rect_region(id: &str, x0: f64, x1: f64) -> ScreenRegion {
        ScreenRegion {
            screen_id: id.into(),
            board: BoardRect {
                screen_id: id.into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            tl: PosePoint {
                yaw: x0,
                pitch: 10.0,
                u: 0.0,
                v: 0.0,
            },
            tr: PosePoint {
                yaw: x1,
                pitch: 10.0,
                u: 1.0,
                v: 0.0,
            },
            br: PosePoint {
                yaw: x1,
                pitch: 0.0,
                u: 1.0,
                v: 1.0,
            },
            bl: PosePoint {
                yaw: x0,
                pitch: 0.0,
                u: 0.0,
                v: 1.0,
            },
        }
    }

    /// A mildly asymmetric convex quad resembling a real calibration.
    fn quad_region(id: &str) -> ScreenRegion {
        ScreenRegion {
            screen_id: id.into(),
            board: BoardRect {
                screen_id: id.into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            tl: PosePoint {
                yaw: -12.0,
                pitch: 9.0,
                u: 0.0,
                v: 0.0,
            },
            tr: PosePoint {
                yaw: 13.0,
                pitch: 8.0,
                u: 1.0,
                v: 0.0,
            },
            br: PosePoint {
                yaw: 11.0,
                pitch: -7.0,
                u: 1.0,
                v: 1.0,
            },
            bl: PosePoint {
                yaw: -10.0,
                pitch: -6.0,
                u: 0.0,
                v: 1.0,
            },
        }
    }

    // ------------------------------------------------------------------
    // point_in_quad
    // ------------------------------------------------------------------

    #[test]
    fn point_in_quad_interior_and_exterior() {
        let r = rect_region("a", -10.0, 10.0);
        assert!(point_in_quad((0.0, 5.0), &r), "center is inside");
        assert!(
            point_in_quad((-9.9, 0.1), &r),
            "near-corner interior is inside"
        );
        assert!(
            !point_in_quad((20.0, 5.0), &r),
            "beyond right edge is outside"
        );
        assert!(!point_in_quad((0.0, 11.0), &r), "above top edge is outside");
        assert!(
            !point_in_quad((-10.5, -0.5), &r),
            "past bottom-left corner is outside"
        );
    }

    #[test]
    fn point_in_quad_boundary_counts_as_contained() {
        let r = rect_region("a", -10.0, 10.0);
        assert!(point_in_quad((-10.0, 10.0), &r), "exactly on the tl corner");
        assert!(point_in_quad((0.0, 10.0), &r), "midpoint of the top edge");
        assert!(point_in_quad((10.0, 3.0), &r), "on the right edge");
        assert!(point_in_quad((-10.0, 2.0), &r), "on the left edge");
    }

    // ------------------------------------------------------------------
    // dilate_region
    // ------------------------------------------------------------------

    #[test]
    fn dilate_at_margin_one_is_identity() {
        let r = quad_region("a");
        assert_eq!(dilate_region(&r, 1.0), r);
    }

    #[test]
    fn dilate_grows_around_fixed_centroid() {
        let r = rect_region("a", -10.0, 10.0);
        let d = dilate_region(&r, 1.3);
        // Centroid (average of the four corners) is invariant.
        let centroid = |q: &ScreenRegion| {
            (
                (q.tl.yaw + q.tr.yaw + q.br.yaw + q.bl.yaw) / 4.0,
                (q.tl.pitch + q.tr.pitch + q.br.pitch + q.bl.pitch) / 4.0,
            )
        };
        let (cx, cp) = centroid(&r);
        let (dx, dp) = centroid(&d);
        assert!((cx - dx).abs() < 1e-12 && (cp - dp).abs() < 1e-12);
        // The dilated quad spans 1.3x around the same centroid: the rect
        // spans yaw [-10, 10] around centroid 0, so dilation reaches -13/13.
        assert!((d.tl.yaw - (-13.0)).abs() < 1e-12, "tl yaw = {}", d.tl.yaw);
        assert!((d.tr.yaw - 13.0).abs() < 1e-12);
        assert!(
            (d.tl.pitch - 11.5).abs() < 1e-12,
            "tl pitch = {}",
            d.tl.pitch
        );
        // The original quad is fully contained in the dilated one.
        for p in [
            (-10.0, 10.0),
            (10.0, 10.0),
            (10.0, 0.0),
            (-10.0, 0.0),
            (0.0, 5.0),
        ] {
            assert!(
                point_in_quad(p, &d),
                "original point {p:?} inside dilated quad"
            );
        }
        // u/v ride along untouched.
        assert_eq!(d.tl.u, r.tl.u);
        assert_eq!(d.br.v, r.br.v);
    }

    // ------------------------------------------------------------------
    // bilinear_map
    // ------------------------------------------------------------------

    #[test]
    fn bilinear_map_on_rectangle_is_exact_linear() {
        let r = rect_region("a", -10.0, 10.0);
        let cases = [
            ((-10.0, 10.0), (0.0, 0.0)),
            ((10.0, 10.0), (1.0, 0.0)),
            ((10.0, 0.0), (1.0, 1.0)),
            ((-10.0, 0.0), (0.0, 1.0)),
            ((0.0, 5.0), (0.5, 0.5)),
            ((-5.0, 7.5), (0.25, 0.25)),
            ((3.0, 3.0), (0.65, 0.7)),
        ];
        for (p, expected) in cases {
            let got = bilinear_map(p, &r);
            assert!(
                (got.0 - expected.0).abs() < 1e-9 && (got.1 - expected.1).abs() < 1e-9,
                "p={p:?} -> {got:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn bilinear_map_on_general_quad_maps_corners_exactly() {
        let r = quad_region("a");
        let cases = [
            ((-12.0, 9.0), (0.0, 0.0)),
            ((13.0, 8.0), (1.0, 0.0)),
            ((11.0, -7.0), (1.0, 1.0)),
            ((-10.0, -6.0), (0.0, 1.0)),
        ];
        for (p, expected) in cases {
            let got = bilinear_map(p, &r);
            assert!(
                (got.0 - expected.0).abs() < 1e-6 && (got.1 - expected.1).abs() < 1e-6,
                "corner p={p:?} -> {got:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn bilinear_map_residual_on_general_quad_is_tiny() {
        let r = quad_region("a");
        // Grid over the unit square: invert each forward sample and demand
        // the round trip reproduces the pose well below the 0.05 deg gate.
        for i in 0..=10u32 {
            for j in 0..=10u32 {
                let s = f64::from(i) / 10.0;
                let t = f64::from(j) / 10.0;
                let p = bilinear_forward(&r, s, t);
                let (s2, t2) = bilinear_map(p, &r);
                let back = bilinear_forward(&r, s2, t2);
                let err = ((back.0 - p.0).powi(2) + (back.1 - p.1).powi(2)).sqrt();
                assert!(err < 0.05, "s={s} t={t} p={p:?} back={back:?} err={err}");
                assert!(
                    (s2 - s).abs() < 0.01 && (t2 - t).abs() < 0.01,
                    "s={s} t={t} -> ({s2}, {t2})"
                );
            }
        }
    }

    #[test]
    fn bilinear_map_clamps_outside_poses_to_unit_square() {
        let r = rect_region("a", -10.0, 10.0);
        let got = bilinear_map((30.0, -20.0), &r);
        assert!((got.0 - 1.0).abs() < 1e-12 && (got.1 - 1.0).abs() < 1e-12);
    }

    // ------------------------------------------------------------------
    // dist_to_region
    // ------------------------------------------------------------------

    #[test]
    fn dist_to_region_is_zero_inside_and_on_boundary() {
        let r = rect_region("a", -10.0, 10.0);
        assert_eq!(dist_to_region((0.0, 5.0), &r), 0.0);
        assert_eq!(dist_to_region((10.0, 4.0), &r), 0.0, "on the right edge");
    }

    #[test]
    fn dist_to_region_exterior_matches_hand_computed_distance() {
        let r = rect_region("a", 0.0, 10.0); // yaw [0, 10], pitch [0, 10]
        assert!((dist_to_region((15.0, 5.0), &r) - 5.0).abs() < 1e-12);
        assert!((dist_to_region((-3.0, 2.0), &r) - 3.0).abs() < 1e-12);
        // Diagonal corner case: nearest point of both the top and right edge
        // segments is the shared corner (10, 10).
        let d = dist_to_region((12.0, 13.0), &r);
        assert!((d - 13.0_f64.sqrt()).abs() < 1e-12, "d = {d}");
    }

    // ------------------------------------------------------------------
    // region_at
    // ------------------------------------------------------------------

    #[test]
    fn region_at_unique_containment() {
        let regions = vec![rect_region("a", -10.0, -2.0), rect_region("b", 2.0, 10.0)];
        assert_eq!(region_at((-6.0, 5.0), &regions, 1.15), Some(0));
        assert_eq!(region_at((6.0, 5.0), &regions, 1.15), Some(1));
        assert_eq!(
            region_at((0.0, 5.0), &regions, 1.0),
            None,
            "margin 1.0 means no dilation"
        );
    }

    #[test]
    fn region_at_gap_picks_nearest_within_margin_and_none_outside() {
        // Gap between yaw -1 and 3; the pose at 1.5 is 2.5 deg from A and
        // 1.5 deg from B.
        let regions = vec![rect_region("a", -10.0, -1.0), rect_region("b", 3.0, 12.0)];
        // margin 1.6 dilates both quads enough to reach 1.5 -> the nearer
        // (undilated) region wins.
        assert_eq!(region_at((1.5, 5.0), &regions, 1.6), Some(1));
        // margin 1.2 leaves the pose outside both dilated quads.
        assert_eq!(region_at((1.5, 5.0), &regions, 1.2), None);
    }

    #[test]
    fn region_at_overlap_falls_back_to_stable_lowest_index() {
        // Both quads contain the pose (overlap zone yaw [5, 10]), so
        // dist_to_region is 0 for each and the documented min-distance rule
        // ties: the lower index wins, keeping the attribution deterministic
        // for a slightly-overlapping calibration.
        let regions = vec![rect_region("a", 0.0, 10.0), rect_region("b", 5.0, 15.0)];
        assert_eq!(region_at((6.0, 5.0), &regions, 1.15), Some(0));
        assert_eq!(region_at((8.0, 5.0), &regions, 1.15), Some(0));
        assert_eq!(region_at((9.5, 5.0), &regions, 1.15), Some(0));
        // Single-containment poses resolve normally.
        assert_eq!(region_at((2.0, 5.0), &regions, 1.15), Some(0));
        assert_eq!(region_at((13.0, 5.0), &regions, 1.15), Some(1));
    }

    // ------------------------------------------------------------------
    // RegionSelector
    // ------------------------------------------------------------------

    #[test]
    fn selector_holds_same_region_without_committing() {
        let mut sel = RegionSelector::new();
        assert_eq!(
            sel.observe(Some(0), 1000, 300),
            RegionDecision {
                region: None,
                committed: false
            }
        );
        assert_eq!(
            sel.observe(Some(0), 1300, 300),
            RegionDecision {
                region: Some(0),
                committed: true
            },
            "first acquisition commits after settle"
        );
        for tick in 1..5u64 {
            let d = sel.observe(Some(0), 1300 + tick * 100, 300);
            assert_eq!(
                d,
                RegionDecision {
                    region: Some(0),
                    committed: false
                },
                "tick {tick}"
            );
        }
    }

    #[test]
    fn selector_requires_full_settle_and_commits_exactly_once() {
        let mut sel = RegionSelector::new();
        sel.observe(Some(0), 0, 300);
        sel.observe(Some(0), 300, 300); // current = 0
                                        // Target flips to 1 at t = 1000.
        assert_eq!(
            sel.observe(Some(1), 1000, 300),
            RegionDecision {
                region: Some(0),
                committed: false
            },
            "pending, not yet committed"
        );
        assert_eq!(
            sel.observe(Some(1), 1299, 300),
            RegionDecision {
                region: Some(0),
                committed: false
            },
            "1 ms short of settle"
        );
        assert_eq!(
            sel.observe(Some(1), 1300, 300),
            RegionDecision {
                region: Some(1),
                committed: true
            },
            "commit exactly when the settle duration is reached"
        );
        assert_eq!(
            sel.observe(Some(1), 1400, 300),
            RegionDecision {
                region: Some(1),
                committed: false
            },
            "held afterwards without re-committing"
        );
    }

    #[test]
    fn selector_flicker_back_drops_pending_and_restarts_settle() {
        let mut sel = RegionSelector::new();
        sel.observe(Some(0), 0, 300);
        sel.observe(Some(0), 300, 300); // current = 0
        sel.observe(Some(1), 1000, 300); // pending 1
                                         // Pose falls back to the current region before settle elapses.
        assert_eq!(
            sel.observe(Some(0), 1100, 300),
            RegionDecision {
                region: Some(0),
                committed: false
            },
            "no commit, pending dropped"
        );
        // A later re-appearance of 1 needs the full settle again.
        assert_eq!(sel.observe(Some(1), 1200, 300).committed, false);
        assert_eq!(
            sel.observe(Some(1), 1400, 300).committed,
            false,
            "still inside the fresh window"
        );
        assert_eq!(sel.observe(Some(1), 1500, 300).committed, true);
    }

    #[test]
    fn selector_none_holds_current_and_drops_pending() {
        let mut sel = RegionSelector::new();
        sel.observe(Some(0), 0, 300);
        sel.observe(Some(0), 300, 300); // current = 0
        sel.observe(Some(1), 1000, 300); // pending 1
                                         // Face lost / pose outside every region: hold, never commit a switch.
        for t in [1100u64, 2000, 5000] {
            assert_eq!(
                sel.observe(None, t, 300),
                RegionDecision {
                    region: Some(0),
                    committed: false
                },
                "t = {t}"
            );
        }
        // And the pending was dropped: 1 must settle from scratch again.
        assert_eq!(sel.observe(Some(1), 5100, 300).committed, false);
        assert_eq!(sel.observe(Some(1), 5400, 300).committed, true);
    }

    #[test]
    fn selector_zero_settle_commits_on_first_tick() {
        let mut sel = RegionSelector::new();
        assert_eq!(
            sel.observe(Some(2), 0, 0),
            RegionDecision {
                region: Some(2),
                committed: true
            },
            "settle 0 commits the moment the target appears"
        );
    }

    #[test]
    fn selector_switching_back_to_old_region_needs_hysteresis_again() {
        let mut sel = RegionSelector::new();
        sel.observe(Some(0), 0, 300);
        sel.observe(Some(0), 300, 300); // current = 0
        sel.observe(Some(1), 1000, 300);
        sel.observe(Some(1), 1300, 300); // committed = 1
                                         // Immediate flicker back to 0 must not commit instantly.
        assert_eq!(sel.observe(Some(0), 1310, 300).committed, false);
        assert_eq!(sel.observe(Some(0), 1400, 300).committed, false);
        assert_eq!(sel.observe(Some(0), 1610, 300).committed, true);
        assert_eq!(sel.current(), Some(0));
    }

    // ------------------------------------------------------------------
    // compensate
    // ------------------------------------------------------------------

    #[test]
    fn compensate_restores_yaw_after_rightward_seat_shift() {
        // Synthetic "watch a fixed point, slide 0.1 m right" setup: D = 0.6 m,
        // so the required yaw drops by atan(0.1/0.6); the normalized face
        // shift is assumed 0.1 units. The true alpha maps one u unit to that
        // angular change.
        let shift_rad = 0.1f64.atan2(0.6);
        let alpha = shift_rad.to_degrees() / 0.1;
        let cal = ScreenCalibration {
            ref_u: 0.5,
            ref_v: 0.5,
            trans_alpha_yaw: alpha,
            trans_alpha_pitch: alpha,
            ..ScreenCalibration::default()
        };
        let yaw_ref = 5.0;
        let observed = (yaw_ref - shift_rad.to_degrees(), -2.0);
        let (yaw_eff, _) = compensate(observed, (0.6, 0.5), &cal);
        assert!(
            (yaw_eff - yaw_ref).abs() < 0.5,
            "yaw_eff = {yaw_eff}, expected ~{yaw_ref}"
        );
    }

    #[test]
    fn compensate_restores_pitch_after_downward_seat_shift() {
        let shift_rad = 0.1f64.atan2(0.6);
        let alpha = shift_rad.to_degrees() / 0.1;
        let cal = ScreenCalibration {
            ref_u: 0.5,
            ref_v: 0.5,
            trans_alpha_yaw: alpha,
            trans_alpha_pitch: alpha,
            ..ScreenCalibration::default()
        };
        let pitch_ref = -2.0;
        // Sliding down grows v and the head must pitch UP more (positive) to
        // hold the same point.
        let observed = (1.0, pitch_ref + shift_rad.to_degrees());
        let (_, pitch_eff) = compensate(observed, (0.5, 0.6), &cal);
        assert!(
            (pitch_eff - pitch_ref).abs() < 0.5,
            "pitch_eff = {pitch_eff}, expected ~{pitch_ref}"
        );
    }

    #[test]
    fn compensate_is_identity_at_reference_seat() {
        let cal = ScreenCalibration::default();
        assert_eq!(compensate((3.0, -4.0), (0.5, 0.5), &cal), (3.0, -4.0));
    }

    // ------------------------------------------------------------------
    // validate_region
    // ------------------------------------------------------------------

    #[test]
    fn validate_accepts_a_well_formed_region() {
        let r = quad_region("a");
        assert_eq!(validate_region(&r, &[]), Ok(()));
    }

    #[test]
    fn validate_rejects_swapped_top_corners() {
        // Swapping TL/TR breaks tl.yaw < tr.yaw (checked first, see
        // validate_region docs: the swap also self-intersects the quad, so a
        // shape-first order would report NonConvex instead).
        let mut r = quad_region("a");
        std::mem::swap(&mut r.tl, &mut r.tr);
        assert_eq!(validate_region(&r, &[]), Err(RegionError::CornerOrder));
    }

    #[test]
    fn validate_rejects_mislabeled_corner_on_a_convex_quad() {
        // Convex CW quad whose "top-left" corner sits BELOW the "bottom-left"
        // one (tl.pitch 2 < bl.pitch 3): pure labeling mistake, no shape
        // defect — CornerOrder must fire on the ordering inequality alone.
        let r = ScreenRegion {
            screen_id: "a".into(),
            board: BoardRect {
                screen_id: "a".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            tl: PosePoint {
                yaw: 5.0,
                pitch: 2.0,
                u: 0.0,
                v: 0.0,
            },
            tr: PosePoint {
                yaw: 15.0,
                pitch: -8.0,
                u: 1.0,
                v: 0.0,
            },
            br: PosePoint {
                yaw: 8.0,
                pitch: -12.0,
                u: 1.0,
                v: 1.0,
            },
            bl: PosePoint {
                yaw: 0.0,
                pitch: 3.0,
                u: 0.0,
                v: 1.0,
            },
        };
        assert_eq!(validate_region(&r, &[]), Err(RegionError::CornerOrder));
    }

    #[test]
    fn validate_rejects_self_intersecting_bowtie() {
        // Satisfies every corner-order inequality, yet the TL-BL and TR-BR
        // side edges cross: convexity must catch it first.
        let r = ScreenRegion {
            screen_id: "a".into(),
            board: BoardRect {
                screen_id: "a".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            tl: PosePoint {
                yaw: 0.0,
                pitch: 10.0,
                u: 0.0,
                v: 0.0,
            },
            tr: PosePoint {
                yaw: 1.0,
                pitch: 8.0,
                u: 1.0,
                v: 0.0,
            },
            br: PosePoint {
                yaw: 10.0,
                pitch: 0.0,
                u: 1.0,
                v: 1.0,
            },
            bl: PosePoint {
                yaw: 9.0,
                pitch: 0.0,
                u: 0.0,
                v: 1.0,
            },
        };
        assert_eq!(validate_region(&r, &[]), Err(RegionError::NonConvex));
    }

    #[test]
    fn validate_rejects_too_narrow_region() {
        let mut r = rect_region("a", 0.0, 0.5); // top/bottom width 0.5 < 1 deg
        r.board.width = 100.0;
        assert_eq!(validate_region(&r, &[]), Err(RegionError::TooSmall));
        let mut r = rect_region("a", 0.0, 10.0);
        // Collapse the vertical extent instead.
        r.bl.pitch = 9.5;
        r.br.pitch = 9.4;
        assert_eq!(validate_region(&r, &[]), Err(RegionError::TooSmall));
    }

    #[test]
    fn validate_rejects_heavy_overlap_with_existing() {
        let existing = vec![rect_region("a", 8.0, 18.0)];
        let fresh = rect_region("b", 0.0, 10.0);
        match validate_region(&fresh, &existing) {
            Err(RegionError::Overlap { other, ratio }) => {
                assert_eq!(other, 0);
                // Grid yaw columns 8.75 and 10.0 (2 of 9 columns) land inside
                // the existing quad: ratio 2/9.
                assert!((ratio - 2.0 / 9.0).abs() < 1e-9, "ratio = {ratio}");
            }
            other => panic!("expected Overlap, got {other:?}"),
        }
        // Fully identical quads: every sample collides.
        let twin = rect_region("b", 0.0, 10.0);
        assert!(matches!(
            validate_region(&twin, &vec![rect_region("a", 0.0, 10.0)]),
            Err(RegionError::Overlap { other: 0, .. })
        ));
    }

    #[test]
    fn validate_allows_slight_overlap_under_threshold() {
        // Only the s = 1.0 column (9 of 81 samples, ~11%) lands inside the
        // existing quad: under the 15% gate, so both may coexist.
        let existing = vec![rect_region("a", 9.5, 19.5)];
        let fresh = rect_region("b", 0.0, 10.0);
        assert_eq!(validate_region(&fresh, &existing), Ok(()));
    }

    // ------------------------------------------------------------------
    // topology_matches
    // ------------------------------------------------------------------

    fn board(id: &str, x: f64, y: f64, w: f64, h: f64) -> BoardRect {
        BoardRect {
            screen_id: id.into(),
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn calibrated(id: &str, b: BoardRect) -> ScreenCalibration {
        ScreenCalibration {
            regions: vec![ScreenRegion {
                screen_id: id.into(),
                board: b,
                ..quad_region(id)
            }],
            ..ScreenCalibration::default()
        }
    }

    #[test]
    fn topology_matches_identical_layout_by_id() {
        let cal = calibrated("hdmi-1", board("hdmi-1", 0.0, 0.0, 1920.0, 1080.0));
        let current = vec![board("hdmi-1", 0.0, 0.0, 1920.0, 1080.0)];
        assert!(topology_matches(&cal, &current));
        // Within the 2 px tolerance it still counts as unchanged.
        let nudged = vec![board("hdmi-1", 1.5, -2.0, 1921.0, 1078.5)];
        assert!(topology_matches(&cal, &nudged));
    }

    #[test]
    fn topology_rejects_count_and_geometry_changes() {
        let cal = calibrated("hdmi-1", board("hdmi-1", 0.0, 0.0, 1920.0, 1080.0));
        let current = vec![board("hdmi-1", 0.0, 0.0, 1920.0, 1080.0)];
        // Screen count changed.
        let two = vec![
            current[0].clone(),
            board("hdmi-2", 1920.0, 0.0, 1920.0, 1080.0),
        ];
        assert!(!topology_matches(&cal, &two));
        // Position beyond tolerance.
        let moved = vec![board("hdmi-1", 3.0, 0.0, 1920.0, 1080.0)];
        assert!(!topology_matches(&cal, &moved));
        // Size beyond tolerance.
        let resized = vec![board("hdmi-1", 0.0, 0.0, 1925.0, 1080.0)];
        assert!(!topology_matches(&cal, &resized));
        // Unknown id AND different geometry: neither the id branch nor the
        // exact-board fallback can match.
        let renamed = vec![board("dp-9", 10.0, 0.0, 1920.0, 1080.0)];
        assert!(!topology_matches(&cal, &renamed));
    }

    #[test]
    fn topology_id_match_takes_priority_over_board_equality() {
        let cal = calibrated("hdmi-1", board("hdmi-1", 0.0, 0.0, 1920.0, 1080.0));
        // A different id owns the identical rect, while the matching id has
        // moved: matching by id (the moved one) must fail, proving id
        // priority over the exact-board fallback.
        let tricky = vec![
            board("dp-9", 0.0, 0.0, 1920.0, 1080.0),
            board("hdmi-1", 5.0, 0.0, 1920.0, 1080.0),
        ];
        assert!(!topology_matches(&cal, &tricky));
        // Conversely, an id change with an exactly equal board still matches
        // via the fallback.
        let renamed_same_rect = vec![board("dp-9", 0.0, 0.0, 1920.0, 1080.0)];
        assert!(topology_matches(&cal, &renamed_same_rect));
    }

    // ------------------------------------------------------------------
    // serde
    // ------------------------------------------------------------------

    #[test]
    fn screen_calibration_defaults_and_tolerates_missing_fields() {
        let cal = ScreenCalibration::default();
        assert_eq!(cal.version, 1);
        assert_eq!(cal.captured_at_ms, 0);
        assert!(cal.regions.is_empty());
        assert_eq!(cal.ref_u, 0.5);
        assert_eq!(cal.ref_v, 0.5);
        assert_eq!(cal.trans_alpha_yaw, 60.0);
        assert_eq!(cal.trans_alpha_pitch, 60.0);
        // Empty object -> all defaults.
        let parsed: ScreenCalibration = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, cal);
        // Partial object -> defaults for the missing keys only.
        let parsed: ScreenCalibration = serde_json::from_str("{\"version\":2}").unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.trans_alpha_yaw, 60.0);
    }

    #[test]
    fn screen_calibration_json_round_trip_is_camel_case() {
        let cal = ScreenCalibration {
            captured_at_ms: 1_700_000_000_000,
            regions: vec![quad_region("hdmi-1")],
            ref_u: 0.48,
            ref_v: 0.52,
            trans_alpha_yaw: 88.0,
            trans_alpha_pitch: 72.0,
            ..ScreenCalibration::default()
        };
        let json = serde_json::to_string(&cal).unwrap();
        for key in [
            "version",
            "capturedAtMs",
            "regions",
            "refU",
            "refV",
            "transAlphaYaw",
            "transAlphaPitch",
            "screenId",
            "board",
        ] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing {key} in {json}"
            );
        }
        let back: ScreenCalibration = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cal);
    }
}
