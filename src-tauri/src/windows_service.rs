//! Windows 锁屏输入服务（mykvm-input-helper）的安装/卸载/防火墙/SID/SCM 管理。
//!
//! 自 lib.rs 机械搬移（分级优化方案 2.1 第一步，纯搬移零行为变更）：
//! - 所有条目保持原有 `#[cfg(target_os = "windows")]` 属性；
//! - 可见性统一提升为 pub(crate)，由 lib.rs 的
//!   `pub(crate) use windows_service::*;` 再导出，调用点符号不变。
// Platform imports: this module also compiles on macOS/Linux, where only the
// type and the stub `current_input_service_status` are used. Keeping the
// imports unconditional broke non-Windows builds after the module split.
#[cfg(target_os = "windows")]
use std::env;
#[cfg(target_os = "windows")]
use std::fs;
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "windows")]
use crate::input;
#[cfg(target_os = "windows")]
use crate::shared_input;
#[cfg(target_os = "windows")]
use crate::wide_null;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputServiceStatus {
    pub(crate) installed: bool,
    pub(crate) running: bool,
    pub(crate) worker_session_id: Option<u32>,
    pub(crate) pipe_available: bool,
    pub(crate) sas_available: bool,
    pub(crate) detail: String,
}

#[cfg(target_os = "windows")]
pub(crate) fn current_input_service_status() -> InputServiceStatus {
    match query_windows_input_service_status() {
        Ok(status) => status,
        Err(error) => InputServiceStatus {
            installed: false,
            running: false,
            worker_session_id: None,
            pipe_available: false,
            sas_available: false,
            detail: error,
        },
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn current_input_service_status() -> InputServiceStatus {
    InputServiceStatus {
        installed: false,
        running: false,
        worker_session_id: None,
        pipe_available: false,
        sas_available: false,
        detail: "Windows lock-screen input service is only available on Windows.".into(),
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn query_windows_input_service_status() -> Result<InputServiceStatus, String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE},
        System::{
            RemoteDesktop::WTSGetActiveConsoleSessionId,
            Services::{
                OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
                SERVICE_RUNNING,
            },
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);

        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST || code == ERROR_SERVICE_MARKED_FOR_DELETE {
                return Ok(InputServiceStatus {
                    installed: false,
                    running: false,
                    worker_session_id: None,
                    pipe_available: false,
                    sas_available: false,
                    detail: "Lock-screen input service is not installed.".into(),
                });
            }
            return Err(windows_last_error("OpenServiceW"));
        }
        let _service = ServiceHandleGuard(service);

        let service_status = query_windows_service_status_process(service)?;
        let running = service_status.dwCurrentState == SERVICE_RUNNING;
        let pipe_available = running && input::windows_input_pipe_available();
        let active_session = WTSGetActiveConsoleSessionId();
        let worker_session_id = (running && active_session != u32::MAX).then_some(active_session);
        let sas_available = running && sas_dll_available() && software_sas_allows_services();
        let detail = if running {
            if pipe_available {
                "Lock-screen input service is running and the worker pipe is available."
            } else {
                "Lock-screen input service is running; waiting for the session worker pipe."
            }
        } else {
            "Lock-screen input service is installed but not running."
        };

        Ok(InputServiceStatus {
            installed: true,
            running,
            worker_session_id,
            pipe_available,
            sas_available,
            detail: detail.into(),
        })
    }
}

#[cfg(target_os = "windows")]
unsafe fn query_windows_service_status_process(
    service: windows_sys::Win32::System::Services::SC_HANDLE,
) -> Result<windows_sys::Win32::System::Services::SERVICE_STATUS_PROCESS, String> {
    use windows_sys::Win32::System::Services::{
        QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
    };

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0_u32;
    if QueryServiceStatusEx(
        service,
        SC_STATUS_PROCESS_INFO,
        &mut status as *mut SERVICE_STATUS_PROCESS as *mut u8,
        std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
        &mut needed,
    ) == 0
    {
        return Err(windows_last_error("QueryServiceStatusEx"));
    }
    Ok(status)
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_input_service_process_id() -> Result<Option<u32>, String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE},
        System::Services::{
            OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(process id)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST || code == ERROR_SERVICE_MARKED_FOR_DELETE {
                return Ok(None);
            }
            return Err(windows_last_error("OpenServiceW(process id)"));
        }
        let _service = ServiceHandleGuard(service);
        let status = query_windows_service_status_process(service)?;
        Ok((status.dwProcessId != 0).then_some(status.dwProcessId))
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn acquire_windows_input_service_network_lease() -> Result<Option<std::fs::File>, String>
{
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GENERIC_READ,
            GENERIC_WRITE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
        },
        System::Pipes::WaitNamedPipeW,
    };

    let pipe_name = wide_null(shared_input::SERVICE_CONTROL_PIPE);
    if unsafe { WaitNamedPipeW(pipe_name.as_ptr(), 2_000) } == 0 {
        let error = unsafe { GetLastError() };
        if matches!(
            error,
            ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY | ERROR_SEM_TIMEOUT
        ) {
            return Ok(None);
        }
        return Err(format!(
            "WaitNamedPipeW(MyKVM service control) failed with Windows error {error}"
        ));
    }

    let handle = unsafe {
        CreateFileW(
            pipe_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(windows_last_error("CreateFileW(MyKVM service control)"));
    }

    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let raw = file.as_raw_handle();
    let request = [1_u8];
    let mut written = 0_u32;
    if unsafe {
        WriteFile(
            raw,
            request.as_ptr(),
            request.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    } == 0
        || written != request.len() as u32
    {
        return Err(windows_last_error("write MyKVM service takeover request"));
    }

    let mut ack = [0_u8; 1];
    let mut read = 0_u32;
    if unsafe {
        ReadFile(
            raw,
            ack.as_mut_ptr(),
            ack.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    } == 0
        || read != 1
        || ack[0] != 1
    {
        return Err("MyKVM input service did not acknowledge network takeover.".into());
    }

    Ok(Some(file))
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_input_service_network_lease_alive(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    unsafe {
        PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_input_service_owns_network_ports() -> Result<bool, String> {
    use windows_sys::Win32::System::Services::{
        OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QUERY_SERVICE_CONFIGW,
        SC_MANAGER_CONNECT, SERVICE_QUERY_CONFIG,
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(query config)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_CONFIG);
        if service.is_null() {
            return Ok(false);
        }
        let _service = ServiceHandleGuard(service);

        let mut needed = 0_u32;
        let _ = QueryServiceConfigW(service, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(windows_last_error("QueryServiceConfigW(size)"));
        }
        let words =
            (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
        let mut buffer = vec![0_usize; words];
        let config = buffer.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        if QueryServiceConfigW(service, config, needed, &mut needed) == 0 {
            return Err(windows_last_error("QueryServiceConfigW"));
        }
        let path = (*config).lpBinaryPathName;
        if path.is_null() {
            return Ok(false);
        }
        let len = (0..)
            .find(|index| *path.add(*index) == 0)
            .unwrap_or_default();
        let command = String::from_utf16_lossy(std::slice::from_raw_parts(path, len));
        Ok(command.contains(shared_input::SERVICE_CONFIG_PATH_ARG))
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn install_windows_input_service(
    helper_path: &Path,
    config_path: &Path,
    owner_sid: &str,
) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_EXISTS},
        System::Services::{
            ChangeServiceConfig2W, ChangeServiceConfigW, CreateServiceW, OpenSCManagerW,
            OpenServiceW, SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_CONNECT,
            SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
            SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_FAILURE_ACTIONS_FLAG,
            SERVICE_WIN32_OWN_PROCESS,
        },
    };

    if !helper_path.is_file() {
        return Err(format!(
            "input helper binary does not exist: {}",
            helper_path.display()
        ));
    }

    stop_windows_input_service_and_wait()?;
    let protected_helper_path = install_protected_input_helper(helper_path)?;

    unsafe {
        let scm = OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        );
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);

        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let display_name = wide_null(shared_input::INPUT_SERVICE_DISPLAY_NAME);
        let binary = wide_null(&format!(
            "{} --service {} {} {} {}",
            quote_windows_arg_str(&protected_helper_path.to_string_lossy()),
            shared_input::SERVICE_CONFIG_PATH_ARG,
            quote_windows_arg_str(&config_path.to_string_lossy()),
            shared_input::SERVICE_OWNER_SID_ARG,
            quote_windows_arg_str(owner_sid),
        ));
        let mut service = CreateServiceW(
            scm,
            service_name.as_ptr(),
            display_name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binary.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );

        if service.is_null() {
            let code = GetLastError();
            if code != ERROR_SERVICE_EXISTS {
                return Err(windows_last_error("CreateServiceW"));
            }
            service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_ALL_ACCESS);
            if service.is_null() {
                return Err(windows_last_error("OpenServiceW(existing)"));
            }
            if ChangeServiceConfigW(
                service,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                binary.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                display_name.as_ptr(),
            ) == 0
            {
                let _service = ServiceHandleGuard(service);
                return Err(windows_last_error("ChangeServiceConfigW"));
            }
        }

        let _service = ServiceHandleGuard(service);

        let mut actions = [1000, 5000, 10_000].map(|delay| SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay,
        });
        let recovery = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
            ..Default::default()
        };
        let failure_flags = SERVICE_FAILURE_ACTIONS_FLAG {
            fFailureActionsOnNonCrashFailures: 1,
        };
        if ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&recovery as *const SERVICE_FAILURE_ACTIONSW).cast(),
        ) == 0
            || ChangeServiceConfig2W(
                service,
                SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                (&failure_flags as *const SERVICE_FAILURE_ACTIONS_FLAG).cast(),
            ) == 0
        {
            return Err(windows_last_error("configure input service recovery"));
        }

        // Authenticated users may only inspect service state/config. The owner
        // may start, stop, and remove it, but cannot reconfigure it or its DACL.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let sddl = format!(
            "D:P(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCRPWPLOSDRC;;;{owner_sid})(A;;CCLC;;;AU)"
        );
        let result = std::process::Command::new("sc.exe")
            .args(["sdset", shared_input::INPUT_SERVICE_NAME, &sddl])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("failed to secure input service permissions: {error}"))?;
        if !result.status.success() {
            let detail = String::from_utf8_lossy(if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            });
            return Err(format!(
                "failed to secure input service permissions: {}",
                detail.trim()
            ));
        }

        ensure_windows_input_service_firewall_rule(&protected_helper_path)
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn protected_input_helper_path() -> Result<PathBuf, String> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG},
    };

    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG(0), None)
            .map_err(|error| format!("failed to locate Program Files: {error}"))?;
        let program_files = raw
            .to_string()
            .map_err(|error| format!("invalid Program Files path: {error}"));
        CoTaskMemFree(Some(raw.as_ptr().cast()));
        Ok(PathBuf::from(program_files?)
            .join("MyKVM")
            .join("mykvm-input-helper.exe"))
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn install_protected_input_helper(source: &Path) -> Result<PathBuf, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let target = protected_input_helper_path()?;
    let directory = target
        .parent()
        .ok_or_else(|| "protected input helper path has no parent directory".to_string())?;
    fs::create_dir_all(directory).map_err(|error| {
        format!(
            "failed to create protected input helper directory {}: {error}",
            directory.display()
        )
    })?;
    secure_protected_input_helper_path(directory)?;

    let staging = target.with_extension("exe.installing");
    let _ = fs::remove_file(&staging);
    let mut staging_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(|error| {
            format!(
                "failed to create protected input helper staging file {}: {error}",
                staging.display()
            )
        })?;
    secure_protected_input_helper_path(&staging)?;
    let mut source_file = fs::File::open(source)
        .map_err(|error| format!("failed to open input helper {}: {error}", source.display()))?;
    std::io::copy(&mut source_file, &mut staging_file).map_err(|error| {
        format!(
            "failed to copy input helper to {}: {error}",
            staging.display()
        )
    })?;
    staging_file.sync_all().map_err(|error| {
        format!(
            "failed to flush protected input helper {}: {error}",
            staging.display()
        )
    })?;
    drop(staging_file);

    let staging_wide = wide_null(&staging.to_string_lossy());
    let target_wide = wide_null(&target.to_string_lossy());
    if unsafe {
        MoveFileExW(
            staging_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = windows_last_error("MoveFileExW(protected input helper)");
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = secure_protected_input_helper_path(&target) {
        let _ = fs::remove_file(&target);
        return Err(error);
    }

    Ok(target)
}

#[cfg(target_os = "windows")]
pub(crate) fn secure_protected_input_helper_path(path: &std::path::Path) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        },
    };

    let descriptor = wide_null("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let mut security_descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(windows_last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(protected helper)",
        ));
    }

    let path = wide_null(&path.to_string_lossy());
    let secured = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security_descriptor,
        )
    } != 0;
    let error = (!secured).then(|| windows_last_error("SetFileSecurityW(protected input helper)"));
    unsafe {
        let _ = LocalFree(security_descriptor as HLOCAL);
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn remove_protected_input_helper() {
    let Ok(target) = protected_input_helper_path() else {
        return;
    };
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(target.with_extension("exe.installing"));
    if let Some(directory) = target.parent() {
        let _ = fs::remove_dir(directory);
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn stop_windows_input_service_and_wait() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST,
            ERROR_SERVICE_NOT_ACTIVE,
        },
        System::Services::{
            ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
            SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
            SERVICE_STOPPED, SERVICE_STOP_PENDING,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(stop)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            service_name.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        );
        if service.is_null() {
            if GetLastError() == ERROR_SERVICE_DOES_NOT_EXIST {
                return Ok(());
            }
            return Err(windows_last_error("OpenServiceW(stop)"));
        }
        let _service = ServiceHandleGuard(service);
        let deadline = Instant::now() + Duration::from_secs(15);

        loop {
            let mut status = SERVICE_STATUS::default();
            if QueryServiceStatus(service, &mut status) == 0 {
                return Err(windows_last_error("QueryServiceStatus(stop)"));
            }
            if status.dwCurrentState == SERVICE_STOPPED {
                return Ok(());
            }

            if status.dwCurrentState != SERVICE_STOP_PENDING {
                let mut stop_status = SERVICE_STATUS::default();
                if ControlService(service, SERVICE_CONTROL_STOP, &mut stop_status) == 0 {
                    let code = GetLastError();
                    if code == ERROR_SERVICE_NOT_ACTIVE {
                        return Ok(());
                    }
                    if code != ERROR_SERVICE_CANNOT_ACCEPT_CTRL {
                        return Err(windows_last_error("ControlService(stop)"));
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err("timed out waiting for MyKVM input service to stop".into());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn current_windows_user_sid() -> Result<String, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree, HLOCAL},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(windows_last_error("OpenProcessToken(current user SID)"));
        }

        let mut needed = 0_u32;
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return Err(windows_last_error(
                "GetTokenInformation(current user SID size)",
            ));
        }
        let mut buffer = vec![0_u8; needed as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        ) == 0
        {
            let _ = CloseHandle(token);
            return Err(windows_last_error("GetTokenInformation(current user SID)"));
        }
        let _ = CloseHandle(token);

        let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid) == 0 {
            return Err(windows_last_error("ConvertSidToStringSidW"));
        }
        let len = (0..)
            .find(|index| *sid.add(*index) == 0)
            .unwrap_or_default();
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        let _ = LocalFree(sid as HLOCAL);
        Ok(value)
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn ensure_windows_input_service_firewall_rule(helper_path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const RULE_NAME: &str = "MyKVM Headless Input (UDP-In)";
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={RULE_NAME}"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let result = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={RULE_NAME}"),
            "dir=in",
            "action=allow",
            &format!("program={}", helper_path.display()),
            "protocol=UDP",
            "profile=any",
            "enable=yes",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to configure helper firewall rule: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "failed to configure helper firewall rule: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn remove_windows_input_service_firewall_rule() {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=MyKVM Headless Input (UDP-In)",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(target_os = "windows")]
pub(crate) fn start_windows_input_service() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_ALREADY_RUNNING},
        System::Services::{
            OpenSCManagerW, OpenServiceW, StartServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
            SERVICE_START,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            service_name.as_ptr(),
            SERVICE_START | SERVICE_QUERY_STATUS,
        );
        if service.is_null() {
            return Err(windows_last_error("OpenServiceW(start)"));
        }
        let _service = ServiceHandleGuard(service);
        if StartServiceW(service, 0, std::ptr::null()) == 0 {
            let code = GetLastError();
            if code != ERROR_SERVICE_ALREADY_RUNNING {
                return Err(windows_last_error("StartServiceW"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn uninstall_windows_input_service() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST},
        Storage::FileSystem::DELETE,
        System::Services::{DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT},
    };

    remove_windows_input_service_firewall_rule();
    stop_windows_input_service_and_wait()?;

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), DELETE);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST {
                remove_protected_input_helper();
                return Ok(());
            }
            return Err(windows_last_error("OpenServiceW(uninstall)"));
        }
        let _service = ServiceHandleGuard(service);

        if DeleteService(service) == 0 {
            return Err(windows_last_error("DeleteService"));
        }
    }

    remove_protected_input_helper();
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn resolve_input_helper_path() -> Result<PathBuf, String> {
    let exe =
        env::current_exe().map_err(|error| format!("failed to locate current exe: {error}"))?;
    let exe_dir = exe
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "current exe has no parent directory".to_string())?;
    let candidates = [
        exe_dir.join("mykvm-input-helper.exe"),
        exe_dir.join("mykvm-input-helper-x86_64-pc-windows-msvc.exe"),
        exe_dir
            .join("resources")
            .join("mykvm-input-helper-x86_64-pc-windows-msvc.exe"),
        exe_dir.join("resources").join("mykvm-input-helper.exe"),
    ];

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .or_else(|| candidates.first().cloned())
        .ok_or_else(|| "failed to build input helper path candidates".into())
}

#[cfg(target_os = "windows")]
pub(crate) struct ServiceHandleGuard(windows_sys::Win32::System::Services::SC_HANDLE);

#[cfg(target_os = "windows")]
impl Drop for ServiceHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows_sys::Win32::System::Services::CloseServiceHandle(self.0);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn software_sas_allows_services() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD,
    };

    unsafe {
        let subkey = wide_null(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return false;
        }
        let _key = RegistryKeyGuard(key);

        let value_name = wide_null("SoftwareSASGeneration");
        let mut value_type = 0_u32;
        let mut value = 0_u32;
        let mut value_len = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            &mut value as *mut u32 as *mut u8,
            &mut value_len,
        ) == 0;
        return ok && value_type == REG_DWORD && matches!(value, 1 | 3);
    }

    struct RegistryKeyGuard(windows_sys::Win32::System::Registry::HKEY);
    impl Drop for RegistryKeyGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = RegCloseKey(self.0);
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn sas_dll_available() -> bool {
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    };

    unsafe {
        let dll = LoadLibraryW(wide_null("sas.dll").as_ptr());
        if dll.is_null() {
            return false;
        }
        let available = GetProcAddress(dll, c"SendSAS".as_ptr() as *const u8).is_some();
        let _ = FreeLibrary(dll);
        available
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn quote_windows_arg_str(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        if ch == '"' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_last_error(context: &str) -> String {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("{context} failed with Windows error {code}")
}
