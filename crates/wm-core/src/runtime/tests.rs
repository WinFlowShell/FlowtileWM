use flowtile_config_rules::{WindowRule, WindowRuleActions, WindowRuleDecision, WindowRuleMatch};
use std::collections::BTreeSet;

use flowtile_domain::{
    CorrelationId, DomainEvent, MonitorId, NavigationScope, Rect, ResizeEdge, RuntimeMode,
    WidthSemantics, WindowClassification, WindowLayer,
};
use flowtile_layout_engine::recompute_workspace;
use flowtile_windows_adapter::{
    ApplyOperation, PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot,
    WindowMonitorScene, WindowMonitorSceneSlice, WindowMonitorSceneSliceKind, WindowOpacityMode,
    WindowPresentation, WindowPresentationMode, WindowPresentationOverride, WindowSurrogateClip,
};

use crate::CoreDaemonRuntime;

use super::{
    ApplyPlanContext, WindowVisualSafety, build_monitor_local_desktop_projection,
    build_visual_emphasis, build_window_presentation_projections, classify_window_visual_safety,
    has_transient_topology_churn, materialized_presentation_cleanup_operations,
    operations_are_activation_only, presentation_cleanup_hwnds,
    presentation_has_auxiliary_surfaces, should_auto_unwind_after_desync,
    should_skip_strict_geometry_revalidation, should_sync_presentation,
    stale_materialized_presentation_hwnds,
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
fn auxiliary_surface_detection_treats_monitor_scene_as_presentation_work_even_in_native_visible_mode()
 {
    assert!(presentation_has_auxiliary_surfaces(&WindowPresentation {
        mode: WindowPresentationMode::NativeVisible,
        surrogate: None,
        monitor_scene: WindowMonitorScene {
            home_visible_rect: Some(Rect::new(928, 16, 672, 868)),
            slices: vec![WindowMonitorSceneSlice {
                kind: WindowMonitorSceneSliceKind::ForeignMonitorSurrogate,
                monitor_rect: Rect::new(1600, 0, 1440, 1200),
                destination_rect: Rect::new(1600, 16, 228, 868),
                source_rect: Rect::new(672, 0, 228, 868),
                native_visible_rect: Rect::new(928, 16, 900, 868),
            }],
        },
    }));
}

#[test]
fn auxiliary_surface_detection_treats_home_clip_only_monitor_scene_as_presentation_work() {
    assert!(presentation_has_auxiliary_surfaces(&WindowPresentation {
        mode: WindowPresentationMode::NativeVisible,
        surrogate: None,
        monitor_scene: WindowMonitorScene {
            home_visible_rect: Some(Rect::new(928, 16, 672, 868)),
            slices: Vec::new(),
        },
    }));
}

#[test]
fn auxiliary_surface_detection_treats_surrogate_metadata_as_presentation_work() {
    assert!(presentation_has_auxiliary_surfaces(&WindowPresentation {
        mode: WindowPresentationMode::NativeVisible,
        surrogate: Some(WindowSurrogateClip {
            destination_rect: Rect::new(928, 16, 672, 868),
            source_rect: Rect::new(0, 0, 672, 868),
            native_visible_rect: Rect::new(928, 16, 900, 868),
        }),
        monitor_scene: WindowMonitorScene::default(),
    }));
    assert!(!presentation_has_auxiliary_surfaces(
        &WindowPresentation::default()
    ));
}

#[test]
fn overview_still_requests_presentation_sync_when_auxiliary_surfaces_are_materialized() {
    let hwnd = 101_u64;
    let materialized = std::iter::once(hwnd).collect::<BTreeSet<_>>();

    assert!(should_sync_presentation(
        true,
        hwnd,
        &WindowPresentation::default(),
        false,
        &materialized,
    ));
}

#[test]
fn overview_without_materialized_surfaces_does_not_force_presentation_sync() {
    assert!(!should_sync_presentation(
        true,
        101,
        &WindowPresentation::default(),
        false,
        &BTreeSet::new(),
    ));
}

#[test]
fn materialized_presentation_cleanup_operations_add_cleanup_only_ops_for_unplanned_hwnds() {
    let actual_window = PlatformWindowSnapshot {
        hwnd: 101,
        title: "Window 101".to_string(),
        class_name: "Notepad".to_string(),
        process_id: 4242,
        process_name: Some("notepad".to_string()),
        rect: Rect::new(448, 16, 420, 868),
        monitor_binding: "\\\\.\\DISPLAY1".to_string(),
        is_visible: true,
        is_focused: false,
        management_candidate: true,
    };
    let actual_windows = std::iter::once((actual_window.hwnd, &actual_window)).collect();
    let operations = materialized_presentation_cleanup_operations(
        &actual_windows,
        &BTreeSet::new(),
        &std::iter::once(actual_window.hwnd).collect(),
    );

    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].hwnd, actual_window.hwnd);
    assert_eq!(operations[0].rect, actual_window.rect);
    assert!(!operations[0].apply_geometry);
    assert!(!operations[0].activate);
    assert_eq!(operations[0].presentation, WindowPresentation::default());
}

#[test]
fn materialized_presentation_cleanup_operations_skip_planned_and_missing_hwnds() {
    let actual_window = PlatformWindowSnapshot {
        hwnd: 101,
        title: "Window 101".to_string(),
        class_name: "Notepad".to_string(),
        process_id: 4242,
        process_name: Some("notepad".to_string()),
        rect: Rect::new(448, 16, 420, 868),
        monitor_binding: "\\\\.\\DISPLAY1".to_string(),
        is_visible: true,
        is_focused: false,
        management_candidate: true,
    };
    let actual_windows = std::iter::once((actual_window.hwnd, &actual_window)).collect();
    let desired_operation_hwnds = std::iter::once(actual_window.hwnd).collect();
    let materialized_hwnds = [actual_window.hwnd, 404].into_iter().collect();

    let operations = materialized_presentation_cleanup_operations(
        &actual_windows,
        &desired_operation_hwnds,
        &materialized_hwnds,
    );

    assert!(operations.is_empty());
}

#[test]
fn stale_materialized_presentation_hwnds_return_only_missing_hwnds() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(101),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1600, 900),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 101,
            title: "Window 101".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(448, 16, 420, 868),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };

    assert_eq!(
        stale_materialized_presentation_hwnds(
            &snapshot,
            &[101_u64, 202_u64, 303_u64].into_iter().collect()
        ),
        vec![202, 303]
    );
}

#[test]
fn presentation_cleanup_hwnds_union_managed_and_materialized_owners() {
    assert_eq!(
        presentation_cleanup_hwnds(
            &[101_u64, 202_u64],
            &[202_u64, 303_u64].into_iter().collect()
        ),
        vec![101, 202, 303]
    );
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
fn topology_churn_detects_window_count_change_after_retry() {
    assert!(has_transient_topology_churn(5, 5, 6, 0, 0));
}

#[test]
fn topology_churn_detects_discovery_activity_in_current_cycle() {
    assert!(has_transient_topology_churn(5, 5, 5, 1, 0));
}

#[test]
fn stable_topology_is_not_reported_as_churn() {
    assert!(!has_transient_topology_churn(5, 5, 5, 0, 0));
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
    assert_eq!(
        planned_operations[1].presentation.mode,
        WindowPresentationMode::SurrogateVisible
    );
    assert!(planned_operations[1].rect.x > 1200);
    assert!(planned_operations[1].suppress_visual_gap);
    assert_eq!(planned_operations[2].hwnd, 102);
    assert_eq!(
        planned_operations[2].presentation.mode,
        WindowPresentationMode::SurrogateVisible
    );
    assert!(planned_operations[2].rect.x > 1200);
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
fn auxiliary_titleless_surface_does_not_promote_into_managed_strip() {
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
            title: "Без имени — Блокнот".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(0, 0, 900, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_auxiliary = PlatformSnapshot {
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
                title: "Без имени — Блокнот".to_string(),
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
                title: "".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(24, 24, 860, 820),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let report = runtime
        .sync_snapshot(snapshot_with_auxiliary, true)
        .expect("second sync should succeed");

    assert_eq!(report.discovered_windows, 0);
    assert!(runtime.find_window_id_by_hwnd(101).is_none());
    assert_eq!(runtime.state().windows.len(), 1);
}

#[test]
fn titleless_primary_candidate_requires_stable_reobservation_before_promotion() {
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
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_pending = PlatformSnapshot {
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
                rect: Rect::new(0, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 101,
                title: "".to_string(),
                class_name: "FlowtileAppWindow".to_string(),
                process_id: 4343,
                process_name: Some("other-app".to_string()),
                rect: Rect::new(440, 0, 420, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let first_report = runtime
        .sync_snapshot(snapshot_with_pending.clone(), true)
        .expect("first pending sync should succeed");
    assert_eq!(first_report.discovered_windows, 0);
    assert!(
        first_report
            .summary_lines()
            .iter()
            .any(|line| line.contains("discovery trace entries: 1"))
    );
    assert!(first_report.discovery_trace_logs.iter().any(|line| {
        line.contains("hwnd=101")
            && line.contains("role=primary")
            && line.contains("disposition=pending-primary-candidate")
            && line.contains("required_ticks=3")
            && line.contains("action=pending")
    }));
    assert!(runtime.find_window_id_by_hwnd(101).is_none());

    let second_report = runtime
        .sync_snapshot(snapshot_with_pending, true)
        .expect("second pending sync should succeed");
    assert_eq!(second_report.discovered_windows, 1);
    assert!(second_report.discovery_trace_logs.iter().any(|line| {
        line.contains("hwnd=101")
            && line.contains("role=primary")
            && line.contains("action=promoted")
    }));
    assert!(runtime.find_window_id_by_hwnd(101).is_some());
}

#[test]
fn titled_overlapping_same_family_hosting_surface_is_not_promoted() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 100,
            title: "FlowShell".to_string(),
            class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
            process_id: 4242,
            process_name: Some("WindowsTerminal".to_string()),
            rect: Rect::new(0, 0, 1200, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_auxiliary = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![
            PlatformWindowSnapshot {
                hwnd: 100,
                title: "FlowShell".to_string(),
                class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                process_id: 4242,
                process_name: Some("WindowsTerminal".to_string()),
                rect: Rect::new(0, 0, 1200, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 101,
                title: "DesktopWindowXamlSource".to_string(),
                class_name: "XamlExplorerHostIslandWindow".to_string(),
                process_id: 4242,
                process_name: Some("WindowsTerminal".to_string()),
                rect: Rect::new(12, 12, 1184, 884),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let first_report = runtime
        .sync_snapshot(snapshot_with_auxiliary.clone(), true)
        .expect("first auxiliary sync should succeed");
    let second_report = runtime
        .sync_snapshot(snapshot_with_auxiliary, true)
        .expect("second auxiliary sync should succeed");

    assert_eq!(first_report.discovered_windows, 0);
    assert_eq!(second_report.discovered_windows, 0);
    assert!(first_report.discovery_trace_logs.iter().any(|line| {
        line.contains("hwnd=101")
            && line.contains("role=auxiliary")
            && line.contains("disposition=auxiliary-app-surface")
            && line.contains("action=blocked-auxiliary")
    }));
    assert!(runtime.find_window_id_by_hwnd(101).is_none());
    assert_eq!(runtime.state().windows.len(), 1);
}

#[test]
fn second_same_family_primary_window_remains_promotable() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 100,
            title: "notes-a.txt - Notepad".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(0, 0, 900, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_second_primary = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![
            PlatformWindowSnapshot {
                hwnd: 100,
                title: "notes-a.txt - Notepad".to_string(),
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
                title: "notes-b.txt - Notepad".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4242,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(930, 0, 900, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let report = runtime
        .sync_snapshot(snapshot_with_second_primary, true)
        .expect("second same-family window should sync");

    assert_eq!(report.discovered_windows, 1);
    assert!(report.discovery_trace_logs.iter().any(|line| {
        line.contains("hwnd=101")
            && line.contains("role=primary")
            && line.contains("disposition=promotable-primary-candidate")
            && line.contains("action=promoted")
    }));
    assert!(runtime.find_window_id_by_hwnd(101).is_some());
    assert_eq!(runtime.state().windows.len(), 2);
}

#[test]
fn duplicate_titleless_family_surfaces_do_not_promote_into_managed_strip() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 100,
            title: "Explorer".to_string(),
            class_name: "CabinetWClass".to_string(),
            process_id: 77,
            process_name: Some("explorer".to_string()),
            rect: Rect::new(0, 0, 1200, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_duplicates = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![
            PlatformWindowSnapshot {
                hwnd: 100,
                title: "Explorer".to_string(),
                class_name: "CabinetWClass".to_string(),
                process_id: 77,
                process_name: Some("explorer".to_string()),
                rect: Rect::new(0, 0, 1200, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 101,
                title: "".to_string(),
                class_name: "CabinetWClass".to_string(),
                process_id: 77,
                process_name: Some("explorer".to_string()),
                rect: Rect::new(1420, 140, 374, 662),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 102,
                title: "".to_string(),
                class_name: "CabinetWClass".to_string(),
                process_id: 77,
                process_name: Some("explorer".to_string()),
                rect: Rect::new(1424, 144, 370, 658),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let report = runtime
        .sync_snapshot(snapshot_with_duplicates, true)
        .expect("duplicate auxiliary sync should succeed");

    assert_eq!(report.discovered_windows, 0);
    assert!(runtime.find_window_id_by_hwnd(101).is_none());
    assert!(runtime.find_window_id_by_hwnd(102).is_none());
    assert_eq!(runtime.state().windows.len(), 1);
}

#[test]
fn titleless_non_primary_footprint_surface_does_not_promote_into_managed_strip() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 100,
            title: "IDE".to_string(),
            class_name: "SunAwtFrame".to_string(),
            process_id: 91,
            process_name: Some("idea64".to_string()),
            rect: Rect::new(0, 0, 1200, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");

    let snapshot_with_utility_surface = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: vec![PlatformMonitorSnapshot {
            binding: "\\\\.\\DISPLAY1".to_string(),
            work_area_rect: Rect::new(0, 0, 1920, 1080),
            dpi: 96,
            is_primary: true,
        }],
        windows: vec![
            PlatformWindowSnapshot {
                hwnd: 100,
                title: "IDE".to_string(),
                class_name: "SunAwtFrame".to_string(),
                process_id: 91,
                process_name: Some("idea64".to_string()),
                rect: Rect::new(0, 0, 1200, 900),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 101,
                title: "".to_string(),
                class_name: "SunAwtWindow".to_string(),
                process_id: 91,
                process_name: Some("idea64".to_string()),
                rect: Rect::new(1600, 40, 176, 50),
                monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };

    let report = runtime
        .sync_snapshot(snapshot_with_utility_surface, true)
        .expect("utility auxiliary sync should succeed");

    assert_eq!(report.discovered_windows, 0);
    assert!(runtime.find_window_id_by_hwnd(101).is_none());
    assert_eq!(runtime.state().windows.len(), 1);
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
    let new_focus = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 101)
        .expect("new focus operation should exist");
    assert!(new_focus.activate);
    assert_eq!(
        new_focus.presentation.mode,
        WindowPresentationMode::NativeVisible
    );
    let previous_focus = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 100)
        .expect("previous focus operation should exist");
    assert_ne!(
        previous_focus.presentation.mode,
        WindowPresentationMode::NativeVisible
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

    assert_eq!(previous_focus.visual_emphasis, None);
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
    assert_eq!(edge_operation.visual_emphasis, None);
    assert_ne!(
        edge_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
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
fn ayugram_windows_skip_strict_visual_safety_classification() {
    assert_eq!(
        classify_window_visual_safety(Some("AyuGram"), "Qt51517QWindowIcon", "Просмотр медиа",),
        WindowVisualSafety::SkipVisualEmphasis
    );
    assert_eq!(
        classify_window_visual_safety(
            Some("AyuGram"),
            "Qt51517QWindowIcon",
            "Some other detached window",
        ),
        WindowVisualSafety::SkipVisualEmphasis
    );
}

#[test]
fn windowsterminal_windows_keep_full_visual_safety_but_skip_strict_revalidation() {
    assert_eq!(
        classify_window_visual_safety(
            Some("WindowsTerminal"),
            "CASCADIA_HOSTING_WINDOW_CLASS",
            "PowerShell",
        ),
        WindowVisualSafety::SafeFullEmphasis
    );
    assert!(should_skip_strict_geometry_revalidation(
        Some("WindowsTerminal"),
        "CASCADIA_HOSTING_WINDOW_CLASS",
        "PowerShell",
    ));
}

#[test]
fn validation_filter_for_snapshot_skips_ayugram_geometry_retry() {
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
            title: "Просмотр медиа".to_string(),
            class_name: "Qt51517QWindowIcon".to_string(),
            process_id: 4343,
            process_name: Some("AyuGram".to_string()),
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
fn validation_filter_for_snapshot_skips_windowsterminal_geometry_retry() {
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
            title: "PowerShell".to_string(),
            class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
            process_id: 4343,
            process_name: Some("WindowsTerminal".to_string()),
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
    let primary_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("primary window should exist after initial sync");
    runtime
        .store
        .dispatch(DomainEvent::window_focus_observed(
            CorrelationId::new(2),
            MonitorId::new(1),
            primary_window_id,
        ))
        .expect("primary window should become canonical focus");
    let initial_operations = runtime
        .plan_apply_operations(&snapshot)
        .expect("initial apply plan should be computed");
    let secondary_parked_rect = initial_operations
        .iter()
        .find(|operation| operation.hwnd == 200)
        .expect("secondary inactive window should receive an initial operation")
        .rect;
    let mut steady_snapshot = snapshot.clone();
    steady_snapshot.windows[1].rect = secondary_parked_rect;

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

    assert_eq!(
        operation.presentation.mode,
        WindowPresentationMode::NativeHidden
    );
    assert!(
        operation.rect.x
            > snapshot.monitors[1]
                .work_area_rect
                .x
                .saturating_add(snapshot.monitors[1].work_area_rect.width as i32),
        "inactive workspace window should park its native owner outside the visible desktop"
    );
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
        .expect("secondary monitor window should still receive a stable surrogate operation");
    assert_eq!(
        secondary_operation.presentation.mode,
        WindowPresentationMode::SurrogateVisible
    );
    assert!(secondary_operation.rect.x > 3040);
    assert!(secondary_operation.rect.x >= steady_snapshot.windows[1].rect.x);
}

#[test]
fn initial_multi_monitor_sync_keeps_secondary_window_native_visible_during_startup_hold() {
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
                work_area_rect: Rect::new(-1920, 0, 1920, 1080),
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
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(-1920, 0, 1200, 1000),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    let report = runtime
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
    let secondary_line = report
        .window_trace_logs
        .iter()
        .find(|line| line.contains("hwnd=200 "))
        .expect("secondary window should appear in startup trace");

    assert!(
        secondary_line.contains(&format!(
            "target=({},{} {}x{})",
            secondary_desired_rect.x,
            secondary_desired_rect.y,
            secondary_desired_rect.width,
            secondary_desired_rect.height
        )),
        "startup hold should keep the secondary window in its native monitor-local rect"
    );
}

#[test]
fn cross_monitor_managed_focus_divergence_keeps_actual_foreground_window_native_visible() {
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
                work_area_rect: Rect::new(-1920, 0, 1920, 1080),
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
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(-1920, 0, 1200, 1000),
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
    let primary_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("primary window should exist after initial sync");
    runtime
        .store
        .dispatch(DomainEvent::window_focus_observed(
            CorrelationId::new(2),
            MonitorId::new(1),
            primary_window_id,
        ))
        .expect("primary window should become canonical focus");

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

    let mut divergent_snapshot = snapshot.clone();
    divergent_snapshot.foreground_hwnd = Some(200);
    divergent_snapshot.windows[0].is_focused = false;
    divergent_snapshot.windows[1].is_focused = true;

    let planned_operations = runtime
        .plan_apply_operations_with_context(
            &divergent_snapshot,
            ApplyPlanContext {
                previous_focused_hwnd: Some(100),
                animate_window_switch: false,
                animate_tiled_geometry: false,
                force_activate_focused_window: false,
                refresh_visual_emphasis: true,
            },
        )
        .expect("apply plan should be computed");

    let secondary_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 200)
        .expect("secondary focused window should still receive an operation");
    assert_eq!(
        secondary_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
    );
    assert_eq!(secondary_operation.rect, secondary_desired_rect);

    let primary_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 100)
        .expect("previously focused primary window should still receive an operation");
    assert_eq!(
        primary_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
    );
    assert!(
        !primary_operation.activate,
        "focus stabilization hold should suppress reverse activation reassert"
    );
}

#[test]
fn transient_platform_focus_candidate_does_not_replace_canonical_focus_without_confirmation() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(200),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(-1920, 0, 1920, 1080),
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
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(-1920, 0, 1200, 1000),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
        ],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot_with_reason(initial_snapshot.clone(), true, "initial-full-scan")
        .expect("initial sync should succeed");
    let primary_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("primary window should exist after initial sync");
    let secondary_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("secondary window should exist after initial sync");
    runtime
        .store
        .dispatch(DomainEvent::window_focus_observed(
            CorrelationId::new(2),
            MonitorId::new(2),
            secondary_window_id,
        ))
        .expect("secondary window should become canonical focus");
    assert_eq!(runtime.current_focused_hwnd(), Some(200));
    assert!(
        runtime.should_stage_platform_focus_observation(100),
        "cross-monitor external foreground should enter staged platform focus path"
    );

    let mut transient_snapshot = initial_snapshot.clone();
    transient_snapshot.foreground_hwnd = Some(100);
    transient_snapshot.windows[0].is_focused = true;
    transient_snapshot.windows[1].is_focused = false;

    runtime
        .sync_snapshot_with_reason(transient_snapshot, true, "win-event-foreground")
        .expect("transient foreground observation should succeed");

    assert_eq!(
        runtime.state().focus.focused_window_id,
        Some(secondary_window_id),
        "first external foreground spike must not immediately replace canonical focus"
    );
    assert_eq!(
        runtime
            .pending_platform_focus_candidate
            .as_ref()
            .map(|candidate| (candidate.observed_hwnd, candidate.stable_snapshots)),
        Some((100, 1))
    );

    runtime
        .sync_snapshot_with_reason(initial_snapshot, true, "periodic-full-scan")
        .expect("follow-up observation should succeed");

    assert_eq!(
        runtime.state().focus.focused_window_id,
        Some(secondary_window_id),
        "candidate must be dropped when the next observation returns the previous foreground"
    );
    assert_eq!(runtime.pending_platform_focus_candidate, None);
    assert_ne!(
        runtime.state().focus.focused_window_id,
        Some(primary_window_id)
    );
}

#[test]
fn confirmed_platform_focus_candidate_commits_on_follow_up_observation() {
    let initial_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(200),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(-1920, 0, 1920, 1080),
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
                is_focused: false,
                management_candidate: true,
            },
            PlatformWindowSnapshot {
                hwnd: 200,
                title: "Window 200".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
                rect: Rect::new(-1920, 0, 1200, 1000),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                is_visible: true,
                is_focused: true,
                management_candidate: true,
            },
        ],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot_with_reason(initial_snapshot.clone(), true, "initial-full-scan")
        .expect("initial sync should succeed");
    let primary_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("primary window should exist after initial sync");
    let secondary_window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("secondary window should exist after initial sync");
    runtime
        .store
        .dispatch(DomainEvent::window_focus_observed(
            CorrelationId::new(2),
            MonitorId::new(2),
            secondary_window_id,
        ))
        .expect("secondary window should become canonical focus");
    assert_eq!(runtime.current_focused_hwnd(), Some(200));
    assert!(
        runtime.should_stage_platform_focus_observation(100),
        "cross-monitor external foreground should enter staged platform focus path"
    );

    let mut candidate_snapshot = initial_snapshot.clone();
    candidate_snapshot.foreground_hwnd = Some(100);
    candidate_snapshot.windows[0].is_focused = true;
    candidate_snapshot.windows[1].is_focused = false;

    runtime
        .sync_snapshot_with_reason(candidate_snapshot.clone(), true, "win-event-foreground")
        .expect("first foreground observation should succeed");
    assert_eq!(
        runtime.state().focus.focused_window_id,
        Some(secondary_window_id)
    );

    runtime
        .sync_snapshot_with_reason(candidate_snapshot, true, "periodic-full-scan")
        .expect("confirming foreground observation should succeed");

    assert_eq!(
        runtime.state().focus.focused_window_id,
        Some(primary_window_id),
        "confirmed candidate must eventually materialize into canonical focus"
    );
    assert_eq!(runtime.pending_platform_focus_candidate, None);
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
    let initial_operations = runtime
        .plan_apply_operations(&snapshot)
        .expect("initial apply plan should be computed");
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
    let secondary_parked_rect = initial_operations
        .iter()
        .find(|operation| operation.hwnd == 200)
        .expect("secondary inactive window should receive an initial operation")
        .rect;
    let mut steady_snapshot = snapshot.clone();
    steady_snapshot.windows[2].rect = secondary_parked_rect;

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
        .expect("secondary monitor window should still receive a stable surrogate operation");
    assert!(
        !secondary_operation.apply_geometry,
        "secondary monitor inactive window should not be geometry-retargeted when focus reveal happens on another monitor and its parked surrogate rect is already stable"
    );
    assert_eq!(secondary_operation.rect, steady_snapshot.windows[2].rect);
    assert!(
        secondary_operation.rect == secondary_desired_rect
            || secondary_operation.rect == steady_snapshot.windows[2].rect,
        "secondary monitor window should keep its already-stable rect instead of receiving a new reveal-driven target"
    );
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
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices.len(),
        1
    );
    assert_eq!(
        overflow_operation
            .presentation
            .monitor_scene
            .home_visible_rect,
        Some(Rect::new(928, 16, 672, 868))
    );
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices[0].kind,
        WindowMonitorSceneSliceKind::ForeignMonitorSurrogate
    );
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices[0].monitor_rect,
        Rect::new(1600, 0, 1440, 1200)
    );
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices[0].destination_rect,
        Rect::new(1600, 16, 228, 868)
    );
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices[0].source_rect,
        Rect::new(672, 0, 228, 868)
    );
    assert_eq!(
        overflow_operation.presentation.monitor_scene.slices[0].native_visible_rect,
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
fn inactive_tiled_window_uses_surrogate_visible_even_without_monitor_spill() {
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
                title: "Window 101".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
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

    let planned_operations = runtime
        .plan_apply_operations(&snapshot)
        .expect("apply plan should be computed");
    let inactive_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 101)
        .expect("inactive tiled window should still receive an operation");

    assert_eq!(
        inactive_operation.presentation.mode,
        WindowPresentationMode::SurrogateVisible
    );
    assert_ne!(inactive_operation.rect, Rect::new(448, 16, 420, 868));
    assert!(inactive_operation.rect.x > 1600);
    assert_eq!(
        inactive_operation
            .presentation
            .surrogate
            .as_ref()
            .expect("surrogate-visible inactive window should publish surrogate metadata")
            .destination_rect,
        Rect::new(448, 16, 420, 868)
    );
}

#[test]
fn current_window_presentations_report_mode_and_reason_for_active_and_inactive_tiled_windows() {
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
                title: "Window 101".to_string(),
                class_name: "Notepad".to_string(),
                process_id: 4343,
                process_name: Some("notepad".to_string()),
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
        .sync_snapshot(snapshot, true)
        .expect("initial sync should succeed");

    let presentations = runtime
        .current_window_presentations()
        .expect("window presentation snapshot should be computed");
    let active_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("active window should exist");
    let inactive_window_id = runtime
        .find_window_id_by_hwnd(101)
        .expect("inactive window should exist");

    assert_eq!(
        presentations
            .get(&active_window_id)
            .expect("active presentation should exist")
            .mode,
        "native-visible"
    );
    assert_eq!(
        presentations
            .get(&active_window_id)
            .expect("active presentation should exist")
            .reason,
        "active-window-native"
    );
    assert_eq!(
        presentations
            .get(&inactive_window_id)
            .expect("inactive presentation should exist")
            .mode,
        "surrogate-visible"
    );
    assert_eq!(
        presentations
            .get(&inactive_window_id)
            .expect("inactive presentation should exist")
            .reason,
        "inactive-fully-visible-surrogate"
    );
}

#[test]
fn window_presentations_prefer_adapter_override_for_surrogate_windows() {
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
        ],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    runtime
        .sync_snapshot(snapshot, true)
        .expect("initial sync should succeed");

    let workspace_layouts = runtime
        .collect_workspace_layouts()
        .expect("workspace layouts should be collected");
    let desktop_projection =
        build_monitor_local_desktop_projection(runtime.state(), &workspace_layouts);
    let presentations = build_window_presentation_projections(
        runtime.state(),
        desktop_projection,
        &std::iter::once((
            101,
            WindowPresentationOverride {
                mode: WindowPresentationMode::NativeVisible,
                reason: "native-fallback:problematic-class".to_string(),
            },
        ))
        .collect(),
    );
    let active_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("active window should exist");
    let inactive_window_id = runtime
        .find_window_id_by_hwnd(101)
        .expect("inactive window should exist");

    assert_eq!(
        presentations
            .get(&active_window_id)
            .expect("active presentation should exist")
            .mode,
        "native-visible"
    );
    assert_eq!(
        presentations
            .get(&inactive_window_id)
            .expect("inactive presentation should exist")
            .mode,
        "native-visible"
    );
    assert_eq!(
        presentations
            .get(&inactive_window_id)
            .expect("inactive presentation should exist")
            .reason,
        "native-fallback:problematic-class"
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
    assert_eq!(
        edge_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
    );
}

#[test]
fn overview_open_uses_logical_rect_for_spill_window_instead_of_parked_rect() {
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
    let spill_operation = planned_operations
        .iter()
        .find(|operation| operation.hwnd == 101)
        .expect("overflowing inactive window should still receive an operation");

    assert_eq!(
        spill_operation.presentation.mode,
        WindowPresentationMode::NativeVisible
    );
    assert_eq!(spill_operation.rect, Rect::new(928, 16, 900, 868));
}

#[test]
fn windows_on_unmanaged_monitor_stay_unmanaged() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(200),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1600, 900),
                dpi: 96,
                is_primary: false,
            },
        ],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 200,
            title: "Window 200".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(1600, 0, 600, 900),
            monitor_binding: "\\\\.\\DISPLAY2".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    set_managed_monitor_bindings(&mut runtime, &["\\\\.\\DISPLAY1"]);

    runtime
        .sync_snapshot(snapshot, true)
        .expect("snapshot sync should succeed");

    let window_id = runtime
        .find_window_id_by_hwnd(200)
        .expect("window should exist");
    let window = runtime
        .state()
        .windows
        .get(&window_id)
        .expect("window should exist");
    let display2_monitor_id = monitor_id_by_binding(runtime.state(), "\\\\.\\DISPLAY2")
        .expect("display2 monitor should exist");

    assert!(!window.is_managed);
    assert!(window.column_id.is_none());
    assert_eq!(
        window.workspace_id,
        runtime.state().focus.active_workspace_by_monitor[&display2_monitor_id]
    );
}

#[test]
fn moving_window_from_managed_to_unmanaged_monitor_reclassifies_it() {
    let initial_snapshot = PlatformSnapshot {
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
                work_area_rect: Rect::new(1600, 0, 1600, 900),
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
            rect: Rect::new(0, 0, 600, 900),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let moved_snapshot = PlatformSnapshot {
        foreground_hwnd: Some(100),
        monitors: initial_snapshot.monitors.clone(),
        windows: vec![PlatformWindowSnapshot {
            monitor_binding: "\\\\.\\DISPLAY2".to_string(),
            rect: Rect::new(1600, 0, 600, 900),
            ..initial_snapshot.windows[0].clone()
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    set_managed_monitor_bindings(&mut runtime, &["\\\\.\\DISPLAY1"]);

    runtime
        .sync_snapshot(initial_snapshot, true)
        .expect("initial sync should succeed");
    let original_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("managed window should exist");
    assert!(
        runtime.state().windows[&original_window_id].is_managed,
        "window should start managed on display1"
    );

    runtime
        .sync_snapshot(moved_snapshot, true)
        .expect("moved snapshot should sync");

    let rebound_window_id = runtime
        .find_window_id_by_hwnd(100)
        .expect("window should still be present after move");
    let rebound_window = runtime
        .state()
        .windows
        .get(&rebound_window_id)
        .expect("rebound window should exist");

    assert_ne!(rebound_window_id, original_window_id);
    assert!(!rebound_window.is_managed);
    assert!(
        !runtime.state().windows.contains_key(&original_window_id),
        "old managed window node should be destroyed before rediscovery"
    );
}

#[test]
fn changing_managed_monitor_set_reclassifies_existing_windows() {
    let snapshot = PlatformSnapshot {
        foreground_hwnd: Some(300),
        monitors: vec![
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1600, 900),
                dpi: 96,
                is_primary: true,
            },
            PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY2".to_string(),
                work_area_rect: Rect::new(1600, 0, 1600, 900),
                dpi: 96,
                is_primary: false,
            },
        ],
        windows: vec![PlatformWindowSnapshot {
            hwnd: 300,
            title: "Window 300".to_string(),
            class_name: "Notepad".to_string(),
            process_id: 4242,
            process_name: Some("notepad".to_string()),
            rect: Rect::new(1600, 0, 700, 900),
            monitor_binding: "\\\\.\\DISPLAY2".to_string(),
            is_visible: true,
            is_focused: true,
            management_candidate: true,
        }],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    set_managed_monitor_bindings(&mut runtime, &["\\\\.\\DISPLAY2"]);

    runtime
        .sync_snapshot(snapshot.clone(), true)
        .expect("initial sync should succeed");
    let original_window_id = runtime
        .find_window_id_by_hwnd(300)
        .expect("window should exist");
    assert!(
        runtime.state().windows[&original_window_id].is_managed,
        "window should start managed while display2 is selected"
    );

    set_managed_monitor_bindings(&mut runtime, &["\\\\.\\DISPLAY1"]);
    runtime
        .sync_snapshot(snapshot, true)
        .expect("sync after config change should succeed");

    let rebound_window_id = runtime
        .find_window_id_by_hwnd(300)
        .expect("window should still be present");
    let rebound_window = runtime
        .state()
        .windows
        .get(&rebound_window_id)
        .expect("rebound window should exist");

    assert_ne!(rebound_window_id, original_window_id);
    assert!(!rebound_window.is_managed);
}

#[test]
fn desktop_projection_skips_unmanaged_monitors() {
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
                work_area_rect: Rect::new(1600, 0, 1600, 900),
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
                rect: Rect::new(0, 0, 700, 900),
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
                rect: Rect::new(1600, 0, 700, 900),
                monitor_binding: "\\\\.\\DISPLAY2".to_string(),
                is_visible: true,
                is_focused: false,
                management_candidate: true,
            },
        ],
    };
    let mut runtime = CoreDaemonRuntime::new(RuntimeMode::WmOnly);
    set_managed_monitor_bindings(&mut runtime, &["\\\\.\\DISPLAY1"]);
    runtime
        .sync_snapshot(snapshot, true)
        .expect("sync should succeed");

    let workspace_layouts = runtime
        .collect_workspace_layouts()
        .expect("workspace layouts should be collected");
    let desktop_projection =
        build_monitor_local_desktop_projection(runtime.state(), &workspace_layouts);
    let display1_monitor_id = monitor_id_by_binding(runtime.state(), "\\\\.\\DISPLAY1")
        .expect("display1 monitor should exist");
    let display2_monitor_id = monitor_id_by_binding(runtime.state(), "\\\\.\\DISPLAY2")
        .expect("display2 monitor should exist");

    assert!(
        desktop_projection
            .monitors
            .contains_key(&display1_monitor_id)
    );
    assert!(
        !desktop_projection
            .monitors
            .contains_key(&display2_monitor_id)
    );
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

fn monitor_id_by_binding(
    state: &flowtile_domain::WmState,
    binding: &str,
) -> Option<flowtile_domain::MonitorId> {
    state.monitors.iter().find_map(|(monitor_id, monitor)| {
        (monitor.platform_binding.as_deref() == Some(binding)).then_some(*monitor_id)
    })
}

fn set_managed_monitor_bindings(runtime: &mut CoreDaemonRuntime, bindings: &[&str]) {
    let bindings = bindings
        .iter()
        .map(|binding| (*binding).to_string())
        .collect::<Vec<_>>();
    runtime.active_config.projection.managed_monitor_bindings = bindings.clone();
    runtime
        .last_valid_config
        .projection
        .managed_monitor_bindings = bindings.clone();
    runtime
        .store
        .state_mut()
        .config_projection
        .managed_monitor_bindings = bindings;
}
