//! FocusHalo head tracker: one camera frame in, one [`HeadSample`] out.
//!
//! [`PoseEngine`] owns the two ONNX sessions (YuNet detection + 6DRepNet pose)
//! and runs the full P0-spike pipeline synchronously; the FocusHalo engine
//! thread calls `infer` from its own thread, so the sessions sit behind a
//! mutex purely to keep the `&self` interface (inference itself stays serial).

pub mod pose;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use image::RgbImage;
use ort::environment::Environment;
use ort::session::{builder::GraphOptimizationLevel, Session};

/// Detection model file expected inside the models directory.
pub const DETECTION_MODEL_FILE: &str = "face_detection_yunet_2023mar_dynamic.onnx";
/// Head-pose model file expected inside the models directory.
pub const POSE_MODEL_FILE: &str = "head-pose-mobilenetv2.onnx";

/// onnxruntime allows exactly one Environment per process ("only one
/// environment is allowed per process"), so every PoseEngine shares this one.
static ORT_ENV: OnceLock<Environment> = OnceLock::new();

/// Return the process-wide ONNX Runtime environment, building it on first use.
fn shared_environment() -> Result<&'static Environment, String> {
    if let Some(env) = ORT_ENV.get() {
        return Ok(env);
    }
    let env = ort::init()
        .with_name("mykvm-focus-halo")
        .build()
        .map_err(|e| format!("onnxruntime init failed: {e}"))?;
    // A concurrent constructor may have won the race; both handles wrap the
    // same underlying runtime, so losing the insert is fine.
    let _ = ORT_ENV.set(env);
    Ok(ORT_ENV
        .get()
        .expect("environment was just inserted or won earlier"))
}

/// One camera frame: tightly packed RGB24 (`data.len() == width*height*3`).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Capture timestamp, milliseconds on an arbitrary monotonic-ish clock.
    pub ts_ms: u64,
}

/// One inference result. When no face is found every angle is 0 and
/// `face_detected` is false; sign convention: positive pitch = looking up
/// (see `pose::estimate_pose` for the sign flip vs the raw model).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadSample {
    pub ts_ms: u64,
    pub face_detected: bool,
    pub yaw_deg: f64,
    pub pitch_deg: f64,
    pub roll_deg: f64,
}

/// Candidate model directories, most likely first.
fn model_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // `cargo run` / `cargo test` set CARGO_MANIFEST_DIR to src-tauri.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(Path::new(&manifest).join("resources").join("halo-models"));
    }
    // Packaged app: models next to the executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("resources").join("halo-models"));
            candidates.push(dir.join("halo-models"));
        }
    }
    // Last resort: working-directory relative (manual runs).
    candidates.push(PathBuf::from("resources").join("halo-models"));
    candidates
}

fn models_complete(dir: &Path) -> bool {
    dir.join(DETECTION_MODEL_FILE).is_file() && dir.join(POSE_MODEL_FILE).is_file()
}

/// Resolve the halo model directory: the first candidate that actually
/// contains both ONNX files wins; when nothing is found the manifest-dir
/// candidate is returned so [`PoseEngine::new`] can report a precise error
/// (callers treat a failed `new` as "feature disabled with a hint").
pub fn default_models_dir() -> PathBuf {
    let candidates = model_dir_candidates();
    for dir in &candidates {
        if models_complete(dir) {
            return dir.clone();
        }
    }
    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("resources/halo-models"))
}

/// Two-stage head-pose engine (detection + pose ONNX sessions).
pub struct PoseEngine {
    det: Mutex<Session>,
    pose: Mutex<Session>,
    det_dims: (u32, u32),
    pose_dims: (u32, u32),
}

impl PoseEngine {
    /// Load both models from `models_dir`. Fails (instead of panicking) when
    /// a file is missing or ONNX Runtime cannot load it; the caller disables
    /// the feature and surfaces the message.
    pub fn new(models_dir: &Path) -> Result<Self, String> {
        let det_path = models_dir.join(DETECTION_MODEL_FILE);
        let pose_path = models_dir.join(POSE_MODEL_FILE);
        for path in [&det_path, &pose_path] {
            if !path.is_file() {
                return Err(format!(
                    "FocusHalo model missing: {} (expected both {} and {} in {})",
                    path.display(),
                    DETECTION_MODEL_FILE,
                    POSE_MODEL_FILE,
                    models_dir.display()
                ));
            }
        }

        let env = shared_environment()?;
        let det = Session::builder(env)
            .map_err(|e| format!("detection session builder failed: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("detection optimization level failed: {e}"))?
            .commit_from_file(&det_path)
            .map_err(|e| format!("failed to load {}: {e}", det_path.display()))?;
        let pose = Session::builder(env)
            .map_err(|e| format!("pose session builder failed: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("pose optimization level failed: {e}"))?
            .commit_from_file(&pose_path)
            .map_err(|e| format!("failed to load {}: {e}", pose_path.display()))?;

        let det_dims = pose::model_input_dims(&det, pose::DET_FALLBACK_DIMS);
        let pose_dims = pose::model_input_dims(&pose, pose::POSE_FALLBACK_DIMS);
        Ok(PoseEngine {
            det: Mutex::new(det),
            pose: Mutex::new(pose),
            det_dims,
            pose_dims,
        })
    }

    /// Run the full two-stage pipeline on one frame. Inference failures are
    /// reported as "no face" (and logged) rather than poisoning the engine.
    pub fn infer(&self, frame: &Frame) -> HeadSample {
        let sample = HeadSample {
            ts_ms: frame.ts_ms,
            face_detected: false,
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let Some(img) = RgbImage::from_raw(frame.width, frame.height, frame.data.clone()) else {
            log::warn!(
                "halo tracker: frame buffer size mismatch ({}x{} needs {} bytes, got {})",
                frame.width,
                frame.height,
                frame.width as usize * frame.height as usize * 3,
                frame.data.len()
            );
            return sample;
        };

        let result = (|| -> Result<HeadSample, Box<dyn std::error::Error>> {
            let mut det = self.det.lock().map_err(lock_poisoned)?;
            let faces = pose::detect_faces(&mut det, &img, self.det_dims)?;
            drop(det);
            let Some(face) = faces.into_iter().next() else {
                return Ok(sample);
            };
            let (cx, cy, cw, ch) = pose::crop_window(&img, &face, pose::CROP_EXPAND);
            let crop = image::imageops::crop_imm(&img, cx, cy, cw, ch).to_image();
            let mut pose_session = self.pose.lock().map_err(lock_poisoned)?;
            let [pitch, yaw, roll] = pose::estimate_pose(&mut pose_session, &crop, self.pose_dims)?;
            drop(pose_session);
            Ok(HeadSample {
                face_detected: true,
                yaw_deg: yaw,
                pitch_deg: pitch,
                roll_deg: roll,
                ..sample
            })
        })();

        match result {
            Ok(s) => s,
            Err(e) => {
                log::warn!("halo tracker: inference failed: {e}");
                sample
            }
        }
    }
}

fn lock_poisoned<T>(_poison: T) -> Box<dyn std::error::Error> {
    "halo tracker: session mutex poisoned".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_assets_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/halo/tracker/test_assets")
    }

    #[test]
    fn default_models_dir_points_at_resources() {
        // On a dev machine (models present) it must resolve to the resources
        // directory; without models it still returns a candidate path, and the
        // no-panic/no-Err contract of the resolver is what this asserts.
        let dir = default_models_dir();
        let manifest_candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("halo-models");
        if models_complete(&manifest_candidate) {
            assert_eq!(dir, manifest_candidate);
        } else {
            assert!(
                !models_complete(&dir),
                "resolver must return an existing dir first"
            );
        }
    }

    #[test]
    fn pose_engine_reports_missing_models_as_error() {
        let empty = std::env::temp_dir().join("halo-no-models-dir");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        let err = match PoseEngine::new(&empty) {
            Err(e) => e,
            Ok(_) => panic!("expected an error when the models dir lacks the ONNX files"),
        };
        assert!(
            err.contains(DETECTION_MODEL_FILE),
            "error should name the missing file: {err}"
        );
    }

    #[test]
    fn infer_rejects_wrong_buffer_sizes() {
        let manifest_candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("halo-models");
        if !models_complete(&manifest_candidate) {
            eprintln!("skipping: halo models not present at {manifest_candidate:?}");
            return;
        }
        let engine = PoseEngine::new(&manifest_candidate).unwrap();
        let frame = Frame {
            width: 2,
            height: 2,
            data: vec![0u8; 3], // too short
            ts_ms: 0,
        };
        let s = engine.infer(&frame);
        assert!(!s.face_detected);
        assert_eq!((s.yaw_deg, s.pitch_deg, s.roll_deg), (0.0, 0.0, 0.0));
    }

    /// Integration: a public-domain portrait must yield a detected face with
    /// plausible angles. Skips (does not fail) when models or the asset are
    /// missing, so CI without model files stays green.
    #[test]
    fn infer_detects_portrait_face_with_plausible_pose() {
        let models_dir = default_models_dir();
        if !models_complete(&models_dir) {
            eprintln!("skipping: halo models not present at {:?}", models_dir);
            return;
        }
        let asset = test_assets_dir().join("portrait.jpg");
        if !asset.is_file() {
            eprintln!("skipping: test portrait missing at {:?}", asset);
            return;
        }
        let img = image::open(&asset)
            .unwrap_or_else(|e| panic!("test asset {asset:?} should decode: {e}"))
            .to_rgb8();
        let frame = Frame {
            width: img.width(),
            height: img.height(),
            data: img.into_raw(),
            ts_ms: 42,
        };
        let engine = PoseEngine::new(&models_dir).unwrap();
        let s = engine.infer(&frame);
        assert!(s.face_detected, "portrait should contain a detectable face");
        assert!(
            s.yaw_deg.abs() < 45.0 && s.pitch_deg.abs() < 45.0,
            "frontal portrait pose should be sane, got yaw={} pitch={}",
            s.yaw_deg,
            s.pitch_deg
        );
        assert_eq!(s.ts_ms, 42);
    }
}
