use super::{
    config::{AppConfig, ToolConfig},
    logging::ToolLog,
};
use crate::platform::{AppPaths, PlatformAdapter, ProcessGuard, ProcessInfo};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Restarting,
    Crashed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "bytes")]
pub enum MemoryValue {
    Idle,
    Pending,
    Bytes(u64),
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolSnapshot {
    pub name: String,
    pub state: ToolState,
    pub pid: Option<u32>,
    pub memory: MemoryValue,
    #[serde(default)]
    pub restart_count: usize,
    #[serde(default)]
    pub started_at_unix_ms: Option<u64>,
}

#[derive(Clone)]
struct Runtime {
    state: ToolState,
    pid: Option<u32>,
    memory: MemoryValue,
    restart_count: usize,
    started_at_unix_ms: Option<u64>,
}

enum Control {
    Start,
    Stop,
    Restart,
    #[cfg(windows)]
    SampleMemory,
    Shutdown,
}

struct ToolHandle {
    config: ToolConfig,
    runtime: Arc<Mutex<Runtime>>,
    log: Arc<ToolLog>,
    control: Sender<Control>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    external_log: bool,
}

pub struct Supervisor {
    config: AppConfig,
    paths: AppPaths,
    platform: Arc<dyn PlatformAdapter>,
    tools: Vec<ToolHandle>,
    manager_memory: Arc<Mutex<MemoryValue>>,
    manager_log: Arc<ToolLog>,
    manager_started_at_unix_ms: u64,
}

struct ManagedProcess {
    child: Child,
    guard: Box<dyn ProcessGuard>,
}

impl Supervisor {
    pub fn new(
        config: AppConfig,
        paths: AppPaths,
        platform: Arc<dyn PlatformAdapter>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&paths.state_directory)?;
        let manager_memory = Arc::new(Mutex::new(MemoryValue::Idle));
        schedule_initial_manager_memory_refresh(manager_memory.clone(), platform.clone());
        let manager_started_at_unix_ms = unix_ms();
        let manager_log = Arc::new(ToolLog::new(
            &paths.log_directory,
            "win-keeper-manager",
            config.manager.log_buffer_lines,
        )?);
        manager_log.write(
            "manager",
            &format!(
                "WinKeeper {} started, pid={}, config={}, tools={}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                paths.config_file.display(),
                config.tools.len()
            ),
        );
        let mut tools = Vec::with_capacity(config.tools.len());
        for tool in &config.tools {
            let runtime = Arc::new(Mutex::new(Runtime {
                state: ToolState::Stopped,
                pid: None,
                memory: MemoryValue::Idle,
                restart_count: 0,
                started_at_unix_ms: None,
            }));
            let log = Arc::new(ToolLog::new(
                &paths.log_directory,
                &tool.name,
                config.manager.log_buffer_lines,
            )?);
            let (tx, rx) = mpsc::channel();
            #[cfg(windows)]
            let external_log = tool.admin;
            #[cfg(not(windows))]
            let external_log = false;
            #[cfg(windows)]
            let worker = if tool.admin {
                spawn_elevated_proxy_worker(
                    tool.clone(),
                    runtime.clone(),
                    log.clone(),
                    rx,
                    paths.state_directory.clone(),
                )
            } else {
                spawn_worker(
                    tool.clone(),
                    runtime.clone(),
                    log.clone(),
                    rx,
                    platform.clone(),
                    Duration::from_millis(config.manager.stop_timeout_ms),
                )
            };
            #[cfg(not(windows))]
            let worker = spawn_worker(
                tool.clone(),
                runtime.clone(),
                log.clone(),
                rx,
                platform.clone(),
                Duration::from_millis(config.manager.stop_timeout_ms),
            );
            let handle = ToolHandle {
                config: tool.clone(),
                runtime,
                log,
                control: tx,
                worker: Mutex::new(Some(worker)),
                external_log,
            };
            tools.push(handle);
        }
        Ok(Self {
            config,
            paths,
            platform,
            tools,
            manager_memory,
            manager_log,
            manager_started_at_unix_ms,
        })
    }

    pub fn snapshots(&self) -> Vec<ToolSnapshot> {
        self.tools
            .iter()
            .map(|tool| {
                let runtime = tool.runtime.lock().unwrap();
                ToolSnapshot {
                    name: tool.config.name.clone(),
                    state: runtime.state,
                    pid: runtime.pid,
                    memory: runtime.memory,
                    restart_count: runtime.restart_count,
                    started_at_unix_ms: runtime.started_at_unix_ms,
                }
            })
            .collect()
    }

    pub fn manager_memory(&self) -> MemoryValue {
        *self.manager_memory.lock().unwrap()
    }

    pub fn manager_started_at_unix_ms(&self) -> u64 {
        self.manager_started_at_unix_ms
    }

    pub fn process_tree(&self, index: usize) -> Result<(String, Vec<ProcessInfo>)> {
        let (name, pid) = if let Some(tool) = self.tools.get(index) {
            (tool.config.name.clone(), tool.runtime.lock().unwrap().pid)
        } else if index == self.tools.len() {
            ("WinKeeper".into(), Some(std::process::id()))
        } else {
            anyhow::bail!("invalid tool index");
        };
        let pid = pid.context("process is not running")?;
        Ok((name, self.platform.process_tree(pid)?))
    }

    pub fn config_path(&self) -> &Path {
        &self.paths.config_file
    }
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
    pub fn start(&self, index: usize) {
        self.log_tool_request(index, "start");
        self.send(index, Control::Start);
    }
    pub fn stop(&self, index: usize) {
        self.log_tool_request(index, "stop");
        self.send(index, Control::Stop);
    }
    pub fn restart(&self, index: usize) {
        self.log_tool_request(index, "restart");
        self.send(index, Control::Restart);
    }
    pub fn start_all(&self) {
        self.manager_log.write("ui", "start all requested");
        for tool in &self.tools {
            let _ = tool.control.send(Control::Start);
        }
    }
    pub fn stop_all(&self) {
        self.manager_log.write("ui", "stop all requested");
        for tool in &self.tools {
            let _ = tool.control.send(Control::Stop);
        }
    }
    pub fn restart_all(&self) {
        self.manager_log.write("ui", "restart all requested");
        for tool in &self.tools {
            let _ = tool.control.send(Control::Restart);
        }
    }

    pub fn shutdown(&self) {
        self.manager_log
            .write("manager", "WinKeeper is shutting down");
        for tool in &self.tools {
            let _ = tool.control.send(Control::Shutdown);
        }
        for tool in &self.tools {
            if let Some(worker) = tool.worker.lock().unwrap().take() {
                let _ = worker.join();
            }
        }
    }

    pub fn log_snapshot(&self, index: usize) -> String {
        if let Some(tool) = self.tools.get(index) {
            if tool.external_log {
                tool.log.snapshot_external()
            } else {
                tool.log.snapshot()
            }
        } else if index == self.tools.len() {
            self.manager_log.snapshot()
        } else {
            String::new()
        }
    }

    pub fn open_log(&self, index: usize) -> Result<()> {
        if let Some(tool) = self.tools.get(index) {
            self.platform.open_path(tool.log.path())
        } else if index == self.tools.len() {
            self.platform.open_path(self.manager_log.path())
        } else {
            anyhow::bail!("invalid tool index")
        }
    }

    pub fn open_workdir(&self, index: usize) -> Result<()> {
        let path = if let Some(tool) = self.tools.get(index) {
            tool.config
                .workdir
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    Path::new(&tool.config.command)
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_path_buf()
                })
        } else if index == self.tools.len() {
            std::env::current_exe()
                .context("failed to resolve WinKeeper executable path")?
                .parent()
                .context("WinKeeper executable has no parent directory")?
                .to_path_buf()
        } else {
            anyhow::bail!("invalid tool index")
        };
        self.platform.open_path(&path)
    }

    pub fn open_config(&self) -> Result<()> {
        self.platform.open_path(&self.paths.config_file)
    }

    pub fn sample_memory(&self) {
        *self.manager_memory.lock().unwrap() = MemoryValue::Pending;
        let platform = self.platform.clone();
        let manager_memory = self.manager_memory.clone();
        thread::spawn(move || {
            let value = platform
                .process_tree_memory_usage(std::process::id())
                .map(MemoryValue::Bytes)
                .unwrap_or(MemoryValue::Unavailable);
            *manager_memory.lock().unwrap() = value;
        });

        for tool in &self.tools {
            #[cfg(windows)]
            if tool.config.admin {
                let mut runtime = tool.runtime.lock().unwrap();
                if runtime.state == ToolState::Running {
                    runtime.memory = MemoryValue::Pending;
                    let _ = tool.control.send(Control::SampleMemory);
                } else {
                    runtime.memory = MemoryValue::Idle;
                }
                continue;
            }
            let pid = {
                let mut runtime = tool.runtime.lock().unwrap();
                if runtime.state == ToolState::Running {
                    runtime.memory = MemoryValue::Pending;
                    runtime.pid
                } else {
                    runtime.memory = MemoryValue::Idle;
                    None
                }
            };
            let Some(pid) = pid else { continue };
            let runtime = tool.runtime.clone();
            let platform = self.platform.clone();
            thread::spawn(move || {
                let value = platform
                    .process_tree_memory_usage(pid)
                    .map(MemoryValue::Bytes)
                    .unwrap_or(MemoryValue::Unavailable);
                let mut current = runtime.lock().unwrap();
                if current.pid == Some(pid) {
                    current.memory = value;
                }
            });
        }
    }

    pub fn minimize_to_tray(&self) -> bool {
        self.config.manager.minimize_to_tray
    }

    pub fn configured_chinese(&self) -> Option<bool> {
        self.config
            .manager
            .lang
            .as_deref()
            .and_then(super::config::configured_chinese)
    }

    pub fn take_activation_request(&self) -> bool {
        super::single_instance::SingleInstanceGuard::take_activation_request(
            &self.paths.state_directory,
        )
        .unwrap_or(false)
    }

    fn send(&self, index: usize, control: Control) {
        if let Some(tool) = self.tools.get(index) {
            let _ = tool.control.send(control);
        }
    }

    fn log_tool_request(&self, index: usize, action: &str) {
        if let Some(tool) = self.tools.get(index) {
            self.manager_log.write(
                "ui",
                &format!("{action} requested for {}", tool.config.name),
            );
        }
    }
}

fn spawn_worker(
    config: ToolConfig,
    runtime: Arc<Mutex<Runtime>>,
    log: Arc<ToolLog>,
    receiver: Receiver<Control>,
    platform: Arc<dyn PlatformAdapter>,
    stop_timeout: Duration,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("supervisor-{}", config.name))
        .spawn(move || worker_loop(config, runtime, log, receiver, platform, stop_timeout))
        .expect("failed to create supervisor thread")
}

fn worker_loop(
    config: ToolConfig,
    runtime: Arc<Mutex<Runtime>>,
    log: Arc<ToolLog>,
    receiver: Receiver<Control>,
    platform: Arc<dyn PlatformAdapter>,
    stop_timeout: Duration,
) {
    let mut process: Option<ManagedProcess> = None;
    let mut desired_running = config.auto_start;
    let mut next_start = if desired_running {
        Some(Instant::now())
    } else {
        None
    };
    let mut restarts = VecDeque::new();
    let mut has_started_once = false;

    loop {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Control::Start) => {
                desired_running = true;
                next_start = Some(Instant::now());
                if process.is_none() {
                    restarts.clear();
                    runtime.lock().unwrap().restart_count = 0;
                }
            }
            Ok(Control::Stop) => {
                desired_running = false;
                next_start = None;
                stop_process(&mut process, &runtime, &log, stop_timeout);
            }
            Ok(Control::Restart) => {
                desired_running = true;
                next_start = Some(Instant::now());
                let mut state = runtime.lock().unwrap();
                state.state = ToolState::Restarting;
                if process.is_some() {
                    state.restart_count += 1;
                }
                drop(state);
                stop_process(&mut process, &runtime, &log, stop_timeout);
            }
            #[cfg(windows)]
            Ok(Control::SampleMemory) => {}
            Ok(Control::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop_process(&mut process, &runtime, &log, stop_timeout);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if let Some(active) = process.as_mut() {
            match active.child.try_wait() {
                Ok(Some(status)) => {
                    log.write("manager", &format!("process exited with {status}"));
                    stop_remaining_process_tree(active.guard.as_ref(), &log, stop_timeout);
                    process = None;
                    let mut state = runtime.lock().unwrap();
                    state.pid = None;
                    state.memory = MemoryValue::Idle;
                    state.started_at_unix_ms = None;
                    if desired_running && config.auto_restart {
                        let now = Instant::now();
                        let window = Duration::from_secs(config.restart_window_seconds);
                        while restarts
                            .front()
                            .is_some_and(|time| now.duration_since(*time) > window)
                        {
                            restarts.pop_front();
                        }
                        if restarts.len() >= config.max_restart_count {
                            state.state = ToolState::Crashed;
                            desired_running = false;
                            log.write(
                                "manager",
                                "automatic restart limit reached; tool marked Crashed",
                            );
                        } else {
                            restarts.push_back(now);
                            state.restart_count += 1;
                            state.state = ToolState::Restarting;
                            next_start = Some(now + Duration::from_millis(config.restart_delay_ms));
                            log.write("manager", "scheduled automatic restart");
                        }
                    } else {
                        state.state = ToolState::Stopped;
                    }
                }
                Ok(None) => {}
                Err(error) => log.write("manager", &format!("failed to query process: {error}")),
            }
        }

        if process.is_none()
            && desired_running
            && next_start.is_some_and(|deadline| Instant::now() >= deadline)
        {
            next_start = None;
            match start_process(&config, &runtime, &log, platform.as_ref(), stop_timeout) {
                Ok(started) => {
                    let delays = if has_started_once {
                        vec![Duration::from_secs(5)]
                    } else {
                        vec![Duration::from_secs(5), Duration::from_secs(10)]
                    };
                    schedule_tool_memory_refresh(
                        started.child.id(),
                        runtime.clone(),
                        platform.clone(),
                        delays,
                    );
                    has_started_once = true;
                    process = Some(started);
                }
                Err(error) => {
                    log.write("manager", &format!("failed to start: {error:#}"));
                    let mut state = runtime.lock().unwrap();
                    state.state = ToolState::Crashed;
                    if config.auto_restart {
                        let now = Instant::now();
                        while restarts.front().is_some_and(|time| {
                            now.duration_since(*time)
                                > Duration::from_secs(config.restart_window_seconds)
                        }) {
                            restarts.pop_front();
                        }
                        if restarts.len() < config.max_restart_count {
                            restarts.push_back(now);
                            state.restart_count += 1;
                            state.state = ToolState::Restarting;
                            next_start = Some(now + Duration::from_millis(config.restart_delay_ms));
                        } else {
                            desired_running = false;
                        }
                    } else {
                        desired_running = false;
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn spawn_elevated_proxy_worker(
    config: ToolConfig,
    runtime: Arc<Mutex<Runtime>>,
    log: Arc<ToolLog>,
    receiver: Receiver<Control>,
    state_directory: PathBuf,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("elevated-proxy-{}", config.name))
        .spawn(move || {
            use crate::core::elevated::{ElevatedCommand, read_state, send_command};

            let mut desired_running = config.auto_start;
            let mut unavailable_since = Some(Instant::now());
            if desired_running {
                runtime.lock().unwrap().state = ToolState::Starting;
                if let Err(error) =
                    send_command(&state_directory, &config.name, ElevatedCommand::Start)
                {
                    log.write(
                        "manager",
                        &format!("failed to contact elevated helper: {error:#}"),
                    );
                }
            }

            loop {
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(Control::Start) => {
                        desired_running = true;
                        runtime.lock().unwrap().state = ToolState::Starting;
                        let _ =
                            send_command(&state_directory, &config.name, ElevatedCommand::Start);
                    }
                    Ok(Control::Stop) => {
                        desired_running = false;
                        runtime.lock().unwrap().state = ToolState::Stopping;
                        let _ = send_command(&state_directory, &config.name, ElevatedCommand::Stop);
                    }
                    Ok(Control::Restart) => {
                        desired_running = true;
                        runtime.lock().unwrap().state = ToolState::Restarting;
                        let _ =
                            send_command(&state_directory, &config.name, ElevatedCommand::Restart);
                    }
                    Ok(Control::SampleMemory) => {
                        let _ = send_command(
                            &state_directory,
                            &config.name,
                            ElevatedCommand::SampleMemory,
                        );
                    }
                    Ok(Control::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = send_command(&state_directory, &config.name, ElevatedCommand::Stop);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }

                match read_state(&state_directory, &config.name) {
                    Ok(Some(state)) if state.is_fresh() => {
                        unavailable_since = None;
                        let mut current = runtime.lock().unwrap();
                        current.state = state.snapshot.state;
                        current.pid = state.snapshot.pid;
                        current.memory = state.snapshot.memory;
                        current.restart_count = state.snapshot.restart_count;
                        current.started_at_unix_ms = state.snapshot.started_at_unix_ms;
                    }
                    Ok(_) | Err(_) => {
                        let since = unavailable_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(8) {
                            let mut current = runtime.lock().unwrap();
                            current.state = if desired_running {
                                ToolState::Crashed
                            } else {
                                ToolState::Stopped
                            };
                            current.pid = None;
                            current.memory = MemoryValue::Unavailable;
                            current.started_at_unix_ms = None;
                        }
                    }
                }
            }
        })
        .expect("failed to create elevated proxy thread")
}

fn start_process(
    config: &ToolConfig,
    runtime: &Arc<Mutex<Runtime>>,
    log: &Arc<ToolLog>,
    platform: &dyn PlatformAdapter,
    stop_timeout: Duration,
) -> Result<ManagedProcess> {
    runtime.lock().unwrap().state = ToolState::Starting;
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(workdir) = &config.workdir {
        if !Path::new(workdir).is_dir() {
            anyhow::bail!("working directory does not exist: {workdir}");
        }
        command.current_dir(workdir);
    }
    let mut guard = platform.prepare_command(&mut command, config, stop_timeout)?;
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute {}", config.command))?;
    if let Err(error) = guard.attach(&child) {
        let _ = guard.force_stop();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let pid = child.id();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, "stdout", log.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, "stderr", log.clone());
    }
    {
        let mut state = runtime.lock().unwrap();
        state.state = ToolState::Running;
        state.pid = Some(pid);
        state.memory = MemoryValue::Idle;
        state.started_at_unix_ms = Some(unix_ms());
    }
    log.write("manager", &format!("process started, pid={pid}"));
    Ok(ManagedProcess { child, guard })
}

fn stop_remaining_process_tree(guard: &dyn ProcessGuard, log: &ToolLog, timeout: Duration) {
    if !guard.is_tree_running().unwrap_or(false) {
        return;
    }
    log.write(
        "manager",
        "root process exited; stopping remaining process tree",
    );
    if guard.request_graceful_stop().unwrap_or(false) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !guard.is_tree_running().unwrap_or(true) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    if guard.is_tree_running().unwrap_or(true)
        && let Err(error) = guard.force_stop()
    {
        log.write(
            "manager",
            &format!("failed to stop remaining process tree: {error:#}"),
        );
    }
}

fn spawn_reader(
    reader: impl std::io::Read + Send + 'static,
    channel: &'static str,
    log: Arc<ToolLog>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => log.write(
                    channel,
                    String::from_utf8_lossy(&bytes).trim_end_matches(['\r', '\n']),
                ),
                Err(error) => {
                    log.write("manager", &format!("{channel} read failed: {error}"));
                    break;
                }
            }
        }
    });
}

fn stop_process(
    process: &mut Option<ManagedProcess>,
    runtime: &Arc<Mutex<Runtime>>,
    log: &Arc<ToolLog>,
    timeout: Duration,
) {
    let Some(active) = process.as_mut() else {
        let mut state = runtime.lock().unwrap();
        state.state = ToolState::Stopped;
        state.pid = None;
        state.memory = MemoryValue::Idle;
        state.started_at_unix_ms = None;
        return;
    };
    runtime.lock().unwrap().state = ToolState::Stopping;
    let graceful = active.guard.request_graceful_stop().unwrap_or(false);
    if graceful {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let _ = active.child.try_wait();
            if !active.guard.is_tree_running().unwrap_or(true) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = active.child.try_wait();
    if active.guard.is_tree_running().unwrap_or(true) {
        log.write(
            "manager",
            "graceful stop timed out; forcing process tree stop",
        );
        if let Err(error) = active.guard.force_stop() {
            log.write("manager", &format!("force stop failed: {error:#}"));
        }
    }
    let _ = active.child.wait();
    *process = None;
    let mut state = runtime.lock().unwrap();
    state.state = ToolState::Stopped;
    state.pid = None;
    state.memory = MemoryValue::Idle;
    state.started_at_unix_ms = None;
    log.write("manager", "process stopped");
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn schedule_initial_manager_memory_refresh(
    manager_memory: Arc<Mutex<MemoryValue>>,
    platform: Arc<dyn PlatformAdapter>,
) {
    let _ = thread::Builder::new()
        .name("manager-initial-memory".into())
        .spawn(move || {
            let started = Instant::now();
            for delay in [Duration::from_secs(5), Duration::from_secs(10)] {
                thread::sleep((started + delay).saturating_duration_since(Instant::now()));
                *manager_memory.lock().unwrap() = MemoryValue::Pending;
                let value = platform
                    .process_tree_memory_usage(std::process::id())
                    .map(MemoryValue::Bytes)
                    .unwrap_or(MemoryValue::Unavailable);
                *manager_memory.lock().unwrap() = value;
            }
        });
}

fn schedule_tool_memory_refresh(
    pid: u32,
    runtime: Arc<Mutex<Runtime>>,
    platform: Arc<dyn PlatformAdapter>,
    delays: Vec<Duration>,
) {
    let _ = thread::Builder::new()
        .name("tool-memory-refresh".into())
        .spawn(move || {
            let started = Instant::now();
            for delay in delays {
                thread::sleep((started + delay).saturating_duration_since(Instant::now()));
                {
                    let mut current = runtime.lock().unwrap();
                    if current.pid != Some(pid) {
                        return;
                    }
                    current.memory = MemoryValue::Pending;
                }
                let value = platform
                    .process_tree_memory_usage(pid)
                    .map(MemoryValue::Bytes)
                    .unwrap_or(MemoryValue::Unavailable);
                let mut current = runtime.lock().unwrap();
                if current.pid != Some(pid) {
                    return;
                }
                current.memory = value;
            }
        });
}
