#![forbid(unsafe_code)]

use flowtile_domain::{
    Column, ColumnId, ColumnMode, ConfigProjection, MaximizedState, Rect, Size, WindowId,
    WindowLayer, WmState, WorkspaceId, all_column_modes,
};

pub fn bootstrap_modes() -> [ColumnMode; 4] {
    all_column_modes()
}

pub const fn preserves_insert_invariant() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    WorkspaceMissing(WorkspaceId),
    MonitorMissing,
    ColumnMissing,
    WindowMissing(WindowId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowGeometryProjection {
    pub window_id: WindowId,
    pub rect: Rect,
    pub layer: WindowLayer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLayoutProjection {
    pub workspace_id: WorkspaceId,
    pub viewport: Rect,
    pub scroll_offset: i32,
    pub content_width: u32,
    pub focused_window_id: Option<WindowId>,
    pub window_geometries: Vec<WindowGeometryProjection>,
}

pub fn padded_tiled_viewport(work_area: Rect, config: &ConfigProjection) -> Rect {
    inset_rect(work_area, config.layout_spacing.outer_padding)
}

pub fn default_floating_rect(
    work_area: Rect,
    desired_size: Size,
    config: &ConfigProjection,
) -> Rect {
    let available = inset_uniform_rect(work_area, config.layout_spacing.floating_margin);
    Rect::new(
        available.x,
        available.y,
        desired_size.width.max(1).min(available.width),
        desired_size.height.max(1).min(available.height),
    )
}

pub fn recompute_workspace(
    state: &WmState,
    workspace_id: WorkspaceId,
) -> Result<WorkspaceLayoutProjection, LayoutError> {
    let workspace = state
        .workspaces
        .get(&workspace_id)
        .ok_or(LayoutError::WorkspaceMissing(workspace_id))?;
    let monitor = state
        .monitors
        .get(&workspace.monitor_id)
        .ok_or(LayoutError::MonitorMissing)?;
    let viewport = padded_tiled_viewport(monitor.work_area_rect, &state.config_projection);
    let spacing = state.config_projection.layout_spacing;
    let mut content_width = 0_u32;
    for (index, column_id) in workspace.strip.ordered_column_ids.iter().enumerate() {
        let column = state
            .layout
            .columns
            .get(column_id)
            .ok_or(LayoutError::ColumnMissing)?;
        content_width = content_width
            .saturating_add(resolve_width(column, viewport.width))
            .saturating_add(if index == 0 { 0 } else { spacing.column_gap });
    }
    let mut x_cursor = strip_origin_x(
        viewport.x,
        viewport.width,
        content_width,
        &workspace.strip.ordered_column_ids,
        state.focus.focused_column_id,
        workspace.strip.scroll_offset,
    );
    let mut window_geometries = Vec::new();

    let last_column_index = workspace.strip.ordered_column_ids.len().saturating_sub(1);
    for (column_index, column_id) in workspace.strip.ordered_column_ids.iter().enumerate() {
        let column = state
            .layout
            .columns
            .get(column_id)
            .ok_or(LayoutError::ColumnMissing)?;
        let column_width = resolve_width(column, viewport.width);

        match column.mode {
            ColumnMode::Tabbed => {
                if let Some(window_id) = column
                    .tab_selection
                    .or_else(|| column.ordered_window_ids.first().copied())
                {
                    let window = state
                        .windows
                        .get(&window_id)
                        .ok_or(LayoutError::WindowMissing(window_id))?;
                    window_geometries.push(WindowGeometryProjection {
                        window_id,
                        rect: Rect::new(x_cursor, viewport.y, column_width, viewport.height),
                        layer: window.layer,
                    });
                }
            }
            _ => {
                let window_count = column.ordered_window_ids.len();
                if window_count > 0 {
                    let desired_height_total = column
                        .ordered_window_ids
                        .iter()
                        .filter_map(|window_id| state.windows.get(window_id))
                        .map(|window| window.desired_size.height.max(1))
                        .sum::<u32>()
                        .max(1);
                    let total_window_gap = gap_total(window_count, spacing.window_gap);
                    let tiled_height = viewport.height.saturating_sub(total_window_gap).max(1);

                    let mut y_cursor = viewport.y;
                    let mut remaining_height = tiled_height;
                    let last_index = window_count.saturating_sub(1);

                    for (index, window_id) in column.ordered_window_ids.iter().copied().enumerate()
                    {
                        let window = state
                            .windows
                            .get(&window_id)
                            .ok_or(LayoutError::WindowMissing(window_id))?;
                        let height = if index == last_index {
                            remaining_height.max(1)
                        } else {
                            ((tiled_height as u64 * window.desired_size.height.max(1) as u64)
                                / desired_height_total as u64) as u32
                        }
                        .max(1);

                        window_geometries.push(WindowGeometryProjection {
                            window_id,
                            rect: Rect::new(x_cursor, y_cursor, column_width, height),
                            layer: window.layer,
                        });

                        y_cursor += height as i32;
                        if index != last_index {
                            y_cursor += spacing.window_gap.min(i32::MAX as u32) as i32;
                        }
                        remaining_height = remaining_height.saturating_sub(height);
                    }
                }
            }
        }

        x_cursor += column_width as i32;
        if column_index != last_column_index {
            x_cursor += spacing.column_gap.min(i32::MAX as u32) as i32;
        }
    }

    for window_id in &workspace.floating_layer.ordered_window_ids {
        let window = state
            .windows
            .get(window_id)
            .ok_or(LayoutError::WindowMissing(*window_id))?;
        let rect = if window.layer == WindowLayer::Fullscreen || window.is_fullscreen {
            monitor.work_area_rect
        } else if window.last_known_rect.width > 0 && window.last_known_rect.height > 0 {
            window.last_known_rect
        } else {
            default_floating_rect(
                monitor.work_area_rect,
                window.desired_size,
                &state.config_projection,
            )
        };

        window_geometries.push(WindowGeometryProjection {
            window_id: *window_id,
            rect,
            layer: window.layer,
        });
    }

    let focused_window_id = state.focus.focused_window_id.filter(|window_id| {
        state
            .windows
            .get(window_id)
            .is_some_and(|window| window.workspace_id == workspace_id)
    });

    Ok(WorkspaceLayoutProjection {
        workspace_id,
        viewport,
        scroll_offset: workspace.strip.scroll_offset,
        content_width,
        focused_window_id,
        window_geometries,
    })
}

fn resolve_width(column: &Column, tiled_viewport_width: u32) -> u32 {
    if column.maximized_state == MaximizedState::Maximized
        || column.mode == ColumnMode::MaximizedColumn
    {
        tiled_viewport_width.max(1)
    } else {
        column.width_semantics.resolve(tiled_viewport_width)
    }
}

fn strip_origin_x(
    viewport_x: i32,
    viewport_width: u32,
    content_width: u32,
    ordered_column_ids: &[ColumnId],
    focused_column_id: Option<ColumnId>,
    scroll_offset: i32,
) -> i32 {
    if content_width >= viewport_width || ordered_column_ids.is_empty() {
        return viewport_x.saturating_sub(scroll_offset);
    }

    let slack = (viewport_width - content_width).min(i32::MAX as u32) as i32;
    if ordered_column_ids.len() == 1 {
        return viewport_x.saturating_add(slack / 2);
    }

    if focused_column_id == ordered_column_ids.last().copied() {
        viewport_x.saturating_add(slack)
    } else {
        viewport_x.saturating_sub(scroll_offset)
    }
}

fn gap_total(item_count: usize, gap: u32) -> u32 {
    item_count
        .saturating_sub(1)
        .try_into()
        .ok()
        .and_then(|count: u32| count.checked_mul(gap))
        .unwrap_or(u32::MAX)
}

fn inset_uniform_rect(rect: Rect, margin: u32) -> Rect {
    inset_rect(rect, flowtile_domain::EdgeInsets::all(margin))
}

fn inset_rect(rect: Rect, insets: flowtile_domain::EdgeInsets) -> Rect {
    let clamped_left = insets.left.min(rect.width.saturating_sub(1));
    let remaining_width = rect.width.saturating_sub(clamped_left);
    let clamped_right = insets.right.min(remaining_width.saturating_sub(1));
    let clamped_top = insets.top.min(rect.height.saturating_sub(1));
    let remaining_height = rect.height.saturating_sub(clamped_top);
    let clamped_bottom = insets.bottom.min(remaining_height.saturating_sub(1));

    Rect::new(
        rect.x
            .saturating_add(clamped_left.min(i32::MAX as u32) as i32),
        rect.y
            .saturating_add(clamped_top.min(i32::MAX as u32) as i32),
        rect.width
            .saturating_sub(clamped_left)
            .saturating_sub(clamped_right)
            .max(1),
        rect.height
            .saturating_sub(clamped_top)
            .saturating_sub(clamped_bottom)
            .max(1),
    )
}

#[cfg(test)]
mod tests {
    use flowtile_domain::{Column, Rect, RuntimeMode, Size, WidthSemantics, WmState};

    use super::{bootstrap_modes, preserves_insert_invariant, recompute_workspace};

    #[test]
    fn exposes_all_bootstrap_modes() {
        let modes = bootstrap_modes();
        assert_eq!(
            modes,
            [
                flowtile_domain::ColumnMode::Normal,
                flowtile_domain::ColumnMode::Tabbed,
                flowtile_domain::ColumnMode::MaximizedColumn,
                flowtile_domain::ColumnMode::CustomWidth,
            ]
        );
    }

    #[test]
    fn keeps_insert_invariant_visible_in_bootstrap() {
        assert!(preserves_insert_invariant());
    }

    #[test]
    fn floating_windows_do_not_follow_strip_scroll() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let tiled_window_id = state.allocate_window_id();
        let floating_window_id = state.allocate_window_id();
        let column_id = state.allocate_column_id();

        state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(1400),
                vec![tiled_window_id],
            ),
        );
        let workspace = state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist");
        workspace.strip.ordered_column_ids.push(column_id);
        workspace.strip.scroll_offset = 200;
        workspace
            .floating_layer
            .ordered_window_ids
            .push(floating_window_id);

        state.windows.insert(
            tiled_window_id,
            flowtile_domain::WindowNode {
                id: tiled_window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 1400, 800),
                desired_size: Size::new(1400, 800),
            },
        );
        state.windows.insert(
            floating_window_id,
            flowtile_domain::WindowNode {
                id: floating_window_id,
                current_hwnd_binding: Some(11),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Floating,
                workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: true,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(300, 120, 500, 320),
                desired_size: Size::new(500, 320),
            },
        );

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let tiled = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == tiled_window_id)
            .expect("tiled geometry should exist");
        let floating = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == floating_window_id)
            .expect("floating geometry should exist");

        assert_eq!(tiled.rect.x, -184);
        assert_eq!(floating.rect.x, 300);
        assert_eq!(floating.rect.y, 120);
    }

    #[test]
    fn maximized_column_uses_padded_viewport_width() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_id = state.allocate_window_id();
        let column_id = state.allocate_column_id();
        let column = Column::new(
            column_id,
            flowtile_domain::ColumnMode::Normal,
            WidthSemantics::Fixed(400),
            vec![window_id],
        );
        state.layout.columns.insert(
            column_id,
            flowtile_domain::Column {
                maximized_state: flowtile_domain::MaximizedState::Maximized,
                ..column
            },
        );
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .ordered_column_ids
            .push(column_id);
        state.windows.insert(
            window_id,
            flowtile_domain::WindowNode {
                id: window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 400, 800),
                desired_size: Size::new(400, 800),
            },
        );

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .expect("geometry should exist");

        assert_eq!(geometry.rect.width, 1168);
    }

    #[test]
    fn tiled_columns_respect_configured_column_gap_projection() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_ids = [
            state.allocate_window_id(),
            state.allocate_window_id(),
            state.allocate_window_id(),
        ];
        let column_ids = [
            state.allocate_column_id(),
            state.allocate_column_id(),
            state.allocate_column_id(),
        ];

        for ((window_id, column_id), width) in window_ids
            .iter()
            .copied()
            .zip(column_ids.iter().copied())
            .zip([200_u32, 350, 400])
        {
            state.layout.columns.insert(
                column_id,
                Column::new(
                    column_id,
                    flowtile_domain::ColumnMode::Normal,
                    WidthSemantics::Fixed(width),
                    vec![window_id],
                ),
            );
            state.windows.insert(
                window_id,
                flowtile_domain::WindowNode {
                    id: window_id,
                    current_hwnd_binding: Some(window_id.get()),
                    classification: flowtile_domain::WindowClassification::Application,
                    layer: flowtile_domain::WindowLayer::Tiled,
                    workspace_id,
                    column_id: Some(column_id),
                    is_managed: true,
                    is_floating: false,
                    is_fullscreen: false,
                    restore_target: None,
                    last_known_rect: Rect::new(0, 0, width, 800),
                    desired_size: Size::new(width, 800),
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
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .scroll_offset = 75;

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let mut geometries = projection
            .window_geometries
            .iter()
            .filter(|geometry| geometry.layer == flowtile_domain::WindowLayer::Tiled)
            .collect::<Vec<_>>();
        geometries.sort_by_key(|geometry| geometry.rect.x);

        assert_eq!(geometries.len(), 3);
        assert_eq!(
            geometries[1].rect.x,
            geometries[0].rect.x + geometries[0].rect.width as i32 + 12
        );
        assert_eq!(
            geometries[2].rect.x,
            geometries[1].rect.x + geometries[1].rect.width as i32 + 12
        );
    }

    #[test]
    fn single_narrow_column_is_centered_in_viewport() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_id = state.allocate_window_id();
        let column_id = state.allocate_column_id();

        state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(400),
                vec![window_id],
            ),
        );
        state.windows.insert(
            window_id,
            flowtile_domain::WindowNode {
                id: window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 400, 800),
                desired_size: Size::new(400, 800),
            },
        );
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .ordered_column_ids
            .push(column_id);

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .expect("window geometry should exist");

        assert_eq!(geometry.rect.x, 400);
        assert_eq!(geometry.rect.width, 400);
    }

    #[test]
    fn narrow_multi_column_strip_pins_last_column_to_right_edge() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let first_window_id = state.allocate_window_id();
        let second_window_id = state.allocate_window_id();
        let first_column_id = state.allocate_column_id();
        let second_column_id = state.allocate_column_id();

        state.layout.columns.insert(
            first_column_id,
            Column::new(
                first_column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(220),
                vec![first_window_id],
            ),
        );
        state.layout.columns.insert(
            second_column_id,
            Column::new(
                second_column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(420),
                vec![second_window_id],
            ),
        );
        state.windows.insert(
            first_window_id,
            flowtile_domain::WindowNode {
                id: first_window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(first_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 220, 800),
                desired_size: Size::new(220, 800),
            },
        );
        state.windows.insert(
            second_window_id,
            flowtile_domain::WindowNode {
                id: second_window_id,
                current_hwnd_binding: Some(11),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(second_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 420, 800),
                desired_size: Size::new(420, 800),
            },
        );
        let workspace = state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist");
        workspace.strip.ordered_column_ids.push(first_column_id);
        workspace.strip.ordered_column_ids.push(second_column_id);
        state.focus.focused_window_id = Some(second_window_id);
        state.focus.focused_column_id = Some(second_column_id);

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let first_geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == first_window_id)
            .expect("first geometry should exist");
        let second_geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == second_window_id)
            .expect("second geometry should exist");

        assert_eq!(first_geometry.rect.x, 532);
        assert_eq!(second_geometry.rect.x, 764);
        assert_eq!(
            second_geometry.rect.x + second_geometry.rect.width as i32,
            1184
        );
    }

    #[test]
    fn narrow_multi_column_strip_stays_left_aligned_before_right_edge() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let first_window_id = state.allocate_window_id();
        let second_window_id = state.allocate_window_id();
        let first_column_id = state.allocate_column_id();
        let second_column_id = state.allocate_column_id();

        state.layout.columns.insert(
            first_column_id,
            Column::new(
                first_column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(220),
                vec![first_window_id],
            ),
        );
        state.layout.columns.insert(
            second_column_id,
            Column::new(
                second_column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(420),
                vec![second_window_id],
            ),
        );
        state.windows.insert(
            first_window_id,
            flowtile_domain::WindowNode {
                id: first_window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(first_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 220, 800),
                desired_size: Size::new(220, 800),
            },
        );
        state.windows.insert(
            second_window_id,
            flowtile_domain::WindowNode {
                id: second_window_id,
                current_hwnd_binding: Some(11),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Tiled,
                workspace_id,
                column_id: Some(second_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 420, 800),
                desired_size: Size::new(420, 800),
            },
        );
        let workspace = state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist");
        workspace.strip.ordered_column_ids.push(first_column_id);
        workspace.strip.ordered_column_ids.push(second_column_id);
        state.focus.focused_window_id = Some(first_window_id);
        state.focus.focused_column_id = Some(first_column_id);

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let first_geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == first_window_id)
            .expect("first geometry should exist");
        let second_geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == second_window_id)
            .expect("second geometry should exist");

        assert_eq!(first_geometry.rect.x, 16);
        assert_eq!(second_geometry.rect.x, 248);
    }

    #[test]
    fn tiled_windows_inside_column_respect_window_gap() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let upper_window_id = state.allocate_window_id();
        let lower_window_id = state.allocate_window_id();
        let column_id = state.allocate_column_id();

        state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                flowtile_domain::ColumnMode::Normal,
                WidthSemantics::Fixed(400),
                vec![upper_window_id, lower_window_id],
            ),
        );
        for window_id in [upper_window_id, lower_window_id] {
            state.windows.insert(
                window_id,
                flowtile_domain::WindowNode {
                    id: window_id,
                    current_hwnd_binding: Some(window_id.get()),
                    classification: flowtile_domain::WindowClassification::Application,
                    layer: flowtile_domain::WindowLayer::Tiled,
                    workspace_id,
                    column_id: Some(column_id),
                    is_managed: true,
                    is_floating: false,
                    is_fullscreen: false,
                    restore_target: None,
                    last_known_rect: Rect::new(0, 0, 400, 400),
                    desired_size: Size::new(400, 400),
                },
            );
        }
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .ordered_column_ids
            .push(column_id);

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let upper = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == upper_window_id)
            .expect("upper geometry should exist");
        let lower = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == lower_window_id)
            .expect("lower geometry should exist");

        assert_eq!(lower.rect.y, upper.rect.y + upper.rect.height as i32 + 12);
    }

    #[test]
    fn default_floating_rect_uses_floating_margin_inside_work_area() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_id = state.allocate_window_id();
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .floating_layer
            .ordered_window_ids
            .push(window_id);
        state.windows.insert(
            window_id,
            flowtile_domain::WindowNode {
                id: window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Floating,
                workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: true,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::default(),
                desired_size: Size::new(500, 320),
            },
        );

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .expect("floating geometry should exist");

        assert_eq!(geometry.rect.x, 16);
        assert_eq!(geometry.rect.y, 16);
    }

    #[test]
    fn fullscreen_window_uses_viewport_geometry() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_id = state.allocate_window_id();
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .floating_layer
            .ordered_window_ids
            .push(window_id);
        state.windows.insert(
            window_id,
            flowtile_domain::WindowNode {
                id: window_id,
                current_hwnd_binding: Some(10),
                classification: flowtile_domain::WindowClassification::Application,
                layer: flowtile_domain::WindowLayer::Fullscreen,
                workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: false,
                is_fullscreen: true,
                restore_target: None,
                last_known_rect: Rect::new(20, 20, 400, 300),
                desired_size: Size::new(400, 300),
            },
        );

        let projection = recompute_workspace(&state, workspace_id).expect("layout should succeed");
        let geometry = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .expect("geometry should exist");

        assert_eq!(geometry.rect, Rect::new(0, 0, 1200, 800));
        assert_eq!(geometry.layer, flowtile_domain::WindowLayer::Fullscreen);
    }
}
