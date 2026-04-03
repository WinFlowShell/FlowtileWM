use super::*;

struct DiscoveryReconcileResult {
    discovered_windows: usize,
    trace_logs: Vec<String>,
}

impl CoreDaemonRuntime {
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

    pub(super) fn sync_snapshot_with_reason(
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
            let reclassified_windows =
                self.reclassify_state_windows_for_monitor_policy(&snapshot)?;

            let discovery_result = self.reconcile_pending_window_discoveries(
                &snapshot,
                snapshot.actual_foreground_hwnd(),
                had_previous_snapshot,
            )?;
            let discovered_windows = discovery_result.discovered_windows;
            let destroyed_windows = reclassified_windows
                + self.ingest_destroyed_windows(&diff.destroyed_hwnds)?
                + self.prune_state_windows_missing_from_snapshot(&snapshot)?;
            let stale_materialized_presentations = stale_materialized_presentation_hwnds(
                &snapshot,
                &self.adapter.materialized_presentation_hwnds(),
            );
            let mut stale_presentation_cleanup_count = 0;

            if !dry_run
                && !stale_materialized_presentations.is_empty()
                && let Err(error) = self
                    .adapter
                    .clear_window_presentations(&stale_materialized_presentations)
            {
                self.push_degraded_reason(format!("stale-presentation-cleanup-failed:{error}"));
            } else if !dry_run {
                stale_presentation_cleanup_count = stale_materialized_presentations.len();
            }

            if had_previous_snapshot && diff.monitor_topology_changed {
                self.push_degraded_reason("monitor-topology-changed".to_string());
            }

            self.refresh_pending_focus_claim(snapshot.actual_foreground_hwnd());
            if let Some(focused_hwnd) =
                self.refresh_pending_platform_focus_candidate(snapshot.actual_foreground_hwnd())
                && self.current_focused_hwnd() != Some(focused_hwnd)
            {
                self.observe_focus(focused_hwnd)?;
            }
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
            let mut recovery_actions = Vec::new();
            if stale_presentation_cleanup_count > 0 {
                recovery_actions.push(format!(
                    "stale-presentation-cleanup:{}",
                    stale_presentation_cleanup_count
                ));
            }

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
                recovery_actions,
                management_enabled: self.management_enabled,
                dry_run,
                degraded_reasons: self.store.state().runtime.degraded_flags.clone(),
                discovery_trace_logs: discovery_result.trace_logs,
                strip_movement_logs,
                window_trace_logs,
                validation_trace_logs: Vec::new(),
            })
        })();
        record_perf_metric(&self.perf.observation_sync, started_at, &result);
        result
    }

    fn reconcile_pending_window_discoveries(
        &mut self,
        snapshot: &PlatformSnapshot,
        focused_hwnd: Option<u64>,
        had_previous_snapshot: bool,
    ) -> Result<DiscoveryReconcileResult, RuntimeError> {
        let mut discovered_windows = 0;
        let mut trace_logs = Vec::new();
        let follow_active_context = had_previous_snapshot;
        let assessments = self.adapter.assess_discovery_windows(snapshot);
        let actual_hwnds = snapshot
            .windows
            .iter()
            .map(|window| window.hwnd)
            .collect::<std::collections::HashSet<_>>();
        let bound_hwnds = self
            .store
            .state()
            .windows
            .values()
            .filter_map(|window| window.current_hwnd_binding)
            .collect::<std::collections::HashSet<_>>();

        self.pending_discoveries
            .retain(|hwnd, _| actual_hwnds.contains(hwnd) && !bound_hwnds.contains(hwnd));

        for window in &snapshot.windows {
            if self.find_window_id_by_hwnd(window.hwnd).is_some() {
                let _ = self.pending_discoveries.remove(&window.hwnd);
                continue;
            }

            let Some(assessment) = assessments.get(&window.hwnd) else {
                continue;
            };

            match assessment.disposition {
                PlatformWindowDisposition::RejectedNoise
                | PlatformWindowDisposition::TransientEscapeSurface
                | PlatformWindowDisposition::AuxiliaryAppSurface => {
                    trace_logs.push(format_discovery_trace_line(
                        window,
                        assessment,
                        None,
                        None,
                        blocked_discovery_action(assessment.disposition),
                    ));
                    let _ = self.pending_discoveries.remove(&window.hwnd);
                    continue;
                }
                PlatformWindowDisposition::PendingPrimaryCandidate
                | PlatformWindowDisposition::PromotablePrimaryCandidate => {}
            }

            let Some(actual_monitor_id) = self.monitor_id_by_binding(&window.monitor_binding)
            else {
                self.push_degraded_reason(format!(
                    "missing-monitor-binding:{}",
                    window.monitor_binding
                ));
                trace_logs.push(format_discovery_trace_line(
                    window,
                    assessment,
                    None,
                    None,
                    "blocked-missing-monitor",
                ));
                continue;
            };

            let (should_promote, stable_ticks, required_ticks, preflight_blocks_promotion) = {
                let now = Instant::now();
                let entry = self
                    .pending_discoveries
                    .entry(window.hwnd)
                    .or_insert_with(|| PendingDiscoveryEntry {
                        first_seen_at: now,
                        last_seen_at: now,
                        stable_ticks: 1,
                        last_rect: window.rect,
                        family_key: assessment.family_key.clone(),
                        disposition: assessment.disposition,
                        disposition_reason: assessment.disposition_reason.clone(),
                        presentation_preflight: assessment.presentation_preflight,
                        presentation_reason: assessment.presentation_reason.clone(),
                    });

                if pending_discovery_rect_is_stable(entry.last_rect, window.rect) {
                    entry.stable_ticks = entry.stable_ticks.saturating_add(1);
                } else {
                    entry.stable_ticks = 1;
                    entry.first_seen_at = now;
                }
                entry.last_seen_at = now;
                entry.last_rect = window.rect;
                entry.family_key = assessment.family_key.clone();
                entry.disposition = assessment.disposition;
                entry.disposition_reason = assessment.disposition_reason.clone();
                entry.presentation_preflight = assessment.presentation_preflight;
                entry.presentation_reason = assessment.presentation_reason.clone();

                let required_ticks = pending_discovery_required_stable_ticks(
                    assessment,
                    window,
                    had_previous_snapshot,
                );
                let preflight_blocks_promotion =
                    assessment.presentation_preflight == PlatformPresentationPreflight::Rejected;

                (
                    !preflight_blocks_promotion && entry.stable_ticks >= required_ticks,
                    entry.stable_ticks,
                    required_ticks,
                    preflight_blocks_promotion,
                )
            };

            if !should_promote {
                trace_logs.push(format_discovery_trace_line(
                    window,
                    assessment,
                    Some(stable_ticks),
                    Some(required_ticks),
                    if preflight_blocks_promotion {
                        "blocked-preflight"
                    } else {
                        "pending"
                    },
                ));
                continue;
            }

            self.promote_pending_window_discovery(
                window,
                focused_hwnd,
                follow_active_context,
                actual_monitor_id,
            )?;
            let _ = self.pending_discoveries.remove(&window.hwnd);
            discovered_windows += 1;
            trace_logs.push(format_discovery_trace_line(
                window,
                assessment,
                Some(stable_ticks),
                Some(required_ticks),
                "promoted",
            ));
        }

        Ok(DiscoveryReconcileResult {
            discovered_windows,
            trace_logs,
        })
    }

    fn promote_pending_window_discovery(
        &mut self,
        window: &PlatformWindowSnapshot,
        focused_hwnd: Option<u64>,
        follow_active_context: bool,
        actual_monitor_id: flowtile_domain::MonitorId,
    ) -> Result<(), RuntimeError> {
        let mut decision = classify_window(
            &self.active_config.rules,
            &WindowRuleInput {
                process_name: window.process_name.clone(),
                class_name: window.class_name.clone(),
                title: window.title.clone(),
            },
            &self.active_config.projection,
        );
        if decision.managed && !self.monitor_is_managed(actual_monitor_id) {
            decision.managed = false;
        }
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
                classification: if decision.managed && decision.layer == WindowLayer::Tiled {
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
        Ok(())
    }

    fn reclassify_state_windows_for_monitor_policy(
        &mut self,
        snapshot: &PlatformSnapshot,
    ) -> Result<usize, RuntimeError> {
        let actual_windows = snapshot
            .windows
            .iter()
            .map(|window| (window.hwnd, window))
            .collect::<std::collections::HashMap<_, _>>();
        let mut window_ids_to_destroy = Vec::new();

        for window in self.store.state().windows.values() {
            let Some(hwnd) = window.current_hwnd_binding else {
                continue;
            };
            let Some(actual_window) = actual_windows.get(&hwnd) else {
                continue;
            };
            let Some(actual_monitor_id) =
                self.monitor_id_by_binding(&actual_window.monitor_binding)
            else {
                continue;
            };
            let decision = classify_window(
                &self.active_config.rules,
                &WindowRuleInput {
                    process_name: actual_window.process_name.clone(),
                    class_name: actual_window.class_name.clone(),
                    title: actual_window.title.clone(),
                },
                &self.active_config.projection,
            );
            let desired_managed = decision.managed && self.monitor_is_managed(actual_monitor_id);

            if window.is_managed != desired_managed {
                window_ids_to_destroy.push(window.id);
            }
        }

        let mut destroyed_windows = 0;
        for window_id in window_ids_to_destroy {
            let correlation_id = self.next_correlation_id();
            self.store
                .dispatch(DomainEvent::window_destroyed(correlation_id, window_id))?;
            destroyed_windows += 1;
        }

        Ok(destroyed_windows)
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
        if !window.is_managed {
            return Ok(());
        }
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
                self.refresh_workspace_set_monitor_projection(workspace_set_id);
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
            self.refresh_workspace_set_monitor_projection(workspace_set_id);
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

    fn refresh_workspace_set_monitor_projection(
        &mut self,
        workspace_set_id: flowtile_domain::WorkspaceSetId,
    ) {
        let Some(monitor_id) = self
            .store
            .state()
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.monitor_id)
        else {
            return;
        };
        let Some(work_area_rect) = self
            .store
            .state()
            .monitors
            .get(&monitor_id)
            .map(|monitor| monitor.work_area_rect)
        else {
            return;
        };

        self.store
            .state_mut()
            .normalize_workspace_set(workspace_set_id);

        let workspace_ids = self
            .store
            .state()
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
            .unwrap_or_default();

        for workspace_id in workspace_ids {
            if let Some(workspace) = self.store.state_mut().workspaces.get_mut(&workspace_id) {
                workspace.monitor_id = monitor_id;
                workspace.strip.visible_region = work_area_rect;
            }
        }
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

    fn monitor_is_managed(&self, monitor_id: MonitorId) -> bool {
        self.store
            .state()
            .monitors
            .get(&monitor_id)
            .is_some_and(|monitor| {
                self.active_config
                    .projection
                    .manages_monitor_binding(monitor.platform_binding.as_deref())
            })
    }

    fn first_managed_monitor_id(&self) -> Option<MonitorId> {
        self.store
            .state()
            .monitor_ids_in_navigation_order()
            .into_iter()
            .find(|monitor_id| self.monitor_is_managed(*monitor_id))
    }

    pub(super) fn find_window_id_by_hwnd(&self, hwnd: u64) -> Option<WindowId> {
        self.store
            .state()
            .windows
            .iter()
            .find_map(|(window_id, window)| {
                (window.current_hwnd_binding == Some(hwnd)).then_some(*window_id)
            })
    }

    pub(super) fn current_focused_hwnd(&self) -> Option<u64> {
        self.store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding)
    }

    fn discovery_target_monitor_id(
        &self,
        actual_monitor_id: MonitorId,
        managed: bool,
    ) -> MonitorId {
        if !managed {
            return actual_monitor_id;
        }

        if let Some(active_context_monitor_id) = self
            .store
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| self.store.state().windows.get(&window_id))
            .and_then(|window| self.store.state().workspaces.get(&window.workspace_id))
            .map(|workspace| workspace.monitor_id)
            .or(self.store.state().focus.focused_monitor_id)
            && self.monitor_is_managed(active_context_monitor_id)
        {
            return active_context_monitor_id;
        }

        if self.monitor_is_managed(actual_monitor_id) {
            return actual_monitor_id;
        }

        self.first_managed_monitor_id().unwrap_or(actual_monitor_id)
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

    pub(super) fn discovered_width_semantics(
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
}

fn format_discovery_trace_line(
    window: &PlatformWindowSnapshot,
    assessment: &PlatformDiscoveryAssessment,
    stable_ticks: Option<u8>,
    required_ticks: Option<u8>,
    action: &str,
) -> String {
    let stable_ticks = stable_ticks
        .map(|ticks| ticks.to_string())
        .unwrap_or_else(|| "none".to_string());
    let required_ticks = required_ticks
        .map(|ticks| ticks.to_string())
        .unwrap_or_else(|| "none".to_string());

    format!(
        "discovery-trace: hwnd={} process={} pid={} class={} title=\"{}\" monitor={} focused={} candidate={} family=\"{}\" role={} role_reason={} disposition={} disposition_reason={} preflight={} preflight_reason={} stable_ticks={} required_ticks={} action={}",
        window.hwnd,
        sanitize_log_text(window.process_name.as_deref().unwrap_or("unknown")),
        window.process_id,
        sanitize_log_text(&window.class_name),
        sanitize_log_text(&window.title),
        sanitize_log_text(&window.monitor_binding),
        window.is_focused,
        window.management_candidate,
        sanitize_log_text(&assessment.family_key),
        platform_window_role_name(assessment.role),
        sanitize_log_text(&assessment.role_reason),
        platform_window_disposition_name(assessment.disposition),
        sanitize_log_text(&assessment.disposition_reason),
        platform_presentation_preflight_name(assessment.presentation_preflight),
        sanitize_log_text(&assessment.presentation_reason),
        stable_ticks,
        required_ticks,
        action,
    )
}

fn blocked_discovery_action(disposition: PlatformWindowDisposition) -> &'static str {
    match disposition {
        PlatformWindowDisposition::RejectedNoise => "blocked-noise",
        PlatformWindowDisposition::TransientEscapeSurface => "blocked-transient",
        PlatformWindowDisposition::AuxiliaryAppSurface => "blocked-auxiliary",
        PlatformWindowDisposition::PendingPrimaryCandidate => "pending",
        PlatformWindowDisposition::PromotablePrimaryCandidate => "promoted",
    }
}

fn platform_window_role_name(role: PlatformWindowRole) -> &'static str {
    match role {
        PlatformWindowRole::Noise => "noise",
        PlatformWindowRole::Transient => "transient",
        PlatformWindowRole::Auxiliary => "auxiliary",
        PlatformWindowRole::Primary => "primary",
    }
}

fn platform_window_disposition_name(disposition: PlatformWindowDisposition) -> &'static str {
    match disposition {
        PlatformWindowDisposition::RejectedNoise => "rejected-noise",
        PlatformWindowDisposition::TransientEscapeSurface => "transient-escape-surface",
        PlatformWindowDisposition::AuxiliaryAppSurface => "auxiliary-app-surface",
        PlatformWindowDisposition::PendingPrimaryCandidate => "pending-primary-candidate",
        PlatformWindowDisposition::PromotablePrimaryCandidate => "promotable-primary-candidate",
    }
}

fn platform_presentation_preflight_name(preflight: PlatformPresentationPreflight) -> &'static str {
    match preflight {
        PlatformPresentationPreflight::Unknown => "unknown",
        PlatformPresentationPreflight::Eligible => "eligible",
        PlatformPresentationPreflight::FallbackOnly => "fallback-only",
        PlatformPresentationPreflight::Rejected => "rejected",
    }
}
