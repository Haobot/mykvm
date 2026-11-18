//! 布局画布点选磁贴后，在对应物理显示器上闪一圈光圈的原生窗口实现。
//!
//! 之前的实现是透明 WebView 窗口（screen-indicator.html）：真机上发现
//! WKWebView 在"初建于 tao 的错误坐标、随后跨显示器 setFrame"时不重绘
//! （Tauri/WebKit 已知问题），主屏初始位置碰巧近似正确所以能画，外接屏
//! 的窗口框架回读全部正确但内容永不出现。改为两端都走 halo 浮层同款的
//! 原生窗口路径——同构浮层在本机外接屏上实测可见：
//!
//! - macOS：无边框点击穿透 NSWindow + CAShapeLayer 圆角描边，Core
//!   Animation 驱动 0.45s×3 交替脉冲，1.4s 后 orderOut+release。
//! - Windows：WS_EX_LAYERED 分层窗 + GDI DIB 像素脉冲（复用 halo win32
//!   的 paint/breathing 思路），自带消息循环线程，1.4s 自毁。
//! - Linux：非运行平台，记日志即可。
//!
//! 入口 [`flash_logical_rect`] 的矩形与 `detect_local_screens` 同一空间
//! （逻辑坐标、左上原点、y 向下），可在任意线程调用。

use std::time::Duration;

/// 光圈停留时长：与 3 拍脉冲动画（0.45s × 3 交替 ≈ 1.35s）对齐。
pub(crate) const FLASH_DURATION: Duration = Duration::from_millis(1400);

/// CSS 里的 `rgba(56, 150, 255, 0.92)`——与前端画布选中描边同色系。
/// 通道统一存 0-255，alpha 同乘 255；两端渲染前各自归一。
pub(crate) const RING_RGBA: (f64, f64, f64, f64) = (56.0, 150.0, 255.0, 0.92 * 255.0);

/// CSS `0.45s ease-in-out 3 alternate`（透明度 0.4→1）的线性近似：
/// 每拍在两端之间往返，奇数拍反向。纯函数，单测锁定端点。
pub(crate) fn pulse_alpha(elapsed_ms: f64) -> f64 {
    const HALF_MS: f64 = 450.0;
    const BEATS: f64 = 3.0;
    if elapsed_ms >= HALF_MS * BEATS {
        return 1.0;
    }
    let step = elapsed_ms / HALF_MS;
    let beat = step.floor() as u32;
    let frac = step - f64::from(beat);
    if beat.is_multiple_of(2) {
        0.4 + 0.6 * frac
    } else {
        1.0 - 0.6 * frac
    }
}

/// 在逻辑矩形对应的物理显示器上闪光圈。
pub fn flash_logical_rect(rect: (f64, f64, f64, f64)) {
    let (_, _, width, height) = rect;
    if !(width > 1.0 && height > 1.0) {
        return;
    }
    imp::flash(rect);
}

#[cfg(target_os = "macos")]
use macos_imp as imp;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use other_imp as imp;
#[cfg(target_os = "windows")]
use windows_imp as imp;

// ---------------------------------------------------------------------------
// macOS：NSWindow + CAShapeLayer（结构克隆 halo/overlay/macos.rs）。
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos_imp {
    use std::ffi::c_void;

    use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    use super::{pulse_alpha, RING_RGBA};

    /// 光圈内缘离屏幕边的距离、描边宽与圆角（点），对齐原 html 的样式。
    const RING_INSET: f64 = 8.0;
    const RING_WIDTH: f64 = 10.0;
    const RING_RADIUS: f64 = 18.0;
    const PULSE_HALF_SECONDS: f64 = 0.45;

    // 与 NSRect 同布局（原点在主屏左下、y 向上，点坐标）。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    // objc2 要求 msg_send 的返回/参数类型实现 Encode；四个 f64 与 NSRect 的
    // ABI 完全一致。
    unsafe impl objc2::encode::Encode for NsRect {
        const ENCODING: objc2::encode::Encoding = objc2::encode::Encoding::Array(4, &f64::ENCODING);
    }

    // CGPathCreateWithRoundedCGRect 已从 macOS 27 SDK 移除，用 mutable+add。
    extern "C" {
        fn CGPathCreateMutable() -> *mut c_void;
        fn CGPathAddRoundedRect(
            path: *mut c_void,
            transform: *const CGAffineTransform,
            rect: CGRect,
            corner_width: f64,
            corner_height: f64,
        );
        fn CGColorCreateGenericRGB(red: f64, green: f64, blue: f64, alpha: f64) -> *mut c_void;
    }
    #[link(name = "System")]
    extern "C" {
        fn dispatch_async_f(
            queue: *mut c_void,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
    }
    extern "C" {
        static _dispatch_main_q: c_void;
    }

    pub(super) fn flash(rect: (f64, f64, f64, f64)) {
        // NSWindow 只能在主线程创建；Tauri 命令线程与输入数据链都走这里。
        on_main(create_flash, Box::into_raw(Box::new(rect)) as *mut c_void);
    }

    fn on_main(work: extern "C" fn(*mut c_void), ctx: *mut c_void) {
        unsafe { dispatch_async_f(std::ptr::addr_of!(_dispatch_main_q).cast_mut(), ctx, work) }
    }

    extern "C" fn create_flash(ctx: *mut c_void) {
        let target = unsafe { Box::from_raw(ctx as *mut (f64, f64, f64, f64)) };
        unsafe {
            let Some(frame) = match_screen_frame(*target) else {
                log::warn!("screen indicator: no NSScreen found");
                return;
            };
            let Some(window) = build_flash_window(frame) else {
                return;
            };
            log::info!(
                "screen indicator covering display at cocoa ({:.0},{:.0}) [{:.0}x{:.0}]",
                frame.x,
                frame.y,
                frame.w,
                frame.h
            );
            schedule_close(window);
        }
    }

    /// 在 NSScreen 列表里按点级几何匹配目标屏，返回它自己的 Cocoa frame；
    /// 匹配失败时按主屏点高翻转兜底（不再退回 tao 坐标——Retina 主屏下
    /// tao 用物理像素高翻转逻辑 y，整窗会飞出屏幕）。
    unsafe fn match_screen_frame(target: (f64, f64, f64, f64)) -> Option<NsRect> {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return None;
        }
        let first: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let main_frame: NsRect = msg_send![first, frame];
        let (tx, ty, tw, th) = target;
        for index in 0..count {
            let screen: *mut AnyObject = msg_send![screens, objectAtIndex: index];
            let frame: NsRect = msg_send![screen, frame];
            let visual_y = main_frame.h - (frame.y + frame.h);
            let same_size = (frame.w - tw).abs() < 1.0 && (frame.h - th).abs() < 1.0;
            let same_origin = (frame.x - tx).abs() < 1.0 && (visual_y - ty).abs() < 1.0;
            if same_size && same_origin {
                return Some(frame);
            }
        }
        Some(NsRect {
            x: tx,
            y: main_frame.h - ty - th,
            w: tw,
            h: th,
        })
    }

    /// halo 同款样板：无边框、透明、无阴影、点击穿透、屏幕保护层、跨空间
    /// （CanJoinAllSpaces | FullScreenAuxiliary）窗口，承载 layer-backed
    /// 内容视图。后台 app 建窗会被抑制显示，用 orderFrontRegardless 强制上屏。
    unsafe fn build_flash_window(frame: NsRect) -> Option<*mut AnyObject> {
        let alloc: *mut AnyObject = msg_send![class!(NSWindow), alloc];
        let window: *mut AnyObject = msg_send![alloc,
            initWithContentRect: frame,
            styleMask: 0usize,      // borderless
            backing: 2usize,        // buffered
            defer: false,
        ];
        if window.is_null() {
            return None;
        }
        let window_ref = &*window;
        let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window_ref, setBackgroundColor: clear];
        let _: () = msg_send![window_ref, setOpaque: false];
        let _: () = msg_send![window_ref, setHasShadow: false];
        let _: () = msg_send![window_ref, setIgnoresMouseEvents: true];
        // tauri 的 always-on-top 是 floating 层（3），会被菜单栏（25）和
        // 程序坞（20）遮住光圈的上/下边缘；抬到 NSScreenSaverWindowLevel。
        let _: () = msg_send![window_ref, setLevel: 1000_i64];
        // CanJoinAllSpaces (1<<0) | FullScreenAuxiliary (1<<8)。
        let _: () = msg_send![window_ref, setCollectionBehavior: 257_usize];

        let view_alloc: *mut AnyObject = msg_send![class!(NSView), alloc];
        let view: *mut AnyObject = msg_send![view_alloc, initWithFrame: frame];
        if !view.is_null() {
            let view_ref = &*view;
            let _: () = msg_send![view_ref, setWantsLayer: true];
            let _: () = msg_send![window_ref, setContentView: view_ref];
            // setContentView 已 retain，交掉我们的 +1。
            let _: () = msg_send![view_ref, release];
            let host: *mut AnyObject = msg_send![view_ref, layer];
            if !host.is_null() {
                attach_ring(&*host, frame, window_ref);
            }
        }
        let _: () = msg_send![window_ref, orderFrontRegardless];
        Some(window)
    }

    /// 圆角描边光圈层：内缩 RING_INSET 让描边完整落在窗内，只描边不填充，
    /// 内部完全透明。path 与 strokeColor 都是 strong 属性，+1 引用直接移交。
    unsafe fn attach_ring(host: &AnyObject, frame: NsRect, window: &AnyObject) {
        let layer_alloc: *mut AnyObject = msg_send![class!(CAShapeLayer), alloc];
        let layer: *mut AnyObject = msg_send![layer_alloc, init];
        if layer.is_null() {
            return;
        }
        let layer_ref = &*layer;
        let path = rounded_rect_path(frame) as *mut AnyObject;
        let _: () = msg_send![layer_ref, setPath: path];
        let (r, g, b, a) = RING_RGBA;
        let stroke =
            CGColorCreateGenericRGB(r / 255.0, g / 255.0, b / 255.0, a / 255.0) as *mut AnyObject;
        let _: () = msg_send![layer_ref, setStrokeColor: stroke];
        let _: () = msg_send![layer_ref, setFillColor: std::ptr::null::<AnyObject>()];
        let scale: f64 = msg_send![window, backingScaleFactor];
        let _: () = msg_send![layer_ref, setContentsScale: scale];
        let _: () = msg_send![layer_ref, setLineWidth: RING_WIDTH];
        let _: () =
            msg_send![layer_ref, setFrame: NsRect { x: 0.0, y: 0.0, w: frame.w, h: frame.h }];
        start_pulse(layer_ref);
        let _: () = msg_send![host, addSublayer: layer_ref];
        // addSublayer 已 retain，交掉我们的 +1。
        let _: () = msg_send![layer_ref, release];
    }

    unsafe fn rounded_rect_path(frame: NsRect) -> *mut c_void {
        let path = CGPathCreateMutable();
        if path.is_null() {
            return std::ptr::null_mut();
        }
        let inner = CGRect {
            origin: CGPoint {
                x: RING_INSET,
                y: RING_INSET,
            },
            size: CGSize {
                width: frame.w - RING_INSET * 2.0,
                height: frame.h - RING_INSET * 2.0,
            },
        };
        CGPathAddRoundedRect(path, std::ptr::null(), inner, RING_RADIUS, RING_RADIUS);
        path
    }

    /// 与 html 版同拍数的 Core Animation 脉冲：透明度 0.4↔1，0.45s 一拍，
    /// 自动往复 3 次（≈1.35s）。
    unsafe fn start_pulse(layer: &AnyObject) {
        let key = NSString::from_str("opacity");
        // `alloc` + `initWithKeyPath:` 已从 macOS 27 runtime 移除（halo 踩过
        // 崩溃），必须走 `animationWithKeyPath:` 类工厂；返回 autoreleased
        // 实例，layer 在 addAnimation 时 retain，主队列的 pool 负责余量。
        let anim: *mut AnyObject = msg_send![class!(CABasicAnimation), animationWithKeyPath: &*key];
        drop(key);
        if anim.is_null() {
            return;
        }
        let anim_ref = &*anim;
        let from_alloc: *mut AnyObject = msg_send![class!(NSNumber), alloc];
        let from: *mut AnyObject = msg_send![from_alloc, initWithDouble: pulse_alpha(0.0)];
        let to_alloc: *mut AnyObject = msg_send![class!(NSNumber), alloc];
        let to: *mut AnyObject = msg_send![to_alloc, initWithDouble: pulse_alpha(450.0)];
        let _: () = msg_send![anim_ref, setFromValue: &*from];
        let _: () = msg_send![anim_ref, setToValue: &*to];
        let _: () = msg_send![&*from, release];
        let _: () = msg_send![&*to, release];
        let _: () = msg_send![anim_ref, setDuration: PULSE_HALF_SECONDS];
        let _: () = msg_send![anim_ref, setAutoreverses: true];
        let _: () = msg_send![anim_ref, setRepeatCount: 3.0_f64];
        let _: () = msg_send![anim_ref, setRemovedOnCompletion: false];
        let _: () = msg_send![layer, addAnimation: anim_ref, forKey: std::ptr::null::<AnyObject>()];
    }

    /// 倒计时归零后回主队列收尾：orderOut 再 release，窗与内容一起释放。
    fn schedule_close(window: *mut AnyObject) {
        let ptr = window as usize;
        std::thread::spawn(move || {
            std::thread::sleep(super::FLASH_DURATION);
            on_main(close_flash, Box::into_raw(Box::new(ptr)) as *mut c_void);
        });
    }

    extern "C" fn close_flash(ctx: *mut c_void) {
        let ptr = unsafe { Box::from_raw(ctx as *mut usize) };
        let window = *ptr as *mut AnyObject;
        unsafe {
            let window_ref = &*window;
            let _: () = msg_send![window_ref, orderOut: std::ptr::null::<AnyObject>()];
            let _: () = msg_send![window_ref, release];
        }
    }
}

// ---------------------------------------------------------------------------
// Windows：分层窗 + GDI 像素脉冲（结构克隆 halo/overlay/win32.rs）。
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_imp {
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT, SIZE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
        GetMonitorInfoW, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, HMONITOR,
        MONITORINFO,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostQuitMessage, RegisterClassW, SetTimer, ShowWindow, UpdateLayeredWindow, MSG,
        SW_SHOWNOACTIVATE, ULW_ALPHA, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    use super::{pulse_alpha, RING_RGBA};

    // 光圈几何（逻辑点）：内缘离屏幕边 8、描边 10、圆角 18——描边中线
    // 离边 13，画进位图时全部乘目标屏 DPI scale 变物理像素。
    const RING_INSET: f64 = 8.0;
    const RING_WIDTH: f64 = 10.0;
    const RING_RADIUS: f64 = 18.0;
    // 重绘节拍：100ms 一帧足够平滑，远轻于 halo 边框的 16ms。
    const TIMER_MS: u32 = 100;

    pub(super) fn flash(rect: (f64, f64, f64, f64)) {
        // 光圈窗自带消息循环线程，与输入数据链解耦。
        std::thread::spawn(move || unsafe { run_flash(rect) });
    }

    unsafe fn run_flash(target: (f64, f64, f64, f64)) {
        let Some((rc, scale)) = pick_monitor(target) else {
            log::warn!("screen indicator: no monitor matched logical rect {target:?}");
            return;
        };
        let (x, y, width, height) = physical_rect(target, scale);
        if width <= 0 || height <= 0 {
            return;
        }
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("MyKVMScreenFlash");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&window_class); // 重复注册无害，多窗共用一个类。
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log::warn!("screen indicator: window create failed");
            return;
        }
        let Some(mut renderer) = FrameRenderer::new(width, height) else {
            DestroyWindow(hwnd);
            return;
        };
        let (pad, band, radius) = ring_geometry(scale);
        log::info!(
            "screen indicator at physical ({x},{y}) [{width}x{height}] scale={scale} monitor=({},{},{},{})",
            rc.left,
            rc.top,
            rc.right,
            rc.bottom
        );
        renderer.present(hwnd, (x, y), 1.0);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, 1, TIMER_MS, None);
        let start = Instant::now();
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            if message.message == WM_TIMER && message.hwnd == hwnd {
                let elapsed = start.elapsed();
                if elapsed >= super::FLASH_DURATION {
                    DestroyWindow(hwnd);
                    PostQuitMessage(0);
                    continue;
                }
                let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
                let pixels = unsafe {
                    std::slice::from_raw_parts_mut(renderer.bits, (width * height) as usize)
                };
                paint_border(
                    pixels,
                    width,
                    height,
                    pad,
                    band,
                    radius,
                    RING_RGBA,
                    pulse_alpha(elapsed_ms),
                );
                renderer.present(hwnd, (x, y), 1.0);
                continue;
            }
            DispatchMessageW(&message);
        }
    }

    /// 枚举显示器并用各自的有效 DPI 还原逻辑矩形，挑选与目标逻辑矩形
    /// 匹配的那块（`detect_local_screens` 用同一 DPI 换算，边距 ≤2px）。
    /// 没有误差 <16px 的命中就不闪——宁可无提示也不闪到错误的屏上。
    unsafe fn pick_monitor(target: (f64, f64, f64, f64)) -> Option<(RECT, f64)> {
        let (tx, ty, tw, th) = target;
        let mut monitors: Vec<(HMONITOR, RECT, f64)> = Vec::new();
        let list = Box::into_raw(Box::new(&mut monitors));
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_monitor),
            list as LPARAM,
        );
        drop(Box::from_raw(list));
        monitors
            .into_iter()
            .map(|(handle, rc, scale)| {
                let error = (f64::from(rc.left) / scale - tx).abs()
                    + (f64::from(rc.top) / scale - ty).abs()
                    + (f64::from(rc.right - rc.left) / scale - tw).abs()
                    + (f64::from(rc.bottom - rc.top) / scale - th).abs();
                (rc, scale, error)
            })
            .min_by(|left, right| left.2.total_cmp(&right.2))
            .filter(|(_, _, error)| *error < 16.0)
            .map(|(rc, scale, _)| (rc, scale))
    }

    unsafe extern "system" fn enum_monitor(
        handle: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> i32 {
        let monitors = &mut *(lparam as *mut Vec<(HMONITOR, RECT, f64)>);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(handle, &mut info) == 0 {
            return 1;
        }
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let scale = if GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
            && dpi_x > 0
        {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        monitors.push((handle, info.rcMonitor, scale));
        1
    }

    /// 逻辑矩形 × 目标屏 DPI scale → 物理像素矩形（左上原点、y 向下）。
    fn physical_rect(target: (f64, f64, f64, f64), scale: f64) -> (i32, i32, i32, i32) {
        let (x, y, width, height) = target;
        (
            (x * scale).round() as i32,
            (y * scale).round() as i32,
            (width * scale).round() as i32,
            (height * scale).round() as i32,
        )
    }

    fn ring_geometry(scale: f64) -> (i32, i32, i32) {
        (
            ((RING_INSET + RING_WIDTH / 2.0) * scale).round() as i32,
            (RING_WIDTH * scale).round() as i32,
            (RING_RADIUS * scale).round() as i32,
        )
    }

    /// Non-premultiplied ARGB → premultiplied BGRA，额外乘一层描边透明度
    /// （脉冲值），与 halo 渲染器的像素格式一致。
    fn premultiply_argb(rgba: (f64, f64, f64, f64), coverage: f64) -> u32 {
        let (r, g, b, a) = rgba;
        let alpha = ((a / 255.0) * coverage).clamp(0.0, 1.0);
        let scale = |channel: f64| ((channel / 255.0) * alpha * 255.0).round() as u32;
        let alpha8 = (alpha * 255.0).round() as u32;
        (alpha8 << 24) | (scale(r) << 16) | (scale(g) << 8) | scale(b)
    }

    /// 判断点是否落在圆角矩形内（含边界），容忍退化输入（w/h ≤ 0、圆角
    /// 过大），与 halo 的实现同逻辑。
    fn inside_rounded(px: f64, py: f64, x: f64, y: f64, w: f64, h: f64, r: f64) -> bool {
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        if px < x || px > x + w || py < y || py > y + h {
            return false;
        }
        let r = r.max(0.0).min(w / 2.0).min(h / 2.0);
        let cx = px.clamp(x + r, x + w - r);
        let cy = py.clamp(y + r, y + h - r);
        let dx = px - cx;
        let dy = py - cy;
        dx * dx + dy * dy <= r * r
    }

    /// 把空心圆角描边带画进 `pixels`（width × height，premultiplied BGRA，
    /// top-down）。带骑在离边 `pad` 像素的圆角轮廓上，带宽 `band`，内部
    /// 全透明。`alpha` 缩放描边（调用方喂脉冲值）。
    fn paint_border(
        pixels: &mut [u32],
        width: i32,
        height: i32,
        pad: i32,
        band: i32,
        radius: i32,
        rgba: (f64, f64, f64, f64),
        alpha: f64,
    ) {
        let outline_w = f64::from(width - 2 * pad);
        let outline_h = f64::from(height - 2 * pad);
        if outline_w <= 0.0 || outline_h <= 0.0 {
            return;
        }
        let half_band = f64::from(band) / 2.0;
        // 包住描边带的内外两个圆角矩形。
        let outer = f64::from(pad) - half_band;
        let outer_w = outline_w + f64::from(band);
        let outer_h = outline_h + f64::from(band);
        let outer_r = f64::from(radius) + half_band;
        let inner = f64::from(pad) + half_band;
        let inner_w = outline_w - f64::from(band);
        let inner_h = outline_h - f64::from(band);
        let inner_r = (f64::from(radius) - half_band).max(0.0);
        let stroke = premultiply_argb(rgba, alpha);
        for y in 0..height {
            for x in 0..width {
                let px = f64::from(x) + 0.5;
                let py = f64::from(y) + 0.5;
                if inside_rounded(px, py, outer, outer, outer_w, outer_h, outer_r)
                    && !inside_rounded(px, py, inner, inner, inner_w, inner_h, inner_r)
                {
                    pixels[(y * width + x) as usize] = stroke;
                }
            }
        }
    }

    /// 每帧渲染器：screen DC、内存 DC 与 DIB 在整个光圈生命周期内复用，
    /// 每帧只重绘像素并调用一次 UpdateLayeredWindow。Drop 时按创建的逆序
    /// 释放 GDI 资源。
    struct FrameRenderer {
        screen: HDC,
        memory: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        bits: *mut u32,
        width: i32,
        height: i32,
    }

    impl FrameRenderer {
        unsafe fn new(width: i32, height: i32) -> Option<Self> {
            let screen = GetDC(std::ptr::null_mut());
            let memory = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
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
                width,
                height,
            })
        }

        /// 重绘一帧并推给分层窗；`at` 同时就是窗口位置（ULW 顺带定位）。
        unsafe fn present(&mut self, hwnd: HWND, at: (i32, i32), opacity: f64) {
            let size = SIZE {
                cx: self.width,
                cy: self.height,
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

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn physical_rect_scales_and_rounds() {
            assert_eq!(
                physical_rect((1512.0, -458.0, 2560.0, 1440.0), 1.25),
                (1890, -573, 3200, 1800)
            );
            assert_eq!(
                physical_rect((0.0, 0.0, 1512.0, 982.0), 2.0),
                (0, 0, 3024, 1964)
            );
        }

        #[test]
        fn ring_geometry_straddles_the_outline() {
            // pad = inset + half band；scale 1.0 时不放大。
            assert_eq!(ring_geometry(1.0), (13, 10, 18));
            assert_eq!(ring_geometry(1.25), (16, 13, 23));
        }

        #[test]
        fn paint_border_leaves_interior_transparent() {
            let mut pixels = vec![0u32; 40 * 30];
            paint_border(&mut pixels, 40, 30, 3, 2, 2, (0.0, 0.0, 255.0, 255.0), 1.0);
            let center = pixels[15 * 40 + 20];
            assert_eq!(center, 0, "光圈内部必须全透明");
            // 描边带上至少有一条不透明像素。
            assert!(pixels.iter().any(|p| (*p >> 24) > 0));
        }
    }
}

// ---------------------------------------------------------------------------
// 其他平台：不闪光圈（输入链路本身也未在这些平台启用）。
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod other_imp {
    pub(super) fn flash(_rect: (f64, f64, f64, f64)) {
        log::info!("screen indicator: not supported on this platform");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_starts_dim_and_hits_full_by_first_turn() {
        assert!((pulse_alpha(0.0) - 0.4).abs() < 1e-9);
        assert!((pulse_alpha(225.0) - 0.7).abs() < 1e-9);
        assert!((pulse_alpha(450.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pulse_alternates_within_bounds_and_settles_at_full() {
        // 第二拍反向：中点从 1.0 回落。
        assert!((pulse_alpha(675.0) - 0.7).abs() < 1e-9);
        for ms in [0.0, 100.0, 450.0, 900.0, 1349.0] {
            let alpha = pulse_alpha(ms);
            assert!((0.4..=1.0).contains(&alpha), "alpha {alpha} at {ms}ms");
        }
        // 三拍后定格在 1.0。
        assert!((pulse_alpha(1400.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn flash_duration_covers_three_pulse_beats() {
        // 3 拍 × 450ms = 1350ms，窗口多留 50ms 收尾。
        assert_eq!(FLASH_DURATION, Duration::from_millis(1400));
    }
}
