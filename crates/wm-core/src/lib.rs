#![forbid(unsafe_code)]

use std::{collections::HashMap, sync::Arc, time::Instant};

use flowtile_config_rules::{LoadedConfig, bootstrap as config_bootstrap};
use flowtile_diagnostics::{
    AtomicPerfMetric, DiagnosticRecord, PerfTelemetrySnapshot, bootstrap as diagnostics_bootstrap,
};
use flowtile_domain::{
    BootstrapProfile, ColumnId, ColumnMode, MonitorId, RuntimeMode, StateVersion, WidthSemantics,
    WindowId, WmState, WorkspaceId,
};
use flowtile_ipc::bootstrap as ipc_bootstrap;
use flowtile_layout_engine::{
    LayoutError, WorkspaceLayoutProjection, bootstrap_modes, preserves_insert_invariant,
};
use flowtile_windows_adapter::{
    PlatformPresentationPreflight, PlatformSnapshot, PlatformWindowDisposition, WindowsAdapter,
    WindowsAdapterError, bootstrap as windows_bootstrap,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreDaemonBootstrap {
    pub profile: BootstrapProfile,
    pub config_path: &'static str,
    pub ipc_command_count: usize,
    pub adapter_discovery_api: &'static str,
    pub diagnostics_channel_count: usize,
    pub layout_modes: [ColumnMode; 4],
}

impl CoreDaemonBootstrap {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        let config = config_bootstrap();
        let diagnostics = diagnostics_bootstrap();
        let ipc = ipc_bootstrap();
        let adapter = windows_bootstrap();
        let state = WmState::new(runtime_mode);

        Self {
            profile: state.bootstrap_profile(),
            config_path: config.default_path,
            ipc_command_count: ipc.commands.len(),
            adapter_discovery_api: adapter.discovery_api,
            diagnostics_channel_count: diagnostics.channels.len(),
            layout_modes: bootstrap_modes(),
        }
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let modes = self
            .layout_modes
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        vec![
            format!("version line: {}", self.profile.version_line),
            format!("runtime mode: {}", self.profile.runtime_mode),
            format!("state version: {}", self.profile.state_version.get()),
            format!("config path: {}", self.config_path),
            format!("layout modes prepared: {modes}"),
            format!(
                "insert invariant visible in bootstrap: {}",
                preserves_insert_invariant()
            ),
            format!(
                "windows adapter discovery API: {}",
                self.adapter_discovery_api
            ),
            format!("ipc commands prepared: {}", self.ipc_command_count),
            format!(
                "diagnostics channels prepared: {}",
                self.diagnostics_channel_count
            ),
        ]
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum CoreError {
    UnknownMonitor(MonitorId),
    UnknownWorkspace(WorkspaceId),
    UnknownColumn(ColumnId),
    UnknownWindow(WindowId),
    NoActiveWorkspace(MonitorId),
    InvalidEvent(&'static str),
    Layout(LayoutError),
}

impl From<LayoutError> for CoreError {
    fn from(value: LayoutError) -> Self {
        Self::Layout(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionResult {
    pub state_version: StateVersion,
    pub affected_workspace_id: Option<WorkspaceId>,
    pub layout_projection: Option<WorkspaceLayoutProjection>,
    pub diagnostics: Vec<DiagnosticRecord>,
}

#[derive(Debug)]
pub enum RuntimeError {
    Adapter(WindowsAdapterError),
    Core(CoreError),
    Config(String),
    NoPlatformMonitors,
}

impl From<WindowsAdapterError> for RuntimeError {
    fn from(value: WindowsAdapterError) -> Self {
        Self::Adapter(value)
    }
}

impl From<CoreError> for RuntimeError {
    fn from(value: CoreError) -> Self {
        Self::Core(value)
    }
}

impl From<LayoutError> for RuntimeError {
    fn from(value: LayoutError) -> Self {
        Self::Core(CoreError::from(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCycleReport {
    pub monitor_count: usize,
    pub observed_window_count: usize,
    pub discovered_windows: usize,
    pub destroyed_windows: usize,
    pub focused_hwnd: Option<u64>,
    pub observation_reason: Option<String>,
    pub planned_operations: usize,
    pub applied_operations: usize,
    pub apply_failures: usize,
    pub apply_failure_messages: Vec<String>,
    pub recovery_rescans: usize,
    pub validation_remaining_operations: usize,
    pub recovery_actions: Vec<String>,
    pub management_enabled: bool,
    pub dry_run: bool,
    pub degraded_reasons: Vec<String>,
    pub discovery_trace_logs: Vec<String>,
    pub strip_movement_logs: Vec<String>,
    pub window_trace_logs: Vec<String>,
    pub validation_trace_logs: Vec<String>,
}

impl RuntimeCycleReport {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("monitors observed: {}", self.monitor_count),
            format!("windows observed: {}", self.observed_window_count),
            format!("windows discovered: {}", self.discovered_windows),
            format!("windows destroyed: {}", self.destroyed_windows),
            format!("platform operations planned: {}", self.planned_operations),
            format!("platform operations applied: {}", self.applied_operations),
            format!("platform apply failures: {}", self.apply_failures),
            format!("recovery rescans: {}", self.recovery_rescans),
            format!(
                "validation operations remaining: {}",
                self.validation_remaining_operations
            ),
            format!("management enabled: {}", self.management_enabled),
            format!("dry run: {}", self.dry_run),
        ];

        if let Some(reason) = &self.observation_reason {
            lines.push(format!("observation reason: {reason}"));
        }
        if !self.apply_failure_messages.is_empty() {
            lines.push(format!(
                "apply failure messages: {}",
                self.apply_failure_messages.join(" | ")
            ));
        }
        if !self.recovery_actions.is_empty() {
            lines.push(format!(
                "recovery actions: {}",
                self.recovery_actions.join(", ")
            ));
        }
        if !self.degraded_reasons.is_empty() {
            lines.push(format!(
                "degraded reasons: {}",
                self.degraded_reasons.join(", ")
            ));
        }
        if !self.discovery_trace_logs.is_empty() {
            lines.push(format!(
                "discovery trace entries: {}",
                self.discovery_trace_logs.len()
            ));
            lines.extend(self.discovery_trace_logs.iter().cloned());
        }
        if !self.strip_movement_logs.is_empty() {
            lines.push(format!(
                "strip movements: {}",
                self.strip_movement_logs.len()
            ));
            lines.extend(self.strip_movement_logs.iter().cloned());
        }
        if !self.window_trace_logs.is_empty() {
            lines.push(format!(
                "window trace entries: {}",
                self.window_trace_logs.len()
            ));
            lines.extend(self.window_trace_logs.iter().cloned());
        }
        if !self.validation_trace_logs.is_empty() {
            lines.push(format!(
                "validation trace entries: {}",
                self.validation_trace_logs.len()
            ));
            lines.extend(self.validation_trace_logs.iter().cloned());
        }

        lines
    }
}

#[derive(Debug, Default)]
struct RuntimePerfTelemetry {
    command_cycle: AtomicPerfMetric,
    observation_sync: AtomicPerfMetric,
    post_apply_validation: AtomicPerfMetric,
    config_reload: AtomicPerfMetric,
}

impl RuntimePerfTelemetry {
    fn snapshot(&self) -> PerfTelemetrySnapshot {
        PerfTelemetrySnapshot {
            metrics: vec![
                self.command_cycle.snapshot("runtime.command-cycle"),
                self.observation_sync.snapshot("runtime.observation-sync"),
                self.post_apply_validation
                    .snapshot("runtime.post-apply-validation"),
                self.config_reload.snapshot("runtime.config-reload"),
            ],
        }
    }
}

#[derive(Clone, Debug)]
pub struct CoreDaemonRuntime {
    store: StateStore,
    adapter: WindowsAdapter,
    perf: Arc<RuntimePerfTelemetry>,
    active_config: LoadedConfig,
    last_valid_config: LoadedConfig,
    last_snapshot: Option<PlatformSnapshot>,
    pending_discoveries: HashMap<u64, PendingDiscoveryEntry>,
    pending_focus_claim: Option<PendingFocusClaim>,
    pending_platform_focus_candidate: Option<PendingPlatformFocusCandidate>,
    pending_geometry_settle_until: Option<Instant>,
    management_enabled: bool,
    consecutive_desync_cycles: u32,
    next_correlation_id: u64,
    next_config_generation: u64,
}

#[derive(Clone, Debug)]
struct PendingFocusClaim {
    desired_hwnd: u64,
    expires_at: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingPlatformFocusCandidate {
    observed_hwnd: u64,
    stable_snapshots: u8,
}

#[derive(Clone, Debug)]
struct PendingDiscoveryEntry {
    first_seen_at: Instant,
    last_seen_at: Instant,
    stable_ticks: u8,
    last_rect: flowtile_domain::Rect,
    family_key: String,
    disposition: PlatformWindowDisposition,
    disposition_reason: String,
    presentation_preflight: PlatformPresentationPreflight,
    presentation_reason: String,
}

#[derive(Clone, Debug)]
pub struct StateStore {
    state: WmState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NewColumnRequest {
    anchor_column_id: Option<ColumnId>,
    insert_index_override: Option<usize>,
    before_anchor: bool,
    mode: ColumnMode,
    width_semantics: WidthSemantics,
    preserve_focus_position: bool,
}

mod runtime;
mod state_store;

pub use runtime::{ActiveTiledResizeTarget, WindowPresentationProjection};

#[cfg(test)]
mod tests;
