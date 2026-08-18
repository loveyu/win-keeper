use anyhow::{Context, Result};
use chrono::Local;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct ToolLog {
    path: PathBuf,
    capacity: usize,
    inner: Mutex<LogInner>,
}

struct LogInner {
    file: File,
    lines: VecDeque<String>,
}

impl ToolLog {
    pub fn new(directory: &Path, name: &str, capacity: usize) -> Result<Self> {
        fs::create_dir_all(directory).context("failed to create log directory")?;
        let safe_name: String = name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let path = directory.join(format!("{safe_name}.log"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("failed to open tool log")?;
        Ok(Self {
            path,
            capacity,
            inner: Mutex::new(LogInner {
                file,
                lines: VecDeque::new(),
            }),
        })
    }

    pub fn write(&self, channel: &str, message: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for line in message.lines() {
            let formatted = format!(
                "{} [{channel}] {line}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
            );
            let _ = writeln!(inner.file, "{formatted}");
            inner.lines.push_back(formatted);
            while inner.lines.len() > self.capacity {
                inner.lines.pop_front();
            }
        }
        let _ = inner.file.flush();
    }

    pub fn snapshot(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
