use crate::{
    ColumnMode, ConfigProjection, CorrelationId, FocusOrigin, MonitorId, Rect, Size, StateVersion,
    WidthSemantics, WindowClassification, WindowId, WindowLayer,
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
    CmdScrollStripLeft,
    CmdScrollStripRight,
    CmdMoveWindow,
    CmdToggleFloating,
    CmdToggleTabbed,
    CmdToggleMaximized,
    CmdToggleFullscreen,
    CmdToggleOverview,
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
            Self::CmdScrollStripLeft => "EVT-CMD-SCROLL-STRIP-LEFT",
            Self::CmdScrollStripRight => "EVT-CMD-SCROLL-STRIP-RIGHT",
            Self::CmdMoveWindow => "EVT-CMD-MOVE-WINDOW",
            Self::CmdToggleFloating => "EVT-CMD-TOGGLE-FLOATING",
            Self::CmdToggleTabbed => "EVT-CMD-TOGGLE-TABBED",
            Self::CmdToggleMaximized => "EVT-CMD-TOGGLE-MAXIMIZED",
            Self::CmdToggleFullscreen => "EVT-CMD-TOGGLE-FULLSCREEN",
            Self::CmdToggleOverview => "EVT-CMD-TOGGLE-OVERVIEW",
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
pub struct OverviewCommandPayload {
    pub monitor_id: Option<MonitorId>,
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
    CmdScrollStripLeft(StripScrollPayload),
    CmdScrollStripRight(StripScrollPayload),
    CmdToggleFloating(WindowCommandPayload),
    CmdToggleTabbed(WindowCommandPayload),
    CmdToggleMaximized(WindowCommandPayload),
    CmdToggleFullscreen(WindowCommandPayload),
    CmdToggleOverview(OverviewCommandPayload),
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

    pub fn toggle_overview(correlation_id: CorrelationId, monitor_id: Option<MonitorId>) -> Self {
        Self::new(
            DomainEventName::CmdToggleOverview,
            EventCategory::UserInputDerived,
            EventSource::InputCommand,
            correlation_id,
            DomainEventPayload::CmdToggleOverview(OverviewCommandPayload { monitor_id }),
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
