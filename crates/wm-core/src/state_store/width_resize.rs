use super::*;

impl StateStore {
    pub(super) fn handle_begin_column_width_resize(
        &mut self,
        payload: &flowtile_domain::ColumnWidthResizePayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let (workspace_id, window_id, column_id) = self.active_tiled_width_target()?;
        let projection = recompute_workspace(&self.state, workspace_id)?;
        let column_rect = projection
            .window_geometries
            .iter()
            .find(|geometry| geometry.window_id == window_id)
            .map(|geometry| geometry.rect)
            .ok_or(CoreError::InvalidEvent(
                "active tiled window is missing from layout projection",
            ))?;
        let viewport = self.workspace_tiled_viewport(workspace_id)?;
        let initial_width = self.column_target_width_bounds(column_id, workspace_id)?.1;
        let (target_width, clamped_preview_rect, anchor_x, current_pointer_x) = self
            .compute_width_resize_metrics(
                payload.edge,
                column_rect,
                initial_width,
                viewport,
                payload.pointer_x,
            )?;

        self.state.layout.width_resize_session = Some(WidthResizeSession {
            workspace_id,
            column_id,
            window_id,
            anchor_edge: payload.edge,
            anchor_x,
            current_pointer_x,
            initial_column_rect: column_rect,
            initial_width,
            target_width,
            clamped_preview_rect,
        });

        Ok(Some(workspace_id))
    }

    pub(super) fn handle_update_column_width_preview(
        &mut self,
        payload: &flowtile_domain::ColumnWidthPointerPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let Some(session) = self.state.layout.width_resize_session.clone() else {
            return Ok(None);
        };
        let viewport = self.workspace_tiled_viewport(session.workspace_id)?;
        let (target_width, clamped_preview_rect, anchor_x, current_pointer_x) = self
            .compute_width_resize_metrics(
                session.anchor_edge,
                session.initial_column_rect,
                session.initial_width,
                viewport,
                payload.pointer_x,
            )?;
        if let Some(active_session) = self.state.layout.width_resize_session.as_mut() {
            active_session.anchor_x = anchor_x;
            active_session.current_pointer_x = current_pointer_x;
            active_session.target_width = target_width;
            active_session.clamped_preview_rect = clamped_preview_rect;
        }
        Ok(Some(session.workspace_id))
    }

    pub(super) fn handle_commit_column_width(
        &mut self,
        payload: &flowtile_domain::ColumnWidthPointerPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let Some(session) = self.state.layout.width_resize_session.clone() else {
            return Ok(None);
        };
        let viewport = self.workspace_tiled_viewport(session.workspace_id)?;
        let (target_width, _, _, _) = self.compute_width_resize_metrics(
            session.anchor_edge,
            session.initial_column_rect,
            session.initial_width,
            viewport,
            payload.pointer_x,
        )?;
        let column = self
            .state
            .layout
            .columns
            .get_mut(&session.column_id)
            .ok_or(CoreError::UnknownColumn(session.column_id))?;
        column.width_semantics = WidthSemantics::Fixed(target_width);
        column.maximized_state = MaximizedState::Normal;
        if column.mode == ColumnMode::MaximizedColumn {
            column.mode = ColumnMode::Normal;
        }
        self.state.layout.width_resize_session = None;
        self.clamp_scroll_offset(session.workspace_id)?;
        self.reveal_column_in_workspace(
            session.workspace_id,
            session.column_id,
            Some(session.column_id),
        )?;
        Ok(Some(session.workspace_id))
    }

    pub(super) fn handle_cancel_column_width_resize(
        &mut self,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self
            .state
            .layout
            .width_resize_session
            .as_ref()
            .map(|session| session.workspace_id);
        self.state.layout.width_resize_session = None;
        Ok(workspace_id)
    }

    pub(super) fn handle_cycle_column_width(&mut self) -> Result<Option<WorkspaceId>, CoreError> {
        let (workspace_id, _window_id, column_id) = self.active_tiled_width_target()?;
        let viewport = self.workspace_tiled_viewport(workspace_id)?;
        let (min_width, max_width) = self.column_target_width_bounds(column_id, workspace_id)?;
        let current_width = {
            let column = self
                .state
                .layout
                .columns
                .get(&column_id)
                .ok_or(CoreError::UnknownColumn(column_id))?;
            self.resolve_column_width(column, viewport.width)
        };
        let next_width = self.next_cycled_column_width(current_width, min_width, max_width);
        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        column.width_semantics = WidthSemantics::Fixed(next_width);
        column.maximized_state = MaximizedState::Normal;
        if column.mode == ColumnMode::MaximizedColumn {
            column.mode = ColumnMode::Normal;
        }
        self.clamp_scroll_offset(workspace_id)?;
        self.reveal_column_in_workspace(workspace_id, column_id, Some(column_id))?;
        Ok(Some(workspace_id))
    }

    pub(super) fn active_tiled_width_target(
        &self,
    ) -> Result<(WorkspaceId, WindowId, ColumnId), CoreError> {
        let workspace_id = self.active_workspace_id_for_commands()?;
        let window_id =
            self.focused_window_in_workspace(workspace_id)
                .ok_or(CoreError::InvalidEvent(
                    "width command requires an active tiled window",
                ))?;
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?;
        let column_id = window.column_id.ok_or(CoreError::InvalidEvent(
            "width command requires an active tiled column",
        ))?;
        if window.layer != WindowLayer::Tiled || window.is_floating || window.is_fullscreen {
            return Err(CoreError::InvalidEvent(
                "width command requires an active managed tiled window",
            ));
        }
        Ok((workspace_id, window_id, column_id))
    }

    pub(super) fn column_target_width_bounds(
        &self,
        column_id: ColumnId,
        workspace_id: WorkspaceId,
    ) -> Result<(u32, u32), CoreError> {
        let viewport = self.workspace_tiled_viewport(workspace_id)?;
        let max_width = viewport.width.max(1);
        let min_width = (max_width / 6).max(1);
        let _ = self
            .state
            .layout
            .columns
            .get(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        Ok((min_width, max_width))
    }

    pub(super) fn compute_width_resize_metrics(
        &self,
        edge: ResizeEdge,
        initial_column_rect: Rect,
        initial_width: u32,
        viewport: Rect,
        pointer_x: i32,
    ) -> Result<(u32, Rect, i32, i32), CoreError> {
        let max_width = viewport.width.max(1);
        let min_width = (max_width / 6).max(1);
        let initial_left = initial_column_rect.x;
        let initial_right = initial_column_rect
            .x
            .saturating_add(initial_column_rect.width as i32);

        let (min_pointer_x, max_pointer_x, anchor_x) = match edge {
            ResizeEdge::Right => (
                initial_left.saturating_add(min_width as i32),
                initial_left.saturating_add(max_width as i32),
                initial_right,
            ),
            ResizeEdge::Left => (
                initial_right.saturating_sub(max_width as i32),
                initial_right.saturating_sub(min_width as i32),
                initial_left,
            ),
        };
        let viewport_left = viewport.x;
        let viewport_right = viewport.x.saturating_add(viewport.width as i32);
        let clamped_pointer_x = pointer_x
            .clamp(min_pointer_x, max_pointer_x)
            .clamp(viewport_left, viewport_right);
        let target_width = match edge {
            ResizeEdge::Right => clamped_pointer_x.saturating_sub(initial_left) as u32,
            ResizeEdge::Left => initial_right.saturating_sub(clamped_pointer_x) as u32,
        }
        .clamp(min_width, max_width);
        let preview_left = anchor_x.min(clamped_pointer_x);
        let preview_right = anchor_x.max(clamped_pointer_x);
        let preview_width = (preview_right.saturating_sub(preview_left) as u32).max(1);
        let preview_rect = Rect::new(
            preview_left,
            viewport.y,
            preview_width,
            initial_column_rect.height.min(viewport.height).max(1),
        );

        let _ = initial_width;
        Ok((target_width, preview_rect, anchor_x, clamped_pointer_x))
    }

    pub(super) fn next_cycled_column_width(
        &self,
        current_width: u32,
        min_width: u32,
        max_width: u32,
    ) -> u32 {
        let mut steps = [
            max_width / 3,
            max_width / 2,
            (max_width.saturating_mul(2)) / 3,
            max_width,
        ]
        .map(|width| width.clamp(min_width, max_width).max(1))
        .to_vec();
        steps.sort_unstable();
        steps.dedup();
        steps
            .iter()
            .copied()
            .find(|width| *width > current_width)
            .unwrap_or_else(|| steps[0])
    }
}
