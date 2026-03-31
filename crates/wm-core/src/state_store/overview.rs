use super::*;

impl StateStore {
    pub(super) fn handle_open_overview(
        &mut self,
        payload: &flowtile_domain::OverviewCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = self.command_monitor_id(payload.monitor_id)?;
        self.open_overview_for_monitor(monitor_id)?;
        Ok(None)
    }

    pub(super) fn handle_close_overview(
        &mut self,
        _payload: &flowtile_domain::OverviewCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.close_overview();
        Ok(None)
    }

    pub(super) fn handle_toggle_overview(
        &mut self,
        payload: &flowtile_domain::OverviewCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = self.command_monitor_id(payload.monitor_id)?;
        if self.state.overview.is_open && self.state.overview.monitor_id == Some(monitor_id) {
            self.close_overview();
        } else {
            self.open_overview_for_monitor(monitor_id)?;
        }
        Ok(None)
    }

    pub(super) fn sync_overview_selection(&mut self, monitor_id: MonitorId) {
        if !self.state.overview.is_open || self.state.overview.monitor_id != Some(monitor_id) {
            return;
        }
        self.state.overview.selection = self.state.active_workspace_id_for_monitor(monitor_id);
        self.state.overview.projection_version =
            self.state.overview.projection_version.saturating_add(1);
    }

    pub(super) fn open_overview_for_monitor(
        &mut self,
        monitor_id: MonitorId,
    ) -> Result<(), CoreError> {
        let workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;
        let overview = &mut self.state.overview;
        let changed = !overview.is_open
            || overview.monitor_id != Some(monitor_id)
            || overview.selection != Some(workspace_id)
            || overview.drag_payload.is_some();
        overview.is_open = true;
        overview.monitor_id = Some(monitor_id);
        overview.selection = Some(workspace_id);
        overview.drag_payload = None;
        if changed {
            overview.projection_version = overview.projection_version.saturating_add(1);
        }
        Ok(())
    }

    pub(super) fn close_overview(&mut self) {
        let overview = &mut self.state.overview;
        let changed = overview.is_open
            || overview.monitor_id.is_some()
            || overview.selection.is_some()
            || overview.drag_payload.is_some();
        overview.is_open = false;
        overview.monitor_id = None;
        overview.selection = None;
        overview.drag_payload = None;
        if changed {
            overview.projection_version = overview.projection_version.saturating_add(1);
        }
    }
}
