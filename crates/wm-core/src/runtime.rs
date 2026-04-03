mod apply_policy;
mod config;
mod desktop_materialization;
mod observation;
mod validation;

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flowtile_config_rules::{
    HotkeyBinding, LoadedConfig, TouchpadConfig, WindowRuleInput, bootstrap as config_bootstrap,
    classify_window, default_loaded_config, ensure_default_config, load_from_path, load_or_default,
};
use flowtile_diagnostics::{AtomicPerfMetric, PerfTelemetrySnapshot};
use flowtile_domain::{
    BindControlMode, ColumnId, CorrelationId, DomainEvent, DomainEventPayload, FocusBehavior,
    MonitorId, Rect, ResizeEdge, RuntimeMode, TopologyRole, WidthSemantics, WindowId, WindowLayer,
    WindowPlacement, WmState,
};
use flowtile_layout_engine::{
    WorkspaceLayoutProjection, padded_tiled_viewport, recompute_workspace,
};
use flowtile_windows_adapter::{
    ApplyBatchResult, ApplyOperation, ObservationEnvelope, ObservationKind,
    PlatformDiscoveryAssessment, PlatformMonitorSnapshot, PlatformPresentationPreflight,
    PlatformSnapshot, PlatformWindowDisposition, PlatformWindowRole, PlatformWindowSnapshot,
    SnapshotDiff, SurrogatePresentationDiagnostics, WINDOW_SWITCH_ANIMATION_DURATION_MS,
    WINDOW_SWITCH_ANIMATION_FRAME_COUNT, WindowMonitorScene, WindowMonitorSceneSlice,
    WindowMonitorSceneSliceKind, WindowPresentation, WindowPresentationMode,
    WindowPresentationOverride, WindowSurrogateClip, WindowSwitchAnimation, WindowsAdapter,
    diff_snapshots, needs_activation_apply, needs_geometry_apply,
    needs_tiled_gapless_geometry_apply,
};

use self::apply_policy::{
    WindowVisualSafety, build_visual_emphasis, classify_window_visual_safety,
    format_window_trace_line, has_transient_topology_churn, normalize_reason_token,
    operations_are_activation_only, sanitize_log_text, should_animate_tiled_geometry,
    should_auto_unwind_after_desync, should_defer_post_apply_retry,
    should_force_activation_reassert, should_skip_strict_geometry_revalidation,
    should_suppress_visual_gap, supports_tiled_window_switch_animation, visual_emphasis_has_effect,
    window_layer_name,
};
use self::config::workspace_path;
use self::desktop_materialization::{
    DesktopWindowPresentationMode, build_monitor_local_desktop_projection,
};
use crate::{
    CoreDaemonRuntime, PendingDiscoveryEntry, PendingPlatformFocusCandidate, RuntimeCycleReport,
    RuntimeError, RuntimePerfTelemetry, StateStore,
};

const FOCUS_OBSERVATION_GRACE: Duration = Duration::from_millis(250);
const GEOMETRY_SETTLE_GRACE: Duration = Duration::from_millis(180);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ApplyPlanContext {
    previous_focused_hwnd: Option<u64>,
    animate_window_switch: bool,
    animate_tiled_geometry: bool,
    force_activate_focused_window: bool,
    refresh_visual_emphasis: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FocusStabilizationHold {
    startup_monitor_ids: BTreeSet<MonitorId>,
    held_hwnds: BTreeSet<u64>,
    suppress_activation_reassert: bool,
}

impl FocusStabilizationHold {
    fn holds_window(&self, hwnd: u64, monitor_id: MonitorId) -> bool {
        self.held_hwnds.contains(&hwnd) || self.startup_monitor_ids.contains(&monitor_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveTiledResizeTarget {
    pub workspace_id: flowtile_domain::WorkspaceId,
    pub column_id: ColumnId,
    pub window_id: WindowId,
    pub hwnd: Option<u64>,
    pub rect: Rect,
    pub viewport: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowPresentationProjection {
    pub window_id: WindowId,
    pub mode: String,
    pub reason: String,
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
            perf: std::sync::Arc::new(RuntimePerfTelemetry::default()),
            active_config: startup_config.clone(),
            last_valid_config: startup_config,
            last_snapshot: None,
            pending_discoveries: HashMap::new(),
            pending_focus_claim: None,
            pending_platform_focus_candidate: None,
            pending_geometry_settle_until: None,
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

    pub fn touchpad_config(&self) -> &TouchpadConfig {
        &self.active_config.touchpad
    }

    pub const fn bind_control_mode(&self) -> BindControlMode {
        self.active_config.projection.bind_control_mode
    }

    pub fn last_snapshot(&self) -> Option<&PlatformSnapshot> {
        self.last_snapshot.as_ref()
    }

    pub fn manual_width_resize_preview_rect(&self) -> Option<Rect> {
        self.store
            .state()
            .layout
            .width_resize_session
            .as_ref()
            .map(|session| session.clamped_preview_rect)
    }

    pub fn active_tiled_resize_target(
        &self,
    ) -> Result<Option<ActiveTiledResizeTarget>, RuntimeError> {
        let Some(workspace_id) =
            self.store
                .state()
                .focus
                .focused_monitor_id
                .and_then(|monitor_id| {
                    self.store
                        .state()
                        .active_workspace_id_for_monitor(monitor_id)
                })
        else {
            return Ok(None);
        };
        let Some(window_id) = self.store.state().focus.focused_window_id else {
            return Ok(None);
        };
        let Some(window) = self.store.state().windows.get(&window_id) else {
            return Ok(None);
        };
        if window.layer != WindowLayer::Tiled || window.is_floating || window.is_fullscreen {
            return Ok(None);
        }
        let Some(column_id) = window.column_id else {
            return Ok(None);
        };
        let projection = recompute_workspace(self.store.state(), workspace_id)?;
        let Some(rect) = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .map(|geometry| geometry.rect)
        else {
            return Ok(None);
        };

        Ok(Some(ActiveTiledResizeTarget {
            workspace_id,
            column_id,
            window_id,
            hwnd: window.current_hwnd_binding,
            rect,
            viewport: projection.viewport,
        }))
    }

    pub const fn management_enabled(&self) -> bool {
        self.management_enabled
    }

    pub fn perf_snapshot(&self) -> PerfTelemetrySnapshot {
        let mut metrics = self.perf.snapshot().metrics;
        metrics.extend(self.adapter.perf_snapshot().metrics);
        metrics.sort_by(|left, right| {
            right
                .total_duration_us
                .cmp(&left.total_duration_us)
                .then_with(|| right.samples.cmp(&left.samples))
                .then_with(|| left.metric.cmp(&right.metric))
        });
        PerfTelemetrySnapshot { metrics }
    }

    pub fn surrogate_presentation_diagnostics(&self) -> SurrogatePresentationDiagnostics {
        self.adapter.surrogate_presentation_diagnostics()
    }

    pub fn current_window_presentations(
        &self,
    ) -> Result<HashMap<WindowId, WindowPresentationProjection>, RuntimeError> {
        let workspace_layouts = self.collect_workspace_layouts()?;
        let desktop_projection =
            build_monitor_local_desktop_projection(self.store.state(), &workspace_layouts);
        let presentation_overrides = self.adapter.surrogate_presentation_overrides();

        Ok(build_window_presentation_projections(
            self.store.state(),
            desktop_projection,
            &presentation_overrides,
        ))
    }

    pub fn request_emergency_unwind(&mut self, reason: &str) {
        self.management_enabled = false;
        let cleanup_hwnds = presentation_cleanup_hwnds(
            &self.managed_window_hwnds(),
            &self.adapter.materialized_presentation_hwnds(),
        );
        if let Err(error) = self.adapter.clear_window_presentations(&cleanup_hwnds) {
            self.push_degraded_reason(format!("presentation-cleanup-failed:{error}"));
        }
        self.push_degraded_reason(format!("emergency-unwind:{reason}"));
    }

    pub fn dispatch_command(
        &mut self,
        event: DomainEvent,
        dry_run: bool,
        reason: &str,
    ) -> Result<RuntimeCycleReport, RuntimeError> {
        let started_at = Instant::now();
        let result = (|| {
            let snapshot = self.adapter.scan_snapshot()?;
            let _ = self.sync_snapshot_with_reason(snapshot.clone(), true, "command-pre-sync")?;
            let previous_focused_hwnd = self.current_focused_hwnd();
            let transition = self.store.dispatch(event)?;
            let apply_plan_context = self.build_apply_plan_context(
                previous_focused_hwnd,
                self.current_focused_hwnd(),
                reason,
                false,
            );
            self.arm_pending_focus_claim(previous_focused_hwnd);
            let planned_operations = if self.management_enabled {
                self.plan_apply_operations_with_context(&snapshot, apply_plan_context)?
            } else {
                Vec::new()
            };
            let apply_result = if dry_run || !self.management_enabled {
                ApplyBatchResult::default()
            } else {
                self.adapter.apply_operations(&planned_operations)?
            };
            self.arm_pending_geometry_settle(reason, planned_operations.len(), dry_run);
            let apply_failure_messages = apply_result
                .failures
                .iter()
                .map(|failure| format!("hwnd {}: {}", failure.hwnd, failure.message))
                .collect::<Vec<_>>();
            let strip_movement_logs = self.describe_strip_movements(&snapshot, &planned_operations);
            let window_trace_logs =
                self.describe_window_trace("plan", &snapshot, &planned_operations, None);

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
                focused_hwnd: snapshot.actual_foreground_hwnd(),
                observation_reason: Some(reason.to_string()),
                planned_operations: planned_operations.len(),
                applied_operations: apply_result.applied,
                apply_failures: apply_result.failures.len(),
                apply_failure_messages,
                recovery_rescans: 0,
                validation_remaining_operations: 0,
                recovery_actions: Vec::new(),
                management_enabled: self.management_enabled,
                dry_run,
                degraded_reasons: self.store.state().runtime.degraded_flags.clone(),
                discovery_trace_logs: Vec::new(),
                strip_movement_logs,
                window_trace_logs,
                validation_trace_logs: Vec::new(),
            };
            self.validate_after_apply(&mut report, dry_run)?;
            Ok(report)
        })();
        record_perf_metric(&self.perf.command_cycle, started_at, &result);
        result
    }

    pub fn begin_column_width_resize(
        &mut self,
        edge: ResizeEdge,
        pointer_x: i32,
    ) -> Result<bool, RuntimeError> {
        let correlation_id = self.next_correlation_id();
        match self.store.dispatch(DomainEvent::begin_column_width_resize(
            correlation_id,
            edge,
            pointer_x,
        )) {
            Ok(_) => Ok(self.store.state().layout.width_resize_session.is_some()),
            Err(crate::CoreError::InvalidEvent(_)) => Ok(false),
            Err(error) => Err(RuntimeError::Core(error)),
        }
    }

    pub fn update_column_width_resize(&mut self, pointer_x: i32) -> Result<(), RuntimeError> {
        let correlation_id = self.next_correlation_id();
        match self
            .store
            .dispatch(DomainEvent::update_column_width_preview(
                correlation_id,
                pointer_x,
            )) {
            Ok(_) => Ok(()),
            Err(crate::CoreError::InvalidEvent(_)) => Ok(()),
            Err(error) => Err(RuntimeError::Core(error)),
        }
    }

    pub fn cancel_column_width_resize(&mut self) -> Result<(), RuntimeError> {
        let correlation_id = self.next_correlation_id();
        match self
            .store
            .dispatch(DomainEvent::cancel_column_width_resize(correlation_id))
        {
            Ok(_) => Ok(()),
            Err(crate::CoreError::InvalidEvent(_)) => Ok(()),
            Err(error) => Err(RuntimeError::Core(error)),
        }
    }

    pub fn commit_column_width_resize(
        &mut self,
        pointer_x: i32,
        dry_run: bool,
    ) -> Result<RuntimeCycleReport, RuntimeError> {
        let correlation_id = self.next_correlation_id();
        self.dispatch_command(
            DomainEvent::commit_column_width(correlation_id, pointer_x),
            dry_run,
            "manual-column-width-commit",
        )
    }

    pub(crate) fn plan_apply_operations(
        &self,
        snapshot: &PlatformSnapshot,
    ) -> Result<Vec<ApplyOperation>, RuntimeError> {
        self.plan_apply_operations_with_context(snapshot, ApplyPlanContext::default())
    }

    fn plan_apply_operations_with_context(
        &self,
        snapshot: &PlatformSnapshot,
        apply_plan_context: ApplyPlanContext,
    ) -> Result<Vec<ApplyOperation>, RuntimeError> {
        let actual_windows = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window))
            .collect::<HashMap<_, _>>();
        let workspace_layouts = self.collect_workspace_layouts()?;
        let desktop_projection =
            build_monitor_local_desktop_projection(self.store.state(), &workspace_layouts);
        let desired_focused_hwnd = self
            .store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .filter(|window| window.is_managed)
            .and_then(|window| window.current_hwnd_binding);
        let actual_focused_hwnd = snapshot.actual_foreground_hwnd();
        let overview_is_open = self.store.state().overview.is_open;
        let focus_stabilization_hold = self.focus_stabilization_hold(snapshot);
        let allow_activation_reassert = !overview_is_open
            && !focus_stabilization_hold.suppress_activation_reassert
            && self.should_attempt_activation_reassert(actual_focused_hwnd);
        let effective_focused_hwnd = if focus_stabilization_hold.suppress_activation_reassert {
            actual_focused_hwnd.or(desired_focused_hwnd)
        } else {
            desired_focused_hwnd
        };
        let materialized_presentation_hwnds = self.adapter.materialized_presentation_hwnds();
        let mut desired_operation_hwnds = BTreeSet::new();
        let mut operations = Vec::new();

        for monitor_id in self.store.state().monitor_ids_in_navigation_order() {
            let Some(monitor_projection) = desktop_projection.monitors.get(&monitor_id) else {
                continue;
            };

            for workspace_band in &monitor_projection.workspace_bands {
                let Some(projection) = workspace_layouts.get(&workspace_band.workspace_id) else {
                    continue;
                };

                for geometry in &projection.window_geometries {
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
                    let Some(window_projection) =
                        desktop_projection.windows.get(&geometry.window_id)
                    else {
                        continue;
                    };
                    let holds_native_visibility = !overview_is_open
                        && geometry.layer == WindowLayer::Tiled
                        && focus_stabilization_hold
                            .holds_window(hwnd, window_projection.monitor_id);
                    let (target_rect, presentation) = if overview_is_open {
                        (
                            window_projection.logical_desktop_rect,
                            WindowPresentation::default(),
                        )
                    } else if holds_native_visibility {
                        (
                            window_projection.logical_desktop_rect,
                            native_visible_presentation_from_desktop_projection(window_projection),
                        )
                    } else {
                        (
                            window_projection.desktop_rect,
                            presentation_from_desktop_projection(window_projection),
                        )
                    };
                    let uses_non_native_presentation_mode =
                        presentation.mode != WindowPresentationMode::NativeVisible;
                    let needs_presentation_sync = should_sync_presentation(
                        overview_is_open,
                        hwnd,
                        &presentation,
                        uses_non_native_presentation_mode,
                        &materialized_presentation_hwnds,
                    );
                    let needs_geometry = if geometry.layer == WindowLayer::Tiled {
                        needs_tiled_gapless_geometry_apply(actual_window.rect, target_rect)
                    } else {
                        needs_geometry_apply(actual_window.rect, target_rect)
                    };
                    let activate = desired_focused_hwnd
                        .filter(|_| allow_activation_reassert)
                        .filter(|desired_hwnd| *desired_hwnd == hwnd)
                        .is_some_and(|desired_hwnd| {
                            needs_activation_apply(actual_focused_hwnd, desired_hwnd)
                                || (apply_plan_context.force_activate_focused_window
                                    && needs_geometry)
                        });
                    let active_state_changed = apply_plan_context.previous_focused_hwnd
                        != effective_focused_hwnd
                        && (apply_plan_context.previous_focused_hwnd == Some(hwnd)
                            || effective_focused_hwnd == Some(hwnd));
                    let visual_emphasis = (!overview_is_open
                        && !uses_non_native_presentation_mode
                        && (needs_geometry
                            || activate
                            || active_state_changed
                            || apply_plan_context.refresh_visual_emphasis))
                        .then(|| {
                            build_visual_emphasis(
                                effective_focused_hwnd == Some(hwnd),
                                actual_window.process_name.as_deref(),
                                &actual_window.class_name,
                                &actual_window.title,
                            )
                        })
                        .filter(visual_emphasis_has_effect);
                    if needs_geometry
                        || activate
                        || visual_emphasis.is_some()
                        || needs_presentation_sync
                    {
                        let window_switch_animation = ((apply_plan_context.animate_window_switch
                            || apply_plan_context.animate_tiled_geometry)
                            && supports_tiled_window_switch_animation(
                                actual_window.process_name.as_deref(),
                                &actual_window.class_name,
                                &actual_window.title,
                            )
                            && geometry.layer == WindowLayer::Tiled
                            && needs_geometry)
                            .then_some(WindowSwitchAnimation {
                                from_rect: actual_window.rect,
                                duration_ms: WINDOW_SWITCH_ANIMATION_DURATION_MS,
                                frame_count: WINDOW_SWITCH_ANIMATION_FRAME_COUNT,
                            });
                        operations.push(ApplyOperation {
                            hwnd,
                            rect: target_rect,
                            apply_geometry: needs_geometry,
                            activate,
                            suppress_visual_gap: should_suppress_visual_gap(
                                geometry.layer,
                                actual_window.process_name.as_deref(),
                                &actual_window.class_name,
                                &actual_window.title,
                            ),
                            window_switch_animation,
                            visual_emphasis,
                            presentation,
                        });
                        desired_operation_hwnds.insert(hwnd);
                    }
                }
            }
        }

        operations.extend(materialized_presentation_cleanup_operations(
            &actual_windows,
            &desired_operation_hwnds,
            &materialized_presentation_hwnds,
        ));

        Ok(operations)
    }

    fn collect_workspace_layouts(
        &self,
    ) -> Result<HashMap<flowtile_domain::WorkspaceId, WorkspaceLayoutProjection>, RuntimeError>
    {
        self.store
            .state()
            .workspaces
            .values()
            .filter(|workspace| !self.store.state().is_workspace_empty(workspace.id))
            .map(|workspace| {
                recompute_workspace(self.store.state(), workspace.id)
                    .map(|projection| (workspace.id, projection))
                    .map_err(Into::into)
            })
            .collect()
    }

    fn build_apply_plan_context(
        &self,
        previous_focused_hwnd: Option<u64>,
        current_focused_hwnd: Option<u64>,
        reason: &str,
        refresh_visual_emphasis: bool,
    ) -> ApplyPlanContext {
        ApplyPlanContext {
            previous_focused_hwnd,
            animate_window_switch: self
                .should_animate_window_switch(previous_focused_hwnd, current_focused_hwnd),
            animate_tiled_geometry: should_animate_tiled_geometry(reason),
            force_activate_focused_window: should_force_activation_reassert(reason),
            refresh_visual_emphasis: refresh_visual_emphasis
                || previous_focused_hwnd != current_focused_hwnd,
        }
    }

    fn should_attempt_activation_reassert(&self, actual_focused_hwnd: Option<u64>) -> bool {
        if self.pending_focus_claim.is_some() {
            return true;
        }

        let Some(actual_focused_hwnd) = actual_focused_hwnd else {
            return true;
        };

        self.find_window_id_by_hwnd(actual_focused_hwnd)
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .is_some_and(|window| window.is_managed)
    }

    fn focus_stabilization_hold(&self, snapshot: &PlatformSnapshot) -> FocusStabilizationHold {
        let mut hold = FocusStabilizationHold::default();

        if self.last_snapshot.is_none() && snapshot.monitors.len() > 1 {
            if let Some(focused_monitor_id) = snapshot
                .actual_foreground_hwnd()
                .and_then(|hwnd| self.managed_tiled_window_monitor_id(hwnd))
            {
                for window in self.store.state().windows.values() {
                    if !window.is_managed || window.layer != WindowLayer::Tiled {
                        continue;
                    }
                    let Some(workspace) = self.store.state().workspaces.get(&window.workspace_id)
                    else {
                        continue;
                    };
                    if workspace.monitor_id != focused_monitor_id {
                        hold.startup_monitor_ids.insert(workspace.monitor_id);
                    }
                }
            }
        }

        if self.pending_focus_claim.is_none()
            && let Some(desired_hwnd) = self.managed_tiled_focused_hwnd()
            && let Some(actual_hwnd) = snapshot.actual_foreground_hwnd()
            && desired_hwnd != actual_hwnd
            && let Some(desired_monitor_id) = self.managed_tiled_window_monitor_id(desired_hwnd)
            && let Some(actual_monitor_id) = self.managed_tiled_window_monitor_id(actual_hwnd)
            && desired_monitor_id != actual_monitor_id
        {
            hold.held_hwnds.insert(desired_hwnd);
            hold.held_hwnds.insert(actual_hwnd);
            hold.suppress_activation_reassert = true;
        }

        hold
    }

    fn managed_tiled_focused_hwnd(&self) -> Option<u64> {
        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .filter(|window| window.is_managed && window.layer == WindowLayer::Tiled)
            .and_then(|window| window.current_hwnd_binding)
    }

    fn managed_tiled_window_monitor_id(&self, hwnd: u64) -> Option<MonitorId> {
        let window_id = self.find_window_id_by_hwnd(hwnd)?;
        let window = self.store.state().windows.get(&window_id)?;
        if !window.is_managed || window.layer != WindowLayer::Tiled {
            return None;
        }
        self.store
            .state()
            .workspaces
            .get(&window.workspace_id)
            .map(|workspace| workspace.monitor_id)
    }

    pub(super) fn should_hold_post_apply_for_focus_stabilization(
        &self,
        snapshot: &PlatformSnapshot,
        operations: &[ApplyOperation],
    ) -> bool {
        if operations.is_empty() {
            return false;
        }

        let hold = self.focus_stabilization_hold(snapshot);
        if !hold.suppress_activation_reassert {
            return false;
        }

        operations.iter().all(|operation| {
            self.managed_tiled_window_monitor_id(operation.hwnd)
                .is_some_and(|monitor_id| hold.holds_window(operation.hwnd, monitor_id))
        })
    }

    fn should_animate_window_switch(
        &self,
        previous_focused_hwnd: Option<u64>,
        current_focused_hwnd: Option<u64>,
    ) -> bool {
        if previous_focused_hwnd == current_focused_hwnd {
            return false;
        }

        current_focused_hwnd
            .and_then(|hwnd| self.find_window_id_by_hwnd(hwnd))
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .is_some_and(|window| window.is_managed && window.layer == WindowLayer::Tiled)
    }

    fn arm_pending_focus_claim(&mut self, previous_focused_hwnd: Option<u64>) {
        let current_focused_hwnd = self.current_focused_hwnd();
        let Some(desired_hwnd) = current_focused_hwnd else {
            self.pending_focus_claim = None;
            self.pending_platform_focus_candidate = None;
            return;
        };

        if previous_focused_hwnd == Some(desired_hwnd) {
            return;
        }

        let focus_origin = self.store.state().focus.focus_origin;
        if focus_origin != flowtile_domain::FocusOrigin::UserCommand {
            return;
        }

        self.pending_platform_focus_candidate = None;
        self.pending_focus_claim = Some(crate::PendingFocusClaim {
            desired_hwnd,
            expires_at: Instant::now() + FOCUS_OBSERVATION_GRACE,
        });
    }

    fn refresh_pending_focus_claim(&mut self, _actual_focused_hwnd: Option<u64>) {
        let Some(pending_claim) = &self.pending_focus_claim else {
            return;
        };

        if Instant::now() >= pending_claim.expires_at {
            self.pending_focus_claim = None;
        }
    }

    fn stage_platform_focus_candidate(&mut self, observed_hwnd: u64) {
        self.pending_platform_focus_candidate = Some(PendingPlatformFocusCandidate {
            observed_hwnd,
            stable_snapshots: 1,
        });
    }

    fn refresh_pending_platform_focus_candidate(
        &mut self,
        actual_focused_hwnd: Option<u64>,
    ) -> Option<u64> {
        let Some(observed_hwnd) = self
            .pending_platform_focus_candidate
            .as_ref()
            .map(|candidate| candidate.observed_hwnd)
        else {
            return None;
        };

        if actual_focused_hwnd != Some(observed_hwnd)
            || !self.should_stage_platform_focus_observation(observed_hwnd)
        {
            self.pending_platform_focus_candidate = None;
            return None;
        }

        let Some(candidate) = &mut self.pending_platform_focus_candidate else {
            return None;
        };
        candidate.stable_snapshots = candidate.stable_snapshots.saturating_add(1);
        if candidate.stable_snapshots < 2 {
            return None;
        }

        self.pending_platform_focus_candidate = None;
        Some(observed_hwnd)
    }

    fn should_stage_platform_focus_observation(&self, observed_hwnd: u64) -> bool {
        if self.pending_focus_claim.is_some() {
            return false;
        }

        let Some(desired_hwnd) = self.managed_tiled_focused_hwnd() else {
            return false;
        };
        if desired_hwnd == observed_hwnd {
            return false;
        }

        let Some(desired_monitor_id) = self.managed_tiled_window_monitor_id(desired_hwnd) else {
            return false;
        };
        let Some(observed_monitor_id) = self.managed_tiled_window_monitor_id(observed_hwnd) else {
            return false;
        };

        desired_monitor_id != observed_monitor_id
    }

    fn arm_pending_geometry_settle(
        &mut self,
        reason: &str,
        planned_operations: usize,
        dry_run: bool,
    ) {
        if dry_run || planned_operations == 0 || !should_defer_post_apply_retry(reason) {
            self.pending_geometry_settle_until = None;
            return;
        }

        self.pending_geometry_settle_until = Some(Instant::now() + GEOMETRY_SETTLE_GRACE);
    }

    fn should_defer_geometry_observation(&mut self, reason: &str) -> bool {
        let Some(expires_at) = self.pending_geometry_settle_until else {
            return false;
        };

        if Instant::now() >= expires_at {
            self.pending_geometry_settle_until = None;
            return false;
        }

        normalize_reason_token(reason).contains("location-change")
    }

    fn should_defer_platform_focus_observation(&mut self, observed_hwnd: u64) -> bool {
        if let Some(pending_claim) = &self.pending_focus_claim {
            if Instant::now() < pending_claim.expires_at {
                return observed_hwnd != pending_claim.desired_hwnd;
            }

            self.pending_focus_claim = None;
        }

        if self.should_stage_platform_focus_observation(observed_hwnd) {
            self.stage_platform_focus_candidate(observed_hwnd);
            return true;
        }

        self.pending_platform_focus_candidate = None;
        false
    }

    fn push_degraded_reason(&mut self, reason: String) {
        if !self.store.state().runtime.degraded_flags.contains(&reason) {
            self.store.state_mut().runtime.degraded_flags.push(reason);
        }
    }

    fn managed_window_hwnds(&self) -> Vec<u64> {
        self.store
            .state()
            .windows
            .values()
            .filter(|window| window.is_managed)
            .filter_map(|window| window.current_hwnd_binding)
            .collect()
    }

    fn next_correlation_id(&mut self) -> CorrelationId {
        let correlation_id = CorrelationId::new(self.next_correlation_id);
        self.next_correlation_id += 1;
        correlation_id
    }
}

fn pending_discovery_required_stable_ticks(
    assessment: &PlatformDiscoveryAssessment,
    window: &PlatformWindowSnapshot,
    had_previous_snapshot: bool,
) -> u8 {
    match assessment.disposition {
        PlatformWindowDisposition::PromotablePrimaryCandidate => {
            if assessment.presentation_preflight == PlatformPresentationPreflight::FallbackOnly
                && !window.is_focused
            {
                2
            } else {
                1
            }
        }
        PlatformWindowDisposition::PendingPrimaryCandidate => {
            if !had_previous_snapshot {
                return if assessment.presentation_preflight
                    == PlatformPresentationPreflight::FallbackOnly
                {
                    3
                } else {
                    2
                };
            }
            if window.is_focused {
                if assessment.presentation_preflight == PlatformPresentationPreflight::FallbackOnly
                {
                    3
                } else {
                    2
                }
            } else if assessment.presentation_preflight
                == PlatformPresentationPreflight::FallbackOnly
            {
                4
            } else {
                3
            }
        }
        _ => u8::MAX,
    }
}

fn pending_discovery_rect_is_stable(previous: Rect, current: Rect) -> bool {
    let x_delta = previous.x.saturating_sub(current.x).abs();
    let y_delta = previous.y.saturating_sub(current.y).abs();
    let width_delta = i64::from(previous.width)
        .saturating_sub(i64::from(current.width))
        .abs();
    let height_delta = i64::from(previous.height)
        .saturating_sub(i64::from(current.height))
        .abs();

    x_delta <= 24 && y_delta <= 24 && width_delta <= 24 && height_delta <= 24
}

fn build_window_presentation_projections(
    state: &WmState,
    desktop_projection: desktop_materialization::DesktopProjection,
    presentation_overrides: &HashMap<u64, WindowPresentationOverride>,
) -> HashMap<WindowId, WindowPresentationProjection> {
    desktop_projection
        .windows
        .into_iter()
        .map(|(window_id, projection)| {
            let (mode, reason) = effective_window_presentation(
                state,
                window_id,
                projection.presentation_mode,
                projection.presentation_reason.as_str(),
                presentation_overrides,
            );
            (
                window_id,
                WindowPresentationProjection {
                    window_id,
                    mode,
                    reason,
                },
            )
        })
        .collect()
}

fn effective_window_presentation(
    state: &WmState,
    window_id: WindowId,
    desired_mode: DesktopWindowPresentationMode,
    desired_reason: &str,
    presentation_overrides: &HashMap<u64, WindowPresentationOverride>,
) -> (String, String) {
    if matches!(
        desired_mode,
        DesktopWindowPresentationMode::SurrogateVisible
            | DesktopWindowPresentationMode::SurrogateClipped
    ) {
        if let Some(override_projection) = state
            .windows
            .get(&window_id)
            .and_then(|window| window.current_hwnd_binding)
            .and_then(|hwnd| presentation_overrides.get(&hwnd))
        {
            return (
                override_projection.mode.as_str().to_string(),
                override_projection.reason.clone(),
            );
        }
    }

    (
        desired_mode.as_str().to_string(),
        desired_reason.to_string(),
    )
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn record_perf_metric<T, E>(metric: &AtomicPerfMetric, started_at: Instant, result: &Result<T, E>) {
    metric.record_duration(started_at.elapsed());
    if result.is_err() {
        metric.record_error();
    }
}

fn presentation_from_desktop_projection(
    projection: &desktop_materialization::DesktopWindowProjection,
) -> WindowPresentation {
    let monitor_scene = monitor_scene_from_desktop_projection(projection);

    match projection.presentation_mode {
        DesktopWindowPresentationMode::NativeVisible => {
            native_visible_presentation_from_desktop_projection(projection)
        }
        DesktopWindowPresentationMode::NativeHidden => WindowPresentation {
            mode: WindowPresentationMode::NativeHidden,
            surrogate: None,
            monitor_scene,
        },
        DesktopWindowPresentationMode::SurrogateVisible => WindowPresentation {
            mode: WindowPresentationMode::SurrogateVisible,
            surrogate: projection
                .surrogate_rect
                .zip(projection.surrogate_source_rect)
                .map(|(destination_rect, source_rect)| WindowSurrogateClip {
                    destination_rect,
                    source_rect,
                    native_visible_rect: projection.logical_desktop_rect,
                }),
            monitor_scene,
        },
        DesktopWindowPresentationMode::SurrogateClipped => WindowPresentation {
            mode: WindowPresentationMode::SurrogateClipped,
            surrogate: projection
                .surrogate_rect
                .zip(projection.surrogate_source_rect)
                .map(|(destination_rect, source_rect)| WindowSurrogateClip {
                    destination_rect,
                    source_rect,
                    native_visible_rect: projection.logical_desktop_rect,
                }),
            monitor_scene,
        },
    }
}

fn native_visible_presentation_from_desktop_projection(
    projection: &desktop_materialization::DesktopWindowProjection,
) -> WindowPresentation {
    let monitor_scene = monitor_scene_from_desktop_projection(projection);

    if !native_visible_presentation_requires_auxiliary_state(projection, &monitor_scene) {
        WindowPresentation::default()
    } else {
        WindowPresentation {
            monitor_scene,
            ..WindowPresentation::default()
        }
    }
}

fn presentation_has_auxiliary_surfaces(presentation: &WindowPresentation) -> bool {
    presentation.surrogate.is_some()
        || presentation.monitor_scene.home_visible_rect.is_some()
        || !presentation.monitor_scene.slices.is_empty()
}

fn native_visible_presentation_requires_auxiliary_state(
    projection: &desktop_materialization::DesktopWindowProjection,
    monitor_scene: &WindowMonitorScene,
) -> bool {
    monitor_scene_requires_home_clip(projection) || !monitor_scene.slices.is_empty()
}

fn monitor_scene_requires_home_clip(
    projection: &desktop_materialization::DesktopWindowProjection,
) -> bool {
    projection
        .owning_monitor_visible_rect
        .is_some_and(|visible_rect| visible_rect != projection.logical_desktop_rect)
}

fn should_sync_presentation(
    overview_is_open: bool,
    hwnd: u64,
    presentation: &WindowPresentation,
    uses_non_native_presentation_mode: bool,
    materialized_presentation_hwnds: &std::collections::BTreeSet<u64>,
) -> bool {
    materialized_presentation_hwnds.contains(&hwnd)
        || (!overview_is_open
            && (presentation_has_auxiliary_surfaces(presentation)
                || uses_non_native_presentation_mode))
}

fn materialized_presentation_cleanup_operations(
    actual_windows: &HashMap<u64, &PlatformWindowSnapshot>,
    desired_operation_hwnds: &BTreeSet<u64>,
    materialized_presentation_hwnds: &BTreeSet<u64>,
) -> Vec<ApplyOperation> {
    materialized_presentation_hwnds
        .iter()
        .copied()
        .filter(|hwnd| !desired_operation_hwnds.contains(hwnd))
        .filter_map(|hwnd| {
            let actual_window = actual_windows.get(&hwnd)?;
            Some(ApplyOperation {
                hwnd,
                rect: actual_window.rect,
                apply_geometry: false,
                activate: false,
                suppress_visual_gap: false,
                window_switch_animation: None,
                visual_emphasis: None,
                presentation: WindowPresentation::default(),
            })
        })
        .collect()
}

fn stale_materialized_presentation_hwnds(
    snapshot: &PlatformSnapshot,
    materialized_presentation_hwnds: &BTreeSet<u64>,
) -> Vec<u64> {
    let actual_hwnds = snapshot
        .windows
        .iter()
        .map(|window| window.hwnd)
        .collect::<BTreeSet<_>>();

    materialized_presentation_hwnds
        .iter()
        .copied()
        .filter(|hwnd| !actual_hwnds.contains(hwnd))
        .collect()
}

fn presentation_cleanup_hwnds(
    managed_window_hwnds: &[u64],
    materialized_presentation_hwnds: &BTreeSet<u64>,
) -> Vec<u64> {
    let mut cleanup_hwnds = managed_window_hwnds
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    cleanup_hwnds.extend(materialized_presentation_hwnds.iter().copied());
    cleanup_hwnds.into_iter().collect()
}

fn monitor_scene_from_desktop_projection(
    projection: &desktop_materialization::DesktopWindowProjection,
) -> WindowMonitorScene {
    WindowMonitorScene {
        home_visible_rect: projection.owning_monitor_visible_rect,
        slices: projection
            .monitor_slices
            .iter()
            .map(|slice| WindowMonitorSceneSlice {
                kind: monitor_scene_slice_kind_from_desktop(slice.kind),
                monitor_rect: slice.monitor_work_area,
                destination_rect: slice.destination_rect,
                source_rect: slice.source_rect,
                native_visible_rect: projection.logical_desktop_rect,
            })
            .collect(),
    }
}

fn monitor_scene_slice_kind_from_desktop(
    kind: desktop_materialization::DesktopWindowMonitorSliceKind,
) -> WindowMonitorSceneSliceKind {
    match kind {
        desktop_materialization::DesktopWindowMonitorSliceKind::ForeignMonitorSurrogate => {
            WindowMonitorSceneSliceKind::ForeignMonitorSurrogate
        }
    }
}

#[cfg(test)]
mod tests;
