use std::{
    mem::zeroed,
    ptr::{null, null_mut},
};

use flowtile_domain::Rect;

#[cfg(not(windows))]
compile_error!("flowtile-core-daemon win32 overlay helpers currently support only Windows.");

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND},
    Graphics::Gdi::HBRUSH,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, HWND_TOPMOST, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, WM_QUIT, WNDCLASSW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    },
};

pub(crate) fn register_basic_window_class(
    instance: HINSTANCE,
    class_name: &str,
    brush: HBRUSH,
) -> Result<(), String> {
    let wide_class_name = widestring(class_name);
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance,
        lpszClassName: wide_class_name.as_ptr(),
        hbrBackground: brush,
        ..unsafe { zeroed() }
    };
    let atom = {
        // SAFETY: the class descriptor references memory that lives for the duration of the call.
        unsafe { RegisterClassW(&window_class) }
    };
    if atom == 0 {
        let error = {
            // SAFETY: read immediately after the failed `RegisterClassW` call.
            unsafe { GetLastError() }
        };
        if error != 1410 {
            return Err(last_error_message("RegisterClassW"));
        }
    }

    Ok(())
}

pub(crate) fn create_layered_overlay_window(
    instance: HINSTANCE,
    class_name: &str,
    alpha: u8,
) -> Result<HWND, String> {
    let wide_class_name = widestring(class_name);
    let window = {
        // SAFETY: creating a no-activate layered popup surface with fixed overlay styles.
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                wide_class_name.as_ptr(),
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        }
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }

    let layered = {
        // SAFETY: sets a constant alpha on the valid overlay HWND.
        unsafe { SetLayeredWindowAttributes(window, 0, alpha, 0x00000002) }
    };
    if layered == 0 {
        return Err(last_error_message("SetLayeredWindowAttributes"));
    }

    Ok(window)
}

pub(crate) fn show_overlay_window(
    window: HWND,
    rect: Rect,
    show_command: i32,
) -> Result<(), String> {
    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "overlay width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "overlay height exceeds Win32 limits".to_string())?;
    let applied = {
        // SAFETY: `window` is a valid overlay HWND owned by the current thread.
        unsafe {
            SetWindowPos(
                window,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    let _ = {
        // SAFETY: best-effort show without activation after geometry update.
        unsafe { ShowWindow(window, show_command) }
    };
    Ok(())
}

pub(crate) fn hide_overlay_window(window: HWND) {
    let _ = {
        // SAFETY: best-effort hide for an overlay HWND owned by this runtime.
        unsafe { ShowWindow(window, SW_HIDE) }
    };
}

pub(crate) fn pump_messages() -> Result<(), String> {
    let mut message: MSG = {
        // SAFETY: `MSG` is plain old data and valid when zero-initialized.
        unsafe { zeroed() }
    };
    loop {
        let has_message = {
            // SAFETY: polls the current thread queue and removes available messages.
            unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) }
        };
        if has_message == 0 {
            break;
        }
        if message.message == WM_QUIT {
            return Ok(());
        }
        let _ = {
            // SAFETY: translate and dispatch the message that was just dequeued.
            unsafe { TranslateMessage(&message) }
        };
        unsafe { DispatchMessageW(&message) };
    }
    Ok(())
}

pub(crate) const fn rgb_color(red: u8, green: u8, blue: u8) -> u32 {
    (red as u32) | ((green as u32) << 8) | ((blue as u32) << 16)
}

pub(crate) fn widestring(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: `GetLastError` reads the current thread-local Win32 error code.
        unsafe { GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}
