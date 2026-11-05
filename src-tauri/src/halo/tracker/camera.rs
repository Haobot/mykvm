//! Platform-neutral camera capture for FocusHalo.
//!
//! [`CameraControl`] pushes camera frames (converted to packed RGB24, i.e.
//! [`Frame`]) to a caller-supplied callback; the FocusHalo engine feeds each
//! frame to [`PoseEngine::infer`](super::PoseEngine::infer). `create_camera`
//! returns the platform backend — AVFoundation on macOS; every other platform
//! currently gets a placeholder that always fails to start (Windows capture
//! is planned for P3+).

/// One packed RGB24 camera frame (`data.len() == width * height * 3`).
/// This is the tracker's [`Frame`](super::Frame); re-exported here so camera
/// code reads naturally.
pub use super::Frame;

/// Control handle for one camera capture session.
///
/// Implementations must be safe to move between threads (`Send`); actual
/// camera work happens inside `start`/`stop`, which the FocusHalo engine
/// calls from its own worker thread.
pub trait CameraControl: Send {
    /// Begin capture, pushing [`Frame`]s to `on_frame` from the
    /// implementation's frame thread until [`CameraControl::stop`].
    ///
    /// Idempotent per the contract: a second `start` tears down any previous
    /// session first. The returned error string is meant for UI hints.
    fn start(&mut self, on_frame: Box<dyn Fn(Frame) + Send>) -> Result<(), String>;

    /// Stop capture and release the camera. Safe to call when not running.
    fn stop(&mut self);
}

/// Placeholder backend for platforms without a camera implementation yet.
struct UnsupportedCamera;

impl CameraControl for UnsupportedCamera {
    fn start(&mut self, _on_frame: Box<dyn Fn(Frame) + Send>) -> Result<(), String> {
        Err("camera unsupported on this platform".to_string())
    }

    fn stop(&mut self) {}
}

/// Build the platform camera backend.
pub fn create_camera() -> Box<dyn CameraControl> {
    #[cfg(target_os = "macos")]
    {
        Box::new(super::camera_macos::MacCamera::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(UnsupportedCamera)
    }
}

/// Convert a row-major BGRA buffer (one `bgra` byte per pixel, `stride`
/// bytes per row, `stride >= width * 4`) into tightly packed RGB24.
///
/// BGRA is the only universally available camera pixel format, and the
/// capture layer hands the raw row data over; the channel order swap happens
/// here so the tracker can consume plain RGB24.
pub(crate) fn bgra_to_rgb(bgra: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    debug_assert!(stride >= width * 4, "stride must cover the pixel row");
    let mut rgb = vec![0u8; width * height * 3];
    for row in 0..height {
        let src = &bgra[row * stride..row * stride + width * 4];
        let dst = &mut rgb[row * width * 3..(row + 1) * width * 3];
        for (px, out) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
            out[0] = px[2]; // R <- B
            out[1] = px[1]; // G <- G
            out[2] = px[0]; // B <- R
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_camera_start_fails() {
        let mut camera = UnsupportedCamera;
        let err = camera
            .start(Box::new(|_| {}))
            .expect_err("placeholder must refuse to start");
        assert!(err.contains("unsupported"), "got: {err}");
    }

    #[test]
    fn bgra_to_rgb_swaps_channels_exactly() {
        // Two pixels, tightly packed: (B0 G0 R0 A0) (B1 G1 R1 A1).
        let bgra = [10, 20, 30, 255, 40, 50, 60, 255];
        let rgb = bgra_to_rgb(&bgra, 2, 1, 8);
        assert_eq!(rgb, vec![30, 20, 10, 60, 50, 40]);
    }

    #[test]
    fn bgra_to_rgb_skips_row_padding() {
        // Width 3 with stride 16: each row carries 12 pixel bytes + 4 pad.
        let mut bgra = Vec::new();
        for row in 0..2 {
            for col in 0..3usize {
                let v = (row * 10 + col) as u8;
                bgra.extend_from_slice(&[v, v + 1, v + 2, 255]);
            }
            bgra.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // padding
        }
        let rgb = bgra_to_rgb(&bgra, 3, 2, 16);
        assert_eq!(rgb.len(), 3 * 2 * 3);
        // Row 0: pixels 0,1,2 -> (2,1,0) (3,2,1) (4,3,2).
        assert_eq!(&rgb[..9], &[2, 1, 0, 3, 2, 1, 4, 3, 2]);
        // Row 1 starts right after row 0's pixels (padding skipped).
        assert_eq!(&rgb[9..], &[12, 11, 10, 13, 12, 11, 14, 13, 12]);
    }

    #[test]
    fn bgra_to_rgb_handles_odd_width() {
        // 1x3, stride 4: pure channel swap, no padding anywhere.
        let bgra = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255];
        let rgb = bgra_to_rgb(&bgra, 1, 3, 4);
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4, 9, 8, 7]);
    }
}
