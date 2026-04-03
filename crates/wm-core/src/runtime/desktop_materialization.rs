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
    pub owning_monitor_visible_rect: Option<Rect>,
    pub presentation_mode: DesktopWindowPresentationMode,
    pub presentation_reason: DesktopWindowPresentationReason,
    pub surrogate_rect: Option<Rect>,
    pub surrogate_source_rect: Option<Rect>,
    pub monitor_slices: Vec<DesktopWindowMonitorSlice>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DesktopWindowMonitorSliceKind {
    #[default]
    ForeignMonitorSurrogate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopWindowMonitorSlice {
    pub kind: DesktopWindowMonitorSliceKind,
    pub monitor_work_area: Rect,
    pub destination_rect: Rect,
    pub source_rect: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DesktopWindowPresentationMode {
    #[default]
    NativeVisible,
    NativeHidden,
    #[allow(dead_code)]
    SurrogateVisible,
    SurrogateClipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DesktopWindowPresentationReason {
    ActiveWindowNative,
    NonTiledLayerNative,
    InactiveFullyVisibleSurrogate,
    InactiveClippedSurrogate,
    InactiveOutsideMonitorHidden,
}

impl DesktopWindowPresentationMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NativeVisible => "native-visible",
            Self::NativeHidden => "native-hidden",
            Self::SurrogateVisible => "surrogate-visible",
            Self::SurrogateClipped => "surrogate-clipped",
        }
    }
}

impl DesktopWindowPresentationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveWindowNative => "active-window-native",
            Self::NonTiledLayerNative => "non-tiled-layer-native",
            Self::InactiveFullyVisibleSurrogate => "inactive-fully-visible-surrogate",
            Self::InactiveClippedSurrogate => "inactive-clipped-surrogate",
            Self::InactiveOutsideMonitorHidden => "inactive-outside-monitor-hidden",
        }
    }
}

pub(crate) fn build_monitor_local_desktop_projection(
    state: &WmState,
    workspace_layouts: &HashMap<WorkspaceId, WorkspaceLayoutProjection>,
) -> DesktopProjection {
    let mut projection = DesktopProjection::default();
    let desktop_bounds = desktop_horizontal_bounds(state);

    for (&monitor_id, monitor) in &state.monitors {
        if !state
            .config_projection
            .manages_monitor_binding(monitor.platform_binding.as_deref())
        {
            continue;
        }

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
                let monitor_slices = build_foreign_monitor_slices(
                    logical_desktop_rect,
                    monitor_id,
                    state
                        .monitors
                        .iter()
                        .map(|(&candidate_monitor_id, candidate_monitor)| {
                            (candidate_monitor_id, candidate_monitor.work_area_rect)
                        }),
                );
                let visible_rect_on_owning_monitor =
                    intersect_rect(logical_desktop_rect, monitor.work_area_rect);
                let is_active_window = state.focus.focused_window_id == Some(geometry.window_id);
                let (presentation_mode, presentation_reason) =
                    determine_desktop_window_presentation(
                        geometry.layer,
                        is_active_window,
                        logical_desktop_rect,
                        visible_rect_on_owning_monitor,
                    );
                let parked_desktop_rect =
                    park_rect_outside_desktop(logical_desktop_rect, desktop_bounds);
                let (desktop_rect, surrogate_rect, surrogate_source_rect) = match presentation_mode
                {
                    DesktopWindowPresentationMode::NativeVisible => {
                        (logical_desktop_rect, None, None)
                    }
                    DesktopWindowPresentationMode::NativeHidden => {
                        (parked_desktop_rect, None, None)
                    }
                    DesktopWindowPresentationMode::SurrogateVisible => (
                        parked_desktop_rect,
                        Some(logical_desktop_rect),
                        Some(full_source_rect_for_logical_desktop_rect(
                            logical_desktop_rect,
                        )),
                    ),
                    DesktopWindowPresentationMode::SurrogateClipped => {
                        let surrogate_rect = visible_rect_on_owning_monitor.expect(
                            "surrogate-clipped presentation requires monitor-local visible rect",
                        );
                        (
                            parked_desktop_rect,
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
                        owning_monitor_visible_rect: visible_rect_on_owning_monitor,
                        presentation_mode,
                        presentation_reason,
                        surrogate_rect,
                        surrogate_source_rect,
                        monitor_slices,
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

fn determine_desktop_window_presentation(
    layer: WindowLayer,
    is_active_window: bool,
    logical_desktop_rect: Rect,
    visible_rect_on_owning_monitor: Option<Rect>,
) -> (
    DesktopWindowPresentationMode,
    DesktopWindowPresentationReason,
) {
    if layer != WindowLayer::Tiled {
        return (
            DesktopWindowPresentationMode::NativeVisible,
            DesktopWindowPresentationReason::NonTiledLayerNative,
        );
    }

    match visible_rect_on_owning_monitor {
        Some(visible_rect) if visible_rect == logical_desktop_rect => {
            if is_active_window {
                (
                    DesktopWindowPresentationMode::NativeVisible,
                    DesktopWindowPresentationReason::ActiveWindowNative,
                )
            } else {
                (
                    DesktopWindowPresentationMode::SurrogateVisible,
                    DesktopWindowPresentationReason::InactiveFullyVisibleSurrogate,
                )
            }
        }
        Some(_) => {
            if is_active_window {
                (
                    DesktopWindowPresentationMode::NativeVisible,
                    DesktopWindowPresentationReason::ActiveWindowNative,
                )
            } else {
                (
                    DesktopWindowPresentationMode::SurrogateClipped,
                    DesktopWindowPresentationReason::InactiveClippedSurrogate,
                )
            }
        }
        None => (
            DesktopWindowPresentationMode::NativeHidden,
            DesktopWindowPresentationReason::InactiveOutsideMonitorHidden,
        ),
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

fn park_rect_outside_desktop(rect: Rect, desktop_bounds: DesktopHorizontalBounds) -> Rect {
    Rect::new(
        desktop_bounds.right.saturating_add(1),
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

fn build_foreign_monitor_slices<I>(
    logical_desktop_rect: Rect,
    owning_monitor_id: MonitorId,
    monitor_work_areas: I,
) -> Vec<DesktopWindowMonitorSlice>
where
    I: IntoIterator<Item = (MonitorId, Rect)>,
{
    monitor_work_areas
        .into_iter()
        .filter(|(monitor_id, _)| *monitor_id != owning_monitor_id)
        .filter_map(|(_, monitor_work_area)| {
            let destination_rect = intersect_rect(logical_desktop_rect, monitor_work_area)?;
            Some(DesktopWindowMonitorSlice {
                kind: DesktopWindowMonitorSliceKind::ForeignMonitorSurrogate,
                monitor_work_area,
                source_rect: source_rect_within_logical_desktop_rect(
                    logical_desktop_rect,
                    destination_rect,
                ),
                destination_rect,
            })
        })
        .collect()
}

fn full_source_rect_for_logical_desktop_rect(logical_rect: Rect) -> Rect {
    Rect::new(0, 0, logical_rect.width, logical_rect.height)
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
        DesktopHorizontalBounds, DesktopWindowMonitorSlice, DesktopWindowMonitorSliceKind,
        DesktopWindowPresentationMode, DesktopWindowPresentationReason,
        build_foreign_monitor_slices, determine_desktop_window_presentation,
        full_source_rect_for_logical_desktop_rect, intersect_rect, park_rect_outside_desktop,
        rect_right, source_rect_within_logical_desktop_rect, translate_rect_to_band,
    };
    use flowtile_domain::{MonitorId, Rect};

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
    fn parked_rect_is_positioned_beyond_desktop_right_edge() {
        let logical_rect = Rect::new(928, 16, 900, 868);

        let parked = park_rect_outside_desktop(
            logical_rect,
            DesktopHorizontalBounds {
                left: 0,
                right: 3040,
            },
        );

        assert!(parked.x > 3040);
        assert!(rect_right(parked) > 3040);
    }

    #[test]
    fn fully_visible_inactive_tiled_window_uses_surrogate_visible_presentation() {
        let logical_rect = Rect::new(16, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation(
                flowtile_domain::WindowLayer::Tiled,
                false,
                logical_rect,
                visible_rect,
            ),
            (
                DesktopWindowPresentationMode::SurrogateVisible,
                DesktopWindowPresentationReason::InactiveFullyVisibleSurrogate
            )
        );
    }

    #[test]
    fn partial_monitor_visibility_yields_surrogate_clipped_presentation_for_inactive_tiled_window()
    {
        let logical_rect = Rect::new(928, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation(
                flowtile_domain::WindowLayer::Tiled,
                false,
                logical_rect,
                visible_rect,
            ),
            (
                DesktopWindowPresentationMode::SurrogateClipped,
                DesktopWindowPresentationReason::InactiveClippedSurrogate
            )
        );
    }

    #[test]
    fn active_tiled_window_stays_native_when_it_spills() {
        let logical_rect = Rect::new(928, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation(
                flowtile_domain::WindowLayer::Tiled,
                true,
                logical_rect,
                visible_rect,
            ),
            (
                DesktopWindowPresentationMode::NativeVisible,
                DesktopWindowPresentationReason::ActiveWindowNative
            )
        );
    }

    #[test]
    fn non_tiled_window_stays_native_with_explicit_reason() {
        let logical_rect = Rect::new(16, 16, 900, 868);
        let work_area = Rect::new(0, 0, 1600, 900);
        let visible_rect = intersect_rect(logical_rect, work_area);

        assert_eq!(
            determine_desktop_window_presentation(
                flowtile_domain::WindowLayer::Floating,
                false,
                logical_rect,
                visible_rect,
            ),
            (
                DesktopWindowPresentationMode::NativeVisible,
                DesktopWindowPresentationReason::NonTiledLayerNative
            )
        );
    }

    #[test]
    fn surrogate_visible_uses_full_source_rect() {
        let logical_rect = Rect::new(16, 16, 900, 868);

        assert_eq!(
            full_source_rect_for_logical_desktop_rect(logical_rect),
            Rect::new(0, 0, 900, 868)
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

    #[test]
    fn foreign_monitor_slices_cover_each_intersecting_non_home_monitor() {
        let logical_rect = Rect::new(1200, 100, 2600, 700);

        let slices = build_foreign_monitor_slices(
            logical_rect,
            MonitorId::new(1),
            vec![
                (MonitorId::new(1), Rect::new(0, 0, 1600, 900)),
                (MonitorId::new(2), Rect::new(1600, 0, 1600, 900)),
                (MonitorId::new(3), Rect::new(3200, 0, 1600, 900)),
            ],
        );

        assert_eq!(
            slices,
            vec![
                DesktopWindowMonitorSlice {
                    kind: DesktopWindowMonitorSliceKind::ForeignMonitorSurrogate,
                    monitor_work_area: Rect::new(1600, 0, 1600, 900),
                    destination_rect: Rect::new(1600, 100, 1600, 700),
                    source_rect: Rect::new(400, 0, 1600, 700),
                },
                DesktopWindowMonitorSlice {
                    kind: DesktopWindowMonitorSliceKind::ForeignMonitorSurrogate,
                    monitor_work_area: Rect::new(3200, 0, 1600, 900),
                    destination_rect: Rect::new(3200, 100, 600, 700),
                    source_rect: Rect::new(2000, 0, 600, 700),
                },
            ]
        );
    }
}
