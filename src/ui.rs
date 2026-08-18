use crate::core::supervisor::{MemoryValue, Supervisor, ToolSnapshot, ToolState};
use anyhow::Result;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{sync::Arc, time::Duration};

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
    log_text: String,
}

pub fn run(supervisor: Arc<Supervisor>, show_window: bool) -> Result<()> {
    let window = AppWindow::new()?;
    let tray = KeeperTray::new()?;
    let chinese = is_chinese_locale();
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

    if show_window || !supervisor.minimize_to_tray() {
        supervisor.sample_memory();
        window.show()?;
    }
    let result = slint::run_event_loop();
    supervisor.shutdown();
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

fn wire_tray(tray: &KeeperTray, window: &AppWindow, supervisor: Arc<Supervisor>) {
    let weak = window.as_weak();
    let show_supervisor = supervisor.clone();
    let show = move || {
        if let Some(window) = weak.upgrade() {
            show_supervisor.sample_memory();
            let _ = window.show();
            window.window().request_redraw();
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
        return ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.trim().is_empty())
            .is_some_and(|value| {
                let locale = value.trim().to_ascii_lowercase();
                locale == "zh"
                    || locale.starts_with("zh_")
                    || locale.starts_with("zh-")
                    || locale.starts_with("zh.")
            });
    }

    #[allow(unreachable_code)]
    false
}
