use std::{process::ExitCode, sync::mpsc};

use flowtile_domain::{DomainEvent, NavigationScope};
use flowtile_wm_core::CoreDaemonRuntime;

use crate::{
    control::{ControlMessage, WatchCommand},
    diag::write_runtime_log,
    flowshell_core::open_wallpaper_selector,
    hotkeys::HotkeyListener,
    terminal::open_default_terminal,
    touchpad::TouchpadListener,
    window_actions::{close_wallpaper_selector_window, close_window},
};

use super::support::{
    RestartInputListenersError, focused_managed_hwnd, next_manual_correlation_id,
    print_reloaded_input_status, print_state_snapshot, record_runtime_report,
    restart_input_listeners,
};

pub(super) enum WatchCommandFlow {
    Continue,
    Quit,
    Exit(ExitCode),
}

pub(super) struct WatchCommandContext<'a> {
    pub(super) runtime: &'a mut CoreDaemonRuntime,
    pub(super) dry_run: bool,
    pub(super) control_sender: &'a mpsc::Sender<ControlMessage>,
    pub(super) hotkey_listener: &'a mut Option<HotkeyListener>,
    pub(super) touchpad_listener: &'a mut Option<TouchpadListener>,
    pub(super) completed_iterations: &'a mut u64,
    pub(super) manual_correlation_id: &'a mut u64,
    pub(super) event_subscribers: &'a mut Vec<mpsc::Sender<String>>,
    pub(super) stream_version: &'a mut u64,
    pub(super) last_streamed_state_version: &'a mut u64,
}

pub(super) fn handle_watch_command(
    command: WatchCommand,
    context: &mut WatchCommandContext<'_>,
) -> WatchCommandFlow {
    if let Some((event, runtime_label, user_label)) = manual_domain_dispatch_spec(
        command,
        next_manual_correlation_id(context.manual_correlation_id),
    ) {
        return match dispatch_manual_domain_event(context, event, runtime_label, user_label) {
            Ok(()) => WatchCommandFlow::Continue,
            Err(code) => WatchCommandFlow::Exit(code),
        };
    }

    match command {
        WatchCommand::OpenTerminal => {
            run_manual_side_effect(
                "open-terminal",
                open_default_terminal,
                "watch: open-terminal-ok",
                "watch: open-terminal-error",
                "open terminal failed",
            );
            WatchCommandFlow::Continue
        }
        WatchCommand::OpenWallpaperSelector => {
            match close_wallpaper_selector_window() {
                Ok(true) => {
                    write_runtime_log("watch: close-wallpaper-selector-ok");
                    println!("manual command: close-wallpaper-selector");
                }
                Ok(false) => {
                    run_manual_side_effect(
                        "open-wallpaper-selector",
                        open_wallpaper_selector,
                        "watch: open-wallpaper-selector-ok",
                        "watch: open-wallpaper-selector-error",
                        "open wallpaper selector failed",
                    );
                }
                Err(error) => {
                    write_runtime_log(format!("watch: close-wallpaper-selector-error={error}"));
                    eprintln!("close wallpaper selector failed: {error}");
                }
            }
            WatchCommandFlow::Continue
        }
        WatchCommand::CloseWindow => {
            run_manual_side_effect(
                "close-window",
                || {
                    focused_managed_hwnd(context.runtime)
                        .ok_or_else(|| {
                            "no focused managed window is currently bound to an HWND".to_string()
                        })
                        .and_then(close_window)
                },
                "watch: close-window-ok",
                "watch: close-window-error",
                "close window failed",
            );
            WatchCommandFlow::Continue
        }
        WatchCommand::ReloadConfig => match context.runtime.reload_config(context.dry_run) {
            Ok(report) => match restart_input_listeners(context.runtime, context.control_sender) {
                Ok(listeners) => {
                    *context.hotkey_listener = listeners.hotkeys;
                    *context.touchpad_listener = listeners.touchpad;
                    print_reloaded_input_status(
                        context.runtime,
                        context.hotkey_listener,
                        context.touchpad_listener,
                        "",
                    );
                    record_runtime_report(
                        context.runtime,
                        context.completed_iterations,
                        context.event_subscribers,
                        context.stream_version,
                        context.last_streamed_state_version,
                        Some("manual command: reload-config"),
                        &report,
                    );
                    WatchCommandFlow::Continue
                }
                Err(RestartInputListenersError::Hotkeys(error)) => {
                    eprintln!("global hotkeys failed to restart: {error}");
                    WatchCommandFlow::Exit(ExitCode::from(1))
                }
                Err(RestartInputListenersError::Touchpad(error)) => {
                    eprintln!("touchpad override failed to restart: {error}");
                    WatchCommandFlow::Exit(ExitCode::from(1))
                }
            },
            Err(error) => {
                eprintln!("{error:?}");
                WatchCommandFlow::Exit(ExitCode::from(1))
            }
        },
        WatchCommand::Snapshot => {
            print_state_snapshot(context.runtime);
            WatchCommandFlow::Continue
        }
        WatchCommand::Unwind => {
            context.runtime.request_emergency_unwind("manual-command");
            println!("management disabled by emergency unwind");
            WatchCommandFlow::Continue
        }
        WatchCommand::Rescan => match context.runtime.scan_and_sync(context.dry_run) {
            Ok(report) => {
                record_runtime_report(
                    context.runtime,
                    context.completed_iterations,
                    context.event_subscribers,
                    context.stream_version,
                    context.last_streamed_state_version,
                    Some("manual rescan"),
                    &report,
                );
                WatchCommandFlow::Continue
            }
            Err(error) => {
                eprintln!("{error:?}");
                WatchCommandFlow::Exit(ExitCode::from(1))
            }
        },
        WatchCommand::Quit => WatchCommandFlow::Quit,
        WatchCommand::FocusNext
        | WatchCommand::FocusPrev
        | WatchCommand::FocusWorkspaceUp
        | WatchCommand::FocusWorkspaceDown
        | WatchCommand::ScrollLeft
        | WatchCommand::ScrollRight
        | WatchCommand::MoveWorkspaceUp
        | WatchCommand::MoveWorkspaceDown
        | WatchCommand::MoveWorkspaceToMonitorNext
        | WatchCommand::MoveWorkspaceToMonitorPrevious
        | WatchCommand::MoveColumnToWorkspaceUp
        | WatchCommand::MoveColumnToWorkspaceDown
        | WatchCommand::CycleColumnWidth
        | WatchCommand::ToggleFloating
        | WatchCommand::ToggleTabbed
        | WatchCommand::ToggleMaximized
        | WatchCommand::ToggleFullscreen
        | WatchCommand::OpenOverview
        | WatchCommand::CloseOverview
        | WatchCommand::ToggleOverview => unreachable!("domain command path handled above"),
    }
}

fn dispatch_manual_domain_event(
    context: &mut WatchCommandContext<'_>,
    event: DomainEvent,
    runtime_label: &'static str,
    user_label: &'static str,
) -> Result<(), ExitCode> {
    match context
        .runtime
        .dispatch_command(event, context.dry_run, runtime_label)
    {
        Ok(report) => {
            let banner = format!("manual command: {user_label}");
            record_runtime_report(
                context.runtime,
                context.completed_iterations,
                context.event_subscribers,
                context.stream_version,
                context.last_streamed_state_version,
                Some(banner.as_str()),
                &report,
            );
            Ok(())
        }
        Err(error) => {
            eprintln!("{error:?}");
            Err(ExitCode::from(1))
        }
    }
}

fn run_manual_side_effect<F>(
    user_label: &str,
    action: F,
    ok_log: &str,
    error_log_prefix: &str,
    error_banner: &str,
) where
    F: FnOnce() -> Result<(), String>,
{
    match action() {
        Ok(()) => {
            write_runtime_log(ok_log);
            println!("manual command: {user_label}");
        }
        Err(error) => {
            write_runtime_log(format!("{error_log_prefix}={error}"));
            eprintln!("{error_banner}: {error}");
        }
    }
}

fn manual_domain_dispatch_spec(
    command: WatchCommand,
    correlation_id: flowtile_domain::CorrelationId,
) -> Option<(DomainEvent, &'static str, &'static str)> {
    match command {
        WatchCommand::FocusNext => Some((
            DomainEvent::focus_next(correlation_id, NavigationScope::WorkspaceStrip),
            "manual-focus-next",
            "focus-next",
        )),
        WatchCommand::FocusPrev => Some((
            DomainEvent::focus_prev(correlation_id, NavigationScope::WorkspaceStrip),
            "manual-focus-prev",
            "focus-prev",
        )),
        WatchCommand::FocusWorkspaceUp => Some((
            DomainEvent::focus_workspace_up(correlation_id, None),
            "manual-focus-workspace-up",
            "focus-workspace-up",
        )),
        WatchCommand::FocusWorkspaceDown => Some((
            DomainEvent::focus_workspace_down(correlation_id, None),
            "manual-focus-workspace-down",
            "focus-workspace-down",
        )),
        WatchCommand::ScrollLeft => Some((
            DomainEvent::scroll_strip_left(correlation_id, NavigationScope::WorkspaceStrip, 0),
            "manual-scroll-left",
            "scroll-left",
        )),
        WatchCommand::ScrollRight => Some((
            DomainEvent::scroll_strip_right(correlation_id, NavigationScope::WorkspaceStrip, 0),
            "manual-scroll-right",
            "scroll-right",
        )),
        WatchCommand::MoveWorkspaceUp => Some((
            DomainEvent::move_workspace_up(correlation_id, None),
            "manual-move-workspace-up",
            "move-workspace-up",
        )),
        WatchCommand::MoveWorkspaceDown => Some((
            DomainEvent::move_workspace_down(correlation_id, None),
            "manual-move-workspace-down",
            "move-workspace-down",
        )),
        WatchCommand::MoveWorkspaceToMonitorNext => Some((
            DomainEvent::move_workspace_to_monitor_next(correlation_id, None),
            "manual-move-workspace-to-monitor-next",
            "move-workspace-to-monitor-next",
        )),
        WatchCommand::MoveWorkspaceToMonitorPrevious => Some((
            DomainEvent::move_workspace_to_monitor_previous(correlation_id, None),
            "manual-move-workspace-to-monitor-previous",
            "move-workspace-to-monitor-previous",
        )),
        WatchCommand::MoveColumnToWorkspaceUp => Some((
            DomainEvent::move_column_to_workspace_up(correlation_id, None),
            "manual-move-column-to-workspace-up",
            "move-column-to-workspace-up",
        )),
        WatchCommand::MoveColumnToWorkspaceDown => Some((
            DomainEvent::move_column_to_workspace_down(correlation_id, None),
            "manual-move-column-to-workspace-down",
            "move-column-to-workspace-down",
        )),
        WatchCommand::CycleColumnWidth => Some((
            DomainEvent::cycle_column_width(correlation_id),
            "manual-cycle-column-width",
            "cycle-column-width",
        )),
        WatchCommand::ToggleFloating => Some((
            DomainEvent::toggle_floating(correlation_id, None),
            "manual-toggle-floating",
            "toggle-floating",
        )),
        WatchCommand::ToggleTabbed => Some((
            DomainEvent::toggle_tabbed(correlation_id, None),
            "manual-toggle-tabbed",
            "toggle-tabbed",
        )),
        WatchCommand::ToggleMaximized => Some((
            DomainEvent::toggle_maximized(correlation_id, None),
            "manual-toggle-maximized",
            "toggle-maximized",
        )),
        WatchCommand::ToggleFullscreen => Some((
            DomainEvent::toggle_fullscreen(correlation_id, None),
            "manual-toggle-fullscreen",
            "toggle-fullscreen",
        )),
        WatchCommand::OpenOverview => Some((
            DomainEvent::open_overview(correlation_id, None),
            "manual-open-overview",
            "open-overview",
        )),
        WatchCommand::CloseOverview => Some((
            DomainEvent::close_overview(correlation_id, None),
            "manual-close-overview",
            "close-overview",
        )),
        WatchCommand::ToggleOverview => Some((
            DomainEvent::toggle_overview(correlation_id, None),
            "manual-toggle-overview",
            "toggle-overview",
        )),
        WatchCommand::OpenTerminal
        | WatchCommand::OpenWallpaperSelector
        | WatchCommand::CloseWindow
        | WatchCommand::ReloadConfig
        | WatchCommand::Snapshot
        | WatchCommand::Unwind
        | WatchCommand::Rescan
        | WatchCommand::Quit => None,
    }
}
