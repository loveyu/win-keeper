use super::{
    AppPaths, PlatformAdapter, ProcessEntry, ProcessGuard, ProcessInfo,
    build_process_tree_with_members,
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
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicI32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static SCOPE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct LinuxAdapter {
    systemd: Option<SystemdSupport>,
}

#[derive(Clone)]
struct SystemdSupport {
    run: PathBuf,
    ctl: PathBuf,
}

#[derive(Clone)]
struct SystemdScope {
    unit: String,
    cgroup: Option<PathBuf>,
    support: SystemdSupport,
}

struct ProcessGroup {
    pgid: AtomicI32,
    stop_timeout: Duration,
    scope: Option<SystemdScope>,
    watchdog: Option<Watchdog>,
}

struct Watchdog {
    child: Child,
    control: Option<ChildStdin>,
}

impl LinuxAdapter {
    pub fn new() -> Self {
        Self {
            systemd: SystemdSupport::detect(),
        }
    }
}

impl SystemdSupport {
    fn detect() -> Option<Self> {
        let runtime_directory = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
        if !runtime_directory.join("systemd/private").exists() {
            return None;
        }
        let support = Self {
            run: find_executable("systemd-run")?,
            ctl: find_executable("systemctl")?,
        };
        let mut command = Command::new(&support.ctl);
        command
            .args(["--user", "--no-ask-password", "show-environment"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unblock_shutdown_signals(&mut command);
        command
            .status()
            .ok()
            .filter(|status| status.success())
            .map(|_| support)
    }
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
        let scope = self.systemd.as_ref().map(|support| {
            let unit = next_scope_unit("tool");
            wrap_command_in_scope(command, support, &unit, &config.name);
            SystemdScope {
                unit,
                cgroup: None,
                support: support.clone(),
            }
        });
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
            scope,
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
        let root_stat = fs::read_to_string(format!("/proc/{root_pid}/stat"))
            .context("failed to read root process stat")?;
        let (_, _, root_pgid) = parse_proc_stat(&root_stat).context("invalid root process stat")?;
        let root_cgroup = process_cgroup(root_pid).ok().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("winkeeper-tool-") && name.ends_with(".scope"))
        });
        let mut processes = Vec::new();
        let mut managed_members = HashSet::new();
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
            let Some((comm, parent_pid, pgid)) = parse_proc_stat(&stat) else {
                continue;
            };
            if pgid == root_pgid
                || root_cgroup.as_ref().is_some_and(|root| {
                    process_cgroup(pid).is_ok_and(|candidate| candidate.starts_with(root))
                })
            {
                managed_members.insert(pid);
            }
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
        Ok(build_process_tree_with_members(
            root_pid,
            processes,
            managed_members,
        ))
    }

    fn open_path(&self, path: &Path) -> Result<()> {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        unblock_shutdown_signals(&mut command);
        spawn_reaped(&mut command, "xdg-open-reaper").context("failed to launch xdg-open")?;
        Ok(())
    }
}

fn spawn_reaped(command: &mut Command, thread_name: &str) -> Result<u32> {
    let (sender, receiver) = mpsc::sync_channel::<Child>(1);
    thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Ok(mut child) = receiver.recv() {
                let _ = child.wait();
            }
        })
        .context("failed to start child reaper")?;
    let child = command.spawn()?;
    let pid = child.id();
    if let Err(error) = sender.send(child) {
        let mut child = error.0;
        let _ = child.kill();
        let _ = child.wait();
        bail!("child reaper stopped unexpectedly");
    }
    Ok(pid)
}

impl ProcessGuard for ProcessGroup {
    fn attach(&mut self, child: &Child) -> Result<()> {
        let pgid = child.id() as i32;
        self.pgid.store(pgid, Ordering::Release);
        if let Some(scope) = self.scope.as_mut() {
            scope.cgroup = Some(wait_for_scope_cgroup(
                &scope.support,
                &scope.unit,
                child.id(),
                Duration::from_secs(3),
            )?);
        }
        self.watchdog = Some(spawn_watchdog(
            pgid,
            self.stop_timeout,
            self.scope.as_ref(),
        )?);
        Ok(())
    }

    fn request_graceful_stop(&self) -> Result<bool> {
        signal_managed_tree(
            self.pgid.load(Ordering::Acquire),
            self.scope.as_ref(),
            Signal::SIGTERM,
        )?;
        Ok(true)
    }

    fn is_tree_running(&self) -> Result<bool> {
        if let Some(scope) = &self.scope
            && let Some(cgroup) = &scope.cgroup
        {
            return cgroup_populated(cgroup);
        }
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
        signal_managed_tree(
            self.pgid.load(Ordering::Acquire),
            self.scope.as_ref(),
            Signal::SIGKILL,
        )
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

fn spawn_watchdog(
    pgid: i32,
    stop_timeout: Duration,
    scope: Option<&SystemdScope>,
) -> Result<Watchdog> {
    use std::os::unix::process::CommandExt;

    let timeout_ms = u64::try_from(stop_timeout.as_millis()).unwrap_or(u64::MAX);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--process-watchdog")
        .arg(pgid.to_string())
        .arg(timeout_ms.to_string());
    if let Some(scope) = scope {
        command
            .arg("--scope-unit")
            .arg(&scope.unit)
            .arg("--scope-cgroup")
            .arg(
                scope
                    .cgroup
                    .as_ref()
                    .context("scope cgroup is unavailable")?,
            )
            .arg("--scope-systemctl")
            .arg(&scope.support.ctl);
        let watchdog_unit = next_scope_unit("watchdog");
        wrap_command_in_scope(
            &mut command,
            &scope.support,
            &watchdog_unit,
            "WinKeeper process watchdog",
        );
    }
    command
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

pub fn run_process_watchdog(
    pgid: i32,
    stop_timeout: Duration,
    scope_unit: Option<String>,
    scope_cgroup: Option<PathBuf>,
    scope_systemctl: Option<PathBuf>,
) -> Result<()> {
    if pgid <= 0 {
        bail!("process watchdog requires a positive process group id");
    }
    let scope = match (scope_unit, scope_cgroup, scope_systemctl) {
        (Some(unit), Some(cgroup), Some(ctl)) => Some(SystemdScope {
            unit,
            cgroup: Some(cgroup),
            support: SystemdSupport {
                run: PathBuf::new(),
                ctl,
            },
        }),
        (None, None, None) => None,
        _ => bail!("process watchdog scope arguments must be provided together"),
    };
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
    stop_managed_tree(pgid, scope.as_ref(), stop_timeout);
    Ok(())
}

fn stop_managed_tree(pgid: i32, scope: Option<&SystemdScope>, timeout: Duration) {
    let _ = signal_managed_tree(pgid, scope, Signal::SIGTERM);
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or(Instant::now());
    while Instant::now() < deadline {
        if !managed_tree_running(pgid, scope) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if managed_tree_running(pgid, scope) {
        let _ = signal_managed_tree(pgid, scope, Signal::SIGKILL);
    }
}

fn managed_tree_running(pgid: i32, scope: Option<&SystemdScope>) -> bool {
    scope
        .and_then(|scope| scope.cgroup.as_ref())
        .is_some_and(|cgroup| cgroup_populated(cgroup).unwrap_or(true))
        || process_group_running(pgid)
}

fn process_group_running(pgid: i32) -> bool {
    matches!(kill(Pid::from_raw(-pgid), None), Ok(()) | Err(Errno::EPERM))
}

fn signal_managed_tree(pgid: i32, scope: Option<&SystemdScope>, signal: Signal) -> Result<()> {
    let mut delivered = false;
    let mut errors = Vec::new();
    if let Some(scope) = scope {
        match signal_scope(scope, signal) {
            Ok(()) => delivered = true,
            Err(error) => errors.push(format!("scope {}: {error:#}", scope.unit)),
        }
    }
    if pgid > 0 {
        match killpg(Pid::from_raw(pgid), signal) {
            Ok(()) => delivered = true,
            Err(Errno::ESRCH) => {}
            Err(error) => errors.push(format!("process group {pgid}: {error}")),
        }
    }
    if delivered || errors.is_empty() || !managed_tree_running(pgid, scope) {
        Ok(())
    } else {
        bail!(
            "failed to signal managed process tree: {}",
            errors.join("; ")
        )
    }
}

fn signal_scope(scope: &SystemdScope, signal: Signal) -> Result<()> {
    let mut command = Command::new(&scope.support.ctl);
    command
        .args(["--user", "--no-ask-password", "kill", "--kill-whom=all"])
        .arg(format!("--signal={}", signal_name(signal)))
        .arg(&scope.unit)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unblock_shutdown_signals(&mut command);
    let status = command
        .status()
        .context("failed to execute systemctl kill")?;
    if !status.success() {
        bail!("systemctl kill exited with {status}");
    }
    Ok(())
}

fn signal_name(signal: Signal) -> &'static str {
    match signal {
        Signal::SIGTERM => "SIGTERM",
        Signal::SIGKILL => "SIGKILL",
        _ => "SIGTERM",
    }
}

fn cgroup_populated(cgroup: &Path) -> Result<bool> {
    match fs::read_to_string(cgroup.join("cgroup.events")) {
        Ok(events) => Ok(events
            .lines()
            .any(|line| line.split_whitespace().eq(["populated", "1"]))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("failed to read cgroup.events"),
    }
}

fn wait_for_scope_cgroup(
    support: &SystemdSupport,
    unit: &str,
    child_pid: u32,
    timeout: Duration,
) -> Result<PathBuf> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or(Instant::now());
    loop {
        if let Ok(cgroup) = process_cgroup(child_pid)
            && cgroup
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == unit)
        {
            return Ok(cgroup);
        }
        if let Ok(cgroup) = query_scope_cgroup(support, unit) {
            return Ok(cgroup);
        }
        if Instant::now() >= deadline {
            bail!("systemd scope {unit} did not become ready");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn query_scope_cgroup(support: &SystemdSupport, unit: &str) -> Result<PathBuf> {
    let mut command = Command::new(&support.ctl);
    command
        .args([
            "--user",
            "--no-ask-password",
            "show",
            "--property=ControlGroup",
            "--value",
        ])
        .arg(unit)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    unblock_shutdown_signals(&mut command);
    let output = command
        .output()
        .context("failed to query systemd scope cgroup")?;
    if !output.status.success() {
        bail!("systemctl show exited with {}", output.status);
    }
    let relative = String::from_utf8(output.stdout).context("systemd scope cgroup is not UTF-8")?;
    let relative = relative.trim();
    if relative.is_empty() {
        bail!("systemd scope cgroup is empty");
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
}

fn process_cgroup(pid: u32) -> Result<PathBuf> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let relative = raw
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("unified cgroup entry is unavailable")?;
    Ok(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
}

fn parse_proc_stat(stat: &str) -> Option<(String, u32, u32)> {
    let open_paren = stat.find('(')?;
    let close_paren = stat.rfind(')')?;
    let comm = stat[open_paren + 1..close_paren].to_owned();
    let mut fields = stat[close_paren + 1..].split_whitespace();
    let _state = fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let process_group = fields.next()?.parse().ok()?;
    Some((comm, parent_pid, process_group))
}

fn next_scope_unit(kind: &str) -> String {
    let sequence = SCOPE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("winkeeper-{kind}-{}-{sequence}.scope", getpid().as_raw())
}

fn wrap_command_in_scope(
    command: &mut Command,
    support: &SystemdSupport,
    unit: &str,
    description: &str,
) {
    let program = command.get_program().to_os_string();
    let arguments = command.get_args().map(OsString::from).collect::<Vec<_>>();
    let current_directory = command.get_current_dir().map(Path::to_path_buf);
    let environment = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsString::from)))
        .collect::<Vec<_>>();

    let mut scoped = Command::new(&support.run);
    scoped
        .args(["--user", "--scope", "--quiet"])
        .arg(format!("--unit={unit}"))
        .arg(format!("--description={description}"))
        .arg("--property=KillMode=control-group")
        .arg("--")
        .arg(program)
        .args(arguments);
    if let Some(directory) = current_directory {
        scoped.current_dir(directory);
    }
    for (key, value) in environment {
        if let Some(value) = value {
            scoped.env(key, value);
        } else {
            scoped.env_remove(key);
        }
    }
    *command = scoped;
}

fn find_executable(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn unblock_shutdown_signals(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            shutdown_signals()
                .thread_unblock()
                .map_err(std::io::Error::from)
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_child_is_reaped() {
        let mut command = Command::new("/usr/bin/true");
        let pid = spawn_reaped(&mut command, "child-reaper-test").unwrap() as i32;
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_group_or_pid_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_group_or_pid_exists(pid));
    }

    #[test]
    fn systemd_scope_wrapper_preserves_launch_specification() {
        let support = SystemdSupport {
            run: PathBuf::from("/usr/bin/systemd-run"),
            ctl: PathBuf::from("/usr/bin/systemctl"),
        };
        let mut command = Command::new("/opt/example tool");
        command
            .args(["--flag", "value with spaces"])
            .current_dir("/tmp")
            .env("WINKEEPER_TEST_ENV", "present");
        wrap_command_in_scope(
            &mut command,
            &support,
            "winkeeper-tool-test.scope",
            "test tool",
        );

        assert_eq!(command.get_program(), "/usr/bin/systemd-run");
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"--unit=winkeeper-tool-test.scope".into()));
        assert!(arguments.contains(&"/opt/example tool".into()));
        assert!(arguments.contains(&"value with spaces".into()));
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
        assert!(command.get_envs().any(|(key, value)| {
            key == "WINKEEPER_TEST_ENV" && value == Some(std::ffi::OsStr::new("present"))
        }));
    }

    fn process_group_or_pid_exists(pid: i32) -> bool {
        matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
    }
}
