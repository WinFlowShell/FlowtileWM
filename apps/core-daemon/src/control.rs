use std::sync::mpsc;

use flowtile_ipc::{IpcRequest, IpcResponse};

#[derive(Clone, Debug)]
pub(crate) enum ControlMessage {
    Watch(WatchCommand),
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
    ScrollLeft,
    ScrollRight,
    ToggleFloating,
    ToggleTabbed,
    ToggleMaximized,
    ToggleFullscreen,
    ToggleOverview,
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
            "scroll-strip-left" => Some(Self::ScrollLeft),
            "scroll-strip-right" => Some(Self::ScrollRight),
            "toggle-floating" => Some(Self::ToggleFloating),
            "toggle-tabbed" => Some(Self::ToggleTabbed),
            "toggle-maximized" => Some(Self::ToggleMaximized),
            "toggle-fullscreen" => Some(Self::ToggleFullscreen),
            "toggle-overview" => Some(Self::ToggleOverview),
            "reload-config" => Some(Self::ReloadConfig),
            "disable-management-and-unwind" => Some(Self::Unwind),
            _ => None,
        }
    }

    pub(crate) fn from_stdin_alias(command: &str) -> Option<Self> {
        match command {
            "focus-next" | "next" => Some(Self::FocusNext),
            "focus-prev" | "prev" => Some(Self::FocusPrev),
            "scroll-left" | "left" => Some(Self::ScrollLeft),
            "scroll-right" | "right" => Some(Self::ScrollRight),
            "toggle-floating" | "floating" => Some(Self::ToggleFloating),
            "toggle-tabbed" | "tabbed" => Some(Self::ToggleTabbed),
            "toggle-maximized" | "maximized" => Some(Self::ToggleMaximized),
            "toggle-fullscreen" | "fullscreen" => Some(Self::ToggleFullscreen),
            "toggle-overview" | "overview" => Some(Self::ToggleOverview),
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
            Self::ScrollLeft => "scroll-strip-left",
            Self::ScrollRight => "scroll-strip-right",
            Self::ToggleFloating => "toggle-floating",
            Self::ToggleTabbed => "toggle-tabbed",
            Self::ToggleMaximized => "toggle-maximized",
            Self::ToggleFullscreen => "toggle-fullscreen",
            Self::ToggleOverview => "toggle-overview",
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
            Self::ScrollLeft => Some("scroll_strip_left"),
            Self::ScrollRight => Some("scroll_strip_right"),
            Self::ToggleFloating => Some("toggle_floating"),
            Self::ToggleTabbed => Some("toggle_tabbed"),
            Self::ToggleMaximized => Some("toggle_maximized"),
            Self::ToggleFullscreen => Some("toggle_fullscreen"),
            Self::ToggleOverview => Some("toggle_overview"),
            Self::ReloadConfig => Some("reload_config"),
            Self::Snapshot | Self::Unwind | Self::Rescan | Self::Quit => None,
        }
    }

    pub(crate) fn repeats_while_held(self) -> bool {
        matches!(self, Self::ScrollLeft | Self::ScrollRight)
    }
}
