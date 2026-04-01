use std::collections::HashMap;

use flowtile_domain::{MonitorId, Rect, WindowId, WindowLayer, WmState, WorkspaceId};
use flowtile_layout_engine::{WorkspaceLayoutProjection, padded_tiled_viewport};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DesktopProjection {
    pub monitors: HashMap<MonitorId, MonitorDesktopProjection>,
    pub windows: HashMap<WindowId, DesktopWindowProjection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorDesktopProjection {
    pub monitor_id: MonitorId,
    pub work_area: Rect,
    pub padded_viewport: Rect,
    pub active_workspace_id: Option<WorkspaceId>,
    pub workspace_bands: Vec<WorkspaceDesktopBand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceDesktopBand {
    pub workspace_id: WorkspaceId,
    pub monitor_id: MonitorId,
    pub is_active: bool,
    pub local_work_area: Rect,
    pub desktop_band_rect: Rect,
    pub vertical_offset_from_active: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopWindowProjection {
    pub window_id: WindowId,
    pub workspace_id: WorkspaceId,
    pub monitor_id: MonitorId,
    pub layer: WindowLayer,
    pub workspace_local_rect: Rect,
    pub logical_desktop_rect: Rect,
    pub desktop_rect: Rect,
    pub presentation_mode: DesktopWindowPresentationMode,
    pub surrogate_rect: Option<Rect>,
    pub surrogate_source_rect: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DesktopWindowPresentationMode {
    #[default]
    NativeVisible,
    NativeHidden,
    SurrogateClipped,
}

pub(crate) fn build_monitor_local_desktop_projection(
    state: &WmState,
    workspace_layouts: &HashMap<WorkspaceId, WorkspaceLayoutProjection>,
) -> DesktopProjection {
    let mut projection = DesktopProjection::default();
    let desktop_bounds = desktop_horizontal_bounds(state);
    let isolate_horizontal_overflow = state.monitors.len() > 1;

    for (&monitor_id, monitor) in &state.monitors {
        let Some(workspace_set_id) = state.workspace_set_id_for_monitor(monitor_id) else {
            continue;
        };
        let Some(workspace_set) = state.workspace_sets.get(&workspace_set_id) else {
            continue;
        };

        let active_workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .or(Some(workspace_set.active_workspace_id));
        let active_position = active_workspace_id
            .and_then(|workspace_id| {
                workspace_set
                    .ordered_workspace_ids
                    .iter()
                    .position(|candidate| *candidate == workspace_id)
            })
            .unwrap_or(0)
            .min(i32::MAX as usize) as i32;
        let band_height = monitor.work_area_rect.height.min(i32::MAX as u32) as i32;
        let mut workspace_bands = Vec::new();

        for (position, workspace_id) in workspace_set
            .ordered_workspace_ids
            .iter()
            .copied()
            .enumerate()
        {
            let Some(layout) = workspace_layouts.get(&workspace_id) else {
                continue;
            };

            let workspace_position = position.min(i32::MAX as usize) as i32;
            let vertical_offset_from_active = workspace_position.saturating_sub(active_position);
            let desktop_band_rect = Rect::new(
                monitor.work_area_rect.x,
                monitor
                    .work_area_rect
                    .y
                    .saturating_add(vertical_offset_from_active.saturating_mul(band_height)),
                monitor.work_area_rect.width,
                monitor.work_area_rect.height,
            );

            workspace_bands.push(WorkspaceDesktopBand {
                workspace_id,
                monitor_id,
                is_active: Some(workspace_id) == active_workspace_id,
                local_work_area: monitor.work_area_rect,
                desktop_band_rect,
                vertical_offset_from_active,
            });

            for geometry in &layout.window_geometries {
                let logical_desktop_rect = translate_rect_to_band(
                    geometry.rect,
                    monitor.work_area_rect,
                    desktop_band_rect,
                );
                let isolated_desktop_rect = materialize_monitor_local_window_rect(
                    geometry.rect,
                    monitor.work_area_rect,
                    desktop_band_rect,
                    monitor.work_area_rect,
                    desktop_bounds,
                    isolate_horizontal_overflow,
                );
                let visible_rect_on_owning_monitor =
                    intersect_rect(logical_desktop_rect, monitor.work_area_rect);
                let is_active_window = state.focus.focused_window_id == Some(geometry.window_id);
                let presentation_mode = determine_desktop_window_presentation_mode(
                    geometry.layer,
                    is_active_window,
                    isolate_horizontal_overflow,
                    logical_desktop_rect,
                    visible_rect_on_owning_monitor,
                );
                let (desktop_rect, surrogate_rect, surrogate_source_rect) = match presentation_mode
                {
                    DesktopWindowPresentationMode::NativeVisible => {
                        (logical_desktop_rect, None, None)
                    }
                    DesktopWindowPresentationMode::NativeHidden => {
                        (isolated_desktop_rect, None, None)
                    }
                    DesktopWindowPresentationMode::SurrogateClipped => {
                        let surrogate_rect = visible_rect_on_owning_monitor.expect(
                            "surrogate-clipped presentation requires monitor-local visible rect",
                        );
                        (
                            isolated_desktop_rect,
                            Some(surrogate_rect),
                            Some(source_rect_within_logical_desktop_rect(
                                logical_desktop_rect,
                                surrogate_rect,
                            )),
                        )
                    }
                };
                projection.windows.insert(
                    geometry.window_id,
                    DesktopWindowProjection {
                        window_id: geometry.window_id,
                        workspace_id,
                        monitor_id,
                        layer: geometry.layer,
                        workspace_local_rect: geometry.rect,
                        logical_desktop_rect,
                        desktop_rect,
                        presentation_mode,
                        surrogate_rect,
                        surrogate_source_rect,
                    },
                );
            }
        }

        projection.monitors.insert(
            monitor_id,
            MonitorDesktopProjection {
                monitor_id,
                work_area: monitor.work_area_rect,
                padded_viewport: padded_tiled_viewport(
                    monitor.work_area_rect,
                    &state.config_projection,
                ),
                active_workspace_id,
                workspace_bands,
            },
        );
    }

    projection
}

fn materialize_monitor_local_window_rect(
    rect: Rect,
    source_work_area: Rect,
    target_band_rect: Rect,
    monitor_work_area: Rect,
    desktop_bounds: DesktopHorizontalBounds,
    isolate_horizontal_overflow: bool,
) -> Rect {
    let translated = translate_rect_to_band(rect, source_work_area, target_band_rect);
    if !isolate_horizontal_overflow {
        return translated;
    }

    isolate_rect_from_foreign_monitor_work_areas(translated, monitor_work_area, desktop_bounds)
}

fn determine_desktop_window_presentation_mode(
    layer: WindowLayer,
    is_active_window: bool,
    isolate_horizontal_overflow: bool,
    logical_desktop_rect: Rect,
    visible_rect_on_owning_monitor: Option<Rect>,
) -> DesktopWindowPresentationMode {
    if !isolate_horizontal_overflow || layer != WindowLayer::Tiled || is_active_window {
        return DesktopWindowPresentationMode::NativeVisible;
    }

    match visible_rect_on_owning_monitor {
        Some(visible_rect) if visible_rect == logical_desktop_rect => {
            DesktopWindowPresentationMode::NativeVisible
        }
        Some(_) => DesktopWindowPresentationMode::SurrogateClipped,
        None => DesktopWindowPresentationMode::NativeHidden,
    }
}

fn translate_rect_to_band(rect: Rect, source_work_area: Rect, target_band_rect: Rect) -> Rect {
    let relative_x = rect.x.saturating_sub(source_work_area.x);
    let relative_y = rect.y.saturating_sub(source_work_area.y);

    Rect::new(
        target_band_rect.x.saturating_add(relative_x),
        target_band_rect.y.saturating_add(relative_y),
        rect.width,
        rect.height,
    )
}

fn isolate_rect_from_foreign_monitor_work_areas(
    rect: Rect,
    monitor_work_area: Rect,
    desktop_bounds: DesktopHorizontalBounds,
) -> Rect {
    let monitor_left = monitor_work_area.x;
    let monitor_right = rect_right(monitor_work_area);
    let rect_right = rect_right(rect);
    let rect_width = rect.width.min(i32::MAX as u32) as i32;

    if rect.x >= monitor_left && rect_right <= monitor_right {
        return rect;
    }

    if rect.x < monitor_left {
        let overflow = monitor_left.saturating_sub(rect.x);
        return Rect::new(
            desktop_bounds
                .left
                .saturating_sub(overflow)
                .saturating_sub(rect_width),
            rect.y,
            rect.width,
            rect.height,
        );
    }

    let overflow = rect_right.saturating_sub(monitor_right);
    Rect::new(
        desktop_bounds.right.saturating_add(overflow),
        rect.y,
        rect.width,
        rect.height,
    )
}

fn intersect_rect(left: Rect, right: Rect) -> Option<Rect> {
    let intersection_left = left.x.max(right.x);
    let intersection_top = left.y.max(right.y);
    let intersection_right = rect_right(left).min(rect_right(right));
    let intersection_bottom = rect_bottom(left).min(rect_bottom(right));
    let intersection_width = intersection_right.saturating_sub(intersection_left);
    let intersection_height = intersection_bottom.saturating_sub(intersection_top);

    if intersection_width <= 0 || intersection_height <= 0 {
        return None;
    }

    Some(Rect::new(
        intersection_left,
        intersection_top,
        intersection_width as u32,
        intersection_height as u32,
    ))
}

fn source_rect_within_logical_desktop_rect(logical_rect: Rect, visible_rect: Rect) -> Rect {
    Rect::new(
        visible_rect.x.saturating_sub(logical_rect.x),
        visible_rect.y.saturating_sub(logical_rect.y),
        visible_rect.width,
        visible_rect.height,
    )
}

fn desktop_horizontal_bounds(state: &WmState) -> DesktopHorizontalBounds {
    let left = state
        .monitors
        .values()
        .map(|monitor| monitor.work_area_rect.x)
        .min()
        .unwrap_or(0);
    let right = state
        .monitors
        .values()
        .map(|monitor| rect_right(monitor.work_area_rect))
        .max()
        .unwrap_or(0);

    DesktopHorizontalBounds { left, right }
}

fn rect_right(rect: Rect) -> i32 {
    rect.x
        .saturating_add(rect.width.min(i32::MAX as u32) as i32)
}

fn rect_bottom(rect: Rect) -> i32 {
    rect.y
        .saturating_add(rect.height.min(i32::MAX as u32) as i32)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DesktopHorizontalBounds {
    left: i32,
    right: i32,
}

#[cfg(test)]
mod tests {
    use super::{
        DesktopHorizontalBounds, DesktopWindowPresentationMode,
        determine_desktop_window_presentation_mode, intersect_rect,
        isolate_rect_from_foreign_monitor_work_areas, rect_right,
        source_rect_within_logical_desktop_rect, translate_rect_to_band,
    };
    use flowtile_domain::Rect;

    #[test]
    fn translates_window_rect_relative_to_work_area_into_target_band() {
        let source_work_area = Rect::new(1200, 0, 1600, 900);
        let target_band = Rect::new(1200, -900, 1600, 900);
        let local_window_rect = Rect::new(1216, 16, 640, 868);

        assert_eq!(
            translate_rect_to_band(local_window_rect, source_work_area, target_band),
            Rect::new(1216, -884, 640, 868)
        );
    }

    #[test]
    fn right_overflow_is_parked_beyond_desktop_right_edge() {
        let monitor_work_area = Rect::new(0, 0, 1600, 900);
        let overflowing_rect = Rect::new(928, 16, 900, 868);

        let parked = isolate_rect_from_foreign_monitor_work_areas(
            overflowing_rect,
            monitor_work_area,
            DesktopHorizontalBounds {
                left: 0,
                right: 3040,
            },
        );

        assert!(parked.x >= 3040);
        assert!(rect_right(parked) > 3040);
    }

    #[test]
    fn left_overflow_is_parked_before_desktop_left_edge() {
        let monitor_work_area = Rect::new(1600, 0, 1440, 1200);
        let overflowing_rect = Rect::new(1200, 24, 900, 1168);

        let parked = isolate_rect_from_foreign_monitor_work_areas(
            overflowing_rect,
            monitor_work_area,
            DesktopHorizontalBounds {
                left: 0,
                right: 3040,
            },
        );

        assert!(rect_right(parked) <= 0);
        assert!(parked.x < 0);
    }

    #[test]
    fn partial_monitor_visibility_yields_surrogate_clipped_presentation_for_inactive_tiled_window()
    {
        let logical_rect = Rect::new(928, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation_mode(
                flowtile_domain::WindowLayer::Tiled,
                false,
                true,
                logical_rect,
                visible_rect,
            ),
            DesktopWindowPresentationMode::SurrogateClipped
        );
    }

    #[test]
    fn active_tiled_window_stays_native_even_when_it_spills() {
        let logical_rect = Rect::new(928, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation_mode(
                flowtile_domain::WindowLayer::Tiled,
                true,
                true,
                logical_rect,
                visible_rect,
            ),
            DesktopWindowPresentationMode::NativeVisible
        );
    }

    #[test]
    fn surrogate_source_rect_is_relative_to_logical_desktop_rect() {
        let logical_rect = Rect::new(928, 16, 900, 868);
        let visible_rect = Rect::new(928, 16, 672, 868);

        assert_eq!(
            source_rect_within_logical_desktop_rect(logical_rect, visible_rect),
            Rect::new(0, 0, 672, 868)
        );
    }
}
