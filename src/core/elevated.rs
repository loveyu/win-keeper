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
    id: u64,
    command: ElevatedCommand,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ElevatedState {
    pub snapshot: ToolSnapshot,
    #[serde(default)]
    pub acknowledged_command_id: u64,
    #[serde(default)]
    pub helper_started_unix_ms: u64,
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
    let helper_started_unix_ms = next_command_id();
    let mut acknowledged = names
        .iter()
        .map(|name| {
            let id = read_state(state_directory, name)
                .ok()
                .flatten()
                .map_or(0, |state| state.acknowledged_command_id);
            (tool_key(name), id)
        })
        .collect::<HashMap<_, _>>();
    let mut last_snapshots = Vec::new();
    let mut last_publish = Instant::now() - Duration::from_secs(3);

    loop {
        let mut acknowledged_changed = false;
        let mut paths = fs::read_dir(&control_directory)
            .context("failed to enumerate elevated command queue")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("command"))
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| command_file_id(path).unwrap_or(u64::MAX));
        for path in paths {
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some((key, _)) = file_name.split_once('.') else {
                continue;
            };
            let claimed = path.with_extension(format!("processing-{}", std::process::id()));
            if fs::rename(&path, &claimed).is_err() {
                continue;
            }
            let request = fs::read_to_string(&claimed)
                .ok()
                .and_then(|raw| toml::from_str::<CommandRequest>(&raw).ok());
            let Some(request) = request else {
                reject_command(&control_directory, &claimed, "invalid");
                continue;
            };
            if command_file_id(&path) != Some(request.id) {
                reject_command(&control_directory, &claimed, "id-mismatch");
                continue;
            }
            let Some(&index) = index_by_key.get(key) else {
                reject_command(&control_directory, &claimed, "unknown-tool");
                continue;
            };
            let last_acknowledged = acknowledged.get(key).copied().unwrap_or(0);
            if request.id <= last_acknowledged {
                let _ = fs::remove_file(&claimed);
                continue;
            }
            match request.command {
                ElevatedCommand::Start => supervisor.start(index),
                ElevatedCommand::Stop => supervisor.stop(index),
                ElevatedCommand::Restart => supervisor.restart(index),
                ElevatedCommand::SampleMemory => supervisor.sample_memory(),
            }
            acknowledged.insert(key.to_owned(), request.id);
            acknowledged_changed = true;
            let _ = fs::remove_file(&claimed);
        }

        let snapshots = supervisor.snapshots();
        if acknowledged_changed
            || snapshots != last_snapshots
            || last_publish.elapsed() >= Duration::from_secs(2)
        {
            for snapshot in &snapshots {
                publish_state(
                    state_directory,
                    snapshot,
                    acknowledged
                        .get(&tool_key(&snapshot.name))
                        .copied()
                        .unwrap_or(0),
                    helper_started_unix_ms,
                )?;
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
) -> Result<u64> {
    let minimum = read_state(state_directory, tool_name)
        .ok()
        .flatten()
        .map_or(0, |state| state.acknowledged_command_id.saturating_add(1));
    let id = next_command_id().max(minimum);
    write_command(state_directory, tool_name, id, command)?;
    Ok(id)
}

pub(crate) fn resend_command(
    state_directory: &Path,
    tool_name: &str,
    id: u64,
    command: ElevatedCommand,
) -> Result<()> {
    write_command(state_directory, tool_name, id, command)
}

fn write_command(
    state_directory: &Path,
    tool_name: &str,
    id: u64,
    command: ElevatedCommand,
) -> Result<()> {
    let directory = elevated_root(state_directory).join("control");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.{id}.command", tool_key(tool_name)));
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}-{id}", std::process::id()));
    fs::write(
        &temporary,
        toml::to_string(&CommandRequest { id, command })?,
    )?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        if !path.exists() {
            return Err(error.into());
        }
    }
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

fn publish_state(
    state_directory: &Path,
    snapshot: &ToolSnapshot,
    acknowledged_command_id: u64,
    helper_started_unix_ms: u64,
) -> Result<()> {
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
        acknowledged_command_id,
        helper_started_unix_ms,
        updated_unix_ms: unix_ms(),
    };
    fs::write(&temporary, toml::to_string(&state)?)?;
    let _ = fs::remove_file(&target);
    fs::rename(temporary, target)?;
    Ok(())
}

fn command_file_id(path: &Path) -> Option<u64> {
    path.file_name()?.to_str()?.split('.').nth(1)?.parse().ok()
}

fn reject_command(control_directory: &Path, claimed: &Path, reason: &str) {
    let rejected = control_directory.join("rejected");
    if fs::create_dir_all(&rejected).is_err() {
        let _ = fs::remove_file(claimed);
        return;
    }
    let file_name = claimed
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-command");
    let target = rejected.join(format!("{file_name}.{reason}"));
    if fs::rename(claimed, target).is_err() {
        let _ = fs::remove_file(claimed);
    }
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
        } else {
            let _ = fs::remove_file(path);
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

fn next_command_id() -> u64 {
    unix_ms()
        .saturating_mul(1_000)
        .saturating_add(COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed) % 1_000)
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
        let id = send_command(&state_directory, "Admin Tool", ElevatedCommand::Restart).unwrap();
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
        assert_eq!(request.id, id);
        assert_eq!(command_file_id(&files[0]), Some(id));
        assert_eq!(request.command, ElevatedCommand::Restart);
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[test]
    fn command_files_sort_by_monotonic_id() {
        let key = tool_key("Admin Tool");
        let mut paths = [
            PathBuf::from(format!("{key}.30.command")),
            PathBuf::from(format!("{key}.2.command")),
            PathBuf::from(format!("{key}.11.command")),
        ];
        paths.sort_by_key(|path| command_file_id(path).unwrap());

        assert_eq!(
            paths
                .iter()
                .map(|path| command_file_id(path).unwrap())
                .collect::<Vec<_>>(),
            vec![2, 11, 30]
        );
    }

    #[test]
    fn legacy_state_defaults_protocol_metadata() {
        let raw = r#"
updated_unix_ms = 1

[snapshot]
name = "Admin Tool"
state = "stopped"
memory = { kind = "idle" }
restart_count = 0
"#;
        let state: ElevatedState = toml::from_str(raw).unwrap();

        assert_eq!(state.acknowledged_command_id, 0);
        assert_eq!(state.helper_started_unix_ms, 0);
    }
}
