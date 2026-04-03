use super::*;

impl StateStore {
    pub(super) fn handle_focus_workspace(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::WorkspaceCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = self.command_monitor_id(payload.monitor_id)?;
        let workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(monitor_id)
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        self.state.normalize_workspace_set(workspace_set_id);
        let ordered_workspace_ids = self
            .state
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        let active_workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;
        let Some(active_index) = ordered_workspace_ids
            .iter()
            .position(|workspace_id| *workspace_id == active_workspace_id)
        else {
            return Err(CoreError::UnknownWorkspace(active_workspace_id));
        };
        let target_index = if forward {
            active_index
                .saturating_add(1)
                .min(ordered_workspace_ids.len() - 1)
        } else {
            active_index.saturating_sub(1)
        };
        let target_workspace_id = ordered_workspace_ids[target_index];
        if target_workspace_id == active_workspace_id {
            return Ok(Some(active_workspace_id));
        }

        self.activate_workspace(monitor_id, target_workspace_id, FocusOrigin::UserCommand)?;
        self.state.normalize_workspace_set(workspace_set_id);
        Ok(Some(target_workspace_id))
    }

    pub(super) fn handle_move_workspace_within_monitor(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::WorkspaceCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = self.command_monitor_id(payload.monitor_id)?;
        let workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(monitor_id)
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        self.state.normalize_workspace_set(workspace_set_id);
        let active_workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;
        let Some(workspace_set) = self.state.workspace_sets.get(&workspace_set_id) else {
            return Err(CoreError::UnknownMonitor(monitor_id));
        };
        let Some(active_index) = workspace_set
            .ordered_workspace_ids
            .iter()
            .position(|workspace_id| *workspace_id == active_workspace_id)
        else {
            return Err(CoreError::UnknownWorkspace(active_workspace_id));
        };
        let target_index = if forward {
            active_index
                .saturating_add(1)
                .min(workspace_set.ordered_workspace_ids.len() - 1)
        } else {
            active_index.saturating_sub(1)
        };
        if target_index == active_index {
            return Ok(Some(active_workspace_id));
        }

        let workspace_set = self
            .state
            .workspace_sets
            .get_mut(&workspace_set_id)
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        workspace_set
            .ordered_workspace_ids
            .swap(active_index, target_index);
        workspace_set.active_workspace_id = active_workspace_id;
        self.state.normalize_workspace_set(workspace_set_id);
        self.sync_overview_selection(monitor_id);
        Ok(Some(active_workspace_id))
    }

    pub(super) fn handle_move_workspace_to_adjacent_monitor(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::WorkspaceCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let source_monitor_id = self.command_monitor_id(payload.monitor_id)?;
        let target_monitor_id = self
            .adjacent_monitor_id(source_monitor_id, forward)
            .unwrap_or(source_monitor_id);
        if target_monitor_id == source_monitor_id {
            return self
                .state
                .active_workspace_id_for_monitor(source_monitor_id)
                .map(Some)
                .ok_or(CoreError::NoActiveWorkspace(source_monitor_id));
        }

        let source_workspace_id = self
            .state
            .active_workspace_id_for_monitor(source_monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(source_monitor_id))?;
        let source_workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(source_monitor_id)
            .ok_or(CoreError::UnknownMonitor(source_monitor_id))?;
        let target_workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(target_monitor_id)
            .ok_or(CoreError::UnknownMonitor(target_monitor_id))?;
        self.state.normalize_workspace_set(source_workspace_set_id);
        self.state.normalize_workspace_set(target_workspace_set_id);

        {
            let source_workspace_set = self
                .state
                .workspace_sets
                .get_mut(&source_workspace_set_id)
                .ok_or(CoreError::UnknownMonitor(source_monitor_id))?;
            source_workspace_set
                .ordered_workspace_ids
                .retain(|workspace_id| *workspace_id != source_workspace_id);
        }

        let target_ordered_workspace_ids = self
            .state
            .workspace_sets
            .get(&target_workspace_set_id)
            .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
            .ok_or(CoreError::UnknownMonitor(target_monitor_id))?;
        let insert_index = target_ordered_workspace_ids
            .iter()
            .position(|workspace_id| {
                self.state
                    .workspaces
                    .get(workspace_id)
                    .is_some_and(|workspace| workspace.is_ephemeral_empty_tail)
            })
            .unwrap_or(target_ordered_workspace_ids.len());

        {
            let target_workspace_set = self
                .state
                .workspace_sets
                .get_mut(&target_workspace_set_id)
                .ok_or(CoreError::UnknownMonitor(target_monitor_id))?;
            target_workspace_set
                .ordered_workspace_ids
                .insert(insert_index, source_workspace_id);
            target_workspace_set.active_workspace_id = source_workspace_id;
        }

        if let Some(workspace) = self.state.workspaces.get_mut(&source_workspace_id) {
            workspace.monitor_id = target_monitor_id;
        }

        self.state.normalize_workspace_set(source_workspace_set_id);
        self.state.normalize_workspace_set(target_workspace_set_id);
        self.activate_workspace(
            target_monitor_id,
            source_workspace_id,
            FocusOrigin::UserCommand,
        )?;
        self.sync_overview_selection(source_monitor_id);
        Ok(Some(source_workspace_id))
    }

    pub(super) fn handle_move_column_to_workspace(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::WorkspaceCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = self.command_monitor_id(payload.monitor_id)?;
        let source_workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;
        let focused_window_id = self
            .focused_window_in_workspace(source_workspace_id)
            .ok_or(CoreError::InvalidEvent(
                "move column to workspace requires an active managed tiled window",
            ))?;
        let focused_window = self
            .state
            .windows
            .get(&focused_window_id)
            .ok_or(CoreError::UnknownWindow(focused_window_id))?
            .clone();
        let column_id = focused_window.column_id.ok_or(CoreError::InvalidEvent(
            "move column to workspace requires an active managed tiled column",
        ))?;
        if focused_window.layer != WindowLayer::Tiled
            || focused_window.is_floating
            || focused_window.is_fullscreen
        {
            return Err(CoreError::InvalidEvent(
                "move column to workspace requires an active managed tiled window",
            ));
        }

        let workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(monitor_id)
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        self.state.normalize_workspace_set(workspace_set_id);
        let ordered_workspace_ids = self
            .state
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
            .ok_or(CoreError::UnknownMonitor(monitor_id))?;
        let Some(source_index) = ordered_workspace_ids
            .iter()
            .position(|workspace_id| *workspace_id == source_workspace_id)
        else {
            return Err(CoreError::UnknownWorkspace(source_workspace_id));
        };
        let target_index = if forward {
            source_index
                .saturating_add(1)
                .min(ordered_workspace_ids.len() - 1)
        } else {
            source_index.saturating_sub(1)
        };
        let target_workspace_id = ordered_workspace_ids[target_index];
        if target_workspace_id == source_workspace_id {
            return Ok(Some(source_workspace_id));
        }
        self.move_column_to_workspace_target_internal(column_id, target_workspace_id, None)
    }

    pub(super) fn handle_move_column_to_workspace_target(
        &mut self,
        payload: &flowtile_domain::MoveColumnToWorkspaceTargetPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.move_column_to_workspace_target_internal(
            payload.source_column_id,
            payload.target_workspace_id,
            payload.insert_after_column_id,
        )
    }

    pub(super) fn move_column_to_workspace_target_internal(
        &mut self,
        source_column_id: ColumnId,
        target_workspace_id: WorkspaceId,
        insert_after_column_id: Option<ColumnId>,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let moved_window_ids = self
            .state
            .layout
            .columns
            .get(&source_column_id)
            .ok_or(CoreError::UnknownColumn(source_column_id))?
            .ordered_window_ids
            .clone();
        let source_window_id = moved_window_ids
            .first()
            .copied()
            .ok_or(CoreError::InvalidEvent(
                "move column target requires a non-empty managed tiled column",
            ))?;
        let source_workspace_id = self.column_workspace_id(source_column_id)?;
        let source_monitor_id = self
            .state
            .workspaces
            .get(&source_workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(source_workspace_id))?;
        let target_monitor_id = self
            .state
            .workspaces
            .get(&target_workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(target_workspace_id))?;
        let source_workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(source_monitor_id)
            .ok_or(CoreError::UnknownMonitor(source_monitor_id))?;
        let target_workspace_set_id = self
            .state
            .workspace_set_id_for_monitor(target_monitor_id)
            .ok_or(CoreError::UnknownMonitor(target_monitor_id))?;
        if source_workspace_id == target_workspace_id
            && insert_after_column_id == Some(source_column_id)
        {
            return Ok(Some(target_workspace_id));
        }

        if let Some(anchor_column_id) = insert_after_column_id {
            let anchor_workspace_id = self.column_workspace_id(anchor_column_id)?;
            if anchor_workspace_id != target_workspace_id {
                return Err(CoreError::InvalidEvent(
                    "move column target anchor must belong to the target workspace",
                ));
            }
        }

        let focus_window_id = self
            .state
            .layout
            .columns
            .get(&source_column_id)
            .and_then(|column| self.column_active_window(column))
            .unwrap_or(source_window_id);

        {
            let source_workspace = self
                .state
                .workspaces
                .get_mut(&source_workspace_id)
                .ok_or(CoreError::UnknownWorkspace(source_workspace_id))?;
            source_workspace
                .strip
                .ordered_column_ids
                .retain(|candidate_column_id| *candidate_column_id != source_column_id);
            if source_workspace.remembered_focused_column_id == Some(source_column_id) {
                source_workspace.remembered_focused_column_id = None;
            }
            if source_workspace
                .remembered_focused_window_id
                .is_some_and(|window_id| moved_window_ids.contains(&window_id))
            {
                source_workspace.remembered_focused_window_id = None;
            }
        }

        {
            let target_workspace = self
                .state
                .workspaces
                .get_mut(&target_workspace_id)
                .ok_or(CoreError::UnknownWorkspace(target_workspace_id))?;
            target_workspace
                .strip
                .ordered_column_ids
                .retain(|candidate_column_id| *candidate_column_id != source_column_id);
            let insert_index = insert_after_column_id
                .and_then(|anchor_column_id| {
                    target_workspace
                        .strip
                        .ordered_column_ids
                        .iter()
                        .position(|candidate_column_id| *candidate_column_id == anchor_column_id)
                        .map(|index| index + 1)
                })
                .unwrap_or(target_workspace.strip.ordered_column_ids.len())
                .min(target_workspace.strip.ordered_column_ids.len());
            target_workspace
                .strip
                .ordered_column_ids
                .insert(insert_index, source_column_id);
            target_workspace.remembered_focused_column_id = Some(source_column_id);
            target_workspace.remembered_focused_window_id = Some(focus_window_id);
        }

        for window_id in &moved_window_ids {
            if let Some(window) = self.state.windows.get_mut(window_id) {
                window.workspace_id = target_workspace_id;
                window.column_id = Some(source_column_id);
            }
        }

        self.activate_workspace(
            target_monitor_id,
            target_workspace_id,
            FocusOrigin::UserCommand,
        )?;
        self.state.normalize_workspace_set(source_workspace_set_id);
        if target_workspace_set_id != source_workspace_set_id {
            self.state.normalize_workspace_set(target_workspace_set_id);
            self.sync_overview_selection(source_monitor_id);
        }
        Ok(Some(target_workspace_id))
    }

    pub(super) fn command_monitor_id(
        &self,
        requested_monitor_id: Option<MonitorId>,
    ) -> Result<MonitorId, CoreError> {
        let monitor_id = if let Some(requested_monitor_id) = requested_monitor_id {
            requested_monitor_id
        } else if self.has_explicit_managed_monitor_set() {
            self.state
                .focus
                .focused_monitor_id
                .filter(|monitor_id| self.monitor_is_managed(*monitor_id))
                .or_else(|| self.first_managed_monitor_id())
                .ok_or(CoreError::InvalidEvent(
                    "workspace command requires at least one managed monitor",
                ))?
        } else {
            self.state
                .focus
                .focused_monitor_id
                .or_else(|| self.state.monitors.keys().next().copied())
                .ok_or(CoreError::InvalidEvent(
                    "workspace command requires a monitor context",
                ))?
        };

        self.state
            .monitors
            .contains_key(&monitor_id)
            .then_some(monitor_id)
            .ok_or(CoreError::UnknownMonitor(monitor_id))
    }

    pub(super) fn adjacent_monitor_id(
        &self,
        source_monitor_id: MonitorId,
        forward: bool,
    ) -> Option<MonitorId> {
        let monitor_ids = if self.has_explicit_managed_monitor_set() {
            self.managed_monitor_ids_in_navigation_order()
        } else {
            self.state.monitor_ids_in_navigation_order()
        };
        let source_index = monitor_ids
            .iter()
            .position(|monitor_id| *monitor_id == source_monitor_id)?;
        let target_index = if forward {
            source_index
                .saturating_add(1)
                .min(monitor_ids.len().saturating_sub(1))
        } else {
            source_index.saturating_sub(1)
        };
        monitor_ids.get(target_index).copied()
    }

    pub(super) fn activate_workspace(
        &mut self,
        monitor_id: MonitorId,
        workspace_id: WorkspaceId,
        origin: FocusOrigin,
    ) -> Result<(), CoreError> {
        let previous_column_id = self.focused_column_in_workspace(workspace_id);
        if let Some((window_id, column_id)) = self.workspace_focus_target(workspace_id)? {
            self.set_focus_to_window(monitor_id, workspace_id, window_id, column_id, origin)?;
            if let Some(column_id) = column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            } else {
                self.clamp_scroll_offset(workspace_id)?;
            }
        } else {
            self.set_active_workspace_without_focus(monitor_id, workspace_id, origin)?;
            self.clamp_scroll_offset(workspace_id)?;
        }
        self.sync_overview_selection(monitor_id);
        Ok(())
    }

    pub(super) fn workspace_focus_target(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<(WindowId, Option<ColumnId>)>, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if let Some(window_id) = workspace.remembered_focused_window_id
            && let Some(window) = self.state.windows.get(&window_id)
            && window.workspace_id == workspace_id
        {
            let column_id = window
                .column_id
                .filter(|column_id| workspace.strip.ordered_column_ids.contains(column_id));
            return Ok(Some((window_id, column_id)));
        }

        if let Some(column_id) = workspace.remembered_focused_column_id
            && workspace.strip.ordered_column_ids.contains(&column_id)
            && let Some(column) = self.state.layout.columns.get(&column_id)
            && let Some(window_id) = self.column_active_window(column)
        {
            return Ok(Some((window_id, Some(column_id))));
        }

        for column_id in &workspace.strip.ordered_column_ids {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            if let Some(window_id) = self.column_active_window(column) {
                return Ok(Some((window_id, Some(*column_id))));
            }
        }

        Ok(workspace
            .floating_layer
            .ordered_window_ids
            .first()
            .copied()
            .map(|window_id| (window_id, None)))
    }

    pub(super) fn set_active_workspace_without_focus(
        &mut self,
        monitor_id: MonitorId,
        workspace_id: WorkspaceId,
        origin: FocusOrigin,
    ) -> Result<(), CoreError> {
        self.state
            .workspaces
            .contains_key(&workspace_id)
            .then_some(())
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        self.state.focus.focused_monitor_id = Some(monitor_id);
        self.state.focus.focused_window_id = None;
        self.state.focus.focused_column_id = None;
        self.state.focus.focus_origin = origin;
        self.state
            .focus
            .active_workspace_by_monitor
            .insert(monitor_id, workspace_id);

        if let Some(workspace_set_id) = self.state.workspace_set_id_for_monitor(monitor_id)
            && let Some(workspace_set) = self.state.workspace_sets.get_mut(&workspace_set_id)
        {
            workspace_set.active_workspace_id = workspace_id;
        }

        Ok(())
    }

    pub(super) fn active_workspace_id_for_commands(&self) -> Result<WorkspaceId, CoreError> {
        if let Some(window_id) = self.state.focus.focused_window_id
            && let Some(window) = self.state.windows.get(&window_id)
            && window.is_managed
        {
            return Ok(window.workspace_id);
        }

        if self.has_explicit_managed_monitor_set() {
            if let Some(monitor_id) = self
                .state
                .focus
                .focused_monitor_id
                .filter(|monitor_id| self.monitor_is_managed(*monitor_id))
                && let Some(workspace_id) = self.state.active_workspace_id_for_monitor(monitor_id)
            {
                return Ok(workspace_id);
            }

            if let Some(monitor_id) = self.first_managed_monitor_id()
                && let Some(workspace_id) = self.state.active_workspace_id_for_monitor(monitor_id)
            {
                return Ok(workspace_id);
            }
        }

        if let Some(monitor_id) = self.state.focus.focused_monitor_id
            && let Some(workspace_id) = self.state.active_workspace_id_for_monitor(monitor_id)
        {
            return Ok(workspace_id);
        }

        self.state
            .workspace_sets
            .values()
            .next()
            .map(|workspace_set| workspace_set.active_workspace_id)
            .ok_or(CoreError::InvalidEvent(
                "no active workspace is available for command handling",
            ))
    }

    fn has_explicit_managed_monitor_set(&self) -> bool {
        !self
            .state
            .config_projection
            .managed_monitor_bindings
            .is_empty()
    }

    fn first_managed_monitor_id(&self) -> Option<MonitorId> {
        self.managed_monitor_ids_in_navigation_order()
            .into_iter()
            .next()
    }

    fn managed_monitor_ids_in_navigation_order(&self) -> Vec<MonitorId> {
        self.state
            .monitor_ids_in_navigation_order()
            .into_iter()
            .filter(|monitor_id| self.monitor_is_managed(*monitor_id))
            .collect()
    }

    fn monitor_is_managed(&self, monitor_id: MonitorId) -> bool {
        self.state.monitors.get(&monitor_id).is_some_and(|monitor| {
            self.state
                .config_projection
                .manages_monitor_binding(monitor.platform_binding.as_deref())
        })
    }

    pub(super) fn command_window_id(
        &self,
        requested: Option<WindowId>,
    ) -> Result<WindowId, CoreError> {
        requested
            .or(self.state.focus.focused_window_id)
            .ok_or(CoreError::InvalidEvent("command requires a target window"))
    }
}
