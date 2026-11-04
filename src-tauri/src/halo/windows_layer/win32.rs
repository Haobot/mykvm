//! FocusHalo window layer for Windows: `EnumWindows` enumeration (z-order top
//! to bottom by callback order), visibility via `IsWindowVisible`/`IsIconic`,
//! cloaking via `DwmGetWindowAttribute(DWMWA_CLOAKED)`, tool-window and shell
//! class filtering, and per-screen DPI conversion into the layout board's
//! logical coordinates (the pure math lives in the parent module and is
//! unit-tested there on any host).
//!
//! The process-name and shell-class handling follows the
//! `input_guard_windows.rs` precedent. Nothing here activates or moves a
//! window: enumeration, hit-testing and caching only.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

use super::{
    native_rect_to_logical, CachedService, ForegroundMatch, RawEnumerator, RawWindow, WindowCache,
};
use crate::halo::overlay::OverlayScreenGeometry;

/// Desktop-shell window classes: never hittable targets.
const SHELL_CLASSES: [&str; 4] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

pub(crate) struct Win32WindowService;

impl Win32WindowService {
    /// Build the cached Windows window service.
    pub(crate) fn new(screens: &[OverlayScreenGeometry]) -> CachedService<Self> {
        CachedService::new(Win32WindowService, screens, WindowCache::real())
    }
}

/// EnumWindows callback: every top-level window handle in z-order (top first).
unsafe extern "system" fn collect_hwnds(hwnd: HWND, lparam: LPARAM) -> i32 {
    let out = &mut *(lparam as *mut Vec<isize>);
    out.push(hwnd as isize);
    1 // TRUE: keep enumerating
}

/// pid to executable file name, cached per thread (pid 0 maps to "").
fn process_name(pid: u32) -> String {
    thread_local! {
        static PROCESS_NAME: RefCell<(u32, String)> = const { RefCell::new((0, String::new())) };
    }
    if pid == 0 {
        return String::new();
    }
    PROCESS_NAME.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != pid {
            cache.0 = pid;
            cache.1.clear();
            unsafe {
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !process.is_null() {
                    let mut path = [0_u16; 4096];
                    let mut len = path.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len) != 0 {
                        let path = String::from_utf16_lossy(&path[..len as usize]);
                        if let Some(name) = path.rsplit(['/', '\\']).next() {
                            cache.1.push_str(name);
                        }
                    }
                    CloseHandle(process);
                }
            }
        }
        cache.1.clone()
    })
}

fn window_text(hwnd: HWND) -> String {
    let mut buffer = [0_u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0_u16; 128];
    let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// `WS_EX_TOOLWINDOW` (palettes, floating toolbars): never hittable targets.
fn is_tool_window(hwnd: HWND) -> bool {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 }
}

/// UWP/shell cloaking (`DWMWA_CLOAKED != 0`): suspended or virtual-desktop
/// hidden windows. An unsupported attribute (query error) counts as visible.
/// windows-sys 0.61 takes the attribute id as u32 while the constant is i32.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        ) == 0
            && cloaked != 0
    }
}

impl RawEnumerator for Win32WindowService {
    fn enumerate_raw(
        &mut self,
        screens: &[OverlayScreenGeometry],
    ) -> Result<(Vec<RawWindow>, ForegroundMatch), String> {
        let mut hwnds: Vec<isize> = Vec::new();
        unsafe {
            if EnumWindows(Some(collect_hwnds), &mut hwnds as *mut _ as LPARAM) == 0 {
                return Err("EnumWindows failed".into());
            }
            let foreground = GetForegroundWindow();
            let foreground_id = if foreground.is_null() {
                0
            } else {
                foreground as usize as u64
            };

            let mut raw = Vec::with_capacity(hwnds.len());
            for hwnd in hwnds {
                let hwnd = hwnd as HWND;
                if hwnd.is_null() {
                    continue;
                }
                let mut pid = 0_u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                let visible = IsWindowVisible(hwnd) != 0
                    && IsIconic(hwnd) == 0
                    && !is_tool_window(hwnd)
                    && !is_cloaked(hwnd);
                let shell = SHELL_CLASSES.contains(&window_class(hwnd).as_str());
                let mut rect = RECT::default();
                let frame = if GetWindowRect(hwnd, &mut rect) != 0 {
                    native_rect_to_logical(
                        (
                            rect.left as f64,
                            rect.top as f64,
                            rect.right as f64,
                            rect.bottom as f64,
                        ),
                        screens,
                    )
                } else {
                    // No rect: a degenerate frame the pure filter drops via
                    // the empty-intersection rule.
                    (0.0, 0.0, 0.0, 0.0)
                };
                raw.push(RawWindow {
                    id: hwnd as usize as u64,
                    pid,
                    frame,
                    title: window_text(hwnd),
                    process: process_name(pid).to_lowercase(),
                    visible,
                    shell,
                });
            }
            Ok((raw, ForegroundMatch::Window(foreground_id)))
        }
    }
}
