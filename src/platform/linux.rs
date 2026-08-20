use super::{
    AppPaths, PlatformAdapter, ProcessEntry, ProcessGuard, ProcessInfo, build_process_tree,
};
use crate::core::config::ToolConfig;
use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::signal::{SigSet, Signal, kill, killpg},
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
        // The manager blocks shutdown signals for its sigwait thread; children must not inherit it.
        unsafe {
            command.pre_exec(|| {
                shutdown_signals()
                    .thread_unblock()
                    .map_err(std::io::Error::from)
            });
        }
        Ok(Box::new(ProcessGroup {
            pgid: AtomicI32::new(0),
        }))
    }

    fn memory_usage(&self, pid: u32) -> Result<u64> {
        let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
            .context("failed to read process smaps rollup")?;
        let pss = rollup
            .lines()
            .find_map(|line| line.strip_prefix("Pss:"))
            .context("Pss is missing")?
            .split_whitespace()
            .next()
            .context("Pss is empty")?
            .parse::<u64>()
            .context("invalid Pss")?;
        Ok(pss * 1024)
    }

    fn process_tree(&self, root_pid: u32) -> Result<Vec<ProcessInfo>> {
        let mut processes = Vec::new();
        for entry in fs::read_dir("/proc").context("failed to enumerate /proc")? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let stat = match fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(_) => continue,
            };
            let Some(close_paren) = stat.rfind(')') else {
                continue;
            };
            let comm = stat
                .find('(')
                .map(|open_paren| stat[open_paren + 1..close_paren].to_owned())
                .unwrap_or_default();
            let Some(parent_pid) = stat[close_paren + 1..]
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            let command = fs::read(entry.path().join("cmdline"))
                .ok()
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .trim_end_matches('\0')
                        .replace('\0', " ")
                })
                .filter(|command| !command.is_empty())
                .unwrap_or(comm);
            processes.push(ProcessEntry {
                pid,
                parent_pid,
                name: command,
                memory_bytes: self.memory_usage(pid).ok(),
            });
        }
        Ok(build_process_tree(root_pid, processes))
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

    fn is_tree_running(&self) -> Result<bool> {
        let pgid = self.pgid.load(Ordering::Acquire);
        if pgid <= 0 {
            return Ok(false);
        }
        match kill(Pid::from_raw(-pgid), None) {
            Ok(()) | Err(Errno::EPERM) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn force_stop(&self) -> Result<()> {
        let pgid = self.pgid.load(Ordering::Acquire);
        if pgid > 0 {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        Ok(())
    }
}

fn shutdown_signals() -> SigSet {
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGINT);
    signals.add(Signal::SIGTERM);
    signals
}

pub fn prepare_shutdown_signals() -> Result<()> {
    shutdown_signals().thread_block()?;
    Ok(())
}

pub fn wait_for_shutdown_signal() -> Result<()> {
    shutdown_signals().wait()?;
    Ok(())
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
            "[Desktop Entry]\nType=Application\nName=WinKeeper\nComment=Cross-platform process supervisor\nExec={} --config {} --autostart\nIcon=win-keeper\nStartupWMClass=win-keeper\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\n",
            quote(&exe),
            quote(config_file)
        ),
    )?;
    Ok(())
}

pub fn show_error(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}
