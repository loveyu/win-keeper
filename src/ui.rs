use crate::core::supervisor::{MemoryValue, Supervisor, ToolSnapshot, ToolState};
use anyhow::Result;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

slint::include_modules!();

#[derive(Default)]
struct StateRefreshCache {
    initialized: bool,
    snapshots: Vec<ToolSnapshot>,
    manager_memory: Option<MemoryValue>,
}

#[derive(Default)]
struct LogRefreshCache {
    initialized: bool,
    selected_index: Option<i32>,
    selected_started_at: Option<u64>,
    log_text: String,
}

pub fn run(supervisor: Arc<Supervisor>, show_window: bool) -> Result<()> {
    let tray = KeeperTray::new()?;
    #[cfg(unix)]
    slint::set_xdg_app_id("win-keeper")?;
    let window = AppWindow::new()?;
    #[cfg(windows)]
    {
        window.set_ui_font_family("Microsoft YaHei UI".into());
        window.set_code_font_family("Consolas".into());
    }
    #[cfg(not(windows))]
    {
        window.set_ui_font_family("Noto Sans CJK SC".into());
        window.set_code_font_family("Noto Sans Mono CJK SC".into());
    }
    let chinese = supervisor
        .configured_chinese()
        .unwrap_or_else(is_chinese_locale);
    window.set_chinese(chinese);
    tray.set_chinese(chinese);
    window.set_config_path(supervisor.config_path().display().to_string().into());

    wire_window(&window, supervisor.clone());
    wire_tray(&tray, &window, supervisor.clone());
    let mut state_cache = StateRefreshCache::default();
    refresh_state(&window, &supervisor, chinese, &mut state_cache);
    let mut log_cache = LogRefreshCache::default();
    refresh_log(&window, &supervisor, chinese, &mut log_cache);

    let weak_window = window.as_weak();
    let state_supervisor = supervisor.clone();
    let state_timer = Timer::default();
    state_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(window) = weak_window.upgrade() {
            refresh_state(&window, &state_supervisor, chinese, &mut state_cache);
        }
    });

    let weak_window = window.as_weak();
    let log_supervisor = supervisor.clone();
    let log_timer = Timer::default();
    log_timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        if let Some(window) = weak_window.upgrade()
            && window.window().is_visible()
        {
            refresh_log(&window, &log_supervisor, chinese, &mut log_cache);
        }
    });

    let weak_window = window.as_weak();
    let activation_supervisor = supervisor.clone();
    let activation_timer = Timer::default();
    activation_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        if activation_supervisor.take_activation_request()
            && let Some(window) = weak_window.upgrade()
        {
            let _ = show_manager(&window, &activation_supervisor);
        }
    });

    if show_window || !supervisor.minimize_to_tray() {
        show_manager(&window, &supervisor)?;
    }
    #[cfg(unix)]
    std::thread::Builder::new()
        .name("shutdown-signal".into())
        .spawn(|| {
            if crate::platform::wait_for_shutdown_signal().is_ok() {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
        })?;
    // Keep tray-only startup alive even if the backend has not exposed the icon yet.
    let result = slint::run_event_loop_until_quit();
    supervisor.shutdown();
    drop(activation_timer);
    drop(log_timer);
    drop(state_timer);
    drop(tray);
    drop(window);
    result.map_err(Into::into)
}

fn wire_window(window: &AppWindow, supervisor: Arc<Supervisor>) {
    window
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    let weak = window.as_weak();
    let action = supervisor.clone();
    window.on_start_tool(move |index| {
        if index >= 0 {
            action.start(index as usize);
        }
    });
    let action = supervisor.clone();
    window.on_stop_tool(move |index| {
        if index >= 0 {
            action.stop(index as usize);
        }
    });
    let action = supervisor.clone();
    window.on_restart_tool(move |index| {
        if index >= 0 {
            action.restart(index as usize);
        }
    });
    let action = supervisor.clone();
    window.on_start_all(move || action.start_all());
    let action = supervisor.clone();
    window.on_stop_all(move || action.stop_all());
    let action = supervisor.clone();
    window.on_restart_all(move || action.restart_all());
    let refresh_weak = window.as_weak();
    let action = supervisor.clone();
    window.on_refresh_memory(move || {
        if let Some(window) = refresh_weak.upgrade() {
            window.set_memory_refreshing(true);
        }
        action.sample_memory();
    });
    let action = supervisor.clone();
    window.on_open_log_file(move |index| {
        if index >= 0 {
            let _ = action.open_log(index as usize);
        }
    });
    let action = supervisor.clone();
    window.on_open_workdir(move |index| {
        if index >= 0 {
            let _ = action.open_workdir(index as usize);
        }
    });
    let action = supervisor.clone();
    window.on_open_config(move || {
        let _ = action.open_config();
    });
    let inspect_weak = window.as_weak();
    let action = supervisor.clone();
    let inspect_generation = Arc::new(AtomicU64::new(0));
    window.on_inspect_process(move |index| {
        if index < 0 {
            return;
        }
        let Some(window) = inspect_weak.upgrade() else {
            return;
        };
        let chinese = window.get_chinese();
        window.set_process_tree_title(if chinese {
            "进程树".into()
        } else {
            "Process Tree".into()
        });
        window.set_process_tree_text("".into());
        window.set_process_tree_loading(true);
        window.set_process_tree_visible(true);

        let generation = inspect_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let current_generation = inspect_generation.clone();
        let result_weak = window.as_weak();
        let action = action.clone();
        std::thread::spawn(move || {
            let result = action.process_tree(index as usize);
            let _ = slint::invoke_from_event_loop(move || {
                if current_generation.load(Ordering::Relaxed) != generation {
                    return;
                }
                let Some(window) = result_weak.upgrade() else {
                    return;
                };
                match result {
                    Ok((name, processes)) => {
                        let title = if chinese {
                            format!("{name} — 进程树")
                        } else {
                            format!("{name} — Process Tree")
                        };
                        window.set_process_tree_title(title.into());
                        window
                            .set_process_tree_text(format_process_tree(&processes, chinese).into());
                    }
                    Err(error) => {
                        window.set_process_tree_title(if chinese {
                            "无法读取进程树".into()
                        } else {
                            "Process tree unavailable".into()
                        });
                        window.set_process_tree_text(format!("{error:#}").into());
                    }
                }
                window.set_process_tree_loading(false);
            });
        });
    });
    window.on_hide_requested(move || {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
    });
    let action = supervisor;
    window.on_exit_requested(move || {
        action.shutdown();
        let _ = slint::quit_event_loop();
    });
}

fn format_process_tree(processes: &[crate::platform::ProcessInfo], chinese: bool) -> String {
    if processes.is_empty() {
        return tr(chinese, "The process has exited.", "进程已退出。").into();
    }
    let total_memory: u64 = processes
        .iter()
        .filter_map(|process| process.memory_bytes)
        .sum();
    let metric = if cfg!(windows) { "Private" } else { "PSS" };
    let mut output = if chinese {
        format!(
            "进程数：{}    {metric} 合计：{}\n\nPID     PPID    {metric:<10}  进程\n",
            processes.len(),
            format_memory(MemoryValue::Bytes(total_memory), true)
        )
    } else {
        format!(
            "Processes: {}    Total {metric}: {}\n\nPID     PPID    {metric:<10}  PROCESS\n",
            processes.len(),
            format_memory(MemoryValue::Bytes(total_memory), false)
        )
    };
    for process in processes {
        let branch = if process.depth == 0 {
            String::new()
        } else {
            format!("{}└─ ", "   ".repeat(process.depth - 1))
        };
        let memory = process
            .memory_bytes
            .map(|bytes| format_memory(MemoryValue::Bytes(bytes), chinese))
            .unwrap_or_else(|| tr(chinese, "N/A", "不可用").into());
        output.push_str(&format!(
            "{:<7} {:<7} {:>10}    {branch}{}\n",
            process.pid, process.parent_pid, memory, process.name
        ));
    }
    output
}

fn wire_tray(tray: &KeeperTray, window: &AppWindow, supervisor: Arc<Supervisor>) {
    let weak = window.as_weak();
    let show_supervisor = supervisor.clone();
    let show = move || {
        if let Some(window) = weak.upgrade() {
            let _ = show_manager(&window, &show_supervisor);
        }
    };
    tray.on_open_manager(show);
    let action = supervisor.clone();
    tray.on_start_all(move || action.start_all());
    let action = supervisor.clone();
    tray.on_stop_all(move || action.stop_all());
    let action = supervisor.clone();
    tray.on_restart_all(move || action.restart_all());
    let action = supervisor.clone();
    tray.on_open_config(move || {
        let _ = action.open_config();
    });
    let action = supervisor;
    tray.on_exit_requested(move || {
        action.shutdown();
        let _ = slint::quit_event_loop();
    });
}

fn show_manager(window: &AppWindow, supervisor: &Supervisor) -> Result<(), slint::PlatformError> {
    supervisor.sample_memory();
    window.show()?;
    window.window().request_redraw();
    Ok(())
}

fn refresh_state(
    window: &AppWindow,
    supervisor: &Supervisor,
    chinese: bool,
    cache: &mut StateRefreshCache,
) {
    let snapshots = supervisor.snapshots();
    let manager_memory = supervisor.manager_memory();

    if !cache.initialized
        || cache.snapshots != snapshots
        || cache.manager_memory != Some(manager_memory)
    {
        let mut rows: Vec<ToolRow> = snapshots
            .iter()
            .map(|snapshot| row_from_snapshot(snapshot, chinese))
            .collect();
        rows.push(ToolRow {
            name: tr(chinese, "Tool Manager", "工具管理器").into(),
            status: tr(chinese, "Running", "运行中").into(),
            memory: format_memory(manager_memory, chinese).into(),
            accent: slint::Color::from_rgb_u8(55, 190, 142),
            pid: std::process::id() as i32,
            restart_count: 0,
            manager: true,
        });
        window.set_tools(ModelRc::new(VecModel::from(rows)));
        window.set_memory_refreshing(
            manager_memory == MemoryValue::Pending
                || snapshots
                    .iter()
                    .any(|tool| tool.memory == MemoryValue::Pending),
        );

        let running = snapshots
            .iter()
            .filter(|tool| tool.state == ToolState::Running)
            .count();
        let crashed = snapshots
            .iter()
            .filter(|tool| tool.state == ToolState::Crashed)
            .count();
        let summary = if chinese {
            format!(
                "{running} 个运行中  /  共 {} 个  /  {crashed} 个需关注",
                snapshots.len()
            )
        } else {
            format!(
                "{running} running  /  {} total  /  {crashed} attention",
                snapshots.len()
            )
        };
        window.set_summary(summary.into());
        cache.snapshots.clone_from(&snapshots);
        cache.manager_memory = Some(manager_memory);
    }
    cache.initialized = true;
}

fn refresh_log(
    window: &AppWindow,
    supervisor: &Supervisor,
    chinese: bool,
    cache: &mut LogRefreshCache,
) {
    let selected = window.get_selected_index();
    let started_at = if selected >= 0 {
        supervisor
            .snapshots()
            .get(selected as usize)
            .and_then(|snapshot| snapshot.started_at_unix_ms)
    } else {
        None
    };
    if !cache.initialized
        || cache.selected_index != Some(selected)
        || cache.selected_started_at != started_at
    {
        window.set_output_timing(
            started_at
                .map(|started_at| format_tool_timing(started_at, chinese))
                .unwrap_or_default()
                .into(),
        );
        cache.selected_started_at = started_at;
    }
    let log_text = if selected >= 0 && (selected as usize) < supervisor.tool_count() {
        supervisor.log_snapshot(selected as usize)
    } else {
        tr(
            chinese,
            "Select a tool to inspect its live output.",
            "选择一个工具以查看实时输出。",
        )
        .into()
    };
    if !cache.initialized || cache.selected_index != Some(selected) || cache.log_text != log_text {
        window.set_log_text(log_text.clone().into());
        cache.selected_index = Some(selected);
        cache.log_text = log_text;
    }
    cache.initialized = true;
}

fn format_tool_timing(started_at_unix_ms: u64, chinese: bool) -> String {
    let started_system = std::time::UNIX_EPOCH + Duration::from_millis(started_at_unix_ms);
    let started_local: chrono::DateTime<chrono::Local> = started_system.into();
    let elapsed_seconds = std::time::SystemTime::now()
        .duration_since(started_system)
        .unwrap_or_default()
        .as_secs();
    let days = elapsed_seconds / 86_400;
    let hours = elapsed_seconds % 86_400 / 3_600;
    let minutes = elapsed_seconds % 3_600 / 60;
    let elapsed = if chinese {
        if days > 0 {
            format!("{days}天 {hours}小时 {minutes}分钟")
        } else if hours > 0 {
            format!("{hours}小时 {minutes}分钟")
        } else {
            format!("{minutes}分钟")
        }
    } else if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    };
    if chinese {
        format!(
            "启动：{}  ·  运行：{elapsed}",
            started_local.format("%Y-%m-%d %H:%M:%S")
        )
    } else {
        format!(
            "Started: {}  ·  Uptime: {elapsed}",
            started_local.format("%Y-%m-%d %H:%M:%S")
        )
    }
}

fn row_from_snapshot(snapshot: &ToolSnapshot, chinese: bool) -> ToolRow {
    let (status, accent) = match snapshot.state {
        ToolState::Stopped => (
            tr(chinese, "Stopped", "已停止"),
            slint::Color::from_rgb_u8(116, 131, 145),
        ),
        ToolState::Starting => (
            tr(chinese, "Starting", "启动中"),
            slint::Color::from_rgb_u8(241, 167, 58),
        ),
        ToolState::Running => (
            tr(chinese, "Running", "运行中"),
            slint::Color::from_rgb_u8(55, 190, 142),
        ),
        ToolState::Stopping => (
            tr(chinese, "Stopping", "停止中"),
            slint::Color::from_rgb_u8(241, 167, 58),
        ),
        ToolState::Restarting => (
            tr(chinese, "Restarting", "重启中"),
            slint::Color::from_rgb_u8(241, 167, 58),
        ),
        ToolState::Crashed => (
            tr(chinese, "Crashed", "已崩溃"),
            slint::Color::from_rgb_u8(239, 93, 89),
        ),
    };
    ToolRow {
        name: snapshot.name.clone().into(),
        status: status.into(),
        memory: format_memory(snapshot.memory, chinese).into(),
        accent,
        pid: snapshot.pid.unwrap_or_default() as i32,
        restart_count: snapshot.restart_count as i32,
        manager: false,
    }
}

fn format_memory(memory: MemoryValue, chinese: bool) -> String {
    match memory {
        MemoryValue::Idle => "-".into(),
        MemoryValue::Pending => tr(chinese, "Querying...", "查询中...").into(),
        MemoryValue::Unavailable => tr(chinese, "N/A", "不可用").into(),
        MemoryValue::Bytes(bytes) => {
            const KB: f64 = 1024.0;
            const MB: f64 = KB * 1024.0;
            const GB: f64 = MB * 1024.0;
            let bytes = bytes as f64;
            if bytes >= GB {
                format!("{:.2} GB", bytes / GB)
            } else if bytes >= MB {
                format!("{:.1} MB", bytes / MB)
            } else if bytes >= KB {
                format!("{:.1} KB", bytes / KB)
            } else {
                format!("{} B", bytes as u64)
            }
        }
    }
}

fn tr<'a>(chinese: bool, english: &'a str, simplified_chinese: &'a str) -> &'a str {
    if chinese { simplified_chinese } else { english }
}

pub fn is_chinese_locale() -> bool {
    #[cfg(windows)]
    {
        const PRIMARY_LANGUAGE_MASK: u16 = 0x03ff;
        const CHINESE_PRIMARY_LANGUAGE: u16 = 0x0004;
        let language = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        return language & PRIMARY_LANGUAGE_MASK == CHINESE_PRIMARY_LANGUAGE;
    }

    #[cfg(unix)]
    {
        return ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .any(|value| value.split(':').any(locale_is_chinese));
    }

    #[allow(unreachable_code)]
    false
}

#[cfg(any(unix, test))]
fn locale_is_chinese(value: &str) -> bool {
    let locale = value.trim().to_ascii_lowercase();
    locale == "zh"
        || locale.starts_with("zh_")
        || locale.starts_with("zh-")
        || locale.starts_with("zh.")
}

#[cfg(test)]
mod tests {
    use super::locale_is_chinese;

    #[test]
    fn recognizes_chinese_locale_variants() {
        for locale in ["zh", "zh_CN", "zh-CN", "zh.UTF-8"] {
            assert!(locale_is_chinese(locale), "{locale}");
        }
        assert!(!locale_is_chinese("en_US.UTF-8"));
        assert!(!locale_is_chinese("C.UTF-8"));
    }
}
