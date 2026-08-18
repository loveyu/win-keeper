use super::{AppPaths, PlatformAdapter, ProcessGuard};
use crate::core::config::ToolConfig;
use anyhow::{Context, Result, bail};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::{AtomicI32, Ordering},
};

pub struct LinuxAdapter;

struct ProcessGroup {
    pgid: AtomicI32,
}

impl PlatformAdapter for LinuxAdapter {
    fn prepare_command(
        &self,
        command: &mut Command,
        config: &ToolConfig,
    ) -> Result<Box<dyn ProcessGuard>> {
        if config.admin {
            bail!("admin=true requires Polkit support, which is outside the first-stage Linux MVP");
        }
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(Box::new(ProcessGroup {
            pgid: AtomicI32::new(0),
        }))
    }

    fn memory_usage(&self, pid: u32) -> Result<u64> {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .context("failed to read process status")?;
        let rss = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .context("VmRSS is missing")?
            .split_whitespace()
            .next()
            .context("VmRSS is empty")?
            .parse::<u64>()
            .context("invalid VmRSS")?;
        Ok(rss * 1024)
    }

    fn open_path(&self, path: &Path) -> Result<()> {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .context("failed to launch xdg-open")?;
        Ok(())
    }
}

impl ProcessGuard for ProcessGroup {
    fn attach(&mut self, child: &Child) -> Result<()> {
        self.pgid.store(child.id() as i32, Ordering::Release);
        Ok(())
    }

    fn request_graceful_stop(&self) -> Result<bool> {
        let pgid = self.pgid.load(Ordering::Acquire);
        if pgid > 0 {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
        }
        Ok(true)
    }

    fn force_stop(&self) -> Result<()> {
        let pgid = self.pgid.load(Ordering::Acquire);
        if pgid > 0 {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        Ok(())
    }
}

pub fn paths(config_override: Option<PathBuf>) -> Result<AppPaths> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    let config_file = config_override.unwrap_or_else(|| config_home.join("winkeeper/config.toml"));
    let state_directory = state_home.join("winkeeper");
    Ok(AppPaths {
        config_file,
        log_directory: state_directory.join("logs"),
        state_directory,
    })
}

pub fn configure_autostart(enabled: bool, config_file: &Path) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let directory = home.join(".config/autostart");
    let desktop = directory.join("winkeeper.desktop");
    if !enabled {
        if desktop.exists() {
            fs::remove_file(desktop)?;
        }
        return Ok(());
    }
    fs::create_dir_all(directory)?;
    let exe = std::env::current_exe()?;
    let quote = |path: &Path| format!("\"{}\"", path.to_string_lossy().replace('"', "\\\""));
    fs::write(
        desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=WinKeeper\nComment=Cross-platform process supervisor\nExec={} --config {}\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\n",
            quote(&exe),
            quote(config_file)
        ),
    )?;
    Ok(())
}

pub fn show_error(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}
