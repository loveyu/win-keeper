use anyhow::{Context, Result};
use chrono::Local;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct ToolLog {
    path: PathBuf,
    line_capacity: usize,
    byte_capacity: usize,
    max_file_bytes: u64,
    max_line_bytes: usize,
    inner: Mutex<LogInner>,
}

struct LogInner {
    file: Option<File>,
    file_bytes: u64,
    lines: VecDeque<String>,
    buffered_bytes: usize,
    external_length: Option<u64>,
    write_failure_reported: bool,
}

impl ToolLog {
    pub fn migrate_legacy_logs<'a>(
        directory: &Path,
        tool_names: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        fs::create_dir_all(directory).context("failed to create log directory")?;
        let mut by_legacy_name: HashMap<String, Vec<&str>> = HashMap::new();
        by_legacy_name
            .entry("win-keeper-manager.log".into())
            .or_default()
            .push("<manager>");
        for name in tool_names {
            by_legacy_name
                .entry(legacy_tool_log_file_name(name))
                .or_default()
                .push(name);
        }
        for (legacy_name, names) in by_legacy_name {
            let [name] = names.as_slice() else {
                continue;
            };
            if *name == "<manager>" {
                continue;
            }
            let current_name = tool_log_file_name(name);
            if current_name == legacy_name {
                continue;
            }
            let legacy = directory.join(legacy_name);
            let current = directory.join(current_name);
            if legacy.exists()
                && !current.exists()
                && let Err(error) = fs::rename(&legacy, &current)
                && legacy.exists()
                && !current.exists()
            {
                eprintln!(
                    "WinKeeper could not migrate legacy tool log {} to {}: {error}",
                    legacy.display(),
                    current.display()
                );
            }
        }
        Ok(())
    }

    pub fn new(
        directory: &Path,
        name: &str,
        line_capacity: usize,
        byte_capacity: usize,
        max_file_bytes: u64,
        max_line_bytes: usize,
    ) -> Result<Self> {
        Self::open(
            directory,
            &tool_log_file_name(name),
            line_capacity,
            byte_capacity,
            max_file_bytes,
            max_line_bytes,
        )
    }

    pub fn new_manager(
        directory: &Path,
        line_capacity: usize,
        byte_capacity: usize,
        max_file_bytes: u64,
        max_line_bytes: usize,
    ) -> Result<Self> {
        Self::open(
            directory,
            "win-keeper-manager.log",
            line_capacity,
            byte_capacity,
            max_file_bytes,
            max_line_bytes,
        )
    }

    fn open(
        directory: &Path,
        file_name: &str,
        line_capacity: usize,
        byte_capacity: usize,
        max_file_bytes: u64,
        max_line_bytes: usize,
    ) -> Result<Self> {
        fs::create_dir_all(directory).context("failed to create log directory")?;
        secure_directory(directory)?;
        let path = directory.join(file_name);
        let file = open_private_log(&path)?;
        let file_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            path,
            line_capacity,
            byte_capacity,
            max_file_bytes,
            max_line_bytes,
            inner: Mutex::new(LogInner {
                file: Some(file),
                file_bytes,
                lines: VecDeque::new(),
                buffered_bytes: 0,
                external_length: None,
                write_failure_reported: false,
            }),
        })
    }

    pub fn write(&self, channel: &str, message: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for source_line in message.lines() {
            let (line, truncated) = truncate_utf8(source_line, self.max_line_bytes);
            let formatted = format!(
                "{} [{channel}] {line}{}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                if truncated { " …[truncated]" } else { "" }
            );
            if let Err(error) = self.write_formatted(&mut inner, &formatted)
                && !inner.write_failure_reported
            {
                eprintln!(
                    "WinKeeper failed to write {}: {error:#}",
                    self.path.display()
                );
                inner.write_failure_reported = true;
            }
            push_bounded(
                &mut inner,
                formatted,
                self.line_capacity,
                self.byte_capacity,
            );
        }
        if let Some(file) = inner.file.as_mut()
            && let Err(error) = file.flush()
            && !inner.write_failure_reported
        {
            eprintln!("WinKeeper failed to flush {}: {error}", self.path.display());
            inner.write_failure_reported = true;
        }
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
                        .take(self.line_capacity)
                        .map(str::to_owned)
                        .collect::<VecDeque<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    inner.buffered_bytes = inner.lines.iter().map(|line| line.len()).sum();
                    while inner.buffered_bytes > self.byte_capacity {
                        let Some(line) = inner.lines.pop_front() else {
                            break;
                        };
                        inner.buffered_bytes = inner.buffered_bytes.saturating_sub(line.len());
                    }
                    inner.external_length = Some(length);
                }
            }
        }
        inner.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    fn write_formatted(&self, inner: &mut LogInner, formatted: &str) -> Result<()> {
        if inner.file.is_none() {
            let file = open_private_log(&self.path)?;
            inner.file_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            inner.file = Some(file);
        }
        let bytes = u64::try_from(formatted.len().saturating_add(1)).unwrap_or(u64::MAX);
        if inner.file_bytes > 0 && inner.file_bytes.saturating_add(bytes) > self.max_file_bytes {
            self.rotate(inner)?;
        }
        let file = inner.file.as_mut().context("tool log is closed")?;
        writeln!(file, "{formatted}")?;
        inner.file_bytes = inner.file_bytes.saturating_add(bytes);
        inner.write_failure_reported = false;
        Ok(())
    }

    fn rotate(&self, inner: &mut LogInner) -> Result<()> {
        if let Some(mut file) = inner.file.take() {
            file.flush()?;
        }
        let backup = self.path.with_extension("log.1");
        let result = (|| {
            match fs::remove_file(&backup) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("failed to remove previous rotated log"),
            }
            match fs::rename(&self.path, &backup) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("failed to rotate tool log"),
            }
            Ok(())
        })();
        let file = open_private_log(&self.path)?;
        inner.file_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        inner.file = Some(file);
        if result.is_ok() {
            inner.external_length = None;
        }
        result
    }
}

fn push_bounded(inner: &mut LogInner, line: String, line_capacity: usize, byte_capacity: usize) {
    inner.buffered_bytes = inner.buffered_bytes.saturating_add(line.len());
    inner.lines.push_back(line);
    while inner.lines.len() > line_capacity || inner.buffered_bytes > byte_capacity {
        let Some(line) = inner.lines.pop_front() else {
            break;
        };
        inner.buffered_bytes = inner.buffered_bytes.saturating_sub(line.len());
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

fn tool_log_file_name(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        && !name.eq_ignore_ascii_case("win-keeper-manager")
    {
        return format!("{name}.log");
    }
    let mut safe = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    if safe.is_empty() {
        safe.push_str("tool");
    }
    let hash = name
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{safe}-{hash:016x}.log")
}

fn legacy_tool_log_file_name(name: &str) -> String {
    let safe_name = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{safe_name}.log")
}

fn open_private_log(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        options.mode(0o600);
        let file = options.open(path).context("failed to open tool log")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    options.open(path).context("failed to open tool log")
}

fn secure_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "winkeeper-log-{name}-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn distinct_names_cannot_share_a_log_file() {
        assert_ne!(tool_log_file_name("中文"), tool_log_file_name("??"));
        assert_ne!(
            tool_log_file_name("win-keeper-manager"),
            "win-keeper-manager.log"
        );
    }

    #[test]
    fn migrates_only_unambiguous_legacy_log_names() {
        let directory = test_directory("migration");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("Unique_Name.log"), "unique").unwrap();
        fs::write(directory.join("a_.log"), "ambiguous").unwrap();

        ToolLog::migrate_legacy_logs(&directory, ["Unique Name", "a?", "a!"]).unwrap();

        assert!(!directory.join("Unique_Name.log").exists());
        assert!(directory.join(tool_log_file_name("Unique Name")).exists());
        assert!(directory.join("a_.log").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounds_memory_lines_and_rotates_disk_log() {
        let directory = test_directory("bounds");
        let log = ToolLog::new(&directory, "worker", 10, 48, 120, 16).unwrap();
        log.write("stdout", "abcdefghijklmnopqrstuvwxyz");
        for index in 0..8 {
            log.write("stdout", &format!("message-{index}"));
        }

        assert!(log.snapshot().len() <= 48);
        assert!(log.path().metadata().unwrap().len() <= 120);
        assert!(log.path().with_extension("log.1").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                directory.metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                log.path().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
