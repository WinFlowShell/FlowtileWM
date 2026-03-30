#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND},
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{SetActiveWindow, SetFocus},
        WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
            PostMessageW, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow, WM_CLOSE,
        },
    },
};

pub(crate) fn close_window(raw_hwnd: u64) -> Result<(), String> {
    #[cfg(windows)]
    {
        let hwnd = normalize_hwnd(raw_hwnd)?;
        let is_valid = {
            // SAFETY: `hwnd` is validated to be non-null and used only for a read-only liveness check.
            unsafe { IsWindow(hwnd) }
        };
        if is_valid == 0 {
            return Err(format!("focused window hwnd {raw_hwnd} is no longer valid"));
        }

        let posted = {
            // SAFETY: This posts a standard polite close request to a live top-level HWND.
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) }
        };
        if posted == 0 {
            let error = {
                // SAFETY: `GetLastError` is read immediately after the failed Win32 call above.
                unsafe { GetLastError() }
            };
            return Err(format!(
                "PostMessageW(WM_CLOSE) failed with Win32 error {error}"
            ));
        }

        Ok(())
    }

    #[cfg(not(windows))]
    {
        let _ = raw_hwnd;
        Err("close-window is only supported on Windows".to_string())
    }
}

pub(crate) fn activate_window(raw_hwnd: u64) -> Result<(), String> {
    #[cfg(windows)]
    {
        let hwnd = normalize_hwnd(raw_hwnd)?;
        let is_valid = {
            // SAFETY: `hwnd` is validated to be non-null and used only for a read-only liveness check.
            unsafe { IsWindow(hwnd) }
        };
        if is_valid == 0 {
            return Err(format!("target window hwnd {raw_hwnd} is no longer valid"));
        }

        let current_thread_id = {
            // SAFETY: `GetCurrentThreadId` reads the current thread identifier without side effects.
            unsafe { GetCurrentThreadId() }
        };
        let foreground_hwnd = {
            // SAFETY: `GetForegroundWindow` is a read-only query for the current desktop.
            unsafe { GetForegroundWindow() }
        };
        let target_thread_id = window_thread_id(hwnd);
        let foreground_thread_id = if foreground_hwnd.is_null() {
            0
        } else {
            window_thread_id(foreground_hwnd)
        };
        let mut attached_pairs = Vec::new();

        attach_thread_pair(current_thread_id, target_thread_id, &mut attached_pairs);
        attach_thread_pair(current_thread_id, foreground_thread_id, &mut attached_pairs);
        attach_thread_pair(target_thread_id, foreground_thread_id, &mut attached_pairs);

        if is_iconic(hwnd) {
            let _ = {
                // SAFETY: best-effort restore for the validated top-level HWND.
                unsafe { ShowWindow(hwnd, SW_RESTORE) }
            };
        } else {
            let _ = {
                // SAFETY: best-effort visibility hint for the validated top-level HWND.
                unsafe { ShowWindow(hwnd, SW_SHOW) }
            };
        }

        let _ = {
            // SAFETY: best-effort z-order raise for the validated top-level HWND.
            unsafe { BringWindowToTop(hwnd) }
        };
        let _ = {
            // SAFETY: best-effort active-window hint while thread input may be bridged.
            unsafe { SetActiveWindow(hwnd) }
        };
        let _ = {
            // SAFETY: best-effort keyboard focus assignment while thread input may be bridged.
            unsafe { SetFocus(hwnd) }
        };
        let _ = {
            // SAFETY: best-effort foreground activation for the validated top-level HWND.
            unsafe { SetForegroundWindow(hwnd) }
        };
        let _ = {
            // SAFETY: a second topmost raise improves activation reliability on some shells.
            unsafe { BringWindowToTop(hwnd) }
        };
        let _ = {
            // SAFETY: a second foreground attempt is still for the same validated HWND.
            unsafe { SetForegroundWindow(hwnd) }
        };

        let activated = {
            // SAFETY: `GetForegroundWindow` is a read-only query for the current desktop.
            unsafe { GetForegroundWindow() == hwnd }
        };

        for (left, right) in attached_pairs.into_iter().rev() {
            let _ = {
                // SAFETY: detaching mirrors successful `AttachThreadInput` calls made above.
                unsafe { AttachThreadInput(left, right, 0) }
            };
        }

        if activated {
            Ok(())
        } else {
            Err(format!(
                "platform activation path failed to foreground hwnd {raw_hwnd}"
            ))
        }
    }

    #[cfg(not(windows))]
    {
        let _ = raw_hwnd;
        Err("activate-window is only supported on Windows".to_string())
    }
}

#[cfg(windows)]
fn normalize_hwnd(raw_hwnd: u64) -> Result<HWND, String> {
    if raw_hwnd == 0 {
        return Err("focused window has no valid HWND binding".to_string());
    }

    Ok(raw_hwnd as isize as HWND)
}

#[cfg(windows)]
fn attach_thread_pair(left: u32, right: u32, attached_pairs: &mut Vec<(u32, u32)>) {
    if left == 0 || right == 0 || left == right {
        return;
    }

    let attached = {
        // SAFETY: both thread ids come from Win32 and are only bridged temporarily.
        unsafe { AttachThreadInput(left, right, 1) }
    };
    if attached != 0 {
        attached_pairs.push((left, right));
    }
}

#[cfg(windows)]
fn window_thread_id(hwnd: HWND) -> u32 {
    let mut process_id = 0_u32;
    {
        // SAFETY: querying thread ownership for a HWND is a read-only Win32 operation.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) }
    }
}

#[cfg(windows)]
fn is_iconic(hwnd: HWND) -> bool {
    let iconic = {
        // SAFETY: querying whether a window is minimized is a read-only Win32 operation.
        unsafe { IsIconic(hwnd) }
    };
    iconic != 0
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::normalize_hwnd;

    #[cfg(windows)]
    #[test]
    fn rejects_zero_hwnd() {
        let error = normalize_hwnd(0).expect_err("zero hwnd should be rejected");
        assert_eq!(error, "focused window has no valid HWND binding");
    }
}
