use std::{
    fmt,
    io::{self, BufRead},
    sync::mpsc,
    thread,
};

use flowtile_config_rules::TouchpadConfig;
use flowtile_domain::{ColumnId, CorrelationId, RuntimeMode, WindowNode, WmState, WorkspaceId};
use flowtile_wm_core::{CoreDaemonRuntime, RuntimeCycleReport};

use crate::{
    control::{ControlMessage, WatchCommand},
    diag::write_runtime_log,
    hotkeys::{HotkeyListener, HotkeyListenerError, ensure_bind_control_mode_supported},
    ipc,
    overview_controller::OverviewSurfaceController,
    tab_indicator::TabIndicatorController,
    touchpad::{
        TouchpadListener, TouchpadListenerError, assess_touchpad_override,
        ensure_touchpad_override_supported,
    },
};

pub(super) struct RuntimeInputListeners {
    pub(super) hotkeys: Option<HotkeyListener>,
    pub(super) touchpad: Option<TouchpadListener>,
}

#[derive(Debug)]
pub(super) enum RestartInputListenersError {
    Hotkeys(HotkeyListenerError),
    Touchpad(TouchpadListenerError),
}

impl fmt::Display for RestartInputListenersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hotkeys(error) => write!(f, "{error}"),
            Self::Touchpad(error) => write!(f, "{error}"),
        }
    }
}

pub(super) fn spawn_stdin_listener(control_sender: mpsc::Sender<ControlMessage>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut locked = stdin.lock();
        let mut line = String::new();

        loop {
            line.clear();
            match locked.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let command = line.trim().to_ascii_lowercase();
                    let watch_command = WatchCommand::from_stdin_alias(command.as_str());
                    if let Some(watch_command) = watch_command
                        && control_sender
                            .send(ControlMessage::Watch(watch_command))
                            .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

pub(super) fn maybe_broadcast_state(
    runtime: &CoreDaemonRuntime,
    event_subscribers: &mut Vec<mpsc::Sender<String>>,
    stream_version: &mut u64,
    last_streamed_state_version: &mut u64,
) {
    let current_state_version = runtime.state().state_version().get();
    if current_state_version == *last_streamed_state_version {
        return;
    }

    ipc::broadcast_runtime_delta(event_subscribers, runtime, stream_version);
    *last_streamed_state_version = current_state_version;
}

pub(super) fn sync_visual_surfaces(
    runtime: &CoreDaemonRuntime,
    control_sender: &mpsc::Sender<ControlMessage>,
    tab_indicator: &mut Option<TabIndicatorController>,
    overview_surface: &mut Option<OverviewSurfaceController>,
) {
    if let Some(controller) = tab_indicator.as_mut()
        && let Err(error) = controller.sync(runtime)
    {
        log_visual_surface_error("tab-indicator", &error);
        *tab_indicator = None;
    }

    ensure_overview_surface(runtime, control_sender, overview_surface);
    if let Some(controller) = overview_surface.as_mut()
        && let Err(error) = controller.sync(runtime)
    {
        log_visual_surface_error("overview-surface", &error);
        *overview_surface = None;
    }
}

pub(super) fn print_iteration(iteration: u64, report: &RuntimeCycleReport) {
    println!("iteration {iteration}");
    print_report(report);
}

pub(super) fn record_runtime_report(
    runtime: &CoreDaemonRuntime,
    completed_iterations: &mut u64,
    event_subscribers: &mut Vec<mpsc::Sender<String>>,
    stream_version: &mut u64,
    last_streamed_state_version: &mut u64,
    banner: Option<&str>,
    report: &RuntimeCycleReport,
) {
    if let Some(banner) = banner {
        println!("{banner}");
    }
    print_iteration(*completed_iterations + 1, report);
    *completed_iterations += 1;
    maybe_broadcast_state(
        runtime,
        event_subscribers,
        stream_version,
        last_streamed_state_version,
    );
}

pub(super) fn print_state_snapshot(runtime: &CoreDaemonRuntime) {
    let state = runtime.state();
    let touchpad = assess_touchpad_override(runtime.touchpad_config());
    println!("state snapshot");
    println!("state version: {}", state.state_version().get());
    println!("monitors: {}", state.monitors.len());
    println!("workspaces: {}", state.workspaces.len());
    println!("windows: {}", state.windows.len());
    println!(
        "focused window: {}",
        state
            .focus
            .focused_window_id
            .map(|window_id| window_id.get().to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    println!("overview open: {}", state.overview.is_open);
    println!("config version: {}", state.config_projection.config_version);
    println!(
        "config rules: {}",
        state.config_projection.active_rule_count
    );
    println!("touchpad override: {}", touchpad.summary_label());
    println!(
        "touchpad gesture bindings: {}",
        touchpad.configured_gesture_count
    );
    if let Some(detail) = touchpad.detail {
        println!("touchpad override detail: {detail}");
    }
    if let Some(monitor_id) = state.focus.focused_monitor_id
        && let Some(workspace_id) = state.active_workspace_id_for_monitor(monitor_id)
        && let Some(workspace) = state.workspaces.get(&workspace_id)
    {
        println!("active workspace: {}", workspace_id.get());
        println!("strip scroll offset: {}", workspace.strip.scroll_offset);
        println!(
            "strip columns: {}",
            workspace.strip.ordered_column_ids.len()
        );
    }
}

pub(super) fn next_manual_correlation_id(counter: &mut u64) -> CorrelationId {
    let correlation_id = CorrelationId::new(*counter);
    *counter += 1;
    correlation_id
}

pub(super) fn focused_managed_hwnd(runtime: &CoreDaemonRuntime) -> Option<u64> {
    runtime
        .state()
        .focus
        .focused_window_id
        .and_then(|window_id| runtime.state().windows.get(&window_id))
        .filter(|window| window.is_managed)
        .and_then(|window| window.current_hwnd_binding)
}

pub(super) fn overview_activation_target_is_valid(state: &WmState, raw_hwnd: u64) -> bool {
    if raw_hwnd == 0 || !state.overview.is_open {
        return false;
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return false;
    };

    state.windows.values().any(|window| {
        if !window.is_managed || window.current_hwnd_binding != Some(raw_hwnd) {
            return false;
        }

        state
            .workspaces
            .get(&window.workspace_id)
            .is_some_and(|workspace| workspace.monitor_id == monitor_id)
    })
}

pub(super) fn overview_workspace_target_is_valid(
    state: &WmState,
    workspace_id: WorkspaceId,
) -> bool {
    if !state.overview.is_open {
        return false;
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return false;
    };

    state
        .workspaces
        .get(&workspace_id)
        .is_some_and(|workspace| workspace.monitor_id == monitor_id)
}

pub(super) fn overview_drag_source_column_id(state: &WmState, raw_hwnd: u64) -> Option<ColumnId> {
    let window = overview_managed_window_for_hwnd(state, raw_hwnd)?;
    (window.layer == flowtile_domain::WindowLayer::Tiled
        && !window.is_floating
        && !window.is_fullscreen)
        .then_some(window.column_id)
        .flatten()
}

pub(super) fn overview_drop_anchor_column_id(
    state: &WmState,
    target_workspace_id: WorkspaceId,
    raw_hwnd: Option<u64>,
) -> Option<Option<ColumnId>> {
    let Some(raw_hwnd) = raw_hwnd else {
        return Some(None);
    };
    let window = overview_managed_window_for_hwnd(state, raw_hwnd)?;
    if window.workspace_id != target_workspace_id
        || window.layer != flowtile_domain::WindowLayer::Tiled
        || window.is_floating
        || window.is_fullscreen
    {
        return None;
    }
    window.column_id.map(Some)
}

pub(super) fn start_hotkey_listener(
    runtime: &CoreDaemonRuntime,
    command_sender: &mpsc::Sender<ControlMessage>,
) -> Result<Option<HotkeyListener>, HotkeyListenerError> {
    HotkeyListener::spawn(
        runtime.hotkeys(),
        runtime.bind_control_mode(),
        command_sender.clone(),
    )
}

pub(super) fn validate_runtime_bind_control_mode(
    runtime: &CoreDaemonRuntime,
) -> Result<(), HotkeyListenerError> {
    ensure_bind_control_mode_supported(runtime.bind_control_mode())
}

pub(super) fn start_touchpad_listener(
    runtime: &CoreDaemonRuntime,
    command_sender: &mpsc::Sender<ControlMessage>,
) -> Result<Option<TouchpadListener>, TouchpadListenerError> {
    TouchpadListener::spawn(runtime.touchpad_config(), command_sender.clone())
}

pub(super) fn validate_runtime_touchpad_override(
    runtime: &CoreDaemonRuntime,
) -> Result<(), TouchpadListenerError> {
    ensure_touchpad_override_supported(runtime.touchpad_config())
}

pub(super) fn touchpad_watch_status(
    config: &TouchpadConfig,
    listener_running: bool,
) -> &'static str {
    if listener_running {
        "enabled"
    } else {
        assess_touchpad_override(config).summary_label()
    }
}

pub(super) fn restart_input_listeners(
    runtime: &CoreDaemonRuntime,
    command_sender: &mpsc::Sender<ControlMessage>,
) -> Result<RuntimeInputListeners, RestartInputListenersError> {
    let hotkeys = start_hotkey_listener(runtime, command_sender)
        .map_err(RestartInputListenersError::Hotkeys)?;
    let touchpad = start_touchpad_listener(runtime, command_sender)
        .map_err(RestartInputListenersError::Touchpad)?;
    Ok(RuntimeInputListeners { hotkeys, touchpad })
}

pub(super) fn print_reloaded_input_status(
    runtime: &CoreDaemonRuntime,
    hotkey_listener: &Option<HotkeyListener>,
    touchpad_listener: &Option<TouchpadListener>,
    context_suffix: &str,
) {
    println!(
        "global hotkeys reloaded{context_suffix}: {}",
        if hotkey_listener.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "touchpad override reloaded{context_suffix}: {}",
        touchpad_watch_status(runtime.touchpad_config(), touchpad_listener.is_some(),)
    );
}

fn ensure_overview_surface(
    runtime: &CoreDaemonRuntime,
    control_sender: &mpsc::Sender<ControlMessage>,
    overview_surface: &mut Option<OverviewSurfaceController>,
) {
    if overview_surface.is_some()
        || !runtime.management_enabled()
        || runtime.state().runtime.boot_mode == RuntimeMode::SafeMode
        || !runtime.state().overview.is_open
    {
        return;
    }

    match OverviewSurfaceController::spawn(control_sender.clone()) {
        Ok(controller) => {
            write_runtime_log("watch: overview-surface-restarted");
            *overview_surface = Some(controller);
        }
        Err(error) => {
            log_visual_surface_error("overview-surface-restart", &error);
        }
    }
}

fn log_visual_surface_error(kind: &str, error: &impl std::fmt::Display) {
    let message = format!("{kind} degraded: {error}");
    write_runtime_log(format!("watch: {message}"));
    eprintln!("{message}");
}

fn print_report(report: &RuntimeCycleReport) {
    for line in report.summary_lines() {
        println!("{line}");
        write_runtime_log(format!("report: {line}"));
    }
}

fn overview_managed_window_for_hwnd(state: &WmState, raw_hwnd: u64) -> Option<&WindowNode> {
    let monitor_id = state.overview.monitor_id?;
    state.windows.values().find(|window| {
        if !window.is_managed || window.current_hwnd_binding != Some(raw_hwnd) {
            return false;
        }

        state
            .workspaces
            .get(&window.workspace_id)
            .is_some_and(|workspace| workspace.monitor_id == monitor_id)
    })
}
