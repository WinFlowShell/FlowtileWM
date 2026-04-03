#![forbid(unsafe_code)]

use core::fmt;

mod events;
mod geometry;
mod ids;
mod model;

pub use events::{
    ColumnWidthPointerPayload, ColumnWidthResizePayload, ConfigReloadFailedPayload,
    ConfigReloadRequestedPayload, ConfigReloadSucceededPayload, DomainEvent, DomainEventName,
    DomainEventPayload, EventCategory, EventSource, FocusBehavior, FocusCommandPayload,
    MoveColumnToWorkspaceTargetPayload, NavigationScope, OverviewCommandPayload,
    RulesUpdatedPayload, StripScrollPayload, WindowCommandPayload, WindowDestroyedPayload,
    WindowDiscoveredPayload, WindowFocusObservedPayload, WindowPlacement, WorkspaceCommandPayload,
};
pub use geometry::{Point, Rect, Size};
pub use ids::{ColumnId, CorrelationId, MonitorId, WindowId, WorkspaceId, WorkspaceSetId};
pub use model::{
    BindControlMode, CapturePolicy, Column, ColumnMode, ConfigProjection, DiagnosticsSummary,
    EdgeInsets, FloatingLayer, FocusOrigin, FocusState, LayoutSpacing, LayoutState, MaximizedState,
    Monitor, OverviewState, ResizeEdge, RestoreTarget, RuntimeMode, RuntimeState, ScrollingStrip,
    StripLayoutMode, TopologyRole, WidthResizeSession, WidthSemantics, WindowClassification,
    WindowLayer, WindowNode, WmState, Workspace, WorkspaceSet, all_column_modes,
};

pub const VERSION_LINE: &str = "v.0.0.23";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct StateVersion(u64);

impl StateVersion {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapProfile {
    pub runtime_mode: RuntimeMode,
    pub state_version: StateVersion,
    pub version_line: &'static str,
}

impl BootstrapProfile {
    pub const fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            runtime_mode,
            state_version: StateVersion::new(0),
            version_line: VERSION_LINE,
        }
    }

    pub const fn from_state(runtime_mode: RuntimeMode, state_version: StateVersion) -> Self {
        Self {
            runtime_mode,
            state_version,
            version_line: VERSION_LINE,
        }
    }
}

impl fmt::Display for StateVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.get())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BindControlMode, BootstrapProfile, Rect, RuntimeMode, Size, StateVersion, VERSION_LINE,
        WindowLayer, WindowNode, WmState,
    };

    #[test]
    fn parses_known_runtime_mode() {
        assert_eq!(
            RuntimeMode::parse("extended-shell"),
            Some(RuntimeMode::ExtendedShell)
        );
    }

    #[test]
    fn parses_known_bind_control_mode() {
        assert_eq!(
            BindControlMode::parse("coexistence"),
            Some(BindControlMode::Coexistence)
        );
    }

    #[test]
    fn builds_bootstrap_profile() {
        let profile = BootstrapProfile::new(RuntimeMode::WmOnly);
        assert_eq!(profile.runtime_mode, RuntimeMode::WmOnly);
        assert_eq!(profile.state_version, StateVersion::new(0));
        assert_eq!(profile.version_line, VERSION_LINE);
    }

    #[test]
    fn monitor_bootstrap_starts_with_single_empty_workspace() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1920, 1080), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("monitor should have an active workspace");

        assert!(state.is_workspace_empty(workspace_id));
        assert_eq!(state.workspaces.len(), 1);
    }

    #[test]
    fn floating_layer_is_not_treated_as_tiled_column_membership() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        let window_id = state.allocate_window_id();
        state.windows.insert(
            window_id,
            WindowNode {
                id: window_id,
                current_hwnd_binding: Some(42),
                classification: super::WindowClassification::Application,
                layer: WindowLayer::Floating,
                workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: true,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(40, 40, 600, 400),
                desired_size: Size::new(600, 400),
            },
        );
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .floating_layer
            .ordered_window_ids
            .push(window_id);

        assert!(!state.is_workspace_empty(workspace_id));
        assert_eq!(
            state
                .workspaces
                .get(&workspace_id)
                .expect("workspace should exist")
                .strip
                .ordered_column_ids
                .len(),
            0
        );
    }
}
