use std::{process::ExitCode, sync::mpsc};

use flowtile_domain::DomainEvent;
use flowtile_ipc::{IpcRequest, IpcResponse};
use flowtile_wm_core::CoreDaemonRuntime;

use crate::{
    control::ControlMessage, ipc, overview_controller::OverviewSurfaceController,
    tab_indicator::TabIndicatorController, window_actions::activate_window,
};

use super::{
    commands::{WatchCommandContext, WatchCommandFlow, handle_watch_command},
    support::{
        RestartInputListenersError, maybe_broadcast_state, next_manual_correlation_id,
        overview_activation_target_is_valid, overview_drag_source_column_id,
        overview_drop_anchor_column_id, overview_workspace_target_is_valid,
        print_reloaded_input_status, record_runtime_report, restart_input_listeners,
        sync_visual_surfaces,
    },
};

pub(super) struct ControlPlaneContext<'a> {
    pub(super) runtime: &'a mut CoreDaemonRuntime,
    pub(super) dry_run: bool,
    pub(super) control_sender: &'a mpsc::Sender<ControlMessage>,
    pub(super) hotkey_listener: &'a mut Option<crate::hotkeys::HotkeyListener>,
    pub(super) touchpad_listener: &'a mut Option<crate::touchpad::TouchpadListener>,
    pub(super) tab_indicator: &'a mut Option<TabIndicatorController>,
    pub(super) overview_surface: &'a mut Option<OverviewSurfaceController>,
    pub(super) completed_iterations: &'a mut u64,
    pub(super) manual_correlation_id: &'a mut u64,
    pub(super) event_subscribers: &'a mut Vec<mpsc::Sender<String>>,
    pub(super) stream_version: &'a mut u64,
    pub(super) last_streamed_state_version: &'a mut u64,
}

pub(super) fn handle_control_message(
    message: ControlMessage,
    context: &mut ControlPlaneContext<'_>,
) -> WatchCommandFlow {
    match message {
        ControlMessage::Watch(command) => handle_watch_command(
            command,
            &mut WatchCommandContext {
                runtime: context.runtime,
                dry_run: context.dry_run,
                control_sender: context.control_sender,
                hotkey_listener: context.hotkey_listener,
                touchpad_listener: context.touchpad_listener,
                completed_iterations: context.completed_iterations,
                manual_correlation_id: context.manual_correlation_id,
                event_subscribers: context.event_subscribers,
                stream_version: context.stream_version,
                last_streamed_state_version: context.last_streamed_state_version,
            },
        ),
        ControlMessage::OverviewActivateWindow { raw_hwnd } => {
            handle_overview_activate_window(raw_hwnd, context)
        }
        ControlMessage::OverviewDismiss => handle_overview_dismiss(context),
        ControlMessage::OverviewMoveColumn {
            dragged_raw_hwnd,
            target_workspace_id,
            insert_after_raw_hwnd,
        } => handle_overview_move_column(
            dragged_raw_hwnd,
            target_workspace_id,
            insert_after_raw_hwnd,
            context,
        ),
        ControlMessage::IpcRequest {
            request,
            response_sender,
        } => handle_ipc_request(request, response_sender, context),
        ControlMessage::EventSubscribe { sender } => {
            if ipc::send_initial_snapshot(&sender, context.runtime, context.stream_version) {
                context.event_subscribers.push(sender);
            }
            WatchCommandFlow::Continue
        }
    }
}

fn handle_overview_activate_window(
    raw_hwnd: u64,
    context: &mut ControlPlaneContext<'_>,
) -> WatchCommandFlow {
    if !overview_activation_target_is_valid(context.runtime.state(), raw_hwnd) {
        crate::diag::write_runtime_log(format!(
            "watch: overview-activate-window-ignored hwnd={raw_hwnd}"
        ));
        return WatchCommandFlow::Continue;
    }

    match context.runtime.dispatch_command(
        DomainEvent::close_overview(
            next_manual_correlation_id(context.manual_correlation_id),
            None,
        ),
        context.dry_run,
        "overview-click-close-overview",
    ) {
        Ok(report) => {
            record_runtime_report(
                context.runtime,
                context.completed_iterations,
                context.event_subscribers,
                context.stream_version,
                context.last_streamed_state_version,
                Some("overview action: activate-window"),
                &report,
            );
        }
        Err(error) => {
            eprintln!("{error:?}");
            return WatchCommandFlow::Exit(ExitCode::from(1));
        }
    }

    sync_visual_surfaces(
        context.runtime,
        context.control_sender,
        context.tab_indicator,
        context.overview_surface,
    );

    if context.dry_run {
        crate::diag::write_runtime_log(format!(
            "watch: overview-activate-window-dry-run hwnd={raw_hwnd}"
        ));
    } else if let Err(error) = activate_window(raw_hwnd) {
        crate::diag::write_runtime_log(format!(
            "watch: overview-activate-window-error hwnd={raw_hwnd} error={error}"
        ));
        eprintln!("overview click activation failed: {error}");
    } else {
        crate::diag::write_runtime_log(format!(
            "watch: overview-activate-window-ok hwnd={raw_hwnd}"
        ));
    }

    WatchCommandFlow::Continue
}

fn handle_overview_dismiss(context: &mut ControlPlaneContext<'_>) -> WatchCommandFlow {
    if !context.runtime.state().overview.is_open {
        crate::diag::write_runtime_log("watch: overview-dismiss-ignored");
        return WatchCommandFlow::Continue;
    }

    match context.runtime.dispatch_command(
        DomainEvent::close_overview(
            next_manual_correlation_id(context.manual_correlation_id),
            None,
        ),
        context.dry_run,
        "overview-dismiss-close-overview",
    ) {
        Ok(report) => {
            record_runtime_report(
                context.runtime,
                context.completed_iterations,
                context.event_subscribers,
                context.stream_version,
                context.last_streamed_state_version,
                Some("overview action: dismiss"),
                &report,
            );
        }
        Err(error) => {
            eprintln!("{error:?}");
            return WatchCommandFlow::Exit(ExitCode::from(1));
        }
    }

    sync_visual_surfaces(
        context.runtime,
        context.control_sender,
        context.tab_indicator,
        context.overview_surface,
    );
    crate::diag::write_runtime_log("watch: overview-dismiss-ok");
    WatchCommandFlow::Continue
}

fn handle_overview_move_column(
    dragged_raw_hwnd: u64,
    target_workspace_id: flowtile_domain::WorkspaceId,
    insert_after_raw_hwnd: Option<u64>,
    context: &mut ControlPlaneContext<'_>,
) -> WatchCommandFlow {
    if !overview_workspace_target_is_valid(context.runtime.state(), target_workspace_id) {
        crate::diag::write_runtime_log(format!(
            "watch: overview-move-column-ignored invalid-target-workspace={}",
            target_workspace_id.get()
        ));
        return WatchCommandFlow::Continue;
    }
    let Some(source_column_id) =
        overview_drag_source_column_id(context.runtime.state(), dragged_raw_hwnd)
    else {
        crate::diag::write_runtime_log(format!(
            "watch: overview-move-column-ignored invalid-source hwnd={dragged_raw_hwnd}"
        ));
        return WatchCommandFlow::Continue;
    };
    let Some(anchor_column_id) = overview_drop_anchor_column_id(
        context.runtime.state(),
        target_workspace_id,
        insert_after_raw_hwnd,
    ) else {
        crate::diag::write_runtime_log(format!(
            "watch: overview-move-column-ignored invalid-anchor hwnd={insert_after_raw_hwnd:?}"
        ));
        return WatchCommandFlow::Continue;
    };

    match context.runtime.dispatch_command(
        DomainEvent::move_column_to_workspace_target(
            next_manual_correlation_id(context.manual_correlation_id),
            source_column_id,
            target_workspace_id,
            anchor_column_id,
        ),
        context.dry_run,
        "overview-drag-drop-column",
    ) {
        Ok(report) => {
            record_runtime_report(
                context.runtime,
                context.completed_iterations,
                context.event_subscribers,
                context.stream_version,
                context.last_streamed_state_version,
                Some("overview action: move-column"),
                &report,
            );
        }
        Err(error) => {
            eprintln!("{error:?}");
            return WatchCommandFlow::Exit(ExitCode::from(1));
        }
    }

    sync_visual_surfaces(
        context.runtime,
        context.control_sender,
        context.tab_indicator,
        context.overview_surface,
    );
    crate::diag::write_runtime_log(format!(
        "watch: overview-move-column-ok column={} target_workspace={} anchor={:?}",
        source_column_id.get(),
        target_workspace_id.get(),
        anchor_column_id.map(|column_id| column_id.get())
    ));
    WatchCommandFlow::Continue
}

fn handle_ipc_request(
    request: IpcRequest,
    response_sender: mpsc::Sender<IpcResponse>,
    context: &mut ControlPlaneContext<'_>,
) -> WatchCommandFlow {
    let command_name = request.command.clone();
    let (response, should_broadcast) = ipc::handle_ipc_request(
        context.runtime,
        context.dry_run,
        request,
        context.manual_correlation_id,
    );
    if command_name == "reload_config" && response.ok {
        match restart_input_listeners(context.runtime, context.control_sender) {
            Ok(listeners) => {
                *context.hotkey_listener = listeners.hotkeys;
                *context.touchpad_listener = listeners.touchpad;
                print_reloaded_input_status(
                    context.runtime,
                    context.hotkey_listener,
                    context.touchpad_listener,
                    " via IPC",
                );
            }
            Err(RestartInputListenersError::Hotkeys(error)) => {
                eprintln!("global hotkeys failed to restart via IPC: {error}");
                return WatchCommandFlow::Exit(ExitCode::from(1));
            }
            Err(RestartInputListenersError::Touchpad(error)) => {
                eprintln!("touchpad override failed to restart via IPC: {error}");
                return WatchCommandFlow::Exit(ExitCode::from(1));
            }
        }
    }
    let _ = response_sender.send(response);
    if should_broadcast {
        maybe_broadcast_state(
            context.runtime,
            context.event_subscribers,
            context.stream_version,
            context.last_streamed_state_version,
        );
    }
    WatchCommandFlow::Continue
}
