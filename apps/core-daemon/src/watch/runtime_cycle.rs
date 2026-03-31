use std::{
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use flowtile_windows_adapter::{ObservationStream, ObservationStreamError};
use flowtile_wm_core::CoreDaemonRuntime;

use super::support::record_runtime_report;

pub(super) enum RuntimeCycleFlow {
    NoWork,
    IterationRecorded,
}

pub(super) struct RuntimeCycleContext<'a> {
    pub(super) runtime: &'a mut CoreDaemonRuntime,
    pub(super) observer: &'a mut Option<ObservationStream>,
    pub(super) dry_run: bool,
    pub(super) poll_interval: Duration,
    pub(super) control_response_slice: Duration,
    pub(super) observer_wait_slice: Duration,
    pub(super) next_poll_deadline: &'a mut Instant,
    pub(super) completed_iterations: &'a mut u64,
    pub(super) event_subscribers: &'a mut Vec<mpsc::Sender<String>>,
    pub(super) stream_version: &'a mut u64,
    pub(super) last_streamed_state_version: &'a mut u64,
}

pub(super) fn process_initial_live_snapshot(
    context: &mut RuntimeCycleContext<'_>,
    timeout: Duration,
) -> Result<(), ExitCode> {
    if let Some(live_observer) = context.observer.as_mut() {
        match live_observer.recv_timeout(timeout) {
            Ok(observation) => match context
                .runtime
                .apply_observation(observation, context.dry_run)
            {
                Ok(Some(report)) => {
                    record_runtime_report(
                        context.runtime,
                        context.completed_iterations,
                        context.event_subscribers,
                        context.stream_version,
                        context.last_streamed_state_version,
                        None,
                        &report,
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("{error:?}");
                    return Err(ExitCode::from(1));
                }
            },
            Err(ObservationStreamError::Timeout) => {
                eprintln!(
                    "live observation did not produce an initial snapshot in time; falling back to polling"
                );
                *context.observer = None;
            }
            Err(error) => {
                eprintln!(
                    "live observation failed during startup: {error}; falling back to polling"
                );
                *context.observer = None;
            }
        }
    }

    Ok(())
}

pub(super) fn run_runtime_cycle(
    context: &mut RuntimeCycleContext<'_>,
) -> Result<RuntimeCycleFlow, ExitCode> {
    let mut fallback_to_polling = false;
    let mut advanced_poll_cycle = false;
    let cycle_result = if let Some(live_observer) = context.observer.as_mut() {
        match live_observer.recv_timeout(context.observer_wait_slice) {
            Ok(observation) => context
                .runtime
                .apply_observation(observation, context.dry_run),
            Err(ObservationStreamError::Timeout) => return Ok(RuntimeCycleFlow::NoWork),
            Err(error) => {
                eprintln!("live observation became unavailable: {error}; switching to polling");
                fallback_to_polling = true;
                advanced_poll_cycle = true;
                context.runtime.scan_and_sync(context.dry_run).map(Some)
            }
        }
    } else {
        let now = Instant::now();
        if now < *context.next_poll_deadline {
            thread::sleep((*context.next_poll_deadline - now).min(context.control_response_slice));
            return Ok(RuntimeCycleFlow::NoWork);
        }

        advanced_poll_cycle = true;
        context.runtime.scan_and_sync(context.dry_run).map(Some)
    };

    if fallback_to_polling {
        *context.observer = None;
    }

    let flow = match cycle_result {
        Ok(Some(report)) => {
            record_runtime_report(
                context.runtime,
                context.completed_iterations,
                context.event_subscribers,
                context.stream_version,
                context.last_streamed_state_version,
                None,
                &report,
            );
            RuntimeCycleFlow::IterationRecorded
        }
        Ok(None) => return Ok(RuntimeCycleFlow::NoWork),
        Err(error) => {
            eprintln!("{error:?}");
            return Err(ExitCode::from(1));
        }
    };

    if context.observer.is_none() && advanced_poll_cycle {
        *context.next_poll_deadline = Instant::now() + context.poll_interval;
    }

    Ok(flow)
}
