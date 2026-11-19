//! Raw ONNX pre/post-processing for the FocusHalo two-stage head-pose
//! pipeline, ported from the P0 spike (`halo-pose-spike`):
//!
//! 1. YuNet face detection (opencv_zoo 2023mar, dynamic 320): input is
//!    1x3xHxW BGR NCHW with raw 0..255 values; the four "heads" output
//!    (cls/obj/bbox/kps per stride) are decoded exactly like OpenCV's
//!    `FaceDetectorYN::postProcess`.
//! 2. Head pose (6DRepNet-MobileNetV2, yakhyo/head-pose-estimation): input is
//!    1x3x224x224 RGB NCHW with /255 then ImageNet mean/std; output is a 1x3x3
//!    rotation matrix decomposed to Euler angles with the upstream
//!    `rotation_matrix_to_euler`.

use image::{imageops::FilterType, RgbImage};
use ndarray::Array4;
use ort::session::Session;
use ort::value::{Tensor, ValueType};

/// YuNet runs on a dynamic-input model; 320x320 is the P0 spike's choice.
pub const DET_FALLBACK_DIMS: (u32, u32) = (320, 320);
/// Pose model input is static 224x224.
pub const POSE_FALLBACK_DIMS: (u32, u32) = (224, 224);
/// YuNet score floor for a valid face.
pub const SCORE_THRESHOLD: f32 = 0.6;
/// Head-crop expansion factor, mirrors yakhyo expand_bbox().
pub const CROP_EXPAND: f32 = 0.2;
/// ImageNet normalization constants (RGB order), mirrors yakhyo preprocess().
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// YuNet raw head output names, matching OpenCV FaceDetectorYN:
/// cls/obj (1 anchor value per cell), bbox (dx,dy,dw,dh), kps (5x dx,dy);
/// strides 8/16/32.
const DET_HEADS: [[&str; 3]; 4] = [
    ["cls_8", "cls_16", "cls_32"],
    ["obj_8", "obj_16", "obj_32"],
    ["bbox_8", "bbox_16", "bbox_32"],
    ["kps_8", "kps_16", "kps_32"],
];
const STRIDES: [u32; 3] = [8, 16, 32];
const NMS_IOU_THRESHOLD: f32 = 0.3;

/// One candidate face box (post-NMS), in full-resolution image coordinates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Face {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    score: f32,
}

impl Face {
    /// Face-box center as normalized (u, v) in 0..1, image origin top-left
    /// (v grows downward). This is the seat-translation probe the screen
    /// calibration consumes (see `screen_map::PosePoint`).
    pub(crate) fn center_uv(&self, img_width: u32, img_height: u32) -> (f64, f64) {
        let cx = self.x + self.w / 2.0;
        let cy = self.y + self.h / 2.0;
        (
            if img_width > 0 {
                f64::from(cx) / f64::from(img_width)
            } else {
                0.0
            },
            if img_height > 0 {
                f64::from(cy) / f64::from(img_height)
            } else {
                0.0
            },
        )
    }
}

/// RGB8 image -> 1x3xHxW BGR float32 NCHW, raw 0..255 values (YuNet expects no
/// normalization).
pub(crate) fn bgr_nchw(img: &RgbImage) -> Array4<f32> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let src = img.as_raw();
    let mut out = vec![0f32; 3 * w * h];
    for (i, px) in src.chunks_exact(3).enumerate() {
        out[i] = px[2] as f32; // B
        out[w * h + i] = px[1] as f32; // G
        out[2 * w * h + i] = px[0] as f32; // R
    }
    Array4::from_shape_vec((1, 3, h, w), out).expect("contiguous rgb buffer")
}

/// RGB8 image -> 1x3xHxW RGB float32 NCHW, /255 then (x - mean) / std
/// (ImageNet, RGB order).
pub(crate) fn rgb_nchw_normalized(img: &RgbImage, w: u32, h: u32) -> Array4<f32> {
    let resized = image::imageops::resize(img, w, h, FilterType::Triangle);
    let src = resized.as_raw();
    let n = (w as usize) * (h as usize);
    let mut out = vec![0f32; 3 * n];
    for (i, px) in src.chunks_exact(3).enumerate() {
        for c in 0..3 {
            out[c * n + i] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    Array4::from_shape_vec((1, 3, h as usize, w as usize), out).expect("contiguous rgb buffer")
}

/// Read a model's declared input (W, H) from session metadata; falls back to
/// `fallback` for dynamic or unexpected shapes.
pub(crate) fn model_input_dims(session: &Session, fallback: (u32, u32)) -> (u32, u32) {
    if let ValueType::Tensor { shape, .. } = session.inputs()[0].dtype() {
        let dims = shape.as_ref();
        if dims.len() == 4 && dims[2] > 0 && dims[3] > 0 {
            return (dims[3] as u32, dims[2] as u32);
        }
    }
    fallback
}

/// Greedy NMS on face boxes, mirrors dnn::NMSBoxes(scoreThreshold,
/// nmsThreshold, topK).
fn nms(mut faces: Vec<Face>, iou_threshold: f32) -> Vec<Face> {
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    'outer: for f in faces {
        for k in &keep {
            let x0 = f.x.max(k.x);
            let y0 = f.y.max(k.y);
            let x1 = (f.x + f.w).min(k.x + k.w);
            let y1 = (f.y + f.h).min(k.y + k.h);
            let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
            let union = f.w * f.h + k.w * k.h - inter;
            if union > 0.0 && inter / union > iou_threshold {
                continue 'outer;
            }
        }
        keep.push(f);
    }
    keep
}

/// Run the YuNet raw-heads model and return NMS-filtered faces, highest score
/// first, with coordinates back-scaled to `img` space.
///
/// Decode math is a faithful port of OpenCV FaceDetectorYN::postProcess
/// (modules/objdetect/src/face_detect.cpp): score = sqrt(clamp(cls)*clamp(obj)),
/// cx = (col + dx)*stride, cy = (row + dy)*stride, w = exp(dw)*stride,
/// h = exp(dh)*stride.
pub(crate) fn detect_faces(
    det: &mut Session,
    img: &RgbImage,
    (dw, dh): (u32, u32),
) -> Result<Vec<Face>, Box<dyn std::error::Error>> {
    let in_name = det.inputs()[0].name().to_string();
    let input = bgr_nchw(&image::imageops::resize(img, dw, dh, FilterType::Triangle));
    let outputs = det.run(ort::inputs![in_name.as_str() => Tensor::from_array(input)?])?;

    let mut faces: Vec<Face> = Vec::new();
    let sx = img.width() as f32 / dw as f32;
    let sy = img.height() as f32 / dh as f32;

    for (i, &stride) in STRIDES.iter().enumerate() {
        let (_, cls) = outputs[DET_HEADS[0][i]].try_extract_tensor::<f32>()?;
        let (_, obj) = outputs[DET_HEADS[1][i]].try_extract_tensor::<f32>()?;
        let (_, bbox) = outputs[DET_HEADS[2][i]].try_extract_tensor::<f32>()?;
        let (_, kps) = outputs[DET_HEADS[3][i]].try_extract_tensor::<f32>()?;

        let cols = (dw / stride) as usize;
        let rows = (dh / stride) as usize;
        let cells = cols * rows;
        if cls.len() < cells
            || obj.len() < cells
            || bbox.len() < cells * 4
            || kps.len() < cells * 10
        {
            return Err(format!(
                "stride {stride} tensors too small for {cols}x{rows} grid: cls={} obj={} bbox={} kps={}",
                cls.len(),
                obj.len(),
                bbox.len(),
                kps.len()
            )
            .into());
        }

        for r in 0..rows {
            for c in 0..cols {
                let idx = r * cols + c;
                let cls_score = cls[idx].clamp(0.0, 1.0);
                let obj_score = obj[idx].clamp(0.0, 1.0);
                let score = (cls_score * obj_score).sqrt();
                if score < SCORE_THRESHOLD {
                    continue;
                }
                let cx = (c as f32 + bbox[idx * 4]) * stride as f32;
                let cy = (r as f32 + bbox[idx * 4 + 1]) * stride as f32;
                let w = bbox[idx * 4 + 2].exp() * stride as f32;
                let h = bbox[idx * 4 + 3].exp() * stride as f32;
                faces.push(Face {
                    x: (cx - w / 2.0) * sx,
                    y: (cy - h / 2.0) * sy,
                    w: w * sx,
                    h: h * sy,
                    score,
                });
            }
        }
    }

    let mut faces = nms(faces, NMS_IOU_THRESHOLD);
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(faces)
}

/// Expand a face box by `factor` of its size on each side, clamp to image
/// bounds. Returns (x, y, w, h) of the crop window.
pub(crate) fn crop_window(img: &RgbImage, face: &Face, factor: f32) -> (u32, u32, u32, u32) {
    let dx = (factor * face.h).round() as i32;
    let dy = (factor * face.w).round() as i32;
    let x0 = (face.x as i32 - dx).max(0);
    let y0 = (face.y as i32 - dy).max(0);
    let x1 = ((face.x + face.w) as i32 + dx).min(img.width() as i32 - 1);
    let y1 = ((face.y + face.h) as i32 + dy).min(img.height() as i32 - 1);
    (
        x0 as u32,
        y0 as u32,
        (x1 - x0 + 1).max(1) as u32,
        (y1 - y0 + 1).max(1) as u32,
    )
}

/// Rotation matrix (1x3x3, row-major) -> (pitch, yaw, roll) in degrees.
/// Matches yakhyo rotation_matrix_to_euler() exactly. On live camera input
/// the decoded pitch is positive when the user looks UP (verified on hardware
/// 2026-11-05); the mapping layer consumes that convention directly.
pub(crate) fn rotation_matrix_to_euler(r: &[f32]) -> [f64; 3] {
    let at = |a: usize, b: usize| r[a * 3 + b] as f64;
    let sy = (at(0, 0) * at(0, 0) + at(1, 0) * at(1, 0)).sqrt();
    let singular = sy < 1e-6;
    let pitch = if singular {
        (-at(1, 2)).atan2(at(1, 1))
    } else {
        at(2, 1).atan2(at(2, 2))
    };
    let yaw = (-at(2, 0)).atan2(sy);
    let roll = if singular {
        0.0
    } else {
        at(1, 0).atan2(at(0, 0))
    };
    [pitch, yaw, roll].map(f64::to_degrees)
}

/// Crop the face window, resize to the model input, run the pose model, and
/// return (pitch, yaw, roll) degrees with FocusHalo's sign convention
/// (positive pitch = looking up, positive yaw/roll follow the model).
pub(crate) fn estimate_pose(
    pose: &mut Session,
    img: &RgbImage,
    (pw, ph): (u32, u32),
) -> Result<[f64; 3], Box<dyn std::error::Error>> {
    let in_name = pose.inputs()[0].name().to_string();
    let out_name = pose.outputs()[0].name().to_string();

    let input = rgb_nchw_normalized(img, pw, ph);
    let outputs = pose.run(ort::inputs![in_name.as_str() => Tensor::from_array(input)?])?;
    let (shape, data) = outputs[out_name.as_str()].try_extract_tensor::<f32>()?;
    if data.len() < 9 {
        return Err(format!(
            "unexpected pose output shape {shape:?} (len {})",
            data.len()
        )
        .into());
    }
    let [pitch, yaw, roll] = rotation_matrix_to_euler(&data[..9]);
    // Live-camera calibration (2026-11-05): the raw model pitch is already
    // positive when looking up, so it maps straight through. The earlier
    // negation (based on the upstream notebook's plot convention) inverted
    // vertical motion on real hardware; users who still see an inverted
    // vertical axis flip it with the `invertPitch` setting instead.
    Ok([pitch, yaw, roll])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity rotation decomposes to all-zero angles.
    #[test]
    fn identity_matrix_is_zero_pose() {
        let r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let [pitch, yaw, roll] = rotation_matrix_to_euler(&r);
        assert!(pitch.abs() < 1e-9 && yaw.abs() < 1e-9 && roll.abs() < 1e-9);
    }

    /// Pure axis rotations decode to the matching single angle. Row-major
    /// rotation matrices: Rx(pitch), Ry(yaw), Rz(roll). The decomposer
    /// already returns degrees; f32 matrix storage means ~1e-4 degree noise,
    /// not exact equality.
    #[test]
    fn axis_rotations_decode_to_single_angles() {
        let (c, s) = (30f64.to_radians().cos(), 30f64.to_radians().sin());

        // Rx: pitch only.
        let rx = [1.0, 0.0, 0.0, 0.0, c, -s, 0.0, s, c].map(|v| v as f32);
        let [pitch, yaw, roll] = rotation_matrix_to_euler(&rx);
        assert!((pitch - 30.0).abs() < 1e-4, "pitch {pitch}");
        assert!(yaw.abs() < 1e-4 && roll.abs() < 1e-4);

        // Ry: yaw only.
        let ry = [c, 0.0, s, 0.0, 1.0, 0.0, -s, 0.0, c].map(|v| v as f32);
        let [pitch, yaw, roll] = rotation_matrix_to_euler(&ry);
        assert!((yaw - 30.0).abs() < 1e-4, "yaw {yaw}");
        assert!(pitch.abs() < 1e-4 && roll.abs() < 1e-4);

        // Rz: roll only.
        let rz = [c, -s, 0.0, s, c, 0.0, 0.0, 0.0, 1.0].map(|v| v as f32);
        let [pitch, yaw, roll] = rotation_matrix_to_euler(&rz);
        assert!((roll - 30.0).abs() < 1e-4, "roll {roll}");
        assert!(pitch.abs() < 1e-4 && yaw.abs() < 1e-4);
    }

    #[test]
    fn bgr_nchw_channel_orders_match_planar_layout() {
        let mut img = RgbImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgb([10, 20, 30]));
        img.put_pixel(1, 0, image::Rgb([40, 50, 60]));
        let arr = bgr_nchw(&img);
        // Channel 0 holds B, channel 1 G, channel 2 R; pixels row-major.
        assert_eq!(arr[[0, 0, 0, 0]], 30.0);
        assert_eq!(arr[[0, 0, 0, 1]], 60.0);
        assert_eq!(arr[[0, 1, 0, 0]], 20.0);
        assert_eq!(arr[[0, 2, 0, 0]], 10.0);
    }

    #[test]
    fn crop_window_expands_and_clamps() {
        let mut img = RgbImage::new(100, 100);
        img.put_pixel(0, 0, image::Rgb([255, 255, 255]));
        let face = Face {
            x: 40.0,
            y: 40.0,
            w: 20.0,
            h: 20.0,
            score: 1.0,
        };
        let (x, y, w, h) = crop_window(&img, &face, 0.2);
        // 0.2 * 20 = 4 px expansion on each side: 36..64 inclusive -> 29 px.
        assert_eq!((x, y, w, h), (36, 36, 29, 29));
        let edge = Face {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            score: 1.0,
        };
        let (x, y, w, h) = crop_window(&img, &edge, 0.2);
        // Clamped at the top-left corner: 0..12 inclusive -> 13 px.
        assert_eq!((x, y, w, h), (0, 0, 13, 13));
    }
}
