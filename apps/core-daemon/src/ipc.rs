use std::{
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flowtile_domain::{CorrelationId, DomainEvent, MonitorId, NavigationScope, WindowId};
use flowtile_ipc::{IpcError, IpcEvent, IpcRequest, IpcResponse, NamedPipeListener};
use flowtile_wm_core::{CoreDaemonRuntime, RuntimeCycleReport, RuntimeError};
use serde_json::{Value, json};

use crate::{ControlMessage, projection::build_snapshot_projection};

pub fn spawn_ipc_servers(control_sender: mpsc::Sender<ControlMessage>) {
    spawn_command_listener(control_sender.clone());
    spawn_event_stream_listener(control_sender);
}

pub fn handle_ipc_request(
    runtime: &mut CoreDaemonRuntime,
    dry_run: bool,
    request: IpcRequest,
    manual_correlation_id: &mut u64,
) -> (IpcResponse, bool) {
    if request.protocol_version != flowtile_ipc::PROTOCOL_VERSION {
        return (
            IpcResponse::error(
                request.request_id,
                IpcError::new(
                    "unsupported_protocol_version",
                    format!(
                        "protocol version {} is not supported",
                        request.protocol_version
                    ),
                    "contract",
                    false,
                ),
            ),
            false,
        );
    }

    let request_id = request.request_id.clone();
    match request.command.as_str() {
        "get_outputs" => {
            let snapshot = build_snapshot_projection(runtime);
            (
                IpcResponse::ok(
                    request_id,
                    json!({
                        "state_version": snapshot.state_version,
                        "outputs": snapshot.outputs,
                    }),
                ),
                false,
            )
        }
        "get_workspaces" => {
            let snapshot = build_snapshot_projection(runtime);
            (
                IpcResponse::ok(
                    request_id,
                    json!({
                        "state_version": snapshot.state_version,
                        "workspaces": snapshot.workspaces,
                    }),
                ),
                false,
            )
        }
        "get_windows" => {
            let snapshot = build_snapshot_projection(runtime);
            (
                IpcResponse::ok(
                    request_id,
                    json!({
                        "state_version": snapshot.state_version,
                        "windows": snapshot.windows,
                    }),
                ),
                false,
            )
        }
        "get_focus" => {
            let snapshot = build_snapshot_projection(runtime);
            (
                IpcResponse::ok(
                    request_id,
                    json!({
                        "state_version": snapshot.state_version,
                        "focus": snapshot.focus,
                        "overview": snapshot.overview,
                        "diagnostics": snapshot.diagnostics,
                    }),
                ),
                false,
            )
        }
        "dump_diagnostics" => {
            let snapshot = build_snapshot_projection(runtime);
            (
                IpcResponse::ok(
                    request_id,
                    json!({
                        "state_version": snapshot.state_version,
                        "diagnostics": snapshot.diagnostics,
                        "config": snapshot.config,
                    }),
                ),
                false,
            )
        }
        "focus_next" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::focus_next(
                next_manual_correlation_id(manual_correlation_id),
                NavigationScope::WorkspaceStrip,
            ),
            "ipc-focus-next",
        ),
        "focus_prev" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::focus_prev(
                next_manual_correlation_id(manual_correlation_id),
                NavigationScope::WorkspaceStrip,
            ),
            "ipc-focus-prev",
        ),
        "scroll_strip_left" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::scroll_strip_left(
                next_manual_correlation_id(manual_correlation_id),
                NavigationScope::WorkspaceStrip,
                0,
            ),
            "ipc-scroll-strip-left",
        ),
        "scroll_strip_right" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::scroll_strip_right(
                next_manual_correlation_id(manual_correlation_id),
                NavigationScope::WorkspaceStrip,
                0,
            ),
            "ipc-scroll-strip-right",
        ),
        "toggle_floating" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::toggle_floating(
                next_manual_correlation_id(manual_correlation_id),
                optional_window_id(&request.payload),
            ),
            "ipc-toggle-floating",
        ),
        "toggle_tabbed" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::toggle_tabbed(
                next_manual_correlation_id(manual_correlation_id),
                optional_window_id(&request.payload),
            ),
            "ipc-toggle-tabbed",
        ),
        "toggle_maximized" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::toggle_maximized(
                next_manual_correlation_id(manual_correlation_id),
                optional_window_id(&request.payload),
            ),
            "ipc-toggle-maximized",
        ),
        "toggle_fullscreen" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::toggle_fullscreen(
                next_manual_correlation_id(manual_correlation_id),
                optional_window_id(&request.payload),
            ),
            "ipc-toggle-fullscreen",
        ),
        "toggle_overview" => respond_to_runtime_command(
            request_id,
            runtime,
            dry_run,
            DomainEvent::toggle_overview(
                next_manual_correlation_id(manual_correlation_id),
                optional_monitor_id(&request.payload),
            ),
            "ipc-toggle-overview",
        ),
        "reload_config" => match runtime.reload_config(dry_run) {
            Ok(report) => (
                IpcResponse::ok(request_id, runtime_report_value(runtime, &report)),
                true,
            ),
            Err(error) => (runtime_error_response(request_id, error), false),
        },
        "move_window" | "consume_window" | "expel_window" | "capture_action" => (
            IpcResponse::error(
                request_id,
                IpcError::new(
                    "unsupported_command",
                    format!("command '{}' is not implemented yet", request.command),
                    "feature-gap",
                    false,
                ),
            ),
            false,
        ),
        _ => (
            IpcResponse::error(
                request_id,
                IpcError::new(
                    "unknown_command",
                    format!("unknown command '{}'", request.command),
                    "contract",
                    false,
                ),
            ),
            false,
        ),
    }
}

pub fn send_initial_snapshot(
    subscriber: &mpsc::Sender<String>,
    runtime: &CoreDaemonRuntime,
    stream_version: &mut u64,
) -> bool {
    let snapshot = build_snapshot_projection(runtime);
    for line in [
        event_line(
            stream_version,
            "snapshot_begin",
            snapshot.state_version,
            json!({}),
        ),
        event_line(
            stream_version,
            "snapshot_state",
            snapshot.state_version,
            serde_json::to_value(&snapshot).expect("snapshot projection should serialize"),
        ),
        event_line(
            stream_version,
            "snapshot_end",
            snapshot.state_version,
            json!({}),
        ),
    ] {
        if subscriber.send(line).is_err() {
            return false;
        }
    }

    true
}

pub fn broadcast_runtime_delta(
    subscribers: &mut Vec<mpsc::Sender<String>>,
    runtime: &CoreDaemonRuntime,
    stream_version: &mut u64,
) {
    let snapshot = build_snapshot_projection(runtime);
    let lines = vec![
        event_line(
            stream_version,
            "monitor_changed",
            snapshot.state_version,
            json!({ "outputs": snapshot.outputs }),
        ),
        event_line(
            stream_version,
            "workspace_changed",
            snapshot.state_version,
            json!({ "workspaces": snapshot.workspaces }),
        ),
        event_line(
            stream_version,
            "window_changed",
            snapshot.state_version,
            json!({ "windows": snapshot.windows }),
        ),
        event_line(
            stream_version,
            "focus_changed",
            snapshot.state_version,
            json!({ "focus": snapshot.focus }),
        ),
        event_line(
            stream_version,
            "overview_changed",
            snapshot.state_version,
            json!({ "overview": snapshot.overview }),
        ),
        event_line(
            stream_version,
            "config_changed",
            snapshot.state_version,
            json!({ "config": snapshot.config }),
        ),
        event_line(
            stream_version,
            "diagnostic_notice",
            snapshot.state_version,
            json!({ "diagnostics": snapshot.diagnostics }),
        ),
    ];

    subscribers.retain(|subscriber| {
        for line in &lines {
            if subscriber.send(line.clone()).is_err() {
                return false;
            }
        }
        true
    });
}

fn respond_to_runtime_command(
    request_id: String,
    runtime: &mut CoreDaemonRuntime,
    dry_run: bool,
    event: DomainEvent,
    reason: &str,
) -> (IpcResponse, bool) {
    match runtime.dispatch_command(event, dry_run, reason) {
        Ok(report) => (
            IpcResponse::ok(request_id, runtime_report_value(runtime, &report)),
            true,
        ),
        Err(error) => (runtime_error_response(request_id, error), false),
    }
}

fn runtime_report_value(runtime: &CoreDaemonRuntime, report: &RuntimeCycleReport) -> Value {
    json!({
        "accepted": true,
        "state_version": runtime.state().state_version().get(),
        "management_enabled": runtime.management_enabled(),
        "observation_reason": report.observation_reason,
        "planned_operations": report.planned_operations,
        "applied_operations": report.applied_operations,
        "apply_failures": report.apply_failures,
        "degraded_reasons": report.degraded_reasons,
    })
}

fn runtime_error_response(request_id: String, error: RuntimeError) -> IpcResponse {
    IpcResponse::error(
        request_id,
        IpcError::new("runtime_error", format!("{error:?}"), "runtime", true),
    )
}

fn optional_window_id(payload: &Value) -> Option<WindowId> {
    payload
        .get("window_id")
        .and_then(Value::as_u64)
        .map(WindowId::new)
}

fn optional_monitor_id(payload: &Value) -> Option<MonitorId> {
    payload
        .get("monitor_id")
        .and_then(Value::as_u64)
        .map(MonitorId::new)
}

fn next_manual_correlation_id(counter: &mut u64) -> CorrelationId {
    let correlation_id = CorrelationId::new(*counter);
    *counter += 1;
    correlation_id
}

fn event_line(
    stream_version: &mut u64,
    event_kind: &str,
    state_version: u64,
    payload: Value,
) -> String {
    let current_version = *stream_version;
    *stream_version = current_version.saturating_add(1);

    serde_json::to_string(&IpcEvent {
        protocol_version: flowtile_ipc::PROTOCOL_VERSION,
        stream_version: current_version,
        event_id: format!("evt-{current_version}"),
        event_kind: event_kind.to_string(),
        state_version,
        payload,
        timestamp: unix_timestamp(),
    })
    .expect("ipc event should serialize")
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_secs()
}

fn spawn_command_listener(control_sender: mpsc::Sender<ControlMessage>) {
    thread::spawn(move || {
        let listener = NamedPipeListener::command();
        loop {
            match listener.accept() {
                Ok(connection) => {
                    let control_sender = control_sender.clone();
                    thread::spawn(move || {
                        let request_text = match connection.read_message() {
                            Ok(payload) => payload,
                            Err(error) => {
                                eprintln!("ipc command read failed: {error}");
                                return;
                            }
                        };

                        let request = match serde_json::from_str::<IpcRequest>(&request_text) {
                            Ok(request) => request,
                            Err(error) => {
                                let response = IpcResponse::error(
                                    "unknown",
                                    IpcError::new(
                                        "malformed_json",
                                        format!("invalid IPC request JSON: {error}"),
                                        "transport",
                                        false,
                                    ),
                                );
                                if let Ok(payload) = serde_json::to_string(&response) {
                                    let _ = connection.write_message(&payload);
                                }
                                return;
                            }
                        };

                        let (response_sender, response_receiver) = mpsc::channel();
                        if control_sender
                            .send(ControlMessage::IpcRequest {
                                request,
                                response_sender,
                            })
                            .is_err()
                        {
                            return;
                        }

                        let Ok(response) = response_receiver.recv() else {
                            return;
                        };
                        let Ok(payload) = serde_json::to_string(&response) else {
                            return;
                        };
                        if let Err(error) = connection.write_message(&payload) {
                            eprintln!("ipc command write failed: {error}");
                        }
                    });
                }
                Err(error) => {
                    eprintln!("ipc command listener accept failed: {error}");
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
}

fn spawn_event_stream_listener(control_sender: mpsc::Sender<ControlMessage>) {
    thread::spawn(move || {
        let listener = NamedPipeListener::event_stream();
        loop {
            match listener.accept() {
                Ok(connection) => {
                    let (sender, receiver) = mpsc::channel::<String>();
                    if control_sender
                        .send(ControlMessage::EventSubscribe { sender })
                        .is_err()
                    {
                        return;
                    }

                    thread::spawn(move || {
                        while let Ok(line) = receiver.recv() {
                            if connection.write_message(&format!("{line}\n")).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(error) => {
                    eprintln!("ipc event listener accept failed: {error}");
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
}
