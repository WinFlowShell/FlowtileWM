use std::{
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
use flowtile_layout_engine::{padded_tiled_viewport, recompute_workspace};
use flowtile_windows_adapter::{
    ApplyBatchResult, ApplyOperation, ObservationEnvelope, ObservationKind,
    PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot, SnapshotDiff,
    WINDOW_SWITCH_ANIMATION_DURATION_MS, WINDOW_SWITCH_ANIMATION_FRAME_COUNT, WindowOpacityMode,
    WindowSwitchAnimation, WindowVisualEmphasis, WindowsAdapter, diff_snapshots,
    needs_activation_apply, needs_geometry_apply, needs_tiled_gapless_geometry_apply,
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

    pub fn reload_config(&mut self, dry_run: bool) -> Result<RuntimeCycleReport, RuntimeError> {
        let started_at = Instant::now();
        let result = (|| {
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
                    let changed_sections =
                        diff_config_sections(&self.active_config, &loaded_config);
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
        })();
        record_perf_metric(&self.perf.config_reload, started_at, &result);
        result
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
                if self.should_defer_geometry_observation(&observation.reason) {
                    return Ok(None);
                }
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
        let started_at = Instant::now();
        let result = (|| {
            snapshot.sort_for_stability();
            self.note_observation_reason(observation_reason);
            let previous_focused_hwnd = self.current_focused_hwnd();
            self.sync_monitors_from_snapshot(&snapshot.monitors)?;

            let had_previous_snapshot = self.last_snapshot.is_some();
            let diff = self
                .last_snapshot
                .as_ref()
                .map(|previous| diff_snapshots(previous, &snapshot))
                .unwrap_or_else(|| SnapshotDiff::initial(&snapshot));

            let discovered_windows = self.ingest_created_windows(
                &diff.created_windows,
                snapshot.actual_foreground_hwnd(),
                had_previous_snapshot,
            )?;
            let destroyed_windows = self.ingest_destroyed_windows(&diff.destroyed_hwnds)?
                + self.prune_state_windows_missing_from_snapshot(&snapshot)?;

            if had_previous_snapshot && diff.monitor_topology_changed {
                self.push_degraded_reason("monitor-topology-changed".to_string());
            }

            self.refresh_pending_focus_claim(snapshot.actual_foreground_hwnd());
            if let Some(focused_hwnd) = diff.focused_hwnd
                && self.current_focused_hwnd() != Some(focused_hwnd)
                && !self.should_defer_platform_focus_observation(focused_hwnd)
            {
                self.observe_focus(focused_hwnd)?;
            }

            let apply_plan_context = self.build_apply_plan_context(
                previous_focused_hwnd,
                self.current_focused_hwnd(),
                "",
                !had_previous_snapshot || discovered_windows > 0,
            );
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
            self.arm_pending_geometry_settle(observation_reason, planned_operations.len(), dry_run);
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
            if !planned_operations.is_empty() {
                self.store.state_mut().runtime.last_reconcile_at = Some(now);
            }
            self.last_snapshot = Some(snapshot.clone());

            Ok(RuntimeCycleReport {
                monitor_count: snapshot.monitors.len(),
                observed_window_count: snapshot.windows.len(),
                discovered_windows,
                destroyed_windows,
                focused_hwnd: snapshot.actual_foreground_hwnd(),
                observation_reason: Some(observation_reason.to_string()),
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
            })
        })();
        record_perf_metric(&self.perf.observation_sync, started_at, &result);
        result
    }

    fn validate_after_apply(
        &mut self,
        report: &mut RuntimeCycleReport,
        dry_run: bool,
    ) -> Result<(), RuntimeError> {
        let started_at = Instant::now();
        let result = (|| {
            if dry_run || !self.management_enabled || report.planned_operations == 0 {
                self.perf.post_apply_validation.record_skip();
                return Ok(());
            }

            let validation_snapshot = self.adapter.scan_snapshot()?;
            report.recovery_rescans += 1;
            let mut remaining_operations = self.filter_validatable_operations_for_snapshot(
                &validation_snapshot,
                self.plan_apply_operations(&validation_snapshot)?,
            );
            let adapted_platform_min_width =
                self.adapt_to_platform_min_widths(&validation_snapshot, &remaining_operations)?;
            if adapted_platform_min_width {
                report
                    .recovery_actions
                    .push("platform-min-width-adapted".to_string());
                remaining_operations = self.filter_validatable_operations_for_snapshot(
                    &validation_snapshot,
                    self.plan_apply_operations(&validation_snapshot)?,
                );
            }
            report.validation_trace_logs = self.describe_window_trace(
                "validation",
                &validation_snapshot,
                &remaining_operations,
                Some("remaining"),
            );
            report.validation_remaining_operations = remaining_operations.len();

            if remaining_operations.is_empty() {
                self.consecutive_desync_cycles = 0;
                report
                    .recovery_actions
                    .push("post-apply-validation-clean".to_string());
                report.degraded_reasons = self.store.state().runtime.degraded_flags.clone();
                return Ok(());
            }

            if operations_are_activation_only(&validation_snapshot, &remaining_operations) {
                self.consecutive_desync_cycles = 0;
                self.push_degraded_reason("activation:foreground-refused".to_string());
                report.recovery_actions.push(format!(
                    "activation-only-degraded:{}-ops-remain",
                    remaining_operations.len()
                ));
                report.degraded_reasons = self.store.state().runtime.degraded_flags.clone();
                return Ok(());
            }

            if !adapted_platform_min_width
                && report
                    .observation_reason
                    .as_deref()
                    .is_some_and(should_defer_post_apply_retry)
            {
                self.consecutive_desync_cycles = 0;
                report.recovery_actions.push(format!(
                    "post-apply-settling:{}-ops-remain",
                    remaining_operations.len()
                ));
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
            report.apply_failure_messages.extend(
                retry_result
                    .failures
                    .iter()
                    .map(|failure| format!("hwnd {}: {}", failure.hwnd, failure.message)),
            );

            let post_retry_snapshot = self.adapter.scan_snapshot()?;
            report.recovery_rescans += 1;
            let post_retry_remaining = self.filter_validatable_operations_for_snapshot(
                &post_retry_snapshot,
                self.plan_apply_operations(&post_retry_snapshot)?,
            );
            report.validation_remaining_operations = post_retry_remaining.len();

            if post_retry_remaining.is_empty() {
                self.consecutive_desync_cycles = 0;
                report.recovery_actions.push("retry-recovered".to_string());
            } else if operations_are_activation_only(&post_retry_snapshot, &post_retry_remaining) {
                self.consecutive_desync_cycles = 0;
                self.push_degraded_reason("activation:foreground-refused".to_string());
                report.recovery_actions.push(format!(
                    "activation-only-degraded:{}-ops-remain",
                    post_retry_remaining.len()
                ));
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

                if should_auto_unwind_after_desync(
                    &post_retry_remaining,
                    self.consecutive_desync_cycles,
                ) {
                    self.request_emergency_unwind("desync-recovery-escalated");
                    report.recovery_actions.push("safe-mode-unwind".to_string());
                }
            }

            report.management_enabled = self.management_enabled;
            report.degraded_reasons = self.store.state().runtime.degraded_flags.clone();
            Ok(())
        })();
        record_perf_metric(&self.perf.post_apply_validation, started_at, &result);
        result
    }

    fn ingest_created_windows(
        &mut self,
        windows: &[PlatformWindowSnapshot],
        focused_hwnd: Option<u64>,
        follow_active_context: bool,
    ) -> Result<usize, RuntimeError> {
        let mut discovered_windows = 0;

        for window in windows {
            if !window.management_candidate {
                continue;
            }

            if self.find_window_id_by_hwnd(window.hwnd).is_some() {
                continue;
            }

            let Some(actual_monitor_id) = self.monitor_id_by_binding(&window.monitor_binding)
            else {
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
            let monitor_id = if follow_active_context {
                self.discovery_target_monitor_id(actual_monitor_id, decision.managed)
            } else {
                actual_monitor_id
            };
            let discovery_width = self.discovered_width_semantics(&decision, window, monitor_id);
            let placement = self.discovery_placement_for_window(
                monitor_id,
                &decision,
                discovery_width,
                follow_active_context,
            );
            let focus_behavior = self.discovery_focus_behavior_for_window(
                window.hwnd,
                focused_hwnd,
                monitor_id,
                &decision,
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
                    placement,
                    focus_behavior,
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

    fn prune_state_windows_missing_from_snapshot(
        &mut self,
        snapshot: &PlatformSnapshot,
    ) -> Result<usize, RuntimeError> {
        let actual_hwnds = snapshot
            .windows
            .iter()
            .map(|window| window.hwnd)
            .collect::<std::collections::HashSet<_>>();
        let orphaned_window_ids = self
            .store
            .state()
            .windows
            .values()
            .filter_map(|window| {
                window
                    .current_hwnd_binding
                    .filter(|hwnd| !actual_hwnds.contains(hwnd))
                    .map(|_| window.id)
            })
            .collect::<Vec<_>>();
        let mut destroyed_windows = 0;

        for window_id in orphaned_window_ids {
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
            .collect::<std::collections::HashMap<_, _>>();
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
                let target_rect =
                    desired_workspace_window_rect(self.store.state(), workspace, geometry.rect);
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
                            || (apply_plan_context.force_activate_focused_window && needs_geometry)
                    });
                let active_state_changed = apply_plan_context.previous_focused_hwnd
                    != desired_focused_hwnd
                    && (apply_plan_context.previous_focused_hwnd == Some(hwnd)
                        || desired_focused_hwnd == Some(hwnd));
                let visual_emphasis = (!overview_is_open
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
                if needs_geometry || activate || visual_emphasis.is_some() {
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
                    let Some(monitor) = state.monitors.get_mut(&monitor_id) else {
                        self.push_degraded_reason(format!(
                            "missing-monitor-state:{}",
                            monitor_snapshot.binding
                        ));
                        continue;
                    };
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
                let Some(monitor) = state.monitors.get_mut(&monitor_id) else {
                    self.push_degraded_reason(format!("missing-monitor-state:{missing_binding}"));
                    continue;
                };
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

    fn current_focused_hwnd(&self) -> Option<u64> {
        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding)
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

    fn discovery_target_monitor_id(
        &self,
        actual_monitor_id: MonitorId,
        managed: bool,
    ) -> MonitorId {
        if !managed {
            return actual_monitor_id;
        }

        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .and_then(|window| self.store.state().workspaces.get(&window.workspace_id))
            .map(|workspace| workspace.monitor_id)
            .or(self.store.state().focus.focused_monitor_id)
            .unwrap_or(actual_monitor_id)
    }

    fn discovery_placement_for_window(
        &self,
        monitor_id: MonitorId,
        decision: &flowtile_config_rules::WindowRuleDecision,
        discovery_width: WidthSemantics,
        follow_active_context: bool,
    ) -> WindowPlacement {
        if follow_active_context
            && decision.managed
            && decision.layer == WindowLayer::Tiled
            && self.active_context_has_focused_window(monitor_id)
        {
            WindowPlacement::NewColumnAfterFocus {
                mode: decision.column_mode,
                width: discovery_width,
            }
        } else {
            WindowPlacement::AppendToWorkspaceEnd {
                mode: decision.column_mode,
                width: discovery_width,
            }
        }
    }

    fn discovery_focus_behavior_for_window(
        &self,
        hwnd: u64,
        focused_hwnd: Option<u64>,
        monitor_id: MonitorId,
        decision: &flowtile_config_rules::WindowRuleDecision,
    ) -> FocusBehavior {
        if Some(hwnd) == focused_hwnd {
            return FocusBehavior::FollowNewWindow;
        }

        if decision.managed
            && decision.layer == WindowLayer::Tiled
            && self.active_context_window_is_fullscreen(monitor_id)
        {
            return FocusBehavior::FollowNewWindow;
        }

        FocusBehavior::PreserveCurrentFocus
    }

    fn active_context_has_focused_window(&self, monitor_id: MonitorId) -> bool {
        let Some(workspace_id) = self
            .store
            .state()
            .active_workspace_id_for_monitor(monitor_id)
        else {
            return false;
        };

        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .is_some_and(|window| window.workspace_id == workspace_id)
    }

    fn active_context_window_is_fullscreen(&self, monitor_id: MonitorId) -> bool {
        let Some(workspace_id) = self
            .store
            .state()
            .active_workspace_id_for_monitor(monitor_id)
        else {
            return false;
        };

        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .is_some_and(|window| {
                window.workspace_id == workspace_id
                    && (window.layer == WindowLayer::Fullscreen || window.is_fullscreen)
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

    fn discovered_width_semantics(
        &self,
        decision: &flowtile_config_rules::WindowRuleDecision,
        window: &PlatformWindowSnapshot,
        target_monitor_id: MonitorId,
    ) -> WidthSemantics {
        if decision.layer != WindowLayer::Tiled || decision.width_semantics_explicit {
            return decision.width_semantics;
        }

        let observed_width = window.rect.width.max(1);
        let maximum_tiled_width = self.maximum_tiled_width_for_monitor(target_monitor_id);

        WidthSemantics::Fixed(observed_width.min(maximum_tiled_width))
    }

    fn maximum_tiled_width_for_monitor(&self, monitor_id: MonitorId) -> u32 {
        self.store
            .state()
            .monitors
            .get(&monitor_id)
            .map(|monitor| {
                padded_tiled_viewport(monitor.work_area_rect, &self.active_config.projection)
                    .width
                    .max(1)
            })
            .unwrap_or(1)
    }

    fn describe_strip_movements(
        &self,
        snapshot: &PlatformSnapshot,
        operations: &[ApplyOperation],
    ) -> Vec<String> {
        let actual_windows = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window))
            .collect::<std::collections::HashMap<_, _>>();

        operations
            .iter()
            .filter_map(|operation| {
                let window_id = self.find_window_id_by_hwnd(operation.hwnd)?;
                let window = self.store.state().windows.get(&window_id)?;
                if !window.is_managed || window.layer != WindowLayer::Tiled {
                    return None;
                }

                let actual_rect = actual_windows.get(&operation.hwnd).map(|window| window.rect);
                let from_rect = actual_rect.unwrap_or(operation.rect);
                let dx = operation.rect.x as i64 - from_rect.x as i64;
                let dy = operation.rect.y as i64 - from_rect.y as i64;
                let dw = operation.rect.width as i64 - from_rect.width as i64;
                let dh = operation.rect.height as i64 - from_rect.height as i64;
                let animated = operation.window_switch_animation.is_some();

                Some(format!(
                    "strip-move: hwnd={} window_id={} from=({},{} {}x{}) to=({},{} {}x{}) delta=({},{} {}x{}) animated={} activate={}",
                    operation.hwnd,
                    window_id.get(),
                    from_rect.x,
                    from_rect.y,
                    from_rect.width,
                    from_rect.height,
                    operation.rect.x,
                    operation.rect.y,
                    operation.rect.width,
                    operation.rect.height,
                    dx,
                    dy,
                    dw,
                    dh,
                    animated,
                    operation.activate
                ))
            })
            .collect()
    }

    fn describe_window_trace(
        &self,
        stage: &str,
        snapshot: &PlatformSnapshot,
        operations: &[ApplyOperation],
        remaining_label: Option<&str>,
    ) -> Vec<String> {
        let operations_by_hwnd = operations
            .iter()
            .map(|operation| (operation.hwnd, operation))
            .collect::<std::collections::HashMap<_, _>>();
        let state_windows_by_hwnd = self
            .store
            .state()
            .windows
            .values()
            .filter_map(|window| window.current_hwnd_binding.map(|hwnd| (hwnd, window)))
            .collect::<std::collections::HashMap<_, _>>();

        let mut lines = snapshot
            .windows
            .iter()
            .map(|observed_window| {
                let tracked_window = state_windows_by_hwnd.get(&observed_window.hwnd).copied();
                let operation = operations_by_hwnd.get(&observed_window.hwnd).copied();
                format_window_trace_line(
                    stage,
                    remaining_label,
                    observed_window.hwnd,
                    observed_window.process_id,
                    observed_window.process_name.as_deref().unwrap_or("unknown"),
                    tracked_window.map(|window| window.layer),
                    sanitize_log_text(&observed_window.title),
                    observed_window.is_focused,
                    observed_window.management_candidate,
                    tracked_window.is_some_and(|window| window.is_managed),
                    tracked_window.map(|window| window.workspace_id.get()),
                    tracked_window.and_then(|window| window.column_id.map(|id| id.get())),
                    observed_window.rect,
                    operation,
                )
            })
            .collect::<Vec<_>>();

        for operation in operations {
            if snapshot
                .windows
                .iter()
                .any(|window| window.hwnd == operation.hwnd)
            {
                continue;
            }

            let tracked_window = self
                .find_window_id_by_hwnd(operation.hwnd)
                .and_then(|window_id| self.store.state().windows.get(&window_id));
            lines.push(format!(
                "window-trace[{stage}]: hwnd={} process=unknown pid=0 layer={} title=\"missing-from-snapshot\" focused=false candidate=false managed={} workspace={:?} column={:?} observed=missing target=({},{} {}x{}) delta=missing apply_geometry={} activate={} animated={} suppress_gap={} status={}",
                operation.hwnd,
                tracked_window
                    .map(|window| window_layer_name(window.layer))
                    .unwrap_or("untracked"),
                tracked_window.is_some_and(|window| window.is_managed),
                tracked_window.map(|window| window.workspace_id.get()),
                tracked_window.and_then(|window| window.column_id.map(|id| id.get())),
                operation.rect.x,
                operation.rect.y,
                operation.rect.width,
                operation.rect.height,
                operation.apply_geometry,
                operation.activate,
                operation.window_switch_animation.is_some(),
                operation.suppress_visual_gap,
                remaining_label.unwrap_or("planned")
            ));
        }

        lines
    }

    fn adapt_to_platform_min_widths(
        &mut self,
        snapshot: &PlatformSnapshot,
        operations: &[ApplyOperation],
    ) -> Result<bool, RuntimeError> {
        let actual_windows = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window.rect))
            .collect::<std::collections::HashMap<_, _>>();
        let mut adapted = false;

        for operation in operations {
            if !operation.apply_geometry {
                continue;
            }

            let Some(actual_rect) = actual_windows.get(&operation.hwnd).copied() else {
                continue;
            };
            if actual_rect.width <= operation.rect.width {
                continue;
            }

            let same_left = actual_rect.x == operation.rect.x;
            let actual_right = actual_rect.x.saturating_add(actual_rect.width as i32);
            let desired_right = operation.rect.x.saturating_add(operation.rect.width as i32);
            let same_right = actual_right == desired_right;
            if !same_left && !same_right {
                continue;
            }

            let Some(window_id) = self.find_window_id_by_hwnd(operation.hwnd) else {
                continue;
            };
            let Some(window) = self.store.state().windows.get(&window_id).cloned() else {
                continue;
            };
            let Some(column_id) = window.column_id else {
                continue;
            };
            if !window.is_managed || window.layer != WindowLayer::Tiled {
                continue;
            }

            let Some(column) = self.store.state_mut().layout.columns.get_mut(&column_id) else {
                continue;
            };
            if column.width_semantics == WidthSemantics::Fixed(actual_rect.width) {
                continue;
            }

            column.width_semantics = WidthSemantics::Fixed(actual_rect.width);
            adapted = true;
        }

        Ok(adapted)
    }

    fn filter_validatable_operations(
        &self,
        operations: Vec<ApplyOperation>,
    ) -> Vec<ApplyOperation> {
        operations
            .into_iter()
            .filter(|operation| operation.apply_geometry || operation.activate)
            .collect()
    }

    fn filter_validatable_operations_for_snapshot(
        &self,
        snapshot: &PlatformSnapshot,
        operations: Vec<ApplyOperation>,
    ) -> Vec<ApplyOperation> {
        let windows_by_hwnd = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window))
            .collect::<std::collections::HashMap<_, _>>();

        self.filter_validatable_operations(operations)
            .into_iter()
            .filter_map(|operation| {
                let window = windows_by_hwnd.get(&operation.hwnd)?;
                let safety = classify_window_visual_safety(
                    window.process_name.as_deref(),
                    &window.class_name,
                    &window.title,
                );
                if safety == WindowVisualSafety::SafeFullEmphasis {
                    return Some(operation);
                }
                if operation.activate {
                    return Some(ApplyOperation {
                        apply_geometry: false,
                        suppress_visual_gap: false,
                        window_switch_animation: None,
                        ..operation
                    });
                }

                None
            })
            .collect()
    }
}

fn diff_config_sections(previous: &LoadedConfig, current: &LoadedConfig) -> Vec<String> {
    let mut changed_sections = Vec::new();

    if previous.projection.strip_scroll_step != current.projection.strip_scroll_step
        || previous.projection.default_column_mode != current.projection.default_column_mode
        || previous.projection.default_column_width != current.projection.default_column_width
        || previous.projection.layout_spacing != current.projection.layout_spacing
    {
        changed_sections.push("layout".to_string());
    }
    if previous.projection.bind_control_mode != current.projection.bind_control_mode
        || previous.hotkeys != current.hotkeys
        || previous.touchpad != current.touchpad
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

fn desired_workspace_window_rect(
    state: &WmState,
    workspace: &flowtile_domain::Workspace,
    rect: Rect,
) -> Rect {
    let workspace_is_active =
        state.active_workspace_id_for_monitor(workspace.monitor_id) == Some(workspace.id);
    if workspace_is_active {
        return rect;
    }

    inactive_workspace_rect(state, workspace, rect)
}

fn inactive_workspace_rect(
    state: &WmState,
    workspace: &flowtile_domain::Workspace,
    rect: Rect,
) -> Rect {
    let Some(active_workspace_id) = state.active_workspace_id_for_monitor(workspace.monitor_id)
    else {
        return rect;
    };
    let Some(active_workspace) = state.workspaces.get(&active_workspace_id) else {
        return rect;
    };

    let active_region = active_workspace.strip.visible_region;
    let workspace_region = workspace.strip.visible_region;
    let active_index = active_workspace.vertical_index.min(i32::MAX as usize) as i32;
    let workspace_index = workspace.vertical_index.min(i32::MAX as usize) as i32;
    let relative_workspace_offset = workspace_index.saturating_sub(active_index);
    let band_height = active_region.height.min(i32::MAX as u32) as i32;
    let relative_x = rect.x.saturating_sub(workspace_region.x);
    let relative_y = rect.y.saturating_sub(workspace_region.y);

    Rect::new(
        active_region.x.saturating_add(relative_x),
        active_region
            .y
            .saturating_add(relative_workspace_offset.saturating_mul(band_height))
            .saturating_add(relative_y),
        rect.width,
        rect.height,
    )
}

fn operations_are_activation_only(
    snapshot: &PlatformSnapshot,
    operations: &[ApplyOperation],
) -> bool {
    if operations.is_empty() {
        return false;
    }

    let actual_windows = snapshot
        .windows
        .iter()
        .map(|window| (window.hwnd, window.rect))
        .collect::<std::collections::HashMap<_, _>>();

    operations.iter().all(|operation| {
        operation.activate
            && actual_windows
                .get(&operation.hwnd)
                .is_some_and(|actual_rect| !needs_geometry_apply(*actual_rect, operation.rect))
    })
}

fn should_animate_tiled_geometry(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    ) || should_animate_workspace_switch(reason)
}

fn should_animate_workspace_switch(reason: &str) -> bool {
    let token = normalize_reason_token(reason);
    token.contains("focus-workspace-up") || token.contains("focus-workspace-down")
}

fn should_defer_post_apply_retry(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    )
}

fn should_force_activation_reassert(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    )
}

fn visual_emphasis_has_effect(emphasis: &WindowVisualEmphasis) -> bool {
    emphasis.opacity_alpha.is_some()
        || emphasis.force_clear_layered_style
        || emphasis.border_color_rgb.is_some()
        || !emphasis.disable_visual_effects
}

fn should_suppress_visual_gap(
    layer: WindowLayer,
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    layer == WindowLayer::Tiled
        && supports_nonessential_tiled_window_effects(process_name, class_name, title)
}

fn supports_nonessential_tiled_window_effects(
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    classify_window_visual_safety(process_name, class_name, title)
        == WindowVisualSafety::SafeFullEmphasis
}

fn supports_tiled_window_switch_animation(
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    matches!(
        classify_window_visual_safety(process_name, class_name, title),
        WindowVisualSafety::SafeFullEmphasis | WindowVisualSafety::BrowserOpacityOnly
    )
}

fn format_window_trace_line(
    stage: &str,
    remaining_label: Option<&str>,
    hwnd: u64,
    process_id: u32,
    process_name: &str,
    layer: Option<WindowLayer>,
    title: String,
    focused: bool,
    management_candidate: bool,
    managed: bool,
    workspace_id: Option<u64>,
    column_id: Option<u64>,
    observed_rect: Rect,
    operation: Option<&ApplyOperation>,
) -> String {
    let target_rect = operation.map(|item| item.rect);
    let delta = target_rect
        .map(|rect| {
            format!(
                "({},{ } {}x{})",
                rect.x as i64 - observed_rect.x as i64,
                rect.y as i64 - observed_rect.y as i64,
                rect.width as i64 - observed_rect.width as i64,
                rect.height as i64 - observed_rect.height as i64
            )
        })
        .unwrap_or_else(|| "none".to_string())
        .replace(", ", ",");

    format!(
        "window-trace[{stage}]: hwnd={} process={} pid={} layer={} title=\"{}\" focused={} candidate={} managed={} workspace={:?} column={:?} observed=({},{} {}x{}) target={} delta={} apply_geometry={} activate={} animated={} suppress_gap={} status={}",
        hwnd,
        sanitize_log_text(process_name),
        process_id,
        layer.map(window_layer_name).unwrap_or("untracked"),
        title,
        focused,
        management_candidate,
        managed,
        workspace_id,
        column_id,
        observed_rect.x,
        observed_rect.y,
        observed_rect.width,
        observed_rect.height,
        target_rect
            .map(|rect| format!("({},{} {}x{})", rect.x, rect.y, rect.width, rect.height))
            .unwrap_or_else(|| "none".to_string()),
        delta,
        operation.is_some_and(|item| item.apply_geometry),
        operation.is_some_and(|item| item.activate),
        operation.is_some_and(|item| item.window_switch_animation.is_some()),
        operation.is_some_and(|item| item.suppress_visual_gap),
        remaining_label.unwrap_or_else(|| if operation.is_some() {
            "planned"
        } else {
            "steady"
        })
    )
}

fn window_layer_name(layer: WindowLayer) -> &'static str {
    match layer {
        WindowLayer::Tiled => "tiled",
        WindowLayer::Floating => "floating",
        WindowLayer::Fullscreen => "fullscreen",
    }
}

fn sanitize_log_text(text: &str) -> String {
    text.replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('"', "'")
}

fn should_auto_unwind_after_desync(
    remaining_operations: &[ApplyOperation],
    consecutive_desync_cycles: u32,
) -> bool {
    if consecutive_desync_cycles < 3 {
        return false;
    }

    let affected_windows = remaining_operations
        .iter()
        .map(|operation| operation.hwnd)
        .collect::<std::collections::HashSet<_>>();

    affected_windows.len() > 1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowVisualSafety {
    SafeFullEmphasis,
    BrowserOpacityOnly,
    SkipVisualEmphasis,
}

fn build_visual_emphasis(
    is_active_window: bool,
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> WindowVisualEmphasis {
    match classify_window_visual_safety(process_name, class_name, title) {
        WindowVisualSafety::SafeFullEmphasis => WindowVisualEmphasis {
            opacity_alpha: inactive_window_opacity_alpha(is_active_window),
            opacity_mode: WindowOpacityMode::DirectLayered,
            // Active managed windows must always converge back to a fully opaque baseline even
            // if a previous opacity pass or an older daemon run left the HWND layered.
            force_clear_layered_style: is_active_window,
            disable_visual_effects: false,
            border_color_rgb: is_active_window.then_some(rgb_color(0x4C, 0xA8, 0xFF)),
            border_thickness_px: 3,
            rounded_corners: true,
        },
        WindowVisualSafety::BrowserOpacityOnly => WindowVisualEmphasis {
            opacity_alpha: inactive_window_opacity_alpha(is_active_window),
            opacity_mode: WindowOpacityMode::OverlayDim,
            force_clear_layered_style: is_active_window,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        },
        WindowVisualSafety::SkipVisualEmphasis => WindowVisualEmphasis {
            opacity_alpha: None,
            opacity_mode: WindowOpacityMode::DirectLayered,
            force_clear_layered_style: false,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        },
    }
}

fn inactive_window_opacity_alpha(is_active_window: bool) -> Option<u8> {
    if is_active_window { None } else { Some(208) }
}

fn classify_window_visual_safety(
    process_name: Option<&str>,
    class_name: &str,
    _title: &str,
) -> WindowVisualSafety {
    let normalized_class_name = normalize_class_name(class_name);
    let normalized_process_name = normalize_process_name(process_name);
    if normalized_class_name.is_none() && normalized_process_name.is_none() {
        return WindowVisualSafety::SkipVisualEmphasis;
    }
    if matches!(
        normalized_class_name.as_deref(),
        Some("chrome_widgetwin_0" | "chrome_widgetwin_1" | "mozillawindowclass")
    ) {
        return WindowVisualSafety::BrowserOpacityOnly;
    }

    if matches!(
        normalized_class_name.as_deref(),
        Some("org.wezfurlong.wezterm")
    ) {
        return WindowVisualSafety::SkipVisualEmphasis;
    }

    let Some(process_name) = normalized_process_name else {
        return WindowVisualSafety::SafeFullEmphasis;
    };
    if matches!(
        process_name.as_str(),
        "msedge"
            | "chrome"
            | "brave"
            | "opera"
            | "vivaldi"
            | "chromium"
            | "firefox"
            | "librewolf"
            | "waterfox"
    ) {
        return WindowVisualSafety::BrowserOpacityOnly;
    }

    if matches!(process_name.as_str(), "wezterm-gui") {
        return WindowVisualSafety::SkipVisualEmphasis;
    }

    WindowVisualSafety::SafeFullEmphasis
}

fn normalize_process_name(process_name: Option<&str>) -> Option<String> {
    let process_name = process_name?.trim();
    if process_name.is_empty() {
        return None;
    }

    let lowered = process_name.to_ascii_lowercase();
    Some(
        lowered
            .strip_suffix(".exe")
            .unwrap_or(lowered.as_str())
            .to_string(),
    )
}

fn normalize_class_name(class_name: &str) -> Option<String> {
    let class_name = class_name.trim();
    if class_name.is_empty() {
        return None;
    }

    Some(class_name.to_ascii_lowercase())
}

const fn rgb_color(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
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
    workspace_root().join(relative_path)
}

fn workspace_root() -> PathBuf {
    if let Ok(root) = std::env::var("FLOWTILE_WORKSPACE_ROOT") {
        return PathBuf::from(root);
    }

    if let Ok(current_dir) = std::env::current_dir()
        && let Some(root) = find_workspace_root(&current_dir)
    {
        return root;
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(exe_dir) = current_exe.parent()
        && let Some(root) = find_workspace_root(exe_dir)
    {
        return root;
    }

    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .to_path_buf()
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("config").is_dir() {
            return Some(candidate.to_path_buf());
        }
    }

    None
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
    use flowtile_config_rules::WindowRuleDecision;
    use flowtile_domain::{
        CorrelationId, DomainEvent, NavigationScope, Rect, ResizeEdge, RuntimeMode, WidthSemantics,
        WindowLayer,
    };
    use flowtile_layout_engine::recompute_workspace;
    use flowtile_windows_adapter::{
        ApplyOperation, PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot,
        WindowOpacityMode,
    };

    use crate::CoreDaemonRuntime;

    use super::{
        ApplyPlanContext, build_visual_emphasis, operations_are_activation_only,
        should_auto_unwind_after_desync,
    };

    #[test]
    fn treats_focus_mismatch_without_geometry_drift_as_activation_only() {
        let snapshot = sample_snapshot(Rect::new(0, 0, 420, 900));
        let operations = vec![ApplyOperation {
            hwnd: 100,
            rect: Rect::new(0, 0, 420, 900),
            apply_geometry: true,
            activate: true,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: None,
        }];

        assert!(operations_are_activation_only(&snapshot, &operations));
    }

    #[test]
    fn does_not_treat_geometry_retry_as_activation_only() {
        let snapshot = sample_snapshot(Rect::new(20, 0, 420, 900));
        let operations = vec![ApplyOperation {
            hwnd: 100,
            rect: Rect::new(0, 0, 420, 900),
            apply_geometry: true,
            activate: true,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: None,
        }];

        assert!(!operations_are_activation_only(&snapshot, &operations));
    }

    #[test]
    fn single_window_desync_does_not_force_auto_unwind() {
        let operations = vec![ApplyOperation {
            hwnd: 100,
            rect: Rect::new(0, 0, 420, 900),
            apply_geometry: true,
            activate: true,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: None,
        }];

        assert!(!should_auto_unwind_after_desync(&operations, 3));
    }

    #[test]
    fn multi_window_persistent_desync_can_force_auto_unwind() {
        let operations = vec![
            ApplyOperation {
                hwnd: 100,
                rect: Rect::new(0, 0, 420, 900),
                apply_geometry: true,
                activate: true,
                suppress_visual_gap: false,
                window_switch_animation: None,
                visual_emphasis: None,
            },
            ApplyOperation {
                hwnd: 200,
                rect: Rect::new(420, 0, 420, 900),
                apply_geometry: true,
                activate: false,
                suppress_visual_gap: false,
                window_switch_animation: None,
                visual_emphasis: None,
            },
        ];

        assert!(should_auto_unwind_after_desync(&operations, 3));
    }

    #[test]
    fn discovery_without_explicit_width_uses_observed_width_below_padded_limit() {
        let decision = WindowRuleDecision {
            layer: WindowLayer::Tiled,
            managed: true,
            column_mode: flowtile_domain::ColumnMode::Normal,
            width_semantics: WidthSemantics::MonitorFraction {
                numerator: 1,
                denominator: 2,
            },
            width_semantics_explicit: false,
            matched_rule_ids: Vec::new(),
        };
        let window = PlatformWindowSnapshot {
            hwnd: 100,
            title: "Window 100".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(0, 0, 420, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        };

        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let monitor_id =
            runtime
                .store
                .state_mut()
                .add_monitor(Rect::new(0, 0, 1200, 900), 96, true);

        assert_eq!(
            runtime.discovered_width_semantics(&decision, &window, monitor_id),
            WidthSemantics::Fixed(420)
        );
    }

    #[test]
    fn discovery_without_explicit_width_clamps_observed_width_to_padded_limit() {
        let decision = WindowRuleDecision {
            layer: WindowLayer::Tiled,
            managed: true,
            column_mode: flowtile_domain::ColumnMode::Normal,
            width_semantics: WidthSemantics::MonitorFraction {
                numerator: 1,
                denominator: 2,
            },
            width_semantics_explicit: false,
            matched_rule_ids: Vec::new(),
        };
        let window = PlatformWindowSnapshot {
            hwnd: 100,
            title: "Window 100".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(0, 0, 4000, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let monitor_id =
            runtime
                .store
                .state_mut()
                .add_monitor(Rect::new(0, 0, 1200, 900), 96, true);

        assert_eq!(
            runtime.discovered_width_semantics(&decision, &window, monitor_id),
            WidthSemantics::Fixed(1168)
        );
    }

    #[test]
    fn discovery_with_explicit_rule_width_keeps_rule_semantics() {
        let decision = WindowRuleDecision {
            layer: WindowLayer::Tiled,
            managed: true,
            column_mode: flowtile_domain::ColumnMode::Normal,
            width_semantics: WidthSemantics::Fixed(560),
            width_semantics_explicit: true,
            matched_rule_ids: vec!["prefer-wide-column".to_string()],
        };
        let window = PlatformWindowSnapshot {
            hwnd: 100,
            title: "Window 100".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(0, 0, 420, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let monitor_id =
            runtime
                .store
                .state_mut()
                .add_monitor(Rect::new(0, 0, 1200, 900), 96, true);

        assert_eq!(
            runtime.discovered_width_semantics(&decision, &window, monitor_id),
            WidthSemantics::Fixed(560)
        );
    }

    #[test]
    fn initial_snapshot_plan_uses_observed_bootstrap_widths_inside_padded_viewport() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
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
                    rect: Rect::new(0, 0, 320, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(430, 0, 320, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 102,
                    title: "Window 102".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(860, 0, 320, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        let planned_operations = runtime
            .plan_apply_operations(&snapshot)
            .expect("apply plan should be computed");

        assert_eq!(planned_operations.len(), 3);
        assert_eq!(planned_operations[0].hwnd, 100);
        assert_eq!(planned_operations[0].rect.x, 16);
        assert_eq!(planned_operations[0].rect.width, 320);
        assert!(planned_operations[0].suppress_visual_gap);
        assert_eq!(planned_operations[1].hwnd, 101);
        assert_eq!(planned_operations[1].rect.x, 348);
        assert_eq!(planned_operations[1].rect.width, 320);
        assert!(planned_operations[1].suppress_visual_gap);
        assert_eq!(planned_operations[2].hwnd, 102);
        assert_eq!(planned_operations[2].rect.x, 680);
        assert_eq!(planned_operations[2].rect.width, 320);
        assert!(planned_operations[2].suppress_visual_gap);
    }

    #[test]
    fn focus_next_to_last_bootstrap_column_keeps_right_outer_padding() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
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
                    rect: Rect::new(0, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(1180, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(2),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus navigation should succeed");

        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: true,
                    animate_tiled_geometry: false,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: false,
                },
            )
            .expect("apply plan should be computed");
        let last = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("last column operation should exist");

        assert_eq!(last.rect.width, 1168);
        assert_eq!(last.rect.x + last.rect.width as i32, 1184);
    }

    #[test]
    fn new_managed_window_follows_active_monitor_context() {
        let initial_snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![
                PlatformMonitorSnapshot {
                    binding: "\\\\.\\DISPLAY1".to_string(),
                    work_area_rect: Rect::new(0, 0, 1200, 900),
                    dpi: 96,
                    is_primary: true,
                },
                PlatformMonitorSnapshot {
                    binding: "\\\\.\\DISPLAY2".to_string(),
                    work_area_rect: Rect::new(1200, 0, 1200, 900),
                    dpi: 96,
                    is_primary: false,
                },
            ],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(1200, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(initial_snapshot, true)
            .expect("initial sync should succeed");

        let snapshot_with_new_window = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![
                PlatformMonitorSnapshot {
                    binding: "\\\\.\\DISPLAY1".to_string(),
                    work_area_rect: Rect::new(0, 0, 1200, 900),
                    dpi: 96,
                    is_primary: true,
                },
                PlatformMonitorSnapshot {
                    binding: "\\\\.\\DISPLAY2".to_string(),
                    work_area_rect: Rect::new(1200, 0, 1200, 900),
                    dpi: 96,
                    is_primary: false,
                },
            ],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Window 100".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(1200, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4343,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };
        runtime
            .sync_snapshot(snapshot_with_new_window, true)
            .expect("second sync should succeed");

        let new_window_id = runtime
            .find_window_id_by_hwnd(101)
            .expect("new window should exist");
        let new_window = runtime
            .state()
            .windows
            .get(&new_window_id)
            .expect("new window should be tracked");
        let workspace = runtime
            .state()
            .workspaces
            .get(&new_window.workspace_id)
            .expect("workspace should exist");
        let monitor = runtime
            .state()
            .monitors
            .get(&workspace.monitor_id)
            .expect("monitor should exist");

        assert_eq!(monitor.platform_binding.as_deref(), Some("\\\\.\\DISPLAY2"));
    }

    #[test]
    fn focus_navigation_plan_marks_tiled_moves_for_window_switch_animation() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
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
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(420, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(2),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus navigation should succeed");

        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: true,
                    animate_tiled_geometry: false,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: false,
                },
            )
            .expect("apply plan should be computed");

        assert!(
            planned_operations
                .iter()
                .all(|operation| operation.window_switch_animation.is_some())
        );
        assert!(
            planned_operations
                .iter()
                .any(|operation| operation.hwnd == 101 && operation.activate)
        );
    }

    #[test]
    fn active_window_change_refreshes_visual_emphasis_for_old_and_new_focus() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
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
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Window 101".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(420, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(2),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus navigation should succeed");

        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: true,
                    animate_tiled_geometry: false,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: false,
                },
            )
            .expect("apply plan should be computed");

        let previous_focus = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("previous focus operation should exist");
        let new_focus = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("new focus operation should exist");

        assert_eq!(
            previous_focus.visual_emphasis,
            Some(build_visual_emphasis(
                false,
                Some("notepad"),
                "Notepad",
                "notes"
            ))
        );
        assert_eq!(
            new_focus.visual_emphasis,
            Some(build_visual_emphasis(
                true,
                Some("notepad"),
                "Notepad",
                "notes"
            ))
        );
    }

    #[test]
    fn refresh_visual_emphasis_context_updates_inactive_browser_without_geometry_change() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "PowerShell".to_string(),
                    class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                    process_id: 4242,
                    process_name: Some("WindowsTerminal".to_string()),
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Example page".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 4343,
                    process_name: Some("msedge".to_string()),
                    rect: Rect::new(420, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");

        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: false,
                    animate_tiled_geometry: false,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: true,
                },
            )
            .expect("apply plan should be computed");

        let edge_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("inactive browser operation should exist");
        assert_eq!(
            edge_operation.visual_emphasis,
            Some(build_visual_emphasis(
                false,
                Some("msedge"),
                "Chrome_WidgetWin_1",
                "Example page"
            ))
        );
    }

    #[test]
    fn cycle_column_width_uses_next_greater_step_for_custom_width() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 500, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        runtime
            .sync_snapshot(snapshot, true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::cycle_column_width(CorrelationId::new(10)))
            .expect("width cycle should succeed");

        let target = runtime
            .active_tiled_resize_target()
            .expect("active target lookup should succeed")
            .expect("active tiled target should exist");
        let column = runtime
            .state()
            .layout
            .columns
            .get(&target.column_id)
            .expect("column should exist after width cycle");

        assert_eq!(column.width_semantics, WidthSemantics::Fixed(584));
    }

    #[test]
    fn cycle_column_width_reasserts_activation_for_the_focused_window() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 500, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::cycle_column_width(CorrelationId::new(10)))
            .expect("width cycle should succeed");

        let apply_plan_context = runtime.build_apply_plan_context(
            Some(100),
            Some(100),
            "manual-cycle-column-width",
            false,
        );
        let planned_operations = runtime
            .plan_apply_operations_with_context(&snapshot, apply_plan_context)
            .expect("apply plan should be computed");

        let active_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("focused window operation should exist");

        assert!(active_operation.apply_geometry);
        assert!(active_operation.activate);
        assert_eq!(
            active_operation.visual_emphasis,
            Some(build_visual_emphasis(
                true,
                Some("notepad"),
                "Notepad",
                "Window 100",
            ))
        );
    }

    #[test]
    fn manual_width_resize_commit_persists_fixed_width_and_clears_preview() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        runtime
            .sync_snapshot(snapshot, true)
            .expect("initial sync should succeed");
        let target = runtime
            .active_tiled_resize_target()
            .expect("active target lookup should succeed")
            .expect("active tiled target should exist");
        let initial_right = target.rect.x + target.rect.width as i32;

        assert!(
            runtime
                .begin_column_width_resize(ResizeEdge::Right, initial_right)
                .expect("begin resize should succeed")
        );
        runtime
            .update_column_width_resize(initial_right + 120)
            .expect("preview update should succeed");

        let preview_rect = runtime
            .manual_width_resize_preview_rect()
            .expect("preview should exist during active resize");
        assert_eq!(preview_rect.width, 120);

        runtime
            .store
            .dispatch(DomainEvent::commit_column_width(
                CorrelationId::new(11),
                initial_right + 120,
            ))
            .expect("commit should succeed");

        let column = runtime
            .state()
            .layout
            .columns
            .get(&target.column_id)
            .expect("column should exist after width commit");
        assert_eq!(column.width_semantics, WidthSemantics::Fixed(540));
        assert!(runtime.manual_width_resize_preview_rect().is_none());
    }

    #[test]
    fn strip_movement_log_keeps_negative_delta_when_window_moves_left() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = sample_snapshot(Rect::new(120, 40, 420, 900));

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");

        let logs = runtime.describe_strip_movements(
            &snapshot,
            &[ApplyOperation {
                hwnd: 100,
                rect: Rect::new(16, 40, 420, 900),
                apply_geometry: true,
                activate: false,
                suppress_visual_gap: true,
                window_switch_animation: None,
                visual_emphasis: None,
            }],
        );

        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("delta=(-104,0 0x0)"));
    }

    #[test]
    fn chromium_windows_use_overlay_dim_only_to_avoid_composited_surface_regressions() {
        let inactive_edge = build_visual_emphasis(
            false,
            Some("msedge.exe"),
            "Chrome_WidgetWin_1",
            "Example page",
        );
        assert_eq!(inactive_edge.opacity_alpha, Some(208));
        assert_eq!(inactive_edge.opacity_mode, WindowOpacityMode::OverlayDim);
        assert!(!inactive_edge.force_clear_layered_style);
        assert!(inactive_edge.disable_visual_effects);
        assert_eq!(inactive_edge.border_color_rgb, None);
        assert!(!inactive_edge.rounded_corners);
        assert!(super::visual_emphasis_has_effect(&inactive_edge));

        let active_chrome =
            build_visual_emphasis(true, Some("chrome"), "Chrome_WidgetWin_1", "Example page");
        assert_eq!(active_chrome.opacity_alpha, None);
        assert_eq!(active_chrome.opacity_mode, WindowOpacityMode::OverlayDim);
        assert!(active_chrome.force_clear_layered_style);
        assert!(active_chrome.disable_visual_effects);
        assert_eq!(active_chrome.border_color_rgb, None);
        assert!(!active_chrome.rounded_corners);
        assert!(super::visual_emphasis_has_effect(&active_chrome));

        assert_eq!(
            build_visual_emphasis(
                false,
                Some("WindowsTerminal.exe"),
                "CASCADIA_HOSTING_WINDOW_CLASS",
                "PowerShell",
            )
            .opacity_alpha,
            Some(208)
        );
        assert!(!super::visual_emphasis_has_effect(&build_visual_emphasis(
            false,
            Some("WezTerm-gui.exe"),
            "org.wezfurlong.wezterm",
            "WezTerm",
        )));
        assert_eq!(
            build_visual_emphasis(false, Some("notepad.exe"), "Notepad", "notes").opacity_alpha,
            Some(208)
        );
        assert_eq!(
            build_visual_emphasis(false, Some("notepad.exe"), "Notepad", "notes").opacity_mode,
            WindowOpacityMode::DirectLayered
        );
        assert!(
            !build_visual_emphasis(false, Some("notepad.exe"), "Notepad", "notes")
                .force_clear_layered_style
        );
        assert!(
            !build_visual_emphasis(false, Some("notepad"), "Notepad", "notes")
                .disable_visual_effects
        );
        assert_eq!(
            build_visual_emphasis(true, Some("notepad.exe"), "Notepad", "notes").opacity_alpha,
            None
        );
    }

    #[test]
    fn active_chromium_windows_clear_layered_style_without_border_or_corners() {
        let emphasis = build_visual_emphasis(
            true,
            Some("msedge.exe"),
            "Chrome_WidgetWin_1",
            "Example page",
        );
        assert!(super::visual_emphasis_has_effect(&emphasis));
        assert!(emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.opacity_alpha, None);
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::OverlayDim);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(!emphasis.rounded_corners);
    }

    #[test]
    fn active_safe_window_requests_full_opacity_cleanup_before_border_effects() {
        let emphasis = build_visual_emphasis(true, Some("notepad.exe"), "Notepad", "notes");
        assert!(emphasis.force_clear_layered_style);
        assert!(!emphasis.disable_visual_effects);
        assert_eq!(emphasis.opacity_alpha, None);
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::DirectLayered);
        assert!(emphasis.border_color_rgb.is_some());
        assert!(emphasis.rounded_corners);
    }

    #[test]
    fn inactive_chromium_windows_use_overlay_dim_visual_emphasis() {
        let emphasis = build_visual_emphasis(
            false,
            Some("msedge.exe"),
            "Chrome_WidgetWin_1",
            "Example page",
        );
        assert_eq!(emphasis.opacity_alpha, Some(208));
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::OverlayDim);
        assert!(!emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(!emphasis.rounded_corners);
        assert!(super::visual_emphasis_has_effect(&emphasis));
    }

    #[test]
    fn inactive_new_tab_browser_windows_use_overlay_dim_visual_emphasis() {
        let emphasis = build_visual_emphasis(
            false,
            Some("msedge.exe"),
            "Chrome_WidgetWin_1",
            "Новая вкладка — Личный: Microsoft Edge",
        );
        assert_eq!(emphasis.opacity_alpha, Some(208));
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::OverlayDim);
        assert!(!emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(!emphasis.rounded_corners);
        assert!(super::visual_emphasis_has_effect(&emphasis));
    }

    #[test]
    fn chromium_like_class_without_browser_process_still_uses_overlay_dim_visual_emphasis() {
        let emphasis = build_visual_emphasis(
            false,
            Some("Code.exe"),
            "Chrome_WidgetWin_1",
            "Visual Studio Code",
        );
        assert!(!emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.opacity_alpha, Some(208));
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::OverlayDim);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(super::visual_emphasis_has_effect(&emphasis));
    }

    #[test]
    fn chromium_like_class_without_process_name_still_uses_overlay_dim_visual_emphasis() {
        let emphasis = build_visual_emphasis(false, None, "Chrome_WidgetWin_1", "Microsoft Edge");
        assert_eq!(emphasis.opacity_alpha, Some(208));
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::OverlayDim);
        assert!(!emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(!emphasis.rounded_corners);
        assert!(super::visual_emphasis_has_effect(&emphasis));
    }

    #[test]
    fn unknown_window_metadata_skips_visual_emphasis_until_discovery_settles() {
        let emphasis = build_visual_emphasis(false, None, "", "");
        assert_eq!(emphasis.opacity_alpha, None);
        assert_eq!(emphasis.opacity_mode, WindowOpacityMode::DirectLayered);
        assert!(!emphasis.force_clear_layered_style);
        assert!(emphasis.disable_visual_effects);
        assert_eq!(emphasis.border_color_rgb, None);
        assert!(!emphasis.rounded_corners);
        assert!(!super::visual_emphasis_has_effect(&emphasis));
    }

    #[test]
    fn chromium_windows_skip_gapless_visual_policy_during_geometry_apply() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "PowerShell".to_string(),
                    class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                    process_id: 4242,
                    process_name: Some("WindowsTerminal".to_string()),
                    rect: Rect::new(0, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Example page".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 4343,
                    process_name: Some("msedge".to_string()),
                    rect: Rect::new(0, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        let planned_operations = runtime
            .plan_apply_operations(&snapshot)
            .expect("apply plan should be computed");

        let terminal_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("terminal operation should exist");
        let edge_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("edge operation should exist");

        assert!(terminal_operation.suppress_visual_gap);
        assert!(!edge_operation.suppress_visual_gap);
        assert!(edge_operation.apply_geometry);
    }

    #[test]
    fn chromium_windows_keep_window_switch_animation_during_geometry_apply() {
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "PowerShell".to_string(),
                    class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                    process_id: 4242,
                    process_name: Some("WindowsTerminal".to_string()),
                    rect: Rect::new(0, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Example page".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 4343,
                    process_name: Some("msedge".to_string()),
                    rect: Rect::new(0, 0, 1180, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: true,
                    animate_tiled_geometry: true,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: false,
                },
            )
            .expect("apply plan should be computed");

        let terminal_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("terminal operation should exist");
        let edge_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("edge operation should exist");

        assert!(terminal_operation.window_switch_animation.is_some());
        assert!(edge_operation.apply_geometry);
        assert!(edge_operation.window_switch_animation.is_some());
    }

    #[test]
    fn validation_filter_for_snapshot_skips_chromium_geometry_retry() {
        let runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(101),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 101,
                title: "Example page".to_string(),
                class_name: "Chrome_WidgetWin_1".to_string(),
                process_id: 4343,
                process_name: Some("msedge".to_string()),
                rect: Rect::new(0, 0, 1180, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        let filtered = runtime.filter_validatable_operations_for_snapshot(
            &snapshot,
            vec![ApplyOperation {
                hwnd: 101,
                rect: Rect::new(16, 16, 1000, 900),
                apply_geometry: true,
                activate: false,
                suppress_visual_gap: false,
                window_switch_animation: None,
                visual_emphasis: None,
            }],
        );

        assert!(filtered.is_empty());
    }

    #[test]
    fn validation_filter_for_snapshot_keeps_browser_activation_retry_but_drops_geometry_retry() {
        let runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(101),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 101,
                title: "Example page".to_string(),
                class_name: "Chrome_WidgetWin_1".to_string(),
                process_id: 4343,
                process_name: Some("msedge".to_string()),
                rect: Rect::new(0, 0, 1180, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        let filtered = runtime.filter_validatable_operations_for_snapshot(
            &snapshot,
            vec![ApplyOperation {
                hwnd: 101,
                rect: Rect::new(16, 16, 1000, 900),
                apply_geometry: true,
                activate: true,
                suppress_visual_gap: false,
                window_switch_animation: None,
                visual_emphasis: Some(build_visual_emphasis(
                    true,
                    Some("msedge"),
                    "Chrome_WidgetWin_1",
                    "Example page",
                )),
            }],
        );

        assert_eq!(filtered.len(), 1);
        assert!(!filtered[0].apply_geometry);
        assert!(filtered[0].activate);
    }

    #[test]
    fn validation_filter_for_snapshot_keeps_safe_window_geometry_retry() {
        let runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1200, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "notes".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 1180, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };

        let filtered = runtime.filter_validatable_operations_for_snapshot(
            &snapshot,
            vec![ApplyOperation {
                hwnd: 100,
                rect: Rect::new(16, 16, 1000, 900),
                apply_geometry: true,
                activate: false,
                suppress_visual_gap: true,
                window_switch_animation: None,
                visual_emphasis: None,
            }],
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].hwnd, 100);
    }

    #[test]
    fn validation_filter_ignores_non_observable_browser_visual_only_operation() {
        let runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let filtered = runtime.filter_validatable_operations(vec![ApplyOperation {
            hwnd: 100,
            rect: Rect::new(0, 0, 400, 900),
            apply_geometry: false,
            activate: false,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: Some(build_visual_emphasis(
                true,
                Some("msedge.exe"),
                "Chrome_WidgetWin_1",
                "Example page",
            )),
        }]);

        assert!(filtered.is_empty());
    }

    #[test]
    fn validation_filter_ignores_non_observable_visual_emphasis_only_operation() {
        let runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        let filtered = runtime.filter_validatable_operations(vec![ApplyOperation {
            hwnd: 100,
            rect: Rect::new(0, 0, 400, 900),
            apply_geometry: false,
            activate: false,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: Some(build_visual_emphasis(
                true,
                Some("notepad.exe"),
                "Notepad",
                "notes",
            )),
        }]);

        assert!(filtered.is_empty());
    }

    #[test]
    fn focus_workspace_down_moves_previous_workspace_windows_into_vertical_stack() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");

        let monitor_id = *runtime
            .state()
            .monitors
            .keys()
            .next()
            .expect("monitor should exist");
        let workspace_ids = ordered_workspace_ids_for_monitor(runtime.state(), monitor_id);
        assert_eq!(workspace_ids.len(), 2);

        runtime
            .store
            .dispatch(DomainEvent::focus_workspace_down(
                CorrelationId::new(2),
                None,
            ))
            .expect("focus workspace down should succeed");

        let planned_operations = runtime
            .plan_apply_operations_with_context(
                &snapshot,
                ApplyPlanContext {
                    previous_focused_hwnd: Some(100),
                    animate_window_switch: false,
                    animate_tiled_geometry: false,
                    force_activate_focused_window: false,
                    refresh_visual_emphasis: true,
                },
            )
            .expect("apply plan should be computed");
        let operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("previous workspace window should be moved away");
        let previous_window_id = runtime
            .find_window_id_by_hwnd(100)
            .expect("previous workspace window should exist");
        let previous_workspace_projection = recompute_workspace(runtime.state(), workspace_ids[0])
            .expect("previous workspace projection should exist");
        let previous_local_rect = previous_workspace_projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == previous_window_id)
            .expect("previous workspace geometry should exist")
            .rect;

        assert_eq!(
            runtime.state().active_workspace_id_for_monitor(monitor_id),
            Some(workspace_ids[1])
        );
        assert_eq!(runtime.state().focus.focused_window_id, None);
        assert!(operation.apply_geometry);
        assert!(!operation.activate);
        assert_eq!(
            operation.rect.y,
            previous_local_rect
                .y
                .saturating_sub(snapshot.monitors[0].work_area_rect.height as i32)
        );
    }

    #[test]
    fn focus_workspace_down_uses_workspace_switch_animation_baseline() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            }],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_workspace_down(
                CorrelationId::new(2),
                None,
            ))
            .expect("focus workspace down should succeed");

        let apply_plan_context =
            runtime.build_apply_plan_context(Some(100), None, "manual-focus-workspace-down", true);
        let planned_operations = runtime
            .plan_apply_operations_with_context(&snapshot, apply_plan_context)
            .expect("apply plan should be computed");
        let operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .expect("previous workspace window should be moved away");

        assert!(operation.apply_geometry);
        assert!(operation.window_switch_animation.is_some());
    }

    #[test]
    fn overview_open_suppresses_live_activation_and_visual_emphasis() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
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
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Microsoft Edge".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 4343,
                    process_name: Some("msedge".to_string()),
                    rect: Rect::new(420, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::open_overview(CorrelationId::new(2), None))
            .expect("open overview should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(3),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus navigation should succeed");

        let apply_plan_context =
            runtime.build_apply_plan_context(Some(100), Some(101), "manual-focus-next", true);
        let planned_operations = runtime
            .plan_apply_operations_with_context(&snapshot, apply_plan_context)
            .expect("apply plan should be computed");

        assert!(
            !planned_operations
                .iter()
                .any(|operation| operation.activate),
            "overview should suppress live activation while it is open"
        );
        assert!(
            planned_operations
                .iter()
                .all(|operation| operation.visual_emphasis.is_none()),
            "overview should suppress live visual emphasis while it is open"
        );
    }

    #[test]
    fn closing_overview_restores_activation_for_new_focus() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
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
                    rect: Rect::new(0, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Microsoft Edge".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 4343,
                    process_name: Some("msedge".to_string()),
                    rect: Rect::new(420, 0, 420, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };
        let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
        runtime
            .sync_snapshot(snapshot.clone(), true)
            .expect("initial sync should succeed");
        runtime
            .store
            .dispatch(DomainEvent::open_overview(CorrelationId::new(2), None))
            .expect("open overview should succeed");
        runtime
            .store
            .dispatch(DomainEvent::focus_next(
                CorrelationId::new(3),
                NavigationScope::WorkspaceStrip,
            ))
            .expect("focus navigation should succeed");
        runtime
            .store
            .dispatch(DomainEvent::close_overview(CorrelationId::new(4), None))
            .expect("close overview should succeed");

        let apply_plan_context =
            runtime.build_apply_plan_context(Some(100), Some(101), "manual-close-overview", true);
        let planned_operations = runtime
            .plan_apply_operations_with_context(&snapshot, apply_plan_context)
            .expect("apply plan should be computed");
        let edge_operation = planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .expect("new focused edge window should be planned");

        assert!(edge_operation.activate);
    }

    fn sample_snapshot(window_rect: Rect) -> PlatformSnapshot {
        PlatformSnapshot {
            foreground_hwnd: None,
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![PlatformWindowSnapshot {
                hwnd: 100,
                title: "Window 100".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: window_rect,
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            }],
        }
    }

    fn ordered_workspace_ids_for_monitor(
        state: &flowtile_domain::WmState,
        monitor_id: flowtile_domain::MonitorId,
    ) -> Vec<flowtile_domain::WorkspaceId> {
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        state
            .workspace_sets
            .get(&workspace_set_id)
            .expect("workspace set should exist")
            .ordered_workspace_ids
            .clone()
    }
}
