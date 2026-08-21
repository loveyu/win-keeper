use super::{
    config::AppConfig,
    single_instance::SingleInstanceGuard,
    supervisor::{MemoryValue, Supervisor, ToolSnapshot, ToolState},
};
use crate::platform::{self, AppPaths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ElevatedCommand {
    Start,
    Stop,
    Restart,
    SampleMemory,
}

#[derive(Deserialize, Serialize)]
struct CommandRequest {
    command: ElevatedCommand,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ElevatedState {
    pub snapshot: ToolSnapshot,
    updated_unix_ms: u64,
}

impl ElevatedState {
    pub fn is_fresh(&self) -> bool {
        unix_ms().saturating_sub(self.updated_unix_ms) <= 8_000
    }
}

pub fn run_helper(manifest: &Path, state_directory: &Path) -> Result<()> {
    let helper_instance_directory = elevated_root(state_directory).join("helper-instance");
    let Some(_helper_guard) = SingleInstanceGuard::try_acquire(&helper_instance_directory)? else {
        return Ok(());
    };
    let mut config = AppConfig::load_or_create(manifest)
        .with_context(|| format!("failed to load elevated manifest {}", manifest.display()))?;
    config.tools.retain(|tool| tool.admin);
    anyhow::ensure!(
        !config.tools.is_empty(),
        "elevated manifest has no admin tools"
    );
    for tool in &mut config.tools {
        tool.admin = false;
        tool.auto_start = false;
    }

    let names: Vec<String> = config.tools.iter().map(|tool| tool.name.clone()).collect();
    let index_by_key: HashMap<String, usize> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (tool_key(name), index))
        .collect();
    let paths = AppPaths {
        config_file: manifest.to_path_buf(),
        log_directory: state_directory.join("logs"),
        state_directory: state_directory.to_path_buf(),
    };
    let supervisor = Arc::new(Supervisor::new(config, paths, platform::adapter())?);
    let control_directory = elevated_root(state_directory).join("control");
    fs::create_dir_all(&control_directory)?;
    recover_claimed_commands(&control_directory);
    let mut last_snapshots = Vec::new();
    let mut last_publish = Instant::now() - Duration::from_secs(3);

    loop {
        if let Ok(entries) = fs::read_dir(&control_directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("command") {
                    continue;
                }
                let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some((key, _)) = file_name.split_once('.') else {
                    continue;
                };
                let Some(&index) = index_by_key.get(key) else {
                    continue;
                };
                let claimed = path.with_extension(format!("processing-{}", std::process::id()));
                if fs::rename(&path, &claimed).is_err() {
                    continue;
                }
                let request = fs::read_to_string(&claimed)
                    .ok()
                    .and_then(|raw| toml::from_str::<CommandRequest>(&raw).ok());
                let _ = fs::remove_file(&claimed);
                match request.map(|request| request.command) {
                    Some(ElevatedCommand::Start) => supervisor.start(index),
                    Some(ElevatedCommand::Stop) => supervisor.stop(index),
                    Some(ElevatedCommand::Restart) => supervisor.restart(index),
                    Some(ElevatedCommand::SampleMemory) => supervisor.sample_memory(),
                    None => {}
                }
            }
        }

        let snapshots = supervisor.snapshots();
        if snapshots != last_snapshots || last_publish.elapsed() >= Duration::from_secs(2) {
            for snapshot in &snapshots {
                publish_state(state_directory, snapshot)?;
            }
            last_snapshots = snapshots;
            last_publish = Instant::now();
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn send_command(
    state_directory: &Path,
    tool_name: &str,
    command: ElevatedCommand,
) -> Result<()> {
    let directory = elevated_root(state_directory).join("control");
    fs::create_dir_all(&directory)?;
    let nonce = unix_ms()
        .saturating_mul(1_000)
        .saturating_add(COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed) % 1_000);
    let path = directory.join(format!("{}.{nonce}.command", tool_key(tool_name)));
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, toml::to_string(&CommandRequest { command })?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub(crate) fn read_state(state_directory: &Path, tool_name: &str) -> Result<Option<ElevatedState>> {
    let path = elevated_root(state_directory).join(format!("{}.state.toml", tool_key(tool_name)));
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(toml::from_str(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn publish_state(state_directory: &Path, snapshot: &ToolSnapshot) -> Result<()> {
    let directory = elevated_root(state_directory);
    fs::create_dir_all(&directory)?;
    let target = directory.join(format!("{}.state.toml", tool_key(&snapshot.name)));
    let temporary = directory.join(format!(
        "{}.state.{}.tmp",
        tool_key(&snapshot.name),
        std::process::id()
    ));
    let state = ElevatedState {
        snapshot: snapshot.clone(),
        updated_unix_ms: unix_ms(),
    };
    fs::write(&temporary, toml::to_string(&state)?)?;
    let _ = fs::remove_file(&target);
    fs::rename(temporary, target)?;
    Ok(())
}

fn recover_claimed_commands(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !extension.starts_with("processing-") {
            continue;
        }
        let command = path.with_extension("command");
        if !command.exists() {
            let _ = fs::rename(path, command);
        }
    }
}

fn elevated_root(state_directory: &Path) -> PathBuf {
    state_directory.join("elevated")
}

fn tool_key(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let hash = name
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{safe}_{hash:016x}")
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[allow(dead_code)]
fn _state_defaults(name: String) -> ToolSnapshot {
    ToolSnapshot {
        name,
        state: ToolState::Stopped,
        pid: None,
        memory: MemoryValue::Idle,
        restart_count: 0,
        started_at_unix_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_command_claimed_by_a_crashed_helper() {
        let directory = std::env::temp_dir().join(format!(
            "winkeeper-elevated-recovery-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let claimed = directory.join("tool.123.processing-999");
        fs::write(&claimed, "command = \"start\"\n").unwrap();

        recover_claimed_commands(&directory);

        assert!(!claimed.exists());
        assert!(directory.join("tool.123.command").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publishes_a_complete_command_atomically() {
        let state_directory = std::env::temp_dir().join(format!(
            "winkeeper-elevated-command-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&state_directory);
        send_command(&state_directory, "Admin Tool", ElevatedCommand::Restart).unwrap();
        let control = elevated_root(&state_directory).join("control");
        let files = fs::read_dir(&control)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();

        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].extension().and_then(|value| value.to_str()),
            Some("command")
        );
        let request: CommandRequest =
            toml::from_str(&fs::read_to_string(&files[0]).unwrap()).unwrap();
        assert_eq!(request.command, ElevatedCommand::Restart);
        fs::remove_dir_all(state_directory).unwrap();
    }
}
