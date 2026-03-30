use std::{
    io::{self, BufRead},
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use flowtile_domain::{
    ColumnId, CorrelationId, DomainEvent, NavigationScope, RuntimeMode, WorkspaceId,
};
use flowtile_ipc::bootstrap as ipc_bootstrap;
use flowtile_windows_adapter::{LiveObservationOptions, ObservationStreamError, WindowsAdapter};
use flowtile_wm_core::{CoreDaemonRuntime, RuntimeCycleReport};

use crate::{
    control::{ControlMessage, WatchCommand},
    diag::write_runtime_log,
    hotkeys::{HotkeyListener, HotkeyListenerError, ensure_bind_control_mode_supported},
    ipc,
    manual_resize::{ManualResizeController, ManualResizeError},
    overview_controller::OverviewSurfaceController,
    tab_indicator::TabIndicatorController,
    terminal::open_default_terminal,
    touchpad::{
        TouchpadListener, TouchpadListenerError, assess_touchpad_override,
        ensure_touchpad_override_supported,
    },
    window_actions::{activate_window, close_window},
};

const CONTROL_RESPONSE_SLICE_MS: u64 = 16;

pub(crate) fn run_watch(
    runtime_mode: RuntimeMode,
    dry_run: bool,
    interval_ms: u64,
    iterations: Option<u64>,
    poll_only: bool,
) -> ExitCode {
    write_runtime_log(format!(
        "watch: start runtime_mode={runtime_mode:?} dry_run={dry_run} interval_ms={interval_ms} iterations={iterations:?} poll_only={poll_only}"
    ));
    let adapter = WindowsAdapter::new();
    let mut runtime = CoreDaemonRuntime::with_adapter(runtime_mode, adapter.clone());
    if let Err(error) = validate_runtime_bind_control_mode(&runtime) {
        write_runtime_log(format!("watch: bind-control-validation-error={error}"));
        eprintln!("bind control mode startup failed: {error}");
        return ExitCode::from(1);
    }
    write_runtime_log("watch: bind-control-validation-ok");
    if let Err(error) = validate_runtime_touchpad_override(&runtime) {
        write_runtime_log(format!("watch: touchpad-validation-error={error}"));
        eprintln!("touchpad override startup failed: {error}");
        return ExitCode::from(1);
    }
    write_runtime_log("watch: touchpad-validation-ok");
    let mut observer = if poll_only {
        write_runtime_log("watch: observation-mode=poll-only");
        None
    } else {
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
    };

    let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
    ipc::spawn_ipc_servers(control_sender.clone());
    let mut hotkey_listener = match start_hotkey_listener(&runtime, &control_sender) {
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
            return ExitCode::from(1);
        }
    };
    let mut touchpad_listener = match start_touchpad_listener(&runtime, &control_sender) {
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
            return ExitCode::from(1);
        }
    };
    let mut manual_resize = match ManualResizeController::spawn() {
        Ok(controller) => {
            write_runtime_log("watch: manual-resize-controller-started");
            controller
        }
        Err(error) => {
            write_runtime_log(format!(
                "watch: manual-resize-controller-start-error={error}"
            ));
            eprintln!("manual width resize failed to start: {error}");
            return ExitCode::from(1);
        }
    };
    let mut tab_indicator = match TabIndicatorController::spawn() {
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
    let mut overview_surface = match OverviewSurfaceController::spawn(control_sender.clone()) {
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
    println!(
        "stdin commands: focus-next, focus-prev, focus-workspace-up, focus-workspace-down, scroll-left, scroll-right, move-workspace-up, move-workspace-down, move-workspace-to-monitor-next, move-workspace-to-monitor-previous, move-column-to-workspace-up, move-column-to-workspace-down, cycle-column-width, toggle-floating, toggle-tabbed, toggle-maximized, toggle-fullscreen, open-overview, close-overview, toggle-overview, open-terminal, close-window, reload-config, snapshot, unwind, rescan, quit"
    );

    let mut completed_iterations = 0_u64;
    let mut manual_correlation_id = 1_u64;
    let mut event_subscribers = Vec::<mpsc::Sender<String>>::new();
    let mut stream_version = 1_u64;
    let mut last_streamed_state_version = runtime.state().state_version().get();
    let poll_interval = Duration::from_millis(interval_ms.max(1));
    let control_response_slice = Duration::from_millis(CONTROL_RESPONSE_SLICE_MS);
    let observer_wait_slice = poll_interval.min(control_response_slice);
    let mut next_poll_deadline = Instant::now();
    write_runtime_log("watch: entering-main-loop");
    sync_visual_surfaces(
        &runtime,
        &control_sender,
        &mut tab_indicator,
        &mut overview_surface,
    );

    if let Some(live_observer) = observer.as_mut() {
        match live_observer.recv_timeout(Duration::from_millis(interval_ms.max(5_000))) {
            Ok(observation) => match runtime.apply_observation(observation, dry_run) {
                Ok(Some(report)) => {
                    print_iteration(completed_iterations + 1, &report);
                    completed_iterations += 1;
                    maybe_broadcast_state(
                        &runtime,
                        &mut event_subscribers,
                        &mut stream_version,
                        &mut last_streamed_state_version,
                    );
                    if iterations.is_some_and(|limit| completed_iterations >= limit) {
                        return ExitCode::SUCCESS;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("{error:?}");
                    return ExitCode::from(1);
                }
            },
            Err(ObservationStreamError::Timeout) => {
                eprintln!(
                    "live observation did not produce an initial snapshot in time; falling back to polling"
                );
                observer = None;
            }
            Err(error) => {
                eprintln!(
                    "live observation failed during startup: {error}; falling back to polling"
                );
                observer = None;
            }
        }
    }

    loop {
        sync_visual_surfaces(
            &runtime,
            &control_sender,
            &mut tab_indicator,
            &mut overview_surface,
        );

        while let Ok(message) = control_receiver.try_recv() {
            match message {
                ControlMessage::Watch(command) => match command {
                    WatchCommand::FocusNext => match runtime.dispatch_command(
                        DomainEvent::focus_next(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            NavigationScope::WorkspaceStrip,
                        ),
                        dry_run,
                        "manual-focus-next",
                    ) {
                        Ok(report) => {
                            println!("manual command: focus-next");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::FocusPrev => match runtime.dispatch_command(
                        DomainEvent::focus_prev(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            NavigationScope::WorkspaceStrip,
                        ),
                        dry_run,
                        "manual-focus-prev",
                    ) {
                        Ok(report) => {
                            println!("manual command: focus-prev");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::FocusWorkspaceUp => match runtime.dispatch_command(
                        DomainEvent::focus_workspace_up(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-focus-workspace-up",
                    ) {
                        Ok(report) => {
                            println!("manual command: focus-workspace-up");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::FocusWorkspaceDown => match runtime.dispatch_command(
                        DomainEvent::focus_workspace_down(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-focus-workspace-down",
                    ) {
                        Ok(report) => {
                            println!("manual command: focus-workspace-down");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ScrollLeft => match runtime.dispatch_command(
                        DomainEvent::scroll_strip_left(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            NavigationScope::WorkspaceStrip,
                            0,
                        ),
                        dry_run,
                        "manual-scroll-left",
                    ) {
                        Ok(report) => {
                            println!("manual command: scroll-left");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ScrollRight => match runtime.dispatch_command(
                        DomainEvent::scroll_strip_right(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            NavigationScope::WorkspaceStrip,
                            0,
                        ),
                        dry_run,
                        "manual-scroll-right",
                    ) {
                        Ok(report) => {
                            println!("manual command: scroll-right");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveWorkspaceUp => match runtime.dispatch_command(
                        DomainEvent::move_workspace_up(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-workspace-up",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-workspace-up");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveWorkspaceDown => match runtime.dispatch_command(
                        DomainEvent::move_workspace_down(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-workspace-down",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-workspace-down");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveWorkspaceToMonitorNext => match runtime.dispatch_command(
                        DomainEvent::move_workspace_to_monitor_next(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-workspace-to-monitor-next",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-workspace-to-monitor-next");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveWorkspaceToMonitorPrevious => match runtime.dispatch_command(
                        DomainEvent::move_workspace_to_monitor_previous(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-workspace-to-monitor-previous",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-workspace-to-monitor-previous");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveColumnToWorkspaceUp => match runtime.dispatch_command(
                        DomainEvent::move_column_to_workspace_up(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-column-to-workspace-up",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-column-to-workspace-up");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::MoveColumnToWorkspaceDown => match runtime.dispatch_command(
                        DomainEvent::move_column_to_workspace_down(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-move-column-to-workspace-down",
                    ) {
                        Ok(report) => {
                            println!("manual command: move-column-to-workspace-down");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::CycleColumnWidth => match runtime.dispatch_command(
                        DomainEvent::cycle_column_width(next_manual_correlation_id(
                            &mut manual_correlation_id,
                        )),
                        dry_run,
                        "manual-cycle-column-width",
                    ) {
                        Ok(report) => {
                            println!("manual command: cycle-column-width");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ToggleFloating => match runtime.dispatch_command(
                        DomainEvent::toggle_floating(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-toggle-floating",
                    ) {
                        Ok(report) => {
                            println!("manual command: toggle-floating");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ToggleTabbed => match runtime.dispatch_command(
                        DomainEvent::toggle_tabbed(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-toggle-tabbed",
                    ) {
                        Ok(report) => {
                            println!("manual command: toggle-tabbed");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ToggleMaximized => match runtime.dispatch_command(
                        DomainEvent::toggle_maximized(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-toggle-maximized",
                    ) {
                        Ok(report) => {
                            println!("manual command: toggle-maximized");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ToggleFullscreen => match runtime.dispatch_command(
                        DomainEvent::toggle_fullscreen(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-toggle-fullscreen",
                    ) {
                        Ok(report) => {
                            println!("manual command: toggle-fullscreen");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::OpenOverview => match runtime.dispatch_command(
                        DomainEvent::open_overview(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-open-overview",
                    ) {
                        Ok(report) => {
                            println!("manual command: open-overview");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::CloseOverview => match runtime.dispatch_command(
                        DomainEvent::close_overview(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-close-overview",
                    ) {
                        Ok(report) => {
                            println!("manual command: close-overview");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::ToggleOverview => match runtime.dispatch_command(
                        DomainEvent::toggle_overview(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "manual-toggle-overview",
                    ) {
                        Ok(report) => {
                            println!("manual command: toggle-overview");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::OpenTerminal => match open_default_terminal() {
                        Ok(()) => {
                            write_runtime_log("watch: open-terminal-ok");
                            println!("manual command: open-terminal");
                        }
                        Err(error) => {
                            write_runtime_log(format!("watch: open-terminal-error={error}"));
                            eprintln!("open terminal failed: {error}");
                        }
                    },
                    WatchCommand::CloseWindow => match focused_managed_hwnd(&runtime)
                        .ok_or_else(|| {
                            "no focused managed window is currently bound to an HWND".to_string()
                        })
                        .and_then(close_window)
                    {
                        Ok(()) => {
                            write_runtime_log("watch: close-window-ok");
                            println!("manual command: close-window");
                        }
                        Err(error) => {
                            write_runtime_log(format!("watch: close-window-error={error}"));
                            eprintln!("close window failed: {error}");
                        }
                    },
                    WatchCommand::ReloadConfig => match runtime.reload_config(dry_run) {
                        Ok(report) => {
                            hotkey_listener = match start_hotkey_listener(&runtime, &control_sender)
                            {
                                Ok(listener) => listener,
                                Err(error) => {
                                    eprintln!("global hotkeys failed to restart: {error}");
                                    return ExitCode::from(1);
                                }
                            };
                            touchpad_listener =
                                match start_touchpad_listener(&runtime, &control_sender) {
                                    Ok(listener) => listener,
                                    Err(error) => {
                                        eprintln!("touchpad override failed to restart: {error}");
                                        return ExitCode::from(1);
                                    }
                                };
                            println!(
                                "global hotkeys reloaded: {}",
                                if hotkey_listener.is_some() {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
                            );
                            println!(
                                "touchpad override reloaded: {}",
                                touchpad_watch_status(
                                    runtime.touchpad_config(),
                                    touchpad_listener.is_some(),
                                )
                            );
                            println!("manual command: reload-config");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::Snapshot => {
                        print_state_snapshot(&runtime);
                    }
                    WatchCommand::Unwind => {
                        runtime.request_emergency_unwind("manual-command");
                        println!("management disabled by emergency unwind");
                    }
                    WatchCommand::Rescan => match runtime.scan_and_sync(dry_run) {
                        Ok(report) => {
                            println!("manual rescan");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    },
                    WatchCommand::Quit => {
                        write_runtime_log("watch: quit-command-received");
                        return ExitCode::SUCCESS;
                    }
                },
                ControlMessage::OverviewActivateWindow { raw_hwnd } => {
                    if !overview_activation_target_is_valid(runtime.state(), raw_hwnd) {
                        write_runtime_log(format!(
                            "watch: overview-activate-window-ignored hwnd={raw_hwnd}"
                        ));
                        continue;
                    }

                    match runtime.dispatch_command(
                        DomainEvent::close_overview(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "overview-click-close-overview",
                    ) {
                        Ok(report) => {
                            println!("overview action: activate-window");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    }

                    sync_visual_surfaces(
                        &runtime,
                        &control_sender,
                        &mut tab_indicator,
                        &mut overview_surface,
                    );

                    if dry_run {
                        write_runtime_log(format!(
                            "watch: overview-activate-window-dry-run hwnd={raw_hwnd}"
                        ));
                    } else if let Err(error) = activate_window(raw_hwnd) {
                        write_runtime_log(format!(
                            "watch: overview-activate-window-error hwnd={raw_hwnd} error={error}"
                        ));
                        eprintln!("overview click activation failed: {error}");
                    } else {
                        write_runtime_log(format!(
                            "watch: overview-activate-window-ok hwnd={raw_hwnd}"
                        ));
                    }
                }
                ControlMessage::OverviewDismiss => {
                    if !runtime.state().overview.is_open {
                        write_runtime_log("watch: overview-dismiss-ignored");
                        continue;
                    }

                    match runtime.dispatch_command(
                        DomainEvent::close_overview(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            None,
                        ),
                        dry_run,
                        "overview-dismiss-close-overview",
                    ) {
                        Ok(report) => {
                            println!("overview action: dismiss");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    }

                    sync_visual_surfaces(
                        &runtime,
                        &control_sender,
                        &mut tab_indicator,
                        &mut overview_surface,
                    );
                    write_runtime_log("watch: overview-dismiss-ok");
                }
                ControlMessage::OverviewMoveColumn {
                    dragged_raw_hwnd,
                    target_workspace_id,
                    insert_after_raw_hwnd,
                } => {
                    if !overview_workspace_target_is_valid(runtime.state(), target_workspace_id) {
                        write_runtime_log(format!(
                            "watch: overview-move-column-ignored invalid-target-workspace={}",
                            target_workspace_id.get()
                        ));
                        continue;
                    }
                    let Some(source_column_id) =
                        overview_drag_source_column_id(runtime.state(), dragged_raw_hwnd)
                    else {
                        write_runtime_log(format!(
                            "watch: overview-move-column-ignored invalid-source hwnd={dragged_raw_hwnd}"
                        ));
                        continue;
                    };
                    let Some(anchor_column_id) = overview_drop_anchor_column_id(
                        runtime.state(),
                        target_workspace_id,
                        insert_after_raw_hwnd,
                    ) else {
                        write_runtime_log(format!(
                            "watch: overview-move-column-ignored invalid-anchor hwnd={:?}",
                            insert_after_raw_hwnd
                        ));
                        continue;
                    };

                    match runtime.dispatch_command(
                        DomainEvent::move_column_to_workspace_target(
                            next_manual_correlation_id(&mut manual_correlation_id),
                            source_column_id,
                            target_workspace_id,
                            anchor_column_id,
                        ),
                        dry_run,
                        "overview-drag-drop-column",
                    ) {
                        Ok(report) => {
                            println!("overview action: move-column");
                            print_iteration(completed_iterations + 1, &report);
                            completed_iterations += 1;
                            maybe_broadcast_state(
                                &runtime,
                                &mut event_subscribers,
                                &mut stream_version,
                                &mut last_streamed_state_version,
                            );
                        }
                        Err(error) => {
                            eprintln!("{error:?}");
                            return ExitCode::from(1);
                        }
                    }

                    sync_visual_surfaces(
                        &runtime,
                        &control_sender,
                        &mut tab_indicator,
                        &mut overview_surface,
                    );
                    write_runtime_log(format!(
                        "watch: overview-move-column-ok column={} target_workspace={} anchor={:?}",
                        source_column_id.get(),
                        target_workspace_id.get(),
                        anchor_column_id.map(|column_id| column_id.get())
                    ));
                }
                ControlMessage::IpcRequest {
                    request,
                    response_sender,
                } => {
                    let command_name = request.command.clone();
                    let (response, should_broadcast) = ipc::handle_ipc_request(
                        &mut runtime,
                        dry_run,
                        request,
                        &mut manual_correlation_id,
                    );
                    if command_name == "reload_config" && response.ok {
                        hotkey_listener = match start_hotkey_listener(&runtime, &control_sender) {
                            Ok(listener) => listener,
                            Err(error) => {
                                eprintln!("global hotkeys failed to restart via IPC: {error}");
                                return ExitCode::from(1);
                            }
                        };
                        touchpad_listener = match start_touchpad_listener(&runtime, &control_sender)
                        {
                            Ok(listener) => listener,
                            Err(error) => {
                                eprintln!("touchpad override failed to restart via IPC: {error}");
                                return ExitCode::from(1);
                            }
                        };
                        println!(
                            "global hotkeys reloaded via IPC: {}",
                            if hotkey_listener.is_some() {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        );
                        println!(
                            "touchpad override reloaded via IPC: {}",
                            touchpad_watch_status(
                                runtime.touchpad_config(),
                                touchpad_listener.is_some(),
                            )
                        );
                    }
                    let _ = response_sender.send(response);
                    if should_broadcast {
                        maybe_broadcast_state(
                            &runtime,
                            &mut event_subscribers,
                            &mut stream_version,
                            &mut last_streamed_state_version,
                        );
                    }
                }
                ControlMessage::EventSubscribe { sender } => {
                    if ipc::send_initial_snapshot(&sender, &runtime, &mut stream_version) {
                        event_subscribers.push(sender);
                    }
                }
            }

            if iterations.is_some_and(|limit| completed_iterations >= limit) {
                return ExitCode::SUCCESS;
            }
        }

        match manual_resize.tick(&mut runtime, dry_run) {
            Ok(Some(report)) => {
                println!("manual command: column-width-drag");
                print_iteration(completed_iterations + 1, &report);
                completed_iterations += 1;
                maybe_broadcast_state(
                    &runtime,
                    &mut event_subscribers,
                    &mut stream_version,
                    &mut last_streamed_state_version,
                );
                if iterations.is_some_and(|limit| completed_iterations >= limit) {
                    return ExitCode::SUCCESS;
                }
            }
            Ok(None) => {}
            Err(ManualResizeError::Runtime(error)) => {
                eprintln!("{error:?}");
                return ExitCode::from(1);
            }
            Err(ManualResizeError::Platform(error)) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        }

        let mut fallback_to_polling = false;
        let mut advanced_poll_cycle = false;
        let cycle_result = if let Some(live_observer) = observer.as_mut() {
            match live_observer.recv_timeout(observer_wait_slice) {
                Ok(observation) => runtime.apply_observation(observation, dry_run),
                Err(ObservationStreamError::Timeout) => continue,
                Err(error) => {
                    eprintln!("live observation became unavailable: {error}; switching to polling");
                    fallback_to_polling = true;
                    advanced_poll_cycle = true;
                    runtime.scan_and_sync(dry_run).map(Some)
                }
            }
        } else {
            let now = Instant::now();
            if now < next_poll_deadline {
                thread::sleep((next_poll_deadline - now).min(control_response_slice));
                continue;
            }

            advanced_poll_cycle = true;
            runtime.scan_and_sync(dry_run).map(Some)
        };

        if fallback_to_polling {
            observer = None;
        }

        match cycle_result {
            Ok(Some(report)) => {
                print_iteration(completed_iterations + 1, &report);
                completed_iterations += 1;
                maybe_broadcast_state(
                    &runtime,
                    &mut event_subscribers,
                    &mut stream_version,
                    &mut last_streamed_state_version,
                );
            }
            Ok(None) => continue,
            Err(error) => {
                eprintln!("{error:?}");
                return ExitCode::from(1);
            }
        }

        if iterations.is_some_and(|limit| completed_iterations >= limit) {
            write_runtime_log(format!(
                "watch: iteration-limit-reached completed_iterations={completed_iterations}"
            ));
            return ExitCode::SUCCESS;
        }

        if observer.is_none() && advanced_poll_cycle {
            next_poll_deadline = Instant::now() + poll_interval;
        }
    }
}

fn spawn_stdin_listener(control_sender: mpsc::Sender<ControlMessage>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut locked = stdin.lock();
        let mut line = String::new();

        loop {
            line.clear();
            match locked.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let command = line.trim().to_ascii_lowercase();
                    let watch_command = WatchCommand::from_stdin_alias(command.as_str());
                    if let Some(watch_command) = watch_command
                        && control_sender
                            .send(ControlMessage::Watch(watch_command))
                            .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn maybe_broadcast_state(
    runtime: &CoreDaemonRuntime,
    event_subscribers: &mut Vec<mpsc::Sender<String>>,
    stream_version: &mut u64,
    last_streamed_state_version: &mut u64,
) {
    let current_state_version = runtime.state().state_version().get();
    if current_state_version == *last_streamed_state_version {
        return;
    }

    ipc::broadcast_runtime_delta(event_subscribers, runtime, stream_version);
    *last_streamed_state_version = current_state_version;
}

fn sync_visual_surfaces(
    runtime: &CoreDaemonRuntime,
    control_sender: &mpsc::Sender<ControlMessage>,
    tab_indicator: &mut Option<TabIndicatorController>,
    overview_surface: &mut Option<OverviewSurfaceController>,
) {
    if let Some(controller) = tab_indicator.as_mut()
        && let Err(error) = controller.sync(runtime)
    {
        log_visual_surface_error("tab-indicator", &error);
        *tab_indicator = None;
    }

    ensure_overview_surface(runtime, control_sender, overview_surface);
    if let Some(controller) = overview_surface.as_mut()
        && let Err(error) = controller.sync(runtime)
    {
        log_visual_surface_error("overview-surface", &error);
        *overview_surface = None;
    }
}

fn ensure_overview_surface(
    runtime: &CoreDaemonRuntime,
    control_sender: &mpsc::Sender<ControlMessage>,
    overview_surface: &mut Option<OverviewSurfaceController>,
) {
    if overview_surface.is_some()
        || !runtime.management_enabled()
        || runtime.state().runtime.boot_mode == RuntimeMode::SafeMode
        || !runtime.state().overview.is_open
    {
        return;
    }

    match OverviewSurfaceController::spawn(control_sender.clone()) {
        Ok(controller) => {
            write_runtime_log("watch: overview-surface-restarted");
            *overview_surface = Some(controller);
        }
        Err(error) => {
            log_visual_surface_error("overview-surface-restart", &error);
        }
    }
}

fn log_visual_surface_error(kind: &str, error: &impl std::fmt::Display) {
    let message = format!("{kind} degraded: {error}");
    write_runtime_log(format!("watch: {message}"));
    eprintln!("{message}");
}

fn print_iteration(iteration: u64, report: &RuntimeCycleReport) {
    println!("iteration {iteration}");
    print_report(report);
}

fn print_report(report: &RuntimeCycleReport) {
    for line in report.summary_lines() {
        println!("{line}");
        write_runtime_log(format!("report: {line}"));
    }
}

fn print_state_snapshot(runtime: &CoreDaemonRuntime) {
    let state = runtime.state();
    let touchpad = assess_touchpad_override(runtime.touchpad_config());
    println!("state snapshot");
    println!("state version: {}", state.state_version().get());
    println!("monitors: {}", state.monitors.len());
    println!("workspaces: {}", state.workspaces.len());
    println!("windows: {}", state.windows.len());
    println!(
        "focused window: {}",
        state
            .focus
            .focused_window_id
            .map(|window_id| window_id.get().to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    println!("overview open: {}", state.overview.is_open);
    println!("config version: {}", state.config_projection.config_version);
    println!(
        "config rules: {}",
        state.config_projection.active_rule_count
    );
    println!("touchpad override: {}", touchpad.summary_label());
    println!(
        "touchpad gesture bindings: {}",
        touchpad.configured_gesture_count
    );
    if let Some(detail) = touchpad.detail {
        println!("touchpad override detail: {detail}");
    }
    if let Some(monitor_id) = state.focus.focused_monitor_id
        && let Some(workspace_id) = state.active_workspace_id_for_monitor(monitor_id)
        && let Some(workspace) = state.workspaces.get(&workspace_id)
    {
        println!("active workspace: {}", workspace_id.get());
        println!("strip scroll offset: {}", workspace.strip.scroll_offset);
        println!(
            "strip columns: {}",
            workspace.strip.ordered_column_ids.len()
        );
    }
}

fn next_manual_correlation_id(counter: &mut u64) -> CorrelationId {
    let correlation_id = CorrelationId::new(*counter);
    *counter += 1;
    correlation_id
}

fn focused_managed_hwnd(runtime: &CoreDaemonRuntime) -> Option<u64> {
    runtime
        .state()
        .focus
        .focused_window_id
        .and_then(|window_id| runtime.state().windows.get(&window_id))
        .and_then(|window| window.current_hwnd_binding)
}

fn overview_activation_target_is_valid(state: &flowtile_domain::WmState, raw_hwnd: u64) -> bool {
    if raw_hwnd == 0 || !state.overview.is_open {
        return false;
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return false;
    };

    state.windows.values().any(|window| {
        if !window.is_managed || window.current_hwnd_binding != Some(raw_hwnd) {
            return false;
        }

        state
            .workspaces
            .get(&window.workspace_id)
            .is_some_and(|workspace| workspace.monitor_id == monitor_id)
    })
}

fn overview_workspace_target_is_valid(
    state: &flowtile_domain::WmState,
    workspace_id: WorkspaceId,
) -> bool {
    if !state.overview.is_open {
        return false;
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return false;
    };

    state
        .workspaces
        .get(&workspace_id)
        .is_some_and(|workspace| workspace.monitor_id == monitor_id)
}

fn overview_drag_source_column_id(
    state: &flowtile_domain::WmState,
    raw_hwnd: u64,
) -> Option<ColumnId> {
    let window = overview_managed_window_for_hwnd(state, raw_hwnd)?;
    (window.layer == flowtile_domain::WindowLayer::Tiled
        && !window.is_floating
        && !window.is_fullscreen)
        .then_some(window.column_id)
        .flatten()
}

fn overview_drop_anchor_column_id(
    state: &flowtile_domain::WmState,
    target_workspace_id: WorkspaceId,
    raw_hwnd: Option<u64>,
) -> Option<Option<ColumnId>> {
    let Some(raw_hwnd) = raw_hwnd else {
        return Some(None);
    };
    let window = overview_managed_window_for_hwnd(state, raw_hwnd)?;
    if window.workspace_id != target_workspace_id
        || window.layer != flowtile_domain::WindowLayer::Tiled
        || window.is_floating
        || window.is_fullscreen
    {
        return None;
    }
    window.column_id.map(Some)
}

fn overview_managed_window_for_hwnd<'a>(
    state: &'a flowtile_domain::WmState,
    raw_hwnd: u64,
) -> Option<&'a flowtile_domain::WindowNode> {
    let monitor_id = state.overview.monitor_id?;
    state.windows.values().find(|window| {
        if !window.is_managed || window.current_hwnd_binding != Some(raw_hwnd) {
            return false;
        }

        state
            .workspaces
            .get(&window.workspace_id)
            .is_some_and(|workspace| workspace.monitor_id == monitor_id)
    })
}

fn start_hotkey_listener(
    runtime: &CoreDaemonRuntime,
    command_sender: &mpsc::Sender<ControlMessage>,
) -> Result<Option<HotkeyListener>, HotkeyListenerError> {
    HotkeyListener::spawn(
        runtime.hotkeys(),
        runtime.bind_control_mode(),
        command_sender.clone(),
    )
}

fn validate_runtime_bind_control_mode(
    runtime: &CoreDaemonRuntime,
) -> Result<(), HotkeyListenerError> {
    ensure_bind_control_mode_supported(runtime.bind_control_mode())
}

fn start_touchpad_listener(
    runtime: &CoreDaemonRuntime,
    command_sender: &mpsc::Sender<ControlMessage>,
) -> Result<Option<TouchpadListener>, TouchpadListenerError> {
    TouchpadListener::spawn(runtime.touchpad_config(), command_sender.clone())
}

fn validate_runtime_touchpad_override(
    runtime: &CoreDaemonRuntime,
) -> Result<(), TouchpadListenerError> {
    ensure_touchpad_override_supported(runtime.touchpad_config())
}

fn touchpad_watch_status(
    config: &flowtile_config_rules::TouchpadConfig,
    listener_running: bool,
) -> &'static str {
    if listener_running {
        "enabled"
    } else {
        assess_touchpad_override(config).summary_label()
    }
}

#[cfg(test)]
mod tests {
    use flowtile_domain::{
        Column, ColumnMode, Rect, RuntimeMode, Size, WidthSemantics, WindowClassification,
        WindowLayer, WindowNode, WmState,
    };

    use super::{
        overview_activation_target_is_valid, overview_drag_source_column_id,
        overview_drop_anchor_column_id, overview_workspace_target_is_valid,
    };

    #[test]
    fn overview_activation_target_must_be_managed_on_the_open_monitor() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let overview_monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let other_monitor_id = state.add_monitor(Rect::new(1600, 0, 1600, 900), 96, false);

        let overview_workspace_id = state
            .active_workspace_id_for_monitor(overview_monitor_id)
            .expect("overview monitor workspace should exist");
        let other_workspace_id = state
            .active_workspace_id_for_monitor(other_monitor_id)
            .expect("other monitor workspace should exist");

        let overview_window_id = state.allocate_window_id();
        state.windows.insert(
            overview_window_id,
            WindowNode {
                id: overview_window_id,
                current_hwnd_binding: Some(100),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id: overview_workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 800, 600),
                desired_size: Size::default(),
            },
        );

        let unmanaged_window_id = state.allocate_window_id();
        state.windows.insert(
            unmanaged_window_id,
            WindowNode {
                id: unmanaged_window_id,
                current_hwnd_binding: Some(200),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id: overview_workspace_id,
                column_id: None,
                is_managed: false,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 800, 600),
                desired_size: Size::default(),
            },
        );

        let other_monitor_window_id = state.allocate_window_id();
        state.windows.insert(
            other_monitor_window_id,
            WindowNode {
                id: other_monitor_window_id,
                current_hwnd_binding: Some(300),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id: other_workspace_id,
                column_id: None,
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(1600, 0, 800, 600),
                desired_size: Size::default(),
            },
        );

        state.overview.is_open = true;
        state.overview.monitor_id = Some(overview_monitor_id);

        assert!(overview_activation_target_is_valid(&state, 100));
        assert!(!overview_activation_target_is_valid(&state, 0));
        assert!(!overview_activation_target_is_valid(&state, 200));
        assert!(!overview_activation_target_is_valid(&state, 300));

        state.overview.is_open = false;
        assert!(!overview_activation_target_is_valid(&state, 100));
    }

    #[test]
    fn overview_drag_helpers_resolve_source_column_and_anchor_within_open_monitor_scene() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let source_workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("source workspace should exist");

        let source_column_id = state.allocate_column_id();
        let anchor_column_id = state.allocate_column_id();
        state.layout.columns.insert(
            source_column_id,
            Column::new(
                source_column_id,
                ColumnMode::Normal,
                WidthSemantics::Fixed(420),
                Vec::new(),
            ),
        );
        state.layout.columns.insert(
            anchor_column_id,
            Column::new(
                anchor_column_id,
                ColumnMode::Normal,
                WidthSemantics::Fixed(420),
                Vec::new(),
            ),
        );

        let source_window_id = state.allocate_window_id();
        state.windows.insert(
            source_window_id,
            WindowNode {
                id: source_window_id,
                current_hwnd_binding: Some(100),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id: source_workspace_id,
                column_id: Some(source_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 420, 900),
                desired_size: Size::default(),
            },
        );
        state
            .layout
            .columns
            .get_mut(&source_column_id)
            .expect("source column should exist")
            .ordered_window_ids
            .push(source_window_id);
        state
            .workspaces
            .get_mut(&source_workspace_id)
            .expect("source workspace should exist")
            .strip
            .ordered_column_ids
            .push(source_column_id);
        let target_workspace_id = state
            .ensure_tail_workspace(monitor_id)
            .expect("tail workspace should exist");

        let anchor_window_id = state.allocate_window_id();
        state.windows.insert(
            anchor_window_id,
            WindowNode {
                id: anchor_window_id,
                current_hwnd_binding: Some(200),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id: target_workspace_id,
                column_id: Some(anchor_column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, 420, 900),
                desired_size: Size::default(),
            },
        );
        state
            .layout
            .columns
            .get_mut(&anchor_column_id)
            .expect("anchor column should exist")
            .ordered_window_ids
            .push(anchor_window_id);
        state
            .workspaces
            .get_mut(&target_workspace_id)
            .expect("target workspace should exist")
            .strip
            .ordered_column_ids
            .push(anchor_column_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);

        assert!(overview_workspace_target_is_valid(
            &state,
            target_workspace_id
        ));
        assert_eq!(
            overview_drag_source_column_id(&state, 100),
            Some(source_column_id)
        );
        assert_eq!(
            overview_drop_anchor_column_id(&state, target_workspace_id, Some(200)),
            Some(Some(anchor_column_id))
        );
        assert_eq!(
            overview_drop_anchor_column_id(&state, target_workspace_id, None),
            Some(None)
        );
        assert_eq!(
            overview_drop_anchor_column_id(&state, source_workspace_id, Some(200)),
            None
        );
    }
}
