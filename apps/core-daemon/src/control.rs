use std::sync::mpsc;

use flowtile_ipc::{IpcRequest, IpcResponse};

#[derive(Clone, Debug)]
pub(crate) enum ControlMessage {
    Watch(WatchCommand),
    OverviewActivateWindow {
        raw_hwnd: u64,
    },
    OverviewDismiss,
    IpcRequest {
        request: IpcRequest,
        response_sender: mpsc::Sender<IpcResponse>,
    },
    EventSubscribe {
        sender: mpsc::Sender<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatchCommand {
    FocusNext,
    FocusPrev,
    FocusWorkspaceUp,
    FocusWorkspaceDown,
    ScrollLeft,
    ScrollRight,
    MoveWorkspaceUp,
    MoveWorkspaceDown,
    MoveWorkspaceToMonitorNext,
    MoveWorkspaceToMonitorPrevious,
    MoveColumnToWorkspaceUp,
    MoveColumnToWorkspaceDown,
    CycleColumnWidth,
    ToggleFloating,
    ToggleTabbed,
    ToggleMaximized,
    ToggleFullscreen,
    OpenOverview,
    CloseOverview,
    ToggleOverview,
    OpenTerminal,
    CloseWindow,
    ReloadConfig,
    Snapshot,
    Unwind,
    Rescan,
    Quit,
}

impl WatchCommand {
    pub(crate) fn from_input_command(command: &str) -> Option<Self> {
        match command {
            "focus-next" => Some(Self::FocusNext),
            "focus-prev" => Some(Self::FocusPrev),
            "focus-workspace-up" => Some(Self::FocusWorkspaceUp),
            "focus-workspace-down" => Some(Self::FocusWorkspaceDown),
            "scroll-strip-left" => Some(Self::ScrollLeft),
            "scroll-strip-right" => Some(Self::ScrollRight),
            "move-workspace-up" => Some(Self::MoveWorkspaceUp),
            "move-workspace-down" => Some(Self::MoveWorkspaceDown),
            "move-workspace-to-monitor-next" => Some(Self::MoveWorkspaceToMonitorNext),
            "move-workspace-to-monitor-previous" => Some(Self::MoveWorkspaceToMonitorPrevious),
            "move-column-to-workspace-up" => Some(Self::MoveColumnToWorkspaceUp),
            "move-column-to-workspace-down" => Some(Self::MoveColumnToWorkspaceDown),
            "cycle-column-width" => Some(Self::CycleColumnWidth),
            "toggle-floating" => Some(Self::ToggleFloating),
            "toggle-tabbed" => Some(Self::ToggleTabbed),
            "toggle-maximized" => Some(Self::ToggleMaximized),
            "toggle-fullscreen" => Some(Self::ToggleFullscreen),
            "open-overview" => Some(Self::OpenOverview),
            "close-overview" => Some(Self::CloseOverview),
            "toggle-overview" => Some(Self::ToggleOverview),
            "open-terminal" => Some(Self::OpenTerminal),
            "close-window" => Some(Self::CloseWindow),
            "reload-config" => Some(Self::ReloadConfig),
            "disable-management-and-unwind" => Some(Self::Unwind),
            _ => None,
        }
    }

    pub(crate) fn from_stdin_alias(command: &str) -> Option<Self> {
        match command {
            "focus-next" | "next" => Some(Self::FocusNext),
            "focus-prev" | "prev" => Some(Self::FocusPrev),
            "focus-workspace-up" | "workspace-up" => Some(Self::FocusWorkspaceUp),
            "focus-workspace-down" | "workspace-down" => Some(Self::FocusWorkspaceDown),
            "scroll-left" | "left" => Some(Self::ScrollLeft),
            "scroll-right" | "right" => Some(Self::ScrollRight),
            "move-workspace-up" | "workspace-move-up" => Some(Self::MoveWorkspaceUp),
            "move-workspace-down" | "workspace-move-down" => Some(Self::MoveWorkspaceDown),
            "move-workspace-to-monitor-next" | "workspace-monitor-next" => {
                Some(Self::MoveWorkspaceToMonitorNext)
            }
            "move-workspace-to-monitor-previous" | "workspace-monitor-prev" => {
                Some(Self::MoveWorkspaceToMonitorPrevious)
            }
            "move-column-to-workspace-up" | "column-workspace-up" => {
                Some(Self::MoveColumnToWorkspaceUp)
            }
            "move-column-to-workspace-down" | "column-workspace-down" => {
                Some(Self::MoveColumnToWorkspaceDown)
            }
            "cycle-column-width" | "cycle-width" | "width" => Some(Self::CycleColumnWidth),
            "toggle-floating" | "floating" => Some(Self::ToggleFloating),
            "toggle-tabbed" | "tabbed" => Some(Self::ToggleTabbed),
            "toggle-maximized" | "maximized" => Some(Self::ToggleMaximized),
            "toggle-fullscreen" | "fullscreen" => Some(Self::ToggleFullscreen),
            "open-overview" | "overview-open" => Some(Self::OpenOverview),
            "close-overview" | "overview-close" => Some(Self::CloseOverview),
            "toggle-overview" | "overview" => Some(Self::ToggleOverview),
            "open-terminal" | "terminal" => Some(Self::OpenTerminal),
            "close-window" | "window-close" => Some(Self::CloseWindow),
            "reload-config" | "reload" => Some(Self::ReloadConfig),
            "snapshot" => Some(Self::Snapshot),
            "unwind" => Some(Self::Unwind),
            "rescan" => Some(Self::Rescan),
            "quit" | "exit" => Some(Self::Quit),
            _ => None,
        }
    }

    pub(crate) fn from_hotkey_command(command: &str) -> Option<Self> {
        Self::from_input_command(command)
    }

    pub(crate) fn as_hotkey_command_name(self) -> &'static str {
        match self {
            Self::FocusNext => "focus-next",
            Self::FocusPrev => "focus-prev",
            Self::FocusWorkspaceUp => "focus-workspace-up",
            Self::FocusWorkspaceDown => "focus-workspace-down",
            Self::ScrollLeft => "scroll-strip-left",
            Self::ScrollRight => "scroll-strip-right",
            Self::MoveWorkspaceUp => "move-workspace-up",
            Self::MoveWorkspaceDown => "move-workspace-down",
            Self::MoveWorkspaceToMonitorNext => "move-workspace-to-monitor-next",
            Self::MoveWorkspaceToMonitorPrevious => "move-workspace-to-monitor-previous",
            Self::MoveColumnToWorkspaceUp => "move-column-to-workspace-up",
            Self::MoveColumnToWorkspaceDown => "move-column-to-workspace-down",
            Self::CycleColumnWidth => "cycle-column-width",
            Self::ToggleFloating => "toggle-floating",
            Self::ToggleTabbed => "toggle-tabbed",
            Self::ToggleMaximized => "toggle-maximized",
            Self::ToggleFullscreen => "toggle-fullscreen",
            Self::OpenOverview => "open-overview",
            Self::CloseOverview => "close-overview",
            Self::ToggleOverview => "toggle-overview",
            Self::OpenTerminal => "open-terminal",
            Self::CloseWindow => "close-window",
            Self::ReloadConfig => "reload-config",
            Self::Snapshot => "snapshot",
            Self::Unwind => "disable-management-and-unwind",
            Self::Rescan => "rescan",
            Self::Quit => "quit",
        }
    }

    pub(crate) fn as_ipc_command_name(self) -> Option<&'static str> {
        match self {
            Self::FocusNext => Some("focus_next"),
            Self::FocusPrev => Some("focus_prev"),
            Self::FocusWorkspaceUp => Some("focus_workspace_up"),
            Self::FocusWorkspaceDown => Some("focus_workspace_down"),
            Self::ScrollLeft => Some("scroll_strip_left"),
            Self::ScrollRight => Some("scroll_strip_right"),
            Self::MoveWorkspaceUp => Some("move_workspace_up"),
            Self::MoveWorkspaceDown => Some("move_workspace_down"),
            Self::MoveWorkspaceToMonitorNext => Some("move_workspace_to_monitor_next"),
            Self::MoveWorkspaceToMonitorPrevious => Some("move_workspace_to_monitor_previous"),
            Self::MoveColumnToWorkspaceUp => Some("move_column_to_workspace_up"),
            Self::MoveColumnToWorkspaceDown => Some("move_column_to_workspace_down"),
            Self::CycleColumnWidth => Some("cycle_column_width"),
            Self::ToggleFloating => Some("toggle_floating"),
            Self::ToggleTabbed => Some("toggle_tabbed"),
            Self::ToggleMaximized => Some("toggle_maximized"),
            Self::ToggleFullscreen => Some("toggle_fullscreen"),
            Self::OpenOverview => Some("open_overview"),
            Self::CloseOverview => Some("close_overview"),
            Self::ToggleOverview => Some("toggle_overview"),
            Self::ReloadConfig => Some("reload_config"),
            Self::OpenTerminal
            | Self::CloseWindow
            | Self::Snapshot
            | Self::Unwind
            | Self::Rescan
            | Self::Quit => None,
        }
    }

    pub(crate) fn repeats_while_held(self) -> bool {
        matches!(self, Self::ScrollLeft | Self::ScrollRight)
    }
}
