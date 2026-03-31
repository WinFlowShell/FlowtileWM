use super::*;

impl StateStore {
    pub(super) fn handle_strip_scroll(
        &mut self,
        direction: i32,
        payload: &flowtile_domain::StripScrollPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self.active_workspace_id_for_commands()?;
        let step = if payload.step == 0 {
            self.state.config_projection.strip_scroll_step
        } else {
            payload.step
        }
        .min(i32::MAX as u32) as i32;
        self.apply_scroll_delta(workspace_id, direction.saturating_mul(step))?;
        Ok(Some(workspace_id))
    }

    pub(super) fn handle_toggle_floating(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if window.layer == WindowLayer::Floating {
            let restore_target = window.restore_target.clone().unwrap_or(RestoreTarget {
                workspace_id,
                column_id: self.focused_column_in_workspace(workspace_id),
                column_index: self.column_index_in_workspace(workspace_id, window.column_id),
                layer: WindowLayer::Tiled,
            });

            self.detach_window_membership(window_id)?;
            let restored_column_id = self.restore_window_to_target(
                window_id,
                restore_target.clone(),
                self.state.config_projection.default_column_mode,
                self.state.config_projection.default_column_width,
            )?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = restore_target.layer;
            window.column_id = restored_column_id;
            window.is_floating = false;
            window.is_fullscreen = false;
            window.restore_target = None;
            let previous_column_id = self.focused_column_in_workspace(workspace_id);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                restored_column_id,
                FocusOrigin::UserCommand,
            )?;
            if let Some(column_id) = restored_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        } else {
            let restore_target = RestoreTarget {
                workspace_id,
                column_id: window.column_id,
                column_index: self.column_index_in_workspace(workspace_id, window.column_id),
                layer: window.layer,
            };
            self.detach_window_membership(window_id)?;
            self.push_window_to_floating_layer(workspace_id, window_id)?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = WindowLayer::Floating;
            window.column_id = None;
            window.is_floating = true;
            window.is_fullscreen = false;
            window.restore_target = Some(restore_target);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                None,
                FocusOrigin::UserCommand,
            )?;
        }

        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }

    pub(super) fn handle_toggle_tabbed(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let (workspace_id, column_id) = {
            let window = self
                .state
                .windows
                .get(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            let Some(column_id) = window.column_id else {
                return Ok(Some(window.workspace_id));
            };
            (window.workspace_id, column_id)
        };

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        if column.mode == ColumnMode::Tabbed {
            column.mode = ColumnMode::Normal;
            column.tab_selection = column.ordered_window_ids.first().copied();
        } else {
            column.mode = ColumnMode::Tabbed;
            column.tab_selection = Some(window_id);
        }
        column.active_window_id = Some(window_id);

        self.reveal_column_in_workspace(
            workspace_id,
            column_id,
            self.focused_column_in_workspace(workspace_id),
        )?;
        Ok(Some(workspace_id))
    }

    pub(super) fn handle_toggle_maximized(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let (workspace_id, column_id) = {
            let window = self
                .state
                .windows
                .get(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            let Some(column_id) = window.column_id else {
                return Ok(Some(window.workspace_id));
            };
            (window.workspace_id, column_id)
        };

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        column.maximized_state = match column.maximized_state {
            MaximizedState::Normal => MaximizedState::Maximized,
            MaximizedState::Maximized => MaximizedState::Normal,
        };

        self.reveal_column_in_workspace(
            workspace_id,
            column_id,
            self.focused_column_in_workspace(workspace_id),
        )?;
        Ok(Some(workspace_id))
    }

    pub(super) fn handle_toggle_fullscreen(
        &mut self,
        payload: &flowtile_domain::WindowCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window_id = self.command_window_id(payload.window_id)?;
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if window.layer == WindowLayer::Fullscreen {
            let restore_target = window.restore_target.clone().unwrap_or(RestoreTarget {
                workspace_id,
                column_id: self.focused_column_in_workspace(workspace_id),
                column_index: self.column_index_in_workspace(workspace_id, window.column_id),
                layer: WindowLayer::Tiled,
            });
            self.detach_window_membership(window_id)?;
            let restored_column_id = self.restore_window_to_target(
                window_id,
                restore_target.clone(),
                self.state.config_projection.default_column_mode,
                self.state.config_projection.default_column_width,
            )?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = restore_target.layer;
            window.column_id = restored_column_id;
            window.is_floating = restore_target.layer == WindowLayer::Floating;
            window.is_fullscreen = false;
            window.restore_target = None;
            let previous_column_id = self.focused_column_in_workspace(workspace_id);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                restored_column_id,
                FocusOrigin::UserCommand,
            )?;
            if let Some(column_id) = restored_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        } else {
            let restore_target = RestoreTarget {
                workspace_id,
                column_id: window.column_id,
                column_index: self.column_index_in_workspace(workspace_id, window.column_id),
                layer: window.layer,
            };
            self.detach_window_membership(window_id)?;
            self.push_window_to_floating_layer(workspace_id, window_id)?;
            let window = self
                .state
                .windows
                .get_mut(&window_id)
                .ok_or(CoreError::UnknownWindow(window_id))?;
            window.layer = WindowLayer::Fullscreen;
            window.column_id = None;
            window.is_floating = false;
            window.is_fullscreen = true;
            window.restore_target = Some(restore_target);
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                None,
                FocusOrigin::UserCommand,
            )?;
        }

        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }
}
