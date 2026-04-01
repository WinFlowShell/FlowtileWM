mod activation;
mod browser_dim_overlay;
mod browser_visual_surrogate;
mod clipped_window_surrogate;
mod visual_effects;
mod window_geometry;
mod window_switch_animation;

use std::{
    ptr::null_mut,
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, RECT},
    Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    UI::WindowsAndMessaging::{
        BeginDeferWindowPos, DeferWindowPos, DispatchMessageW, EndDeferWindowPos, GetWindowRect,
        IsWindow, MSG, PM_REMOVE, PeekMessageW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
        SWP_SHOWWINDOW, SetWindowPos, TranslateMessage, WM_QUIT,
    },
};

use self::{
    activation::activate_window,
    browser_dim_overlay::{hide_browser_dim_overlay_if_initialized, show_browser_dim_overlay},
    browser_visual_surrogate::{
        hide_browser_visual_surrogate_if_initialized, show_browser_visual_surrogate,
    },
    clipped_window_surrogate::{
        clipped_window_surrogate_classifier_reason, hide_clipped_window_surrogate_if_initialized,
        record_clipped_window_surrogate_native_fallback, should_use_clipped_window_surrogate,
        show_clipped_window_surrogate,
        surrogate_presentation_diagnostics_snapshot as clipped_surrogate_diagnostics_snapshot,
    },
    visual_effects::{
        AppliedVisualState, applied_visual_states, apply_window_border, apply_window_corners,
        apply_window_opacity, browser_surrogate_alpha, direct_layered_opacity_alpha, layered_hwnds,
        normalized_visual_emphasis, overlay_dim_alpha, overlay_dim_alpha_from_window_opacity,
        query_window_ex_style, should_skip_visual_write, style_has_layered,
    },
    window_geometry::translated_window_rect,
    window_switch_animation::{
        animated_frame_operation, ease_out_cubic, uses_window_switch_animation,
    },
};
use crate::{
    ApplyBatchResult, ApplyFailure, ApplyOperation, SurrogatePresentationDiagnostics,
    WindowOpacityMode, WindowPresentation, WindowPresentationMode, WindowVisualEmphasis, dpi,
};

use std::convert::TryFrom;
use std::mem::zeroed;

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

    let normalized_operations = operations
        .iter()
        .map(normalize_surrogate_presentation_operation)
        .collect::<Vec<_>>();
    let (animated_operations, direct_operations): (Vec<_>, Vec<_>) = normalized_operations
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

fn normalize_surrogate_presentation_operation(operation: &ApplyOperation) -> ApplyOperation {
    if operation.presentation.mode != WindowPresentationMode::SurrogateClipped {
        return operation.clone();
    }

    if should_use_clipped_window_surrogate(operation.hwnd) {
        return operation.clone();
    }

    let reason = clipped_window_surrogate_classifier_reason(operation.hwnd);
    record_clipped_window_surrogate_native_fallback(operation.hwnd, reason);
    operation_with_native_visible_surrogate_fallback(operation).unwrap_or_else(|| operation.clone())
}

pub(crate) fn surrogate_presentation_diagnostics_snapshot() -> SurrogatePresentationDiagnostics {
    clipped_surrogate_diagnostics_snapshot()
}

fn operation_with_native_visible_surrogate_fallback(
    operation: &ApplyOperation,
) -> Option<ApplyOperation> {
    let native_visible_rect = operation
        .presentation
        .surrogate
        .as_ref()?
        .native_visible_rect;
    let mut normalized = operation.clone();
    normalized.rect = native_visible_rect;
    normalized.presentation = WindowPresentation::default();
    Some(normalized)
}

fn apply_operations_direct(operations: &[ApplyOperation]) -> ApplyBatchResult {
    if operations.is_empty() {
        return ApplyBatchResult::default();
    }

    let (surrogate_operations, remaining_operations): (Vec<_>, Vec<_>) =
        operations.iter().cloned().partition(|operation| {
            operation.presentation.mode == WindowPresentationMode::SurrogateClipped
        });
    let (geometry_operations, visual_only_operations): (Vec<_>, Vec<_>) = remaining_operations
        .iter()
        .cloned()
        .partition(|operation| operation.apply_geometry);

    let mut result = ApplyBatchResult::default();
    merge_apply_result(
        &mut result,
        apply_operations_individually(&surrogate_operations),
    );

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
        sync_window_presentations(&frame_operations)?;
        sync_browser_visual_surfaces_for_animation_frame(operations, &frame_operations)?;

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
                if operation.activate
                    && operation.presentation.mode == WindowPresentationMode::NativeVisible
                    && error.code == ERROR_ACCESS_DENIED
                {
                    sync_window_presentation(operation)?;
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

    sync_window_presentation(operation)?;
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
        if let Err(message) = sync_window_presentation(operation) {
            applied = applied.saturating_sub(1);
            failures.push(ApplyFailure {
                hwnd: operation.hwnd,
                message,
            });
            continue;
        }

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
    visual_effects::apply_gapless_visual_policy(hwnd);
}

fn sync_window_presentations(operations: &[ApplyOperation]) -> Result<(), String> {
    for operation in operations {
        sync_window_presentation(operation)?;
    }

    Ok(())
}

fn sync_window_presentation(operation: &ApplyOperation) -> Result<(), String> {
    match operation.presentation.mode {
        WindowPresentationMode::NativeVisible | WindowPresentationMode::NativeHidden => {
            hide_clipped_window_surrogate_if_initialized(operation.hwnd)
        }
        WindowPresentationMode::SurrogateClipped => {
            let surrogate = operation.presentation.surrogate.as_ref().ok_or_else(|| {
                format!(
                    "surrogate-clipped presentation for hwnd {} is missing clip metadata",
                    operation.hwnd
                )
            })?;
            show_clipped_window_surrogate(
                operation.hwnd,
                surrogate.destination_rect,
                surrogate.source_rect,
                surrogate.native_visible_rect,
            )
        }
    }
}

fn apply_visual_emphasis_for_operation(operation: &ApplyOperation) -> Result<(), String> {
    let Some(visual_emphasis) = &operation.visual_emphasis else {
        return Ok(());
    };
    let effective_visual_emphasis = normalized_visual_emphasis(visual_emphasis);
    let hwnd = hwnd_from_raw(operation.hwnd)?;
    let tracked_hwnd = hwnd as isize;
    let desired_visual_state = AppliedVisualState::from(&effective_visual_emphasis);
    apply_window_browser_visual_surface(
        operation.hwnd,
        operation.rect,
        &effective_visual_emphasis,
    )?;
    let ex_style = query_window_ex_style(hwnd)?;
    let currently_layered = style_has_layered(ex_style);
    let wm_tracked_layered = layered_hwnds()
        .lock()
        .expect("layered hwnd registry lock should not be poisoned")
        .contains(&tracked_hwnd);
    let can_skip_visual_write = applied_visual_states()
        .lock()
        .expect("visual state registry lock should not be poisoned")
        .get(&tracked_hwnd)
        .is_some_and(|state| {
            should_skip_visual_write(
                *state,
                desired_visual_state,
                effective_visual_emphasis.force_clear_layered_style,
                wm_tracked_layered,
                currently_layered,
            )
        });
    if can_skip_visual_write {
        return Ok(());
    }
    apply_window_opacity(
        hwnd,
        ex_style,
        direct_layered_opacity_alpha(&effective_visual_emphasis),
        effective_visual_emphasis.force_clear_layered_style,
    )?;
    if effective_visual_emphasis.disable_visual_effects {
        applied_visual_states()
            .lock()
            .expect("visual state registry lock should not be poisoned")
            .insert(tracked_hwnd, desired_visual_state);
        return Ok(());
    }
    apply_window_corners(hwnd, &effective_visual_emphasis);
    apply_window_border(hwnd, &effective_visual_emphasis);
    applied_visual_states()
        .lock()
        .expect("visual state registry lock should not be poisoned")
        .insert(tracked_hwnd, desired_visual_state);
    Ok(())
}

fn sync_browser_visual_surfaces_for_animation_frame(
    operations: &[ApplyOperation],
    frame_operations: &[ApplyOperation],
) -> Result<(), String> {
    for (operation, frame_operation) in operations.iter().zip(frame_operations.iter()) {
        let Some(visual_emphasis) = &operation.visual_emphasis else {
            continue;
        };
        apply_window_browser_visual_surface(
            frame_operation.hwnd,
            frame_operation.rect,
            visual_emphasis,
        )?;
    }

    Ok(())
}

fn apply_window_browser_visual_surface(
    raw_hwnd: u64,
    rect: flowtile_domain::Rect,
    visual_emphasis: &WindowVisualEmphasis,
) -> Result<(), String> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => {
            hide_browser_visual_surrogate_if_initialized(raw_hwnd)?;
            hide_browser_dim_overlay_if_initialized(raw_hwnd)
        }
        WindowOpacityMode::OverlayDim => {
            hide_browser_visual_surrogate_if_initialized(raw_hwnd)?;
            let Some(alpha) = overlay_dim_alpha(visual_emphasis) else {
                return hide_browser_dim_overlay_if_initialized(raw_hwnd);
            };
            show_browser_dim_overlay(raw_hwnd, rect, alpha)
        }
        WindowOpacityMode::BrowserSurrogate => {
            let Some(alpha) = browser_surrogate_alpha(visual_emphasis) else {
                hide_browser_visual_surrogate_if_initialized(raw_hwnd)?;
                return hide_browser_dim_overlay_if_initialized(raw_hwnd);
            };

            match show_browser_visual_surrogate(raw_hwnd, rect, alpha) {
                Ok(()) => hide_browser_dim_overlay_if_initialized(raw_hwnd),
                Err(error) => {
                    eprintln!("browser visual surrogate fallback for hwnd {raw_hwnd}: {error}");
                    show_browser_dim_overlay(
                        raw_hwnd,
                        rect,
                        overlay_dim_alpha_from_window_opacity(alpha),
                    )
                }
            }
        }
    }
}

fn is_valid_window(hwnd: HWND) -> bool {
    !hwnd.is_null() && {
        // SAFETY: `IsWindow` is a pure Win32 query for a window handle.
        unsafe { IsWindow(hwnd) != 0 }
    }
}

fn pump_overlay_messages() -> Result<(), String> {
    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 structure and valid when zero-initialized.
        unsafe { zeroed() }
    };

    loop {
        let has_message = {
            // SAFETY: we poll and remove messages from the current thread queue.
            unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) }
        };
        if has_message == 0 {
            break;
        }
        if message.message == WM_QUIT {
            return Ok(());
        }
        let _ = {
            // SAFETY: forwarding the message to Win32 translation is valid for a dequeued `MSG`.
            unsafe { TranslateMessage(&message) }
        };
        unsafe { DispatchMessageW(&message) };
    }

    Ok(())
}

fn widestring(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
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

    use crate::{
        WindowOpacityMode, WindowPresentation, WindowPresentationMode, WindowSurrogateClip,
        WindowSwitchAnimation, WindowVisualEmphasis,
    };

    use super::{
        browser_visual_surrogate::source_relative_rect,
        operation_with_native_visible_surrogate_fallback,
        visual_effects::{
            AppliedVisualState, browser_surrogate_alpha, direct_layered_opacity_alpha,
            normalized_visual_emphasis, overlay_dim_alpha, overlay_dim_alpha_from_window_opacity,
            should_clear_layered_style, should_refresh_after_layered_enable,
            should_skip_visual_write,
        },
        window_geometry::{FrameInsets, frame_insets, visible_frame_is_compatible},
        window_switch_animation::{ease_out_cubic, interpolate_rect, uses_window_switch_animation},
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
            presentation: crate::WindowPresentation::default(),
        };

        assert!(uses_window_switch_animation(&operation));
    }

    #[test]
    fn surrogate_fallback_restores_native_visible_rect_and_native_presentation() {
        let operation = crate::ApplyOperation {
            hwnd: 200,
            rect: Rect::new(5000, 5000, 32, 32),
            apply_geometry: true,
            activate: false,
            suppress_visual_gap: false,
            window_switch_animation: None,
            visual_emphasis: None,
            presentation: WindowPresentation {
                mode: WindowPresentationMode::SurrogateClipped,
                surrogate: Some(WindowSurrogateClip {
                    destination_rect: Rect::new(928, 16, 672, 868),
                    source_rect: Rect::new(0, 0, 672, 868),
                    native_visible_rect: Rect::new(928, 16, 900, 868),
                }),
            },
        };

        let fallback = operation_with_native_visible_surrogate_fallback(&operation)
            .expect("surrogate operation should downgrade back to native visible");

        assert_eq!(fallback.rect, Rect::new(928, 16, 900, 868));
        assert_eq!(fallback.presentation, WindowPresentation::default());
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
    fn interpolated_rect_preserves_shared_edge_motion() {
        let left_animation = WindowSwitchAnimation {
            from_rect: Rect::new(0, 0, 400, 900),
            duration_ms: 90,
            frame_count: 6,
        };
        let right_animation = WindowSwitchAnimation {
            from_rect: Rect::new(400, 0, 400, 900),
            duration_ms: 90,
            frame_count: 6,
        };
        let left_frame = interpolate_rect(&left_animation, Rect::new(0, 0, 500, 900), 0.5);
        let right_frame = interpolate_rect(&right_animation, Rect::new(500, 0, 300, 900), 0.5);

        assert_eq!(
            left_frame.x.saturating_add(left_frame.width as i32),
            right_frame.x
        );
    }

    #[test]
    fn easing_curve_advances_without_overshoot() {
        let first = ease_out_cubic(0.2);
        let second = ease_out_cubic(0.8);

        assert!(first > 0.0);
        assert!(second > first);
        assert!(second < 1.0);
    }

    #[test]
    fn force_clear_visual_write_is_not_skipped_while_window_is_still_layered() {
        let state = AppliedVisualState {
            opacity_alpha: None,
            opacity_mode: WindowOpacityMode::DirectLayered,
            disable_visual_effects: false,
            border_color_rgb: Some(0x4CA8FF),
            rounded_corners: true,
        };

        assert!(!should_skip_visual_write(state, state, true, true, true));
        assert!(should_skip_visual_write(state, state, true, false, true));
        assert!(should_skip_visual_write(state, state, true, true, false));
    }

    #[test]
    fn layered_cleanup_requires_wm_owned_layering() {
        assert!(should_clear_layered_style(true, true));
        assert!(!should_clear_layered_style(false, true));
        assert!(!should_clear_layered_style(true, false));
    }

    #[test]
    fn layered_enable_refresh_runs_only_on_first_transition_to_layered() {
        assert!(should_refresh_after_layered_enable(false));
        assert!(!should_refresh_after_layered_enable(true));
    }

    #[test]
    fn browser_surrogate_mode_skips_direct_layered_alpha() {
        let emphasis = WindowVisualEmphasis {
            opacity_alpha: Some(208),
            opacity_mode: WindowOpacityMode::BrowserSurrogate,
            force_clear_layered_style: false,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        };

        assert_eq!(direct_layered_opacity_alpha(&emphasis), None);
        assert_eq!(browser_surrogate_alpha(&emphasis), Some(208));
        assert_eq!(overlay_dim_alpha(&emphasis), None);
    }

    #[test]
    fn browser_surrogate_requests_are_normalized_to_overlay_dim() {
        let emphasis = WindowVisualEmphasis {
            opacity_alpha: Some(208),
            opacity_mode: WindowOpacityMode::BrowserSurrogate,
            force_clear_layered_style: false,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        };

        let normalized = normalized_visual_emphasis(&emphasis);

        assert_eq!(normalized.opacity_mode, WindowOpacityMode::OverlayDim);
        assert_eq!(normalized.opacity_alpha, Some(208));
        assert_eq!(browser_surrogate_alpha(&normalized), None);
        assert_eq!(overlay_dim_alpha(&normalized), Some(47));
    }

    #[test]
    fn safe_window_mode_keeps_direct_layered_alpha() {
        let emphasis = WindowVisualEmphasis {
            opacity_alpha: Some(208),
            opacity_mode: WindowOpacityMode::DirectLayered,
            force_clear_layered_style: false,
            disable_visual_effects: false,
            border_color_rgb: Some(0x4CA8FF),
            border_thickness_px: 3,
            rounded_corners: true,
        };

        assert_eq!(direct_layered_opacity_alpha(&emphasis), Some(208));
        assert_eq!(overlay_dim_alpha(&emphasis), None);
    }

    #[test]
    fn overlay_alpha_tracks_the_inverse_of_window_opacity() {
        assert_eq!(overlay_dim_alpha_from_window_opacity(208), 47);
        assert_eq!(overlay_dim_alpha_from_window_opacity(u8::MAX), 0);
    }

    #[test]
    fn source_relative_rect_translates_screen_rect_against_source_bounds() {
        let source_bounds = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let translated =
            source_relative_rect(source_bounds, Rect::new(-1600, 120, 800, 600), "backdrop")
                .expect("source-relative rect should be computed");

        assert_eq!(translated.left, 320);
        assert_eq!(translated.top, 120);
        assert_eq!(translated.right, 1120);
        assert_eq!(translated.bottom, 720);
    }

    #[test]
    fn source_relative_rect_rejects_overflowing_right_edge() {
        let source_bounds = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let result =
            source_relative_rect(source_bounds, Rect::new(i32::MAX - 4, 0, 8, 10), "backdrop");

        assert!(result.is_err());
    }
}
