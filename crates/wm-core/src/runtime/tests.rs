use flowtile_config_rules::{WindowRule, WindowRuleActions, WindowRuleDecision, WindowRuleMatch};
use flowtile_domain::{
    CorrelationId, DomainEvent, NavigationScope, Rect, ResizeEdge, RuntimeMode, WidthSemantics,
    WindowClassification, WindowLayer,
};
use flowtile_layout_engine::recompute_workspace;
use flowtile_windows_adapter::{
    ApplyOperation, PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot,
    WindowOpacityMode, WindowPresentation, WindowPresentationMode,
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
        presentation: WindowPresentation::default(),
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
        presentation: WindowPresentation::default(),
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
        presentation: WindowPresentation::default(),
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
            presentation: WindowPresentation::default(),
        },
        ApplyOperation {
            hwnd: 200,
            rect: Rect::new(420, 0, 420, 900),
            apply_geometry: true,
            activate: false,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: None,
            presentation: WindowPresentation::default(),
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
    let monitor_id = runtime
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
    let monitor_id = runtime
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
    let monitor_id = runtime
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
fn missing_monitor_fallback_refreshes_workspace_set_projection_to_surviving_work_area() {
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

    let missing_monitor_id = runtime
        .state()
        .monitors
        .iter()
        .find_map(|(monitor_id, monitor)| {
            (monitor.platform_binding.as_deref() == Some("\\\\.\\DISPLAY2")).then_some(*monitor_id)
        })
        .expect("second monitor should exist after initial sync");

    let fallback_snapshot = PlatformSnapshot {
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
        .sync_snapshot(fallback_snapshot.clone(), true)
        .expect("fallback sync should succeed");

    let missing_monitor = runtime
        .state()
        .monitors
        .get(&missing_monitor_id)
        .expect("missing logical monitor should still exist");
    assert_eq!(
        missing_monitor.platform_binding.as_deref(),
        Some("\\\\.\\DISPLAY2")
    );
    assert_eq!(
        missing_monitor.work_area_rect,
        fallback_snapshot.monitors[0].work_area_rect
    );

    let workspace_set_id = missing_monitor.workspace_set_id;
    let workspace_ids = runtime
        .state()
        .workspace_sets
        .get(&workspace_set_id)
        .expect("workspace set should exist")
        .ordered_workspace_ids
        .clone();
    assert!(!workspace_ids.is_empty());
    for workspace_id in &workspace_ids {
        let workspace = runtime
            .state()
            .workspaces
            .get(workspace_id)
            .expect("workspace should exist");
        assert_eq!(workspace.monitor_id, missing_monitor_id);
        assert_eq!(
            workspace.strip.visible_region,
            fallback_snapshot.monitors[0].work_area_rect
        );
    }

    let planned_operations = runtime
        .plan_apply_operations(&fallback_snapshot)
        .expect("apply plan should be computed");
    let operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 100)
        .expect("fallback monitor window should still receive a target rect");
    assert!(operation.rect.x < fallback_snapshot.monitors[0].work_area_rect.width as i32);
    assert!(operation.rect.y >= fallback_snapshot.monitors[0].work_area_rect.y);
}

#[test]
fn wallpaper_selector_window_stays_out_of_layout_and_focus() {
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime.active_config.rules.push(WindowRule {
        id: "companion-wallpaper-selector".to_string(),
        priority: 120,
        enabled: true,
        matchers: WindowRuleMatch {
            process_name: Some("FlowShellWallpaper.UI".to_string()),
            class_substring: None,
            title_substring: None,
        },
        actions: WindowRuleActions {
            layer: None,
            column_mode: None,
            width_semantics: None,
            managed: Some(false),
        },
    });

    let initial_snapshot = PlatformSnapshot {
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
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");
    let managed_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("managed window should exist");

    let snapshot_with_wallpaper = PlatformSnapshot {
        foreground_hwnd: Some(200),
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
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "FlowShellWallpaper.UI".to_string(),
                class_name: "WinUIDesktopWin32WindowClass".to_string(),
                process_id: 5555,
                process_name: Some("FlowShellWallpaper.UI".to_string()),
                rect: Rect::new(40, 40, 640, 480),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
        ],
    };
    runtime
        .sync_snapshot(snapshot_with_wallpaper, true)
        .expect("wallpaper sync should succeed");

    let wallpaper_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("wallpaper window should be tracked");
    let wallpaper_window = runtime
        .state()
        .windows
        .get(&wallpaper_window_id)
        .expect("wallpaper window should exist");
    let workspace = runtime
        .state()
        .workspaces
        .get(&wallpaper_window.workspace_id)
        .expect("workspace should exist");

    assert!(!wallpaper_window.is_managed);
    assert_eq!(
        wallpaper_window.classification,
        WindowClassification::Utility
    );
    assert_eq!(wallpaper_window.column_id, None);
    assert!(
        !workspace
            .floating_layer
            .ordered_window_ids
            .contains(&wallpaper_window_id)
    );
    assert!(
        runtime
            .state()
            .layout
            .columns
            .values()
            .all(|column| !column.ordered_window_ids.contains(&wallpaper_window_id))
    );
    assert_eq!(
        runtime.state().focus.focused_window_id,
        Some(managed_window_id)
    );
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

    let apply_plan_context =
        runtime.build_apply_plan_context(Some(100), Some(100), "manual-cycle-column-width", false);
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
            presentation: WindowPresentation::default(),
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
        !build_visual_emphasis(false, Some("notepad"), "Notepad", "notes").disable_visual_effects
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
            presentation: WindowPresentation::default(),
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
            presentation: WindowPresentation::default(),
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
            presentation: WindowPresentation::default(),
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
        presentation: WindowPresentation::default(),
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
        presentation: WindowPresentation::default(),
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
fn workspace_switch_materialization_ignores_stale_active_visible_region_from_other_monitor() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1440, 1200),
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
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(1600, 0, 420, 1200),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
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
    let secondary_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("secondary window should exist after initial sync");
    let secondary_workspace_id = runtime
        .state()
        .windows
        .get(&secondary_window_id)
        .expect("secondary window should exist in state")
        .workspace_id;
    let secondary_desired_rect = recompute_workspace(runtime.state(), secondary_workspace_id)
        .expect("secondary workspace projection should exist")
        .window_geometries
        .iter()
        .find(|geometry| geometry.window_id == secondary_window_id)
        .expect("secondary window geometry should exist")
        .rect;
    let mut steady_snapshot = snapshot.clone();
    steady_snapshot.windows[1].rect = secondary_desired_rect;

    let primary_monitor_id = runtime
        .state()
        .monitors
        .iter()
        .find_map(|(monitor_id, monitor)| (monitor.work_area_rect.x == 0).then_some(*monitor_id))
        .expect("primary monitor should exist");
    let workspace_ids = ordered_workspace_ids_for_monitor(runtime.state(), primary_monitor_id);
    assert_eq!(workspace_ids.len(), 2);

    runtime
        .store
        .dispatch(DomainEvent::focus_workspace_down(
            CorrelationId::new(2),
            Some(primary_monitor_id),
        ))
        .expect("focus workspace down should succeed");

    if let Some(active_workspace) = runtime
        .store
        .state_mut()
        .workspaces
        .get_mut(&workspace_ids[1])
    {
        active_workspace.strip.visible_region = snapshot.monitors[1].work_area_rect;
    }

    let planned_operations = runtime
        .plan_apply_operations_with_context(
            &steady_snapshot,
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
        .expect("previous workspace window should be moved into inactive band");
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

    assert_eq!(operation.rect.x, previous_local_rect.x);
    assert_eq!(
        operation.rect.y,
        previous_local_rect
            .y
            .saturating_sub(snapshot.monitors[0].work_area_rect.height as i32)
    );
    assert!(operation.rect.x < snapshot.monitors[1].work_area_rect.x);
}

#[test]
fn workspace_switch_on_primary_monitor_does_not_replan_secondary_monitor_geometry() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1440, 1200),
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
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(1600, 0, 420, 1200),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
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
    let secondary_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("secondary window should exist after initial sync");
    let secondary_workspace_id = runtime
        .state()
        .windows
        .get(&secondary_window_id)
        .expect("secondary window should exist in state")
        .workspace_id;
    let secondary_desired_rect = recompute_workspace(runtime.state(), secondary_workspace_id)
        .expect("secondary workspace projection should exist")
        .window_geometries
        .iter()
        .find(|geometry| geometry.window_id == secondary_window_id)
        .expect("secondary window geometry should exist")
        .rect;
    let mut steady_snapshot = snapshot.clone();
    steady_snapshot.windows[1].rect = secondary_desired_rect;

    let primary_monitor_id = runtime
        .state()
        .monitors
        .iter()
        .find_map(|(monitor_id, monitor)| (monitor.work_area_rect.x == 0).then_some(*monitor_id))
        .expect("primary monitor should exist");

    runtime
        .store
        .dispatch(DomainEvent::focus_workspace_down(
            CorrelationId::new(2),
            Some(primary_monitor_id),
        ))
        .expect("focus workspace down should succeed");

    let planned_operations = runtime
        .plan_apply_operations_with_context(
            &steady_snapshot,
            ApplyPlanContext {
                previous_focused_hwnd: Some(100),
                animate_window_switch: false,
                animate_tiled_geometry: false,
                force_activate_focused_window: false,
                refresh_visual_emphasis: true,
            },
        )
        .expect("apply plan should be computed");

    assert!(
        planned_operations
            .iter()
            .any(|operation| operation.hwnd == 100),
        "primary monitor workspace switch should still replan the previous active window"
    );
    let secondary_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 200)
        .expect("secondary monitor may still receive a visual-only refresh operation");
    assert!(
        !secondary_operation.apply_geometry,
        "secondary monitor steady-state window must not receive geometry retargeting when only the primary monitor workspace changed"
    );
    assert_eq!(secondary_operation.rect, steady_snapshot.windows[1].rect);
}

#[test]
fn primary_focus_navigation_reveal_does_not_geometry_retarget_secondary_monitor() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1440, 1200),
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
                rect: Rect::new(0, 0, 900, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
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
                rect: Rect::new(900, 0, 900, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4444,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(1600, 0, 420, 1200),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
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
    let secondary_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("secondary window should exist after initial sync");
    let secondary_workspace_id = runtime
        .state()
        .windows
        .get(&secondary_window_id)
        .expect("secondary window should exist in state")
        .workspace_id;
    let secondary_desired_rect = recompute_workspace(runtime.state(), secondary_workspace_id)
        .expect("secondary workspace projection should exist")
        .window_geometries
        .iter()
        .find(|geometry| geometry.window_id == secondary_window_id)
        .expect("secondary window geometry should exist")
        .rect;
    let mut steady_snapshot = snapshot.clone();
    steady_snapshot.windows[2].rect = secondary_desired_rect;

    runtime
        .store
        .dispatch(DomainEvent::focus_next(
            CorrelationId::new(2),
            NavigationScope::WorkspaceStrip,
        ))
        .expect("focus navigation should succeed");

    let planned_operations = runtime
        .plan_apply_operations_with_context(
            &steady_snapshot,
            ApplyPlanContext {
                previous_focused_hwnd: Some(100),
                animate_window_switch: false,
                animate_tiled_geometry: false,
                force_activate_focused_window: false,
                refresh_visual_emphasis: true,
            },
        )
        .expect("apply plan should be computed");

    let primary_reveal_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 101)
        .expect("primary monitor next column should receive an operation");
    assert!(
        primary_reveal_operation.apply_geometry,
        "focus navigation on the primary monitor should still drive reveal geometry there"
    );

    let secondary_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 200)
        .expect("secondary monitor may still receive a visual-only refresh operation");
    assert!(
        !secondary_operation.apply_geometry,
        "secondary monitor steady-state window must not receive geometry retargeting from primary focus navigation reveal"
    );
    assert_eq!(secondary_operation.rect, steady_snapshot.windows[2].rect);
}

#[test]
fn primary_strip_overflow_is_not_materialized_inside_secondary_monitor_work_area() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1440, 1200),
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
                rect: Rect::new(0, 0, 900, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
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
                rect: Rect::new(900, 0, 900, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4444,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(1600, 0, 420, 1200),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
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
    let overflow_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 101)
        .expect("overflowing primary strip window should still receive a target rect");
    let secondary_work_area = snapshot.monitors[1].work_area_rect;
    let overflow_right = overflow_operation
        .rect
        .x
        .saturating_add(overflow_operation.rect.width as i32);
    let secondary_right = secondary_work_area
        .x
        .saturating_add(secondary_work_area.width as i32);

    assert!(
        overflow_right <= secondary_work_area.x || overflow_operation.rect.x >= secondary_right,
        "primary monitor strip overflow must not become visible inside the secondary monitor work area"
    );
    assert_eq!(
        overflow_operation.presentation.mode,
        WindowPresentationMode::SurrogateClipped
    );
    assert_eq!(
        overflow_operation
            .presentation
            .surrogate
            .as_ref()
            .expect("spill operation should expose surrogate clip")
            .destination_rect,
        Rect::new(928, 16, 672, 868)
    );
    assert_eq!(
        overflow_operation
            .presentation
            .surrogate
            .as_ref()
            .expect("spill operation should expose surrogate clip")
            .source_rect,
        Rect::new(0, 0, 672, 868)
    );
    assert_eq!(
        overflow_operation
            .presentation
            .surrogate
            .as_ref()
            .expect("spill operation should expose surrogate clip")
            .native_visible_rect,
        Rect::new(928, 16, 900, 868)
    );
    let active_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 100)
        .expect("active primary window should still receive an operation");
    assert_eq!(
        active_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
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
