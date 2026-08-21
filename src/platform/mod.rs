use crate::core::config::ToolConfig;
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub depth: usize,
    pub name: String,
    pub memory_bytes: Option<u64>,
}

pub(crate) struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub memory_bytes: Option<u64>,
}

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
    fn check_health(&mut self) -> Result<()> {
        Ok(())
    }
    fn request_graceful_stop(&self) -> Result<bool>;
    fn is_tree_running(&self) -> Result<bool>;
    fn force_stop(&self) -> Result<()>;
}

pub trait PlatformAdapter: Send + Sync {
    fn prepare_command(
        &self,
        command: &mut Command,
        config: &ToolConfig,
        stop_timeout: Duration,
    ) -> Result<Box<dyn ProcessGuard>>;
    fn memory_usage(&self, pid: u32) -> Result<u64>;
    fn process_tree(&self, root_pid: u32) -> Result<Vec<ProcessInfo>>;
    fn process_tree_memory_usage(&self, root_pid: u32) -> Result<u64> {
        self.process_tree(root_pid)?
            .into_iter()
            .filter_map(|process| process.memory_bytes)
            .reduce(u64::saturating_add)
            .context("process tree memory is unavailable")
    }
    fn open_path(&self, path: &Path) -> Result<()>;
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn build_process_tree(root_pid: u32, entries: Vec<ProcessEntry>) -> Vec<ProcessInfo> {
    build_process_tree_with_members(root_pid, entries, HashSet::new())
}

pub(crate) fn build_process_tree_with_members(
    root_pid: u32,
    entries: Vec<ProcessEntry>,
    mut member_pids: HashSet<u32>,
) -> Vec<ProcessInfo> {
    let mut by_pid = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in entries {
        children
            .entry(entry.parent_pid)
            .or_default()
            .push(entry.pid);
        by_pid.insert(entry.pid, entry);
    }
    for child_pids in children.values_mut() {
        child_pids.sort_unstable();
    }

    fn append(
        pid: u32,
        depth: usize,
        by_pid: &HashMap<u32, ProcessEntry>,
        children: &HashMap<u32, Vec<u32>>,
        visited: &mut HashSet<u32>,
        output: &mut Vec<ProcessInfo>,
    ) {
        if !visited.insert(pid) {
            return;
        }
        let Some(entry) = by_pid.get(&pid) else {
            return;
        };
        output.push(ProcessInfo {
            pid: entry.pid,
            parent_pid: entry.parent_pid,
            depth,
            name: entry.name.clone(),
            memory_bytes: entry.memory_bytes,
        });
        if let Some(child_pids) = children.get(&pid) {
            for &child_pid in child_pids {
                append(child_pid, depth + 1, by_pid, children, visited, output);
            }
        }
    }

    let mut output = Vec::new();
    let mut visited = HashSet::new();
    append(root_pid, 0, &by_pid, &children, &mut visited, &mut output);

    member_pids.insert(root_pid);
    let mut detached_roots = member_pids
        .iter()
        .copied()
        .filter(|pid| {
            !visited.contains(pid)
                && by_pid
                    .get(pid)
                    .is_some_and(|entry| !member_pids.contains(&entry.parent_pid))
        })
        .collect::<Vec<_>>();
    detached_roots.sort_unstable();
    for pid in detached_roots {
        append(pid, 1, &by_pid, &children, &mut visited, &mut output);
    }

    // Defensive fallback for malformed/cyclic snapshots: every selected member must be visible.
    let mut remaining = member_pids
        .into_iter()
        .filter(|pid| !visited.contains(pid))
        .collect::<Vec<_>>();
    remaining.sort_unstable();
    for pid in remaining {
        append(pid, 1, &by_pid, &children, &mut visited, &mut output);
    }
    output
}

#[cfg(target_os = "linux")]
pub fn adapter() -> Arc<dyn PlatformAdapter> {
    Arc::new(linux::LinuxAdapter::new())
}
#[cfg(windows)]
pub fn adapter() -> Arc<dyn PlatformAdapter> {
    Arc::new(windows::WindowsAdapter)
}

#[cfg(target_os = "linux")]
pub use linux::{
    configure_autostart, paths, prepare_shutdown_signals, run_process_watchdog, secure_paths,
    show_error, wait_for_shutdown_signal,
};
#[cfg(windows)]
pub use windows::{configure_autostart, paths, prepare_shutdown_signals, secure_paths, show_error};

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("WinKeeper supports Windows and Linux only");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_contains_only_root_and_descendants() {
        let entries = vec![
            ProcessEntry {
                pid: 10,
                parent_pid: 1,
                name: "root".into(),
                memory_bytes: Some(100),
            },
            ProcessEntry {
                pid: 11,
                parent_pid: 10,
                name: "child".into(),
                memory_bytes: Some(50),
            },
            ProcessEntry {
                pid: 12,
                parent_pid: 11,
                name: "grandchild".into(),
                memory_bytes: Some(25),
            },
            ProcessEntry {
                pid: 20,
                parent_pid: 1,
                name: "unrelated".into(),
                memory_bytes: Some(200),
            },
        ];
        let tree = build_process_tree(10, entries);
        assert_eq!(
            tree.iter()
                .map(|process| (process.pid, process.depth))
                .collect::<Vec<_>>(),
            vec![(10, 0), (11, 1), (12, 2)]
        );
    }

    #[test]
    fn process_tree_can_include_reparented_group_members() {
        let entries = vec![
            ProcessEntry {
                pid: 10,
                parent_pid: 1,
                name: "root".into(),
                memory_bytes: Some(100),
            },
            ProcessEntry {
                pid: 11,
                parent_pid: 10,
                name: "child".into(),
                memory_bytes: Some(50),
            },
            ProcessEntry {
                pid: 12,
                parent_pid: 1,
                name: "reparented".into(),
                memory_bytes: Some(25),
            },
            ProcessEntry {
                pid: 20,
                parent_pid: 1,
                name: "unrelated".into(),
                memory_bytes: Some(200),
            },
        ];
        let tree = build_process_tree_with_members(10, entries, HashSet::from([10, 11, 12]));
        assert_eq!(
            tree.iter()
                .map(|process| (process.pid, process.depth))
                .collect::<Vec<_>>(),
            vec![(10, 0), (11, 1), (12, 1)]
        );
    }
}
