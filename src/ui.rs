use crate::core::supervisor::{MemoryValue, Supervisor, ToolSnapshot, ToolState};
use anyhow::Result;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{sync::Arc, time::Duration};

slint::include_modules!();

pub fn run(supervisor: Arc<Supervisor>, show_window: bool) -> Result<()> {
    let window = AppWindow::new()?;
    let tray = KeeperTray::new()?;
    window.set_config_path(supervisor.config_path().display().to_string().into());

    wire_window(&window, supervisor.clone());
    wire_tray(&tray, &window, supervisor.clone());
    refresh(&window, &supervisor);

    let weak_window = window.as_weak();
    let refresh_supervisor = supervisor.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        if let Some(window) = weak_window.upgrade() {
            refresh(&window, &refresh_supervisor);
        }
    });

    if show_window || !supervisor.minimize_to_tray() {
        supervisor.sample_memory();
        window.show()?;
    }
    let result = slint::run_event_loop();
    supervisor.shutdown();
    drop(timer);
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
    let action = supervisor.clone();
    window.on_refresh_memory(move || action.sample_memory());
    let action = supervisor.clone();
    window.on_open_config(move || {
        let _ = action.open_config();
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

fn refresh(window: &AppWindow, supervisor: &Supervisor) {
    let snapshots = supervisor.snapshots();
    let selected = window.get_selected_index();
    let mut rows: Vec<ToolRow> = snapshots.iter().map(row_from_snapshot).collect();
    rows.push(ToolRow {
        name: "Tool Manager".into(),
        status: "Running".into(),
        memory: format_memory(supervisor.manager_memory()).into(),
        accent: slint::Color::from_rgb_u8(55, 190, 142),
        pid: std::process::id() as i32,
        manager: true,
    });
    window.set_tools(ModelRc::new(VecModel::from(rows)));
    let running = snapshots
        .iter()
        .filter(|tool| tool.state == ToolState::Running)
        .count();
    let crashed = snapshots
        .iter()
        .filter(|tool| tool.state == ToolState::Crashed)
        .count();
    window.set_summary(
        format!(
            "{running} running  /  {} total  /  {crashed} attention",
            snapshots.len()
        )
        .into(),
    );
    if selected >= 0 && (selected as usize) < supervisor.tool_count() {
        window.set_log_text(supervisor.log_snapshot(selected as usize).into());
    } else {
        window.set_log_text("Select a tool to inspect its live output.".into());
    }
}

fn row_from_snapshot(snapshot: &ToolSnapshot) -> ToolRow {
    let (status, accent) = match snapshot.state {
        ToolState::Stopped => ("Stopped", slint::Color::from_rgb_u8(116, 131, 145)),
        ToolState::Starting => ("Starting", slint::Color::from_rgb_u8(241, 167, 58)),
        ToolState::Running => ("Running", slint::Color::from_rgb_u8(55, 190, 142)),
        ToolState::Stopping => ("Stopping", slint::Color::from_rgb_u8(241, 167, 58)),
        ToolState::Restarting => ("Restarting", slint::Color::from_rgb_u8(241, 167, 58)),
        ToolState::Crashed => ("Crashed", slint::Color::from_rgb_u8(239, 93, 89)),
    };
    ToolRow {
        name: snapshot.name.clone().into(),
        status: status.into(),
        memory: format_memory(snapshot.memory).into(),
        accent,
        pid: snapshot.pid.unwrap_or_default() as i32,
        manager: false,
    }
}

fn format_memory(memory: MemoryValue) -> String {
    match memory {
        MemoryValue::Idle => "-".into(),
        MemoryValue::Pending => "Querying...".into(),
        MemoryValue::Unavailable => "N/A".into(),
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
