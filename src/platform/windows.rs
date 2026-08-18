use super::{AppPaths, PlatformAdapter, ProcessGuard};
use crate::core::config::ToolConfig;
use anyhow::{Context, Result, bail};
use std::{
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
            RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
        },
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, OpenProcess, PROCESS_QUERY_INFORMATION,
            PROCESS_VM_READ,
        },
    },
    UI::{
        Shell::ShellExecuteW,
        WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW, SW_SHOWNORMAL},
    },
};

pub struct WindowsAdapter;

struct JobObject {
    handle: HANDLE,
}

unsafe impl Send for JobObject {}

impl PlatformAdapter for WindowsAdapter {
    fn prepare_command(
        &self,
        command: &mut Command,
        config: &ToolConfig,
    ) -> Result<Box<dyn ProcessGuard>> {
        if config.admin {
            bail!(
                "admin=true requires the elevated helper, which is outside the first-stage Windows MVP"
            );
        }
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        Ok(Box::new(JobObject::new()?))
    }

    fn memory_usage(&self, pid: u32) -> Result<u64> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if process.is_null() {
            return Err(std::io::Error::last_os_error()).context("OpenProcess failed");
        }
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb) };
        unsafe { CloseHandle(process) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("GetProcessMemoryInfo failed");
        }
        Ok(counters.WorkingSetSize as u64)
    }

    fn open_path(&self, path: &Path) -> Result<()> {
        let target = wide(path.as_os_str());
        let verb = wide("open");
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if result <= 32 {
            bail!("ShellExecuteW failed with code {result}");
        }
        Ok(())
    }
}

impl JobObject {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("CreateJobObjectW failed");
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(handle) };
            return Err(std::io::Error::last_os_error()).context("SetInformationJobObject failed");
        }
        Ok(Self { handle })
    }
}

impl ProcessGuard for JobObject {
    fn attach(&mut self, child: &Child) -> Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        if unsafe { AssignProcessToJobObject(self.handle, process) } == 0 {
            return Err(std::io::Error::last_os_error()).context("AssignProcessToJobObject failed");
        }
        Ok(())
    }

    fn request_graceful_stop(&self) -> Result<bool> {
        Ok(false)
    }

    fn force_stop(&self) -> Result<()> {
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            return Err(std::io::Error::last_os_error()).context("TerminateJobObject failed");
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

pub fn paths(config_override: Option<PathBuf>) -> Result<AppPaths> {
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA is not set")?;
    let root = appdata.join("WinKeeper");
    let config_file = config_override.unwrap_or_else(|| root.join("config.toml"));
    Ok(AppPaths {
        config_file,
        log_directory: root.join("logs"),
        state_directory: root,
    })
}

pub fn configure_autostart(enabled: bool, config_file: &Path) -> Result<()> {
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = wide("WinKeeper");
    let mut key: HKEY = null_mut();
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    };
    if result != 0 {
        bail!("failed to open Windows Run registry key: {result}");
    }
    let operation = if enabled {
        let command = format!(
            "\"{}\" --config \"{}\"",
            std::env::current_exe()?.display(),
            config_file.display()
        );
        let data = wide(command);
        unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * size_of::<u16>()) as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, value_name.as_ptr()) }
    };
    unsafe { RegCloseKey(key) };
    if operation != 0 && !(operation == 2 && !enabled) {
        bail!("failed to update Windows Run registry value: {operation}");
    }
    Ok(())
}

pub fn show_error(title: &str, message: &str) {
    let title = wide(title);
    let message = wide(message);
    unsafe {
        MessageBoxW(
            HWND::default(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
