//! FocusHalo halo overlay for Windows: a click-through, per-pixel-alpha
//! layered window on its own thread, software-rasterized ring plus dwell
//! progress arc, moved with the source-DC-less UpdateLayeredWindow fast path.
//!
//! Structure is cloned from `windows_drag_overlay.rs` (extension styles,
//! GetMessageW loop, reusable DIB renderer). Colors mirror the macOS
//! renderer's constants in `macos.rs`. Runtime behavior needs verification on
//! real Windows hardware.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, SetTimer, ShowWindow, UpdateLayeredWindow, MSG, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::halo::overlay::{HaloFrame, HaloVisual, OverlayBackend, OverlayScreenGeometry};
use crate::halo::params::FocusHaloSettings;

// Ring stroke width and gap keeping it inside the window bounds; on Windows
// everything below is physical pixels (logical × scale).
const RING_WIDTH: f64 = 6.0;
const MARGIN: f64 = 8.0;

// Packed ARGB (r, g, b, a), non-premultiplied; numerically matches macos.rs's
// NEUTRAL / NEUTRAL_SUSPENDED / PULSE / ACCENT floats.
const RING_IDLE: (u8, u8, u8, u8) = (158, 168, 184, 217);
const RING_SUSPENDED: (u8, u8, u8, u8) = (158, 168, 184, 77);
const RING_PULSE: (u8, u8, u8, u8) = (51, 204, 242, 255);
const ARC_ACCENT: (u8, u8, u8, u8) = (51, 204, 242, 242);

/// What the backend hands the window thread; `dirty` asks for a full repaint
/// (movement alone takes the cheap UpdateLayeredWindow path).
struct UiState {
    visible: bool,
    geometry: Option<OverlayScreenGeometry>,
    local: (f64, f64),
    frame: HaloFrame,
    dirty: bool,
}

struct Shared {
    close: AtomicBool,
    state: Mutex<UiState>,
}

/// Logical `local` (layout-board points) → physical window top-left:
/// physical center = native origin + local × scale, then subtract half the
/// physical diameter. Pure math (no Win32 types) so the test below reads as
/// plain arithmetic; it only runs under a Windows test build.
fn halo_window_origin(
    geometry: &OverlayScreenGeometry,
    local: (f64, f64),
    diameter: f64,
) -> (i32, i32) {
    let center_x = geometry.native_x + local.0 * geometry.scale;
    let center_y = geometry.native_y + local.1 * geometry.scale;
    let physical_diameter = diameter * geometry.scale;
    (
        (center_x - physical_diameter / 2.0).round() as i32,
        (center_y - physical_diameter / 2.0).round() as i32,
    )
}

pub struct Win32Overlay {
    shared: Arc<Shared>,
}

impl Win32Overlay {
    pub fn start(settings: &FocusHaloSettings) -> Self {
        let settings = settings.sanitized();
        let shared = Arc::new(Shared {
            close: AtomicBool::new(false),
            state: Mutex::new(UiState {
                visible: false,
                geometry: None,
                local: (0.0, 0.0),
                frame: HaloFrame {
                    visual: HaloVisual::Idle,
                    opacity: settings.halo_opacity,
                },
                dirty: false,
            }),
        });
        let diameter = settings.halo_diameter;
        let thread_shared = Arc::clone(&shared);
        std::thread::spawn(move || unsafe { run(thread_shared, diameter) });
        Win32Overlay { shared }
    }
}

impl Drop for Win32Overlay {
    fn drop(&mut self) {
        // The window thread destroys itself on the next timer tick; it owns
        // every GDI resource, so nothing else to clean here.
        self.shared.close.store(true, Ordering::Relaxed);
    }
}

impl OverlayBackend for Win32Overlay {
    fn show(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.visible = true;
            state.dirty = true;
        }
    }

    fn hide(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.visible = false;
        }
    }

    fn set_screen(&mut self, geometry: OverlayScreenGeometry) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.geometry = Some(geometry);
            // Scale may have changed → pixel size of the DIB changes.
            state.dirty = true;
        }
    }

    fn update(&mut self, local: (f64, f64), frame: &HaloFrame) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.local = local;
            state.frame = *frame;
            state.dirty = true;
        }
    }
}

unsafe fn run(shared: Arc<Shared>, diameter: f64) {
    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide("MyKVMHaloOverlay");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&window_class); // Fails harmlessly once already registered.
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        std::ptr::null(),
        WS_POPUP,
        0,
        0,
        1,
        1,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        log::warn!("halo overlay: could not create its window");
        return;
    }
    SetTimer(hwnd, 1, 16, None);

    // Window-thread-local render bookkeeping; `state` only carries inputs.
    let mut renderer: Option<FrameRenderer> = None;
    let mut shown = false;
    let mut last_frame: Option<HaloFrame> = None;
    let mut last_origin: Option<(i32, i32)> = None;
    let mut last_px = 0;

    let mut message: MSG = std::mem::zeroed();
    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
        if message.message == WM_TIMER && message.hwnd == hwnd {
            if shared.close.load(Ordering::Relaxed) {
                DestroyWindow(hwnd);
                PostQuitMessage(0);
                continue;
            }
            let snapshot = shared.state.lock().ok().map(|state| {
                (
                    state.visible,
                    state.geometry,
                    state.local,
                    state.frame,
                    state.dirty,
                )
            });
            if let Some((visible, geometry, local, frame, dirty)) = snapshot {
                let Some(geometry) = geometry else {
                    continue;
                };
                if !visible && shown {
                    ShowWindow(hwnd, SW_HIDE);
                    shown = false;
                } else if visible && !shown {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    shown = true;
                }
                if !shown {
                    continue;
                }
                let px = ((diameter * geometry.scale).round() as i32).max(1);
                let origin = halo_window_origin(&geometry, local, diameter);
                let repaint =
                    dirty || last_frame != Some(frame) || px != last_px || renderer.is_none();
                if repaint {
                    if renderer.is_none() || px != last_px {
                        // Recreate the DIB when the pixel size changes; on GDI
                        // failure keep last_px so the next tick retries.
                        let candidate = FrameRenderer::new(px);
                        if candidate.is_some() {
                            last_px = px;
                        }
                        renderer = candidate;
                    }
                    if let Some(r) = renderer.as_mut() {
                        let pixels = std::slice::from_raw_parts_mut(r.bits, (px * px) as usize);
                        pixels.fill(0);
                        paint(pixels, px, &frame);
                        r.present(hwnd, origin, frame.opacity);
                        last_frame = Some(frame);
                        last_origin = Some(origin);
                        if let Ok(mut state) = shared.state.lock() {
                            state.dirty = false;
                        }
                    }
                } else if last_origin != Some(origin) {
                    // Move only: no source DC means no repaint.
                    let point = POINT {
                        x: origin.0,
                        y: origin.1,
                    };
                    UpdateLayeredWindow(
                        hwnd,
                        std::ptr::null_mut(),
                        &point,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                    );
                    last_origin = Some(origin);
                }
            }
            continue;
        }
        DispatchMessageW(&message);
    }
    // Frees the GDI resources in reverse creation order.
    drop(renderer);
}

/// 每帧渲染器：screen DC、内存 DC 与 DIB 在整个浮层生命周期内复用，
/// 每帧只重绘像素并调用一次 UpdateLayeredWindow。像素边长可随 DPI 变化
/// （set_screen 换屏后重建 DIB）。Drop 时按创建的逆序释放。
struct FrameRenderer {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u32,
    size: i32,
}

impl FrameRenderer {
    unsafe fn new(size: i32) -> Option<Self> {
        let screen = GetDC(std::ptr::null_mut());
        let memory = CreateCompatibleDC(screen);
        let info = bitmap_info(size);
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(
            memory,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if screen.is_null() || memory.is_null() || bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            if !screen.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen);
            }
            return None;
        }
        let previous = SelectObject(memory, bitmap);
        Some(Self {
            screen,
            memory,
            bitmap,
            previous,
            bits: bits.cast::<u32>(),
            size,
        })
    }

    /// Repaint one frame and push it to the layered window. `opacity` maps to
    /// SourceConstantAlpha, mirroring the macOS backend's setAlphaValue:.
    unsafe fn present(&mut self, hwnd: HWND, at: (i32, i32), opacity: f64) {
        let size = SIZE {
            cx: self.size,
            cy: self.size,
        };
        let source = POINT { x: 0, y: 0 };
        let destination = POINT { x: at.0, y: at.1 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(
            hwnd,
            self.screen,
            &destination,
            &size,
            self.memory,
            &source,
            0,
            &blend,
            ULW_ALPHA,
        );
    }
}

impl Drop for FrameRenderer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.memory);
            ReleaseDC(std::ptr::null_mut(), self.screen);
        }
    }
}

/// Premultiplied BGRA into the top-down `size` × `size` canvas.
fn paint(pixels: &mut [u32], size: i32, frame: &HaloFrame) {
    let center = size as f64 / 2.0;
    // Radius of the ring's centerline; `soft` is the radial falloff half-width
    // so the band fades to nothing at both edges.
    let radius = center - MARGIN - RING_WIDTH / 2.0;
    let soft = RING_WIDTH / 2.0;
    let ring = match frame.visual {
        HaloVisual::SwitchPulse => RING_PULSE,
        HaloVisual::Suspended => RING_SUSPENDED,
        HaloVisual::Idle | HaloVisual::Candidate { .. } => RING_IDLE,
    };
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f64 + 0.5) - center;
            let dy = (y as f64 + 0.5) - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let band = ((distance - radius) / soft).abs();
            if band >= 1.0 {
                continue;
            }
            let coverage = (1.0 - band) as f32;
            let mut pixel = premultiply_argb(ring, coverage);
            // Progress arc: from 12 o'clock clockwise, sweeping `progress` of
            // the full circle, drawn over the ring in the accent color.
            if let HaloVisual::Candidate { progress } = frame.visual {
                let mut angle = dx.atan2(-dy); // 0 at top, +π/2 at 3 o'clock
                if angle < 0.0 {
                    angle += std::f64::consts::TAU;
                }
                if angle <= progress.clamp(0.0, 1.0) as f64 * std::f64::consts::TAU {
                    pixel = over(pixel, premultiply_argb(ARC_ACCENT, coverage));
                }
            }
            pixels[(y * size + x) as usize] = pixel;
        }
    }
}

/// Non-premultiplied ARGB → premultiplied BGRA with an extra coverage factor
/// from the soft ring edge.
fn premultiply_argb(rgba: (u8, u8, u8, u8), coverage: f32) -> u32 {
    let (r, g, b, a) = rgba;
    let alpha = ((a as f32 / 255.0) * coverage).clamp(0.0, 1.0);
    let scale = |channel: u8| ((channel as f32 / 255.0) * alpha * 255.0).round() as u32;
    let alpha8 = (alpha * 255.0).round() as u32;
    (alpha8 << 24) | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

/// Premultiplied "src over dst" for the arc drawn on top of the ring.
fn over(src: u32, dst: u32) -> u32 {
    let src_alpha = (src >> 24) & 0xFF;
    let inv = 255 - src_alpha;
    let mix = |shift: u32| {
        let s = (src >> shift) & 0xFF;
        let d = (dst >> shift) & 0xFF;
        s + d * inv / 255
    };
    let alpha = src_alpha + ((dst >> 24) & 0xFF) * inv / 255;
    (alpha << 24) | (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

fn bitmap_info(size: i32) -> BITMAPINFO {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size,
        biHeight: -size, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    info
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(scale: f64) -> OverlayScreenGeometry {
        OverlayScreenGeometry {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            scale,
            native_x: 100.0,
            native_y: 200.0,
        }
    }

    #[test]
    fn origin_centers_physical_window_on_the_local_point() {
        // 160 logical × scale 2 = 320 physical diameter → offset 160.
        assert_eq!(
            halo_window_origin(&geometry(2.0), (0.0, 0.0), 160.0),
            (-60, 40)
        );
        assert_eq!(
            halo_window_origin(&geometry(1.0), (500.0, 400.0), 160.0),
            (500, 460)
        );
    }

    #[test]
    fn ring_band_is_soft_and_center_is_transparent() {
        let size = 160;
        let mut pixels = vec![0u32; (size * size) as usize];
        paint(
            &mut pixels,
            size,
            &HaloFrame {
                visual: HaloVisual::Idle,
                opacity: 1.0,
            },
        );
        // Dead center sits inside the ring, never on the band.
        assert_eq!(pixels[((size / 2) * size + size / 2) as usize], 0);
        // A pixel on the ring's top centerline carries visible alpha.
        let band = pixels[((size as i32 / 2 - 69) * size + size / 2) as usize];
        assert!(band >> 24 > 0, "ring band pixel should have alpha");
    }

    #[test]
    fn progress_arc_brightens_the_band_it_covers() {
        let size = 160;
        let frame = |progress: f32| HaloFrame {
            visual: HaloVisual::Candidate { progress },
            opacity: 1.0,
        };
        let mut none = vec![0u32; (size * size) as usize];
        paint(&mut none, size, &frame(0.0));
        let mut full = vec![0u32; (size * size) as usize];
        paint(&mut full, size, &frame(1.0));
        let index = ((size as i32 / 2 - 69) * size + size / 2) as usize;
        let ring_alpha = none[index] >> 24;
        let arc_alpha = full[index] >> 24;
        assert!(
            arc_alpha > ring_alpha,
            "progress arc should brighten the band"
        );
    }

    #[test]
    fn suspended_diminishing_keeps_ring_shape() {
        let size = 160;
        let mut idle = vec![0u32; (size * size) as usize];
        paint(
            &mut idle,
            size,
            &HaloFrame {
                visual: HaloVisual::Idle,
                opacity: 1.0,
            },
        );
        let mut suspended = vec![0u32; (size * size) as usize];
        paint(
            &mut suspended,
            size,
            &HaloFrame {
                visual: HaloVisual::Suspended,
                opacity: 1.0,
            },
        );
        let index = ((size as i32 / 2 - 69) * size + size / 2) as usize;
        let idle_alpha = idle[index] >> 24;
        let suspended_alpha = suspended[index] >> 24;
        assert!(
            suspended_alpha > 0,
            "suspended ring should still be visible"
        );
        assert!(
            suspended_alpha < idle_alpha,
            "suspended ring should be dimmer than idle at the same pixel"
        );
    }
}
