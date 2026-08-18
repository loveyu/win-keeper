use crate::core::config::ToolConfig;
use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Arc,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

pub struct AppPaths {
    pub config_file: PathBuf,
    pub log_directory: PathBuf,
    pub state_directory: PathBuf,
}

pub trait ProcessGuard: Send {
    fn attach(&mut self, child: &Child) -> Result<()>;
    fn request_graceful_stop(&self) -> Result<bool>;
    fn force_stop(&self) -> Result<()>;
}

pub trait PlatformAdapter: Send + Sync {
    fn prepare_command(
        &self,
        command: &mut Command,
        config: &ToolConfig,
    ) -> Result<Box<dyn ProcessGuard>>;
    fn memory_usage(&self, pid: u32) -> Result<u64>;
    fn open_path(&self, path: &Path) -> Result<()>;
}

#[cfg(target_os = "linux")]
pub fn adapter() -> Arc<dyn PlatformAdapter> {
    Arc::new(linux::LinuxAdapter)
}
#[cfg(windows)]
pub fn adapter() -> Arc<dyn PlatformAdapter> {
    Arc::new(windows::WindowsAdapter)
}

#[cfg(target_os = "linux")]
pub use linux::{configure_autostart, paths, show_error};
#[cfg(windows)]
pub use windows::{configure_autostart, paths, show_error};

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("WinKeeper supports Windows and Linux only");
