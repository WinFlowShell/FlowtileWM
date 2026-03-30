use std::collections::HashSet;
use std::time::{Duration, Instant};

use flowtile_domain::{
    Column, ColumnMode, MaximizedState, Rect, RuntimeMode, Size, WidthSemantics,
    WindowClassification, WindowLayer, WindowNode, WmState, WorkspaceId,
};
use flowtile_layout_engine::recompute_workspace;
use flowtile_windows_adapter::WINDOW_SWITCH_ANIMATION_DURATION_MS;

use super::{
    HWND, HWND_TOPMOST, OVERVIEW_DEFAULT_ZOOM, OVERVIEW_WORKSPACE_GAP_RATIO,
    OverlayWindowPlacement, OverviewRenderFrame, OverviewScene, PreviewClickTarget,
    SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT, SHELL_OVERLAY_RESTORE_SETTLE, SWP_NOACTIVATE,
    SWP_NOZORDER, SceneFrameMode, ShellOverlayEscapeState, ShellScreenshotWindows,
    WindowPreviewScene, WindowRenderFrame, WorkspacePreviewScene, WorkspaceRenderFrame,
    build_overview_scene, build_preview_stack_layout, hit_test_preview_targets, intersect_rect,
    is_shell_screenshot_overlay, is_shell_screenshot_result_window, overview_viewport_column_rect,
    overview_window_rect, preview_click_targets, preview_click_targets_for_frame,
    preview_shell_targets, preview_shell_targets_for_frame, preview_window_rects, rect_bottom,
    rect_right, render_frame_for_transition, resolve_z_order_target, scale_rect_to_overview,
    spring_progress, thumbnail_projection, window_rect_for_mode, workspace_open_close_source_rect,
};

#[test]
fn closed_overview_has_no_scene() {
    let state = WmState::new(RuntimeMode::WmOnly);

    let scene = build_overview_scene(&state).expect("scene should build");
    assert!(scene.is_none());
}

#[test]
fn overview_scene_centers_active_workspace_stack() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let first_workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, first_workspace_id, 100, 420);
    state.normalize_workspace_set(workspace_set_id);

    let second_workspace_id = state
        .ensure_tail_workspace(monitor_id)
        .expect("tail workspace should exist");
    add_tiled_column(&mut state, second_workspace_id, 200, 520);
    state.normalize_workspace_set(workspace_set_id);

    let third_workspace_id = state
        .ensure_tail_workspace(monitor_id)
        .expect("second tail workspace should exist");
    add_tiled_column(&mut state, third_workspace_id, 300, 480);
    state.normalize_workspace_set(workspace_set_id);

    set_active_workspace(
        &mut state,
        monitor_id,
        workspace_set_id,
        second_workspace_id,
    );

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(second_workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let layout = build_preview_stack_layout(scene.monitor_rect);
    let expected_gap =
        (900.0 * OVERVIEW_WORKSPACE_GAP_RATIO * OVERVIEW_DEFAULT_ZOOM).round() as i32;

    let first = scene
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == first_workspace_id)
        .expect("first workspace preview should remain visible");
    let second = scene
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == second_workspace_id)
        .expect("active workspace preview should exist");
    let third = scene
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == third_workspace_id)
        .expect("next workspace preview should remain visible");

    assert_eq!(second.frame_rect.height, layout.frame_height);
    assert_eq!(second.frame_rect.y, layout.active_frame_y);
    assert_eq!(
        second.frame_rect.x,
        scene.monitor_rect.x
            + (scene.monitor_rect.width.min(i32::MAX as u32) as i32
                - layout.frame_width.min(i32::MAX as u32) as i32)
                / 2
    );
    assert_eq!(second.frame_rect.width, layout.frame_width);
    assert_eq!(
        first.frame_rect.y,
        second.frame_rect.y - layout.frame_height as i32 - expected_gap
    );
    assert_eq!(
        third.frame_rect.y,
        second.frame_rect.y + layout.frame_height as i32 + expected_gap
    );
    assert!(first.frame_rect.y < scene.monitor_rect.y);
    assert!(rect_bottom(third.frame_rect) > rect_bottom(scene.monitor_rect));
    assert!(second.selected);
    assert!(!second.windows.is_empty());
}

#[test]
fn overview_scene_preserves_window_proportions_at_fixed_zoom() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 320);
    add_tiled_column(&mut state, workspace_id, 200, 1000);
    add_tiled_column(&mut state, workspace_id, 300, 260);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("active workspace preview should exist");
    let width_scales = workspace
        .windows
        .iter()
        .map(|window| window.overview_rect.width as f64 / window.live_rect.width as f64)
        .collect::<Vec<_>>();
    let height_scales = workspace
        .windows
        .iter()
        .map(|window| window.overview_rect.height as f64 / window.live_rect.height as f64)
        .collect::<Vec<_>>();

    for scale in width_scales {
        assert!((scale - OVERVIEW_DEFAULT_ZOOM).abs() < 0.02);
    }
    for scale in height_scales {
        assert!((scale - OVERVIEW_DEFAULT_ZOOM).abs() < 0.02);
    }
    assert_eq!(workspace.frame_rect.width, 800);
    assert_eq!(workspace.frame_rect.height, 450);
    assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
}

#[test]
fn overview_open_close_animation_starts_from_live_workspace_geometry() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");
    let window = workspace
        .windows
        .first()
        .copied()
        .expect("preview should contain at least one window");
    let source_rect = workspace_open_close_source_rect(workspace, scene.monitor_rect);

    assert_eq!(source_rect, workspace.live_rect);
    assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
    assert_eq!(workspace.canvas_rect.height, workspace.frame_rect.height);
    assert!(source_rect.width > workspace.frame_rect.width);
    assert!(source_rect.height > workspace.frame_rect.height);
    assert_eq!(
        window_rect_for_mode(
            window,
            workspace,
            scene.monitor_rect,
            SceneFrameMode::Opening { progress_milli: 0 }
        ),
        window.live_rect
    );
}

#[test]
fn overview_open_close_animation_uses_live_rects_for_current_ribbon_windows() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 640);
    add_tiled_column(&mut state, workspace_id, 200, 640);
    add_tiled_column(&mut state, workspace_id, 300, 640);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");

    for window in &workspace.windows {
        assert_eq!(
            window_rect_for_mode(
                *window,
                workspace,
                scene.monitor_rect,
                SceneFrameMode::Opening { progress_milli: 0 }
            ),
            window.live_rect
        );
    }
}

#[test]
fn overview_open_close_animation_keeps_neighboring_workspace_offscreen_source_geometry() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let first_workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, first_workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);
    let second_workspace_id = state
        .workspace_sets
        .get(&workspace_set_id)
        .and_then(|workspace_set| workspace_set.ordered_workspace_ids.get(1).copied())
        .expect("workspace below should exist");
    add_tiled_column(&mut state, second_workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(first_workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == second_workspace_id)
        .expect("neighbor workspace preview should exist");
    let window = workspace
        .windows
        .first()
        .copied()
        .expect("neighbor preview should contain at least one window");

    assert_eq!(
        workspace_open_close_source_rect(workspace, scene.monitor_rect),
        workspace.live_rect
    );
    assert!(workspace.live_rect.y > scene.monitor_rect.y);
    assert_eq!(
        window_rect_for_mode(
            window,
            workspace,
            scene.monitor_rect,
            SceneFrameMode::Opening { progress_milli: 0 }
        ),
        window.live_rect
    );
}

#[test]
fn overview_open_close_animation_keeps_upper_neighbor_offscreen_source_geometry() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let first_workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, first_workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);
    let second_workspace_id = state
        .workspace_sets
        .get(&workspace_set_id)
        .and_then(|workspace_set| workspace_set.ordered_workspace_ids.get(1).copied())
        .expect("workspace below should exist");
    add_tiled_column(&mut state, second_workspace_id, 200, 960);
    state.normalize_workspace_set(workspace_set_id);
    set_active_workspace(
        &mut state,
        monitor_id,
        workspace_set_id,
        second_workspace_id,
    );

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(second_workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == first_workspace_id)
        .expect("upper workspace preview should exist");
    let window = workspace
        .windows
        .first()
        .copied()
        .expect("upper preview should contain at least one window");

    assert_eq!(
        workspace_open_close_source_rect(workspace, scene.monitor_rect),
        workspace.live_rect
    );
    assert!(rect_bottom(workspace.live_rect) <= scene.monitor_rect.y);
    assert!(rect_bottom(window.live_rect) <= scene.monitor_rect.y);
    assert_eq!(
        window_rect_for_mode(
            window,
            workspace,
            scene.monitor_rect,
            SceneFrameMode::Opening { progress_milli: 0 }
        ),
        window.live_rect
    );
}

#[test]
fn selected_workspace_close_animation_scales_width_and_height_together() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");
    let window = workspace
        .windows
        .first()
        .copied()
        .expect("preview should contain at least one window");

    let mode = SceneFrameMode::Closing {
        progress_milli: 500,
    };
    let animated_rect = window_rect_for_mode(window, workspace, scene.monitor_rect, mode);

    let width_scale = animated_rect.width as f64 / window.live_rect.width as f64;
    let height_scale = animated_rect.height as f64 / window.live_rect.height as f64;
    assert!(
        (width_scale - height_scale).abs() < 0.02,
        "expected uniform scale, got width_scale={width_scale} height_scale={height_scale}"
    );
}

#[test]
fn non_selected_workspace_uses_same_preview_scale_as_selected() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let first_workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, first_workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);
    let second_workspace_id = state
        .workspace_sets
        .get(&workspace_set_id)
        .and_then(|workspace_set| workspace_set.ordered_workspace_ids.get(1).copied())
        .expect("workspace below should exist");
    add_tiled_column(&mut state, second_workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(first_workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let selected_workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == first_workspace_id)
        .expect("selected workspace preview should exist");
    let other_workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == second_workspace_id)
        .expect("unselected workspace preview should exist");

    assert_eq!(
        selected_workspace.frame_rect.width,
        other_workspace.frame_rect.width
    );
    assert_eq!(
        selected_workspace.frame_rect.height,
        other_workspace.frame_rect.height
    );
    assert_eq!(
        selected_workspace.canvas_rect.width,
        scene.monitor_rect.width
    );
    assert_eq!(other_workspace.canvas_rect.width, scene.monitor_rect.width);
    assert_eq!(
        selected_workspace.windows[0].overview_rect.width,
        other_workspace.windows[0].overview_rect.width
    );
    assert_eq!(other_workspace.live_rect.height, 900);
    assert_eq!(other_workspace.frame_rect.height, 450);
}

#[test]
fn long_workspace_ribbon_uses_monitor_wide_canvas_without_squeezing_edge_windows() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 320);
    add_tiled_column(&mut state, workspace_id, 200, 1200);
    add_tiled_column(&mut state, workspace_id, 300, 640);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");
    let rightmost_window = workspace
        .windows
        .iter()
        .max_by_key(|window| rect_right(window.overview_rect))
        .copied()
        .expect("preview should contain windows");
    let widest_live_window = workspace
        .windows
        .iter()
        .max_by_key(|window| window.live_rect.width)
        .copied()
        .expect("preview should contain windows");

    assert_eq!(workspace.frame_rect.width, 800);
    assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
    assert_eq!(workspace.canvas_rect.height, workspace.frame_rect.height);
    assert_eq!(workspace.canvas_rect.y, workspace.frame_rect.y);
    assert!(
        (widest_live_window.overview_rect.width as f64 / widest_live_window.live_rect.width as f64
            - OVERVIEW_DEFAULT_ZOOM)
            .abs()
            < 0.02
    );
    assert!(rect_right(rightmost_window.overview_rect) > rect_right(workspace.frame_rect));
}

#[test]
fn overview_mapping_preserves_leftward_offset_outside_viewport_frame() {
    let viewport_rect = Rect::new(500, 100, 800, 400);
    let target_rect = Rect::new(600, 200, 400, 200);
    let left_window_rect = Rect::new(300, 100, 400, 400);

    let overview_rect = overview_window_rect(
        left_window_rect,
        viewport_rect,
        target_rect,
        WindowLayer::Tiled,
    );

    assert!(overview_rect.x < target_rect.x);
    assert!(rect_right(overview_rect) > target_rect.x);
    assert_eq!(overview_rect.width, 200);
    assert_eq!(overview_rect.height, 200);
}

#[test]
fn preview_hit_testing_prefers_last_visible_window() {
    let click_targets = vec![
        PreviewClickTarget {
            hwnd: 100,
            workspace_id: WorkspaceId::new(1),
            rect: Rect::new(10, 10, 100, 100),
        },
        PreviewClickTarget {
            hwnd: 200,
            workspace_id: WorkspaceId::new(1),
            rect: Rect::new(40, 40, 100, 100),
        },
    ];

    assert_eq!(hit_test_preview_targets(&click_targets, 20, 20), Some(100));
    assert_eq!(hit_test_preview_targets(&click_targets, 60, 60), Some(200));
    assert_eq!(hit_test_preview_targets(&click_targets, 500, 500), None);
}

#[test]
fn thumbnail_projection_crops_left_edge_without_rescaling() {
    let window_rect = Rect::new(100, 50, 400, 200);
    let clipped_rect = Rect::new(200, 50, 300, 200);
    let canvas_rect = Rect::new(0, 0, 600, 300);
    let source_rect = Rect::new(0, 0, 800, 400);

    let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
        .expect("projection should exist");

    assert_eq!(projection.destination_rect, Rect::new(200, 50, 300, 200));
    assert_eq!(projection.source_rect, Rect::new(200, 0, 600, 400));
}

#[test]
fn thumbnail_projection_crops_right_edge_without_rescaling() {
    let window_rect = Rect::new(100, 50, 400, 200);
    let clipped_rect = Rect::new(100, 50, 260, 200);
    let canvas_rect = Rect::new(0, 0, 360, 300);
    let source_rect = Rect::new(0, 0, 800, 400);

    let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
        .expect("projection should exist");

    assert_eq!(projection.destination_rect, Rect::new(100, 50, 260, 200));
    assert_eq!(projection.source_rect, Rect::new(0, 0, 520, 400));
}

#[test]
fn thumbnail_projection_keeps_full_window_source_when_window_is_unclipped() {
    let window_rect = Rect::new(100, 50, 400, 200);
    let clipped_rect = window_rect;
    let canvas_rect = Rect::new(0, 0, 600, 300);
    let source_rect = Rect::new(0, 0, 1016, 700);

    let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
        .expect("projection should exist");

    assert_eq!(projection.destination_rect, Rect::new(100, 50, 400, 200));
    assert_eq!(projection.source_rect, source_rect);
}

#[test]
fn full_window_preview_destination_expands_by_scaled_non_client_insets() {
    let visible_destination_rect = Rect::new(200, 50, 400, 200);
    let outer_rect = Rect::new(100, 100, 1016, 700);
    let visible_rect = Rect::new(108, 100, 1000, 700);

    let expanded = super::expand_destination_rect_to_outer_bounds(
        visible_destination_rect,
        outer_rect,
        visible_rect,
    )
    .expect("expanded destination should exist");

    assert_eq!(expanded, Rect::new(196, 50, 408, 200));
}

#[test]
fn thumbnail_projection_preserves_non_zero_source_origin_without_extra_shift() {
    let window_rect = Rect::new(100, 50, 400, 200);
    let clipped_rect = Rect::new(100, 50, 400, 200);
    let canvas_rect = Rect::new(0, 0, 600, 300);
    let source_rect = Rect::new(8, 0, 1000, 400);

    let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
        .expect("projection should exist");

    assert_eq!(projection.destination_rect, Rect::new(100, 50, 400, 200));
    assert_eq!(projection.source_rect, Rect::new(8, 0, 1000, 400));
}

#[test]
fn maximized_tiled_preview_stays_inside_viewport_bounds() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 960);
    state.normalize_workspace_set(workspace_set_id);
    let column_id = state
        .workspaces
        .get(&workspace_id)
        .and_then(|workspace| workspace.strip.ordered_column_ids.first().copied())
        .expect("workspace should contain a column");
    state
        .layout
        .columns
        .get_mut(&column_id)
        .expect("column should exist")
        .maximized_state = MaximizedState::Maximized;
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");
    let projection = recompute_workspace(&state, workspace_id).expect("layout should exist");
    let window = workspace
        .windows
        .first()
        .copied()
        .expect("workspace preview should contain a window");
    let expected_viewport_rect = scale_rect_to_overview(
        projection.viewport,
        scene.monitor_rect,
        workspace.frame_rect,
    );

    assert_eq!(window.overview_rect.x, expected_viewport_rect.x);
    assert_eq!(window.overview_rect.y, expected_viewport_rect.y);
    assert_eq!(window.overview_rect.width, expected_viewport_rect.width);
    assert_eq!(window.overview_rect.height, expected_viewport_rect.height);
}

#[test]
fn underfilled_tiled_preview_windows_do_not_overlap_after_overview_mapping() {
    let mut state = WmState::new(RuntimeMode::WmOnly);
    let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
    let workspace_set_id = state
        .workspace_set_id_for_monitor(monitor_id)
        .expect("workspace set should exist");
    let workspace_id = state
        .active_workspace_id_for_monitor(monitor_id)
        .expect("active workspace should exist");

    add_tiled_column(&mut state, workspace_id, 100, 760);
    add_tiled_column(&mut state, workspace_id, 200, 780);
    state.normalize_workspace_set(workspace_set_id);

    state.overview.is_open = true;
    state.overview.monitor_id = Some(monitor_id);
    state.overview.selection = Some(workspace_id);

    let scene = build_overview_scene(&state)
        .expect("scene should build")
        .expect("overview scene should exist");
    let workspace = scene
        .workspaces
        .iter()
        .find(|candidate| candidate.workspace_id == workspace_id)
        .expect("workspace preview should exist");
    let mut windows = workspace.windows.clone();
    windows.sort_by_key(|window| window.overview_rect.x);

    assert_eq!(windows.len(), 2);
    assert!(
        rect_right(windows[0].overview_rect) <= windows[1].overview_rect.x,
        "expected no overlap, left={:?} right={:?}",
        windows[0].overview_rect,
        windows[1].overview_rect
    );
}

#[test]
fn preview_click_targets_ignore_windows_without_visible_thumbnails() {
    let workspace = WorkspaceRenderFrame {
        workspace_id: WorkspaceId::new(1),
        canvas_rect: Rect::new(0, 0, 1600, 320),
        viewport_rect: Rect::new(0, 0, 1600, 320),
        windows: vec![
            WindowRenderFrame {
                hwnd: 100,
                rect: Rect::new(512, 290, 288, 320),
            },
            WindowRenderFrame {
                hwnd: 200,
                rect: Rect::new(800, 290, 288, 320),
            },
        ],
    };
    let visible_hwnds = HashSet::from([200_u64]);

    let click_targets = preview_click_targets(
        &workspace,
        workspace.canvas_rect,
        &visible_hwnds,
        workspace.canvas_rect,
    );

    assert_eq!(click_targets.len(), 1);
    assert_eq!(click_targets[0].hwnd, 200);
}

#[test]
fn preview_window_rects_keep_workspace_geometry_without_thumbnails() {
    let workspace = WorkspaceRenderFrame {
        workspace_id: WorkspaceId::new(1),
        canvas_rect: Rect::new(0, 0, 1600, 320),
        viewport_rect: Rect::new(0, 0, 1600, 320),
        windows: vec![
            WindowRenderFrame {
                hwnd: 100,
                rect: Rect::new(512, 290, 288, 320),
            },
            WindowRenderFrame {
                hwnd: 200,
                rect: Rect::new(800, 290, 288, 320),
            },
        ],
    };

    let window_rects =
        preview_window_rects(&workspace, workspace.canvas_rect, workspace.canvas_rect);

    assert_eq!(window_rects.len(), 2);
    assert_eq!(window_rects[0].hwnd, 100);
    assert_eq!(window_rects[1].hwnd, 200);
}

#[test]
fn preview_shell_targets_preserve_ribbon_geometry_outside_viewport_column() {
    let workspace = WorkspaceRenderFrame {
        workspace_id: WorkspaceId::new(1),
        canvas_rect: Rect::new(0, 0, 1600, 320),
        viewport_rect: Rect::new(512, 0, 576, 320),
        windows: vec![
            WindowRenderFrame {
                hwnd: 100,
                rect: Rect::new(512, 0, 628, 320),
            },
            WindowRenderFrame {
                hwnd: 200,
                rect: Rect::new(1140, 0, 360, 320),
            },
        ],
    };

    let shell_targets =
        preview_shell_targets(&workspace, workspace.canvas_rect, workspace.canvas_rect);

    assert_eq!(shell_targets.len(), 2);
    assert_eq!(shell_targets[0].rect, Rect::new(512, 0, 628, 320));
    assert_eq!(shell_targets[1].rect, Rect::new(1140, 0, 360, 320));
}

#[test]
fn viewport_column_spans_full_monitor_height() {
    let frame = OverviewRenderFrame {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(1),
                canvas_rect: Rect::new(0, 120, 1600, 320),
                viewport_rect: Rect::new(512, 120, 576, 320),
                windows: Vec::new(),
            },
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(2),
                canvas_rect: Rect::new(0, 520, 1600, 320),
                viewport_rect: Rect::new(512, 520, 576, 320),
                windows: Vec::new(),
            },
        ],
    };

    let column = overview_viewport_column_rect(&frame).expect("column should exist");

    assert_eq!(column.x, 512);
    assert_eq!(column.y, 0);
    assert_eq!(column.width, 576);
    assert_eq!(column.height, 900);
}

#[test]
fn viewport_column_uses_closest_visible_preview_geometry() {
    let frame = OverviewRenderFrame {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(1),
                canvas_rect: Rect::new(0, -40, 1600, 320),
                viewport_rect: Rect::new(420, -40, 520, 320),
                windows: Vec::new(),
            },
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(2),
                canvas_rect: Rect::new(0, 290, 1600, 320),
                viewport_rect: Rect::new(560, 290, 640, 320),
                windows: Vec::new(),
            },
        ],
    };

    let column = overview_viewport_column_rect(&frame).expect("column should exist");

    assert_eq!(column.x, 560);
    assert_eq!(column.y, 0);
    assert_eq!(column.width, 640);
    assert_eq!(column.height, 900);
}

#[test]
fn relative_overview_layers_are_promoted_above_the_backdrop() {
    let placement = OverlayWindowPlacement {
        rect: Some(Rect::new(0, 0, 100, 100)),
        visible: true,
        topmost: true,
        child_insert_after: None,
    };
    let mut flags = SWP_NOACTIVATE;
    let anchor = 0x1234usize as HWND;

    let target = resolve_z_order_target(Some(anchor), true, &placement, &mut flags);

    assert_eq!(target as isize, HWND_TOPMOST as isize);
    assert_eq!(flags & SWP_NOZORDER, 0);
}

#[test]
fn shell_escape_restores_after_overlay_disappears() {
    let started_at = Instant::now();
    let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            overlay_present: true,
            ..ShellScreenshotWindows::default()
        },
        started_at
    ));
    assert!(!state.should_restore(&ShellScreenshotWindows::default(), started_at));
    assert!(!state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + SHELL_OVERLAY_RESTORE_SETTLE / 2
    ));
    assert!(state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + SHELL_OVERLAY_RESTORE_SETTLE + Duration::from_millis(1)
    ));
}

#[test]
fn shell_escape_does_not_restore_until_session_is_observed() {
    let started_at = Instant::now();
    let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

    assert!(!state.should_restore(&ShellScreenshotWindows::default(), started_at));
    assert!(!state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + Duration::from_secs(5)
    ));
}

#[test]
fn shell_escape_waits_until_new_snipping_tool_result_window_disappears() {
    let started_at = Instant::now();
    let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            overlay_present: true,
            ..ShellScreenshotWindows::default()
        },
        started_at
    ));
    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            result_window_hwnds: HashSet::from([777_u64]),
            ..ShellScreenshotWindows::default()
        },
        started_at + Duration::from_millis(10)
    ));
    assert!(!state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + Duration::from_millis(10) + SHELL_OVERLAY_RESTORE_SETTLE / 2
    ));
    assert!(!state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at
            + Duration::from_millis(10)
            + SHELL_OVERLAY_RESTORE_SETTLE
            + Duration::from_millis(1)
    ));
    assert!(state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at
            + Duration::from_millis(10)
            + SHELL_OVERLAY_RESTORE_SETTLE * 2
            + Duration::from_millis(1)
    ));
}

#[test]
fn shell_escape_ignores_baseline_snipping_tool_window_when_no_new_result_appears() {
    let started_at = Instant::now();
    let baseline = HashSet::from([111_u64]);
    let mut state = ShellOverlayEscapeState::new(started_at, baseline.clone(), None);

    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            overlay_present: true,
            result_window_hwnds: baseline.clone(),
            ..ShellScreenshotWindows::default()
        },
        started_at
    ));
    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            result_window_hwnds: baseline,
            ..ShellScreenshotWindows::default()
        },
        started_at
    ));
    assert!(state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + SHELL_OVERLAY_RESTORE_SETTLE + Duration::from_millis(1)
    ));
}

#[test]
fn shell_escape_waits_while_foreground_screenshot_ui_is_active() {
    let started_at = Instant::now();
    let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            foreground_screenshot_hwnd: Some(777),
            ..ShellScreenshotWindows::default()
        },
        started_at + Duration::from_millis(1)
    ));
    assert!(!state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + SHELL_OVERLAY_RESTORE_SETTLE / 2
    ));
    assert!(state.should_restore(
        &ShellScreenshotWindows::default(),
        started_at + SHELL_OVERLAY_RESTORE_SETTLE * 2 + Duration::from_millis(1)
    ));
}

#[test]
fn shell_escape_ignores_baseline_foreground_screenshot_window() {
    let started_at = Instant::now();
    let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), Some(555));

    assert!(!state.should_restore(
        &ShellScreenshotWindows {
            foreground_screenshot_hwnd: Some(555),
            ..ShellScreenshotWindows::default()
        },
        started_at
    ));
    assert!(state.should_restore(
        &ShellScreenshotWindows {
            foreground_screenshot_hwnd: Some(555),
            ..ShellScreenshotWindows::default()
        },
        started_at + SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT + Duration::from_millis(1)
    ));
}

#[test]
fn preview_targets_are_clipped_to_visible_monitor_canvas() {
    let workspace = WorkspaceRenderFrame {
        workspace_id: WorkspaceId::new(1),
        canvas_rect: Rect::new(0, -50, 1600, 320),
        viewport_rect: Rect::new(0, -50, 1600, 320),
        windows: vec![WindowRenderFrame {
            hwnd: 100,
            rect: Rect::new(0, -50, 1600, 320),
        }],
    };
    let monitor_rect = Rect::new(0, 0, 1600, 900);
    let visible_canvas_rect =
        intersect_rect(workspace.canvas_rect, monitor_rect).expect("canvas should intersect");
    let visible_hwnds = HashSet::from([100_u64]);

    let click_targets = preview_click_targets(
        &workspace,
        visible_canvas_rect,
        &visible_hwnds,
        visible_canvas_rect,
    );

    assert_eq!(click_targets.len(), 1);
    assert_eq!(click_targets[0].rect.y, 0);
    assert!(click_targets[0].rect.height <= visible_canvas_rect.height);
}

#[test]
fn frame_shell_targets_preserve_monitor_relative_scene_geometry() {
    let frame = OverviewRenderFrame {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(1),
                canvas_rect: Rect::new(0, 120, 1600, 320),
                viewport_rect: Rect::new(512, 120, 576, 320),
                windows: vec![WindowRenderFrame {
                    hwnd: 100,
                    rect: Rect::new(512, 120, 628, 320),
                }],
            },
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(2),
                canvas_rect: Rect::new(0, 520, 1600, 320),
                viewport_rect: Rect::new(512, 520, 576, 320),
                windows: vec![WindowRenderFrame {
                    hwnd: 200,
                    rect: Rect::new(1140, 520, 360, 320),
                }],
            },
        ],
    };

    let mut shell_targets = preview_shell_targets_for_frame(&frame, frame.monitor_rect);
    shell_targets.sort_by_key(|target| target.hwnd);

    assert_eq!(shell_targets.len(), 2);
    assert_eq!(shell_targets[0].rect, Rect::new(512, 120, 628, 320));
    assert_eq!(shell_targets[1].rect, Rect::new(1140, 520, 360, 320));
}

#[test]
fn frame_click_targets_use_monitor_relative_origin_across_workspaces() {
    let frame = OverviewRenderFrame {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(1),
                canvas_rect: Rect::new(0, -50, 1600, 320),
                viewport_rect: Rect::new(512, -50, 576, 320),
                windows: vec![WindowRenderFrame {
                    hwnd: 100,
                    rect: Rect::new(0, -50, 1600, 320),
                }],
            },
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(2),
                canvas_rect: Rect::new(0, 520, 1600, 320),
                viewport_rect: Rect::new(512, 520, 576, 320),
                windows: vec![WindowRenderFrame {
                    hwnd: 200,
                    rect: Rect::new(640, 520, 320, 320),
                }],
            },
        ],
    };
    let visible_hwnds = HashSet::from([100_u64, 200_u64]);

    let mut click_targets =
        preview_click_targets_for_frame(&frame, frame.monitor_rect, &visible_hwnds);
    click_targets.sort_by_key(|target| target.hwnd);

    assert_eq!(click_targets.len(), 2);
    assert_eq!(click_targets[0].rect, Rect::new(0, 0, 1600, 270));
    assert_eq!(click_targets[1].rect, Rect::new(640, 520, 320, 320));
}

#[test]
fn transition_frame_interpolates_workspace_and_window_rects() {
    let from_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 200, 1600, 320),
            frame_rect: Rect::new(512, 200, 576, 320),
            selected: true,
            windows: vec![WindowPreviewScene {
                hwnd: 100,
                live_rect: Rect::new(0, 0, 800, 900),
                overview_rect: Rect::new(512, 200, 288, 320),
            }],
        }],
    };
    let to_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 320, 1600, 320),
            frame_rect: Rect::new(512, 320, 576, 320),
            selected: true,
            windows: vec![WindowPreviewScene {
                hwnd: 100,
                live_rect: Rect::new(0, 0, 800, 900),
                overview_rect: Rect::new(640, 320, 288, 320),
            }],
        }],
    };

    let frame = render_frame_for_transition(&from_scene, &to_scene, 500);

    assert_eq!(
        frame.workspaces[0],
        WorkspaceRenderFrame {
            workspace_id: WorkspaceId::new(1),
            canvas_rect: Rect::new(0, 260, 1600, 320),
            viewport_rect: Rect::new(512, 260, 576, 320),
            windows: vec![WindowRenderFrame {
                hwnd: 100,
                rect: Rect::new(576, 260, 288, 320),
            }],
        }
    );
}

#[test]
fn transition_frame_moves_entering_and_leaving_workspaces_with_stack_delta() {
    let from_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, -68, 1600, 320),
                frame_rect: Rect::new(512, -68, 576, 320),
                selected: false,
                windows: vec![WindowPreviewScene {
                    hwnd: 101,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, -68, 288, 320),
                }],
            },
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(2),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, 288, 288, 320),
                }],
            },
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(3),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 644, 1600, 320),
                frame_rect: Rect::new(512, 644, 576, 320),
                selected: false,
                windows: vec![WindowPreviewScene {
                    hwnd: 301,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, 644, 288, 320),
                }],
            },
        ],
    };
    let to_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(2),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, -68, 1600, 320),
                frame_rect: Rect::new(512, -68, 576, 320),
                selected: false,
                windows: vec![WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, -68, 288, 320),
                }],
            },
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(3),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![WindowPreviewScene {
                    hwnd: 301,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, 288, 288, 320),
                }],
            },
            WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(4),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 644, 1600, 320),
                frame_rect: Rect::new(512, 644, 576, 320),
                selected: false,
                windows: vec![WindowPreviewScene {
                    hwnd: 401,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, 644, 288, 320),
                }],
            },
        ],
    };

    let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
    let leaving = frame
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == WorkspaceId::new(1))
        .expect("leaving workspace should remain animated");
    let entering = frame
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == WorkspaceId::new(4))
        .expect("entering workspace should be animated from offscreen");

    assert_eq!(leaving.canvas_rect.y, -246);
    assert_eq!(leaving.windows[0].rect.y, -246);
    assert_eq!(entering.canvas_rect.y, 822);
    assert_eq!(entering.windows[0].rect.y, 822);
    assert_eq!(leaving.windows[0].rect.width, 288);
    assert_eq!(entering.windows[0].rect.width, 288);
}

#[test]
fn transition_frame_brings_entering_window_from_ribbon_edge() {
    let from_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 288, 1600, 320),
            frame_rect: Rect::new(512, 288, 576, 320),
            selected: true,
            windows: vec![
                WindowPreviewScene {
                    hwnd: 101,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(200, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(800, 0, 800, 900),
                    overview_rect: Rect::new(600, 288, 400, 320),
                },
            ],
        }],
    };
    let to_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 288, 1600, 320),
            frame_rect: Rect::new(512, 288, 576, 320),
            selected: true,
            windows: vec![
                WindowPreviewScene {
                    hwnd: 101,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(0, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(800, 0, 800, 900),
                    overview_rect: Rect::new(400, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 301,
                    live_rect: Rect::new(1600, 0, 800, 900),
                    overview_rect: Rect::new(800, 288, 400, 320),
                },
            ],
        }],
    };

    let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
    let entering = frame.workspaces[0]
        .windows
        .iter()
        .find(|window| window.hwnd == 301)
        .expect("entering window should remain part of the transition");

    assert_eq!(entering.rect.x, 900);
    assert_eq!(entering.rect.y, 288);
    assert_eq!(entering.rect.width, 400);
    assert_eq!(entering.rect.height, 320);
}

#[test]
fn transition_frame_pushes_leaving_window_out_through_ribbon_edge() {
    let from_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 288, 1600, 320),
            frame_rect: Rect::new(512, 288, 576, 320),
            selected: true,
            windows: vec![
                WindowPreviewScene {
                    hwnd: 101,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(0, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(800, 0, 800, 900),
                    overview_rect: Rect::new(400, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 301,
                    live_rect: Rect::new(1600, 0, 800, 900),
                    overview_rect: Rect::new(800, 288, 400, 320),
                },
            ],
        }],
    };
    let to_scene = OverviewScene {
        monitor_rect: Rect::new(0, 0, 1600, 900),
        workspaces: vec![WorkspacePreviewScene {
            workspace_id: WorkspaceId::new(1),
            live_rect: Rect::new(0, 0, 1600, 900),
            canvas_rect: Rect::new(0, 288, 1600, 320),
            frame_rect: Rect::new(512, 288, 576, 320),
            selected: true,
            windows: vec![
                WindowPreviewScene {
                    hwnd: 201,
                    live_rect: Rect::new(800, 0, 800, 900),
                    overview_rect: Rect::new(200, 288, 400, 320),
                },
                WindowPreviewScene {
                    hwnd: 301,
                    live_rect: Rect::new(1600, 0, 800, 900),
                    overview_rect: Rect::new(600, 288, 400, 320),
                },
            ],
        }],
    };

    let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
    let leaving = frame.workspaces[0]
        .windows
        .iter()
        .find(|window| window.hwnd == 101)
        .expect("leaving window should remain part of the transition");

    assert_eq!(leaving.rect.x, -100);
    assert_eq!(leaving.rect.y, 288);
    assert_eq!(leaving.rect.width, 400);
    assert_eq!(leaving.rect.height, 320);
}

#[test]
fn shell_screenshot_overlay_classifier_tracks_capture_toolbar_windows() {
    assert!(is_shell_screenshot_overlay(
        "ApplicationFrameWindow",
        "Screen clip",
        Some("ScreenClippingHost"),
    ));
    assert!(is_shell_screenshot_overlay(
        "ApplicationFrameWindow",
        "Ножницы",
        Some("Unknown"),
    ));
    assert!(is_shell_screenshot_overlay(
        "Microsoft.UI.Content.DesktopChildSiteBridge",
        "Панель инструментов записи",
        Some("SnippingTool.exe"),
    ));
    assert!(is_shell_screenshot_overlay(
        "Windows.UI.Core.CoreWindow",
        "Recording toolbar",
        Some("SnippingTool.exe"),
    ));
}

#[test]
fn shell_screenshot_result_classifier_tracks_snipping_tool_session_windows() {
    assert!(is_shell_screenshot_result_window(
        "ApplicationFrameWindow",
        "Snipping Tool",
        Some("SnippingTool.exe"),
    ));
    assert!(is_shell_screenshot_result_window(
        "ApplicationFrameWindow",
        "Recording toolbar",
        Some("SnippingTool.exe"),
    ));
    assert!(!is_shell_screenshot_result_window(
        "ApplicationFrameWindow",
        "PowerShell",
        Some("WindowsTerminal.exe"),
    ));
}

#[test]
fn overview_spring_nearly_settles_within_window_switch_timebox() {
    let progress = spring_progress(Duration::from_millis(u64::from(
        WINDOW_SWITCH_ANIMATION_DURATION_MS,
    )));

    assert!(
        progress >= 0.95,
        "expected overview spring to nearly settle within window-switch time-box, got {progress}"
    );
}

fn set_active_workspace(
    state: &mut WmState,
    monitor_id: flowtile_domain::MonitorId,
    workspace_set_id: flowtile_domain::WorkspaceSetId,
    workspace_id: WorkspaceId,
) {
    state
        .workspace_sets
        .get_mut(&workspace_set_id)
        .expect("workspace set should exist")
        .active_workspace_id = workspace_id;
    state
        .focus
        .active_workspace_by_monitor
        .insert(monitor_id, workspace_id);
    state.normalize_workspace_set(workspace_set_id);
}

fn add_tiled_column(state: &mut WmState, workspace_id: WorkspaceId, hwnd: u64, width: u32) {
    let window_id = state.allocate_window_id();
    let column_id = state.allocate_column_id();
    state.layout.columns.insert(
        column_id,
        Column::new(
            column_id,
            ColumnMode::Normal,
            WidthSemantics::Fixed(width),
            vec![window_id],
        ),
    );
    state.windows.insert(
        window_id,
        WindowNode {
            id: window_id,
            current_hwnd_binding: Some(hwnd),
            classification: WindowClassification::Application,
            layer: WindowLayer::Tiled,
            workspace_id,
            column_id: Some(column_id),
            is_managed: true,
            is_floating: false,
            is_fullscreen: false,
            restore_target: None,
            last_known_rect: Rect::new(0, 0, width, 900),
            desired_size: Size::new(width, 900),
        },
    );
    state
        .workspaces
        .get_mut(&workspace_id)
        .expect("workspace should exist")
        .strip
        .ordered_column_ids
        .push(column_id);
}
