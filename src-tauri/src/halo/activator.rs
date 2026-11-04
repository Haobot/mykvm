//! FocusHalo window activation (P3): brings a dwell-selected window to the
//! foreground on the platform's own terms.
//!
//! Contract: `WindowService` produced the id, [`WindowActivator`] consumes it
//! — same id space (macOS `CGWindowNumber`, Windows HWND as u64). The engine
//! only keeps the halo and the mouse where they are after a switch (no
//! pointer move, no overlay relocation), so activation is the sole effect.
//!
//! - macOS: AX raise (`AXUIElementPerformAction(kAXRaiseAction)`) on the
//!   `AXWindows` element whose frame matches the CG window, then app-level
//!   `NSRunningApplication.activateWithOptions` so the app comes forward even
//!   when the AX path is unavailable (Accessibility permission off, AX
//!   disabled by the target app).
//! - Windows: `SetForegroundWindow`; on the foreground-lock failure the
//!   `AttachThreadInput` combo (attach both threads → `BringWindowToTop` +
//!   `SetForegroundWindow` → detach); still failing → `FlashWindowEx`
//!   attention flash so the user at least sees the request.
//! - Other platforms: placeholder outcome (`activated=false`, "unsupported").
//!
//! Every failure lands in [`ActivationOutcome::detail`] for the engine's info
//! log; the engine never panics on activation problems.

/// Result of one activation attempt, consumed by the engine's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationOutcome {
    /// The platform accepted the activation request (AX raise success, or the
    /// fallback app-level activation succeeded).
    pub activated: bool,
    /// The direct path failed and a fallback carried the attempt (AX
    /// unavailable → app-level only; `SetForegroundWindow` refused →
    /// AttachThreadInput combo or flash).
    pub fell_back: bool,
    /// Human-readable per-step result, one entry per attempt, "; "-joined.
    /// Log material, never parsed.
    pub detail: String,
}

/// Activates a window by the id [`crate::halo::windows_layer::WindowService`]
/// handed out. Implementations keep their own small lookup state (macOS: an
/// id → (pid, frame) table with the window layer's 300 ms cache semantics).
pub trait WindowActivator: Send {
    fn activate(&mut self, window_id: u64) -> ActivationOutcome;
}

/// Platform factory: the real activator on macOS/Windows, the placeholder
/// elsewhere.
pub fn create_activator() -> Box<dyn WindowActivator> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOSActivator::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(win32::Win32Activator)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(UnsupportedActivator)
    }
}

// ---------------------------------------------------------------------------
// macOS: AX raise + NSRunningApplication fallback
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    use core_foundation::base::{CFRelease, TCFType};
    use core_foundation::string::CFString;
    use core_graphics::geometry::CGRect;

    use super::{ActivationOutcome, WindowActivator};
    use crate::halo::windows_layer::macos::{ax_windows_for_pid, cg_window_table, frames_match};
    use crate::halo::windows_layer::CACHE_TTL_MS;

    // Same signature shape as windows_layer/macos.rs's AX externs; declared
    // here so the activator stays self-contained.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        /// Perform the named accessibility action on `element`
        /// (kAXErrorSuccess == 0).
        fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
    }

    /// `NSApplicationActivationOptions.NSApplicationActivateIgnoringOtherApps`.
    const NS_APPLICATION_ACTIVATE_IGNORING_OTHER_APPS: u64 = 1 << 1;
    /// The accessibility action that raises one window (kAXRaiseAction).
    const K_AX_RAISE_ACTION: &str = "AXRaise";

    /// Process-uptime milliseconds (the window layer's cache clock shape:
    /// static epoch keeps values small and monotonic within the process).
    fn now_ms() -> u64 {
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        let epoch = EPOCH.get_or_init(std::time::Instant::now);
        epoch.elapsed().as_millis() as u64
    }

    /// Maps window ids to (owner pid, CG frame) with the same 300 ms cache
    /// semantics as the window layer: one raw `CGWindowListCopyWindowInfo`
    /// scan per TTL window, served from memory in between.
    pub struct MacOSActivator {
        table_at_ms: Option<u64>,
        table: HashMap<u64, (u32, CGRect)>,
    }

    impl MacOSActivator {
        pub fn new() -> Self {
            MacOSActivator {
                table_at_ms: None,
                table: HashMap::new(),
            }
        }

        /// The (pid, frame) entry for `id`, refreshing the table first when
        /// the cache went stale. A failed scan caches an empty table for the
        /// TTL so a broken window server cannot turn activation into a scan
        /// hammer.
        fn window_entry(&mut self, id: u64) -> Option<(u32, CGRect)> {
            let now = now_ms();
            let stale = self
                .table_at_ms
                .is_none_or(|at| now.saturating_sub(at) >= CACHE_TTL_MS);
            if stale {
                self.table = match cg_window_table() {
                    Ok(rows) => rows
                        .into_iter()
                        .map(|(id, pid, frame)| (id, (pid, frame)))
                        .collect(),
                    Err(_) => HashMap::new(),
                };
                self.table_at_ms = Some(now);
            }
            self.table.get(&id).copied()
        }
    }

    impl WindowActivator for MacOSActivator {
        fn activate(&mut self, window_id: u64) -> ActivationOutcome {
            let Some((pid, cg_frame)) = self.window_entry(window_id) else {
                return ActivationOutcome {
                    activated: false,
                    fell_back: false,
                    detail: format!(
                        "window id {window_id} not found in the on-screen window table"
                    ),
                };
            };

            let mut parts: Vec<String> = Vec::new();

            // 1. AX raise: the matched AX window element comes to the front
            //    within its app, which is what a real click on the title bar
            //    does. Each element in the list is +1 retained; release them
            //    all on the way out.
            let mut ax_ok = false;
            let ax_windows = ax_windows_for_pid(pid);
            let matched = ax_windows
                .iter()
                .find(|ax| frames_match(ax.frame, cg_frame))
                .map(|ax| ax.element);
            match matched {
                Some(element) => {
                    let action = CFString::new(K_AX_RAISE_ACTION);
                    let status = unsafe {
                        AXUIElementPerformAction(element, action.as_concrete_TypeRef().cast())
                    };
                    ax_ok = status == 0;
                    parts.push(if ax_ok {
                        "ax raise ok".into()
                    } else {
                        format!("ax raise error {status}")
                    });
                }
                None if ax_windows.is_empty() => {
                    parts.push("no ax windows exposed by the app".into());
                }
                None => {
                    parts.push("no ax window matched the cg frame".into());
                }
            }
            for ax in &ax_windows {
                unsafe { CFRelease(ax.element) };
            }

            // 2. App-level activation afterwards so the app itself comes
            //    forward even when the AX path failed (permission off, AX
            //    disabled, no matching element). Also the sole success path
            //    when the AX step could not run.
            let ns_ok = ns_activate_pid(pid, &mut parts);

            ActivationOutcome {
                activated: ax_ok || ns_ok,
                fell_back: !ax_ok,
                detail: parts.join("; "),
            }
        }
    }

    /// `NSRunningApplication(pid).activateWithOptions(IgnoringOtherApps)`.
    /// Returns whether the request was accepted; failures land in `parts`.
    fn ns_activate_pid(pid: u32, parts: &mut Vec<String>) -> bool {
        #[link(name = "objc")]
        extern "C" {
            fn objc_getClass(name: *const c_char) -> *mut c_void;
            fn sel_registerName(name: *const c_char) -> *mut c_void;
            fn objc_msgSend();
            fn objc_autoreleasePoolPush() -> *mut c_void;
            fn objc_autoreleasePoolPop(pool: *mut c_void);
        }

        unsafe {
            let pool = objc_autoreleasePoolPush();
            // Two distinct msgSend signatures: the class method takes the pid,
            // the instance method takes the activation-options bitmask. One
            // shared transmute would pass options in the wrong register.
            let send_class: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let send_options: unsafe extern "C" fn(*mut c_void, *mut c_void, u64) -> i8 =
                std::mem::transmute(objc_msgSend as *const ());
            let app = send_class(
                objc_getClass(b"NSRunningApplication\0".as_ptr().cast()),
                sel_registerName(
                    b"runningApplicationWithProcessIdentifier:\0"
                        .as_ptr()
                        .cast(),
                ),
                pid as i32,
            );
            let ok = if app.is_null() {
                parts.push(format!("no NSRunningApplication for pid {pid}"));
                false
            } else {
                let accepted = send_options(
                    app,
                    sel_registerName(b"activateWithOptions:\0".as_ptr().cast()),
                    NS_APPLICATION_ACTIVATE_IGNORING_OTHER_APPS,
                ) != 0;
                parts.push(if accepted {
                    "ns activate ok".into()
                } else {
                    "ns activate returned false".into()
                });
                accepted
            };
            objc_autoreleasePoolPop(pool);
            ok
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: SetForegroundWindow, AttachThreadInput combo, FlashWindowEx
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod win32 {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FlashWindowEx, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, FLASHWINFO, FLASHW_ALL, SW_RESTORE,
    };

    use super::{ActivationOutcome, WindowActivator};

    /// Flash count for the final fallback (product: 3 flashes).
    const FLASH_COUNT: u32 = 3;

    pub struct Win32Activator;

    /// A stored window id is an HWND; 0 is never a valid one.
    fn hwnd_of(window_id: u64) -> Option<HWND> {
        if window_id == 0 {
            return None;
        }
        Some((window_id as usize) as HWND)
    }

    impl WindowActivator for Win32Activator {
        fn activate(&mut self, window_id: u64) -> ActivationOutcome {
            let Some(hwnd) = hwnd_of(window_id) else {
                return ActivationOutcome {
                    activated: false,
                    fell_back: false,
                    detail: "window id 0 is not a valid HWND".into(),
                };
            };
            let mut parts: Vec<String> = Vec::new();
            unsafe {
                // 1. Plain attempt: works whenever the calling thread is
                //    allowed to set the foreground (foreground process or a
                //    recent input event).
                if SetForegroundWindow(hwnd) != 0 {
                    parts.push("set-foreground ok".into());
                    return ActivationOutcome {
                        activated: true,
                        fell_back: false,
                        detail: parts.join("; "),
                    };
                }
                parts.push("set-foreground refused (foreground lock)".into());

                // 2. AttachThreadInput combo: attaching the foreground
                //    thread's and the target thread's input queues to ours
                //    merges their input state, which lifts the
                //    caller-must-be-foreground restriction for the retry.
                //    Minimized targets are restored first — an iconic window
                //    cannot take the foreground.
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                    parts.push("restored minimized target".into());
                }
                let foreground = GetForegroundWindow();
                let mut foreground_pid = 0_u32;
                let foreground_thread = if foreground.is_null() {
                    0
                } else {
                    GetWindowThreadProcessId(foreground, &mut foreground_pid)
                };
                let mut target_pid = 0_u32;
                let target_thread = GetWindowThreadProcessId(hwnd, &mut target_pid);
                let current_thread = GetCurrentThreadId();

                let attached_foreground = foreground_thread != 0
                    && foreground_thread != current_thread
                    && AttachThreadInput(foreground_thread, current_thread, 1) != 0;
                let attached_target = target_thread != 0
                    && target_thread != current_thread
                    && AttachThreadInput(target_thread, current_thread, 1) != 0;
                if !attached_foreground {
                    parts.push(format!(
                        "foreground-thread attach skipped (thread {foreground_thread})"
                    ));
                }
                if !attached_target {
                    parts.push(format!(
                        "target-thread attach skipped (thread {target_thread})"
                    ));
                }

                BringWindowToTop(hwnd);
                let combo_ok = SetForegroundWindow(hwnd) != 0;

                // Detach unconditionally: leaked input-queue attachments
                // would corrupt unrelated windows' input handling.
                if attached_foreground {
                    AttachThreadInput(foreground_thread, current_thread, 0);
                }
                if attached_target {
                    AttachThreadInput(target_thread, current_thread, 0);
                }

                if combo_ok {
                    parts.push("combo set-foreground ok".into());
                    return ActivationOutcome {
                        activated: true,
                        fell_back: true,
                        detail: parts.join("; "),
                    };
                }
                parts.push("combo set-foreground refused".into());

                // 3. Last resort: flash the window so the user sees the
                //    request even though the system refused to switch.
                let flash = FLASHWINFO {
                    cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                    hwnd,
                    dwFlags: FLASHW_ALL,
                    uCount: FLASH_COUNT,
                    dwTimeout: 0,
                };
                let flashed = FlashWindowEx(&flash) != 0;
                if flashed {
                    parts.push(format!("flashed instead ({FLASH_COUNT}x)"));
                } else {
                    parts.push("flash failed too".into());
                }
                ActivationOutcome {
                    activated: false,
                    fell_back: true,
                    detail: parts.join("; "),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Placeholder for platforms without a window layer yet
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct UnsupportedActivator;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl WindowActivator for UnsupportedActivator {
    fn activate(&mut self, _window_id: u64) -> ActivationOutcome {
        ActivationOutcome {
            activated: false,
            fell_back: false,
            detail: "unsupported".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Linux placeholder behavior (host-independent assertion) -----

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unsupported_platform_reports_placeholder_outcome() {
        let mut activator = create_activator();
        let outcome = activator.activate(42);
        assert!(!outcome.activated);
        assert!(!outcome.fell_back);
        assert_eq!(outcome.detail, "unsupported");
    }

    #[test]
    fn activation_outcome_is_constructible_and_comparable() {
        let a = ActivationOutcome {
            activated: true,
            fell_back: false,
            detail: "ok".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert!(a.activated);
    }
}
