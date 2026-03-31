mod event_processing;
mod hooks;
mod message_loop;

#[cfg(test)]
mod tests;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT},
};

use self::{
    event_processing::{
        IncrementalApplyResult, RescanSnapshotResult, apply_incremental_event,
        next_periodic_scan_interval, rescan_snapshot, snapshot_envelope,
    },
    hooks::{
        ObserverSignalState, register_hooks, register_thread_state, remove_thread_state, unhook_all,
    },
    message_loop::{drain_message_queue, ensure_message_queue, wait_for_messages},
};
use crate::{
    AdapterPerfTelemetry, LiveObservationOptions, ObserverMessage, WindowsAdapterError, dpi,
    native_snapshot,
};

const RESUME_REVALIDATION_MULTIPLIER: u32 = 3;
const MAX_PERIODIC_SCAN_INTERVAL_MULTIPLIER: u32 = 8;

pub(crate) struct NativeObservationRuntime {
    stop_requested: Arc<AtomicBool>,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl NativeObservationRuntime {
    pub(crate) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        let _ = {
            // SAFETY: `thread_id` belongs to the live observer thread created by this runtime,
            // and `WM_QUIT` is the documented way to stop its message loop.
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) }
        };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn spawn(
    options: LiveObservationOptions,
    sender: Sender<ObserverMessage>,
    perf: Arc<AdapterPerfTelemetry>,
) -> Result<NativeObservationRuntime, WindowsAdapterError> {
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_for_worker = Arc::clone(&stop_requested);
    let (startup_sender, startup_receiver) = mpsc::channel::<Result<u32, String>>();

    let worker = thread::spawn(move || {
        run_observer(options, sender, stop_for_worker, startup_sender, perf);
    });

    let thread_id = startup_receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| WindowsAdapterError::RuntimeFailed {
            component: "native-observer",
            message: format!("observer startup handshake timed out: {error}"),
        })?
        .map_err(|message| WindowsAdapterError::RuntimeFailed {
            component: "native-observer",
            message,
        })?;

    Ok(NativeObservationRuntime {
        stop_requested,
        thread_id,
        worker: Some(worker),
    })
}

fn run_observer(
    options: LiveObservationOptions,
    sender: Sender<ObserverMessage>,
    stop_requested: Arc<AtomicBool>,
    startup_sender: Sender<Result<u32, String>>,
    perf: Arc<AdapterPerfTelemetry>,
) {
    let thread_id = {
        // SAFETY: `GetCurrentThreadId` is a parameterless Win32 query for the current thread.
        unsafe { GetCurrentThreadId() }
    };
    if let Err(message) = dpi::ensure_current_thread_per_monitor_v2("native-observer") {
        let _ = startup_sender.send(Err(message));
        return;
    }
    ensure_message_queue();

    let shared = Arc::new(ObserverSignalState::default());
    register_thread_state(thread_id, Arc::clone(&shared));

    let hooks = match register_hooks() {
        Ok(hooks) => hooks,
        Err(error) => {
            let _ = startup_sender.send(Err(error));
            remove_thread_state(thread_id);
            return;
        }
    };

    let mut snapshot = match native_snapshot::scan_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = startup_sender.send(Err(error.to_string()));
            unhook_all(&hooks);
            remove_thread_state(thread_id);
            return;
        }
    };
    if sender
        .send(ObserverMessage::Envelope(snapshot_envelope(
            "initial-full-scan",
            snapshot.clone(),
        )))
        .is_err()
    {
        unhook_all(&hooks);
        remove_thread_state(thread_id);
        return;
    }
    let _ = startup_sender.send(Ok(thread_id));

    let fallback_interval = Duration::from_millis(options.fallback_scan_interval_ms.max(1_000));
    let max_periodic_scan_interval =
        fallback_interval.saturating_mul(MAX_PERIODIC_SCAN_INTERVAL_MULTIPLIER);
    let mut periodic_scan_interval = fallback_interval;
    let debounce = Duration::from_millis(options.debounce_ms.max(1));
    let mut last_emit_at = Instant::now();
    let mut last_periodic_scan_at = last_emit_at;
    let mut last_loop_at = last_emit_at;

    while !stop_requested.load(Ordering::Acquire) {
        wait_for_messages();
        if !drain_message_queue() {
            break;
        }

        let now = Instant::now();
        if now.duration_since(last_loop_at)
            >= fallback_interval.saturating_mul(RESUME_REVALIDATION_MULTIPLIER)
        {
            match rescan_snapshot("resume-revalidation", &sender, &mut snapshot, &perf) {
                RescanSnapshotResult::Stop => break,
                RescanSnapshotResult::Changed | RescanSnapshotResult::Warning => {
                    last_emit_at = now;
                }
                RescanSnapshotResult::Unchanged => {}
            }
            last_periodic_scan_at = now;
            periodic_scan_interval = fallback_interval;
            shared.clear_pending();
        } else if shared.pending.load(Ordering::Acquire)
            && now.duration_since(last_emit_at) >= debounce
        {
            let event_type = shared.last_event_type.swap(0, Ordering::AcqRel);
            let hwnd = shared.last_hwnd.swap(0, Ordering::AcqRel) as u64;
            shared.pending.store(false, Ordering::Release);

            match apply_incremental_event(event_type, hwnd, &sender, &mut snapshot, &perf) {
                IncrementalApplyResult::Continue => {
                    periodic_scan_interval = fallback_interval;
                }
                IncrementalApplyResult::Rescanned => {
                    last_periodic_scan_at = now;
                    periodic_scan_interval = fallback_interval;
                }
                IncrementalApplyResult::Stop => break,
            }
            last_emit_at = now;
        } else if now.duration_since(last_periodic_scan_at) >= periodic_scan_interval {
            match rescan_snapshot("periodic-full-scan", &sender, &mut snapshot, &perf) {
                RescanSnapshotResult::Changed | RescanSnapshotResult::Warning => {
                    last_emit_at = now;
                    periodic_scan_interval = fallback_interval;
                }
                RescanSnapshotResult::Unchanged => {
                    periodic_scan_interval = next_periodic_scan_interval(
                        periodic_scan_interval,
                        fallback_interval,
                        max_periodic_scan_interval,
                    );
                }
                RescanSnapshotResult::Stop => break,
            }
            last_periodic_scan_at = now;
        }

        last_loop_at = now;
    }

    unhook_all(&hooks);
    remove_thread_state(thread_id);
}
