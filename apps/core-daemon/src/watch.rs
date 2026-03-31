use std::{
    process::ExitCode,
    sync::mpsc,
    time::{Duration, Instant},
};

mod commands;
mod control_plane;
mod runtime_cycle;
mod startup;
mod support;

use commands::WatchCommandFlow;
use control_plane::{ControlPlaneContext, handle_control_message};
use flowtile_domain::RuntimeMode;
use flowtile_windows_adapter::WindowsAdapter;
use flowtile_wm_core::CoreDaemonRuntime;
use runtime_cycle::{
    RuntimeCycleContext, RuntimeCycleFlow, process_initial_live_snapshot, run_runtime_cycle,
};
use startup::{initialize_watch_startup, print_watch_banner};

use crate::{diag::write_runtime_log, manual_resize::ManualResizeError};
use support::{
    record_runtime_report, sync_visual_surfaces, validate_runtime_bind_control_mode,
    validate_runtime_touchpad_override,
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
    let startup = match initialize_watch_startup(&runtime, &adapter, poll_only, interval_ms) {
        Ok(startup) => startup,
        Err(code) => return code,
    };
    let startup::WatchStartup {
        mut observer,
        control_sender,
        control_receiver,
        mut hotkey_listener,
        mut touchpad_listener,
        mut manual_resize,
        mut tab_indicator,
        mut overview_surface,
    } = startup;
    print_watch_banner(&runtime, &observer, &hotkey_listener, &touchpad_listener);

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

    if let Err(code) = process_initial_live_snapshot(
        &mut RuntimeCycleContext {
            runtime: &mut runtime,
            observer: &mut observer,
            dry_run,
            poll_interval,
            control_response_slice,
            observer_wait_slice,
            next_poll_deadline: &mut next_poll_deadline,
            completed_iterations: &mut completed_iterations,
            event_subscribers: &mut event_subscribers,
            stream_version: &mut stream_version,
            last_streamed_state_version: &mut last_streamed_state_version,
        },
        Duration::from_millis(interval_ms.max(5_000)),
    ) {
        return code;
    }
    if iterations.is_some_and(|limit| completed_iterations >= limit) {
        return ExitCode::SUCCESS;
    }

    loop {
        sync_visual_surfaces(
            &runtime,
            &control_sender,
            &mut tab_indicator,
            &mut overview_surface,
        );

        while let Ok(message) = control_receiver.try_recv() {
            let flow = handle_control_message(
                message,
                &mut ControlPlaneContext {
                    runtime: &mut runtime,
                    dry_run,
                    control_sender: &control_sender,
                    hotkey_listener: &mut hotkey_listener,
                    touchpad_listener: &mut touchpad_listener,
                    tab_indicator: &mut tab_indicator,
                    overview_surface: &mut overview_surface,
                    completed_iterations: &mut completed_iterations,
                    manual_correlation_id: &mut manual_correlation_id,
                    event_subscribers: &mut event_subscribers,
                    stream_version: &mut stream_version,
                    last_streamed_state_version: &mut last_streamed_state_version,
                },
            );
            match flow {
                WatchCommandFlow::Continue => {}
                WatchCommandFlow::Quit => {
                    write_runtime_log("watch: quit-command-received");
                    return ExitCode::SUCCESS;
                }
                WatchCommandFlow::Exit(code) => return code,
            }

            if iterations.is_some_and(|limit| completed_iterations >= limit) {
                return ExitCode::SUCCESS;
            }
        }

        match manual_resize.tick(&mut runtime, dry_run) {
            Ok(Some(report)) => {
                record_runtime_report(
                    &runtime,
                    &mut completed_iterations,
                    &mut event_subscribers,
                    &mut stream_version,
                    &mut last_streamed_state_version,
                    Some("manual command: column-width-drag"),
                    &report,
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

        match run_runtime_cycle(&mut RuntimeCycleContext {
            runtime: &mut runtime,
            observer: &mut observer,
            dry_run,
            poll_interval,
            control_response_slice,
            observer_wait_slice,
            next_poll_deadline: &mut next_poll_deadline,
            completed_iterations: &mut completed_iterations,
            event_subscribers: &mut event_subscribers,
            stream_version: &mut stream_version,
            last_streamed_state_version: &mut last_streamed_state_version,
        }) {
            Ok(RuntimeCycleFlow::NoWork) => continue,
            Ok(RuntimeCycleFlow::IterationRecorded) => {}
            Err(code) => return code,
        }

        if iterations.is_some_and(|limit| completed_iterations >= limit) {
            write_runtime_log(format!(
                "watch: iteration-limit-reached completed_iterations={completed_iterations}"
            ));
            return ExitCode::SUCCESS;
        }
    }
}

#[cfg(test)]
mod tests {
    use flowtile_domain::{
        Column, ColumnMode, Rect, RuntimeMode, Size, WidthSemantics, WindowClassification,
        WindowLayer, WindowNode, WmState,
    };

    use super::support::{
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
