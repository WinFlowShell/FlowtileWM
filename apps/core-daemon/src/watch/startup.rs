use std::{process::ExitCode, sync::mpsc};

use flowtile_ipc::bootstrap as ipc_bootstrap;
use flowtile_windows_adapter::{LiveObservationOptions, ObservationStream, WindowsAdapter};
use flowtile_wm_core::CoreDaemonRuntime;

use crate::{
    control::ControlMessage, diag::write_runtime_log, hotkeys::HotkeyListener, ipc,
    manual_resize::ManualResizeController, overview_controller::OverviewSurfaceController,
    tab_indicator::TabIndicatorController, touchpad::TouchpadListener,
};

use super::support::{
    spawn_stdin_listener, start_hotkey_listener, start_touchpad_listener, touchpad_watch_status,
};

const WATCH_STDIN_COMMANDS: &str = "stdin commands: focus-next, focus-prev, focus-workspace-up, focus-workspace-down, scroll-left, scroll-right, move-workspace-up, move-workspace-down, move-workspace-to-monitor-next, move-workspace-to-monitor-previous, move-column-to-workspace-up, move-column-to-workspace-down, cycle-column-width, toggle-floating, toggle-tabbed, toggle-maximized, toggle-fullscreen, open-overview, close-overview, toggle-overview, open-terminal, open-wallpaper-selector, close-window, reload-config, snapshot, unwind, rescan, quit";

pub(super) struct WatchStartup {
    pub(super) observer: Option<ObservationStream>,
    pub(super) control_sender: mpsc::Sender<ControlMessage>,
    pub(super) control_receiver: mpsc::Receiver<ControlMessage>,
    pub(super) hotkey_listener: Option<HotkeyListener>,
    pub(super) touchpad_listener: Option<TouchpadListener>,
    pub(super) manual_resize: ManualResizeController,
    pub(super) tab_indicator: Option<TabIndicatorController>,
    pub(super) overview_surface: Option<OverviewSurfaceController>,
}

pub(super) fn initialize_watch_startup(
    runtime: &CoreDaemonRuntime,
    adapter: &WindowsAdapter,
    poll_only: bool,
    interval_ms: u64,
) -> Result<WatchStartup, ExitCode> {
    let observer = start_observer(adapter, poll_only, interval_ms);

    let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
    ipc::spawn_ipc_servers(control_sender.clone());

    let hotkey_listener = match start_hotkey_listener(runtime, &control_sender) {
        Ok(listener) => {
            write_runtime_log(format!(
                "watch: hotkey-listener-started enabled={}",
                listener.is_some()
            ));
            listener
        }
        Err(error) => {
            write_runtime_log(format!("watch: hotkey-listener-start-error={error}"));
            eprintln!("global hotkeys failed to start: {error}");
            return Err(ExitCode::from(1));
        }
    };

    let touchpad_listener = match start_touchpad_listener(runtime, &control_sender) {
        Ok(listener) => {
            write_runtime_log(format!(
                "watch: touchpad-listener-started enabled={}",
                listener.is_some()
            ));
            listener
        }
        Err(error) => {
            write_runtime_log(format!("watch: touchpad-listener-start-error={error}"));
            eprintln!("touchpad override failed to start: {error}");
            return Err(ExitCode::from(1));
        }
    };

    let manual_resize = match ManualResizeController::spawn() {
        Ok(controller) => {
            write_runtime_log("watch: manual-resize-controller-started");
            controller
        }
        Err(error) => {
            write_runtime_log(format!(
                "watch: manual-resize-controller-start-error={error}"
            ));
            eprintln!("manual width resize failed to start: {error}");
            return Err(ExitCode::from(1));
        }
    };

    let tab_indicator = match TabIndicatorController::spawn() {
        Ok(controller) => {
            write_runtime_log("watch: tab-indicator-started");
            Some(controller)
        }
        Err(error) => {
            write_runtime_log(format!("watch: tab-indicator-start-error={error}"));
            eprintln!("tab indicator surface failed to start: {error}");
            None
        }
    };

    let overview_surface = match OverviewSurfaceController::spawn(control_sender.clone()) {
        Ok(controller) => {
            write_runtime_log("watch: overview-surface-started");
            Some(controller)
        }
        Err(error) => {
            write_runtime_log(format!("watch: overview-surface-start-error={error}"));
            eprintln!("overview surface failed to start: {error}");
            None
        }
    };

    spawn_stdin_listener(control_sender.clone());
    write_runtime_log("watch: stdin-listener-started");

    Ok(WatchStartup {
        observer,
        control_sender,
        control_receiver,
        hotkey_listener,
        touchpad_listener,
        manual_resize,
        tab_indicator,
        overview_surface,
    })
}

pub(super) fn print_watch_banner(
    runtime: &CoreDaemonRuntime,
    observer: &Option<ObservationStream>,
    hotkey_listener: &Option<HotkeyListener>,
    touchpad_listener: &Option<TouchpadListener>,
) {
    let ipc = ipc_bootstrap();
    println!("flowtile-core-daemon watch");
    println!(
        "observation mode: {}",
        if observer.is_some() {
            "live-hooks"
        } else {
            "polling-fallback"
        }
    );
    println!(
        "bind control mode: {}",
        runtime.bind_control_mode().as_str()
    );
    println!(
        "global hotkeys: {}",
        if hotkey_listener.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "touchpad override: {}",
        touchpad_watch_status(runtime.touchpad_config(), touchpad_listener.is_some())
    );
    println!(
        "ipc command pipe: {} | event stream pipe: {}",
        ipc.command_pipe_name, ipc.event_stream_pipe_name
    );
    println!("{WATCH_STDIN_COMMANDS}");
}

fn start_observer(
    adapter: &WindowsAdapter,
    poll_only: bool,
    interval_ms: u64,
) -> Option<ObservationStream> {
    if poll_only {
        write_runtime_log("watch: observation-mode=poll-only");
        return None;
    }

    match adapter.spawn_observer(LiveObservationOptions {
        fallback_scan_interval_ms: interval_ms.max(1_000),
        ..LiveObservationOptions::default()
    }) {
        Ok(stream) => {
            write_runtime_log("watch: live-observer-started");
            Some(stream)
        }
        Err(error) => {
            write_runtime_log(format!(
                "watch: live-observer-start-failed={error}; using polling fallback"
            ));
            eprintln!("live observation failed to start: {error}; falling back to polling");
            None
        }
    }
}
