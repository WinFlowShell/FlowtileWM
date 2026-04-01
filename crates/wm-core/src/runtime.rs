mod apply_policy;
mod config;
mod desktop_materialization;
mod observation;
mod validation;

use std::{
    collections::HashMap,
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
    PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot, SnapshotDiff,
    SurrogatePresentationDiagnostics, WINDOW_SWITCH_ANIMATION_DURATION_MS,
    WINDOW_SWITCH_ANIMATION_FRAME_COUNT, WindowPresentation, WindowPresentationMode,
    WindowSurrogateClip, WindowSwitchAnimation, WindowsAdapter, diff_snapshots,
    needs_activation_apply, needs_geometry_apply, needs_tiled_gapless_geometry_apply,
};

use self::apply_policy::{
    WindowVisualSafety, build_visual_emphasis, classify_window_visual_safety,
    format_window_trace_line, normalize_reason_token, operations_are_activation_only,
    sanitize_log_text, should_animate_tiled_geometry, should_auto_unwind_after_desync,
    should_defer_post_apply_retry, should_force_activation_reassert, should_suppress_visual_gap,
    supports_tiled_window_switch_animation, visual_emphasis_has_effect, window_layer_name,
};
use self::config::workspace_path;
use self::desktop_materialization::{
    DesktopWindowPresentationMode, build_monitor_local_desktop_projection,
};
use crate::{
    CoreDaemonRuntime, RuntimeCycleReport, RuntimeError, RuntimePerfTelemetry, StateStore,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveTiledResizeTarget {
    pub workspace_id: flowtile_domain::WorkspaceId,
    pub column_id: ColumnId,
    pub window_id: WindowId,
    pub hwnd: Option<u64>,
    pub rect: Rect,
    pub viewport: Rect,
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
            pending_focus_claim: None,
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
        let allow_activation_reassert =
            !overview_is_open && self.should_attempt_activation_reassert(actual_focused_hwnd);
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
                    let target_rect = window_projection.desktop_rect;
                    let presentation = if overview_is_open {
                        WindowPresentation::default()
                    } else {
                        presentation_from_desktop_projection(window_projection)
                    };
                    let needs_presentation =
                        presentation.mode != WindowPresentationMode::NativeVisible;
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
                        != desired_focused_hwnd
                        && (apply_plan_context.previous_focused_hwnd == Some(hwnd)
                            || desired_focused_hwnd == Some(hwnd));
                    let visual_emphasis = (!overview_is_open
                        && !needs_presentation
                        && (needs_geometry
                            || activate
                            || active_state_changed
                            || apply_plan_context.refresh_visual_emphasis))
                        .then(|| {
                            build_visual_emphasis(
                                desired_focused_hwnd == Some(hwnd),
                                actual_window.process_name.as_deref(),
                                &actual_window.class_name,
                                &actual_window.title,
                            )
                        })
                        .filter(visual_emphasis_has_effect);
                    if needs_geometry || activate || visual_emphasis.is_some() || needs_presentation
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
                    }
                }
            }
        }

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
            return;
        };

        if previous_focused_hwnd == Some(desired_hwnd) {
            return;
        }

        let focus_origin = self.store.state().focus.focus_origin;
        if focus_origin != flowtile_domain::FocusOrigin::UserCommand {
            return;
        }

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
        let Some(pending_claim) = &self.pending_focus_claim else {
            return false;
        };

        if Instant::now() >= pending_claim.expires_at {
            self.pending_focus_claim = None;
            return false;
        }

        observed_hwnd != pending_claim.desired_hwnd
    }

    fn push_degraded_reason(&mut self, reason: String) {
        if !self.store.state().runtime.degraded_flags.contains(&reason) {
            self.store.state_mut().runtime.degraded_flags.push(reason);
        }
    }

    fn next_correlation_id(&mut self) -> CorrelationId {
        let correlation_id = CorrelationId::new(self.next_correlation_id);
        self.next_correlation_id += 1;
        correlation_id
    }
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
    match projection.presentation_mode {
        DesktopWindowPresentationMode::NativeVisible => WindowPresentation::default(),
        DesktopWindowPresentationMode::NativeHidden => WindowPresentation {
            mode: WindowPresentationMode::NativeHidden,
            surrogate: None,
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
        },
    }
}

#[cfg(test)]
mod tests;
