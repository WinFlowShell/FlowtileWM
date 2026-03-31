use std::mem::zeroed;

use windows_sys::Win32::{
    Foundation::WAIT_TIMEOUT,
    UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE,
        PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
    },
};

pub(super) fn ensure_message_queue() {
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

pub(super) fn wait_for_messages() {
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

pub(super) fn drain_message_queue() -> bool {
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
