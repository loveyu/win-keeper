use super::{
    AppPaths, PlatformAdapter, ProcessEntry, ProcessGuard, ProcessInfo, build_process_tree,
};
use crate::core::config::ToolConfig;
use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::{
        prctl,
        signal::{SigSet, Signal, kill, killpg},
    },
    unistd::{Pid, getpid, getppid},
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::{Duration, Instant},
};

pub struct LinuxAdapter;

struct ProcessGroup {
    pgid: AtomicI32,
    stop_timeout: Duration,
    watchdog: Option<Watchdog>,
}

struct Watchdog {
    child: Child,
    control: Option<ChildStdin>,
}

impl PlatformAdapter for LinuxAdapter {
    fn prepare_command(
        &self,
        command: &mut Command,
        config: &ToolConfig,
        stop_timeout: Duration,
    ) -> Result<Box<dyn ProcessGuard>> {
        if config.admin {
            bail!("admin=true requires Polkit support, which is outside the first-stage Linux MVP");
        }
        use std::os::unix::process::CommandExt;
        let manager_pid = getpid();
        command.process_group(0);
        // The manager blocks shutdown signals for its sigwait thread; children must not inherit it.
        // PDEATHSIG covers the short window before the process-group watchdog is attached.
        unsafe {
            command.pre_exec(move || {
                shutdown_signals()
                    .thread_unblock()
                    .map_err(std::io::Error::from)?;
                prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::from)?;
                if getppid() != manager_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "WinKeeper exited before the process watchdog was attached",
                    ));
                }
                Ok(())
            });
        }
        Ok(Box::new(ProcessGroup {
            pgid: AtomicI32::new(0),
            stop_timeout,
            watchdog: None,
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
        let pgid = child.id() as i32;
        self.pgid.store(pgid, Ordering::Release);
        self.watchdog = Some(spawn_watchdog(pgid, self.stop_timeout)?);
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

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(mut watchdog) = self.watchdog.take() else {
            return;
        };
        if let Some(mut control) = watchdog.control.take() {
            let _ = control.write_all(b"D");
        }
        let _ = watchdog.child.wait();
    }
}

fn spawn_watchdog(pgid: i32, stop_timeout: Duration) -> Result<Watchdog> {
    use std::os::unix::process::CommandExt;

    let timeout_ms = u64::try_from(stop_timeout.as_millis()).unwrap_or(u64::MAX);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--process-watchdog")
        .arg(pgid.to_string())
        .arg(timeout_ms.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    unsafe {
        command.pre_exec(|| {
            shutdown_signals()
                .thread_unblock()
                .map_err(std::io::Error::from)
        });
    }
    let mut child = command
        .spawn()
        .context("failed to start process watchdog")?;
    let control = child
        .stdin
        .take()
        .context("process watchdog control pipe is unavailable")?;
    let mut ready_pipe = child
        .stdout
        .take()
        .context("process watchdog readiness pipe is unavailable")?;
    let mut ready = [0_u8; 1];
    if ready_pipe.read_exact(&mut ready).is_err() || ready[0] != b'R' {
        drop(control);
        let _ = child.kill();
        let _ = child.wait();
        bail!("process watchdog failed to become ready");
    }
    Ok(Watchdog {
        child,
        control: Some(control),
    })
}

pub fn run_process_watchdog(pgid: i32, stop_timeout: Duration) -> Result<()> {
    if pgid <= 0 {
        bail!("process watchdog requires a positive process group id");
    }
    {
        let mut ready = std::io::stdout().lock();
        ready
            .write_all(b"R")
            .context("failed to report process watchdog readiness")?;
        ready
            .flush()
            .context("failed to flush process watchdog readiness")?;
    }
    let mut command = [0_u8; 1];
    loop {
        match std::io::stdin().read(&mut command) {
            Ok(0) => break,
            Ok(_) if command[0] == b'D' => return Ok(()),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    stop_process_group(pgid, stop_timeout);
    Ok(())
}

fn stop_process_group(pgid: i32, timeout: Duration) {
    let pid = Pid::from_raw(pgid);
    let _ = killpg(pid, Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_group_running(pgid) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if process_group_running(pgid) {
        let _ = killpg(pid, Signal::SIGKILL);
    }
}

fn process_group_running(pgid: i32) -> bool {
    matches!(kill(Pid::from_raw(-pgid), None), Ok(()) | Err(Errno::EPERM))
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
