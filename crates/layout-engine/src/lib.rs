#![forbid(unsafe_code)]

use flowtile_domain::{
    Column, ColumnMode, MaximizedState, Rect, WindowId, WindowLayer, WmState, WorkspaceId,
    all_column_modes,
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
    let viewport = workspace.strip.visible_region;
    let mut content_width = 0_u32;
    let mut x_cursor = viewport.x - workspace.strip.scroll_offset;
    let mut window_geometries = Vec::new();

    for column_id in &workspace.strip.ordered_column_ids {
        let column = state
            .layout
            .columns
            .get(column_id)
            .ok_or(LayoutError::ColumnMissing)?;
        let column_width = resolve_width(column, monitor.work_area_rect.width);
        content_width = content_width.saturating_add(column_width);

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

                    let mut y_cursor = viewport.y;
                    let mut remaining_height = viewport.height;
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
                            ((viewport.height as u64 * window.desired_size.height.max(1) as u64)
                                / desired_height_total as u64) as u32
                        }
                        .max(1);

                        window_geometries.push(WindowGeometryProjection {
                            window_id,
                            rect: Rect::new(x_cursor, y_cursor, column_width, height),
                            layer: window.layer,
                        });

                        y_cursor += height as i32;
                        remaining_height = remaining_height.saturating_sub(height);
                    }
                }
            }
        }

        x_cursor += column_width as i32;
    }

    for window_id in &workspace.floating_layer.ordered_window_ids {
        let window = state
            .windows
            .get(window_id)
            .ok_or(LayoutError::WindowMissing(*window_id))?;
        let rect = if window.layer == WindowLayer::Fullscreen || window.is_fullscreen {
            viewport
        } else if window.last_known_rect.width > 0 && window.last_known_rect.height > 0 {
            window.last_known_rect
        } else {
            Rect::new(
                viewport.x + 24,
                viewport.y + 24,
                window.desired_size.width.max(1),
                window.desired_size.height.max(1),
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

fn resolve_width(column: &Column, monitor_width: u32) -> u32 {
    if column.maximized_state == MaximizedState::Maximized
        || column.mode == ColumnMode::MaximizedColumn
    {
        monitor_width.max(1)
    } else {
        column.width_semantics.resolve(monitor_width)
    }
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
                WidthSemantics::Fixed(400),
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
                last_known_rect: Rect::new(0, 0, 400, 800),
                desired_size: Size::new(400, 800),
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

        assert_eq!(tiled.rect.x, -200);
        assert_eq!(floating.rect.x, 300);
        assert_eq!(floating.rect.y, 120);
    }

    #[test]
    fn maximized_column_uses_full_monitor_width() {
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

        assert_eq!(geometry.rect.width, 1200);
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
