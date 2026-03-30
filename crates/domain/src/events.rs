use crate::{
    ColumnId, ColumnMode, ConfigProjection, CorrelationId, FocusOrigin, MonitorId, Rect,
    ResizeEdge, Size, StateVersion, WidthSemantics, WindowClassification, WindowId, WindowLayer,
    WorkspaceId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCategory {
    PlatformDerived,
    UserInputDerived,
    ConfigRulesDerived,
    IpcDerived,
    SystemRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventSource {
    WindowsAdapter,
    InputCommand,
    ConfigRules,
    IpcClient,
    WmCore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainEventName {
    WindowDiscovered,
    WindowDestroyed,
    WindowShown,
    WindowHidden,
    WindowLocationChanged,
    WindowFocusObserved,
    MonitorTopologyChanged,
    SystemSuspend,
    SystemResume,
    ExplorerRestartObserved,
    CmdFocusNext,
    CmdFocusPrev,
    CmdFocusWorkspaceUp,
    CmdFocusWorkspaceDown,
    CmdScrollStripLeft,
    CmdScrollStripRight,
    CmdMoveWorkspaceUp,
    CmdMoveWorkspaceDown,
    CmdMoveWorkspaceToMonitorNext,
    CmdMoveWorkspaceToMonitorPrevious,
    CmdMoveColumnToWorkspaceUp,
    CmdMoveColumnToWorkspaceDown,
    CmdMoveColumnToWorkspaceTarget,
    CmdMoveWindow,
    CmdToggleFloating,
    CmdToggleTabbed,
    CmdToggleMaximized,
    CmdToggleFullscreen,
    CmdOpenOverview,
    CmdCloseOverview,
    CmdToggleOverview,
    CmdBeginColumnWidthResize,
    CmdUpdateColumnWidthPreview,
    CmdCommitColumnWidth,
    CmdCancelColumnWidthResize,
    CmdCycleColumnWidth,
    CmdEmergencyUnwind,
    ConfigReloadRequested,
    ConfigReloadSucceeded,
    ConfigReloadFailed,
    RulesUpdated,
    IpcCommandReceived,
    IpcSnapshotRequested,
    IpcClientConnected,
    IpcClientDisconnected,
    ReconcileRequested,
    FullScanRequested,
    DesyncDetected,
    UiHostCrashed,
    CaptureModuleCrashed,
}

impl DomainEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WindowDiscovered => "EVT-WINDOW-DISCOVERED",
            Self::WindowDestroyed => "EVT-WINDOW-DESTROYED",
            Self::WindowShown => "EVT-WINDOW-SHOWN",
            Self::WindowHidden => "EVT-WINDOW-HIDDEN",
            Self::WindowLocationChanged => "EVT-WINDOW-LOCATION-CHANGED",
            Self::WindowFocusObserved => "EVT-WINDOW-FOCUS-OBSERVED",
            Self::MonitorTopologyChanged => "EVT-MONITOR-TOPOLOGY-CHANGED",
            Self::SystemSuspend => "EVT-SYSTEM-SUSPEND",
            Self::SystemResume => "EVT-SYSTEM-RESUME",
            Self::ExplorerRestartObserved => "EVT-EXPLORER-RESTART-OBSERVED",
            Self::CmdFocusNext => "EVT-CMD-FOCUS-NEXT",
            Self::CmdFocusPrev => "EVT-CMD-FOCUS-PREV",
            Self::CmdFocusWorkspaceUp => "EVT-CMD-FOCUS-WORKSPACE-UP",
            Self::CmdFocusWorkspaceDown => "EVT-CMD-FOCUS-WORKSPACE-DOWN",
            Self::CmdScrollStripLeft => "EVT-CMD-SCROLL-STRIP-LEFT",
            Self::CmdScrollStripRight => "EVT-CMD-SCROLL-STRIP-RIGHT",
            Self::CmdMoveWorkspaceUp => "EVT-CMD-MOVE-WORKSPACE-UP",
            Self::CmdMoveWorkspaceDown => "EVT-CMD-MOVE-WORKSPACE-DOWN",
            Self::CmdMoveWorkspaceToMonitorNext => "EVT-CMD-MOVE-WORKSPACE-TO-MONITOR-NEXT",
            Self::CmdMoveWorkspaceToMonitorPrevious => "EVT-CMD-MOVE-WORKSPACE-TO-MONITOR-PREVIOUS",
            Self::CmdMoveColumnToWorkspaceUp => "EVT-CMD-MOVE-COLUMN-TO-WORKSPACE-UP",
            Self::CmdMoveColumnToWorkspaceDown => "EVT-CMD-MOVE-COLUMN-TO-WORKSPACE-DOWN",
            Self::CmdMoveColumnToWorkspaceTarget => "EVT-CMD-MOVE-COLUMN-TO-WORKSPACE-TARGET",
            Self::CmdMoveWindow => "EVT-CMD-MOVE-WINDOW",
            Self::CmdToggleFloating => "EVT-CMD-TOGGLE-FLOATING",
            Self::CmdToggleTabbed => "EVT-CMD-TOGGLE-TABBED",
            Self::CmdToggleMaximized => "EVT-CMD-TOGGLE-MAXIMIZED",
            Self::CmdToggleFullscreen => "EVT-CMD-TOGGLE-FULLSCREEN",
            Self::CmdOpenOverview => "EVT-CMD-OPEN-OVERVIEW",
            Self::CmdCloseOverview => "EVT-CMD-CLOSE-OVERVIEW",
            Self::CmdToggleOverview => "EVT-CMD-TOGGLE-OVERVIEW",
            Self::CmdBeginColumnWidthResize => "EVT-CMD-BEGIN-COLUMN-WIDTH-RESIZE",
            Self::CmdUpdateColumnWidthPreview => "EVT-CMD-UPDATE-COLUMN-WIDTH-PREVIEW",
            Self::CmdCommitColumnWidth => "EVT-CMD-COMMIT-COLUMN-WIDTH",
            Self::CmdCancelColumnWidthResize => "EVT-CMD-CANCEL-COLUMN-WIDTH-RESIZE",
            Self::CmdCycleColumnWidth => "EVT-CMD-CYCLE-COLUMN-WIDTH",
            Self::CmdEmergencyUnwind => "EVT-CMD-EMERGENCY-UNWIND",
            Self::ConfigReloadRequested => "EVT-CONFIG-RELOAD-REQUESTED",
            Self::ConfigReloadSucceeded => "EVT-CONFIG-RELOAD-SUCCEEDED",
            Self::ConfigReloadFailed => "EVT-CONFIG-RELOAD-FAILED",
            Self::RulesUpdated => "EVT-RULES-UPDATED",
            Self::IpcCommandReceived => "EVT-IPC-COMMAND-RECEIVED",
            Self::IpcSnapshotRequested => "EVT-IPC-SNAPSHOT-REQUESTED",
            Self::IpcClientConnected => "EVT-IPC-CLIENT-CONNECTED",
            Self::IpcClientDisconnected => "EVT-IPC-CLIENT-DISCONNECTED",
            Self::ReconcileRequested => "EVT-RECONCILE-REQUESTED",
            Self::FullScanRequested => "EVT-FULL-SCAN-REQUESTED",
            Self::DesyncDetected => "EVT-DESYNC-DETECTED",
            Self::UiHostCrashed => "EVT-UI-HOST-CRASHED",
            Self::CaptureModuleCrashed => "EVT-CAPTURE-MODULE-CRASHED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusBehavior {
    FollowNewWindow,
    PreserveCurrentFocus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowPlacement {
    AppendToFocusedColumn,
    NewColumnAfterFocus {
        mode: ColumnMode,
        width: WidthSemantics,
    },
    NewColumnBeforeFocus {
        mode: ColumnMode,
        width: WidthSemantics,
    },
    AppendToWorkspaceEnd {
        mode: ColumnMode,
        width: WidthSemantics,
    },
}

impl Default for WindowPlacement {
    fn default() -> Self {
        Self::NewColumnAfterFocus {
            mode: ColumnMode::Normal,
            width: WidthSemantics::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NavigationScope {
    #[default]
    WorkspaceStrip,
    ColumnTabs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowDiscoveredPayload {
    pub monitor_id: MonitorId,
    pub hwnd: u64,
    pub classification: WindowClassification,
    pub desired_size: Size,
    pub last_known_rect: Rect,
    pub placement: WindowPlacement,
    pub focus_behavior: FocusBehavior,
    pub layer: WindowLayer,
    pub managed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowDestroyedPayload {
    pub window_id: WindowId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowFocusObservedPayload {
    pub monitor_id: MonitorId,
    pub window_id: WindowId,
    pub focus_origin: FocusOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusCommandPayload {
    pub scope: NavigationScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StripScrollPayload {
    pub scope: NavigationScope,
    pub step: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowCommandPayload {
    pub window_id: Option<WindowId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceCommandPayload {
    pub monitor_id: Option<MonitorId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveColumnToWorkspaceTargetPayload {
    pub source_column_id: ColumnId,
    pub target_workspace_id: WorkspaceId,
    pub insert_after_column_id: Option<ColumnId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverviewCommandPayload {
    pub monitor_id: Option<MonitorId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnWidthResizePayload {
    pub edge: ResizeEdge,
    pub pointer_x: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnWidthPointerPayload {
    pub pointer_x: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigReloadRequestedPayload {
    pub source: EventSource,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigReloadSucceededPayload {
    pub config_generation: u64,
    pub changed_sections: Vec<String>,
    pub projection: ConfigProjection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigReloadFailedPayload {
    pub error_code: String,
    pub human_message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RulesUpdatedPayload {
    pub rule_set_version: u64,
    pub changed_rules: Vec<String>,
    pub active_rule_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DomainEventPayload {
    WindowDiscovered(WindowDiscoveredPayload),
    WindowDestroyed(WindowDestroyedPayload),
    WindowFocusObserved(WindowFocusObservedPayload),
    CmdFocusNext(FocusCommandPayload),
    CmdFocusPrev(FocusCommandPayload),
    CmdFocusWorkspaceUp(WorkspaceCommandPayload),
    CmdFocusWorkspaceDown(WorkspaceCommandPayload),
    CmdScrollStripLeft(StripScrollPayload),
    CmdScrollStripRight(StripScrollPayload),
    CmdMoveWorkspaceUp(WorkspaceCommandPayload),
    CmdMoveWorkspaceDown(WorkspaceCommandPayload),
    CmdMoveWorkspaceToMonitorNext(WorkspaceCommandPayload),
    CmdMoveWorkspaceToMonitorPrevious(WorkspaceCommandPayload),
    CmdMoveColumnToWorkspaceUp(WorkspaceCommandPayload),
    CmdMoveColumnToWorkspaceDown(WorkspaceCommandPayload),
    CmdMoveColumnToWorkspaceTarget(MoveColumnToWorkspaceTargetPayload),
    CmdToggleFloating(WindowCommandPayload),
    CmdToggleTabbed(WindowCommandPayload),
    CmdToggleMaximized(WindowCommandPayload),
    CmdToggleFullscreen(WindowCommandPayload),
    CmdOpenOverview(OverviewCommandPayload),
    CmdCloseOverview(OverviewCommandPayload),
    CmdToggleOverview(OverviewCommandPayload),
    CmdBeginColumnWidthResize(ColumnWidthResizePayload),
    CmdUpdateColumnWidthPreview(ColumnWidthPointerPayload),
    CmdCommitColumnWidth(ColumnWidthPointerPayload),
    CmdCancelColumnWidthResize,
    CmdCycleColumnWidth,
    ConfigReloadRequested(ConfigReloadRequestedPayload),
    ConfigReloadSucceeded(ConfigReloadSucceededPayload),
    ConfigReloadFailed(ConfigReloadFailedPayload),
    RulesUpdated(RulesUpdatedPayload),
    ReconcileRequested,
    FullScanRequested,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainEvent {
    pub event_id: DomainEventName,
    pub event_kind: EventCategory,
    pub timestamp: u64,
    pub source: EventSource,
    pub correlation_id: CorrelationId,
    pub payload: DomainEventPayload,
    pub state_version_at_enqueue: Option<StateVersion>,
}

impl DomainEvent {
    pub const fn new(
        event_id: DomainEventName,
        event_kind: EventCategory,
        source: EventSource,
        correlation_id: CorrelationId,
        payload: DomainEventPayload,
    ) -> Self {
        Self {
            event_id,
            event_kind,
            timestamp: 0,
            source,
            correlation_id,
            payload,
            state_version_at_enqueue: None,
        }
    }

    pub fn window_discovered(
        correlation_id: CorrelationId,
        monitor_id: MonitorId,
        hwnd: u64,
        desired_size: Size,
        last_known_rect: Rect,
    ) -> Self {
        Self::window_discovered_with(
            correlation_id,
            monitor_id,
            hwnd,
            desired_size,
            last_known_rect,
            WindowPlacement::default(),
            FocusBehavior::FollowNewWindow,
        )
    }

    pub fn window_discovered_with(
        correlation_id: CorrelationId,
        monitor_id: MonitorId,
        hwnd: u64,
        desired_size: Size,
        last_known_rect: Rect,
        placement: WindowPlacement,
        focus_behavior: FocusBehavior,
    ) -> Self {
        Self::new(
            DomainEventName::WindowDiscovered,
            EventCategory::PlatformDerived,
            EventSource::WindowsAdapter,
            correlation_id,
            DomainEventPayload::WindowDiscovered(WindowDiscoveredPayload {
                monitor_id,
                hwnd,
                classification: WindowClassification::Application,
                desired_size,
                last_known_rect,
                placement,
                focus_behavior,
                layer: WindowLayer::Tiled,
                managed: true,
            }),
        )
    }

    pub fn window_destroyed(correlation_id: CorrelationId, window_id: WindowId) -> Self {
        Self::new(
            DomainEventName::WindowDestroyed,
            EventCategory::PlatformDerived,
            EventSource::WindowsAdapter,
            correlation_id,
            DomainEventPayload::WindowDestroyed(WindowDestroyedPayload { window_id }),
        )
    }

    pub fn window_focus_observed(
        correlation_id: CorrelationId,
        monitor_id: MonitorId,
        window_id: WindowId,
    ) -> Self {
        Self::new(
            DomainEventName::WindowFocusObserved,
            EventCategory::PlatformDerived,
            EventSource::WindowsAdapter,
            correlation_id,
            DomainEventPayload::WindowFocusObserved(WindowFocusObservedPayload {
                monitor_id,
                window_id,
                focus_origin: FocusOrigin::PlatformObservation,
            }),
        )
    }

    pub fn focus_next(correlation_id: CorrelationId, scope: NavigationScope) -> Self {
        Self::new(
            DomainEventName::CmdFocusNext,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdFocusNext(FocusCommandPayload { scope }),
        )
    }

    pub fn focus_prev(correlation_id: CorrelationId, scope: NavigationScope) -> Self {
        Self::new(
            DomainEventName::CmdFocusPrev,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdFocusPrev(FocusCommandPayload { scope }),
        )
    }

    pub fn focus_workspace_up(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdFocusWorkspaceUp,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdFocusWorkspaceUp(WorkspaceCommandPayload { monitor_id }),
        )
    }

    pub fn focus_workspace_down(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdFocusWorkspaceDown,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdFocusWorkspaceDown(WorkspaceCommandPayload { monitor_id }),
        )
    }

    pub fn scroll_strip_left(
        correlation_id: CorrelationId,
        scope: NavigationScope,
        step: u32,
    ) -> Self {
        Self::new(
            DomainEventName::CmdScrollStripLeft,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdScrollStripLeft(StripScrollPayload { scope, step }),
        )
    }

    pub fn scroll_strip_right(
        correlation_id: CorrelationId,
        scope: NavigationScope,
        step: u32,
    ) -> Self {
        Self::new(
            DomainEventName::CmdScrollStripRight,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdScrollStripRight(StripScrollPayload { scope, step }),
        )
    }

    pub fn move_workspace_up(correlation_id: CorrelationId, monitor_id: Option<MonitorId>) -> Self {
        Self::new(
            DomainEventName::CmdMoveWorkspaceUp,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveWorkspaceUp(WorkspaceCommandPayload { monitor_id }),
        )
    }

    pub fn move_workspace_down(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveWorkspaceDown,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveWorkspaceDown(WorkspaceCommandPayload { monitor_id }),
        )
    }

    pub fn move_workspace_to_monitor_next(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveWorkspaceToMonitorNext,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveWorkspaceToMonitorNext(WorkspaceCommandPayload {
                monitor_id,
            }),
        )
    }

    pub fn move_workspace_to_monitor_previous(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveWorkspaceToMonitorPrevious,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveWorkspaceToMonitorPrevious(WorkspaceCommandPayload {
                monitor_id,
            }),
        )
    }

    pub fn move_column_to_workspace_up(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveColumnToWorkspaceUp,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveColumnToWorkspaceUp(WorkspaceCommandPayload { monitor_id }),
        )
    }

    pub fn move_column_to_workspace_down(
        correlation_id: CorrelationId,
        monitor_id: Option<MonitorId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveColumnToWorkspaceDown,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveColumnToWorkspaceDown(WorkspaceCommandPayload {
                monitor_id,
            }),
        )
    }

    pub fn move_column_to_workspace_target(
        correlation_id: CorrelationId,
        source_column_id: ColumnId,
        target_workspace_id: WorkspaceId,
        insert_after_column_id: Option<ColumnId>,
    ) -> Self {
        Self::new(
            DomainEventName::CmdMoveColumnToWorkspaceTarget,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdMoveColumnToWorkspaceTarget(
                MoveColumnToWorkspaceTargetPayload {
                    source_column_id,
                    target_workspace_id,
                    insert_after_column_id,
                },
            ),
        )
    }

    pub fn toggle_floating(correlation_id: CorrelationId, window_id: Option<WindowId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleFloating,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleFloating(WindowCommandPayload { window_id }),
        )
    }

    pub fn toggle_tabbed(correlation_id: CorrelationId, window_id: Option<WindowId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleTabbed,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleTabbed(WindowCommandPayload { window_id }),
        )
    }

    pub fn toggle_maximized(correlation_id: CorrelationId, window_id: Option<WindowId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleMaximized,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleMaximized(WindowCommandPayload { window_id }),
        )
    }

    pub fn toggle_fullscreen(correlation_id: CorrelationId, window_id: Option<WindowId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleFullscreen,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleFullscreen(WindowCommandPayload { window_id }),
        )
    }

    pub fn open_overview(correlation_id: CorrelationId, monitor_id: Option<MonitorId>) -> Self {
        Self::new(
            DomainEventName::CmdOpenOverview,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdOpenOverview(OverviewCommandPayload { monitor_id }),
        )
    }

    pub fn close_overview(correlation_id: CorrelationId, monitor_id: Option<MonitorId>) -> Self {
        Self::new(
            DomainEventName::CmdCloseOverview,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdCloseOverview(OverviewCommandPayload { monitor_id }),
        )
    }

    pub fn toggle_overview(correlation_id: CorrelationId, monitor_id: Option<MonitorId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleOverview,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleOverview(OverviewCommandPayload { monitor_id }),
        )
    }

    pub fn begin_column_width_resize(
        correlation_id: CorrelationId,
        edge: ResizeEdge,
        pointer_x: i32,
    ) -> Self {
        Self::new(
            DomainEventName::CmdBeginColumnWidthResize,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdBeginColumnWidthResize(ColumnWidthResizePayload {
                edge,
                pointer_x,
            }),
        )
    }

    pub fn update_column_width_preview(correlation_id: CorrelationId, pointer_x: i32) -> Self {
        Self::new(
            DomainEventName::CmdUpdateColumnWidthPreview,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdUpdateColumnWidthPreview(ColumnWidthPointerPayload {
                pointer_x,
            }),
        )
    }

    pub fn commit_column_width(correlation_id: CorrelationId, pointer_x: i32) -> Self {
        Self::new(
            DomainEventName::CmdCommitColumnWidth,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdCommitColumnWidth(ColumnWidthPointerPayload { pointer_x }),
        )
    }

    pub fn cancel_column_width_resize(correlation_id: CorrelationId) -> Self {
        Self::new(
            DomainEventName::CmdCancelColumnWidthResize,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdCancelColumnWidthResize,
        )
    }

    pub fn cycle_column_width(correlation_id: CorrelationId) -> Self {
        Self::new(
            DomainEventName::CmdCycleColumnWidth,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdCycleColumnWidth,
        )
    }

    pub fn config_reload_requested(
        correlation_id: CorrelationId,
        source: EventSource,
        path: Option<String>,
    ) -> Self {
        Self::new(
            DomainEventName::ConfigReloadRequested,
            EventCategory::ConfigRulesDerived,
            source,
            correlation_id,
            DomainEventPayload::ConfigReloadRequested(ConfigReloadRequestedPayload {
                source,
                path,
            }),
        )
    }

    pub fn config_reload_succeeded(
        correlation_id: CorrelationId,
        config_generation: u64,
        changed_sections: Vec<String>,
        projection: ConfigProjection,
    ) -> Self {
        Self::new(
            DomainEventName::ConfigReloadSucceeded,
            EventCategory::ConfigRulesDerived,
            EventSource::ConfigRules,
            correlation_id,
            DomainEventPayload::ConfigReloadSucceeded(ConfigReloadSucceededPayload {
                config_generation,
                changed_sections,
                projection,
            }),
        )
    }

    pub fn config_reload_failed(
        correlation_id: CorrelationId,
        error_code: impl Into<String>,
        human_message: impl Into<String>,
    ) -> Self {
        Self::new(
            DomainEventName::ConfigReloadFailed,
            EventCategory::ConfigRulesDerived,
            EventSource::ConfigRules,
            correlation_id,
            DomainEventPayload::ConfigReloadFailed(ConfigReloadFailedPayload {
                error_code: error_code.into(),
                human_message: human_message.into(),
            }),
        )
    }

    pub fn rules_updated(
        correlation_id: CorrelationId,
        rule_set_version: u64,
        changed_rules: Vec<String>,
        active_rule_count: usize,
    ) -> Self {
        Self::new(
            DomainEventName::RulesUpdated,
            EventCategory::ConfigRulesDerived,
            EventSource::ConfigRules,
            correlation_id,
            DomainEventPayload::RulesUpdated(RulesUpdatedPayload {
                rule_set_version,
                changed_rules,
                active_rule_count,
            }),
        )
    }
}
