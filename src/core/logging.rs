use anyhow::{Context, Result};
use chrono::Local;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
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
    external_length: Option<u64>,
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
                external_length: None,
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

    pub fn snapshot_external(&self) -> String {
        let Ok(length) = fs::metadata(&self.path).map(|metadata| metadata.len()) else {
            return String::new();
        };
        let Ok(mut inner) = self.inner.lock() else {
            return String::new();
        };
        if inner.external_length != Some(length) {
            const MAX_TAIL_BYTES: u64 = 2 * 1024 * 1024;
            let start = length.saturating_sub(MAX_TAIL_BYTES);
            if let Ok(mut file) = File::open(&self.path) {
                let _ = file.seek(SeekFrom::Start(start));
                let mut bytes = Vec::with_capacity((length - start) as usize);
                if file.read_to_end(&mut bytes).is_ok() {
                    let text = String::from_utf8_lossy(&bytes);
                    let text = if start > 0 {
                        text.split_once('\n').map_or("", |(_, tail)| tail)
                    } else {
                        text.as_ref()
                    };
                    inner.lines = text
                        .lines()
                        .rev()
                        .take(self.capacity)
                        .map(str::to_owned)
                        .collect::<VecDeque<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    inner.external_length = Some(length);
                }
            }
        }
        inner.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
