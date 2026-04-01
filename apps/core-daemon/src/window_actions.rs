use std::path::Path;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{BOOL, CloseHandle, GetLastError, HWND, LPARAM},
    System::Threading::{
        AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::{
        Input::KeyboardAndMouse::{SetActiveWindow, SetFocus},
        WindowsAndMessaging::{
            BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
            PostMessageW, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow, WM_CLOSE,
        },
    },
};

#[cfg(windows)]
const WALLPAPER_SELECTOR_PROCESS: &str = "flowshellwallpaper.ui";
#[cfg(windows)]
const WALLPAPER_SELECTOR_TITLE: &str = "flowshellwallpaper.ui";

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

pub(crate) fn close_wallpaper_selector_window() -> Result<bool, String> {
    #[cfg(windows)]
    {
        let Some(raw_hwnd) = find_wallpaper_selector_hwnd() else {
            return Ok(false);
        };
        close_window(raw_hwnd)?;
        Ok(true)
    }

    #[cfg(not(windows))]
    {
        Err("close-wallpaper-selector is only supported on Windows".to_string())
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
fn find_wallpaper_selector_hwnd() -> Option<u64> {
    let mut found = None;
    let _ = {
        // SAFETY: the callback receives a pointer to `found` that remains valid for the
        // duration of the synchronous enumeration call.
        unsafe {
            EnumWindows(
                Some(find_wallpaper_selector_enum_proc),
                &mut found as *mut Option<u64> as LPARAM,
            )
        }
    };
    found
}

#[cfg(windows)]
unsafe extern "system" fn find_wallpaper_selector_enum_proc(hwnd: HWND, user_data: LPARAM) -> BOOL {
    if hwnd.is_null() {
        return 1;
    }

    let visible = {
        // SAFETY: read-only visibility probe for the enumerated top-level HWND.
        unsafe { IsWindowVisible(hwnd) != 0 }
    };
    if !visible {
        return 1;
    }

    let process_name = query_process_name_for_window(hwnd);
    let title = query_window_title(hwnd);
    if !is_wallpaper_selector_window(process_name.as_deref(), &title) {
        return 1;
    }

    let found = {
        // SAFETY: `user_data` is a pointer to the enum accumulator passed into the synchronous call.
        unsafe { &mut *(user_data as *mut Option<u64>) }
    };
    *found = Some(hwnd as usize as u64);
    0
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

#[cfg(windows)]
fn is_wallpaper_selector_window(process_name: Option<&str>, title: &str) -> bool {
    normalized_process_name(process_name).as_deref() == Some(WALLPAPER_SELECTOR_PROCESS)
        || title.trim().eq_ignore_ascii_case(WALLPAPER_SELECTOR_TITLE)
}

#[cfg(windows)]
fn normalized_process_name(process_name: Option<&str>) -> Option<String> {
    let process_name = process_name?.trim();
    if process_name.is_empty() {
        return None;
    }

    let lowered = process_name.to_ascii_lowercase();
    Some(
        lowered
            .strip_suffix(".exe")
            .unwrap_or(lowered.as_str())
            .to_string(),
    )
}

#[cfg(windows)]
fn query_process_name_for_window(hwnd: HWND) -> Option<String> {
    let mut process_id = 0_u32;
    {
        // SAFETY: read-only ownership query for the enumerated HWND.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    }
    query_process_name(process_id)
}

#[cfg(windows)]
fn query_process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }

    let process_handle = {
        // SAFETY: read-only process query handle for an existing PID.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) }
    };
    if process_handle.is_null() {
        return None;
    }

    let mut buffer = vec![0_u16; 260];
    let mut length = buffer.len() as u32;
    let queried = {
        // SAFETY: writes at most `length` UTF-16 code units into `buffer`.
        unsafe {
            QueryFullProcessImageNameW(process_handle, 0, buffer.as_mut_ptr(), &mut length) != 0
        }
    };
    let _ = {
        // SAFETY: paired cleanup for the query-only process handle above.
        unsafe { CloseHandle(process_handle) }
    };
    if !queried || length == 0 {
        return None;
    }

    let length = usize::try_from(length).ok()?;
    let path = String::from_utf16_lossy(&buffer[..length]);
    Path::new(path.trim())
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .map(|file_name| file_name.to_string())
}

#[cfg(windows)]
fn query_window_title(hwnd: HWND) -> String {
    let length = {
        // SAFETY: read-only title-length query for the enumerated HWND.
        unsafe { GetWindowTextLengthW(hwnd) }
    };
    if length <= 0 {
        return String::new();
    }

    let mut buffer = vec![0_u16; usize::try_from(length).unwrap_or_default() + 1];
    let copied = {
        // SAFETY: reads the current window title into the allocated buffer.
        unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) }
    };
    if copied <= 0 {
        return String::new();
    }

    String::from_utf16_lossy(&buffer[..usize::try_from(copied).unwrap_or_default()])
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::{is_wallpaper_selector_window, normalize_hwnd, normalized_process_name};

    #[cfg(windows)]
    #[test]
    fn rejects_zero_hwnd() {
        let error = normalize_hwnd(0).expect_err("zero hwnd should be rejected");
        assert_eq!(error, "focused window has no valid HWND binding");
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_process_name_without_exe_suffix() {
        assert_eq!(
            normalized_process_name(Some("FlowShellWallpaper.UI.exe")).as_deref(),
            Some("flowshellwallpaper.ui")
        );
    }

    #[cfg(windows)]
    #[test]
    fn matches_wallpaper_selector_by_process_or_title() {
        assert!(is_wallpaper_selector_window(
            Some("FlowShellWallpaper.UI.exe"),
            "Other title"
        ));
        assert!(is_wallpaper_selector_window(None, "FlowShellWallpaper.UI"));
        assert!(!is_wallpaper_selector_window(
            Some("notepad.exe"),
            "Notepad"
        ));
    }
}
