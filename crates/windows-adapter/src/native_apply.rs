use std::{
    thread,
    time::{Duration, Instant},
};

use std::convert::TryFrom;
use std::mem::zeroed;
use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, RECT},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_EXTENDED_FRAME_BOUNDS,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmGetWindowAttribute,
        DwmSetWindowAttribute,
    },
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
            SetActiveWindow, SetFocus, VK_MENU,
        },
        WindowsAndMessaging::{
            BeginDeferWindowPos, BringWindowToTop, DeferWindowPos, EndDeferWindowPos, GWL_EXSTYLE,
            GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
            IsIconic, LWA_ALPHA, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
            SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetLayeredWindowAttributes,
            SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_EX_LAYERED,
        },
    },
};

use crate::{
    ApplyBatchResult, ApplyFailure, ApplyOperation, TILED_VISUAL_OVERLAP_X_PX,
    WindowSwitchAnimation, WindowVisualEmphasis, dpi,
};

const GEOMETRY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const ERROR_ACCESS_DENIED: u32 = 5;

pub(crate) fn apply_operations(operations: &[ApplyOperation]) -> ApplyBatchResult {
    if let Err(message) = dpi::ensure_current_thread_per_monitor_v2("native-apply") {
        return ApplyBatchResult {
            attempted: operations.len(),
            applied: 0,
            failures: operations
                .iter()
                .map(|operation| ApplyFailure {
                    hwnd: operation.hwnd,
                    message: message.clone(),
                })
                .collect(),
        };
    }

    let (animated_operations, direct_operations): (Vec<_>, Vec<_>) = operations
        .iter()
        .cloned()
        .partition(uses_window_switch_animation);
    let mut result = ApplyBatchResult::default();

    merge_apply_result(&mut result, apply_operations_direct(&direct_operations));
    merge_apply_result(
        &mut result,
        apply_window_switch_animations(&animated_operations),
    );

    result
}

fn apply_operations_direct(operations: &[ApplyOperation]) -> ApplyBatchResult {
    if operations.is_empty() {
        return ApplyBatchResult::default();
    }

    let (geometry_operations, visual_only_operations): (Vec<_>, Vec<_>) = operations
        .iter()
        .cloned()
        .partition(|operation| operation.apply_geometry);

    let mut result = ApplyBatchResult::default();

    if geometry_operations.is_empty() {
        merge_apply_result(
            &mut result,
            apply_operations_individually(&visual_only_operations),
        );
        return result;
    }

    if geometry_operations.len() > 1 && try_apply_operations_batched(&geometry_operations).is_ok() {
        merge_apply_result(
            &mut result,
            finalize_apply_without_animation(&geometry_operations),
        );
    } else {
        merge_apply_result(
            &mut result,
            apply_operations_individually(&geometry_operations),
        );
    }

    merge_apply_result(
        &mut result,
        apply_operations_individually(&visual_only_operations),
    );

    result
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

fn apply_window_switch_animations(operations: &[ApplyOperation]) -> ApplyBatchResult {
    if operations.is_empty() {
        return ApplyBatchResult::default();
    }

    if try_apply_window_switch_animations(operations).is_ok() {
        return finalize_apply_without_animation(operations);
    }

    apply_operations_direct(operations)
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
        if operation.suppress_visual_gap {
            apply_gapless_visual_policy(hwnd);
        }
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

fn try_apply_window_switch_animations(operations: &[ApplyOperation]) -> Result<(), String> {
    let Some(animation) = operations
        .iter()
        .find_map(|operation| operation.window_switch_animation.clone())
    else {
        return Ok(());
    };
    if animation.frame_count <= 1 {
        return Ok(());
    }

    let total_duration = Duration::from_millis(u64::from(animation.duration_ms.max(1)));
    let frame_count = u64::from(animation.frame_count);
    let animation_start = Instant::now();

    for frame_index in 1..=frame_count {
        let progress = frame_index as f32 / frame_count as f32;
        let eased_progress = ease_out_cubic(progress);
        let frame_operations = operations
            .iter()
            .map(|operation| animated_frame_operation(operation, eased_progress))
            .collect::<Vec<_>>();
        try_apply_operations_batched(&frame_operations)?;

        if frame_index < frame_count {
            let target_elapsed_ms = total_duration
                .as_millis()
                .saturating_mul(frame_index as u128)
                / frame_count as u128;
            let target_elapsed = Duration::from_millis(target_elapsed_ms as u64);
            if let Some(remaining) = target_elapsed.checked_sub(animation_start.elapsed()) {
                thread::sleep(remaining);
            }
        }
    }

    Ok(())
}

fn apply_operation(operation: &ApplyOperation) -> Result<(), String> {
    if operation.apply_geometry {
        match apply_geometry(operation) {
            Ok(()) => {}
            Err(error) => {
                if operation.activate && error.code == ERROR_ACCESS_DENIED {
                    apply_visual_emphasis_for_operation(operation)?;
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
    }

    apply_visual_emphasis_for_operation(operation)?;

    if operation.activate {
        activate_window(operation.hwnd)?;
    }

    Ok(())
}

fn finalize_apply_without_animation(operations: &[ApplyOperation]) -> ApplyBatchResult {
    let mut applied = operations.len();
    let mut failures = Vec::new();

    for operation in operations {
        if let Err(message) = apply_visual_emphasis_for_operation(operation) {
            applied = applied.saturating_sub(1);
            failures.push(ApplyFailure {
                hwnd: operation.hwnd,
                message,
            });
            continue;
        }

        if !operation.activate {
            continue;
        }

        if let Err(message) = activate_window(operation.hwnd) {
            applied = applied.saturating_sub(1);
            failures.push(ApplyFailure {
                hwnd: operation.hwnd,
                message,
            });
        }
    }

    ApplyBatchResult {
        attempted: operations.len(),
        applied,
        failures,
    }
}

fn merge_apply_result(target: &mut ApplyBatchResult, source: ApplyBatchResult) {
    target.attempted += source.attempted;
    target.applied += source.applied;
    target.failures.extend(source.failures);
}

fn uses_window_switch_animation(operation: &ApplyOperation) -> bool {
    operation.apply_geometry
        && operation
            .window_switch_animation
            .as_ref()
            .is_some_and(|animation| {
                animation.frame_count > 1 && animation.from_rect != operation.rect
            })
}

fn animated_frame_operation(operation: &ApplyOperation, progress: f32) -> ApplyOperation {
    let animation = operation
        .window_switch_animation
        .as_ref()
        .expect("animation operation should carry animation metadata");
    ApplyOperation {
        hwnd: operation.hwnd,
        rect: interpolate_rect(animation, operation.rect, progress),
        apply_geometry: true,
        activate: false,
        suppress_visual_gap: operation.suppress_visual_gap,
        window_switch_animation: None,
        visual_emphasis: None,
    }
}

fn interpolate_rect(
    animation: &WindowSwitchAnimation,
    target_rect: flowtile_domain::Rect,
    progress: f32,
) -> flowtile_domain::Rect {
    if progress >= 1.0 {
        return target_rect;
    }

    flowtile_domain::Rect::new(
        interpolate_i32(animation.from_rect.x, target_rect.x, progress),
        interpolate_i32(animation.from_rect.y, target_rect.y, progress),
        interpolate_u32(animation.from_rect.width, target_rect.width, progress),
        interpolate_u32(animation.from_rect.height, target_rect.height, progress),
    )
}

fn interpolate_i32(from: i32, to: i32, progress: f32) -> i32 {
    let delta = (to - from) as f32;
    from.saturating_add((delta * progress).round() as i32)
}

fn interpolate_u32(from: u32, to: u32, progress: f32) -> u32 {
    let delta = to as f32 - from as f32;
    ((from as f32) + delta * progress).round().max(1.0) as u32
}

fn ease_out_cubic(progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    1.0 - (1.0 - progress).powi(3)
}

fn apply_geometry(operation: &ApplyOperation) -> Result<(), Win32ApiError> {
    let hwnd =
        hwnd_from_raw(operation.hwnd).map_err(|message| Win32ApiError { code: 0, message })?;
    if operation.suppress_visual_gap {
        apply_gapless_visual_policy(hwnd);
    }
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

fn apply_gapless_visual_policy(hwnd: HWND) {
    set_dwm_u32_attribute(hwnd, DWMWA_BORDER_COLOR as u32, DWMWA_COLOR_NONE);
}

fn apply_visual_emphasis_for_operation(operation: &ApplyOperation) -> Result<(), String> {
    let Some(visual_emphasis) = &operation.visual_emphasis else {
        return Ok(());
    };
    let hwnd = hwnd_from_raw(operation.hwnd)?;
    apply_window_opacity(hwnd, visual_emphasis.opacity_alpha)?;
    apply_window_corners(hwnd, visual_emphasis);
    apply_window_border(hwnd, visual_emphasis);
    Ok(())
}

fn apply_window_opacity(hwnd: HWND, opacity_alpha: u8) -> Result<(), String> {
    let ex_style = {
        // SAFETY: `GetWindowLongPtrW` is a read-only style query for a validated window handle.
        unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }
    };
    if ex_style == 0 {
        let error = {
            // SAFETY: reading the thread-local last-error code after a Win32 call is valid.
            unsafe { GetLastError() }
        };
        if error != 0 {
            return Err(format!("GetWindowLongPtrW failed with Win32 error {error}"));
        }
    }

    let layered_style = (ex_style as u32) | WS_EX_LAYERED;
    let _ = {
        // SAFETY: we write back the same style flags plus `WS_EX_LAYERED` for the validated HWND.
        unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, layered_style as isize) }
    };
    let layered_applied = {
        // SAFETY: we pass a validated HWND and a simple alpha value for the documented layered-window API.
        unsafe { SetLayeredWindowAttributes(hwnd, 0, opacity_alpha, LWA_ALPHA) }
    };
    if layered_applied == 0 {
        return Err(last_error_message("SetLayeredWindowAttributes"));
    }

    Ok(())
}

fn apply_window_border(hwnd: HWND, visual_emphasis: &WindowVisualEmphasis) {
    let border_color = visual_emphasis.border_color_rgb.unwrap_or(DWMWA_COLOR_NONE);
    let _ = visual_emphasis.border_thickness_px;
    set_dwm_u32_attribute(hwnd, DWMWA_BORDER_COLOR as u32, border_color);
}

fn apply_window_corners(hwnd: HWND, visual_emphasis: &WindowVisualEmphasis) {
    let corner_preference = if visual_emphasis.rounded_corners {
        DWMWCP_ROUND
    } else {
        DWMWCP_DONOTROUND
    };
    set_dwm_i32_attribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        corner_preference,
    );
}

fn set_dwm_i32_attribute(hwnd: HWND, attribute: u32, value: i32) {
    let _ = {
        // SAFETY: We pass the documented attribute payload type and size for a validated top-level
        // window handle. This visual policy is best-effort and failure is non-fatal.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                &value as *const _ as *const _,
                std::mem::size_of::<i32>() as u32,
            )
        }
    };
}

fn set_dwm_u32_attribute(hwnd: HWND, attribute: u32, value: u32) {
    let _ = {
        // SAFETY: We pass the documented attribute payload type and size for a validated top-level
        // window handle. This visual policy is best-effort and failure is non-fatal.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                &value as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            )
        }
    };
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

fn translated_window_rect(
    hwnd: HWND,
    operation: &ApplyOperation,
) -> Result<TranslatedRect, String> {
    let target_visible_rect = compensated_visible_rect(operation)?;
    let visible_width = i32::try_from(target_visible_rect.width)
        .map_err(|_| format!("window {} width exceeded Win32 limits", operation.hwnd))?;
    let visible_height = i32::try_from(target_visible_rect.height)
        .map_err(|_| format!("window {} height exceeded Win32 limits", operation.hwnd))?;
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
        x: target_visible_rect.x.saturating_sub(insets.left),
        y: target_visible_rect.y.saturating_sub(insets.top),
        width,
        height,
    })
}

fn compensated_visible_rect(operation: &ApplyOperation) -> Result<flowtile_domain::Rect, String> {
    if !operation.suppress_visual_gap {
        return Ok(operation.rect);
    }

    let x = if operation.rect.x > 0 {
        operation
            .rect
            .x
            .saturating_sub(TILED_VISUAL_OVERLAP_X_PX.max(0))
    } else {
        operation.rect.x
    };

    Ok(flowtile_domain::Rect::new(
        x,
        operation.rect.y,
        operation.rect.width,
        operation.rect.height,
    ))
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

    use flowtile_domain::Rect;

    use crate::WindowSwitchAnimation;

    use super::{
        FrameInsets, ease_out_cubic, frame_insets, interpolate_rect, uses_window_switch_animation,
        visible_frame_is_compatible,
    };

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

    #[test]
    fn window_switch_animation_detection_requires_real_geometry_change() {
        let operation = crate::ApplyOperation {
            hwnd: 100,
            rect: Rect::new(120, 0, 420, 900),
            apply_geometry: true,
            activate: true,
            suppress_visual_gap: true,
            window_switch_animation: Some(WindowSwitchAnimation {
                from_rect: Rect::new(0, 0, 420, 900),
                duration_ms: 90,
                frame_count: 6,
            }),
            visual_emphasis: None,
        };

        assert!(uses_window_switch_animation(&operation));
    }

    #[test]
    fn interpolated_window_switch_rect_reaches_exact_target() {
        let animation = WindowSwitchAnimation {
            from_rect: Rect::new(0, 0, 420, 900),
            duration_ms: 90,
            frame_count: 6,
        };
        let target = Rect::new(-240, 0, 420, 900);

        assert_eq!(interpolate_rect(&animation, target, 1.0), target);
    }

    #[test]
    fn easing_curve_advances_without_overshoot() {
        let first = ease_out_cubic(0.2);
        let second = ease_out_cubic(0.8);

        assert!(first > 0.0);
        assert!(second > first);
        assert!(second < 1.0);
    }
}
