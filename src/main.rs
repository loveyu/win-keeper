#![cfg_attr(windows, windows_subsystem = "windows")]

mod core;
mod platform;
mod ui;

use anyhow::{Context, Result};
use core::{config::AppConfig, supervisor::Supervisor};
use std::{path::PathBuf, sync::Arc};

fn main() {
    if let Err(error) = run() {
        eprintln!("WinKeeper startup failed: {error:#}");
        platform::show_error("WinKeeper", &format!("Startup failed:\n{error:#}"));
    }
}

fn run() -> Result<()> {
    let (config_override, check_only, show_window) = parse_args()?;
    let paths = platform::paths(config_override)?;
    let config = AppConfig::load_or_create(&paths.config_file)
        .with_context(|| format!("failed to load {}", paths.config_file.display()))?;

    if check_only {
        println!("configuration is valid: {}", paths.config_file.display());
        return Ok(());
    }

    platform::configure_autostart(config.manager.start_with_system, &paths.config_file)?;
    let platform = platform::adapter();
    let supervisor = Arc::new(Supervisor::new(config, paths, platform)?);
    ui::run(supervisor, show_window)
}

fn parse_args() -> Result<(Option<PathBuf>, bool, bool)> {
    let mut config = None;
    let mut check_only = false;
    let mut show_window = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--config" => {
                config = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ));
            }
            "--check-config" => check_only = true,
            "--show" => show_window = true,
            "--version" | "-V" => {
                println!("win-keeper {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("win-keeper [--config PATH] [--check-config] [--show]");
                std::process::exit(0);
            }
            unknown => anyhow::bail!("unknown argument: {unknown}"),
        }
    }
    Ok((config, check_only, show_window))
}
