use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub manager: ManagerConfig,
    pub tools: Vec<ToolConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagerConfig {
    pub start_with_system: bool,
    pub minimize_to_tray: bool,
    pub log_buffer_lines: usize,
    pub stop_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub workdir: Option<String>,
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
            start_with_system: true,
            minimize_to_tray: true,
            log_buffer_lines: 10_000,
            stop_timeout_ms: 5_000,
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
            }
            let default = Self::default();
            fs::write(path, toml::to_string_pretty(&default)?)
                .context("failed to create default configuration")?;
            return Ok(default);
        }
        let raw = fs::read_to_string(path).context("failed to read configuration")?;
        let config: Self = toml::from_str(&raw).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.manager.log_buffer_lines == 0 {
            bail!("manager.log_buffer_lines must be greater than zero");
        }
        let mut names = HashSet::new();
        for tool in &self.tools {
            if tool.name.trim().is_empty() {
                bail!("tool name cannot be empty");
            }
            if tool.command.trim().is_empty() {
                bail!("tool '{}' command cannot be empty", tool.name);
            }
            if !names.insert(tool.name.to_lowercase()) {
                bail!("duplicate tool name: {}", tool.name);
            }
            if tool.max_restart_count == 0 {
                bail!(
                    "tool '{}' max_restart_count must be greater than zero",
                    tool.name
                );
            }
        }
        Ok(())
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
}
