#![forbid(unsafe_code)]

use flowtile_config_rules::{
    HotkeyBinding, LoadedConfig, WindowRuleInput, bootstrap as config_bootstrap, classify_window,
    default_loaded_config, ensure_default_config, load_from_path, load_or_default,
};
use flowtile_diagnostics::{
    DiagnosticRecord, bootstrap as diagnostics_bootstrap, layout_recomputed, transition_applied,
};
use flowtile_domain::{
    BindControlMode, BootstrapProfile, Column, ColumnId, ColumnMode, CorrelationId, DomainEvent,
    DomainEventPayload, FocusBehavior, FocusOrigin, MaximizedState, MonitorId, NavigationScope,
    RestoreTarget, RuntimeMode, StateVersion, TopologyRole, WidthSemantics, WindowId, WindowLayer,
    WindowNode, WindowPlacement, WmState, WorkspaceId,
};
use flowtile_ipc::bootstrap as ipc_bootstrap;
use flowtile_layout_engine::{
    LayoutError, WorkspaceLayoutProjection, bootstrap_modes, preserves_insert_invariant,
    recompute_workspace,
};
use flowtile_windows_adapter::{
    ApplyBatchResult, ApplyOperation, ObservationEnvelope, ObservationKind,
    PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot, SnapshotDiff,
    WindowsAdapter, WindowsAdapterError, bootstrap as windows_bootstrap, diff_snapshots,
    needs_activation_apply, needs_geometry_apply,
};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
    pub recovery_rescans: usize,
    pub validation_remaining_operations: usize,
    pub recovery_actions: Vec<String>,
    pub management_enabled: bool,
    pub dry_run: bool,
    pub degraded_reasons: Vec<String>,
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

        lines
    }
}

#[derive(Clone, Debug)]
pub struct CoreDaemonRuntime {
    store: StateStore,
    adapter: WindowsAdapter,
    active_config: LoadedConfig,
    last_valid_config: LoadedConfig,
    last_snapshot: Option<PlatformSnapshot>,
    management_enabled: bool,
    consecutive_desync_cycles: u32,
    next_correlation_id: u64,
    next_config_generation: u64,
}

#[derive(Clone, Debug)]
pub struct StateStore {
    state: WmState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NewColumnRequest {
    anchor_column_id: Option<ColumnId>,
    before_anchor: bool,
    mode: ColumnMode,
    width_semantics: WidthSemantics,
    preserve_focus_position: bool,
}

impl StateStore {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            state: WmState::new(runtime_mode),
        }
    }

    pub const fn state(&self) -> &WmState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut WmState {
        &mut self.state
    }

    pub fn dispatch(&mut self, event: DomainEvent) -> Result<TransitionResult, CoreError> {
        let affected_workspace_id = self.apply_event(&event)?;
        let state_version = self.state.bump_state_version();
        let mut diagnostics = vec![transition_applied(
            state_version,
            event.correlation_id,
            event.event_id.as_str(),
        )];
        let layout_projection = if let Some(workspace_id) = affected_workspace_id {
            let projection = recompute_workspace(&self.state, workspace_id)?;
            diagnostics.push(layout_recomputed(
                state_version,
                event.correlation_id,
                workspace_id,
                projection.window_geometries.len(),
            ));
            Some(projection)
        } else {
            None
        };

        self.state.diagnostics_summary.total_records += diagnostics.len() as u64;
        self.state.diagnostics_summary.last_transition_label =
            Some(event.event_id.as_str().to_string());
        self.state.diagnostics_summary.last_state_version = state_version;

        Ok(TransitionResult {
            state_version,
            affected_workspace_id,
            layout_projection,
            diagnostics,
        })
    }

    fn apply_event(&mut self, event: &DomainEvent) -> Result<Option<WorkspaceId>, CoreError> {
        match &event.payload {
            DomainEventPayload::WindowDiscovered(payload) => self.handle_window_discovered(payload),
            DomainEventPayload::WindowDestroyed(payload) => self.handle_window_destroyed(payload),
            DomainEventPayload::WindowFocusObserved(payload) => {
                self.handle_window_focus_observed(payload)
            }
            DomainEventPayload::CmdFocusNext(payload) => {
                self.handle_focus_navigation(true, payload)
            }
            DomainEventPayload::CmdFocusPrev(payload) => {
                self.handle_focus_navigation(false, payload)
            }
            DomainEventPayload::CmdScrollStripLeft(payload) => {
                self.handle_strip_scroll(-1, payload)
            }
            DomainEventPayload::CmdScrollStripRight(payload) => {
                self.handle_strip_scroll(1, payload)
            }
            DomainEventPayload::CmdToggleFloating(payload) => self.handle_toggle_floating(payload),
            DomainEventPayload::CmdToggleTabbed(payload) => self.handle_toggle_tabbed(payload),
            DomainEventPayload::CmdToggleMaximized(payload) => {
                self.handle_toggle_maximized(payload)
            }
            DomainEventPayload::CmdToggleFullscreen(payload) => {
                self.handle_toggle_fullscreen(payload)
            }
            DomainEventPayload::CmdToggleOverview(payload) => self.handle_toggle_overview(payload),
            DomainEventPayload::ConfigReloadRequested(_) => Ok(None),
            DomainEventPayload::ConfigReloadSucceeded(payload) => {
                self.handle_config_reload_succeeded(payload)
            }
            DomainEventPayload::ConfigReloadFailed(_) => Ok(None),
            DomainEventPayload::RulesUpdated(payload) => self.handle_rules_updated(payload),
            DomainEventPayload::ReconcileRequested => Ok(None),
            DomainEventPayload::FullScanRequested => Ok(None),
        }
    }

    fn handle_window_discovered(
        &mut self,
        payload: &flowtile_domain::WindowDiscoveredPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        if !self.state.monitors.contains_key(&payload.monitor_id) {
            return Err(CoreError::UnknownMonitor(payload.monitor_id));
        }

        let workspace_id = self
            .state
            .active_workspace_id_for_monitor(payload.monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(payload.monitor_id))?;
        let should_preserve_current_focus =
            matches!(payload.focus_behavior, FocusBehavior::PreserveCurrentFocus)
                && self.focused_window_in_workspace(workspace_id).is_some();
        let focused_column_id = self.focused_column_in_workspace(workspace_id);
        let window_id = self.state.allocate_window_id();

        let target_column_id = match payload.layer {
            WindowLayer::Floating | WindowLayer::Fullscreen => {
                self.push_window_to_floating_layer(workspace_id, window_id)?;
                None
            }
            _ => Some(match payload.placement {
                WindowPlacement::AppendToFocusedColumn => {
                    if let Some(column_id) = focused_column_id {
                        let column = self
                            .state
                            .layout
                            .columns
                            .get_mut(&column_id)
                            .ok_or(CoreError::UnknownColumn(column_id))?;
                        column.ordered_window_ids.push(window_id);
                        if column.active_window_id.is_none() {
                            column.active_window_id = Some(window_id);
                        }
                        if column.mode == ColumnMode::Tabbed {
                            column.tab_selection = Some(window_id);
                        }
                        column_id
                    } else {
                        self.insert_new_column(
                            workspace_id,
                            window_id,
                            NewColumnRequest {
                                anchor_column_id: None,
                                before_anchor: false,
                                mode: ColumnMode::Normal,
                                width_semantics: WidthSemantics::default(),
                                preserve_focus_position: false,
                            },
                        )?
                    }
                }
                WindowPlacement::NewColumnAfterFocus { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: focused_column_id,
                        before_anchor: false,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: false,
                    },
                )?,
                WindowPlacement::NewColumnBeforeFocus { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: focused_column_id,
                        before_anchor: true,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: should_preserve_current_focus,
                    },
                )?,
                WindowPlacement::AppendToWorkspaceEnd { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: None,
                        before_anchor: false,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: false,
                    },
                )?,
            }),
        };

        self.state.windows.insert(
            window_id,
            WindowNode {
                id: window_id,
                current_hwnd_binding: Some(payload.hwnd),
                classification: payload.classification,
                layer: payload.layer,
                workspace_id,
                column_id: target_column_id,
                is_managed: payload.managed,
                is_floating: payload.layer == WindowLayer::Floating,
                is_fullscreen: payload.layer == WindowLayer::Fullscreen,
                restore_target: None,
                last_known_rect: payload.last_known_rect,
                desired_size: payload.desired_size,
            },
        );

        self.state
            .focus
            .active_workspace_by_monitor
            .insert(payload.monitor_id, workspace_id);

        if let Some(workspace_set_id) = self.state.workspace_set_id_for_monitor(payload.monitor_id)
            && let Some(workspace_set) = self.state.workspace_sets.get_mut(&workspace_set_id)
        {
            workspace_set.active_workspace_id = workspace_id;
        }

        let previous_column_id = focused_column_id;
        if !should_preserve_current_focus {
            self.set_focus_to_window(
                payload.monitor_id,
                workspace_id,
                window_id,
                target_column_id,
                FocusOrigin::ReducerDefault,
            )?;
        }

        self.state.ensure_tail_workspace(payload.monitor_id);
        if !should_preserve_current_focus {
            self.clamp_scroll_offset(workspace_id)?;
            if let Some(column_id) = target_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        }
        Ok(Some(workspace_id))
    }

    fn handle_window_destroyed(
        &mut self,
        payload: &flowtile_domain::WindowDestroyedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window = self
            .state
            .windows
            .get(&payload.window_id)
            .ok_or(CoreError::UnknownWindow(payload.window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        self.detach_window_membership(payload.window_id)?;
        self.state.windows.remove(&payload.window_id);

        if self.state.focus.focused_window_id == Some(payload.window_id) {
            self.retarget_focus_after_destroy(workspace_id, window.column_id)?;
        }

        self.state.ensure_tail_workspace(monitor_id);
        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }

    fn handle_window_focus_observed(
        &mut self,
        payload: &flowtile_domain::WindowFocusObservedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        if !self.state.monitors.contains_key(&payload.monitor_id) {
            return Err(CoreError::UnknownMonitor(payload.monitor_id));
        }

        let window = self
            .state
            .windows
            .get(&payload.window_id)
            .ok_or(CoreError::UnknownWindow(payload.window_id))?;
        let workspace_id = window.workspace_id;
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if workspace.monitor_id != payload.monitor_id {
            return Err(CoreError::InvalidEvent(
                "focused window monitor does not match workspace monitor",
            ));
        }

        let focused_column_id = window.column_id;
        let previous_column_id = self.focused_column_in_workspace(workspace_id);
        self.set_focus_to_window(
            payload.monitor_id,
            workspace_id,
            payload.window_id,
            focused_column_id,
            payload.focus_origin,
        )?;
        if let Some(column_id) = focused_column_id {
            self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
        }

        Ok(Some(workspace_id))
    }

    fn handle_focus_navigation(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::FocusCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self.active_workspace_id_for_commands()?;

        if matches!(payload.scope, NavigationScope::ColumnTabs)
            && let Some(column_id) = self.focused_column_in_workspace(workspace_id)
            && self.try_cycle_tabbed_focus(workspace_id, column_id, forward)?
        {
            return Ok(Some(workspace_id));
        }

        let sequence = self.navigation_sequence_for_workspace(workspace_id)?;
        if sequence.is_empty() {
            return Ok(Some(workspace_id));
        }

        let next_index = match self.focused_window_in_workspace(workspace_id) {
            Some(window_id) => {
                let current_index = sequence
                    .iter()
                    .position(|(candidate_window_id, _)| *candidate_window_id == window_id)
                    .unwrap_or(0);
                if forward {
                    (current_index + 1) % sequence.len()
                } else if current_index == 0 {
                    sequence.len() - 1
                } else {
                    current_index - 1
                }
            }
            None if forward => 0,
            None => sequence.len() - 1,
        };
        let (window_id, column_id) = sequence[next_index];
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        let previous_column_id = self.focused_column_in_workspace(workspace_id);
        self.set_focus_to_window(
            monitor_id,
            workspace_id,
            window_id,
            column_id,
            FocusOrigin::UserCommand,
        )?;
        if let Some(column_id) = column_id {
            self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
        }

        Ok(Some(workspace_id))
    }

    fn handle_strip_scroll(
        &mut self,
        direction: i32,
        payload: &flowtile_domain::StripScrollPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self.active_workspace_id_for_commands()?;
        let step = if payload.step == 0 {
            self.state.config_projection.strip_scroll_step
        } else {
            payload.step
        }
        .min(i32::MAX as u32) as i32;
        self.apply_scroll_delta(workspace_id, direction.saturating_mul(step))?;
        Ok(Some(workspace_id))
    }

    fn handle_toggle_floating(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if window.layer == WindowLayer::Floating {
            let restore_target = window.restore_target.clone().unwrap_or(RestoreTarget {
                workspace_id,
                column_id: self.focused_column_in_workspace(workspace_id),
                layer: WindowLayer::Tiled,
            });

            self.detach_window_membership(window_id)?;
            let restored_column_id = self.restore_window_to_target(
                window_id,
                restore_target.clone(),
                self.state.config_projection.default_column_mode,
                self.state.config_projection.default_column_width,
            )?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = restore_target.layer;
            window.column_id = restored_column_id;
            window.is_floating = false;
            window.is_fullscreen = false;
            window.restore_target = None;
            let previous_column_id = self.focused_column_in_workspace(workspace_id);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                restored_column_id,
                FocusOrigin::UserCommand,
            )?;
            if let Some(column_id) = restored_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        } else {
            let restore_target = RestoreTarget {
                workspace_id,
                column_id: window.column_id,
                layer: window.layer,
            };
            self.detach_window_membership(window_id)?;
            self.push_window_to_floating_layer(workspace_id, window_id)?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = WindowLayer::Floating;
            window.column_id = None;
            window.is_floating = true;
            window.is_fullscreen = false;
            window.restore_target = Some(restore_target);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                None,
                FocusOrigin::UserCommand,
            )?;
        }

        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }

    fn handle_toggle_tabbed(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let (workspace_id, column_id) = {
            let window = self
                .state
                .windows
                .get(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            let Some(column_id) = window.column_id else {
                return Ok(Some(window.workspace_id));
            };
            (window.workspace_id, column_id)
        };

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        if column.mode == ColumnMode::Tabbed {
            column.mode = ColumnMode::Normal;
            column.tab_selection = column.ordered_window_ids.first().copied();
        } else {
            column.mode = ColumnMode::Tabbed;
            column.tab_selection = Some(window_id);
        }
        column.active_window_id = Some(window_id);

        self.reveal_column_in_workspace(
            workspace_id,
            column_id,
            self.focused_column_in_workspace(workspace_id),
        )?;
        Ok(Some(workspace_id))
    }

    fn handle_toggle_maximized(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let (workspace_id, column_id) = {
            let window = self
                .state
                .windows
                .get(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            let Some(column_id) = window.column_id else {
                return Ok(Some(window.workspace_id));
            };
            (window.workspace_id, column_id)
        };

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        column.maximized_state = match column.maximized_state {
            MaximizedState::Normal => MaximizedState::Maximized,
            MaximizedState::Maximized => MaximizedState::Normal,
        };

        self.reveal_column_in_workspace(
            workspace_id,
            column_id,
            self.focused_column_in_workspace(workspace_id),
        )?;
        Ok(Some(workspace_id))
    }

    fn handle_toggle_fullscreen(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if window.layer == WindowLayer::Fullscreen {
            let restore_target = window.restore_target.clone().unwrap_or(RestoreTarget {
                workspace_id,
                column_id: self.focused_column_in_workspace(workspace_id),
                layer: WindowLayer::Tiled,
            });
            self.detach_window_membership(window_id)?;
            let restored_column_id = self.restore_window_to_target(
                window_id,
                restore_target.clone(),
                self.state.config_projection.default_column_mode,
                self.state.config_projection.default_column_width,
            )?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = restore_target.layer;
            window.column_id = restored_column_id;
            window.is_floating = restore_target.layer == WindowLayer::Floating;
            window.is_fullscreen = false;
            window.restore_target = None;
            let previous_column_id = self.focused_column_in_workspace(workspace_id);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                restored_column_id,
                FocusOrigin::UserCommand,
            )?;
            if let Some(column_id) = restored_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        } else {
            let restore_target = RestoreTarget {
                workspace_id,
                column_id: window.column_id,
                layer: window.layer,
            };
            self.detach_window_membership(window_id)?;
            self.push_window_to_floating_layer(workspace_id, window_id)?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = WindowLayer::Fullscreen;
            window.column_id = None;
            window.is_floating = false;
            window.is_fullscreen = true;
            window.restore_target = Some(restore_target);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                None,
                FocusOrigin::UserCommand,
            )?;
        }

        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }

    fn handle_toggle_overview(
        &mut self,
        payload: &flowtile_domain::OverviewCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = payload
            .monitor_id
            .or(self.state.focus.focused_monitor_id)
            .ok_or(CoreError::InvalidEvent(
                "overview command requires a monitor context",
            ))?;
        let workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;

        self.state.overview.is_open = !self.state.overview.is_open;
        self.state.overview.monitor_id = self.state.overview.is_open.then_some(monitor_id);
        self.state.overview.selection = self.state.overview.is_open.then_some(workspace_id);
        self.state.overview.projection_version =
            self.state.overview.projection_version.saturating_add(1);
        Ok(None)
    }

    fn handle_config_reload_succeeded(
        &mut self,
        payload: &flowtile_domain::ConfigReloadSucceededPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.state.config_projection = payload.projection.clone();
        Ok(None)
    }

    fn handle_rules_updated(
        &mut self,
        payload: &flowtile_domain::RulesUpdatedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.state.config_projection.active_rule_count = payload.active_rule_count;
        Ok(None)
    }

    fn insert_new_column(
        &mut self,
        workspace_id: WorkspaceId,
        window_id: WindowId,
        request: NewColumnRequest,
    ) -> Result<ColumnId, CoreError> {
        let column_id = self.state.allocate_column_id();
        self.state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                request.mode,
                request.width_semantics,
                vec![window_id],
            ),
        );

        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let insert_index = request
            .anchor_column_id
            .and_then(|anchor| {
                workspace
                    .strip
                    .ordered_column_ids
                    .iter()
                    .position(|column_id| *column_id == anchor)
                    .map(|index| {
                        if request.before_anchor {
                            index
                        } else {
                            index + 1
                        }
                    })
            })
            .unwrap_or({
                if request.before_anchor {
                    0
                } else {
                    workspace.strip.ordered_column_ids.len()
                }
            });

        workspace
            .strip
            .ordered_column_ids
            .insert(insert_index, column_id);

        if request.preserve_focus_position
            && request.before_anchor
            && request.anchor_column_id.is_some()
        {
            let width = request
                .width_semantics
                .resolve(workspace.strip.visible_region.width)
                .min(i32::MAX as u32) as i32;
            workspace.strip.scroll_offset = workspace.strip.scroll_offset.saturating_add(width);
        }

        Ok(column_id)
    }

    fn focused_window_in_workspace(&self, workspace_id: WorkspaceId) -> Option<WindowId> {
        self.state.focus.focused_window_id.filter(|window_id| {
            self.state
                .windows
                .get(window_id)
                .is_some_and(|window| window.workspace_id == workspace_id)
        })
    }

    fn focused_column_in_workspace(&self, workspace_id: WorkspaceId) -> Option<ColumnId> {
        let focused_column_id = self.state.focus.focused_column_id?;
        let workspace = self.state.workspaces.get(&workspace_id)?;
        workspace
            .strip
            .ordered_column_ids
            .contains(&focused_column_id)
            .then_some(focused_column_id)
    }

    fn column_active_window(&self, column: &Column) -> Option<WindowId> {
        if column.mode == ColumnMode::Tabbed {
            column
                .tab_selection
                .or(column.active_window_id)
                .or_else(|| column.ordered_window_ids.first().copied())
        } else {
            column
                .active_window_id
                .or_else(|| column.ordered_window_ids.first().copied())
        }
    }

    fn active_workspace_id_for_commands(&self) -> Result<WorkspaceId, CoreError> {
        if let Some(monitor_id) = self.state.focus.focused_monitor_id
            && let Some(workspace_id) = self.state.active_workspace_id_for_monitor(monitor_id)
        {
            return Ok(workspace_id);
        }

        self.state
            .workspace_sets
            .values()
            .next()
            .map(|workspace_set| workspace_set.active_workspace_id)
            .ok_or(CoreError::InvalidEvent(
                "no active workspace is available for command handling",
            ))
    }

    fn command_window_id(&self, requested: Option<WindowId>) -> Result<WindowId, CoreError> {
        requested
            .or(self.state.focus.focused_window_id)
            .ok_or(CoreError::InvalidEvent("command requires a target window"))
    }

    fn push_window_to_floating_layer(
        &mut self,
        workspace_id: WorkspaceId,
        window_id: WindowId,
    ) -> Result<(), CoreError> {
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        if !workspace
            .floating_layer
            .ordered_window_ids
            .contains(&window_id)
        {
            workspace.floating_layer.ordered_window_ids.push(window_id);
        }
        let z_hint = workspace.floating_layer.ordered_window_ids.len() as u32;
        workspace.floating_layer.z_hints.insert(window_id, z_hint);
        Ok(())
    }

    fn detach_window_membership(&mut self, window_id: WindowId) -> Result<(), CoreError> {
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();

        if let Some(column_id) = window.column_id {
            let mut column_is_empty = false;
            if let Some(column) = self.state.layout.columns.get_mut(&column_id) {
                column
                    .ordered_window_ids
                    .retain(|candidate_window_id| *candidate_window_id != window_id);
                if column.tab_selection == Some(window_id) {
                    column.tab_selection = column.ordered_window_ids.first().copied();
                }
                if column.active_window_id == Some(window_id) {
                    column.active_window_id = column
                        .tab_selection
                        .or_else(|| column.ordered_window_ids.first().copied());
                }
                column_is_empty = column.ordered_window_ids.is_empty();
            }

            if column_is_empty {
                self.state.layout.columns.remove(&column_id);
                let workspace = self
                    .state
                    .workspaces
                    .get_mut(&window.workspace_id)
                    .ok_or(CoreError::UnknownWorkspace(window.workspace_id))?;
                workspace
                    .strip
                    .ordered_column_ids
                    .retain(|candidate_column_id| *candidate_column_id != column_id);
            }
        } else {
            let workspace = self
                .state
                .workspaces
                .get_mut(&window.workspace_id)
                .ok_or(CoreError::UnknownWorkspace(window.workspace_id))?;
            workspace
                .floating_layer
                .ordered_window_ids
                .retain(|candidate_window_id| *candidate_window_id != window_id);
            workspace.floating_layer.z_hints.remove(&window_id);
        }

        Ok(())
    }

    fn restore_window_to_target(
        &mut self,
        window_id: WindowId,
        restore_target: RestoreTarget,
        fallback_mode: ColumnMode,
        fallback_width: WidthSemantics,
    ) -> Result<Option<ColumnId>, CoreError> {
        match restore_target.layer {
            WindowLayer::Floating => {
                self.push_window_to_floating_layer(restore_target.workspace_id, window_id)?;
                Ok(None)
            }
            _ => {
                if let Some(column_id) = restore_target.column_id {
                    let workspace_contains_column = self
                        .state
                        .workspaces
                        .get(&restore_target.workspace_id)
                        .is_some_and(|workspace| {
                            workspace.strip.ordered_column_ids.contains(&column_id)
                        });
                    if workspace_contains_column {
                        let column = self
                            .state
                            .layout
                            .columns
                            .get_mut(&column_id)
                            .ok_or(CoreError::UnknownColumn(column_id))?;
                        if !column.ordered_window_ids.contains(&window_id) {
                            column.ordered_window_ids.push(window_id);
                        }
                        if column.active_window_id.is_none() {
                            column.active_window_id = Some(window_id);
                        }
                        if column.mode == ColumnMode::Tabbed {
                            column.tab_selection = Some(window_id);
                        }
                        return Ok(Some(column_id));
                    }
                }

                let new_column_id = self.insert_new_column(
                    restore_target.workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: self
                            .focused_column_in_workspace(restore_target.workspace_id),
                        before_anchor: false,
                        mode: fallback_mode,
                        width_semantics: fallback_width,
                        preserve_focus_position: false,
                    },
                )?;
                Ok(Some(new_column_id))
            }
        }
    }

    fn navigation_sequence_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(WindowId, Option<ColumnId>)>, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let mut sequence = Vec::new();

        for column_id in &workspace.strip.ordered_column_ids {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            if let Some(window_id) = self.column_active_window(column) {
                sequence.push((window_id, Some(*column_id)));
            }
        }

        if sequence.is_empty() {
            sequence.extend(
                workspace
                    .floating_layer
                    .ordered_window_ids
                    .iter()
                    .copied()
                    .map(|window_id| (window_id, None)),
            );
        }

        Ok(sequence)
    }

    fn try_cycle_tabbed_focus(
        &mut self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
        forward: bool,
    ) -> Result<bool, CoreError> {
        let (ordered_window_ids, current_index) = {
            let column = self
                .state
                .layout
                .columns
                .get(&column_id)
                .ok_or(CoreError::UnknownColumn(column_id))?;
            if column.mode != ColumnMode::Tabbed || column.ordered_window_ids.len() < 2 {
                return Ok(false);
            }

            let current_window_id = column
                .tab_selection
                .or(column.active_window_id)
                .or(self.focused_window_in_workspace(workspace_id))
                .or_else(|| column.ordered_window_ids.first().copied())
                .ok_or(CoreError::InvalidEvent(
                    "tabbed column is missing a selected window",
                ))?;
            let current_index = column
                .ordered_window_ids
                .iter()
                .position(|candidate_window_id| *candidate_window_id == current_window_id)
                .unwrap_or(0);
            (column.ordered_window_ids.clone(), current_index)
        };

        let next_index = if forward {
            (current_index + 1) % ordered_window_ids.len()
        } else if current_index == 0 {
            ordered_window_ids.len() - 1
        } else {
            current_index - 1
        };
        let window_id = ordered_window_ids[next_index];
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        column.tab_selection = Some(window_id);
        self.set_focus_to_window(
            monitor_id,
            workspace_id,
            window_id,
            Some(column_id),
            FocusOrigin::UserCommand,
        )?;
        self.reveal_column_in_workspace(workspace_id, column_id, Some(column_id))?;
        Ok(true)
    }

    fn set_focus_to_window(
        &mut self,
        monitor_id: MonitorId,
        workspace_id: WorkspaceId,
        window_id: WindowId,
        column_id: Option<ColumnId>,
        origin: FocusOrigin,
    ) -> Result<(), CoreError> {
        self.state.focus.focused_monitor_id = Some(monitor_id);
        self.state.focus.focused_window_id = Some(window_id);
        self.state.focus.focused_column_id = column_id;
        self.state.focus.focus_origin = origin;
        self.state
            .focus
            .active_workspace_by_monitor
            .insert(monitor_id, workspace_id);

        if let Some(workspace_set_id) = self.state.workspace_set_id_for_monitor(monitor_id)
            && let Some(workspace_set) = self.state.workspace_sets.get_mut(&workspace_set_id)
        {
            workspace_set.active_workspace_id = workspace_id;
        }

        if let Some(column_id) = column_id
            && let Some(column) = self.state.layout.columns.get_mut(&column_id)
        {
            column.active_window_id = Some(window_id);
            if column.mode == ColumnMode::Tabbed {
                column.tab_selection = Some(window_id);
            }
        }

        Ok(())
    }

    fn apply_scroll_delta(
        &mut self,
        workspace_id: WorkspaceId,
        delta: i32,
    ) -> Result<(), CoreError> {
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        workspace.strip.scroll_offset = workspace
            .strip
            .scroll_offset
            .saturating_add(delta)
            .clamp(0, max_scroll_offset);
        Ok(())
    }

    fn clamp_scroll_offset(&mut self, workspace_id: WorkspaceId) -> Result<(), CoreError> {
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        workspace.strip.scroll_offset = workspace.strip.scroll_offset.clamp(0, max_scroll_offset);
        Ok(())
    }

    fn max_scroll_offset(&self, workspace_id: WorkspaceId) -> Result<i32, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let viewport_width = workspace.strip.visible_region.width.min(i32::MAX as u32) as i32;
        let content_width = self.workspace_content_width(workspace_id)?;
        Ok((content_width - viewport_width).max(0))
    }

    fn workspace_content_width(&self, workspace_id: WorkspaceId) -> Result<i32, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let monitor = self
            .state
            .monitors
            .get(&workspace.monitor_id)
            .ok_or(CoreError::UnknownMonitor(workspace.monitor_id))?;
        let mut total_width = 0_i32;

        for column_id in &workspace.strip.ordered_column_ids {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            let column_width = self
                .resolve_column_width(column, monitor.work_area_rect.width)
                .min(i32::MAX as u32) as i32;
            total_width = total_width.saturating_add(column_width);
        }

        Ok(total_width)
    }

    fn column_bounds_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        target_column_id: ColumnId,
    ) -> Result<Option<(i32, i32, i32)>, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let monitor = self
            .state
            .monitors
            .get(&workspace.monitor_id)
            .ok_or(CoreError::UnknownMonitor(workspace.monitor_id))?;
        let mut column_left = 0_i32;

        for column_id in &workspace.strip.ordered_column_ids {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            let column_width = self
                .resolve_column_width(column, monitor.work_area_rect.width)
                .min(i32::MAX as u32) as i32;
            let column_right = column_left.saturating_add(column_width);

            if *column_id == target_column_id {
                return Ok(Some((column_left, column_right, column_width)));
            }

            column_left = column_right;
        }

        Ok(None)
    }

    fn reveal_column_in_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        target_column_id: ColumnId,
        previous_column_id: Option<ColumnId>,
    ) -> Result<(), CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let viewport_width = workspace.strip.visible_region.width.min(i32::MAX as u32) as i32;
        let visible_left = workspace.strip.scroll_offset;
        let visible_right = visible_left.saturating_add(viewport_width);
        let is_single_column_workspace = workspace.strip.ordered_column_ids.len() == 1;
        let Some((column_left, column_right, column_width)) =
            self.column_bounds_in_workspace(workspace_id, target_column_id)?
        else {
            return Ok(());
        };
        let previous_bounds = previous_column_id
            .filter(|column_id| *column_id != target_column_id)
            .and_then(|column_id| {
                self.column_bounds_in_workspace(workspace_id, column_id)
                    .ok()
            })
            .flatten();
        let should_center_target = if column_width < viewport_width {
            if is_single_column_workspace {
                true
            } else {
                previous_bounds.is_some_and(|(previous_left, previous_right, _)| {
                    let reveal_span_left = previous_left.min(column_left);
                    let reveal_span_right = previous_right.max(column_right);
                    reveal_span_right.saturating_sub(reveal_span_left) > viewport_width
                })
            }
        } else {
            false
        };
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let desired_scroll_offset = if should_center_target {
            column_left
                .saturating_add(column_width / 2)
                .saturating_sub(viewport_width / 2)
                .clamp(0, max_scroll_offset)
        } else if column_width >= viewport_width || column_left < visible_left {
            column_left
        } else if column_right > visible_right {
            column_right.saturating_sub(viewport_width)
        } else {
            visible_left
        };
        let desired_scroll_offset = desired_scroll_offset.clamp(0, max_scroll_offset);
        self.apply_scroll_delta(
            workspace_id,
            desired_scroll_offset.saturating_sub(visible_left),
        )?;

        Ok(())
    }

    fn resolve_column_width(&self, column: &Column, monitor_width: u32) -> u32 {
        if column.maximized_state == MaximizedState::Maximized
            || column.mode == ColumnMode::MaximizedColumn
        {
            monitor_width.max(1)
        } else {
            column.width_semantics.resolve(monitor_width)
        }
    }

    fn retarget_focus_after_destroy(
        &mut self,
        workspace_id: WorkspaceId,
        preferred_column_id: Option<ColumnId>,
    ) -> Result<(), CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let monitor_id = workspace.monitor_id;
        let next_focus = preferred_column_id
            .and_then(|column_id| {
                let column = self.state.layout.columns.get(&column_id)?;
                self.column_active_window(column)
                    .map(|window_id| (window_id, Some(column_id)))
            })
            .or_else(|| {
                workspace
                    .strip
                    .ordered_column_ids
                    .iter()
                    .find_map(|column_id| {
                        if Some(*column_id) == preferred_column_id {
                            return None;
                        }
                        let column = self.state.layout.columns.get(column_id)?;
                        self.column_active_window(column)
                            .map(|window_id| (window_id, Some(*column_id)))
                    })
            })
            .or_else(|| {
                workspace
                    .floating_layer
                    .ordered_window_ids
                    .first()
                    .copied()
                    .map(|window_id| (window_id, None))
            });

        if let Some((window_id, column_id)) = next_focus {
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                column_id,
                FocusOrigin::ReducerDefault,
            )?;
            if let Some(column_id) = column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, preferred_column_id)?;
            }
        } else {
            self.state.focus.focused_monitor_id = Some(monitor_id);
            self.state.focus.focus_origin = FocusOrigin::ReducerDefault;
            self.state.focus.focused_window_id = None;
            self.state.focus.focused_column_id = None;
        }

        Ok(())
    }
}

impl CoreDaemonRuntime {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self::with_adapter(runtime_mode, WindowsAdapter::new())
    }

    pub fn with_adapter(runtime_mode: RuntimeMode, adapter: WindowsAdapter) -> Self {
        let config_path = workspace_path(config_bootstrap().default_path);
        let had_startup_config = config_path.exists();
        let startup_config = ensure_default_config(&config_path)
            .ok()
            .and_then(|path| load_or_default(&path, 1).ok())
            .unwrap_or_else(|| default_loaded_config(1, config_path.display().to_string()));
        let mut store = StateStore::new(runtime_mode);
        store.state_mut().config_projection = startup_config.projection.clone();

        let mut runtime = Self {
            store,
            adapter,
            active_config: startup_config.clone(),
            last_valid_config: startup_config,
            last_snapshot: None,
            management_enabled: runtime_mode != RuntimeMode::SafeMode,
            consecutive_desync_cycles: 0,
            next_correlation_id: 1,
            next_config_generation: 2,
        };

        if !had_startup_config {
            runtime.push_degraded_reason("config-bootstrap-fallback".to_string());
        }

        runtime
    }

    pub const fn state(&self) -> &WmState {
        self.store.state()
    }

    pub fn hotkeys(&self) -> &[HotkeyBinding] {
        &self.active_config.hotkeys
    }

    pub const fn bind_control_mode(&self) -> BindControlMode {
        self.active_config.projection.bind_control_mode
    }

    pub fn last_snapshot(&self) -> Option<&PlatformSnapshot> {
        self.last_snapshot.as_ref()
    }

    pub const fn management_enabled(&self) -> bool {
        self.management_enabled
    }

    pub fn request_emergency_unwind(&mut self, reason: &str) {
        self.management_enabled = false;
        self.push_degraded_reason(format!("emergency-unwind:{reason}"));
    }

    pub fn dispatch_command(
        &mut self,
        event: DomainEvent,
        dry_run: bool,
        reason: &str,
    ) -> Result<RuntimeCycleReport, RuntimeError> {
        let snapshot = self.adapter.scan_snapshot()?;
        let _ = self.sync_snapshot_with_reason(snapshot.clone(), true, "command-pre-sync")?;
        let transition = self.store.dispatch(event)?;
        let planned_operations = if self.management_enabled {
            self.plan_apply_operations(&snapshot)?
        } else {
            Vec::new()
        };
        let apply_result = if dry_run || !self.management_enabled {
            ApplyBatchResult::default()
        } else {
            self.adapter.apply_operations(&planned_operations)?
        };

        let now = unix_timestamp();
        self.store.state_mut().runtime.last_full_scan_at = Some(now);
        if transition.affected_workspace_id.is_some() || !planned_operations.is_empty() {
            self.store.state_mut().runtime.last_reconcile_at = Some(now);
        }
        self.last_snapshot = Some(snapshot.clone());

        let mut report = RuntimeCycleReport {
            monitor_count: snapshot.monitors.len(),
            observed_window_count: snapshot.windows.len(),
            discovered_windows: 0,
            destroyed_windows: 0,
            focused_hwnd: snapshot.focused_window().map(|window| window.hwnd),
            observation_reason: Some(reason.to_string()),
            planned_operations: planned_operations.len(),
            applied_operations: apply_result.applied,
            apply_failures: apply_result.failures.len(),
            recovery_rescans: 0,
            validation_remaining_operations: 0,
            recovery_actions: Vec::new(),
            management_enabled: self.management_enabled,
            dry_run,
            degraded_reasons: self.store.state().runtime.degraded_flags.clone(),
        };
        self.validate_after_apply(&mut report, dry_run)?;
        Ok(report)
    }

    pub fn reload_config(&mut self, dry_run: bool) -> Result<RuntimeCycleReport, RuntimeError> {
        let config_path = self.store.state().config_projection.source_path.clone();
        let correlation_id = self.next_correlation_id();
        let _ = self.store.dispatch(DomainEvent::config_reload_requested(
            correlation_id,
            flowtile_domain::EventSource::InputCommand,
            Some(config_path.clone()),
        ))?;

        match load_from_path(&config_path, self.next_config_generation) {
            Ok(loaded_config) => {
                ensure_supported_bind_control_mode(loaded_config.projection.bind_control_mode)?;
                let changed_sections = diff_config_sections(&self.active_config, &loaded_config);
                let rule_ids = loaded_config
                    .rules
                    .iter()
                    .map(|rule| rule.id.clone())
                    .collect::<Vec<_>>();
                self.active_config = loaded_config.clone();
                self.last_valid_config = loaded_config.clone();
                self.next_config_generation += 1;

                let reload_succeeded_correlation = self.next_correlation_id();
                self.store.dispatch(DomainEvent::config_reload_succeeded(
                    reload_succeeded_correlation,
                    loaded_config.projection.config_version,
                    changed_sections,
                    loaded_config.projection.clone(),
                ))?;
                let rules_updated_correlation = self.next_correlation_id();
                self.store.dispatch(DomainEvent::rules_updated(
                    rules_updated_correlation,
                    loaded_config.projection.config_version,
                    rule_ids,
                    loaded_config.projection.active_rule_count,
                ))?;

                let report_correlation = self.next_correlation_id();
                self.dispatch_command(
                    DomainEvent::config_reload_requested(
                        report_correlation,
                        flowtile_domain::EventSource::ConfigRules,
                        Some(config_path),
                    ),
                    dry_run,
                    "config-reload",
                )
            }
            Err(error) => {
                let failure_correlation = self.next_correlation_id();
                let _ = self.store.dispatch(DomainEvent::config_reload_failed(
                    failure_correlation,
                    "config-reload-failed",
                    error.to_string(),
                ));
                self.active_config = self.last_valid_config.clone();
                self.push_degraded_reason("config-reload-failed".to_string());
                Err(RuntimeError::Config(error.to_string()))
            }
        }
    }

    pub fn scan_and_sync(&mut self, dry_run: bool) -> Result<RuntimeCycleReport, RuntimeError> {
        let snapshot = self.adapter.scan_snapshot()?;
        let mut report = self.sync_snapshot_with_reason(snapshot, dry_run, "full-scan")?;
        self.validate_after_apply(&mut report, dry_run)?;
        Ok(report)
    }

    pub fn apply_observation(
        &mut self,
        observation: ObservationEnvelope,
        dry_run: bool,
    ) -> Result<Option<RuntimeCycleReport>, RuntimeError> {
        match observation.kind {
            ObservationKind::Snapshot => {
                let Some(snapshot) = observation.snapshot else {
                    self.push_degraded_reason(format!(
                        "observer-missing-snapshot:{}",
                        normalize_reason_token(&observation.reason)
                    ));
                    return Ok(None);
                };

                let mut report =
                    self.sync_snapshot_with_reason(snapshot, dry_run, &observation.reason)?;
                self.validate_after_apply(&mut report, dry_run)?;
                Ok(Some(report))
            }
            ObservationKind::Suspend => {
                self.push_degraded_reason("system-suspend".to_string());
                Ok(None)
            }
            ObservationKind::Resume => {
                self.push_degraded_reason("system-resume".to_string());
                let mut report = self.scan_and_sync(dry_run)?;
                report.observation_reason = Some(observation.reason);
                Ok(Some(report))
            }
            ObservationKind::Warning => {
                self.push_degraded_reason(format!(
                    "observer-warning:{}",
                    normalize_reason_token(&observation.reason)
                ));
                if let Some(message) = observation.message {
                    self.push_degraded_reason(format!(
                        "observer-detail:{}",
                        normalize_reason_token(&message)
                    ));
                }
                Ok(None)
            }
        }
    }

    pub fn sync_snapshot(
        &mut self,
        snapshot: PlatformSnapshot,
        dry_run: bool,
    ) -> Result<RuntimeCycleReport, RuntimeError> {
        self.sync_snapshot_with_reason(snapshot, dry_run, "external-snapshot")
    }

    fn sync_snapshot_with_reason(
        &mut self,
        mut snapshot: PlatformSnapshot,
        dry_run: bool,
        observation_reason: &str,
    ) -> Result<RuntimeCycleReport, RuntimeError> {
        snapshot.sort_for_stability();
        self.note_observation_reason(observation_reason);
        self.sync_monitors_from_snapshot(&snapshot.monitors)?;

        let had_previous_snapshot = self.last_snapshot.is_some();
        let diff = self
            .last_snapshot
            .as_ref()
            .map(|previous| diff_snapshots(previous, &snapshot))
            .unwrap_or_else(|| SnapshotDiff::initial(&snapshot));

        let discovered_windows = self.ingest_created_windows(&diff.created_windows)?;
        let destroyed_windows = self.ingest_destroyed_windows(&diff.destroyed_hwnds)?;

        if had_previous_snapshot && diff.monitor_topology_changed {
            self.push_degraded_reason("monitor-topology-changed".to_string());
        }

        if let Some(focused_hwnd) = diff.focused_hwnd {
            self.observe_focus(focused_hwnd)?;
        }

        let planned_operations = if self.management_enabled {
            self.plan_apply_operations(&snapshot)?
        } else {
            Vec::new()
        };
        let apply_result = if dry_run || !self.management_enabled {
            ApplyBatchResult::default()
        } else {
            self.adapter.apply_operations(&planned_operations)?
        };

        let now = unix_timestamp();
        self.store.state_mut().runtime.last_full_scan_at = Some(now);
        if !planned_operations.is_empty() {
            self.store.state_mut().runtime.last_reconcile_at = Some(now);
        }
        self.last_snapshot = Some(snapshot.clone());

        Ok(RuntimeCycleReport {
            monitor_count: snapshot.monitors.len(),
            observed_window_count: snapshot.windows.len(),
            discovered_windows,
            destroyed_windows,
            focused_hwnd: snapshot.focused_window().map(|window| window.hwnd),
            observation_reason: Some(observation_reason.to_string()),
            planned_operations: planned_operations.len(),
            applied_operations: apply_result.applied,
            apply_failures: apply_result.failures.len(),
            recovery_rescans: 0,
            validation_remaining_operations: 0,
            recovery_actions: Vec::new(),
            management_enabled: self.management_enabled,
            dry_run,
            degraded_reasons: self.store.state().runtime.degraded_flags.clone(),
        })
    }

    fn validate_after_apply(
        &mut self,
        report: &mut RuntimeCycleReport,
        dry_run: bool,
    ) -> Result<(), RuntimeError> {
        if dry_run || !self.management_enabled || report.planned_operations == 0 {
            return Ok(());
        }

        let validation_snapshot = self.adapter.scan_snapshot()?;
        report.recovery_rescans += 1;
        let remaining_operations = self.plan_apply_operations(&validation_snapshot)?;
        report.validation_remaining_operations = remaining_operations.len();

        if remaining_operations.is_empty() {
            self.consecutive_desync_cycles = 0;
            self.last_snapshot = Some(validation_snapshot);
            report
                .recovery_actions
                .push("post-apply-validation-clean".to_string());
            report.degraded_reasons = self.store.state().runtime.degraded_flags.clone();
            return Ok(());
        }

        self.push_degraded_reason("desync:post-apply-diverged".to_string());
        report.recovery_actions.push(format!(
            "targeted-retry:{}-ops-remain",
            remaining_operations.len()
        ));

        let retry_result = self.adapter.apply_operations(&remaining_operations)?;
        report.applied_operations += retry_result.applied;
        report.apply_failures += retry_result.failures.len();

        let post_retry_snapshot = self.adapter.scan_snapshot()?;
        report.recovery_rescans += 1;
        let post_retry_remaining = self.plan_apply_operations(&post_retry_snapshot)?;
        report.validation_remaining_operations = post_retry_remaining.len();
        self.last_snapshot = Some(post_retry_snapshot);

        if post_retry_remaining.is_empty() {
            self.consecutive_desync_cycles = 0;
            report.recovery_actions.push("retry-recovered".to_string());
        } else {
            self.consecutive_desync_cycles += 1;
            self.push_degraded_reason(format!(
                "desync:remaining-operations:{}",
                post_retry_remaining.len()
            ));
            report.recovery_actions.push(format!(
                "full-scan-escalation:{}-ops-still-diverged",
                post_retry_remaining.len()
            ));

            if report.apply_failures > 0 || self.consecutive_desync_cycles >= 2 {
                self.request_emergency_unwind("desync-recovery-escalated");
                report.recovery_actions.push("safe-mode-unwind".to_string());
            }
        }

        report.management_enabled = self.management_enabled;
        report.degraded_reasons = self.store.state().runtime.degraded_flags.clone();
        Ok(())
    }

    fn ingest_created_windows(
        &mut self,
        windows: &[PlatformWindowSnapshot],
    ) -> Result<usize, RuntimeError> {
        let mut discovered_windows = 0;

        for window in windows {
            if self.find_window_id_by_hwnd(window.hwnd).is_some() {
                continue;
            }

            let Some(monitor_id) = self.monitor_id_by_binding(&window.monitor_binding) else {
                self.push_degraded_reason(format!(
                    "missing-monitor-binding:{}",
                    window.monitor_binding
                ));
                continue;
            };

            let decision = classify_window(
                &self.active_config.rules,
                &WindowRuleInput {
                    process_name: window.process_name.clone(),
                    class_name: window.class_name.clone(),
                    title: window.title.clone(),
                },
                &self.active_config.projection,
            );
            let correlation_id = self.next_correlation_id();
            self.store.dispatch(DomainEvent::new(
                flowtile_domain::DomainEventName::WindowDiscovered,
                flowtile_domain::EventCategory::PlatformDerived,
                flowtile_domain::EventSource::WindowsAdapter,
                correlation_id,
                DomainEventPayload::WindowDiscovered(flowtile_domain::WindowDiscoveredPayload {
                    monitor_id,
                    hwnd: window.hwnd,
                    classification: if decision.layer == WindowLayer::Tiled {
                        flowtile_domain::WindowClassification::Application
                    } else {
                        flowtile_domain::WindowClassification::Utility
                    },
                    desired_size: flowtile_domain::Size::new(window.rect.width, window.rect.height),
                    last_known_rect: window.rect,
                    placement: WindowPlacement::AppendToWorkspaceEnd {
                        mode: decision.column_mode,
                        width: decision.width_semantics,
                    },
                    focus_behavior: FocusBehavior::PreserveCurrentFocus,
                    layer: decision.layer,
                    managed: decision.managed,
                }),
            ))?;
            discovered_windows += 1;
        }

        Ok(discovered_windows)
    }

    fn ingest_destroyed_windows(&mut self, hwnds: &[u64]) -> Result<usize, RuntimeError> {
        let mut destroyed_windows = 0;

        for hwnd in hwnds {
            let Some(window_id) = self.find_window_id_by_hwnd(*hwnd) else {
                continue;
            };

            let correlation_id = self.next_correlation_id();
            self.store
                .dispatch(DomainEvent::window_destroyed(correlation_id, window_id))?;
            destroyed_windows += 1;
        }

        Ok(destroyed_windows)
    }

    fn observe_focus(&mut self, hwnd: u64) -> Result<(), RuntimeError> {
        let Some(window_id) = self.find_window_id_by_hwnd(hwnd) else {
            return Ok(());
        };
        let Some(window) = self.store.state().windows.get(&window_id) else {
            return Ok(());
        };
        let workspace_id = window.workspace_id;
        let Some(workspace) = self.store.state().workspaces.get(&workspace_id) else {
            return Ok(());
        };
        let monitor_id = workspace.monitor_id;

        let correlation_id = self.next_correlation_id();
        self.store.dispatch(DomainEvent::window_focus_observed(
            correlation_id,
            monitor_id,
            window_id,
        ))?;
        Ok(())
    }

    fn plan_apply_operations(
        &self,
        snapshot: &PlatformSnapshot,
    ) -> Result<Vec<ApplyOperation>, RuntimeError> {
        let actual_windows = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window))
            .collect::<std::collections::HashMap<_, _>>();
        let desired_focused_hwnd = self
            .store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .filter(|window| window.is_managed)
            .and_then(|window| window.current_hwnd_binding);
        let actual_focused_hwnd = snapshot.focused_window().map(|window| window.hwnd);
        let mut operations = Vec::new();

        for workspace in self.store.state().workspaces.values() {
            if self.store.state().is_workspace_empty(workspace.id) {
                continue;
            }

            let projection = recompute_workspace(self.store.state(), workspace.id)?;
            for geometry in projection.window_geometries {
                let Some(window) = self.store.state().windows.get(&geometry.window_id) else {
                    continue;
                };
                if !window.is_managed {
                    continue;
                }
                let Some(hwnd) = window.current_hwnd_binding else {
                    continue;
                };
                let Some(actual_window) = actual_windows.get(&hwnd) else {
                    continue;
                };
                let activate = desired_focused_hwnd
                    .filter(|desired_hwnd| *desired_hwnd == hwnd)
                    .is_some_and(|desired_hwnd| {
                        needs_activation_apply(actual_focused_hwnd, desired_hwnd)
                    });
                if needs_geometry_apply(actual_window.rect, geometry.rect) || activate {
                    operations.push(ApplyOperation {
                        hwnd,
                        rect: geometry.rect,
                        activate,
                    });
                }
            }
        }

        Ok(operations)
    }

    fn sync_monitors_from_snapshot(
        &mut self,
        monitors: &[PlatformMonitorSnapshot],
    ) -> Result<(), RuntimeError> {
        if monitors.is_empty() {
            return Err(RuntimeError::NoPlatformMonitors);
        }

        let known_bindings = self
            .store
            .state()
            .monitors
            .values()
            .filter_map(|monitor| monitor.platform_binding.clone())
            .collect::<Vec<_>>();

        for monitor_snapshot in monitors {
            if let Some(monitor_id) = self.monitor_id_by_binding(&monitor_snapshot.binding) {
                let workspace_set_id = {
                    let state = self.store.state_mut();
                    let monitor = state
                        .monitors
                        .get_mut(&monitor_id)
                        .expect("known monitor should exist");
                    monitor.platform_binding = Some(monitor_snapshot.binding.clone());
                    monitor.work_area_rect = monitor_snapshot.work_area_rect;
                    monitor.dpi = monitor_snapshot.dpi;
                    monitor.is_primary_hint = monitor_snapshot.is_primary;
                    monitor.topology_role = if monitor_snapshot.is_primary {
                        TopologyRole::Primary
                    } else {
                        TopologyRole::Secondary
                    };
                    monitor.workspace_set_id
                };

                let workspace_ids = self
                    .store
                    .state()
                    .workspace_sets
                    .get(&workspace_set_id)
                    .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
                    .unwrap_or_default();

                for workspace_id in workspace_ids {
                    if let Some(workspace) =
                        self.store.state_mut().workspaces.get_mut(&workspace_id)
                    {
                        workspace.monitor_id = monitor_id;
                        workspace.strip.visible_region = monitor_snapshot.work_area_rect;
                    }
                }
            } else {
                let monitor_id = self.store.state_mut().add_monitor(
                    monitor_snapshot.work_area_rect,
                    monitor_snapshot.dpi,
                    monitor_snapshot.is_primary,
                );
                if let Some(monitor) = self.store.state_mut().monitors.get_mut(&monitor_id) {
                    monitor.platform_binding = Some(monitor_snapshot.binding.clone());
                }
            }
        }

        let fallback_monitor = monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .or_else(|| monitors.first())
            .cloned();

        for missing_binding in known_bindings
            .into_iter()
            .filter(|binding| !monitors.iter().any(|monitor| monitor.binding == *binding))
        {
            self.push_degraded_reason(format!("missing-monitor:{missing_binding}"));

            let Some(fallback_monitor) = &fallback_monitor else {
                continue;
            };
            let Some(monitor_id) = self.monitor_id_by_binding(&missing_binding) else {
                continue;
            };

            let workspace_set_id = {
                let state = self.store.state_mut();
                let monitor = state
                    .monitors
                    .get_mut(&monitor_id)
                    .expect("known monitor should exist");
                monitor.work_area_rect = fallback_monitor.work_area_rect;
                monitor.dpi = fallback_monitor.dpi;
                monitor.is_primary_hint = false;
                monitor.topology_role = TopologyRole::Secondary;
                monitor.workspace_set_id
            };

            let workspace_ids = self
                .store
                .state()
                .workspace_sets
                .get(&workspace_set_id)
                .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
                .unwrap_or_default();

            for workspace_id in workspace_ids {
                if let Some(workspace) = self.store.state_mut().workspaces.get_mut(&workspace_id) {
                    workspace.strip.visible_region = fallback_monitor.work_area_rect;
                }
            }
        }

        if let Some(primary_monitor) = monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .or_else(|| monitors.first())
            && let Some(monitor_id) = self.monitor_id_by_binding(&primary_monitor.binding)
            && self.store.state().focus.focused_monitor_id.is_none()
        {
            self.store.state_mut().focus.focused_monitor_id = Some(monitor_id);
        }

        Ok(())
    }

    fn monitor_id_by_binding(&self, binding: &str) -> Option<MonitorId> {
        self.store
            .state()
            .monitors
            .iter()
            .find_map(|(monitor_id, monitor)| {
                (monitor.platform_binding.as_deref() == Some(binding)).then_some(*monitor_id)
            })
    }

    fn find_window_id_by_hwnd(&self, hwnd: u64) -> Option<WindowId> {
        self.store
            .state()
            .windows
            .iter()
            .find_map(|(window_id, window)| {
                (window.current_hwnd_binding == Some(hwnd)).then_some(*window_id)
            })
    }

    fn push_degraded_reason(&mut self, reason: String) {
        if !self.store.state().runtime.degraded_flags.contains(&reason) {
            self.store.state_mut().runtime.degraded_flags.push(reason);
        }
    }

    fn note_observation_reason(&mut self, reason: &str) {
        let token = normalize_reason_token(reason);
        if token.contains("resume") {
            self.push_degraded_reason("resume-revalidation".to_string());
        }
        if token.contains("display") {
            self.push_degraded_reason("display-settings-changed".to_string());
        }
        if token.contains("monitor") {
            self.push_degraded_reason("monitor-topology-revalidation".to_string());
        }
    }

    fn next_correlation_id(&mut self) -> CorrelationId {
        let correlation_id = CorrelationId::new(self.next_correlation_id);
        self.next_correlation_id += 1;
        correlation_id
    }
}

fn diff_config_sections(previous: &LoadedConfig, current: &LoadedConfig) -> Vec<String> {
    let mut changed_sections = Vec::new();

    if previous.projection.strip_scroll_step != current.projection.strip_scroll_step
        || previous.projection.default_column_mode != current.projection.default_column_mode
        || previous.projection.default_column_width != current.projection.default_column_width
    {
        changed_sections.push("layout".to_string());
    }
    if previous.projection.bind_control_mode != current.projection.bind_control_mode
        || previous.hotkeys != current.hotkeys
    {
        changed_sections.push("input".to_string());
    }
    if previous.rules != current.rules {
        changed_sections.push("rules".to_string());
    }
    if changed_sections.is_empty() {
        changed_sections.push("none".to_string());
    }

    changed_sections
}

fn ensure_supported_bind_control_mode(
    bind_control_mode: BindControlMode,
) -> Result<(), RuntimeError> {
    match bind_control_mode {
        BindControlMode::Coexistence => Ok(()),
        _ => Err(RuntimeError::Config(format!(
            "bind control mode '{}' is not supported by the current runtime slice; only 'coexistence' is available",
            bind_control_mode.as_str()
        ))),
    }
}

fn workspace_path(relative_path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative_path)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn normalize_reason_token(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use flowtile_domain::{
        BindControlMode, ColumnMode, CorrelationId, DomainEvent, FocusBehavior, NavigationScope,
        Rect, RuntimeMode, Size, WidthSemantics, WindowPlacement,
    };
    use flowtile_windows_adapter::{
        ObservationEnvelope, ObservationKind, PlatformMonitorSnapshot, PlatformSnapshot,
        PlatformWindowSnapshot,
    };

    use super::{CoreDaemonBootstrap, CoreDaemonRuntime, RuntimeError, StateStore};

    #[test]
    fn builds_summary_without_product_logic() {
        let bootstrap = CoreDaemonBootstrap::new(RuntimeMode::ExtendedShell);
        let summary = bootstrap.summary_lines();
        assert!(summary.iter().any(|line| line.contains("extended-shell")));
        assert!(
            summary
                .iter()
                .any(|line| line.contains("ipc commands prepared"))
        );
    }

    #[test]
    fn discovery_creates_tail_workspace_and_diagnostics() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

        let result = store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(420, 900),
                Rect::new(0, 0, 420, 900),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(420),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("dispatch should succeed");

        let workspace_set_id = store
            .state()
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_set = store
            .state()
            .workspace_sets
            .get(&workspace_set_id)
            .expect("workspace set should exist");
        let tail_workspace_id = *workspace_set
            .ordered_workspace_ids
            .last()
            .expect("tail workspace should exist");

        assert_eq!(result.state_version.get(), 1);
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(store.state().workspaces.len(), 2);
        assert!(store.state().is_workspace_empty(tail_workspace_id));
        assert!(
            store
                .state()
                .workspaces
                .get(&tail_workspace_id)
                .expect("tail workspace should exist")
                .is_ephemeral_empty_tail
        );
    }

    #[test]
    fn inserting_new_column_does_not_resize_existing_column() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

        let first = store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(420, 900),
                Rect::new(0, 0, 420, 900),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(420),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("first dispatch should succeed");
        let first_window_id = store
            .state()
            .focus
            .focused_window_id
            .expect("first window should be focused");
        let first_rect_before = geometry_x_width(
            first
                .layout_projection
                .as_ref()
                .expect("layout should exist"),
            first_window_id,
        );

        let second = store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(2),
                monitor_id,
                101,
                Size::new(360, 900),
                Rect::new(420, 0, 360, 900),
                WindowPlacement::NewColumnAfterFocus {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(360),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("second dispatch should succeed");
        let first_rect_after = geometry_x_width(
            second
                .layout_projection
                .as_ref()
                .expect("layout should exist"),
            first_window_id,
        );

        assert_eq!(first_rect_before.1, first_rect_after.1);
    }

    #[test]
    fn inserting_before_focus_keeps_visual_position_stable() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(420, 900),
                Rect::new(0, 0, 420, 900),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(420),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("first dispatch should succeed");

        let second = store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(2),
                monitor_id,
                101,
                Size::new(360, 900),
                Rect::new(420, 0, 360, 900),
                WindowPlacement::NewColumnAfterFocus {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(360),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("second dispatch should succeed");
        let focused_window_id = store
            .state()
            .focus
            .focused_window_id
            .expect("second window should be focused");
        let focused_x_before = geometry_x_width(
            second
                .layout_projection
                .as_ref()
                .expect("layout should exist"),
            focused_window_id,
        )
        .0;

        let third = store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(3),
                monitor_id,
                102,
                Size::new(220, 900),
                Rect::new(0, 0, 220, 900),
                WindowPlacement::NewColumnBeforeFocus {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(220),
                },
                FocusBehavior::PreserveCurrentFocus,
            ))
            .expect("third dispatch should succeed");
        let focused_x_after = geometry_x_width(
            third
                .layout_projection
                .as_ref()
                .expect("layout should exist"),
            focused_window_id,
        )
        .0;

        assert_eq!(focused_x_before, focused_x_after);
        assert_eq!(
            store.state().focus.focused_window_id,
            Some(focused_window_id)
        );
        assert_eq!(
            third
                .layout_projection
                .as_ref()
                .expect("layout should exist")
                .scroll_offset,
            220
        );
    }

    #[test]
    fn destroying_last_window_collapses_extra_empty_tail() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(420, 900),
                Rect::new(0, 0, 420, 900),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(420),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("dispatch should succeed");
        let window_id = store
            .state()
            .focus
            .focused_window_id
            .expect("window should be focused");

        store
            .dispatch(DomainEvent::window_destroyed(
                CorrelationId::new(2),
                window_id,
            ))
            .expect("destroy should succeed");

        let workspace_set_id = store
            .state()
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_set = store
            .state()
            .workspace_sets
            .get(&workspace_set_id)
            .expect("workspace set should exist");
        let remaining_workspace_id = workspace_set.active_workspace_id;

        assert_eq!(workspace_set.ordered_workspace_ids.len(), 1);
        assert!(store.state().is_workspace_empty(remaining_workspace_id));
        assert_eq!(store.state().focus.focused_window_id, None);
    }

    #[test]
    fn sync_snapshot_discovers_windows_and_plans_dry_run_geometry() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        let report = runtime
            .sync_snapshot(
                sample_snapshot(100, Rect::new(200, 120, 420, 700), true),
                true,
            )
            .expect("sync should succeed");

        assert_eq!(report.monitor_count, 1);
        assert_eq!(report.observed_window_count, 1);
        assert_eq!(report.discovered_windows, 1);
        assert_eq!(report.destroyed_windows, 0);
        assert_eq!(report.focused_hwnd, Some(100));
        assert_eq!(report.planned_operations, 1);
        assert_eq!(report.applied_operations, 0);
        assert!(report.management_enabled);
        assert!(report.dry_run);
        assert_eq!(runtime.state().windows.len(), 1);
        assert!(runtime.state().focus.focused_window_id.is_some());
        assert!(runtime.state().runtime.last_full_scan_at.is_some());
        assert!(runtime.state().runtime.last_reconcile_at.is_some());
    }

    #[test]
    fn location_change_observation_plans_prompt_reassert() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        runtime
            .sync_snapshot(sample_snapshot(100, Rect::new(0, 0, 420, 900), true), true)
            .expect("initial sync should succeed");

        let report = runtime
            .apply_observation(
                ObservationEnvelope {
                    kind: ObservationKind::Snapshot,
                    reason: "win-event-location-change".to_string(),
                    snapshot: Some(sample_snapshot(100, Rect::new(760, 120, 420, 900), true)),
                    message: None,
                },
                true,
            )
            .expect("observation should succeed")
            .expect("snapshot observation should produce a cycle report");

        assert_eq!(report.planned_operations, 1);
        assert_eq!(
            report.observation_reason.as_deref(),
            Some("win-event-location-change")
        );
    }

    #[test]
    fn emergency_unwind_disables_management_before_next_sync() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        runtime.request_emergency_unwind("test-case");
        let report = runtime
            .sync_snapshot(
                sample_snapshot(200, Rect::new(300, 0, 360, 800), true),
                false,
            )
            .expect("sync should succeed");

        assert!(!runtime.management_enabled());
        assert!(!report.management_enabled);
        assert_eq!(report.planned_operations, 0);
        assert_eq!(report.applied_operations, 0);
        assert!(
            runtime
                .state()
                .runtime
                .degraded_flags
                .contains(&"emergency-unwind:test-case".to_string())
        );
    }

    #[test]
    fn warning_observation_marks_runtime_degraded_without_cycle_report() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        let report = runtime
            .apply_observation(
                ObservationEnvelope {
                    kind: ObservationKind::Warning,
                    reason: "observer-scan-failed".to_string(),
                    snapshot: None,
                    message: Some("transient failure".to_string()),
                },
                true,
            )
            .expect("warning observation should be accepted");

        assert!(report.is_none());
        assert!(
            runtime
                .state()
                .runtime
                .degraded_flags
                .contains(&"observer-warning:observer-scan-failed".to_string())
        );
    }

    #[test]
    fn focus_navigation_reveals_offscreen_column() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 900, 700), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(400, 700),
                Rect::new(0, 0, 400, 700),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(400),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("first dispatch should succeed");
        let first_window_id = store
            .state()
            .focus
            .focused_window_id
            .expect("first window should be focused");

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(2),
                monitor_id,
                101,
                Size::new(400, 700),
                Rect::new(400, 0, 400, 700),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(400),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("second dispatch should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(3),
                monitor_id,
                102,
                Size::new(400, 700),
                Rect::new(800, 0, 400, 700),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(400),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("third dispatch should succeed");

        store
            .dispatch(DomainEvent::window_focus_observed(
                CorrelationId::new(4),
                monitor_id,
                first_window_id,
            ))
            .expect("focus reset should succeed");

        store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(5),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus next should succeed");
        let result = store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(6),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("second focus next should succeed");

        assert_eq!(
            store.state().focus.focused_window_id.map(|id| id.get()),
            Some(3)
        );
        assert_eq!(
            result
                .layout_projection
                .as_ref()
                .expect("layout should exist")
                .scroll_offset,
            300
        );
    }

    #[test]
    fn strip_navigation_returns_to_remembered_active_window_of_column() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 600, 700), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(400, 350),
                Rect::new(0, 0, 400, 350),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(400),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("first window discovery should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(2),
                monitor_id,
                101,
                Size::new(400, 350),
                Rect::new(0, 350, 400, 350),
                WindowPlacement::AppendToFocusedColumn,
                FocusBehavior::FollowNewWindow,
            ))
            .expect("second window discovery should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(3),
                monitor_id,
                102,
                Size::new(400, 350),
                Rect::new(400, 0, 400, 350),
                WindowPlacement::NewColumnAfterFocus {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(400),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("third window discovery should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(4),
                monitor_id,
                103,
                Size::new(400, 350),
                Rect::new(400, 350, 400, 350),
                WindowPlacement::AppendToFocusedColumn,
                FocusBehavior::FollowNewWindow,
            ))
            .expect("fourth window discovery should succeed");

        let prev_result = store
            .dispatch(DomainEvent::focus_prev(
                CorrelationId::new(5),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus prev should succeed");
        assert_eq!(
            store.state().focus.focused_window_id.map(|id| id.get()),
            Some(2)
        );
        assert_eq!(
            prev_result
                .layout_projection
                .as_ref()
                .expect("layout should exist")
                .scroll_offset,
            0
        );

        let next_result = store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(6),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus next should succeed");
        assert_eq!(
            store.state().focus.focused_window_id.map(|id| id.get()),
            Some(4)
        );
        assert_eq!(
            next_result
                .layout_projection
                .as_ref()
                .expect("layout should exist")
                .scroll_offset,
            200
        );
    }

    #[test]
    fn overflow_focus_navigation_centers_target_column_when_possible() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 800, 700), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(500, 700),
                Rect::new(0, 0, 500, 700),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(500),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("first window discovery should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(2),
                monitor_id,
                101,
                Size::new(500, 700),
                Rect::new(500, 0, 500, 700),
                WindowPlacement::NewColumnAfterFocus {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(500),
                },
                FocusBehavior::PreserveCurrentFocus,
            ))
            .expect("second window discovery should succeed");
        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(3),
                monitor_id,
                102,
                Size::new(500, 700),
                Rect::new(1000, 0, 500, 700),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(500),
                },
                FocusBehavior::PreserveCurrentFocus,
            ))
            .expect("third window discovery should succeed");

        let result = store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(4),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus next should succeed");
        let projection = result
            .layout_projection
            .as_ref()
            .expect("layout should exist");

        assert_eq!(
            store
                .state()
                .focus
                .focused_window_id
                .map(|window_id| window_id.get()),
            Some(2)
        );
        assert_eq!(projection.scroll_offset, 350);
    }

    #[test]
    fn scroll_command_is_clamped_to_content_width() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 600, 700), 96, true);

        for (correlation, hwnd) in [(1_u64, 100_u64), (2, 101)] {
            store
                .dispatch(DomainEvent::window_discovered_with(
                    CorrelationId::new(correlation),
                    monitor_id,
                    hwnd,
                    Size::new(400, 700),
                    Rect::new(0, 0, 400, 700),
                    WindowPlacement::AppendToWorkspaceEnd {
                        mode: ColumnMode::Normal,
                        width: WidthSemantics::Fixed(400),
                    },
                    FocusBehavior::FollowNewWindow,
                ))
                .expect("window discovery should succeed");
        }

        let result = store
            .dispatch(DomainEvent::scroll_strip_right(
                CorrelationId::new(3),
                NavigationScope::WorkspaceStrip,
                0,
            ))
            .expect("scroll command should succeed");

        assert_eq!(
            result
                .layout_projection
                .as_ref()
                .expect("layout should exist")
                .scroll_offset,
            200
        );
    }

    #[test]
    fn scroll_command_changes_projected_geometry_and_apply_plan() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Window 100".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(0, 0, 300, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(300, 0, 300, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                },
                PlatformWindowSnapshot {
                    hwnd: 102,
                    title: "Window 102".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(600, 0, 300, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");

        let first_window_id = runtime
            .state()
            .windows
            .values()
            .find(|window| window.current_hwnd_binding == Some(100))
            .map(|window| window.id)
            .expect("first window should exist");
        let second_window_id = runtime
            .state()
            .windows
            .values()
            .find(|window| window.current_hwnd_binding == Some(101))
            .map(|window| window.id)
            .expect("second window should exist");
        let third_window_id = runtime
            .state()
            .windows
            .values()
            .find(|window| window.current_hwnd_binding == Some(102))
            .map(|window| window.id)
            .expect("third window should exist");
        let workspace_id = runtime
            .state()
            .windows
            .get(&first_window_id)
            .map(|window| window.workspace_id)
            .expect("workspace should exist");

        let result = runtime
            .store
            .dispatch(DomainEvent::scroll_strip_right(
                CorrelationId::new(2),
                NavigationScope::WorkspaceStrip,
                0,
            ))
            .expect("scroll command should succeed");
        let projection = result
            .layout_projection
            .as_ref()
            .expect("layout projection should exist");
        let planned_operations = runtime
            .plan_apply_operations(&snapshot)
            .expect("apply plan should be computed");

        assert_eq!(projection.workspace_id, workspace_id);
        assert_eq!(projection.scroll_offset, 240);
        assert_eq!(geometry_x_width(projection, first_window_id).0, -240);
        assert_eq!(geometry_x_width(projection, second_window_id).0, 60);
        assert_eq!(geometry_x_width(projection, third_window_id).0, 360);
        assert_eq!(planned_operations.len(), 3);
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 100)
                .map(|operation| operation.rect.x),
            Some(-240)
        );
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 100)
                .map(|operation| operation.activate),
            Some(false)
        );
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 101)
                .map(|operation| operation.rect.x),
            Some(60)
        );
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 101)
                .map(|operation| operation.activate),
            Some(false)
        );
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 102)
                .map(|operation| operation.rect.x),
            Some(360)
        );
        assert_eq!(
            planned_operations
                .iter()
                .find(|operation| operation.hwnd == 102)
                .map(|operation| operation.activate),
            Some(false)
        );
    }

    #[test]
    fn focus_mismatch_plans_activation_even_without_geometry_change() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(sample_snapshot(100, Rect::new(0, 0, 420, 900), false), true)
            .expect("initial sync should succeed");

        let planned_operations = runtime
            .plan_apply_operations(&sample_snapshot(100, Rect::new(0, 0, 420, 900), false))
            .expect("apply plan should be computed");

        assert_eq!(planned_operations.len(), 1);
        assert_eq!(planned_operations[0].hwnd, 100);
        assert!(planned_operations[0].activate);
    }

    #[test]
    fn floating_toggle_roundtrip_restores_tiled_membership() {
        let mut store = StateStore::new(RuntimeMode::WmOnly);
        let monitor_id = store
            .state_mut()
            .add_monitor(Rect::new(0, 0, 1200, 800), 96, true);

        store
            .dispatch(DomainEvent::window_discovered_with(
                CorrelationId::new(1),
                monitor_id,
                100,
                Size::new(420, 800),
                Rect::new(0, 0, 420, 800),
                WindowPlacement::AppendToWorkspaceEnd {
                    mode: ColumnMode::Normal,
                    width: WidthSemantics::Fixed(420),
                },
                FocusBehavior::FollowNewWindow,
            ))
            .expect("window discovery should succeed");
        let window_id = store
            .state()
            .focus
            .focused_window_id
            .expect("window should be focused");

        store
            .dispatch(DomainEvent::toggle_floating(
                CorrelationId::new(2),
                Some(window_id),
            ))
            .expect("toggle floating should succeed");
        let floating_window = store
            .state()
            .windows
            .get(&window_id)
            .expect("window should exist");
        assert_eq!(
            floating_window.layer,
            flowtile_domain::WindowLayer::Floating
        );
        assert!(floating_window.column_id.is_none());

        store
            .dispatch(DomainEvent::toggle_floating(
                CorrelationId::new(3),
                Some(window_id),
            ))
            .expect("second toggle floating should succeed");
        let restored_window = store
            .state()
            .windows
            .get(&window_id)
            .expect("window should exist");
        assert_eq!(restored_window.layer, flowtile_domain::WindowLayer::Tiled);
        assert!(restored_window.column_id.is_some());
    }

    #[test]
    fn reload_config_rejects_unsupported_bind_control_mode() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let config_path = unique_config_test_path("bind-control-mode");
        std::fs::create_dir_all(config_path.parent().expect("temp dir should exist"))
            .expect("temp dir should be created");
        std::fs::write(
            &config_path,
            "input {\n  bind-control-mode \"managed-shell\"\n}\n",
        )
        .expect("config should be written");

        let config_path_string = config_path.display().to_string();
        runtime.active_config.projection.source_path = config_path_string.clone();
        runtime.last_valid_config.projection.source_path = config_path_string.clone();
        runtime.store.state_mut().config_projection.source_path = config_path_string;

        let error = runtime
            .reload_config(true)
            .expect_err("unsupported bind control mode should fail reload");

        match error {
            RuntimeError::Config(message) => assert!(message.contains("managed-shell")),
            other => panic!("unexpected reload error: {other:?}"),
        }
        assert_eq!(runtime.bind_control_mode(), BindControlMode::Coexistence);

        let _ = std::fs::remove_file(config_path);
    }

    fn geometry_x_width(
        projection: &WorkspaceLayoutProjection,
        window_id: flowtile_domain::WindowId,
    ) -> (i32, u32) {
        projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .map(|geometry| (geometry.rect.x, geometry.rect.width))
            .expect("window geometry should exist")
    }

    fn sample_snapshot(hwnd: u64, rect: Rect, focused: bool) -> PlatformSnapshot {
        PlatformSnapshot {
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd,
                title: format!("Window {hwnd}"),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect,
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: focused,
            }],
        }
    }

    fn unique_config_test_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir()
            .join("flowtilewm-wm-core-tests")
            .join(format!("{label}-{nonce}.kdl"))
    }

    use flowtile_layout_engine::WorkspaceLayoutProjection;
}
