use std::convert::TryFrom;
use std::mem::zeroed;
use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, RECT},
    Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
            SetActiveWindow, SetFocus, VK_MENU,
        },
        WindowsAndMessaging::{
            BeginDeferWindowPos, BringWindowToTop, DeferWindowPos, EndDeferWindowPos,
            GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
            SW_SHOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW,
            SetForegroundWindow, SetWindowPos, ShowWindow,
        },
    },
};

use crate::{ApplyBatchResult, ApplyFailure, ApplyOperation};

const GEOMETRY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const ERROR_ACCESS_DENIED: u32 = 5;

pub(crate) fn apply_operations(operations: &[ApplyOperation]) -> ApplyBatchResult {
    if operations.len() > 1 && try_apply_operations_batched(operations).is_ok() {
        let mut applied = operations.len();
        let mut failures = Vec::new();

        for operation in operations.iter().filter(|operation| operation.activate) {
            if let Err(message) = activate_window(operation.hwnd) {
                applied = applied.saturating_sub(1);
                failures.push(ApplyFailure {
                    hwnd: operation.hwnd,
                    message,
                });
            }
        }

        return ApplyBatchResult {
            attempted: operations.len(),
            applied,
            failures,
        };
    }

    apply_operations_individually(operations)
}

fn apply_operations_individually(operations: &[ApplyOperation]) -> ApplyBatchResult {
    let mut failures = Vec::new();
    let mut applied = 0_usize;

    for operation in operations {
        match apply_operation(operation) {
            Ok(()) => {
                applied += 1;
            }
            Err(message) => failures.push(ApplyFailure {
                hwnd: operation.hwnd,
                message,
            }),
        }
    }

    ApplyBatchResult {
        attempted: operations.len(),
        applied,
        failures,
    }
}

fn try_apply_operations_batched(operations: &[ApplyOperation]) -> Result<(), String> {
    let Some(batch_capacity) = i32::try_from(operations.len()).ok() else {
        return Err("operation batch is larger than supported Win32 defer capacity".to_string());
    };

    let mut batch = {
        // SAFETY: The Win32 API requires a raw call to allocate a defer-window-position batch.
        // The count comes from the slice length and does not outlive this function.
        unsafe { BeginDeferWindowPos(batch_capacity) }
    };
    if batch.is_null() {
        return Err(last_error_message("BeginDeferWindowPos"));
    }

    for operation in operations {
        let hwnd = hwnd_from_raw(operation.hwnd)?;
        let translated = translated_window_rect(hwnd, operation)?;
        let next_batch = {
            // SAFETY: `batch` is a handle returned by `BeginDeferWindowPos` or a previous
            // `DeferWindowPos` call. `hwnd` is derived from the raw HWND recorded in state and
            // the rectangle values are simple POD integers.
            unsafe {
                DeferWindowPos(
                    batch,
                    hwnd,
                    null_mut(),
                    translated.x,
                    translated.y,
                    translated.width,
                    translated.height,
                    GEOMETRY_APPLY_FLAGS,
                )
            }
        };

        if next_batch.is_null() {
            let _ = {
                // SAFETY: Best-effort cleanup for the partially constructed batch before we
                // fall back to individual SetWindowPos calls.
                unsafe { EndDeferWindowPos(batch) }
            };
            return Err(last_error_message("DeferWindowPos"));
        }

        batch = next_batch;
    }

    let committed = {
        // SAFETY: `batch` is the final defer handle built above and is committed exactly once.
        unsafe { EndDeferWindowPos(batch) }
    };
    if committed == 0 {
        return Err(last_error_message("EndDeferWindowPos"));
    }

    Ok(())
}

fn apply_operation(operation: &ApplyOperation) -> Result<(), String> {
    match apply_geometry(operation) {
        Ok(()) => {}
        Err(error) => {
            if operation.activate && error.code == ERROR_ACCESS_DENIED {
                activate_window(operation.hwnd).map_err(|activation_error| {
                    format!(
                        "{}; activation fallback failed: {}",
                        error.message, activation_error
                    )
                })?;
                return Ok(());
            }

            return Err(error.message);
        }
    }

    if operation.activate {
        activate_window(operation.hwnd)?;
    }

    Ok(())
}

fn apply_geometry(operation: &ApplyOperation) -> Result<(), Win32ApiError> {
    let hwnd =
        hwnd_from_raw(operation.hwnd).map_err(|message| Win32ApiError { code: 0, message })?;
    let translated = translated_window_rect(hwnd, operation)
        .map_err(|message| Win32ApiError { code: 0, message })?;
    let applied = {
        // SAFETY: `hwnd` is reconstructed from the raw HWND tracked by the runtime and the rest
        // of the parameters are primitive coordinates and flags forwarded directly to Win32.
        unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                translated.x,
                translated.y,
                translated.width,
                translated.height,
                GEOMETRY_APPLY_FLAGS,
            )
        }
    };

    if applied == 0 {
        return Err(last_error("SetWindowPos"));
    }

    Ok(())
}

fn activate_window(raw_hwnd: u64) -> Result<(), String> {
    let hwnd = hwnd_from_raw(raw_hwnd)?;
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
        // SAFETY: best-effort topmost z-order raise for the validated top-level HWND.
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

    let mut activation_succeeded = {
        // SAFETY: `GetForegroundWindow` is a read-only query for the current desktop.
        unsafe { GetForegroundWindow() == hwnd }
    };
    if !activation_succeeded {
        unlock_foreground_with_alt();
        let _ = {
            // SAFETY: best-effort topmost raise after foreground unlock.
            unsafe { BringWindowToTop(hwnd) }
        };
        let _ = {
            // SAFETY: final foreground retry after synthetic Alt unlock.
            unsafe { SetForegroundWindow(hwnd) }
        };
        activation_succeeded = {
            // SAFETY: `GetForegroundWindow` is a read-only query for the current desktop.
            unsafe { GetForegroundWindow() == hwnd }
        };
    }

    for (left, right) in attached_pairs.into_iter().rev() {
        let _ = {
            // SAFETY: detaching mirrors successful `AttachThreadInput` calls made above.
            unsafe { AttachThreadInput(left, right, 0) }
        };
    }

    if activation_succeeded {
        Ok(())
    } else {
        Err(format!(
            "platform activation path failed to foreground hwnd {raw_hwnd}"
        ))
    }
}

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

fn window_thread_id(hwnd: HWND) -> u32 {
    let mut process_id = 0_u32;
    {
        // SAFETY: querying thread ownership for a HWND is a read-only Win32 operation.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) }
    }
}

fn is_iconic(hwnd: HWND) -> bool {
    let iconic = {
        // SAFETY: querying whether a window is minimized is a read-only Win32 operation.
        unsafe { IsIconic(hwnd) }
    };
    iconic != 0
}

fn unlock_foreground_with_alt() {
    let mut inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: 0,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];

    let _ = {
        // SAFETY: sending a synthetic Alt tap is a best-effort user-mode foreground unlock
        // fallback and uses a fixed local INPUT buffer with the documented size.
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_mut_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        }
    };
}

fn hwnd_from_raw(hwnd: u64) -> Result<HWND, String> {
    let raw = isize::try_from(hwnd)
        .map_err(|_| format!("window handle {hwnd} does not fit pointer width"))?;
    Ok(raw as HWND)
}

#[derive(Debug)]
struct Win32ApiError {
    code: u32,
    message: String,
}

fn rect_size(operation: &ApplyOperation) -> Result<(i32, i32), String> {
    let width = i32::try_from(operation.rect.width).map_err(|_| {
        format!(
            "window {} width {} exceeds Win32 coordinate range",
            operation.hwnd, operation.rect.width
        )
    })?;
    let height = i32::try_from(operation.rect.height).map_err(|_| {
        format!(
            "window {} height {} exceeds Win32 coordinate range",
            operation.hwnd, operation.rect.height
        )
    })?;

    Ok((width, height))
}

fn translated_window_rect(
    hwnd: HWND,
    operation: &ApplyOperation,
) -> Result<TranslatedRect, String> {
    let (visible_width, visible_height) = rect_size(operation)?;
    let outer_rect = query_outer_window_rect(hwnd)?;
    let visible_rect = query_visible_frame_rect(hwnd)
        .filter(|visible_rect| visible_frame_is_compatible(outer_rect, *visible_rect))
        .unwrap_or(outer_rect);
    let insets = frame_insets(outer_rect, visible_rect);

    let width = visible_width
        .checked_add(insets.left)
        .and_then(|value| value.checked_add(insets.right))
        .ok_or_else(|| format!("window {} translated width overflowed", operation.hwnd))?;
    let height = visible_height
        .checked_add(insets.top)
        .and_then(|value| value.checked_add(insets.bottom))
        .ok_or_else(|| format!("window {} translated height overflowed", operation.hwnd))?;

    Ok(TranslatedRect {
        x: operation.rect.x.saturating_sub(insets.left),
        y: operation.rect.y.saturating_sub(insets.top),
        width,
        height,
    })
}

fn query_outer_window_rect(hwnd: HWND) -> Result<RECT, String> {
    let mut rect: RECT = {
        // SAFETY: `RECT` is a plain Win32 structure and is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let ok = {
        // SAFETY: `rect` points to writable memory for the synchronous Win32 call.
        unsafe { GetWindowRect(hwnd, &mut rect) != 0 }
    };
    if !ok {
        return Err(last_error_message("GetWindowRect"));
    }
    Ok(rect)
}

fn query_visible_frame_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect: RECT = {
        // SAFETY: `RECT` is a plain Win32 structure and is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let result = {
        // SAFETY: We pass a valid pointer to a writable `RECT` buffer with the documented size.
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
        }
    };
    (result >= 0).then_some(rect)
}

fn frame_insets(outer_rect: RECT, visible_rect: RECT) -> FrameInsets {
    FrameInsets {
        left: (visible_rect.left - outer_rect.left).max(0),
        top: (visible_rect.top - outer_rect.top).max(0),
        right: (outer_rect.right - visible_rect.right).max(0),
        bottom: (outer_rect.bottom - visible_rect.bottom).max(0),
    }
}

fn visible_frame_is_compatible(outer_rect: RECT, visible_rect: RECT) -> bool {
    rect_is_non_empty(outer_rect)
        && rect_is_non_empty(visible_rect)
        && visible_rect.left >= outer_rect.left
        && visible_rect.top >= outer_rect.top
        && visible_rect.right <= outer_rect.right
        && visible_rect.bottom <= outer_rect.bottom
}

fn rect_is_non_empty(rect: RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameInsets {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TranslatedRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn last_error_message(api: &str) -> String {
    last_error(api).message
}

fn last_error(api: &str) -> Win32ApiError {
    let code = {
        // SAFETY: Reading the thread-local Win32 last-error code immediately after a failed API
        // call is the intended contract of `GetLastError`.
        unsafe { GetLastError() }
    };
    Win32ApiError {
        code,
        message: format!("{api} failed with Win32 error {code}"),
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::RECT;

    use super::{FrameInsets, frame_insets, visible_frame_is_compatible};

    #[test]
    fn frame_insets_follow_visible_dwm_bounds_inside_outer_window_rect() {
        let outer = RECT {
            left: 100,
            top: 50,
            right: 540,
            bottom: 460,
        };
        let visible = RECT {
            left: 108,
            top: 58,
            right: 532,
            bottom: 452,
        };

        assert_eq!(
            frame_insets(outer, visible),
            FrameInsets {
                left: 8,
                top: 8,
                right: 8,
                bottom: 8,
            }
        );
    }

    #[test]
    fn visible_frame_is_rejected_when_it_is_outside_outer_rect() {
        let outer = RECT {
            left: -7,
            top: -7,
            right: 1543,
            bottom: 823,
        };
        let incompatible = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1020,
        };

        assert!(!visible_frame_is_compatible(outer, incompatible));
    }
}
