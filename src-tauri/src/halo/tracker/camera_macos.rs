//! AVFoundation camera capture for FocusHalo (macOS).
//!
//! One [`AVCaptureSession`] feeds BGRA frames from the default (built-in)
//! camera into an [`AVCaptureVideoDataOutput`]; the delegate copies each
//! pixel buffer out, converts it to RGB24 and calls the user callback.
//!
//! Threading model:
//! - All session operations (`start`/`stop`) are marshalled onto a dedicated
//!   worker thread (see [`worker_loop`]) so they run serially and never on
//!   the app's main thread — `startRunning`/`stopRunning` are blocking and
//!   can take hundreds of milliseconds.
//! - Frames arrive on a serial libdispatch queue owned by this module
//!   ([`FRAME_QUEUE_LABEL`]); AVFoundation drops frames for us when the
//!   queue falls behind (`alwaysDiscardsLateVideoFrames`).
//! - The pixel buffer is copied out while locked, and the lock is released
//!   before any user callback runs — inference (the expensive part) never
//!   happens under the CoreVideo lock.
//!
//! Built with the official `objc2` crate family (0.6.x / 0.3.x), sharing the
//! versions tauri/tao/arboard already pull in.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_av_foundation::{
    AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession,
    AVCaptureSessionPreset640x480, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBuffer,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObjectProtocol, NSString};

use super::camera::{bgra_to_rgb, CameraControl};
use super::Frame;

/// Serial queue label for sample-buffer delivery (visible in Instruments).
const FRAME_QUEUE_LABEL: &str = "com.xzhpl.mykvm.halo.camera.frames";
/// OS thread name for the session worker.
const WORKER_THREAD_NAME: &str = "halo-camera";
/// How long `start`/`stop` wait for the worker before giving up on it.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

/// Milliseconds on an arbitrary monotonic-ish clock (process start).
fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Objective-C ivars of the frame delegate. The mutex exists purely to make
/// the ivars `Sync` (a class is only `Send` when its ivars are, and the
/// retained delegate must move onto the capture worker thread); AVFoundation
/// guarantees all calls happen on the single serial frame queue, so the lock
/// is never contended.
struct FrameDelegateIvars {
    on_frame: Mutex<Option<Box<dyn Fn(Frame) + Send>>>,
}

define_class!(
    /// Sample-buffer delegate: converts each delivered BGRA pixel buffer into
    /// an RGB [`Frame`] and hands it to the stored callback.
    #[unsafe(super(NSObject))]
    #[ivars = FrameDelegateIvars]
    struct FrameDelegate;

    impl FrameDelegate {
        /// AVFoundation's per-frame callback. Note the ordering constraints:
        /// copy while the pixel buffer is locked, then unlock BEFORE calling
        /// the Rust closure (which will run inference).
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn capture_output_did_output_sample_buffer(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            emit_frame(&self.ivars(), sample_buffer);
        }
    }

    unsafe impl NSObjectProtocol for FrameDelegate {}
);

// The protocol's method is optional; providing it as an ObjC method above is
// exactly how objc2 expects optional protocol methods to be implemented.
unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {}

impl FrameDelegate {
    fn new(on_frame: Box<dyn Fn(Frame) + Send>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FrameDelegateIvars {
            on_frame: Mutex::new(Some(on_frame)),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Detach the callback so a frame in flight cannot fire after stop.
    fn clear_callback(&self) {
        let mut guard = self
            .ivars()
            .on_frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = None;
    }
}

/// Extract one BGRA frame from the sample buffer and dispatch it.
fn emit_frame(ivars: &FrameDelegateIvars, sample_buffer: &CMSampleBuffer) {
    let guard = ivars
        .on_frame
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(callback) = guard.as_ref() else {
        return;
    };
    let Some(copied) = copy_bgra_locked(sample_buffer) else {
        return;
    };
    // The pixel buffer is unlocked again at this point; conversion and the
    // user callback run without holding CoreVideo locks.
    let (width, height, stride, bgra) = copied;
    let frame = Frame {
        width: width as u32,
        height: height as u32,
        data: bgra_to_rgb(&bgra, width, height, stride),
        ts_ms: mono_ms(),
    };
    callback(frame);
}

/// Copy the pixel buffer's BGRA rows out while holding the CoreVideo lock.
///
/// Returns `(width, height, bytes_per_row, packed BGRA bytes)`, or `None`
/// for unusable buffers (no image buffer, null base address, bad stride).
fn copy_bgra_locked(sample_buffer: &CMSampleBuffer) -> Option<(usize, usize, usize, Vec<u8>)> {
    // CMSampleBufferGetImageBuffer returns a CVImageBuffer; CVPixelBuffer is
    // a type alias of it, so the retained reference passes through directly.
    let pixel_buffer = unsafe { sample_buffer.image_buffer() }?;
    let copied = unsafe {
        let status = CVPixelBufferLockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        if status != kCVReturnSuccess {
            log::warn!("halo camera: CVPixelBufferLockBaseAddress failed: {status}");
            None
        } else {
            let copied = copy_bgra_rows(&pixel_buffer);
            CVPixelBufferUnlockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
            copied
        }
    };
    copied
}

/// Row-by-row copy honouring `bytesPerRow` (the row stride usually carries
/// padding, so the buffer cannot be copied as one flat slice).
fn copy_bgra_rows(pixel_buffer: &CVPixelBuffer) -> Option<(usize, usize, usize, Vec<u8>)> {
    let width = CVPixelBufferGetWidth(pixel_buffer);
    let height = CVPixelBufferGetHeight(pixel_buffer);
    let stride = CVPixelBufferGetBytesPerRow(pixel_buffer);
    let base = CVPixelBufferGetBaseAddress(pixel_buffer);
    if width == 0 || height == 0 || base.is_null() || stride < width * 4 {
        return None;
    }
    let mut bgra = Vec::with_capacity(height * width * 4);
    for row in 0..height {
        // SAFETY: `base` points at `height` rows of `stride` bytes while the
        // buffer is locked (see the caller); row indices stay in range.
        let src =
            unsafe { std::slice::from_raw_parts(base.add(row * stride) as *const u8, width * 4) };
        bgra.extend_from_slice(src);
    }
    Some((width, height, stride, bgra))
}

/// A running capture chain: everything needed to tear the session down again.
struct CaptureChain {
    session: Retained<AVCaptureSession>,
    input: Retained<AVCaptureDeviceInput>,
    output: Retained<AVCaptureVideoDataOutput>,
    /// AVFoundation does NOT retain its sample-buffer delegate, so the
    /// delegate must be kept alive here for as long as capture runs.
    delegate: Retained<FrameDelegate>,
    #[allow(dead_code)]
    frame_queue: DispatchRetained<DispatchQueue>,
}

impl CaptureChain {
    /// Stop capture and disconnect everything, in AVFoundation's expected
    /// order (stopRunning, then reconfigure). Must run on the worker thread.
    fn teardown(&mut self) {
        unsafe {
            self.session.stopRunning();
            self.session.beginConfiguration();
            self.session.removeInput(&self.input);
            self.session.removeOutput(&self.output);
            self.session.commitConfiguration();
            // Setting a nil delegate also requires a nil queue.
            self.output.setSampleBufferDelegate_queue(None, None);
        }
        self.delegate.clear_callback();
    }
}

/// Build and start a full capture chain on the worker thread.
fn start_capture(
    on_frame: Box<dyn Fn(Frame) + Send>,
    device_id: Option<&str>,
    slot: &mut Option<CaptureChain>,
) -> Result<(), String> {
    // Idempotent start: tear down any previous chain first.
    if let Some(mut chain) = slot.take() {
        chain.teardown();
    }

    // SAFETY: plain ObjC runtime / AVFoundation calls; every failure path
    // returns an error instead of unwrapping anything.
    unsafe {
        // Camera selection: explicit device id when set, otherwise the system
        // default video device (the built-in camera).
        let device = match device_id {
            Some(id) => {
                let unique_id = NSString::from_str(id);
                AVCaptureDevice::deviceWithUniqueID(&unique_id)
                    .ok_or_else(|| format!("halo camera: no camera with id '{id}'"))?
            }
            None => {
                let media_type =
                    AVMediaTypeVideo.ok_or("halo camera: AVMediaTypeVideo constant unavailable")?;
                AVCaptureDevice::defaultDeviceWithMediaType(media_type)
                    .ok_or("halo camera: no video capture device found")?
            }
        };
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|err| {
            format!(
                "halo camera: failed to open device input: {}",
                err.localizedDescription()
            )
        })?;

        let session = AVCaptureSession::new();
        session.beginConfiguration();
        // 640x480 at 30 fps is plenty for head-pose inference at low CPU cost.
        session.setSessionPreset(AVCaptureSessionPreset640x480);

        if !session.canAddInput(&input) {
            return Err("halo camera: session rejected the camera input".to_string());
        }
        session.addInput(&input);

        let output = AVCaptureVideoDataOutput::new();
        // Pin the pixel format to 32BGRA: available on every Mac camera
        // without extra conversion passes.
        let format_key: &NSString = kCVPixelBufferPixelFormatTypeKey.as_ref();
        let format_value: Retained<AnyObject> = NSNumber::new_u32(kCVPixelFormatType_32BGRA)
            .into_super()
            .into_super()
            .into_super();
        let video_settings = NSDictionary::from_slices(&[format_key], &[&*format_value]);
        output.setVideoSettings(Some(&video_settings));
        output.setAlwaysDiscardsLateVideoFrames(true);

        let delegate = FrameDelegate::new(on_frame);
        let frame_queue = DispatchQueue::new(FRAME_QUEUE_LABEL, DispatchQueueAttr::SERIAL);
        // Type-erased protocol object; AVFoundation keeps a non-retaining
        // reference, the concrete delegate stays alive in the capture chain.
        let delegate_obj: Retained<
            ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate>,
        > = ProtocolObject::from_retained(delegate.clone());
        // SAFETY: the delegate outlives the output (held in the same chain),
        // and the queue is a fresh serial queue.
        output.setSampleBufferDelegate_queue(Some(&delegate_obj), Some(&frame_queue));

        if !session.canAddOutput(&output) {
            return Err("halo camera: session rejected the video output".to_string());
        }
        session.addOutput(&output);
        session.commitConfiguration();
        session.startRunning();

        *slot = Some(CaptureChain {
            session,
            input,
            output,
            delegate,
            frame_queue,
        });
    }
    Ok(())
}

/// One command per start/stop; `start` and `stop` both block on a reply so
/// the public API stays synchronous and errors are surfaced to the UI.
enum Command {
    Start {
        on_frame: Box<dyn Fn(Frame) + Send>,
        device_id: Option<String>,
        reply: Sender<Result<(), String>>,
    },
    Stop {
        reply: Sender<()>,
    },
    Shutdown,
}

/// Serializes every session operation onto one thread, in command order.
fn worker_loop(rx: Receiver<Command>) {
    let mut chain: Option<CaptureChain> = None;
    for command in rx {
        match command {
            Command::Start {
                on_frame,
                device_id,
                reply,
            } => {
                let _ = reply.send(start_capture(on_frame, device_id.as_deref(), &mut chain));
            }
            Command::Stop { reply } => {
                if let Some(mut session) = chain.take() {
                    session.teardown();
                }
                let _ = reply.send(());
            }
            Command::Shutdown => break,
        }
    }
    // Shutdown with a live session: release it the same way.
    if let Some(mut session) = chain.take() {
        session.teardown();
    }
}

/// macOS [`CameraControl`] implementation backed by AVFoundation.
pub struct MacCamera {
    tx: Option<Sender<Command>>,
    worker: Option<JoinHandle<()>>,
    /// Preferred camera (AVFoundation uniqueID); `None` = system default.
    device_id: Option<String>,
}

impl MacCamera {
    /// Create the control handle and its worker thread. The camera itself is
    /// only opened (and permission requested) in [`CameraControl::start`].
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_string())
            .spawn(move || worker_loop(rx))
            .map_err(|e| log::warn!("halo camera: failed to spawn worker thread: {e}"))
            .ok();
        MacCamera {
            tx: Some(tx),
            worker,
            device_id: None,
        }
    }

    /// Select a camera by AVFoundation uniqueID; `None` resets to the
    /// default (built-in) camera. Takes effect on the next `start`.
    pub fn set_device_id(&mut self, device_id: Option<String>) {
        self.device_id = device_id;
    }

    fn send_command(&self, command: Command) -> Result<(), String> {
        let tx = self
            .tx
            .as_ref()
            .ok_or("halo camera: worker thread unavailable")?;
        tx.send(command)
            .map_err(|_| "halo camera: worker exited".to_string())
    }
}

impl Default for MacCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraControl for MacCamera {
    fn start(&mut self, on_frame: Box<dyn Fn(Frame) + Send>) -> Result<(), String> {
        // Idempotent: stop any previous session before starting a new one.
        self.stop();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_command(Command::Start {
            on_frame,
            device_id: self.device_id.clone(),
            reply: reply_tx,
        })?;
        reply_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "halo camera: start timed out".to_string())?
    }

    fn stop(&mut self) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(Command::Stop { reply: reply_tx }).is_ok() {
            let _ = reply_rx.recv_timeout(COMMAND_TIMEOUT);
        }
    }
}

impl Drop for MacCamera {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
