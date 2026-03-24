use std::convert::TryFrom;
use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND},
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{SetActiveWindow, SetFocus},
        WindowsAndMessaging::{
            BeginDeferWindowPos, BringWindowToTop, DeferWindowPos, EndDeferWindowPos,
            GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SW_SHOW,
            SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow,
            SetWindowPos, ShowWindow,
        },
    },
};

use crate::{ApplyBatchResult, ApplyFailure, ApplyOperation};

const GEOMETRY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;

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
        let (width, height) = rect_size(operation)?;
        let next_batch = {
            // SAFETY: `batch` is a handle returned by `BeginDeferWindowPos` or a previous
            // `DeferWindowPos` call. `hwnd` is derived from the raw HWND recorded in state and
            // the rectangle values are simple POD integers.
            unsafe {
                DeferWindowPos(
                    batch,
                    hwnd,
                    null_mut(),
                    operation.rect.x,
                    operation.rect.y,
                    width,
                    height,
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
    apply_geometry(operation)?;
    if operation.activate {
        activate_window(operation.hwnd)?;
    }

    Ok(())
}

fn apply_geometry(operation: &ApplyOperation) -> Result<(), String> {
    let hwnd = hwnd_from_raw(operation.hwnd)?;
    let (width, height) = rect_size(operation)?;
    let applied = {
        // SAFETY: `hwnd` is reconstructed from the raw HWND tracked by the runtime and the rest
        // of the parameters are primitive coordinates and flags forwarded directly to Win32.
        unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                operation.rect.x,
                operation.rect.y,
                width,
                height,
                GEOMETRY_APPLY_FLAGS,
            )
        }
    };

    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
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

    let activation_succeeded = {
        // SAFETY: `GetForegroundWindow` is a read-only query for the current desktop.
        unsafe { GetForegroundWindow() == hwnd }
    };

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

fn hwnd_from_raw(hwnd: u64) -> Result<HWND, String> {
    let raw = isize::try_from(hwnd)
        .map_err(|_| format!("window handle {hwnd} does not fit pointer width"))?;
    Ok(raw as HWND)
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

fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: Reading the thread-local Win32 last-error code immediately after a failed API
        // call is the intended contract of `GetLastError`.
        unsafe { GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}
