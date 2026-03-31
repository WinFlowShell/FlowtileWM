use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND},
    System::Threading::GetCurrentThreadId,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
    },
};

#[derive(Default)]
pub(super) struct ObserverSignalState {
    pub(super) pending: AtomicBool,
    pub(super) last_event_type: AtomicU32,
    pub(super) last_hwnd: AtomicUsize,
}

impl ObserverSignalState {
    pub(super) fn clear_pending(&self) {
        self.pending.store(false, Ordering::Release);
        self.last_event_type.store(0, Ordering::Release);
        self.last_hwnd.store(0, Ordering::Release);
    }
}

pub(super) fn register_hooks() -> Result<Vec<HWINEVENTHOOK>, String> {
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

pub(super) fn unhook_all(hooks: &[HWINEVENTHOOK]) {
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

pub(super) fn register_thread_state(thread_id: u32, state: Arc<ObserverSignalState>) {
    if let Ok(mut registry) = registry().lock() {
        registry.insert(thread_id, state);
    }
}

pub(super) fn remove_thread_state(thread_id: u32) {
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
