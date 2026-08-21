use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path};

const MAX_LOG_BUFFER_LINES: usize = 100_000;
const MAX_LOG_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const MAX_LOG_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LOG_LINE_BYTES: usize = 1024 * 1024;
const MIN_STOP_TIMEOUT_MS: u64 = 100;
const MAX_STOP_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const MAX_RESTART_DELAY_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_RESTART_WINDOW_SECONDS: u64 = 24 * 60 * 60;
const MAX_RESTART_COUNT: usize = 1_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub manager: ManagerConfig,
    pub tools: Vec<ToolConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagerConfig {
    pub lang: Option<String>,
    pub start_with_system: bool,
    pub minimize_to_tray: bool,
    pub log_buffer_lines: usize,
    pub log_buffer_bytes: usize,
    pub log_file_max_bytes: u64,
    pub log_line_max_bytes: usize,
    pub stop_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub workdir: Option<String>,
    pub graceful_stop_command: Option<String>,
    pub graceful_stop_args: Vec<String>,
    pub admin: bool,
    pub auto_start: bool,
    pub auto_restart: bool,
    pub restart_delay_ms: u64,
    pub max_restart_count: usize,
    pub restart_window_seconds: u64,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            lang: None,
            start_with_system: true,
            minimize_to_tray: true,
            log_buffer_lines: 10_000,
            log_buffer_bytes: 2 * 1024 * 1024,
            log_file_max_bytes: 10 * 1024 * 1024,
            log_line_max_bytes: 64 * 1024,
            stop_timeout_ms: 30_000,
        }
    }
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            workdir: None,
            graceful_stop_command: None,
            graceful_stop_args: Vec::new(),
            admin: false,
            auto_start: false,
            auto_restart: true,
            restart_delay_ms: 3_000,
            max_restart_count: 5,
            restart_window_seconds: 60,
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).context("failed to create configuration directory")?;
                secure_private_directory(parent)
                    .context("failed to secure configuration directory")?;
            }
            let default = Self::default();
            write_private_file(path, toml::to_string_pretty(&default)?.as_bytes())
                .context("failed to create default configuration")?;
            return Ok(default);
        }
        secure_private_file(path).context("failed to secure configuration file")?;
        let raw = fs::read_to_string(path).context("failed to read configuration")?;
        let config: Self = toml::from_str(&raw).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self
            .manager
            .lang
            .as_deref()
            .is_some_and(|lang| configured_chinese(lang).is_none())
        {
            bail!("manager.lang must be an English or Chinese locale, such as 'en' or 'zh_CN'");
        }
        if !(1..=MAX_LOG_BUFFER_LINES).contains(&self.manager.log_buffer_lines) {
            bail!("manager.log_buffer_lines must be between 1 and {MAX_LOG_BUFFER_LINES}");
        }
        if !(1..=MAX_LOG_BUFFER_BYTES).contains(&self.manager.log_buffer_bytes) {
            bail!("manager.log_buffer_bytes must be between 1 and {MAX_LOG_BUFFER_BYTES}");
        }
        if !(1..=MAX_LOG_FILE_BYTES).contains(&self.manager.log_file_max_bytes) {
            bail!("manager.log_file_max_bytes must be between 1 and {MAX_LOG_FILE_BYTES}");
        }
        if !(1..=MAX_LOG_LINE_BYTES).contains(&self.manager.log_line_max_bytes) {
            bail!("manager.log_line_max_bytes must be between 1 and {MAX_LOG_LINE_BYTES}");
        }
        if self.manager.log_file_max_bytes
            < u64::try_from(self.manager.log_line_max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(256)
        {
            bail!("manager.log_file_max_bytes must be at least log_line_max_bytes + 256");
        }
        if !(MIN_STOP_TIMEOUT_MS..=MAX_STOP_TIMEOUT_MS).contains(&self.manager.stop_timeout_ms) {
            bail!(
                "manager.stop_timeout_ms must be between {MIN_STOP_TIMEOUT_MS} and {MAX_STOP_TIMEOUT_MS}"
            );
        }
        let mut names = HashSet::new();
        for tool in &self.tools {
            if tool.name.trim().is_empty() {
                bail!("tool name cannot be empty");
            }
            if tool.command.trim().is_empty() {
                bail!("tool '{}' command cannot be empty", tool.name);
            }
            if tool
                .graceful_stop_command
                .as_deref()
                .is_some_and(|command| command.trim().is_empty())
            {
                bail!("tool '{}' graceful_stop_command cannot be empty", tool.name);
            }
            if tool.graceful_stop_command.is_none() && !tool.graceful_stop_args.is_empty() {
                bail!(
                    "tool '{}' graceful_stop_args requires graceful_stop_command",
                    tool.name
                );
            }
            if !names.insert(tool.name.to_lowercase()) {
                bail!("duplicate tool name: {}", tool.name);
            }
            if !(1..=MAX_RESTART_COUNT).contains(&tool.max_restart_count) {
                bail!(
                    "tool '{}' max_restart_count must be between 1 and {MAX_RESTART_COUNT}",
                    tool.name,
                );
            }
            if tool.restart_delay_ms > MAX_RESTART_DELAY_MS {
                bail!(
                    "tool '{}' restart_delay_ms cannot exceed {MAX_RESTART_DELAY_MS}",
                    tool.name,
                );
            }
            if !(1..=MAX_RESTART_WINDOW_SECONDS).contains(&tool.restart_window_seconds) {
                bail!(
                    "tool '{}' restart_window_seconds must be between 1 and {MAX_RESTART_WINDOW_SECONDS}",
                    tool.name,
                );
            }
        }
        Ok(())
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)?;
        Ok(())
    }
}

fn secure_private_file(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn secure_private_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn configured_chinese(lang: &str) -> Option<bool> {
    match lang
        .trim()
        .split(['_', '-', '.'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "zh" => Some(true),
        "en" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_configuration_is_valid() {
        let raw = include_str!("../../config.example.toml");
        let config: AppConfig = toml::from_str(raw).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn configured_language_accepts_common_locales() {
        assert_eq!(configured_chinese("zh_CN"), Some(true));
        assert_eq!(configured_chinese("zh-CN"), Some(true));
        assert_eq!(configured_chinese("en_US"), Some(false));
        assert_eq!(configured_chinese("en-US"), Some(false));
        assert_eq!(configured_chinese("C.UTF-8"), None);
    }

    #[test]
    fn rejects_unbounded_resource_and_restart_values() {
        let mut config = AppConfig::default();
        config.manager.log_buffer_bytes = 0;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.manager.stop_timeout_ms = u64::MAX;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.tools.push(ToolConfig {
            name: "worker".into(),
            command: "worker".into(),
            restart_window_seconds: 0,
            ..ToolConfig::default()
        });
        assert!(config.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_configuration_paths() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "winkeeper-config-permissions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("config.toml");

        AppConfig::load_or_create(&path).unwrap();

        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(directory).unwrap();
    }
}
