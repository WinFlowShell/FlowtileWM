use std::{
    collections::HashMap,
    mem::zeroed,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, WAIT_TIMEOUT},
    System::Threading::GetCurrentThreadId,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
            EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, MSG,
            MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, OBJID_WINDOW, PM_NOREMOVE, PM_REMOVE,
            PeekMessageW, PostThreadMessageW, QS_ALLINPUT, TranslateMessage, WINEVENT_OUTOFCONTEXT,
            WINEVENT_SKIPOWNPROCESS, WM_QUIT,
        },
    },
};

use crate::{
    LiveObservationOptions, ObservationEnvelope, ObservationKind, ObserverMessage,
    WindowsAdapterError, dpi, native_snapshot,
};

const RESUME_REVALIDATION_MULTIPLIER: u32 = 3;

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
) -> Result<NativeObservationRuntime, WindowsAdapterError> {
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_for_worker = Arc::clone(&stop_requested);
    let (startup_sender, startup_receiver) = mpsc::channel::<Result<u32, String>>();

    let worker = thread::spawn(move || {
        run_observer(options, sender, stop_for_worker, startup_sender);
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
            if !rescan_snapshot("resume-revalidation", &sender, &mut snapshot) {
                break;
            }
            last_emit_at = now;
            last_periodic_scan_at = now;
            shared.clear_pending();
        } else if shared.pending.load(Ordering::Acquire)
            && now.duration_since(last_emit_at) >= debounce
        {
            let event_type = shared.last_event_type.swap(0, Ordering::AcqRel);
            let hwnd = shared.last_hwnd.swap(0, Ordering::AcqRel) as u64;
            shared.pending.store(false, Ordering::Release);

            match apply_incremental_event(event_type, hwnd, &sender, &mut snapshot) {
                IncrementalApplyResult::Continue => {}
                IncrementalApplyResult::Rescanned => {
                    last_periodic_scan_at = now;
                }
                IncrementalApplyResult::Stop => break,
            }
            last_emit_at = now;
        } else if now.duration_since(last_periodic_scan_at) >= fallback_interval {
            if !rescan_snapshot("periodic-full-scan", &sender, &mut snapshot) {
                break;
            }
            last_emit_at = now;
            last_periodic_scan_at = now;
        }

        last_loop_at = now;
    }

    unhook_all(&hooks);
    remove_thread_state(thread_id);
}

fn apply_incremental_event(
    event_type: u32,
    hwnd: u64,
    sender: &Sender<ObserverMessage>,
    snapshot: &mut crate::PlatformSnapshot,
) -> IncrementalApplyResult {
    let reason = event_reason(event_type);
    let hwnd_known_before = snapshot_contains_hwnd(snapshot, hwnd);
    let updated = match event_type {
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => {
            native_snapshot::remove_window(snapshot, hwnd);
            native_snapshot::refresh_focus(snapshot)
        }
        _ => native_snapshot::refresh_window(snapshot, hwnd),
    };

    match updated {
        Ok(()) => {
            let hwnd_known_after = snapshot_contains_hwnd(snapshot, hwnd);
            if should_rescan_after_incremental_event(
                event_type,
                hwnd_known_before,
                hwnd_known_after,
            ) {
                if rescan_snapshot("event-recovery-full-scan", sender, snapshot) {
                    IncrementalApplyResult::Rescanned
                } else {
                    IncrementalApplyResult::Stop
                }
            } else if sender
                .send(ObserverMessage::Envelope(snapshot_envelope(
                    reason,
                    snapshot.clone(),
                )))
                .is_ok()
            {
                IncrementalApplyResult::Continue
            } else {
                IncrementalApplyResult::Stop
            }
        }
        Err(message) => {
            if sender
                .send(ObserverMessage::Envelope(warning_envelope(
                    reason, &message,
                )))
                .is_err()
            {
                return IncrementalApplyResult::Stop;
            }
            if rescan_snapshot("event-recovery-full-scan", sender, snapshot) {
                IncrementalApplyResult::Rescanned
            } else {
                IncrementalApplyResult::Stop
            }
        }
    }
}

fn snapshot_contains_hwnd(snapshot: &crate::PlatformSnapshot, hwnd: u64) -> bool {
    snapshot.windows.iter().any(|window| window.hwnd == hwnd)
}

fn should_rescan_after_incremental_event(
    event_type: u32,
    hwnd_known_before: bool,
    hwnd_known_after: bool,
) -> bool {
    match event_type {
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE => !(hwnd_known_before && hwnd_known_after),
        EVENT_OBJECT_CREATE | EVENT_OBJECT_DESTROY | EVENT_SYSTEM_FOREGROUND => {
            !hwnd_known_after && !hwnd_known_before
        }
        _ => false,
    }
}

fn rescan_snapshot(
    reason: &str,
    sender: &Sender<ObserverMessage>,
    snapshot: &mut crate::PlatformSnapshot,
) -> bool {
    match native_snapshot::scan_snapshot() {
        Ok(new_snapshot) => {
            *snapshot = new_snapshot.clone();
            sender
                .send(ObserverMessage::Envelope(snapshot_envelope(
                    reason,
                    new_snapshot,
                )))
                .is_ok()
        }
        Err(error) => sender
            .send(ObserverMessage::Envelope(warning_envelope(
                reason,
                &error.to_string(),
            )))
            .is_ok(),
    }
}

fn register_hooks() -> Result<Vec<HWINEVENTHOOK>, String> {
    let mut hooks = Vec::new();
    for (event_min, event_max) in [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
    ] {
        let hook = {
            // SAFETY: We register a static callback function for documented WinEvent ranges and
            // request out-of-context notifications for the whole desktop session.
            unsafe {
                SetWinEventHook(
                    event_min,
                    event_max,
                    std::ptr::null_mut(),
                    Some(win_event_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            }
        };
        if hook.is_null() {
            unhook_all(&hooks);
            return Err(last_error_message("SetWinEventHook"));
        }
        hooks.push(hook);
    }

    Ok(hooks)
}

fn unhook_all(hooks: &[HWINEVENTHOOK]) {
    for hook in hooks {
        if hook.is_null() {
            continue;
        }

        let _ = {
            // SAFETY: Each hook in `hooks` was returned by `SetWinEventHook` in this thread and
            // is being released exactly once during shutdown.
            unsafe { UnhookWinEvent(*hook) }
        };
    }
}

unsafe extern "system" fn win_event_callback(
    _: HWINEVENTHOOK,
    event_type: u32,
    window_handle: HWND,
    object_id: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if window_handle.is_null() {
        return;
    }
    if event_type != EVENT_SYSTEM_FOREGROUND && object_id != OBJID_WINDOW {
        return;
    }

    let thread_id = {
        // SAFETY: `GetCurrentThreadId` is a parameterless Win32 query for the callback thread.
        unsafe { GetCurrentThreadId() }
    };
    let shared = registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&thread_id).cloned());
    if let Some(shared) = shared {
        shared.last_event_type.store(event_type, Ordering::Release);
        shared
            .last_hwnd
            .store(window_handle as usize, Ordering::Release);
        shared.pending.store(true, Ordering::Release);
    }
}

fn ensure_message_queue() {
    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 message structure that is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let _ = {
        // SAFETY: `PeekMessageW` with `PM_NOREMOVE` forces the current thread to own a message
        // queue before we start posting or waiting on messages.
        unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) }
    };
}

fn wait_for_messages() {
    let wait_result = {
        // SAFETY: We do not wait on kernel handles here; we only ask Win32 to wake on any input
        // queue activity for the current thread.
        unsafe {
            MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 100, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        }
    };

    if wait_result == WAIT_TIMEOUT {
        std::thread::yield_now();
    }
}

fn drain_message_queue() -> bool {
    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 message structure that is valid when zero-initialized.
        unsafe { zeroed() }
    };

    loop {
        let has_message = {
            // SAFETY: `PeekMessageW` reads queued messages for the current thread and writes them
            // into the `message` buffer.
            unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 }
        };
        if !has_message {
            return true;
        }
        if message.message == WM_QUIT {
            return false;
        }

        let _ = {
            // SAFETY: `message` was just read from the current thread's queue.
            unsafe { TranslateMessage(&message) }
        };
        let _ = {
            // SAFETY: `message` was just read from the current thread's queue.
            unsafe { DispatchMessageW(&message) }
        };
    }
}

fn snapshot_envelope(reason: &str, snapshot: crate::PlatformSnapshot) -> ObservationEnvelope {
    ObservationEnvelope {
        kind: ObservationKind::Snapshot,
        reason: reason.to_string(),
        snapshot: Some(snapshot),
        message: None,
    }
}

fn warning_envelope(reason: &str, message: &str) -> ObservationEnvelope {
    ObservationEnvelope {
        kind: ObservationKind::Warning,
        reason: reason.to_string(),
        snapshot: None,
        message: Some(message.to_string()),
    }
}

fn event_reason(event_type: u32) -> &'static str {
    match event_type {
        EVENT_SYSTEM_FOREGROUND => "win-event-foreground",
        EVENT_OBJECT_CREATE => "win-event-create",
        EVENT_OBJECT_DESTROY => "win-event-destroy",
        EVENT_OBJECT_SHOW => "win-event-show",
        EVENT_OBJECT_HIDE => "win-event-hide",
        EVENT_OBJECT_LOCATIONCHANGE => "win-event-location-change",
        _ => "win-event-update",
    }
}

fn register_thread_state(thread_id: u32, state: Arc<ObserverSignalState>) {
    if let Ok(mut registry) = registry().lock() {
        registry.insert(thread_id, state);
    }
}

fn remove_thread_state(thread_id: u32) {
    if let Ok(mut registry) = registry().lock() {
        registry.remove(&thread_id);
    }
}

fn registry() -> &'static Mutex<HashMap<u32, Arc<ObserverSignalState>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, Arc<ObserverSignalState>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: Reading the thread-local Win32 last-error code immediately after a failed API
        // call is the intended contract of `GetLastError`.
        unsafe { GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}

#[derive(Default)]
struct ObserverSignalState {
    pending: AtomicBool,
    last_event_type: AtomicU32,
    last_hwnd: AtomicUsize,
}

enum IncrementalApplyResult {
    Continue,
    Rescanned,
    Stop,
}

impl ObserverSignalState {
    fn clear_pending(&self) {
        self.pending.store(false, Ordering::Release);
        self.last_event_type.store(0, Ordering::Release);
        self.last_hwnd.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    };

    use super::should_rescan_after_incremental_event;

    #[test]
    fn create_for_unknown_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_CREATE,
            false,
            false,
        ));
    }

    #[test]
    fn show_for_unknown_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_SHOW,
            false,
            false,
        ));
    }

    #[test]
    fn foreground_for_unknown_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_SYSTEM_FOREGROUND,
            false,
            false,
        ));
    }

    #[test]
    fn create_for_known_hwnd_does_not_force_rescan() {
        assert!(!should_rescan_after_incremental_event(
            EVENT_OBJECT_CREATE,
            true,
            true,
        ));
    }

    #[test]
    fn hide_for_known_hwnd_membership_change_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_HIDE,
            true,
            false,
        ));
    }

    #[test]
    fn show_for_restored_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_SHOW,
            false,
            true,
        ));
    }

    #[test]
    fn location_change_for_unknown_hwnd_does_not_force_rescan() {
        assert!(!should_rescan_after_incremental_event(
            EVENT_OBJECT_LOCATIONCHANGE,
            false,
            false,
        ));
    }

    #[test]
    fn destroy_for_unknown_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_DESTROY,
            false,
            false,
        ));
    }

    #[test]
    fn hide_for_unknown_hwnd_escalates_to_full_scan() {
        assert!(should_rescan_after_incremental_event(
            EVENT_OBJECT_HIDE,
            false,
            false,
        ));
    }
}
