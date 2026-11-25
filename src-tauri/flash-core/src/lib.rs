//! 布局画布点选磁贴后，在对应物理显示器上闪一圈光圈的原生窗口实现。
//!
//! 之前的实现是透明 WebView 窗口（screen-indicator.html）：真机上发现
//! WKWebView 在"初建于 tao 的错误坐标、随后跨显示器 setFrame"时不重绘
//! （Tauri/WebKit 已知问题），主屏初始位置碰巧近似正确所以能画，外接屏
//! 的窗口框架回读全部正确但内容永不出现。改为两端都走 halo 浮层同款的
//! 原生窗口路径——同构浮层在本机外接屏上实测可见：
//!
//! - macOS：无边框点击穿透 NSWindow + CAShapeLayer 双层虚化光晕
//!   （外层弥散光晕 + 内层亮芯，见 [`glow`]），Core Animation 驱动
//!   0.45s×3 交替脉冲，1.4s 后 orderOut+release。
//! - Windows：WS_EX_LAYERED 分层窗 + GDI DIB 像素脉冲（复用 halo win32
//!   的 paint/breathing 思路），自带消息循环线程，1.4s 自毁。
//! - Linux：非运行平台，记日志即可。
//!
//! 全屏应用会把光圈挤下层级（Windows）或挤出 Space 前排（macOS），
//! 两端在存活期内周期性重申置顶（macOS 300ms×4；Windows 随 100ms 重绘
//! tick 逐次重申 HWND_TOPMOST）。
//!
//! 入口 [`flash_logical_rect`] 的矩形与 `detect_local_screens` 同一空间
//! （逻辑坐标、左上原点、y 向下），可在任意线程调用。

use std::time::Duration;

// 剖面只被 macOS/Windows 绘制器与单测消费；Linux（空实现平台）豁免死代码。
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", test)),
    allow(dead_code)
)]
pub(crate) mod glow;

/// 光圈停留时长：与 3 拍脉冲动画（0.45s × 3 交替 ≈ 1.35s）对齐。
pub const FLASH_DURATION: Duration = Duration::from_millis(1400);

/// CSS `0.45s ease-in-out 3 alternate`（透明度 0.35→1）的线性近似：
/// 每拍在两端之间往返，奇数拍反向。纯函数，单测锁定端点。
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", test)),
    allow(dead_code)
)]
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
        0.35 + 0.65 * frac
    } else {
        1.0 - 0.65 * frac
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
    use std::time::Duration;

    use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    use super::{glow, pulse_alpha, FLASH_DURATION};

    /// 存活期内置顶重申：全屏应用会把光圈挤出 Space 前排/压下层级，
    /// 4 次 × 300ms 重申 + 尾部收尾 = 停留时长 1400ms。
    const REASSERT_INTERVAL: Duration = Duration::from_millis(300);
    const REASSERT_TIMES: u32 = 4;

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
        fn CGPathRelease(path: *mut c_void);
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
        // 先按小矩形建窗再用 setFrame: 撑到目标框。init 的大 rect 传参在
        // macOS 27 上属高风险路径（本轮排查中窗口服务器里出现的 30px 窄条
        // 最终证实是无关的启动辅助窗，但 init 传参与 setFrame 相比缺乏
        // 独立验证），setFrame 路径已被多轮 frame 回读证实可靠。
        let seed = NsRect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let window: *mut AnyObject = msg_send![alloc,
            initWithContentRect: seed,
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
        // CanJoinAllSpaces (1<<0) | FullScreenAuxiliary (1<<8)。
        //
        // 注意：setCollectionBehavior: 在 macOS 27 上也是整数 ABI（f64 兜底
        // 会写入垃圾标志直接崩溃），usize 传参回读位值正确。
        let _: () = msg_send![window_ref, setCollectionBehavior: 257_usize];

        let view_alloc: *mut AnyObject = msg_send![class!(NSView), alloc];
        let view: *mut AnyObject = msg_send![view_alloc, initWithFrame: seed];
        if !view.is_null() {
            let view_ref = &*view;
            let _: () = msg_send![view_ref, setWantsLayer: true];
            let _: () = msg_send![window_ref, setContentView: view_ref];
            // setContentView 已 retain，交掉我们的 +1。
            let _: () = msg_send![view_ref, release];
            let host: *mut AnyObject = msg_send![view_ref, layer];
            if !host.is_null() {
                attach_glow(&*host, frame, window_ref);
            }
        }
        // 撑到目标全屏框（setFrame 的 rect 传参已证可靠），并把内容视图
        // 跟着放大（borderless 窗口无自动跟随）。
        let _: () = msg_send![window_ref, setFrame: frame, display: true];
        if !view.is_null() {
            let view_ref = &*view;
            let _: () =
                msg_send![view_ref, setFrame: NsRect { x: 0.0, y: 0.0, w: frame.w, h: frame.h }];
        }
        let frame_now: NsRect = msg_send![window_ref, frame];
        log::info!(
            "screen indicator after setFrame now=({:.0},{:.0}) [{:.0}x{:.0}]",
            frame_now.x,
            frame_now.y,
            frame_now.w,
            frame_now.h
        );
        assert_flash_front(window_ref);
        Some(window)
    }

    /// 把光圈窗抬到 NSScreenSaverWindowLevel 并强制上屏。建窗收尾与
    /// 存活期重申（[`schedule_close`]）共用同一条路径。
    ///
    /// tauri 的 always-on-top 是 floating 层（3），会被菜单栏（25）和
    /// 程序坞（20）遮住光圈的上/下边缘；抬到 NSScreenSaverWindowLevel
    /// （1000）。后台 app 建窗可能被 macOS 抑制显示，`orderFrontRegardless`
    /// 绕过激活检查强制上屏；扩展屏跑应用全屏（全屏 Space）时也靠它把
    /// 光圈从前排夺回。
    ///
    /// setLevel: 的参数 ABI 在 macOS 27 上实测也变了：按传统 CGFloat(f64)
    /// 传参时，实现从整数寄存器读值（读回 level 得到垃圾/1），层级实际
    /// 只有 ≈0~1，压不过副屏的全屏空间。改为先按 i64 设并回读，读不到
    /// 1000 再按 f64 兜底，两种 ABI 下都保证最终是 1000。不要换回
    /// set_always_on_top——那会把层级压回 floating 层。
    fn assert_flash_front(window_ref: &AnyObject) {
        unsafe {
            let _: () = msg_send![window_ref, setLevel: 1000_i64];
            let probe: i64 = msg_send![window_ref, level];
            if probe != 1000 {
                let _: () = msg_send![window_ref, setLevel: 1000.0_f64];
            }
            let _: () = msg_send![window_ref, orderFrontRegardless];
        }
    }

    /// 双层虚化光晕：外层弥散光晕 + 内层亮芯，取代旧硬描边。壳层剖面
    /// 见 [`glow`]——由外到内、先光晕后亮芯（与 CSS `.glow` → `.ring`
    /// 层叠顺序一致），逐壳层 source-over 叠加等价于
    /// [`glow::composite_pixel`] 的合成模型。只描边不填充，内部透明。
    unsafe fn attach_glow(host: &AnyObject, frame: NsRect, window: &AnyObject) {
        let path = rounded_rect_path(frame);
        if path.is_null() {
            return;
        }
        let scale: f64 = msg_send![window, backingScaleFactor];
        for (width, alpha, rgb) in glow::GLOW_SHELLS
            .iter()
            .map(|(width, alpha)| (*width, *alpha, glow::GLOW_RGB))
            .chain(
                glow::CORE_SHELLS
                    .iter()
                    .map(|(width, alpha)| (*width, *alpha, glow::CORE_RGB)),
            )
        {
            add_shell_layer(host, path, frame, scale, width, alpha, rgb);
        }
        // 各层 setPath: 都已 retain，交掉我们的 +1。
        CGPathRelease(path);
    }

    /// 一圈固定宽度/透明度的描边壳层。path 与 strokeColor 都是 strong
    /// 属性，+1 引用移交后释放我们的那份。
    unsafe fn add_shell_layer(
        host: &AnyObject,
        path: *mut c_void,
        frame: NsRect,
        scale: f64,
        width: f64,
        alpha: f64,
        rgb: (f64, f64, f64),
    ) {
        let layer_alloc: *mut AnyObject = msg_send![class!(CAShapeLayer), alloc];
        let layer: *mut AnyObject = msg_send![layer_alloc, init];
        if layer.is_null() {
            return;
        }
        let layer_ref = &*layer;
        let _: () = msg_send![layer_ref, setPath: path as *mut AnyObject];
        let (r, g, b) = rgb;
        let stroke =
            CGColorCreateGenericRGB(r / 255.0, g / 255.0, b / 255.0, alpha) as *mut AnyObject;
        let _: () = msg_send![layer_ref, setStrokeColor: stroke];
        let _: () = msg_send![layer_ref, setFillColor: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![layer_ref, setContentsScale: scale];
        let _: () = msg_send![layer_ref, setLineWidth: width];
        let _: () =
            msg_send![layer_ref, setFrame: NsRect { x: 0.0, y: 0.0, w: frame.w, h: frame.h }];
        start_pulse(layer_ref);
        let _: () = msg_send![host, addSublayer: layer_ref];
        // addSublayer 已 retain，交掉我们的 +1。
        let _: () = msg_send![layer_ref, release];
    }

    /// 脊线圆角矩形：内缩 [`glow::SPINE_INSET`]、圆角 [`glow::SPINE_RADIUS`]，
    /// 各壳层以它为中心线按各自宽度对称铺开。
    unsafe fn rounded_rect_path(frame: NsRect) -> *mut c_void {
        let path = CGPathCreateMutable();
        if path.is_null() {
            return std::ptr::null_mut();
        }
        let inner = CGRect {
            origin: CGPoint {
                x: glow::SPINE_INSET,
                y: glow::SPINE_INSET,
            },
            size: CGSize {
                width: (frame.w - glow::SPINE_INSET * 2.0).max(0.0),
                height: (frame.h - glow::SPINE_INSET * 2.0).max(0.0),
            },
        };
        CGPathAddRoundedRect(
            path,
            std::ptr::null(),
            inner,
            glow::SPINE_RADIUS,
            glow::SPINE_RADIUS,
        );
        path
    }

    /// 与 html 版同拍数的 Core Animation 脉冲：透明度 0.35↔1，0.45s 一拍，
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
    /// 存活期内每 [`REASSERT_INTERVAL`] 重申一次置顶/上屏——扩展屏跑
    /// 应用全屏（全屏 Space）时，全屏应用会把光圈挤出前排，靠 4 次重申
    /// 压回来（4×300ms 重申 + 尾部收尾 = 1400ms 停留）。
    fn schedule_close(window: *mut AnyObject) {
        let ptr = window as usize;
        std::thread::spawn(move || {
            for _ in 0..REASSERT_TIMES {
                std::thread::sleep(REASSERT_INTERVAL);
                on_main(reassert_flash_front, ptr as *mut c_void);
            }
            std::thread::sleep(FLASH_DURATION.saturating_sub(REASSERT_INTERVAL * REASSERT_TIMES));
            on_main(close_flash, Box::into_raw(Box::new(ptr)) as *mut c_void);
        });
    }

    /// 重申回调：借用窗口指针（不转移所有权，收尾仍归 [`close_flash`]）。
    extern "C" fn reassert_flash_front(ctx: *mut c_void) {
        let window = unsafe { &*(ctx as *const AnyObject) };
        assert_flash_front(window);
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
        PostQuitMessage, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, UpdateLayeredWindow,
        HWND_TOPMOST, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE, ULW_ALPHA,
        WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT, WS_POPUP,
    };

    use super::{glow, pulse_alpha};

    // 重绘节拍：100ms 一帧足够平滑，远轻于 halo 边框的 16ms。每个 tick
    // 顺带重申 HWND_TOPMOST——扩展屏跑应用全屏时，全屏应用会把光圈压下
    // 层级，逐 tick 重申（SWP_NOMOVE|NOSIZE|NOACTIVATE，几乎零开销）保它
    // 始终压在最上层。
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
                // 置顶重申：全屏应用可能在存活期内把光圈压下层级。
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
                let pixels = unsafe {
                    std::slice::from_raw_parts_mut(renderer.bits, (width * height) as usize)
                };
                paint_glow(pixels, width, height, scale, pulse_alpha(elapsed_ms));
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
            .map(|(_, rc, scale)| {
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

    /// 把双层光晕描边画进 `pixels`（width × height，premultiplied BGRA，
    /// top-down）。几何与合成见 [`glow`]：内缩 [`glow::SPINE_INSET`] 的
    /// 圆角矩形是两族壳层的共同脊线，逐像素按到脊线的距离叠壳层覆盖率，
    /// 亮芯 over 光晕合成，`pulse` 整体缩放透明度。覆盖足迹不随脉冲变，
    /// 足迹外的像素始终不动（DIB 初始为零）；足迹内逐帧覆写，零脉冲会
    /// 把上一帧清干净。
    fn paint_glow(pixels: &mut [u32], width: i32, height: i32, scale: f64, pulse: f64) {
        let inset = glow::SPINE_INSET * scale;
        let radius = glow::SPINE_RADIUS * scale;
        let spine_w = f64::from(width) - 2.0 * inset;
        let spine_h = f64::from(height) - 2.0 * inset;
        if spine_w <= 0.0 || spine_h <= 0.0 {
            return;
        }
        // 壳层宽度换算到物理像素，透明度不变。
        let glow_shells = glow::GLOW_SHELLS.map(|(width, alpha)| (width * scale, alpha));
        let core_shells = glow::CORE_SHELLS.map(|(width, alpha)| (width * scale, alpha));
        for y in 0..height {
            for x in 0..width {
                let sd = glow::sd_rounded_rect(
                    f64::from(x) + 0.5,
                    f64::from(y) + 0.5,
                    inset,
                    inset,
                    spine_w,
                    spine_h,
                    radius,
                )
                .abs();
                let ag = glow::coverage(sd, &glow_shells);
                let ac = glow::coverage(sd, &core_shells);
                if ag <= 0.0 && ac <= 0.0 {
                    continue;
                }
                pixels[(y * width + x) as usize] = glow::composite_pixel(ac, ag, pulse);
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
        fn paint_glow_keeps_the_far_interior_transparent() {
            let mut pixels = vec![0u32; 200 * 200];
            paint_glow(&mut pixels, 200, 200, 1.0, 1.0);
            // 画布中心离脊线（内缩 12）足够远，必须保持透明。
            assert_eq!(pixels[100 * 200 + 100], 0, "光圈内部必须全透明");
            // 脊线（右缘内 12px 处）上必须有可见像素。
            let spine = pixels[100 * 200 + (200 - 12)];
            assert!(spine >> 24 > 0, "脊线上必须有可见像素");
        }

        #[test]
        fn paint_glow_zero_pulse_clears_the_previous_frame() {
            let mut pixels = vec![0u32; 60 * 60];
            paint_glow(&mut pixels, 60, 60, 1.0, 1.0);
            assert!(pixels.iter().any(|p| *p >> 24 > 0), "峰值脉冲必须画出像素");
            paint_glow(&mut pixels, 60, 60, 1.0, 0.0);
            assert!(pixels.iter().all(|p| *p == 0), "零脉冲必须清掉上一帧");
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
        assert!((pulse_alpha(0.0) - 0.35).abs() < 1e-9);
        assert!((pulse_alpha(225.0) - 0.675).abs() < 1e-9);
        assert!((pulse_alpha(450.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pulse_alternates_within_bounds_and_settles_at_full() {
        // 第二拍反向：中点从 1.0 回落。
        assert!((pulse_alpha(675.0) - 0.675).abs() < 1e-9);
        for ms in [0.0, 100.0, 450.0, 900.0, 1349.0] {
            let alpha = pulse_alpha(ms);
            assert!((0.35..=1.0).contains(&alpha), "alpha {alpha} at {ms}ms");
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
