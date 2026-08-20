#![cfg_attr(windows, windows_subsystem = "windows")]

mod core;
mod platform;
mod ui;

use anyhow::{Context, Result};
use core::{
    config::{AppConfig, configured_chinese},
    single_instance::SingleInstanceGuard,
    supervisor::Supervisor,
};
use std::{path::PathBuf, sync::Arc};

fn main() {
    if let Err(error) = run() {
        let message = if ui::is_chinese_locale() {
            format!("WinKeeper 启动失败：{error:#}")
        } else {
            format!("WinKeeper startup failed: {error:#}")
        };
        eprintln!("{message}");
        platform::show_error("WinKeeper", &message);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    if args.elevated_helper {
        #[cfg(windows)]
        {
            let manifest = args
                .manifest
                .context("--elevated-helper requires --manifest")?;
            let state_directory = args
                .state_directory
                .context("--elevated-helper requires --state-dir")?;
            return core::elevated::run_helper(&manifest, &state_directory);
        }
        #[cfg(not(windows))]
        anyhow::bail!("--elevated-helper is supported on Windows only");
    }

    let config_override = args.config;
    let paths = platform::paths(config_override)?;
    let config = AppConfig::load_or_create(&paths.config_file)
        .with_context(|| format!("failed to load {}", paths.config_file.display()))?;

    if args.check_only {
        let chinese = config
            .manager
            .lang
            .as_deref()
            .and_then(configured_chinese)
            .unwrap_or_else(ui::is_chinese_locale);
        if chinese {
            println!("配置有效：{}", paths.config_file.display());
        } else {
            println!("configuration is valid: {}", paths.config_file.display());
        }
        return Ok(());
    }

    let Some(_instance_guard) = SingleInstanceGuard::try_acquire(&paths.state_directory)? else {
        if args.show_window || !args.autostart {
            SingleInstanceGuard::request_activation(&paths.state_directory)?;
        }
        let chinese = config
            .manager
            .lang
            .as_deref()
            .and_then(configured_chinese)
            .unwrap_or_else(ui::is_chinese_locale);
        if chinese {
            eprintln!("WinKeeper 已在当前用户会话中运行。");
        } else {
            eprintln!("WinKeeper is already running for the current user.");
        }
        return Ok(());
    };

    platform::configure_autostart(config.manager.start_with_system, &paths.config_file)?;
    platform::prepare_shutdown_signals()?;
    let platform = platform::adapter();
    let supervisor = Arc::new(Supervisor::new(config, paths, platform)?);
    ui::run(supervisor, args.show_window || !args.autostart)
}

#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    check_only: bool,
    show_window: bool,
    autostart: bool,
    elevated_helper: bool,
    manifest: Option<PathBuf>,
    state_directory: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut parsed = Args::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--config" => {
                parsed.config = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ));
            }
            "--check-config" => parsed.check_only = true,
            "--show" => parsed.show_window = true,
            "--autostart" => parsed.autostart = true,
            "--elevated-helper" => parsed.elevated_helper = true,
            "--manifest" => {
                parsed.manifest = Some(PathBuf::from(
                    args.next().context("--manifest requires a path")?,
                ));
            }
            "--state-dir" => {
                parsed.state_directory = Some(PathBuf::from(
                    args.next().context("--state-dir requires a path")?,
                ));
            }
            "--version" | "-V" => {
                println!("win-keeper {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "win-keeper [--config PATH] [--check-config] [--show] [--autostart] [--elevated-helper --manifest PATH --state-dir PATH]"
                );
                std::process::exit(0);
            }
            unknown => anyhow::bail!("unknown argument: {unknown}"),
        }
    }
    Ok(parsed)
}
