#![deny(unsafe_op_in_unsafe_fn)]

mod transport;

use flowtile_diagnostics::PerfTelemetrySnapshot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use transport::{
    CommandClient, NamedPipeConnection, NamedPipeListener, TransportError, connect_event_stream,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const TRANSPORT: &str = "local-named-pipe";
pub const COMMAND_PIPE_NAME: &str = "flowtilewm-ipc-v1";
pub const EVENT_STREAM_PIPE_NAME: &str = "flowtilewm-event-stream-v1";
pub const COMMANDS: &[&str] = &[
    "get_outputs",
    "get_workspaces",
    "get_windows",
    "get_focus",
    "focus_next",
    "focus_prev",
    "focus_workspace_up",
    "focus_workspace_down",
    "scroll_strip_left",
    "scroll_strip_right",
    "move_workspace_up",
    "move_workspace_down",
    "move_workspace_to_monitor_next",
    "move_workspace_to_monitor_previous",
    "move_column_to_workspace_up",
    "move_column_to_workspace_down",
    "cycle_column_width",
    "move_window",
    "consume_window",
    "expel_window",
    "toggle_floating",
    "toggle_tabbed",
    "toggle_maximized",
    "toggle_fullscreen",
    "open_overview",
    "close_overview",
    "toggle_overview",
    "touchpad_gesture",
    "capture_action",
    "reload_config",
    "dump_diagnostics",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpcBootstrap {
    pub transport: &'static str,
    pub protocol_version: u32,
    pub commands: Vec<&'static str>,
    pub command_pipe_name: &'static str,
    pub event_stream_pipe_name: &'static str,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IpcRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub command: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IpcError {
    pub code: String,
    pub message: String,
    pub category: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IpcResponse {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IpcEvent {
    pub protocol_version: u32,
    pub stream_version: u64,
    pub event_id: String,
    pub event_kind: String,
    pub state_version: u64,
    pub payload: Value,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotProjection {
    pub version_line: String,
    pub runtime_mode: String,
    pub state_version: u64,
    pub outputs: Vec<OutputProjection>,
    pub workspaces: Vec<WorkspaceProjection>,
    pub windows: Vec<WindowProjection>,
    pub focus: FocusProjection,
    pub overview: OverviewProjection,
    pub diagnostics: DiagnosticsProjection,
    pub config: ConfigProjection,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RectProjection {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputProjection {
    pub monitor_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub dpi: u32,
    pub is_primary: bool,
    pub work_area: RectProjection,
    pub workspace_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_workspace_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceProjection {
    pub workspace_id: u64,
    pub monitor_id: u64,
    pub vertical_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub is_active: bool,
    pub is_empty: bool,
    pub is_tail: bool,
    pub scroll_offset: i32,
    pub column_count: usize,
    pub tiled_window_count: usize,
    pub floating_window_count: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowProjection {
    pub window_id: u64,
    pub monitor_id: u64,
    pub workspace_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwnd: Option<u64>,
    pub title: String,
    pub class_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    pub layer: String,
    pub classification: String,
    pub is_managed: bool,
    pub is_focused: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FocusProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u64>,
    pub origin: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverviewProjection {
    pub is_open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_workspace_id: Option<u64>,
    pub projection_version: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticsProjection {
    pub total_records: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_label: Option<String>,
    pub degraded_flags: Vec<String>,
    pub management_enabled: bool,
    pub touchpad_override_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touchpad_override_detail: Option<String>,
    #[serde(default)]
    pub perf: PerfTelemetrySnapshot,
    #[serde(default)]
    pub surrogate_presentation: SurrogatePresentationDiagnosticsProjection,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SurrogatePresentationDiagnosticsProjection {
    pub active_hosts: usize,
    pub show_requests: u64,
    pub hide_requests: u64,
    pub classifier_rejections: u64,
    pub native_fallbacks: u64,
    pub transient_escapes: u64,
    pub handoff_promotions: u64,
    pub pointer_replay_attempts: u64,
    pub pointer_replay_successes: u64,
    pub pointer_replay_failures: u64,
    pub dwm_thumbnail_backend_uses: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigProjection {
    pub config_version: u64,
    pub source_path: String,
    pub bind_control_mode: String,
    pub touchpad_override_enabled: bool,
    pub touchpad_gesture_count: usize,
    pub active_rule_count: usize,
    pub strip_scroll_step: u32,
    pub default_column_mode: String,
    pub outer_padding: InsetsProjection,
    pub column_gap: u32,
    pub window_gap: u32,
    pub floating_margin: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct InsetsProjection {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl IpcRequest {
    pub fn new(request_id: impl Into<String>, command: impl Into<String>, payload: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            command: command.into(),
            payload,
        }
    }
}

impl IpcError {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        category: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            category: category.into(),
            retryable,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl IpcResponse {
    pub fn ok(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(request_id: impl Into<String>, error: IpcError) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

pub fn bootstrap() -> IpcBootstrap {
    IpcBootstrap {
        transport: TRANSPORT,
        protocol_version: PROTOCOL_VERSION,
        commands: COMMANDS.to_vec(),
        command_pipe_name: COMMAND_PIPE_NAME,
        event_stream_pipe_name: EVENT_STREAM_PIPE_NAME,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        COMMAND_PIPE_NAME, EVENT_STREAM_PIPE_NAME, IpcRequest, IpcResponse, PROTOCOL_VERSION,
        TRANSPORT, bootstrap,
    };

    #[test]
    fn exposes_bootstrap_transport_and_version() {
        let bootstrap = bootstrap();
        assert_eq!(bootstrap.transport, TRANSPORT);
        assert_eq!(bootstrap.protocol_version, PROTOCOL_VERSION);
        assert_eq!(bootstrap.command_pipe_name, COMMAND_PIPE_NAME);
        assert_eq!(bootstrap.event_stream_pipe_name, EVENT_STREAM_PIPE_NAME);
        assert!(bootstrap.commands.contains(&"open_overview"));
        assert!(bootstrap.commands.contains(&"close_overview"));
        assert!(bootstrap.commands.contains(&"toggle_overview"));
        assert!(bootstrap.commands.contains(&"touchpad_gesture"));
    }

    #[test]
    fn builds_request_and_response_envelopes() {
        let request = IpcRequest::new("r-1", "get_focus", json!({}));
        let response = IpcResponse::ok("r-1", json!({ "accepted": true }));

        assert_eq!(request.protocol_version, PROTOCOL_VERSION);
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        assert!(response.ok);
    }
}
