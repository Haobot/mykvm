#![cfg(target_os = "windows")]

use std::{
    ptr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::shared_input::{mouse_button_mask, InputCommand, MouseButton};

// Input injected by our own receiver/shortcut dispatcher remains local, even
// if the controller resumes sharing before a queued injection is delivered.
pub const KEYBOARD_INPUT_MARKER: usize = 0x4D59_4B53;

/// Explains a refused button/key injection, throttled to one line per 10s.
///
/// Windows blocks `SendInput` from a standard-user process into an elevated or
/// uiAccess foreground window (Task Manager — which ships `uiAccess="true"` — a
/// UAC-elevated app, etc.) with `ERROR_ACCESS_DENIED`; the events are dropped
/// silently, which reads to the user as "remote control just stopped" even
/// though MyKVM is still running. Cursor MOVE keeps working because it goes
/// through SetCursorPos, not SendInput — so only clicks and keys die, exactly
/// the reported symptom. Surface it instead of failing mutely (this replaces a
/// leftover per-event debug write to C:\ProgramData\MyKVM\*.txt).
fn note_injection_refused(kind: &str, error: u32) {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    static LAST_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();
    let cell = LAST_WARN.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = Instant::now();
    }

    if error == ERROR_ACCESS_DENIED {
        log::warn!(
            "injected {kind} refused by Windows (ERROR_ACCESS_DENIED): the foreground window \
             runs at another integrity level (Task Manager, a UAC prompt, an elevated app). \
             The app retries such input through the MyKVM input service; if that service \
             is not installed, install it in Settings or run MyKVM as administrator."
        );
    } else {
        log::warn!("injected {kind} was refused: SendInput failed with error {error}");
    }
}

pub fn inject_command(command: &InputCommand, pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    if matches!(command, InputCommand::ReleaseAll) {
        release_pressed_inputs(pressed_keys, button_mask);
        return;
    }

    track_pressed_inputs(command, pressed_keys, button_mask);
    let _ = inject_command_without_tracking(command);
}

pub fn release_pressed_inputs_on_fresh_input_desktop(
    pressed_keys: &mut Vec<u16>,
    button_mask: &mut u64,
) -> Result<String, String> {
    let mut desktop = DesktopAttachment::new();
    let name = desktop.attach_current_input_desktop()?;
    release_pressed_inputs(pressed_keys, button_mask);
    Ok(name)
}

pub fn track_pressed_inputs(
    command: &InputCommand,
    pressed_keys: &mut Vec<u16>,
    button_mask: &mut u64,
) {
    match *command {
        InputCommand::MouseButton { button, down, .. } => {
            if down {
                *button_mask |= mouse_button_mask(button);
            } else {
                *button_mask &= !mouse_button_mask(button);
            }
        }
        InputCommand::Key { key_code, down } => {
            if down {
                if !pressed_keys.contains(&key_code) {
                    pressed_keys.push(key_code);
                }
            } else {
                pressed_keys.retain(|pressed| *pressed != key_code);
            }
        }
        _ => {}
    }
}

/// Injects `command`; `Err` carries the Win32 error of a refused SendInput.
pub fn inject_command_without_tracking(command: &InputCommand) -> Result<(), u32> {
    match *command {
        InputCommand::MouseMove { x, y, .. } => {
            inject_mouse_move(x, y, None);
            Ok(())
        }
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y } => inject_scroll(delta_x, delta_y),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        InputCommand::ReleaseAll => Ok(()),
        InputCommand::SecureAttention => {
            let _ = send_secure_attention();
            Ok(())
        }
        // dispatch_input_command 已把 FlashScreen 分流给光圈执行器，注入
        // 路径不会收到；穷尽匹配仍需覆盖该变体。
        InputCommand::FlashScreen { .. } => Ok(()),
    }
}

pub fn release_pressed_inputs(pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    let keys = std::mem::take(pressed_keys);
    for key_code in keys.into_iter().rev() {
        let _ = inject_key(key_code, false);
    }

    for button in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        let mask = mouse_button_mask(button);
        if *button_mask & mask != 0 {
            let _ = inject_mouse_button(button, false, 0, 0);
        }
    }
    *button_mask = 0;
}

pub struct DesktopAttachment {
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    name: String,
}

impl Default for DesktopAttachment {
    fn default() -> Self {
        Self {
            desktop: ptr::null_mut(),
            name: String::new(),
        }
    }
}

impl DesktopAttachment {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach_current_input_desktop(&mut self) -> Result<String, String> {
        use windows_sys::Win32::System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_CREATEWINDOW,
            DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
            DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS,
        };

        unsafe {
            // DESKTOP_JOURNALPLAYBACK is REQUIRED for SendInput to be accepted on
            // the attached desktop: without it the worker's synthetic clicks/keys
            // are refused with ERROR_ACCESS_DENIED (only mouse-move, which uses a
            // different path, slips through). This is why the SYSTEM worker could
            // move the cursor on the lock screen but not click or type.
            let desktop = OpenInputDesktop(
                0,
                0,
                DESKTOP_READOBJECTS
                    | DESKTOP_WRITEOBJECTS
                    | DESKTOP_SWITCHDESKTOP
                    | DESKTOP_CREATEWINDOW
                    | DESKTOP_JOURNALPLAYBACK
                    | DESKTOP_JOURNALRECORD,
            );
            if desktop.is_null() {
                return Err("OpenInputDesktop failed".into());
            }

            let name = desktop_name(desktop).unwrap_or_else(|| "<unknown>".into());

            // Always re-attach to the freshly opened input desktop. Caching by
            // name is unsafe: a secure-desktop transition (e.g. clicking
            // "I forgot my PIN" / "Reset password" on the lock screen) switches
            // to a DIFFERENT desktop object that often carries the SAME name
            // ("Winlogon"). A name-equality cache would then skip SetThreadDesktop
            // and leave the worker bound to the old, now-inactive desktop, so
            // clicks/keys silently stop until the worker restarts. OpenInputDesktop
            // is already called every time here, so re-attaching is essentially
            // free.
            if SetThreadDesktop(desktop) == 0 {
                let _ = CloseDesktop(desktop);
                return Err(format!("SetThreadDesktop failed for {name}"));
            }

            if !self.desktop.is_null() {
                let _ = CloseDesktop(self.desktop);
            }
            self.desktop = desktop;
            self.name = name.clone();
            Ok(name)
        }
    }
}

unsafe fn desktop_name(
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
) -> Option<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    let mut needed = 0_u32;
    let mut buffer = [0_u16; 256];
    let ok = GetUserObjectInformationW(
        desktop as _,
        UOI_NAME,
        buffer.as_mut_ptr() as *mut _,
        (buffer.len() * std::mem::size_of::<u16>()) as u32,
        &mut needed,
    ) != 0;
    if !ok || needed == 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

impl Drop for DesktopAttachment {
    fn drop(&mut self) {
        if !self.desktop.is_null() {
            unsafe {
                let _ = windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.desktop);
            }
        }
    }
}

pub fn inject_mouse_move(x: i32, y: i32, _drag_button: Option<MouseButton>) {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SetCursorPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    };

    unsafe {
        let virtual_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let virtual_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let virtual_width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let virtual_height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        let normalized_x =
            ((x - virtual_x) as i64 * 65_535 / (virtual_width - 1).max(1) as i64) as i32;
        let normalized_y =
            ((y - virtual_y) as i64 * 65_535 / (virtual_height - 1).max(1) as i64) as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: normalized_x.clamp(0, 65_535),
                    dy: normalized_y.clamp(0, 65_535),
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        if SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 0 {
            let _ = SetCursorPos(x, y);
        }
    }
}

pub fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) -> Result<(), u32> {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
        },
        WindowsAndMessaging::{XBUTTON1, XBUTTON2},
    };

    if x != 0 || y != 0 {
        inject_mouse_move(x, y, None);
    }

    // Use SendInput instead of the deprecated mouse_event wrapper: mouse_event
    // was silently dropping button events when called from the helper service's
    // spawned injection thread on some desktops, which produced the "cursor
    // moves but cannot click" symptom. SendInput reports failures via its
    // return value and is the recommended injection API.
    //
    // The side buttons ride MOUSEEVENTF_X* with the button in mouseData
    // (XBUTTON1 = back, XBUTTON2 = forward).
    let (flag, mouse_data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
    };

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data as u32,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    sent_or_refused(&input, "mouse button")
}

/// SendInput of one event; a refusal is noted and its error returned.
fn sent_or_refused(
    input: &windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT,
    kind: &str,
) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};

    if unsafe { SendInput(1, input, std::mem::size_of::<INPUT>() as i32) } != 0 {
        return Ok(());
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    note_injection_refused(kind, error);
    Err(error)
}

pub fn inject_scroll(delta_x: i32, delta_y: i32) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };

    for (flag, delta) in [(MOUSEEVENTF_WHEEL, delta_y), (MOUSEEVENTF_HWHEEL, delta_x)] {
        if delta == 0 {
            continue;
        }

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (delta * 120) as u32,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        sent_or_refused(&input, "scroll")?;
    }
    Ok(())
}

pub fn inject_key(key_code: u16, down: bool) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC,
    };

    let mut dw_flags = if down { 0 } else { KEYEVENTF_KEYUP };
    if is_extended_key_vk(key_code) {
        dw_flags |= KEYEVENTF_EXTENDEDKEY;
    }

    let scan = unsafe { MapVirtualKeyW(key_code as u32, MAPVK_VK_TO_VSC) } as u16;

    // Use SendInput instead of keybd_event: same reason as inject_mouse_button
    // — keybd_event was silently dropping key events from the helper's spawned
    // thread, leaving the keyboard dead while mouse moves still worked.
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key_code,
                wScan: scan,
                dwFlags: dw_flags,
                time: 0,
                dwExtraInfo: KEYBOARD_INPUT_MARKER,
            },
        },
    };
    sent_or_refused(&input, "key")
}

fn is_extended_key_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21 | 0x22
            | 0x23
            | 0x24
            | 0x25
            | 0x26
            | 0x27
            | 0x28
            | 0x2C
            | 0x2D
            | 0x2E
            | 0x5B
            | 0x5C
            | 0x5D
            | 0x6F
            | 0x90
            | 0xA3
            | 0xA5
            // browser, volume and media keys (E0-prefixed on a keyboard)
            | 0xA6..=0xB7
    )
}

pub fn send_secure_attention() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    };

    type SendSasFn = unsafe extern "system" fn(windows_sys::core::BOOL);

    unsafe {
        let dll = LoadLibraryW(crate::wide_null("sas.dll").as_ptr());
        if dll.is_null() {
            return Err("SAS.dll is not available on this Windows installation".into());
        }
        let Some(proc) = GetProcAddress(dll, c"SendSAS".as_ptr() as *const u8) else {
            let _ = FreeLibrary(dll);
            return Err("SendSAS entry point is not available".into());
        };
        let send_sas: SendSasFn = std::mem::transmute(proc);
        send_sas(0);
        let _ = FreeLibrary(dll);
    }
    Ok(())
}

// ---- 无人值守顶班时的屏幕光圈 ----
//
// GUI 在时 FlashScreen 在 dispatch_input_command 就被分流到 Tauri 的
// screen-indicator 窗口；GUI 退出（"保持被控"）后 FlashScreen 经输入管道
// 到达会话 worker，这里用纯 Win32 分层窗口把同一圈蓝色光圈画到用户桌面
// 上。视觉参数对齐 public/screen-indicator.html：RGB(56,150,255)、10px
// 边框、8px 内缩、0.45s 脉冲、共 1.4s。

const FLASH_RING_R: u32 = 56;
const FLASH_RING_G: u32 = 150;
const FLASH_RING_B: u32 = 255;
const FLASH_DURATION_MS: u64 = 1_400;
const FLASH_PULSE_PERIOD_MS: u64 = 450;
const FLASH_TIMER_ID: usize = 1;
const FLASH_TIMER_MS: u32 = 45;

struct PhysicalFlashRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: f64,
}

/// 把逻辑坐标的光圈矩形（detect_local_screens 空间）换算成目标显示器上
/// 的物理像素矩形：逐显示器用有效 DPI 还原 winit 的 logical = physical *
/// 96/dpi 映射，找中心点所在的显示器。
fn physical_flash_rect(x: f64, y: f64, width: f64, height: f64) -> Option<PhysicalFlashRect> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe extern "system" fn collect_monitor(
        monitor: HMONITOR,
        _hdc: windows_sys::Win32::Graphics::Gdi::HDC,
        _rect: *mut RECT,
        data: isize,
    ) -> windows_sys::core::BOOL {
        let monitors = &mut *(data as *mut Vec<(HMONITOR, MONITORINFO)>);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            monitors.push((monitor, info));
        }
        1
    }

    let mut monitors: Vec<(HMONITOR, MONITORINFO)> = Vec::new();
    unsafe {
        if EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_monitor),
            &mut monitors as *mut Vec<(HMONITOR, MONITORINFO)> as isize,
        ) == 0
        {
            return None;
        }
    }

    let center_x = x + width / 2.0;
    let center_y = y + height / 2.0;
    let mut chosen: Option<f64> = None;
    for (monitor, info) in &monitors {
        let rc = info.rcMonitor;
        if rc.right <= rc.left || rc.bottom <= rc.top {
            continue;
        }
        let mut dpi_x = 0_u32;
        let mut dpi_y = 0_u32;
        let dpi_ok =
            unsafe { GetDpiForMonitor(*monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0 };
        let scale = if dpi_ok && dpi_x > 0 {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        // winit 逻辑空间：logical = physical * 96 / dpi。
        let (left, top) = (f64::from(rc.left) / scale, f64::from(rc.top) / scale);
        let (right, bottom) = (f64::from(rc.right) / scale, f64::from(rc.bottom) / scale);
        if center_x >= left && center_x < right && center_y >= top && center_y < bottom {
            chosen = Some(scale);
            break;
        }
        // 兜底：中心点不落在任何显示器的逻辑矩形里时用第一个有效显示器。
        chosen = chosen.or(Some(scale));
    }

    let scale = chosen?;
    Some(PhysicalFlashRect {
        x: (x * scale).round() as i32,
        y: (y * scale).round() as i32,
        width: ((width * scale).round() as i32).max(1),
        height: ((height * scale).round() as i32).max(1),
        scale,
    })
}

/// 在会话 worker（SYSTEM）里画光圈：专用线程绑定输入桌面后开四条
/// TOPMOST|TRANSPARENT 分层窗口条拼成边框，按 GUI 同款节奏做透明度脉冲。
pub fn flash_screen_overlay(x: f64, y: f64, width: f64, height: f64) {
    let Some(rect) = physical_flash_rect(x, y, width, height) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("mykvm-flash-overlay".into())
        .spawn(move || unsafe { run_flash_overlay(rect) });
}

unsafe fn run_flash_overlay(rect: PhysicalFlashRect) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_CREATEWINDOW,
        DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostQuitMessage, RegisterClassExW, SetLayeredWindowAttributes, SetTimer, ShowWindow,
        TranslateMessage, LWA_ALPHA, MSG, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT, WS_POPUP,
    };

    // 绑定到当前输入桌面（正常桌面；锁屏/UAC 期间 OpenInputDesktop 指向
    // 安全桌面，光圈画了也看不见，直接放弃——与 GUI 行为一致）。
    let desktop = OpenInputDesktop(
        0,
        0,
        DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_CREATEWINDOW,
    );
    if desktop.is_null() || SetThreadDesktop(desktop) == 0 {
        if !desktop.is_null() {
            let _ = CloseDesktop(desktop);
        }
        return;
    }

    static CLASS_ATOM: OnceLock<u16> = OnceLock::new();
    let atom = *CLASS_ATOM.get_or_init(|| {
        let class_name = crate::wide_null("MyKVMFlashBar");
        let brush = CreateSolidBrush(FLASH_RING_R | (FLASH_RING_G << 8) | (FLASH_RING_B << 16));
        let module = GetModuleHandleW(std::ptr::null());
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = Some(DefWindowProcW);
        class.hInstance = module;
        class.hbrBackground = brush; // 与类同生命周期，进程存活期间持有
        class.lpszClassName = class_name.as_ptr();
        RegisterClassExW(&class)
    });
    if atom == 0 {
        let _ = CloseDesktop(desktop);
        return;
    }

    let margin = (((8.0_f64) * rect.scale).round() as i32).max(4);
    let thickness = (((10.0_f64) * rect.scale).round() as i32).clamp(6, 40);
    let inner_x = rect.x + margin;
    let inner_y = rect.y + margin;
    let inner_w = (rect.width - 2 * margin).max(2 * thickness);
    let inner_h = (rect.height - 2 * margin).max(2 * thickness);
    let bars = [
        (inner_x, inner_y, inner_w, thickness),
        (inner_x, inner_y + inner_h - thickness, inner_w, thickness),
        (
            inner_x,
            inner_y + thickness,
            thickness,
            inner_h - 2 * thickness,
        ),
        (
            inner_x + inner_w - thickness,
            inner_y + thickness,
            thickness,
            inner_h - 2 * thickness,
        ),
    ];

    let class_name = crate::wide_null("MyKVMFlashBar");
    let module = GetModuleHandleW(std::ptr::null());
    let ex_style: WINDOW_EX_STYLE =
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let style: WINDOW_STYLE = WS_POPUP;

    let mut windows: Vec<HWND> = Vec::with_capacity(bars.len());
    for (bar_x, bar_y, bar_w, bar_h) in bars {
        let hwnd = CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            std::ptr::null(),
            style,
            bar_x,
            bar_y,
            bar_w.max(1),
            bar_h.max(1),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            continue;
        }
        // 0.92 透明度基准（与 CSS 的 alpha 0.92 对齐），脉冲由定时器驱动。
        SetLayeredWindowAttributes(hwnd, 0, 235, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        windows.push(hwnd);
    }

    if windows.is_empty() {
        let _ = CloseDesktop(desktop);
        return;
    }
    // 定时器挂在第一条窗口上：消息循环统一驱动脉冲与收尾。
    SetTimer(windows[0], FLASH_TIMER_ID, FLASH_TIMER_MS, None);

    let started = Instant::now();
    let mut message: MSG = std::mem::zeroed();
    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
        if message.message == WM_TIMER && message.hwnd == windows[0] {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            if elapsed_ms >= FLASH_DURATION_MS {
                for hwnd in &windows {
                    let _ = DestroyWindow(*hwnd);
                }
                let _ = CloseDesktop(desktop);
                PostQuitMessage(0);
            } else {
                let phase = f64::from((elapsed_ms % FLASH_PULSE_PERIOD_MS) as u32)
                    / FLASH_PULSE_PERIOD_MS as f64;
                let triangle = if phase < 0.5 {
                    phase * 2.0
                } else {
                    2.0 - phase * 2.0
                };
                let alpha = ((0.4 + 0.6 * triangle) * 235.0).round() as u8;
                for hwnd in &windows {
                    SetLayeredWindowAttributes(*hwnd, 0, alpha, LWA_ALPHA);
                }
            }
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}
