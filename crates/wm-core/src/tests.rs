use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flowtile_domain::{
    BindControlMode, ColumnMode, CorrelationId, DomainEvent, FocusBehavior, NavigationScope, Rect,
    RuntimeMode, Size, WidthSemantics, WindowPlacement,
};
use flowtile_windows_adapter::{
    ObservationEnvelope, ObservationKind, PlatformMonitorSnapshot, PlatformSnapshot,
    PlatformWindowSnapshot, WindowPresentationMode,
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
        232
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
fn minimized_window_drops_out_of_layout_and_focus_retargets() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    let initial_snapshot = snapshot_with_windows(
        Rect::new(0, 0, 600, 900),
        vec![
            (100, Rect::new(0, 0, 300, 900), false, true),
            (101, Rect::new(300, 0, 300, 900), true, true),
        ],
    );

    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let report = runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-hide".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 600, 900),
                    vec![(100, Rect::new(0, 0, 300, 900), true, true)],
                )),
                message: None,
            },
            true,
        )
        .expect("observation should succeed")
        .expect("snapshot observation should produce a cycle report");

    assert_eq!(report.destroyed_windows, 1);
    assert_eq!(runtime.state().windows.len(), 1);
    assert_eq!(
        runtime
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| runtime.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding),
        Some(100)
    );
}

#[test]
fn newly_focused_window_enters_navigation_and_remains_on_monitor() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(
            snapshot_with_windows(
                Rect::new(0, 0, 600, 900),
                vec![(100, Rect::new(0, 0, 400, 900), true, true)],
            ),
            true,
        )
        .expect("initial sync should succeed");

    let report = runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-create".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 600, 900),
                    vec![
                        (100, Rect::new(0, 0, 400, 900), false, true),
                        (101, Rect::new(400, 0, 400, 900), true, true),
                    ],
                )),
                message: None,
            },
            true,
        )
        .expect("observation should succeed")
        .expect("snapshot observation should produce a cycle report");

    let focused_window_id = runtime
        .state()
        .focus
        .focused_window_id
        .expect("new window should be focused");
    let workspace_id = runtime
        .state()
        .windows
        .get(&focused_window_id)
        .map(|window| window.workspace_id)
        .expect("focused window should exist");
    let projection = flowtile_layout_engine::recompute_workspace(runtime.state(), workspace_id)
        .expect("layout projection should exist");
    let focused_geometry = projection
        .window_geometries
        .iter()
        .find(|geometry| geometry.window_id == focused_window_id)
        .expect("focused geometry should exist");

    assert_eq!(report.discovered_windows, 1);
    assert_eq!(
        runtime
            .state()
            .windows
            .get(&focused_window_id)
            .and_then(|window| window.current_hwnd_binding),
        Some(101)
    );
    assert!(focused_geometry.rect.x < 600);
    assert!(focused_geometry.rect.x + focused_geometry.rect.width as i32 > 0);
}

#[test]
fn platform_bounce_back_does_not_immediately_override_user_focus_command() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(
            snapshot_with_windows(
                Rect::new(0, 0, 800, 900),
                vec![
                    (100, Rect::new(0, 0, 400, 900), true, true),
                    (101, Rect::new(400, 0, 400, 900), false, true),
                ],
            ),
            true,
        )
        .expect("initial sync should succeed");

    runtime
        .store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(2),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus next should succeed");
    runtime.pending_focus_claim = Some(super::PendingFocusClaim {
        desired_hwnd: 101,
        expires_at: Instant::now() + Duration::from_millis(250),
    });

    assert_eq!(
        runtime
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| runtime.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding),
        Some(101)
    );

    runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-foreground".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 800, 900),
                    vec![
                        (100, Rect::new(0, 0, 400, 900), false, true),
                        (101, Rect::new(400, 0, 400, 900), true, true),
                    ],
                )),
                message: None,
            },
            true,
        )
        .expect("foreground confirmation should succeed")
        .expect("foreground confirmation should produce a cycle report");

    runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-foreground".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 800, 900),
                    vec![
                        (100, Rect::new(0, 0, 400, 900), true, true),
                        (101, Rect::new(400, 0, 400, 900), false, true),
                    ],
                )),
                message: None,
            },
            true,
        )
        .expect("bounce-back observation should succeed")
        .expect("bounce-back observation should produce a cycle report");

    assert_eq!(
        runtime
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| runtime.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding),
        Some(101)
    );
}

#[test]
fn restored_window_reenters_layout_without_manual_rescan() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(
            snapshot_with_windows(
                Rect::new(0, 0, 700, 900),
                vec![
                    (100, Rect::new(0, 0, 350, 900), true, true),
                    (101, Rect::new(350, 0, 350, 900), false, true),
                ],
            ),
            true,
        )
        .expect("initial sync should succeed");

    runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-hide".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 700, 900),
                    vec![(100, Rect::new(0, 0, 350, 900), true, true)],
                )),
                message: None,
            },
            true,
        )
        .expect("minimize observation should succeed")
        .expect("minimize observation should produce a cycle report");

    let report = runtime
        .apply_observation(
            ObservationEnvelope {
                kind: ObservationKind::Snapshot,
                reason: "win-event-show".to_string(),
                snapshot: Some(snapshot_with_windows(
                    Rect::new(0, 0, 700, 900),
                    vec![
                        (100, Rect::new(0, 0, 350, 900), false, true),
                        (101, Rect::new(350, 0, 350, 900), true, true),
                    ],
                )),
                message: None,
            },
            true,
        )
        .expect("restore observation should succeed")
        .expect("restore observation should produce a cycle report");

    assert_eq!(report.discovered_windows, 1);
    assert_eq!(runtime.state().windows.len(), 2);
    assert_eq!(
        runtime
            .state()
            .focus
            .focused_window_id
            .and_then(|window_id| runtime.state().windows.get(&window_id))
            .and_then(|window| window.current_hwnd_binding),
        Some(101)
    );
}

#[test]
fn authoritative_sync_prunes_state_windows_missing_from_current_snapshot() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    let initial_snapshot = snapshot_with_windows(
        Rect::new(0, 0, 900, 900),
        vec![
            (100, Rect::new(0, 0, 300, 900), true, true),
            (101, Rect::new(300, 0, 300, 900), false, true),
        ],
    );

    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    runtime.last_snapshot = Some(snapshot_with_windows(
        Rect::new(0, 0, 900, 900),
        vec![(100, Rect::new(0, 0, 300, 900), true, true)],
    ));

    let report = runtime
        .sync_snapshot(
            snapshot_with_windows(
                Rect::new(0, 0, 900, 900),
                vec![(100, Rect::new(0, 0, 300, 900), true, true)],
            ),
            true,
        )
        .expect("sync should prune stale state window");

    assert_eq!(report.destroyed_windows, 1);
    assert_eq!(runtime.state().windows.len(), 1);
    assert!(
        !runtime
            .state()
            .windows
            .values()
            .any(|window| window.current_hwnd_binding == Some(101))
    );
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
    runtime
        .sync_snapshot(sample_snapshot(100, Rect::new(0, 0, 420, 900), true), true)
        .expect("initial managed snapshot should succeed");

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
    assert!(
        !runtime
            .state()
            .runtime
            .degraded_flags
            .iter()
            .any(|flag| flag.starts_with("presentation-cleanup-failed:"))
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
        356
    );
}

#[test]
fn focus_prev_at_strip_start_does_not_wrap_to_last_column() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 800, 700), 96, true);

    for (correlation, hwnd, focus_behavior) in [
        (1_u64, 100_u64, FocusBehavior::FollowNewWindow),
        (2, 101, FocusBehavior::PreserveCurrentFocus),
        (3, 102, FocusBehavior::PreserveCurrentFocus),
    ] {
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
                focus_behavior,
            ))
            .expect("window discovery should succeed");
    }

    let first_window_id = store
        .state()
        .focus
        .focused_window_id
        .expect("first window should be focused");
    let result = store
        .dispatch(DomainEvent::focus_prev(
            CorrelationId::new(4),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus prev should succeed");

    assert_eq!(store.state().focus.focused_window_id, Some(first_window_id));
    assert_eq!(
        result
            .layout_projection
            .as_ref()
            .expect("layout should exist")
            .scroll_offset,
        0
    );
}

#[test]
fn focus_next_at_strip_end_does_not_wrap_to_first_column() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 800, 700), 96, true);

    for (correlation, hwnd, focus_behavior) in [
        (1_u64, 100_u64, FocusBehavior::FollowNewWindow),
        (2, 101, FocusBehavior::PreserveCurrentFocus),
        (3, 102, FocusBehavior::PreserveCurrentFocus),
    ] {
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
                focus_behavior,
            ))
            .expect("window discovery should succeed");
    }

    store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(4),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("first focus next should succeed");
    let edge_result = store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(5),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("second focus next should succeed");
    let focused_window_id = store
        .state()
        .focus
        .focused_window_id
        .expect("last window should be focused");
    let result = store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(6),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus next at strip end should succeed");

    assert_eq!(
        edge_result
            .layout_projection
            .as_ref()
            .expect("layout should exist")
            .scroll_offset,
        456
    );
    assert_eq!(
        store.state().focus.focused_window_id,
        Some(focused_window_id)
    );
    assert_eq!(
        result
            .layout_projection
            .as_ref()
            .expect("layout should exist")
            .scroll_offset,
        456
    );
}

#[test]
fn focus_next_to_last_underfilled_column_pins_strip_to_right_edge() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1200, 700), 96, true);

    store
        .dispatch(DomainEvent::window_discovered_with(
            CorrelationId::new(1),
            monitor_id,
            100,
            Size::new(220, 700),
            Rect::new(0, 0, 220, 700),
            WindowPlacement::AppendToWorkspaceEnd {
                mode: ColumnMode::Normal,
                width: WidthSemantics::Fixed(220),
            },
            FocusBehavior::FollowNewWindow,
        ))
        .expect("first window discovery should succeed");
    store
        .dispatch(DomainEvent::window_discovered_with(
            CorrelationId::new(2),
            monitor_id,
            101,
            Size::new(420, 700),
            Rect::new(220, 0, 420, 700),
            WindowPlacement::AppendToWorkspaceEnd {
                mode: ColumnMode::Normal,
                width: WidthSemantics::Fixed(420),
            },
            FocusBehavior::PreserveCurrentFocus,
        ))
        .expect("second window discovery should succeed");

    let result = store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(3),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus next should succeed");
    let projection = result
        .layout_projection
        .as_ref()
        .expect("layout should exist");

    assert_eq!(projection.scroll_offset, 0);
    assert_eq!(
        geometry_x_width(projection, flowtile_domain::WindowId::new(1)).0,
        532
    );
    let second_geometry = geometry_x_width(projection, flowtile_domain::WindowId::new(2));
    assert_eq!(second_geometry.0, 764);
    assert_eq!(
        second_geometry.0 + second_geometry.1 as i32,
        projection.viewport.x + projection.viewport.width as i32
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
        244
    );
}

#[test]
fn overflow_focus_navigation_uses_edge_reveal_by_default() {
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
    assert_eq!(projection.scroll_offset, 244);
}

#[test]
fn new_window_after_fullscreen_inserts_to_the_right_and_becomes_active() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 800, 700), 96, true);

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
        .expect("first window discovery should succeed");
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
        .expect("second window discovery should succeed");
    store
        .dispatch(DomainEvent::focus_prev(
            CorrelationId::new(3),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus prev should succeed");
    store
        .dispatch(DomainEvent::toggle_fullscreen(CorrelationId::new(4), None))
        .expect("toggle fullscreen should succeed");

    let result = store
        .dispatch(DomainEvent::window_discovered_with(
            CorrelationId::new(5),
            monitor_id,
            102,
            Size::new(400, 700),
            Rect::new(0, 0, 400, 700),
            WindowPlacement::NewColumnAfterFocus {
                mode: ColumnMode::Normal,
                width: WidthSemantics::Fixed(400),
            },
            FocusBehavior::PreserveCurrentFocus,
        ))
        .expect("new window discovery should succeed");

    let workspace_id = store
        .state()
        .active_workspace_id_for_monitor(monitor_id)
        .expect("workspace should exist");
    let workspace = store
        .state()
        .workspaces
        .get(&workspace_id)
        .expect("workspace should exist");

    assert_eq!(workspace.strip.ordered_column_ids.len(), 2);
    assert_eq!(
        store
            .state()
            .focus
            .focused_window_id
            .map(|window_id| window_id.get()),
        Some(3)
    );
    assert_eq!(
        result
            .layout_projection
            .as_ref()
            .expect("layout should exist")
            .scroll_offset,
        0
    );

    let first_column = store
        .state()
        .layout
        .columns
        .get(&workspace.strip.ordered_column_ids[0])
        .expect("first column should exist");
    let second_column = store
        .state()
        .layout
        .columns
        .get(&workspace.strip.ordered_column_ids[1])
        .expect("second column should exist");

    assert_eq!(
        first_column
            .ordered_window_ids
            .first()
            .map(|window_id| window_id.get()),
        Some(3)
    );
    assert_eq!(
        second_column
            .ordered_window_ids
            .first()
            .map(|window_id| window_id.get()),
        Some(2)
    );
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
        244
    );
}

#[test]
fn scroll_command_changes_projected_geometry_and_apply_plan() {
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
                rect: Rect::new(0, 0, 300, 900),
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
                rect: Rect::new(300, 0, 300, 900),
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
                rect: Rect::new(600, 0, 300, 900),
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
    assert_eq!(geometry_x_width(projection, first_window_id).0, -224);
    assert_eq!(geometry_x_width(projection, second_window_id).0, 88);
    assert_eq!(geometry_x_width(projection, third_window_id).0, 400);
    assert_eq!(planned_operations.len(), 3);
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .map(|operation| operation.rect.x),
        Some(-224)
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
            .find(|operation| operation.hwnd == 100)
            .map(|operation| operation.presentation.mode),
        Some(WindowPresentationMode::NativeVisible)
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 100)
            .and_then(|operation| operation.presentation.monitor_scene.home_visible_rect),
        Some(Rect::new(0, 16, 76, 868))
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .map(|operation| operation.rect.x),
        Some(601)
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
            .find(|operation| operation.hwnd == 101)
            .map(|operation| operation.presentation.mode),
        Some(WindowPresentationMode::SurrogateVisible)
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 101)
            .and_then(|operation| operation.presentation.surrogate.as_ref())
            .map(|surrogate| surrogate.destination_rect.x),
        Some(88)
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 102)
            .map(|operation| operation.presentation.mode),
        Some(WindowPresentationMode::SurrogateClipped)
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 102)
            .map(|operation| operation.rect.x),
        Some(601)
    );
    assert_eq!(
        planned_operations
            .iter()
            .find(|operation| operation.hwnd == 102)
            .and_then(|operation| operation.presentation.surrogate.as_ref())
            .map(|surrogate| surrogate.destination_rect.x),
        Some(400)
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
fn external_transient_foreground_does_not_trigger_activation_reassert() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(sample_snapshot(100, Rect::new(0, 0, 420, 900), true), true)
        .expect("initial sync should succeed");

    let overlay_snapshot = snapshot_with_windows(
        Rect::new(0, 0, 1600, 900),
        vec![
            (100, Rect::new(0, 0, 420, 900), false, true),
            (900, Rect::new(200, 100, 700, 500), true, false),
        ],
    );

    let planned_operations = runtime
        .plan_apply_operations(&overlay_snapshot)
        .expect("apply plan should be computed");

    assert!(
        !planned_operations
            .iter()
            .any(|operation| operation.hwnd == 100 && operation.activate)
    );
}

#[test]
fn filtered_transient_foreground_fact_does_not_trigger_activation_reassert() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(sample_snapshot(100, Rect::new(0, 0, 420, 900), true), true)
        .expect("initial sync should succeed");

    let overlay_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(900),
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
            is_focused: false,
            management_candidate: true,
        }],
    };

    let planned_operations = runtime
        .plan_apply_operations(&overlay_snapshot)
        .expect("apply plan should be computed");

    assert!(
        !planned_operations
            .iter()
            .any(|operation| operation.hwnd == 100 && operation.activate)
    );
}

#[test]
fn transient_snapshot_window_is_not_ingested_into_wm_state() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    let snapshot = snapshot_with_windows(
        Rect::new(0, 0, 1600, 900),
        vec![
            (100, Rect::new(0, 0, 420, 900), true, true),
            (900, Rect::new(200, 100, 700, 500), false, false),
        ],
    );

    runtime
        .sync_snapshot(snapshot, true)
        .expect("sync should succeed");

    assert!(
        runtime
            .state()
            .windows
            .values()
            .any(|window| window.current_hwnd_binding == Some(100))
    );
    assert!(
        !runtime
            .state()
            .windows
            .values()
            .any(|window| window.current_hwnd_binding == Some(900))
    );
}

#[test]
fn competing_managed_foreground_still_triggers_activation_reassert() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(
            snapshot_with_windows(
                Rect::new(0, 0, 1600, 900),
                vec![
                    (100, Rect::new(0, 0, 420, 900), true, true),
                    (101, Rect::new(420, 0, 420, 900), false, true),
                ],
            ),
            true,
        )
        .expect("initial sync should succeed");

    let planned_operations = runtime
        .plan_apply_operations(&snapshot_with_windows(
            Rect::new(0, 0, 1600, 900),
            vec![
                (100, Rect::new(0, 0, 420, 900), false, true),
                (101, Rect::new(420, 0, 420, 900), true, true),
            ],
        ))
        .expect("apply plan should be computed");

    assert!(
        planned_operations
            .iter()
            .any(|operation| operation.hwnd == 100 && operation.activate)
    );
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
fn workspace_navigation_restores_focus_after_column_move() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let window_a = discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 420, 900));
    let window_b =
        discover_tiled_window(&mut store, 2, monitor_id, 101, Rect::new(420, 0, 360, 900));

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(3),
            None,
        ))
        .expect("column move to workspace should succeed");

    let workspace_ids = ordered_workspace_ids_for_monitor(store.state(), monitor_id);
    let workspace_above = workspace_ids[0];
    let workspace_below = workspace_ids[1];
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_id),
        Some(workspace_below)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_b));

    store
        .dispatch(DomainEvent::focus_workspace_up(CorrelationId::new(4), None))
        .expect("focus workspace up should succeed");
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_id),
        Some(workspace_above)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_a));

    store
        .dispatch(DomainEvent::focus_workspace_down(
            CorrelationId::new(5),
            None,
        ))
        .expect("focus workspace down should succeed");
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_id),
        Some(workspace_below)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_b));
}

#[test]
fn targeted_column_move_can_transfer_column_into_explicit_workspace() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let window_a = discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 420, 900));
    let window_b =
        discover_tiled_window(&mut store, 2, monitor_id, 101, Rect::new(420, 0, 360, 900));
    let source_column_id = store
        .state()
        .windows
        .get(&window_a)
        .and_then(|window| window.column_id)
        .expect("window A should belong to a tiled column");
    let target_workspace_id = ordered_workspace_ids_for_monitor(store.state(), monitor_id)[1];

    store
        .dispatch(DomainEvent::move_column_to_workspace_target(
            CorrelationId::new(3),
            source_column_id,
            target_workspace_id,
            None,
        ))
        .expect("targeted column move should succeed");

    let ordered_workspace_ids = ordered_workspace_ids_for_monitor(store.state(), monitor_id);
    assert_eq!(ordered_workspace_ids.len(), 3);
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_id),
        Some(target_workspace_id)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_a));
    assert_eq!(
        store
            .state()
            .windows
            .get(&window_a)
            .expect("window A should exist")
            .workspace_id,
        target_workspace_id
    );
    assert_eq!(
        store
            .state()
            .windows
            .get(&window_b)
            .expect("window B should exist")
            .workspace_id,
        ordered_workspace_ids[0]
    );
}

#[test]
fn targeted_column_move_reorders_column_within_same_workspace() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let window_a = discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 320, 900));
    let window_b =
        discover_tiled_window(&mut store, 2, monitor_id, 101, Rect::new(320, 0, 320, 900));
    let window_c =
        discover_tiled_window(&mut store, 3, monitor_id, 102, Rect::new(640, 0, 320, 900));
    let workspace_id = store
        .state()
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");
    let column_a = store
        .state()
        .windows
        .get(&window_a)
        .and_then(|window| window.column_id)
        .expect("window A should belong to a tiled column");
    let column_b = store
        .state()
        .windows
        .get(&window_b)
        .and_then(|window| window.column_id)
        .expect("window B should belong to a tiled column");
    let column_c = store
        .state()
        .windows
        .get(&window_c)
        .and_then(|window| window.column_id)
        .expect("window C should belong to a tiled column");

    store
        .dispatch(DomainEvent::move_column_to_workspace_target(
            CorrelationId::new(4),
            column_a,
            workspace_id,
            Some(column_b),
        ))
        .expect("targeted column reorder should succeed");

    let ordered_columns = store
        .state()
        .workspaces
        .get(&workspace_id)
        .expect("workspace should exist")
        .strip
        .ordered_column_ids
        .clone();
    assert_eq!(ordered_columns, vec![column_b, column_a, column_c]);
    assert_eq!(store.state().focus.focused_window_id, Some(window_a));
    assert_eq!(store.state().focus.focused_column_id, Some(column_a));
}

#[test]
fn empty_workspace_in_middle_collapses_after_column_move() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let window_a = discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 420, 900));
    let _window_b =
        discover_tiled_window(&mut store, 2, monitor_id, 101, Rect::new(420, 0, 360, 900));

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(3),
            None,
        ))
        .expect("first column move should succeed");
    let workspace_above = ordered_workspace_ids_for_monitor(store.state(), monitor_id)[0];

    store
        .dispatch(DomainEvent::focus_workspace_up(CorrelationId::new(4), None))
        .expect("focus workspace up should succeed");
    assert_eq!(store.state().focus.focused_window_id, Some(window_a));

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(5),
            None,
        ))
        .expect("second column move should succeed");

    let workspace_ids = ordered_workspace_ids_for_monitor(store.state(), monitor_id);
    assert_eq!(workspace_ids.len(), 2);
    assert!(!workspace_ids.contains(&workspace_above));
}

#[test]
fn move_workspace_up_reorders_without_recreating_workspace() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let _window_a =
        discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 420, 900));
    let window_b =
        discover_tiled_window(&mut store, 2, monitor_id, 101, Rect::new(420, 0, 360, 900));

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(3),
            None,
        ))
        .expect("column move to workspace should succeed");

    let before = ordered_workspace_ids_for_monitor(store.state(), monitor_id);
    let workspace_to_reorder = before[1];

    store
        .dispatch(DomainEvent::move_workspace_up(CorrelationId::new(4), None))
        .expect("move workspace up should succeed");

    let after = ordered_workspace_ids_for_monitor(store.state(), monitor_id);
    assert_eq!(after[0], workspace_to_reorder);
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_id),
        Some(workspace_to_reorder)
    );
    assert_eq!(
        store
            .state()
            .windows
            .get(&window_b)
            .expect("window should exist")
            .workspace_id,
        workspace_to_reorder
    );
}

#[test]
fn move_workspace_to_next_monitor_preserves_identity_and_focus() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_left = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let monitor_right = store
        .state_mut()
        .add_monitor(Rect::new(1600, 0, 1600, 900), 96, false);

    let _window_a =
        discover_tiled_window(&mut store, 1, monitor_left, 100, Rect::new(0, 0, 420, 900));
    let window_b = discover_tiled_window(
        &mut store,
        2,
        monitor_left,
        101,
        Rect::new(420, 0, 360, 900),
    );

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(3),
            None,
        ))
        .expect("column move to workspace should succeed");

    let moved_workspace_id = store
        .state()
        .active_workspace_id_for_monitor(monitor_left)
        .expect("active workspace should exist on left monitor");

    store
        .dispatch(DomainEvent::move_workspace_to_monitor_next(
            CorrelationId::new(4),
            None,
        ))
        .expect("move workspace to next monitor should succeed");

    assert_eq!(store.state().focus.focused_monitor_id, Some(monitor_right));
    assert_eq!(
        store.state().active_workspace_id_for_monitor(monitor_right),
        Some(moved_workspace_id)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_b));
    assert_eq!(
        store
            .state()
            .workspaces
            .get(&moved_workspace_id)
            .expect("workspace should exist")
            .monitor_id,
        monitor_right
    );
    assert_eq!(
        store
            .state()
            .windows
            .get(&window_b)
            .expect("window should exist")
            .workspace_id,
        moved_workspace_id
    );
    assert!(
        ordered_workspace_ids_for_monitor(store.state(), monitor_right)
            .contains(&moved_workspace_id)
    );
}

#[test]
fn overview_defaults_to_first_managed_monitor_when_primary_is_ordinary() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let ordinary_primary = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let managed_secondary = store
        .state_mut()
        .add_monitor(Rect::new(1600, 0, 1600, 900), 96, false);
    set_test_monitor_binding(&mut store, ordinary_primary, "\\\\.\\DISPLAY1");
    set_test_monitor_binding(&mut store, managed_secondary, "\\\\.\\DISPLAY2");
    set_test_managed_monitor_bindings(&mut store, &["\\\\.\\DISPLAY2"]);

    store
        .dispatch(DomainEvent::open_overview(CorrelationId::new(1), None))
        .expect("open overview should succeed");

    assert_eq!(store.state().overview.monitor_id, Some(managed_secondary));
    assert_eq!(
        store.state().overview.selection,
        store
            .state()
            .active_workspace_id_for_monitor(managed_secondary)
    );
}

#[test]
fn focus_navigation_defaults_to_managed_monitor_when_primary_is_ordinary() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let ordinary_primary = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let managed_secondary = store
        .state_mut()
        .add_monitor(Rect::new(1600, 0, 1600, 900), 96, false);
    set_test_monitor_binding(&mut store, ordinary_primary, "\\\\.\\DISPLAY1");
    set_test_monitor_binding(&mut store, managed_secondary, "\\\\.\\DISPLAY2");
    set_test_managed_monitor_bindings(&mut store, &["\\\\.\\DISPLAY2"]);

    let first_window = discover_tiled_window(
        &mut store,
        1,
        managed_secondary,
        100,
        Rect::new(1600, 0, 420, 900),
    );
    let _second_window = discover_tiled_window(
        &mut store,
        2,
        managed_secondary,
        101,
        Rect::new(2020, 0, 360, 900),
    );

    store.state_mut().focus.focused_monitor_id = Some(ordinary_primary);
    store.state_mut().focus.focused_window_id = None;
    store.state_mut().focus.focused_column_id = None;

    store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(3),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus navigation should succeed");

    assert_eq!(
        store.state().focus.focused_monitor_id,
        Some(managed_secondary)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(first_window));
}

#[test]
fn move_workspace_to_next_monitor_skips_ordinary_monitors_when_managed_set_is_active() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let managed_left = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let ordinary_middle = store
        .state_mut()
        .add_monitor(Rect::new(1600, 0, 1600, 900), 96, false);
    let managed_right = store
        .state_mut()
        .add_monitor(Rect::new(3200, 0, 1600, 900), 96, false);
    set_test_monitor_binding(&mut store, managed_left, "\\\\.\\DISPLAY1");
    set_test_monitor_binding(&mut store, ordinary_middle, "\\\\.\\DISPLAY2");
    set_test_monitor_binding(&mut store, managed_right, "\\\\.\\DISPLAY3");
    set_test_managed_monitor_bindings(&mut store, &["\\\\.\\DISPLAY1", "\\\\.\\DISPLAY3"]);

    let _window_a =
        discover_tiled_window(&mut store, 1, managed_left, 100, Rect::new(0, 0, 420, 900));
    let window_b = discover_tiled_window(
        &mut store,
        2,
        managed_left,
        101,
        Rect::new(420, 0, 360, 900),
    );

    store
        .dispatch(DomainEvent::move_column_to_workspace_down(
            CorrelationId::new(3),
            None,
        ))
        .expect("column move to workspace should succeed");

    let moved_workspace_id = store
        .state()
        .active_workspace_id_for_monitor(managed_left)
        .expect("active workspace should exist on left monitor");

    store
        .dispatch(DomainEvent::move_workspace_to_monitor_next(
            CorrelationId::new(4),
            None,
        ))
        .expect("move workspace to next managed monitor should succeed");

    assert_eq!(store.state().focus.focused_monitor_id, Some(managed_right));
    assert_eq!(
        store.state().active_workspace_id_for_monitor(managed_right),
        Some(moved_workspace_id)
    );
    assert_eq!(store.state().focus.focused_window_id, Some(window_b));
    assert_eq!(
        store
            .state()
            .workspaces
            .get(&moved_workspace_id)
            .expect("workspace should exist")
            .monitor_id,
        managed_right
    );
    assert!(
        !ordered_workspace_ids_for_monitor(store.state(), ordinary_middle)
            .contains(&moved_workspace_id)
    );
}

#[test]
fn directional_overview_commands_are_not_reduced_to_blind_toggle() {
    let mut store = StateStore::new(RuntimeMode::WmOnly);
    let monitor_id = store
        .state_mut()
        .add_monitor(Rect::new(0, 0, 1600, 900), 96, true);

    let _window = discover_tiled_window(&mut store, 1, monitor_id, 100, Rect::new(0, 0, 420, 900));
    let workspace_id = store
        .state()
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    store
        .dispatch(DomainEvent::open_overview(CorrelationId::new(2), None))
        .expect("open overview should succeed");
    assert!(store.state().overview.is_open);
    assert_eq!(store.state().overview.monitor_id, Some(monitor_id));
    assert_eq!(store.state().overview.selection, Some(workspace_id));
    assert_eq!(store.state().overview.projection_version, 1);

    store
        .dispatch(DomainEvent::open_overview(CorrelationId::new(3), None))
        .expect("repeated open overview should succeed");
    assert!(store.state().overview.is_open);
    assert_eq!(store.state().overview.monitor_id, Some(monitor_id));
    assert_eq!(store.state().overview.selection, Some(workspace_id));
    assert_eq!(store.state().overview.projection_version, 1);

    store
        .dispatch(DomainEvent::close_overview(CorrelationId::new(4), None))
        .expect("close overview should succeed");
    assert!(!store.state().overview.is_open);
    assert_eq!(store.state().overview.monitor_id, None);
    assert_eq!(store.state().overview.selection, None);
    assert_eq!(store.state().overview.projection_version, 2);

    store
        .dispatch(DomainEvent::close_overview(CorrelationId::new(5), None))
        .expect("repeated close overview should succeed");
    assert!(!store.state().overview.is_open);
    assert_eq!(store.state().overview.monitor_id, None);
    assert_eq!(store.state().overview.selection, None);
    assert_eq!(store.state().overview.projection_version, 2);
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

fn discover_tiled_window(
    store: &mut StateStore,
    correlation: u64,
    monitor_id: flowtile_domain::MonitorId,
    hwnd: u64,
    rect: Rect,
) -> flowtile_domain::WindowId {
    store
        .dispatch(DomainEvent::window_discovered_with(
            CorrelationId::new(correlation),
            monitor_id,
            hwnd,
            Size::new(rect.width, rect.height),
            rect,
            WindowPlacement::AppendToWorkspaceEnd {
                mode: ColumnMode::Normal,
                width: WidthSemantics::Fixed(rect.width),
            },
            FocusBehavior::FollowNewWindow,
        ))
        .expect("window discovery should succeed");
    store
        .state()
        .focus
        .focused_window_id
        .expect("discovered window should become focused")
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

fn sample_snapshot(hwnd: u64, rect: Rect, focused: bool) -> PlatformSnapshot {
    snapshot_with_windows(
        Rect::new(0, 0, 1600, 900),
        vec![(hwnd, rect, focused, true)],
    )
}

fn set_test_monitor_binding(
    store: &mut StateStore,
    monitor_id: flowtile_domain::MonitorId,
    binding: &str,
) {
    store
        .state_mut()
        .monitors
        .get_mut(&monitor_id)
        .expect("monitor should exist")
        .platform_binding = Some(binding.to_string());
}

fn set_test_managed_monitor_bindings(store: &mut StateStore, bindings: &[&str]) {
    store.state_mut().config_projection.managed_monitor_bindings = bindings
        .iter()
        .map(|binding| (*binding).to_string())
        .collect();
}

fn snapshot_with_windows(
    monitor_rect: Rect,
    windows: Vec<(u64, Rect, bool, bool)>,
) -> PlatformSnapshot {
    let foreground_hwnd = windows
        .iter()
        .find_map(|(hwnd, _, focused, _)| (*focused).then_some(*hwnd));
    PlatformSnapshot {
        foreground_hwnd,
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: monitor_rect,
            dpi: 96,
            is_primary: true,
        }],
        windows: windows
            .into_iter()
            .map(
                |(hwnd, rect, focused, management_candidate)| PlatformWindowSnapshot {
                    hwnd,
                    title: format!("Window {hwnd}"),
                    class_name: "Notepad".to_string(),
                    process_id: 4242,
                    process_name: Some("notepad".to_string()),
                    rect,
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: focused,
                    management_candidate,
                },
            )
            .collect(),
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
