use super::apply_policy::WindowTraceLine;
use super::*;

impl CoreDaemonRuntime {
    pub(super) fn validate_after_apply(
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

    pub(super) fn describe_strip_movements(
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

    pub(super) fn describe_window_trace(
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
                format_window_trace_line(WindowTraceLine {
                    stage,
                    remaining_label,
                    hwnd: observed_window.hwnd,
                    process_id: observed_window.process_id,
                    process_name: observed_window.process_name.as_deref().unwrap_or("unknown"),
                    layer: tracked_window.map(|window| window.layer),
                    title: sanitize_log_text(&observed_window.title),
                    focused: observed_window.is_focused,
                    management_candidate: observed_window.management_candidate,
                    managed: tracked_window.is_some_and(|window| window.is_managed),
                    workspace_id: tracked_window.map(|window| window.workspace_id.get()),
                    column_id: tracked_window
                        .and_then(|window| window.column_id)
                        .map(|id| id.get()),
                    observed_rect: observed_window.rect,
                    operation,
                })
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

    pub(super) fn filter_validatable_operations(
        &self,
        operations: Vec<ApplyOperation>,
    ) -> Vec<ApplyOperation> {
        operations
            .into_iter()
            .filter(|operation| operation.apply_geometry || operation.activate)
            .collect()
    }

    pub(super) fn filter_validatable_operations_for_snapshot(
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
