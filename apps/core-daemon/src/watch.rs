use std::{
    io::{self, BufRead},
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use flowtile_domain::{CorrelationId, DomainEvent, NavigationScope, RuntimeMode};
use flowtile_ipc::bootstrap as ipc_bootstrap;
use flowtile_windows_adapter::{LiveObservationOptions, ObservationStreamError, WindowsAdapter};
use flowtile_wm_core::{CoreDaemonRuntime, RuntimeCycleReport};

use crate::{
    control::{ControlMessage, WatchCommand},
    hotkeys::{HotkeyListener, HotkeyListenerError, ensure_bind_control_mode_supported},
    ipc,
};

const CONTROL_RESPONSE_SLICE_MS: u64 = 16;

pub(crate) fn run_watch(
    runtime_mode: RuntimeMode,
    dry_run: bool,
    interval_ms: u64,
    iterations: Option<u64>,
    poll_only: bool,
) -> ExitCode {
    let adapter = WindowsAdapter::new();
    let mut runtime = CoreDaemonRuntime::with_adapter(runtime_mode, adapter.clone());
    if let Err(error) = validate_runtime_bind_control_mode(&runtime) {
        eprintln!("bind control mode startup failed: {error}");
        return ExitCode::from(1);
    }
    let mut observer = if poll_only {
        None
    } else {
        match adapter.spawn_observer(LiveObservationOptions {
            fallback_scan_interval_ms: interval_ms.max(1_000),
            ..LiveObservationOptions::default()
        }) {
            Ok(stream) => Some(stream),
            Err(error) => {
                eprintln!("live observation failed to start: {error}; falling back to polling");
                None
            }
        }
    };

    let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
    ipc::spawn_ipc_servers(control_sender.clone());
    let mut hotkey_listener = match start_hotkey_listener(&runtime, &control_sender) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("global hotkeys failed to start: {error}");
            return ExitCode::from(1);
        }
    };
    spawn_stdin_listener(control_sender.clone());

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
        "ipc command pipe: {} | event stream pipe: {}",
        ipc.command_pipe_name, ipc.event_stream_pipe_name
    );
    println!(
        "stdin commands: focus-next, focus-prev, scroll-left, scroll-right, toggle-floating, toggle-tabbed, toggle-maximized, toggle-fullscreen, toggle-overview, reload-config, snapshot, unwind, rescan, quit"
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
                            println!(
                                "global hotkeys reloaded: {}",
                                if hotkey_listener.is_some() {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
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
                    WatchCommand::Quit => return ExitCode::SUCCESS,
                },
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
                        println!(
                            "global hotkeys reloaded via IPC: {}",
                            if hotkey_listener.is_some() {
                                "enabled"
                            } else {
                                "disabled"
                            }
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

fn print_iteration(iteration: u64, report: &RuntimeCycleReport) {
    println!("iteration {iteration}");
    print_report(report);
}

fn print_report(report: &RuntimeCycleReport) {
    for line in report.summary_lines() {
        println!("{line}");
    }
}

fn print_state_snapshot(runtime: &CoreDaemonRuntime) {
    let state = runtime.state();
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
