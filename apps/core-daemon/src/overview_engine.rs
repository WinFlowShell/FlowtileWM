use std::collections::HashSet;

use flowtile_domain::{Rect, WindowLayer, WmState, WorkspaceId};
use flowtile_layout_engine::{LayoutError, WorkspaceLayoutProjection, recompute_workspace};

pub(crate) const OVERVIEW_DEFAULT_ZOOM: f64 = 0.5;
pub(crate) const OVERVIEW_WORKSPACE_GAP_RATIO: f64 = 0.1;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OverviewScene {
    pub(crate) monitor_rect: Rect,
    pub(crate) workspaces: Vec<WorkspacePreviewScene>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspacePreviewScene {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) live_rect: Rect,
    pub(crate) canvas_rect: Rect,
    pub(crate) frame_rect: Rect,
    pub(crate) selected: bool,
    pub(crate) windows: Vec<WindowPreviewScene>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowPreviewScene {
    pub(crate) hwnd: u64,
    pub(crate) live_rect: Rect,
    pub(crate) overview_rect: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OverviewRenderFrame {
    pub(crate) monitor_rect: Rect,
    pub(crate) workspaces: Vec<WorkspaceRenderFrame>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceRenderFrame {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) canvas_rect: Rect,
    pub(crate) viewport_rect: Rect,
    pub(crate) windows: Vec<WindowRenderFrame>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowRenderFrame {
    pub(crate) hwnd: u64,
    pub(crate) rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewStackLayout {
    pub(crate) frame_width: u32,
    pub(crate) frame_height: u32,
    pub(crate) gap: i32,
    pub(crate) active_frame_y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverviewRectTransform {
    source_space: Rect,
    target_space: Rect,
}

impl OverviewRectTransform {
    fn map_rect(self, source_rect: Rect) -> Rect {
        scale_rect_to_overview(source_rect, self.source_space, self.target_space)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceOverviewGeometry {
    canvas_rect: Rect,
    viewport_rect: Rect,
    tiled_window_transform: OverviewRectTransform,
    monitor_window_transform: OverviewRectTransform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewClickTarget {
    pub(crate) hwnd: u64,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceDropTarget {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneFrameMode {
    Final,
    Opening { progress_milli: u16 },
    Closing { progress_milli: u16 },
}

pub(crate) fn build_overview_scene(state: &WmState) -> Result<Option<OverviewScene>, LayoutError> {
    if !state.overview.is_open {
        return Ok(None);
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return Ok(None);
    };
    let Some(monitor) = state.monitors.get(&monitor_id) else {
        return Ok(None);
    };
    let Some(workspace_set_id) = state.workspace_set_id_for_monitor(monitor_id) else {
        return Ok(None);
    };
    let Some(workspace_set) = state.workspace_sets.get(&workspace_set_id) else {
        return Ok(None);
    };
    if workspace_set.ordered_workspace_ids.is_empty() {
        return Ok(None);
    }

    let selection_workspace_id = state
        .overview
        .selection
        .or_else(|| state.active_workspace_id_for_monitor(monitor_id));
    let active_workspace_id = state.active_workspace_id_for_monitor(monitor_id);
    let active_workspace_index = active_workspace_id
        .and_then(|workspace_id| {
            workspace_set
                .ordered_workspace_ids
                .iter()
                .position(|candidate| *candidate == workspace_id)
        })
        .unwrap_or(0) as i32;
    let layout = build_preview_stack_layout(monitor.work_area_rect);
    let mut workspaces = Vec::with_capacity(workspace_set.ordered_workspace_ids.len());
    for (index, workspace_id) in workspace_set
        .ordered_workspace_ids
        .iter()
        .copied()
        .enumerate()
    {
        let projection = recompute_workspace(state, workspace_id)?;
        let stack_offset = index as i32 - active_workspace_index;
        let frame_rect = workspace_preview_frame_rect(monitor.work_area_rect, layout, stack_offset);
        if !rects_intersect(frame_rect, monitor.work_area_rect) {
            continue;
        }

        let live_rect = workspace_live_rect(monitor.work_area_rect, stack_offset);
        let overview_geometry =
            workspace_overview_geometry(monitor.work_area_rect, frame_rect, &projection);
        let mut windows = projection
            .window_geometries
            .iter()
            .filter_map(|geometry| {
                let window = state.windows.get(&geometry.window_id)?;
                let hwnd = window.current_hwnd_binding?;
                let overview_rect = workspace_window_overview_rect(overview_geometry, geometry);
                Some(WindowPreviewScene {
                    hwnd,
                    live_rect: translate_rect_between_origins(
                        geometry.rect,
                        monitor.work_area_rect,
                        live_rect,
                    ),
                    overview_rect,
                })
            })
            .collect::<Vec<_>>();
        let canvas_rect = overview_geometry.canvas_rect;
        windows.retain(|window| rects_intersect(window.overview_rect, canvas_rect));

        let selected = selection_workspace_id == Some(workspace_id);
        workspaces.push(WorkspacePreviewScene {
            workspace_id,
            live_rect,
            canvas_rect,
            frame_rect,
            selected,
            windows,
        });
    }

    Ok(Some(OverviewScene {
        monitor_rect: monitor.work_area_rect,
        workspaces,
    }))
}

fn workspace_live_rect(monitor_rect: Rect, stack_offset: i32) -> Rect {
    let monitor_height = monitor_rect.height.min(i32::MAX as u32) as i32;
    Rect::new(
        monitor_rect.x,
        monitor_rect
            .y
            .saturating_add(stack_offset.saturating_mul(monitor_height)),
        monitor_rect.width,
        monitor_rect.height,
    )
}

pub(crate) fn build_preview_stack_layout(monitor_rect: Rect) -> PreviewStackLayout {
    build_preview_stack_layout_with_zoom(monitor_rect, OVERVIEW_DEFAULT_ZOOM)
}

fn build_preview_stack_layout_with_zoom(monitor_rect: Rect, zoom: f64) -> PreviewStackLayout {
    let zoom = zoom.clamp(OVERVIEW_DEFAULT_ZOOM, 1.0);
    let frame_height = ((monitor_rect.height.max(1) as f64) * zoom)
        .round()
        .max(1.0) as u32;
    let frame_width = ((monitor_rect.width.max(1) as f64) * zoom).round().max(1.0) as u32;
    let gap = ((monitor_rect.height.max(1) as f64) * OVERVIEW_WORKSPACE_GAP_RATIO * zoom)
        .round()
        .max(1.0) as i32;
    let monitor_height = monitor_rect.height.min(i32::MAX as u32) as i32;

    PreviewStackLayout {
        frame_width,
        frame_height,
        gap,
        active_frame_y: monitor_rect
            .y
            .saturating_add((monitor_height - frame_height.min(i32::MAX as u32) as i32) / 2),
    }
}

fn workspace_preview_frame_rect(
    monitor_rect: Rect,
    layout: PreviewStackLayout,
    stack_offset: i32,
) -> Rect {
    let step = layout.frame_height.min(i32::MAX as u32) as i32 + layout.gap;
    Rect::new(
        monitor_rect.x.saturating_add(
            (monitor_rect.width.min(i32::MAX as u32) as i32
                - layout.frame_width.min(i32::MAX as u32) as i32)
                / 2,
        ),
        layout
            .active_frame_y
            .saturating_add(stack_offset.saturating_mul(step)),
        layout.frame_width,
        layout.frame_height,
    )
}

pub(crate) fn workspace_open_close_source_rect(
    workspace: &WorkspacePreviewScene,
    _monitor_rect: Rect,
) -> Rect {
    workspace.live_rect
}

fn workspace_overview_geometry(
    monitor_rect: Rect,
    frame_rect: Rect,
    _projection: &WorkspaceLayoutProjection,
) -> WorkspaceOverviewGeometry {
    let viewport_rect = frame_rect;

    WorkspaceOverviewGeometry {
        canvas_rect: workspace_preview_canvas_rect(frame_rect, monitor_rect),
        viewport_rect,
        tiled_window_transform: OverviewRectTransform {
            source_space: monitor_rect,
            target_space: viewport_rect,
        },
        monitor_window_transform: OverviewRectTransform {
            source_space: monitor_rect,
            target_space: viewport_rect,
        },
    }
}

fn workspace_window_overview_rect(
    geometry: WorkspaceOverviewGeometry,
    window: &flowtile_layout_engine::WindowGeometryProjection,
) -> Rect {
    match window.layer {
        WindowLayer::Tiled => geometry.tiled_window_transform.map_rect(window.rect),
        _ => geometry.monitor_window_transform.map_rect(window.rect),
    }
}

pub(crate) fn scale_rect_to_overview(
    source_rect: Rect,
    source_space: Rect,
    target_rect: Rect,
) -> Rect {
    let scale_x = target_rect.width.max(1) as f64 / source_space.width.max(1) as f64;
    let scale_y = target_rect.height.max(1) as f64 / source_space.height.max(1) as f64;
    let relative_x = i64::from(source_rect.x) - i64::from(source_space.x);
    let relative_y = i64::from(source_rect.y) - i64::from(source_space.y);
    let target_x = i64::from(target_rect.x)
        + (relative_x as f64 * scale_x)
            .round()
            .clamp(i64::from(i32::MIN) as f64, i64::from(i32::MAX) as f64) as i64;
    let target_y = i64::from(target_rect.y)
        + (relative_y as f64 * scale_y)
            .round()
            .clamp(i64::from(i32::MIN) as f64, i64::from(i32::MAX) as f64) as i64;

    Rect::new(
        target_x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        target_y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        ((source_rect.width.max(1) as f64) * scale_x)
            .round()
            .max(1.0) as u32,
        ((source_rect.height.max(1) as f64) * scale_y)
            .round()
            .max(1.0) as u32,
    )
}

#[cfg(test)]
pub(crate) fn overview_window_rect(
    source_rect: Rect,
    source_space: Rect,
    target_rect: Rect,
    _layer: WindowLayer,
) -> Rect {
    scale_rect_to_overview(source_rect, source_space, target_rect)
}

fn workspace_preview_canvas_rect(frame_rect: Rect, monitor_rect: Rect) -> Rect {
    Rect::new(
        monitor_rect.x,
        frame_rect.y,
        monitor_rect.width,
        frame_rect.height,
    )
}

fn rects_intersect(left: Rect, right: Rect) -> bool {
    rect_right(left) > right.x
        && rect_right(right) > left.x
        && rect_bottom(left) > right.y
        && rect_bottom(right) > left.y
}

pub(crate) fn intersect_rect(left: Rect, right: Rect) -> Option<Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = rect_right(left).min(rect_right(right));
    let bottom_edge = rect_bottom(left).min(rect_bottom(right));
    if right_edge <= x || bottom_edge <= y {
        return None;
    }

    Some(Rect::new(
        x,
        y,
        (right_edge - x) as u32,
        (bottom_edge - y) as u32,
    ))
}

fn signed_axis_delta(from: i32, to: i32) -> i32 {
    (i64::from(to) - i64::from(from)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn translate_rect_between_origins(rect: Rect, from_origin: Rect, to_origin: Rect) -> Rect {
    translate_rect(
        rect,
        signed_axis_delta(from_origin.x, to_origin.x),
        signed_axis_delta(from_origin.y, to_origin.y),
    )
}

pub(crate) fn rect_relative_to(rect: Rect, origin: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_sub(origin.x),
        rect.y.saturating_sub(origin.y),
        rect.width,
        rect.height,
    )
}

pub(crate) fn rect_right(rect: Rect) -> i32 {
    rect.x
        .saturating_add(rect.width.min(i32::MAX as u32) as i32)
}

pub(crate) fn rect_bottom(rect: Rect) -> i32 {
    rect.y
        .saturating_add(rect.height.min(i32::MAX as u32) as i32)
}

fn workspace_canvas_rect(
    workspace: &WorkspacePreviewScene,
    monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_rect = workspace_open_close_source_rect(workspace, monitor_rect);
    match mode {
        SceneFrameMode::Final => workspace.canvas_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, workspace.canvas_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(workspace.canvas_rect, source_rect, progress_milli)
        }
    }
}

fn workspace_viewport_rect(
    workspace: &WorkspacePreviewScene,
    monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_rect = workspace_open_close_source_rect(workspace, monitor_rect);
    match mode {
        SceneFrameMode::Final => workspace.frame_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, workspace.frame_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(workspace.frame_rect, source_rect, progress_milli)
        }
    }
}

pub(crate) fn window_rect_for_mode(
    window: WindowPreviewScene,
    _workspace: &WorkspacePreviewScene,
    _monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_rect = window.live_rect;
    match mode {
        SceneFrameMode::Final => window.overview_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, window.overview_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(window.overview_rect, source_rect, progress_milli)
        }
    }
}

pub(crate) fn render_frame_for_scene(
    scene: &OverviewScene,
    mode: SceneFrameMode,
) -> OverviewRenderFrame {
    let workspaces = scene
        .workspaces
        .iter()
        .map(|workspace| {
            let canvas_rect = workspace_canvas_rect(workspace, scene.monitor_rect, mode);
            let viewport_rect = workspace_viewport_rect(workspace, scene.monitor_rect, mode);
            let windows = workspace
                .windows
                .iter()
                .map(|window| WindowRenderFrame {
                    hwnd: window.hwnd,
                    rect: window_rect_for_mode(*window, workspace, scene.monitor_rect, mode),
                })
                .collect();
            WorkspaceRenderFrame {
                workspace_id: workspace.workspace_id,
                canvas_rect,
                viewport_rect,
                windows,
            }
        })
        .collect();

    OverviewRenderFrame {
        monitor_rect: scene.monitor_rect,
        workspaces,
    }
}

pub(crate) fn render_frame_for_transition(
    from_scene: &OverviewScene,
    to_scene: &OverviewScene,
    progress_milli: u16,
) -> OverviewRenderFrame {
    let stack_delta = transition_stack_delta(from_scene, to_scene);
    let workspaces = collect_transition_workspace_ids(from_scene, to_scene)
        .into_iter()
        .map(|workspace_id| {
            let from_workspace = from_scene
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id);
            let to_workspace = to_scene
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id);
            let from_canvas = from_workspace
                .map(|workspace| workspace.canvas_rect)
                .or_else(|| {
                    to_workspace.map(|workspace| {
                        translate_rect(workspace.canvas_rect, -stack_delta.0, -stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let to_canvas = to_workspace
                .map(|workspace| workspace.canvas_rect)
                .or_else(|| {
                    from_workspace.map(|workspace| {
                        translate_rect(workspace.canvas_rect, stack_delta.0, stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let from_viewport = from_workspace
                .map(|workspace| workspace.frame_rect)
                .or_else(|| {
                    to_workspace.map(|workspace| {
                        translate_rect(workspace.frame_rect, -stack_delta.0, -stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let to_viewport = to_workspace
                .map(|workspace| workspace.frame_rect)
                .or_else(|| {
                    from_workspace.map(|workspace| {
                        translate_rect(workspace.frame_rect, stack_delta.0, stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");

            WorkspaceRenderFrame {
                workspace_id,
                canvas_rect: interpolate_rect(from_canvas, to_canvas, progress_milli),
                viewport_rect: interpolate_rect(from_viewport, to_viewport, progress_milli),
                windows: render_transition_window_frames(
                    from_workspace,
                    to_workspace,
                    stack_delta,
                    progress_milli,
                ),
            }
        })
        .collect();

    OverviewRenderFrame {
        monitor_rect: interpolate_rect(
            from_scene.monitor_rect,
            to_scene.monitor_rect,
            progress_milli,
        ),
        workspaces,
    }
}

fn transition_stack_delta(from_scene: &OverviewScene, to_scene: &OverviewScene) -> (i32, i32) {
    for to_workspace in &to_scene.workspaces {
        let Some(from_workspace) = from_scene
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == to_workspace.workspace_id)
        else {
            continue;
        };
        return (
            signed_axis_delta(from_workspace.frame_rect.x, to_workspace.frame_rect.x),
            signed_axis_delta(from_workspace.frame_rect.y, to_workspace.frame_rect.y),
        );
    }

    (0, 0)
}

fn collect_transition_workspace_ids(
    from_scene: &OverviewScene,
    to_scene: &OverviewScene,
) -> Vec<WorkspaceId> {
    let mut ids = to_scene
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_id)
        .collect::<Vec<_>>();
    for workspace in &from_scene.workspaces {
        if !ids.contains(&workspace.workspace_id) {
            ids.push(workspace.workspace_id);
        }
    }
    ids
}

fn render_transition_window_frames(
    from_workspace: Option<&WorkspacePreviewScene>,
    to_workspace: Option<&WorkspacePreviewScene>,
    stack_delta: (i32, i32),
    progress_milli: u16,
) -> Vec<WindowRenderFrame> {
    let window_delta =
        transition_window_delta_for_workspace(from_workspace, to_workspace, stack_delta);
    let mut hwnds = to_workspace
        .map(|workspace| {
            workspace
                .windows
                .iter()
                .map(|window| window.hwnd)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(workspace) = from_workspace {
        for window in &workspace.windows {
            if !hwnds.contains(&window.hwnd) {
                hwnds.push(window.hwnd);
            }
        }
    }

    hwnds
        .into_iter()
        .filter_map(|hwnd| {
            let from_rect = from_workspace.and_then(|workspace| {
                workspace
                    .windows
                    .iter()
                    .find(|window| window.hwnd == hwnd)
                    .map(|window| window.overview_rect)
            });
            let to_rect = to_workspace.and_then(|workspace| {
                workspace
                    .windows
                    .iter()
                    .find(|window| window.hwnd == hwnd)
                    .map(|window| window.overview_rect)
            });
            let from_rect = from_rect.or_else(|| {
                to_rect.map(|rect| translate_rect(rect, -window_delta.0, -window_delta.1))
            })?;
            let to_rect = to_rect
                .or_else(|| Some(translate_rect(from_rect, window_delta.0, window_delta.1)))?;
            Some(WindowRenderFrame {
                hwnd,
                rect: interpolate_rect(from_rect, to_rect, progress_milli),
            })
        })
        .collect()
}

fn transition_window_delta_for_workspace(
    from_workspace: Option<&WorkspacePreviewScene>,
    to_workspace: Option<&WorkspacePreviewScene>,
    fallback_delta: (i32, i32),
) -> (i32, i32) {
    let (Some(from_workspace), Some(to_workspace)) = (from_workspace, to_workspace) else {
        return fallback_delta;
    };

    for to_window in &to_workspace.windows {
        let Some(from_window) = from_workspace
            .windows
            .iter()
            .find(|window| window.hwnd == to_window.hwnd)
        else {
            continue;
        };
        return (
            signed_axis_delta(from_window.overview_rect.x, to_window.overview_rect.x),
            signed_axis_delta(from_window.overview_rect.y, to_window.overview_rect.y),
        );
    }

    (
        signed_axis_delta(from_workspace.frame_rect.x, to_workspace.frame_rect.x),
        signed_axis_delta(from_workspace.frame_rect.y, to_workspace.frame_rect.y),
    )
}

fn preview_center_distance(workspace: &WorkspaceRenderFrame, monitor_rect: Rect) -> i64 {
    let workspace_center =
        i64::from(workspace.canvas_rect.y) + i64::from(workspace.canvas_rect.height) / 2;
    let monitor_center = i64::from(monitor_rect.y) + i64::from(monitor_rect.height) / 2;
    (workspace_center - monitor_center).abs()
}

pub(crate) fn ordered_frame_workspaces(frame: &OverviewRenderFrame) -> Vec<&WorkspaceRenderFrame> {
    let mut ordered_workspaces = frame.workspaces.iter().collect::<Vec<_>>();
    ordered_workspaces.sort_by(|left, right| {
        preview_center_distance(*right, frame.monitor_rect)
            .cmp(&preview_center_distance(*left, frame.monitor_rect))
            .then(left.canvas_rect.y.cmp(&right.canvas_rect.y))
    });
    ordered_workspaces
}

pub(crate) fn overview_viewport_column_rect(frame: &OverviewRenderFrame) -> Option<Rect> {
    let viewport_rect = frame
        .workspaces
        .iter()
        .filter_map(|workspace| {
            intersect_rect(workspace.viewport_rect, frame.monitor_rect).map(|viewport_rect| {
                (
                    preview_center_distance(workspace, frame.monitor_rect),
                    viewport_rect,
                )
            })
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, viewport_rect)| viewport_rect)?;

    Some(Rect::new(
        viewport_rect.x,
        frame.monitor_rect.y,
        viewport_rect.width,
        frame.monitor_rect.height,
    ))
}

pub(crate) fn preview_click_targets_for_frame(
    frame: &OverviewRenderFrame,
    visible_scene_rect: Rect,
    visible_hwnds: &HashSet<u64>,
) -> Vec<PreviewClickTarget> {
    let mut targets = Vec::new();
    for workspace in ordered_frame_workspaces(frame) {
        let Some(visible_canvas_rect) = intersect_rect(workspace.canvas_rect, visible_scene_rect)
        else {
            continue;
        };
        targets.extend(preview_click_targets(
            workspace,
            visible_canvas_rect,
            visible_hwnds,
            visible_scene_rect,
        ));
    }
    targets
}

pub(crate) fn preview_click_targets(
    workspace: &WorkspaceRenderFrame,
    visible_canvas_rect: Rect,
    visible_hwnds: &HashSet<u64>,
    origin_rect: Rect,
) -> Vec<PreviewClickTarget> {
    workspace
        .windows
        .iter()
        .filter(|window| visible_hwnds.contains(&window.hwnd))
        .filter_map(|window| {
            let clipped_rect = intersect_rect(window.rect, visible_canvas_rect)?;
            Some(PreviewClickTarget {
                hwnd: window.hwnd,
                workspace_id: workspace.workspace_id,
                rect: rect_relative_to(clipped_rect, origin_rect),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn preview_shell_targets_for_frame(
    frame: &OverviewRenderFrame,
    visible_scene_rect: Rect,
) -> Vec<PreviewClickTarget> {
    let mut targets = Vec::new();
    for workspace in ordered_frame_workspaces(frame) {
        let Some(visible_canvas_rect) = intersect_rect(workspace.canvas_rect, visible_scene_rect)
        else {
            continue;
        };
        targets.extend(preview_shell_targets(
            workspace,
            visible_canvas_rect,
            visible_scene_rect,
        ));
    }
    targets
}

#[cfg(test)]
pub(crate) fn preview_shell_targets(
    workspace: &WorkspaceRenderFrame,
    visible_canvas_rect: Rect,
    origin_rect: Rect,
) -> Vec<PreviewClickTarget> {
    preview_window_rects(workspace, visible_canvas_rect, origin_rect)
}

#[cfg(test)]
pub(crate) fn preview_window_rects(
    workspace: &WorkspaceRenderFrame,
    clip_rect: Rect,
    origin_rect: Rect,
) -> Vec<PreviewClickTarget> {
    workspace
        .windows
        .iter()
        .filter_map(|window| {
            let clipped_rect = intersect_rect(window.rect, clip_rect)?;
            Some(PreviewClickTarget {
                hwnd: window.hwnd,
                workspace_id: workspace.workspace_id,
                rect: rect_relative_to(clipped_rect, origin_rect),
            })
        })
        .collect()
}

pub(crate) fn workspace_drop_targets_for_frame(
    frame: &OverviewRenderFrame,
    visible_scene_rect: Rect,
) -> Vec<WorkspaceDropTarget> {
    frame
        .workspaces
        .iter()
        .filter_map(|workspace| {
            let clipped_rect = intersect_rect(workspace.canvas_rect, visible_scene_rect)?;
            Some(WorkspaceDropTarget {
                workspace_id: workspace.workspace_id,
                rect: rect_relative_to(clipped_rect, visible_scene_rect),
            })
        })
        .collect()
}

pub(crate) fn translate_rect(rect: Rect, delta_x: i32, delta_y: i32) -> Rect {
    Rect::new(
        rect.x.saturating_add(delta_x),
        rect.y.saturating_add(delta_y),
        rect.width,
        rect.height,
    )
}

fn interpolate_rect(from: Rect, to: Rect, progress_milli: u16) -> Rect {
    if progress_milli == 0 {
        return from;
    }
    if progress_milli >= 1000 {
        return to;
    }

    Rect::new(
        interpolate_i32(from.x, to.x, progress_milli),
        interpolate_i32(from.y, to.y, progress_milli),
        interpolate_u32(from.width.max(1), to.width.max(1), progress_milli),
        interpolate_u32(from.height.max(1), to.height.max(1), progress_milli),
    )
}

fn interpolate_i32(from: i32, to: i32, progress_milli: u16) -> i32 {
    let delta = i64::from(to) - i64::from(from);
    let step = (delta * i64::from(progress_milli)) / 1000;
    from.saturating_add(step.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

fn interpolate_u32(from: u32, to: u32, progress_milli: u16) -> u32 {
    let delta = i64::from(to) - i64::from(from);
    let step = (delta * i64::from(progress_milli)) / 1000;
    let value = i64::from(from) + step;
    value.clamp(1, i64::from(u32::MAX)) as u32
}

pub(crate) fn rect_contains_point(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect_right(rect) && y >= rect.y && y < rect_bottom(rect)
}

pub(crate) fn hit_test_preview_targets(
    click_targets: &[PreviewClickTarget],
    x: i32,
    y: i32,
) -> Option<u64> {
    preview_target_at_point(click_targets, x, y).map(|target| target.hwnd)
}

pub(crate) fn preview_target_at_point(
    click_targets: &[PreviewClickTarget],
    x: i32,
    y: i32,
) -> Option<PreviewClickTarget> {
    click_targets
        .iter()
        .rev()
        .find(|target| rect_contains_point(target.rect, x, y))
        .copied()
}

pub(crate) fn hit_test_workspace_drop_targets(
    workspace_targets: &[WorkspaceDropTarget],
    x: i32,
    y: i32,
) -> Option<WorkspaceId> {
    workspace_targets
        .iter()
        .rev()
        .find(|target| rect_contains_point(target.rect, x, y))
        .map(|target| target.workspace_id)
}
