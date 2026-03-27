use flowtile_diagnostics::{layout_recomputed, transition_applied};
use flowtile_domain::{
    Column, ColumnId, ColumnMode, DomainEvent, DomainEventPayload, FocusBehavior, FocusOrigin,
    MaximizedState, MonitorId, NavigationScope, Rect, ResizeEdge, RestoreTarget, RuntimeMode,
    WidthResizeSession, WidthSemantics, WindowId, WindowLayer, WindowNode, WindowPlacement,
    WmState, WorkspaceId,
};
use flowtile_layout_engine::{padded_tiled_viewport, recompute_workspace};

use crate::{CoreError, NewColumnRequest, StateStore, TransitionResult};

impl StateStore {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            state: WmState::new(runtime_mode),
        }
    }

    pub const fn state(&self) -> &WmState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut WmState {
        &mut self.state
    }

    pub fn dispatch(&mut self, event: DomainEvent) -> Result<TransitionResult, CoreError> {
        let affected_workspace_id = self.apply_event(&event)?;
        let state_version = self.state.bump_state_version();
        let mut diagnostics = vec![transition_applied(
            state_version,
            event.correlation_id,
            event.event_id.as_str(),
        )];
        let layout_projection = if let Some(workspace_id) = affected_workspace_id {
            let projection = recompute_workspace(&self.state, workspace_id)?;
            diagnostics.push(layout_recomputed(
                state_version,
                event.correlation_id,
                workspace_id,
                projection.window_geometries.len(),
            ));
            Some(projection)
        } else {
            None
        };

        self.state.diagnostics_summary.total_records += diagnostics.len() as u64;
        self.state.diagnostics_summary.last_transition_label =
            Some(event.event_id.as_str().to_string());
        self.state.diagnostics_summary.last_state_version = state_version;

        Ok(TransitionResult {
            state_version,
            affected_workspace_id,
            layout_projection,
            diagnostics,
        })
    }

    fn apply_event(&mut self, event: &DomainEvent) -> Result<Option<WorkspaceId>, CoreError> {
        match &event.payload {
            DomainEventPayload::WindowDiscovered(payload) => self.handle_window_discovered(payload),
            DomainEventPayload::WindowDestroyed(payload) => self.handle_window_destroyed(payload),
            DomainEventPayload::WindowFocusObserved(payload) => {
                self.handle_window_focus_observed(payload)
            }
            DomainEventPayload::CmdFocusNext(payload) => {
                self.handle_focus_navigation(true, payload)
            }
            DomainEventPayload::CmdFocusPrev(payload) => {
                self.handle_focus_navigation(false, payload)
            }
            DomainEventPayload::CmdScrollStripLeft(payload) => {
                self.handle_strip_scroll(-1, payload)
            }
            DomainEventPayload::CmdScrollStripRight(payload) => {
                self.handle_strip_scroll(1, payload)
            }
            DomainEventPayload::CmdToggleFloating(payload) => self.handle_toggle_floating(payload),
            DomainEventPayload::CmdToggleTabbed(payload) => self.handle_toggle_tabbed(payload),
            DomainEventPayload::CmdToggleMaximized(payload) => {
                self.handle_toggle_maximized(payload)
            }
            DomainEventPayload::CmdToggleFullscreen(payload) => {
                self.handle_toggle_fullscreen(payload)
            }
            DomainEventPayload::CmdToggleOverview(payload) => self.handle_toggle_overview(payload),
            DomainEventPayload::CmdBeginColumnWidthResize(payload) => {
                self.handle_begin_column_width_resize(payload)
            }
            DomainEventPayload::CmdUpdateColumnWidthPreview(payload) => {
                self.handle_update_column_width_preview(payload)
            }
            DomainEventPayload::CmdCommitColumnWidth(payload) => {
                self.handle_commit_column_width(payload)
            }
            DomainEventPayload::CmdCancelColumnWidthResize => {
                self.handle_cancel_column_width_resize()
            }
            DomainEventPayload::CmdCycleColumnWidth => self.handle_cycle_column_width(),
            DomainEventPayload::ConfigReloadRequested(_) => Ok(None),
            DomainEventPayload::ConfigReloadSucceeded(payload) => {
                self.handle_config_reload_succeeded(payload)
            }
            DomainEventPayload::ConfigReloadFailed(_) => Ok(None),
            DomainEventPayload::RulesUpdated(payload) => self.handle_rules_updated(payload),
            DomainEventPayload::ReconcileRequested => Ok(None),
            DomainEventPayload::FullScanRequested => Ok(None),
        }
    }

    fn handle_window_discovered(
        &mut self,
        payload: &flowtile_domain::WindowDiscoveredPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        if !self.state.monitors.contains_key(&payload.monitor_id) {
            return Err(CoreError::UnknownMonitor(payload.monitor_id));
        }

        let workspace_id = self
            .state
            .active_workspace_id_for_monitor(payload.monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(payload.monitor_id))?;
        let focused_window_id = self.focused_window_in_workspace(workspace_id);
        let focused_window_is_fullscreen = focused_window_id
            .and_then(|window_id| self.state.windows.get(&window_id))
            .is_some_and(|window| window.layer == WindowLayer::Fullscreen || window.is_fullscreen);
        let should_preserve_current_focus =
            matches!(payload.focus_behavior, FocusBehavior::PreserveCurrentFocus)
                && focused_window_id.is_some()
                && !focused_window_is_fullscreen;
        let focused_column_id = self.focused_column_in_workspace(workspace_id);
        let insertion_anchor_column_id = self.discovery_anchor_column_in_workspace(workspace_id);
        let fullscreen_restore_index = self.fullscreen_restore_index_in_workspace(workspace_id);
        let window_id = self.state.allocate_window_id();

        let target_column_id = match payload.layer {
            WindowLayer::Floating | WindowLayer::Fullscreen => {
                self.push_window_to_floating_layer(workspace_id, window_id)?;
                None
            }
            _ => Some(match payload.placement {
                WindowPlacement::AppendToFocusedColumn => {
                    if let Some(column_id) = focused_column_id {
                        let column = self
                            .state
                            .layout
                            .columns
                            .get_mut(&column_id)
                            .ok_or(CoreError::UnknownColumn(column_id))?;
                        column.ordered_window_ids.push(window_id);
                        if column.active_window_id.is_none() {
                            column.active_window_id = Some(window_id);
                        }
                        if column.mode == ColumnMode::Tabbed {
                            column.tab_selection = Some(window_id);
                        }
                        column_id
                    } else {
                        self.insert_new_column(
                            workspace_id,
                            window_id,
                            NewColumnRequest {
                                anchor_column_id: None,
                                insert_index_override: None,
                                before_anchor: false,
                                mode: ColumnMode::Normal,
                                width_semantics: WidthSemantics::default(),
                                preserve_focus_position: false,
                            },
                        )?
                    }
                }
                WindowPlacement::NewColumnAfterFocus { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: insertion_anchor_column_id,
                        insert_index_override: insertion_anchor_column_id
                            .is_none()
                            .then_some(fullscreen_restore_index)
                            .flatten(),
                        before_anchor: false,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: false,
                    },
                )?,
                WindowPlacement::NewColumnBeforeFocus { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: insertion_anchor_column_id,
                        insert_index_override: insertion_anchor_column_id
                            .is_none()
                            .then_some(fullscreen_restore_index)
                            .flatten(),
                        before_anchor: true,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: should_preserve_current_focus,
                    },
                )?,
                WindowPlacement::AppendToWorkspaceEnd { mode, width } => self.insert_new_column(
                    workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: None,
                        insert_index_override: None,
                        before_anchor: false,
                        mode,
                        width_semantics: width,
                        preserve_focus_position: false,
                    },
                )?,
            }),
        };

        self.state.windows.insert(
            window_id,
            WindowNode {
                id: window_id,
                current_hwnd_binding: Some(payload.hwnd),
                classification: payload.classification,
                layer: payload.layer,
                workspace_id,
                column_id: target_column_id,
                is_managed: payload.managed,
                is_floating: payload.layer == WindowLayer::Floating,
                is_fullscreen: payload.layer == WindowLayer::Fullscreen,
                restore_target: None,
                last_known_rect: payload.last_known_rect,
                desired_size: payload.desired_size,
            },
        );

        self.state
            .focus
            .active_workspace_by_monitor
            .insert(payload.monitor_id, workspace_id);

        if let Some(workspace_set_id) = self.state.workspace_set_id_for_monitor(payload.monitor_id)
            && let Some(workspace_set) = self.state.workspace_sets.get_mut(&workspace_set_id)
        {
            workspace_set.active_workspace_id = workspace_id;
        }

        let previous_column_id = focused_column_id.or(insertion_anchor_column_id);
        if !should_preserve_current_focus {
            self.set_focus_to_window(
                payload.monitor_id,
                workspace_id,
                window_id,
                target_column_id,
                FocusOrigin::ReducerDefault,
            )?;
        }

        self.state.ensure_tail_workspace(payload.monitor_id);
        if !should_preserve_current_focus {
            self.clamp_scroll_offset(workspace_id)?;
            if let Some(column_id) = target_column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
            }
        }
        Ok(Some(workspace_id))
    }

    fn handle_window_destroyed(
        &mut self,
        payload: &flowtile_domain::WindowDestroyedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let window = self
            .state
            .windows
            .get(&payload.window_id)
            .ok_or(CoreError::UnknownWindow(payload.window_id))?
            .clone();
        let workspace_id = window.workspace_id;
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        self.detach_window_membership(payload.window_id)?;
        self.state.windows.remove(&payload.window_id);

        if self.state.focus.focused_window_id == Some(payload.window_id) {
            self.retarget_focus_after_destroy(workspace_id, window.column_id)?;
        }

        self.state.ensure_tail_workspace(monitor_id);
        self.clamp_scroll_offset(workspace_id)?;
        Ok(Some(workspace_id))
    }

    fn handle_window_focus_observed(
        &mut self,
        payload: &flowtile_domain::WindowFocusObservedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        if !self.state.monitors.contains_key(&payload.monitor_id) {
            return Err(CoreError::UnknownMonitor(payload.monitor_id));
        }

        let window = self
            .state
            .windows
            .get(&payload.window_id)
            .ok_or(CoreError::UnknownWindow(payload.window_id))?;
        let workspace_id = window.workspace_id;
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        if workspace.monitor_id != payload.monitor_id {
            return Err(CoreError::InvalidEvent(
                "focused window monitor does not match workspace monitor",
            ));
        }

        let focused_column_id = window.column_id;
        let previous_column_id = self.focused_column_in_workspace(workspace_id);
        self.set_focus_to_window(
            payload.monitor_id,
            workspace_id,
            payload.window_id,
            focused_column_id,
            payload.focus_origin,
        )?;
        if let Some(column_id) = focused_column_id {
            self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
        }

        Ok(Some(workspace_id))
    }

    fn handle_focus_navigation(
        &mut self,
        forward: bool,
        payload: &flowtile_domain::FocusCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self.active_workspace_id_for_commands()?;

        if matches!(payload.scope, NavigationScope::ColumnTabs)
            && let Some(column_id) = self.focused_column_in_workspace(workspace_id)
            && self.try_cycle_tabbed_focus(workspace_id, column_id, forward)?
        {
            return Ok(Some(workspace_id));
        }

        let sequence = self.navigation_sequence_for_workspace(workspace_id)?;
        if sequence.is_empty() {
            return Ok(Some(workspace_id));
        }

        let next_index = match self.focused_window_in_workspace(workspace_id) {
            Some(window_id) => {
                let current_index = sequence
                    .iter()
                    .position(|(candidate_window_id, _)| *candidate_window_id == window_id)
                    .unwrap_or(0);
                let last_index = sequence.len().saturating_sub(1);
                if forward {
                    current_index.saturating_add(1).min(last_index)
                } else {
                    current_index.saturating_sub(1)
                }
            }
            None if forward => 0,
            None => sequence.len() - 1,
        };
        let (window_id, column_id) = sequence[next_index];
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        let previous_column_id = self.focused_column_in_workspace(workspace_id);
        self.set_focus_to_window(
            monitor_id,
            workspace_id,
            window_id,
            column_id,
            FocusOrigin::UserCommand,
        )?;
        if let Some(column_id) = column_id {
            self.reveal_column_in_workspace(workspace_id, column_id, previous_column_id)?;
        }

        Ok(Some(workspace_id))
    }

    fn handle_strip_scroll(
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

    fn handle_toggle_floating(
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

    fn handle_toggle_tabbed(
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

    fn handle_toggle_maximized(
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

    fn handle_toggle_fullscreen(
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

    fn handle_toggle_overview(
        &mut self,
        payload: &flowtile_domain::OverviewCommandPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        let monitor_id = payload
            .monitor_id
            .or(self.state.focus.focused_monitor_id)
            .ok_or(CoreError::InvalidEvent(
                "overview command requires a monitor context",
            ))?;
        let workspace_id = self
            .state
            .active_workspace_id_for_monitor(monitor_id)
            .ok_or(CoreError::NoActiveWorkspace(monitor_id))?;

        self.state.overview.is_open = !self.state.overview.is_open;
        self.state.overview.monitor_id = self.state.overview.is_open.then_some(monitor_id);
        self.state.overview.selection = self.state.overview.is_open.then_some(workspace_id);
        self.state.overview.projection_version =
            self.state.overview.projection_version.saturating_add(1);
        Ok(None)
    }

    fn handle_begin_column_width_resize(
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

    fn handle_update_column_width_preview(
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

    fn handle_commit_column_width(
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

    fn handle_cancel_column_width_resize(&mut self) -> Result<Option<WorkspaceId>, CoreError> {
        let workspace_id = self
            .state
            .layout
            .width_resize_session
            .as_ref()
            .map(|session| session.workspace_id);
        self.state.layout.width_resize_session = None;
        Ok(workspace_id)
    }

    fn handle_cycle_column_width(&mut self) -> Result<Option<WorkspaceId>, CoreError> {
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

    fn handle_config_reload_succeeded(
        &mut self,
        payload: &flowtile_domain::ConfigReloadSucceededPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.state.config_projection = payload.projection.clone();
        Ok(None)
    }

    fn handle_rules_updated(
        &mut self,
        payload: &flowtile_domain::RulesUpdatedPayload,
    ) -> Result<Option<WorkspaceId>, CoreError> {
        self.state.config_projection.active_rule_count = payload.active_rule_count;
        Ok(None)
    }

    fn insert_new_column(
        &mut self,
        workspace_id: WorkspaceId,
        window_id: WindowId,
        request: NewColumnRequest,
    ) -> Result<ColumnId, CoreError> {
        let column_id = self.state.allocate_column_id();
        self.state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                request.mode,
                request.width_semantics,
                vec![window_id],
            ),
        );
        let tiled_viewport_width = self.workspace_tiled_viewport(workspace_id)?.width;

        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let default_insert_index = if request.before_anchor {
            0
        } else {
            workspace.strip.ordered_column_ids.len()
        };
        let insert_index = request
            .insert_index_override
            .map(|index| index.min(workspace.strip.ordered_column_ids.len()))
            .or_else(|| {
                request.anchor_column_id.and_then(|anchor| {
                    workspace
                        .strip
                        .ordered_column_ids
                        .iter()
                        .position(|column_id| *column_id == anchor)
                        .map(|index| {
                            if request.before_anchor {
                                index
                            } else {
                                index + 1
                            }
                        })
                })
            })
            .unwrap_or(default_insert_index);

        workspace
            .strip
            .ordered_column_ids
            .insert(insert_index, column_id);

        if request.preserve_focus_position
            && request.before_anchor
            && request.anchor_column_id.is_some()
        {
            let column_gap = self.state.config_projection.layout_spacing.column_gap;
            let width = request
                .width_semantics
                .resolve(tiled_viewport_width)
                .min(i32::MAX as u32) as i32;
            workspace.strip.scroll_offset = workspace
                .strip
                .scroll_offset
                .saturating_add(width)
                .saturating_add(column_gap.min(i32::MAX as u32) as i32);
        }

        Ok(column_id)
    }

    fn active_tiled_width_target(&self) -> Result<(WorkspaceId, WindowId, ColumnId), CoreError> {
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

    fn column_target_width_bounds(
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

    fn compute_width_resize_metrics(
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

    fn next_cycled_column_width(&self, current_width: u32, min_width: u32, max_width: u32) -> u32 {
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

    fn focused_window_in_workspace(&self, workspace_id: WorkspaceId) -> Option<WindowId> {
        self.state.focus.focused_window_id.filter(|window_id| {
            self.state
                .windows
                .get(window_id)
                .is_some_and(|window| window.workspace_id == workspace_id)
        })
    }

    fn focused_column_in_workspace(&self, workspace_id: WorkspaceId) -> Option<ColumnId> {
        let focused_column_id = self.state.focus.focused_column_id?;
        let workspace = self.state.workspaces.get(&workspace_id)?;
        workspace
            .strip
            .ordered_column_ids
            .contains(&focused_column_id)
            .then_some(focused_column_id)
    }

    fn discovery_anchor_column_in_workspace(&self, workspace_id: WorkspaceId) -> Option<ColumnId> {
        if let Some(column_id) = self.focused_column_in_workspace(workspace_id) {
            return Some(column_id);
        }

        let focused_window_id = self.focused_window_in_workspace(workspace_id)?;
        let window = self.state.windows.get(&focused_window_id)?;
        if window.layer != WindowLayer::Fullscreen && !window.is_fullscreen {
            return None;
        }

        let restore_target = window.restore_target.as_ref()?;
        if restore_target.workspace_id != workspace_id {
            return None;
        }

        let column_id = restore_target.column_id?;
        self.state
            .workspaces
            .get(&workspace_id)?
            .strip
            .ordered_column_ids
            .contains(&column_id)
            .then_some(column_id)
    }

    fn fullscreen_restore_index_in_workspace(&self, workspace_id: WorkspaceId) -> Option<usize> {
        let focused_window_id = self.focused_window_in_workspace(workspace_id)?;
        let window = self.state.windows.get(&focused_window_id)?;
        if window.layer != WindowLayer::Fullscreen && !window.is_fullscreen {
            return None;
        }

        let restore_target = window.restore_target.as_ref()?;
        (restore_target.workspace_id == workspace_id)
            .then_some(restore_target.column_index)
            .flatten()
    }

    fn column_index_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        column_id: Option<ColumnId>,
    ) -> Option<usize> {
        let column_id = column_id?;
        self.state
            .workspaces
            .get(&workspace_id)?
            .strip
            .ordered_column_ids
            .iter()
            .position(|candidate_column_id| *candidate_column_id == column_id)
    }

    fn column_active_window(&self, column: &Column) -> Option<WindowId> {
        if column.mode == ColumnMode::Tabbed {
            column
                .tab_selection
                .or(column.active_window_id)
                .or_else(|| column.ordered_window_ids.first().copied())
        } else {
            column
                .active_window_id
                .or_else(|| column.ordered_window_ids.first().copied())
        }
    }

    fn active_workspace_id_for_commands(&self) -> Result<WorkspaceId, CoreError> {
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

    fn command_window_id(&self, requested: Option<WindowId>) -> Result<WindowId, CoreError> {
        requested
            .or(self.state.focus.focused_window_id)
            .ok_or(CoreError::InvalidEvent("command requires a target window"))
    }

    fn push_window_to_floating_layer(
        &mut self,
        workspace_id: WorkspaceId,
        window_id: WindowId,
    ) -> Result<(), CoreError> {
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        if !workspace
            .floating_layer
            .ordered_window_ids
            .contains(&window_id)
        {
            workspace.floating_layer.ordered_window_ids.push(window_id);
        }
        let z_hint = workspace.floating_layer.ordered_window_ids.len() as u32;
        workspace.floating_layer.z_hints.insert(window_id, z_hint);
        Ok(())
    }

    fn detach_window_membership(&mut self, window_id: WindowId) -> Result<(), CoreError> {
        let window = self
            .state
            .windows
            .get(&window_id)
            .ok_or(CoreError::UnknownWindow(window_id))?
            .clone();

        if let Some(column_id) = window.column_id {
            let mut column_is_empty = false;
            if let Some(column) = self.state.layout.columns.get_mut(&column_id) {
                column
                    .ordered_window_ids
                    .retain(|candidate_window_id| *candidate_window_id != window_id);
                if column.tab_selection == Some(window_id) {
                    column.tab_selection = column.ordered_window_ids.first().copied();
                }
                if column.active_window_id == Some(window_id) {
                    column.active_window_id = column
                        .tab_selection
                        .or_else(|| column.ordered_window_ids.first().copied());
                }
                column_is_empty = column.ordered_window_ids.is_empty();
            }

            if column_is_empty {
                self.state.layout.columns.remove(&column_id);
                let workspace = self
                    .state
                    .workspaces
                    .get_mut(&window.workspace_id)
                    .ok_or(CoreError::UnknownWorkspace(window.workspace_id))?;
                workspace
                    .strip
                    .ordered_column_ids
                    .retain(|candidate_column_id| *candidate_column_id != column_id);
            }
        } else {
            let workspace = self
                .state
                .workspaces
                .get_mut(&window.workspace_id)
                .ok_or(CoreError::UnknownWorkspace(window.workspace_id))?;
            workspace
                .floating_layer
                .ordered_window_ids
                .retain(|candidate_window_id| *candidate_window_id != window_id);
            workspace.floating_layer.z_hints.remove(&window_id);
        }

        Ok(())
    }

    fn restore_window_to_target(
        &mut self,
        window_id: WindowId,
        restore_target: RestoreTarget,
        fallback_mode: ColumnMode,
        fallback_width: WidthSemantics,
    ) -> Result<Option<ColumnId>, CoreError> {
        match restore_target.layer {
            WindowLayer::Floating => {
                self.push_window_to_floating_layer(restore_target.workspace_id, window_id)?;
                Ok(None)
            }
            _ => {
                if let Some(column_id) = restore_target.column_id {
                    let workspace_contains_column = self
                        .state
                        .workspaces
                        .get(&restore_target.workspace_id)
                        .is_some_and(|workspace| {
                            workspace.strip.ordered_column_ids.contains(&column_id)
                        });
                    if workspace_contains_column {
                        let column = self
                            .state
                            .layout
                            .columns
                            .get_mut(&column_id)
                            .ok_or(CoreError::UnknownColumn(column_id))?;
                        if !column.ordered_window_ids.contains(&window_id) {
                            column.ordered_window_ids.push(window_id);
                        }
                        if column.active_window_id.is_none() {
                            column.active_window_id = Some(window_id);
                        }
                        if column.mode == ColumnMode::Tabbed {
                            column.tab_selection = Some(window_id);
                        }
                        return Ok(Some(column_id));
                    }
                }

                let new_column_id = self.insert_new_column(
                    restore_target.workspace_id,
                    window_id,
                    NewColumnRequest {
                        anchor_column_id: None,
                        insert_index_override: restore_target.column_index,
                        before_anchor: false,
                        mode: fallback_mode,
                        width_semantics: fallback_width,
                        preserve_focus_position: false,
                    },
                )?;
                Ok(Some(new_column_id))
            }
        }
    }

    fn navigation_sequence_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(WindowId, Option<ColumnId>)>, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let mut sequence = Vec::new();

        for column_id in &workspace.strip.ordered_column_ids {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            if let Some(window_id) = self.column_active_window(column) {
                sequence.push((window_id, Some(*column_id)));
            }
        }

        if sequence.is_empty() {
            sequence.extend(
                workspace
                    .floating_layer
                    .ordered_window_ids
                    .iter()
                    .copied()
                    .map(|window_id| (window_id, None)),
            );
        }

        Ok(sequence)
    }

    fn try_cycle_tabbed_focus(
        &mut self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
        forward: bool,
    ) -> Result<bool, CoreError> {
        let (ordered_window_ids, current_index) = {
            let column = self
                .state
                .layout
                .columns
                .get(&column_id)
                .ok_or(CoreError::UnknownColumn(column_id))?;
            if column.mode != ColumnMode::Tabbed || column.ordered_window_ids.len() < 2 {
                return Ok(false);
            }

            let current_window_id = column
                .tab_selection
                .or(column.active_window_id)
                .or(self.focused_window_in_workspace(workspace_id))
                .or_else(|| column.ordered_window_ids.first().copied())
                .ok_or(CoreError::InvalidEvent(
                    "tabbed column is missing a selected window",
                ))?;
            let current_index = column
                .ordered_window_ids
                .iter()
                .position(|candidate_window_id| *candidate_window_id == current_window_id)
                .unwrap_or(0);
            (column.ordered_window_ids.clone(), current_index)
        };

        let next_index = if forward {
            (current_index + 1) % ordered_window_ids.len()
        } else if current_index == 0 {
            ordered_window_ids.len() - 1
        } else {
            current_index - 1
        };
        let window_id = ordered_window_ids[next_index];
        let monitor_id = self
            .state
            .workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.monitor_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;

        let column = self
            .state
            .layout
            .columns
            .get_mut(&column_id)
            .ok_or(CoreError::UnknownColumn(column_id))?;
        column.tab_selection = Some(window_id);
        self.set_focus_to_window(
            monitor_id,
            workspace_id,
            window_id,
            Some(column_id),
            FocusOrigin::UserCommand,
        )?;
        self.reveal_column_in_workspace(workspace_id, column_id, Some(column_id))?;
        Ok(true)
    }

    fn set_focus_to_window(
        &mut self,
        monitor_id: MonitorId,
        workspace_id: WorkspaceId,
        window_id: WindowId,
        column_id: Option<ColumnId>,
        origin: FocusOrigin,
    ) -> Result<(), CoreError> {
        self.state.focus.focused_monitor_id = Some(monitor_id);
        self.state.focus.focused_window_id = Some(window_id);
        self.state.focus.focused_column_id = column_id;
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

        if let Some(column_id) = column_id
            && let Some(column) = self.state.layout.columns.get_mut(&column_id)
        {
            column.active_window_id = Some(window_id);
            if column.mode == ColumnMode::Tabbed {
                column.tab_selection = Some(window_id);
            }
        }

        Ok(())
    }

    fn apply_scroll_delta(
        &mut self,
        workspace_id: WorkspaceId,
        delta: i32,
    ) -> Result<(), CoreError> {
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        workspace.strip.scroll_offset = workspace
            .strip
            .scroll_offset
            .saturating_add(delta)
            .clamp(0, max_scroll_offset);
        Ok(())
    }

    fn clamp_scroll_offset(&mut self, workspace_id: WorkspaceId) -> Result<(), CoreError> {
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let workspace = self
            .state
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        workspace.strip.scroll_offset = workspace.strip.scroll_offset.clamp(0, max_scroll_offset);
        Ok(())
    }

    fn max_scroll_offset(&self, workspace_id: WorkspaceId) -> Result<i32, CoreError> {
        let viewport_width = self
            .workspace_tiled_viewport(workspace_id)?
            .width
            .min(i32::MAX as u32) as i32;
        let content_width = self.workspace_content_width(workspace_id)?;
        Ok((content_width - viewport_width).max(0))
    }

    fn workspace_content_width(&self, workspace_id: WorkspaceId) -> Result<i32, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let viewport = self.workspace_tiled_viewport(workspace_id)?;
        let mut total_width = 0_i32;
        let column_gap = self.state.config_projection.layout_spacing.column_gap;

        for (index, column_id) in workspace.strip.ordered_column_ids.iter().enumerate() {
            let column = self
                .state
                .layout
                .columns
                .get(column_id)
                .ok_or(CoreError::UnknownColumn(*column_id))?;
            let column_width = self
                .resolve_column_width(column, viewport.width)
                .min(i32::MAX as u32) as i32;
            total_width = total_width.saturating_add(column_width);
            if index > 0 {
                total_width = total_width.saturating_add(column_gap.min(i32::MAX as u32) as i32);
            }
        }

        Ok(total_width)
    }

    fn reveal_column_in_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        target_column_id: ColumnId,
        _previous_column_id: Option<ColumnId>,
    ) -> Result<(), CoreError> {
        let projection = recompute_workspace(&self.state, workspace_id)?;
        let viewport_left = projection.viewport.x;
        let viewport_width = projection.viewport.width.min(i32::MAX as u32) as i32;
        let viewport_right = projection
            .viewport
            .x
            .saturating_add(projection.viewport.width.min(i32::MAX as u32) as i32);
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let is_single_column_workspace = workspace.strip.ordered_column_ids.len() == 1;
        let target_bounds = projection
            .window_geometries
            .iter()
            .filter(|geometry| {
                self.state
                    .windows
                    .get(&geometry.window_id)
                    .is_some_and(|window| window.column_id == Some(target_column_id))
            })
            .fold(None, |acc: Option<(i32, i32)>, geometry| {
                let left = geometry.rect.x;
                let right = geometry
                    .rect
                    .x
                    .saturating_add(geometry.rect.width.min(i32::MAX as u32) as i32);
                Some(match acc {
                    Some((current_left, current_right)) => {
                        (current_left.min(left), current_right.max(right))
                    }
                    None => (left, right),
                })
            });
        let Some((column_left, column_right)) = target_bounds else {
            return Ok(());
        };
        let column_width = column_right.saturating_sub(column_left);
        let visible_left = viewport_left;
        let visible_right = viewport_right;
        let should_center_target = column_width < viewport_width && is_single_column_workspace;
        let max_scroll_offset = self.max_scroll_offset(workspace_id)?;
        let desired_scroll_offset = if should_center_target {
            column_left
                .saturating_add(column_width / 2)
                .saturating_sub(projection.viewport.width.min(i32::MAX as u32) as i32 / 2)
                .clamp(0, max_scroll_offset)
        } else if column_left < visible_left {
            workspace
                .strip
                .scroll_offset
                .saturating_add(column_left.saturating_sub(visible_left))
        } else if column_right > visible_right {
            workspace
                .strip
                .scroll_offset
                .saturating_add(column_right.saturating_sub(visible_right))
        } else {
            workspace.strip.scroll_offset
        };
        let desired_scroll_offset = desired_scroll_offset.clamp(0, max_scroll_offset);
        self.apply_scroll_delta(
            workspace_id,
            desired_scroll_offset.saturating_sub(workspace.strip.scroll_offset),
        )?;

        Ok(())
    }

    fn resolve_column_width(&self, column: &Column, monitor_width: u32) -> u32 {
        if column.maximized_state == MaximizedState::Maximized
            || column.mode == ColumnMode::MaximizedColumn
        {
            monitor_width.max(1)
        } else {
            column.width_semantics.resolve(monitor_width)
        }
    }

    fn workspace_tiled_viewport(&self, workspace_id: WorkspaceId) -> Result<Rect, CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let monitor = self
            .state
            .monitors
            .get(&workspace.monitor_id)
            .ok_or(CoreError::UnknownMonitor(workspace.monitor_id))?;
        Ok(padded_tiled_viewport(
            monitor.work_area_rect,
            &self.state.config_projection,
        ))
    }

    fn retarget_focus_after_destroy(
        &mut self,
        workspace_id: WorkspaceId,
        preferred_column_id: Option<ColumnId>,
    ) -> Result<(), CoreError> {
        let workspace = self
            .state
            .workspaces
            .get(&workspace_id)
            .ok_or(CoreError::UnknownWorkspace(workspace_id))?;
        let monitor_id = workspace.monitor_id;
        let next_focus = preferred_column_id
            .and_then(|column_id| {
                let column = self.state.layout.columns.get(&column_id)?;
                self.column_active_window(column)
                    .map(|window_id| (window_id, Some(column_id)))
            })
            .or_else(|| {
                workspace
                    .strip
                    .ordered_column_ids
                    .iter()
                    .find_map(|column_id| {
                        if Some(*column_id) == preferred_column_id {
                            return None;
                        }
                        let column = self.state.layout.columns.get(column_id)?;
                        self.column_active_window(column)
                            .map(|window_id| (window_id, Some(*column_id)))
                    })
            })
            .or_else(|| {
                workspace
                    .floating_layer
                    .ordered_window_ids
                    .first()
                    .copied()
                    .map(|window_id| (window_id, None))
            });

        if let Some((window_id, column_id)) = next_focus {
            self.set_focus_to_window(
                monitor_id,
                workspace_id,
                window_id,
                column_id,
                FocusOrigin::ReducerDefault,
            )?;
            if let Some(column_id) = column_id {
                self.reveal_column_in_workspace(workspace_id, column_id, preferred_column_id)?;
            }
        } else {
            self.state.focus.focused_monitor_id = Some(monitor_id);
            self.state.focus.focus_origin = FocusOrigin::ReducerDefault;
            self.state.focus.focused_window_id = None;
            self.state.focus.focused_column_id = None;
        }

        Ok(())
    }
}
