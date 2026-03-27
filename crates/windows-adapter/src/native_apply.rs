use std::{
    collections::{HashMap, HashSet},
    ptr::{null, null_mut},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
            DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
            DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
            DWMWCP_ROUND, DwmGetWindowAttribute, DwmRegisterThumbnail, DwmSetWindowAttribute,
            DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
        },
        Gdi::{CreateSolidBrush, HBRUSH},
    },
    System::LibraryLoader::GetModuleHandleW,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
            SetActiveWindow, SetFocus, VK_MENU,
        },
        WindowsAndMessaging::{
            BeginDeferWindowPos, BringWindowToTop, CreateWindowExW, DefWindowProcW, DeferWindowPos,
            DestroyWindow, DispatchMessageW, EndDeferWindowPos, GWL_EXSTYLE, GWLP_USERDATA,
            GetForegroundWindow, GetShellWindow, GetWindowLongPtrW, GetWindowRect,
            GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindow, LWA_ALPHA, MSG, PM_REMOVE,
            PeekMessageW, RegisterClassW, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNA,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
            SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetLayeredWindowAttributes,
            SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_LBUTTONDOWN,
            WM_MBUTTONDOWN, WM_NCLBUTTONDOWN, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSW, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
};

use crate::{
    ApplyBatchResult, ApplyFailure, ApplyOperation, TILED_VISUAL_OVERLAP_X_PX, WindowOpacityMode,
    WindowSwitchAnimation, WindowVisualEmphasis, dpi,
};

use std::convert::TryFrom;
use std::mem::zeroed;

const GEOMETRY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const VISUAL_STYLE_REFRESH_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED;
const BROWSER_DIM_OVERLAY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const ERROR_ACCESS_DENIED: u32 = 5;
const WINDOW_CLASS_ALREADY_EXISTS: u32 = 1410;
const BROWSER_DIM_OVERLAY_CLASS: &str = "FlowTileBrowserDimOverlay";
const BROWSER_SURROGATE_CLASS: &str = "FlowTileBrowserVisualSurrogate";
const BROWSER_DIM_OVERLAY_THREAD_SLICE: Duration = Duration::from_millis(16);
const BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_DIM_OVERLAY_COLOR_RGB: u32 = 0x000000;

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

    let left = interpolate_i32(animation.from_rect.x, target_rect.x, progress);
    let top = interpolate_i32(animation.from_rect.y, target_rect.y, progress);
    let right = interpolate_i32(
        animation
            .from_rect
            .x
            .saturating_add(animation.from_rect.width as i32),
        target_rect.x.saturating_add(target_rect.width as i32),
        progress,
    );
    let bottom = interpolate_i32(
        animation
            .from_rect
            .y
            .saturating_add(animation.from_rect.height as i32),
        target_rect.y.saturating_add(target_rect.height as i32),
        progress,
    );

    flowtile_domain::Rect::new(
        left,
        top,
        right.saturating_sub(left).max(1) as u32,
        bottom.saturating_sub(top).max(1) as u32,
    )
}

fn interpolate_i32(from: i32, to: i32, progress: f32) -> i32 {
    let delta = (to - from) as f32;
    from.saturating_add((delta * progress).round() as i32)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AppliedVisualState {
    opacity_alpha: Option<u8>,
    opacity_mode: WindowOpacityMode,
    disable_visual_effects: bool,
    border_color_rgb: Option<u32>,
    rounded_corners: bool,
}

impl From<&WindowVisualEmphasis> for AppliedVisualState {
    fn from(value: &WindowVisualEmphasis) -> Self {
        Self {
            opacity_alpha: value.opacity_alpha,
            opacity_mode: value.opacity_mode,
            disable_visual_effects: value.disable_visual_effects,
            border_color_rgb: value.border_color_rgb,
            rounded_corners: value.rounded_corners,
        }
    }
}

fn apply_visual_emphasis_for_operation(operation: &ApplyOperation) -> Result<(), String> {
    let Some(visual_emphasis) = &operation.visual_emphasis else {
        return Ok(());
    };
    let hwnd = hwnd_from_raw(operation.hwnd)?;
    let tracked_hwnd = hwnd as isize;
    let desired_visual_state = AppliedVisualState::from(visual_emphasis);
    apply_window_browser_visual_surface(operation.hwnd, operation.rect, visual_emphasis)?;
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
                visual_emphasis.force_clear_layered_style,
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
        direct_layered_opacity_alpha(visual_emphasis),
        visual_emphasis.force_clear_layered_style,
    )?;
    if visual_emphasis.disable_visual_effects {
        applied_visual_states()
            .lock()
            .expect("visual state registry lock should not be poisoned")
            .insert(tracked_hwnd, desired_visual_state);
        return Ok(());
    }
    apply_window_corners(hwnd, visual_emphasis);
    apply_window_border(hwnd, visual_emphasis);
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
                return browser_dim_overlay_controller()?.hide(raw_hwnd);
            };
            browser_dim_overlay_controller()?.show(raw_hwnd, rect, alpha)
        }
        WindowOpacityMode::BrowserSurrogate => {
            let Some(alpha) = browser_surrogate_alpha(visual_emphasis) else {
                hide_browser_visual_surrogate_if_initialized(raw_hwnd)?;
                return hide_browser_dim_overlay_if_initialized(raw_hwnd);
            };

            match browser_visual_surrogate_controller()?.show(raw_hwnd, rect, alpha) {
                Ok(()) => hide_browser_dim_overlay_if_initialized(raw_hwnd),
                Err(error) => {
                    eprintln!("browser visual surrogate fallback for hwnd {raw_hwnd}: {error}");
                    browser_dim_overlay_controller()?.show(
                        raw_hwnd,
                        rect,
                        overlay_dim_alpha_from_window_opacity(alpha),
                    )
                }
            }
        }
    }
}

fn direct_layered_opacity_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => visual_emphasis.opacity_alpha,
        WindowOpacityMode::BrowserSurrogate => None,
        WindowOpacityMode::OverlayDim => None,
    }
}

fn browser_surrogate_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => None,
        WindowOpacityMode::BrowserSurrogate => visual_emphasis.opacity_alpha,
        WindowOpacityMode::OverlayDim => None,
    }
}

fn overlay_dim_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => None,
        WindowOpacityMode::BrowserSurrogate => None,
        WindowOpacityMode::OverlayDim => visual_emphasis
            .opacity_alpha
            .map(overlay_dim_alpha_from_window_opacity),
    }
}

const fn overlay_dim_alpha_from_window_opacity(opacity_alpha: u8) -> u8 {
    u8::MAX - opacity_alpha
}

fn apply_window_opacity(
    hwnd: HWND,
    current_ex_style: u32,
    opacity_alpha: Option<u8>,
    force_clear_layered_style: bool,
) -> Result<(), String> {
    let tracked_hwnd = hwnd as isize;
    if let Some(opacity_alpha) = opacity_alpha {
        let currently_layered = style_has_layered(current_ex_style);
        let layered_style = current_ex_style | WS_EX_LAYERED;
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
        layered_hwnds()
            .lock()
            .expect("layered hwnd registry lock should not be poisoned")
            .insert(tracked_hwnd);
        if should_refresh_after_layered_enable(currently_layered) {
            refresh_window_frame(hwnd);
        }
        return Ok(());
    }

    let currently_layered = style_has_layered(current_ex_style);
    let wm_tracked_layered = layered_hwnds()
        .lock()
        .expect("layered hwnd registry lock should not be poisoned")
        .remove(&tracked_hwnd);
    if should_clear_layered_style(wm_tracked_layered, currently_layered) {
        if currently_layered {
            let _ = {
                // SAFETY: restoring full alpha before dropping `WS_EX_LAYERED` helps Windows
                // redraw the HWND as a fully opaque surface instead of keeping stale translucency.
                unsafe { SetLayeredWindowAttributes(hwnd, 0, u8::MAX, LWA_ALPHA) }
            };
        }
        let cleared_style = current_ex_style & !WS_EX_LAYERED;
        let _ = {
            // SAFETY: we restore the original extended style minus the layered bit previously set by the WM.
            unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, cleared_style as isize) }
        };
        if currently_layered {
            refresh_window_frame(hwnd);
        }
    } else if force_clear_layered_style && wm_tracked_layered {
        let _ = {
            // SAFETY: best-effort alpha reset for a HWND that the WM previously dimmed.
            unsafe { SetLayeredWindowAttributes(hwnd, 0, u8::MAX, LWA_ALPHA) }
        };
    }

    Ok(())
}

fn query_window_ex_style(hwnd: HWND) -> Result<u32, String> {
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

    Ok(ex_style as u32)
}

const fn style_has_layered(ex_style: u32) -> bool {
    (ex_style & WS_EX_LAYERED) != 0
}

const fn should_refresh_after_layered_enable(currently_layered: bool) -> bool {
    !currently_layered
}

const fn should_clear_layered_style(wm_tracked_layered: bool, currently_layered: bool) -> bool {
    wm_tracked_layered && currently_layered
}

fn should_skip_visual_write(
    previous_state: AppliedVisualState,
    desired_state: AppliedVisualState,
    force_clear_layered_style: bool,
    wm_tracked_layered: bool,
    currently_layered: bool,
) -> bool {
    previous_state == desired_state
        && (!force_clear_layered_style || !wm_tracked_layered || !currently_layered)
}

fn refresh_window_frame(hwnd: HWND) {
    let _ = {
        // SAFETY: best-effort non-client refresh for a validated HWND after layered-style cleanup.
        unsafe { SetWindowPos(hwnd, null_mut(), 0, 0, 0, 0, VISUAL_STYLE_REFRESH_FLAGS) }
    };
}

fn layered_hwnds() -> &'static Mutex<HashSet<isize>> {
    static LAYERED_HWNDS: OnceLock<Mutex<HashSet<isize>>> = OnceLock::new();
    LAYERED_HWNDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn applied_visual_states() -> &'static Mutex<HashMap<isize, AppliedVisualState>> {
    static APPLIED_VISUAL_STATES: OnceLock<Mutex<HashMap<isize, AppliedVisualState>>> =
        OnceLock::new();
    APPLIED_VISUAL_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Debug)]
struct BrowserVisualSurrogate {
    window: HWND,
    backdrop_thumbnail_id: Option<isize>,
    browser_thumbnail_id: Option<isize>,
}

enum BrowserVisualSurrogateCommand {
    Show {
        owner_hwnd: u64,
        rect: flowtile_domain::Rect,
        alpha: u8,
        response: Sender<Result<(), String>>,
    },
    Hide {
        owner_hwnd: u64,
        response: Sender<Result<(), String>>,
    },
}

struct BrowserVisualSurrogateController {
    sender: Sender<BrowserVisualSurrogateCommand>,
}

static BROWSER_VISUAL_SURROGATE_CONTROLLER: OnceLock<BrowserVisualSurrogateController> =
    OnceLock::new();

impl BrowserVisualSurrogateController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<BrowserVisualSurrogateCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            run_browser_visual_surrogate_thread(command_receiver, startup_sender)
        });
        startup_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(&self, owner_hwnd: u64, rect: flowtile_domain::Rect, alpha: u8) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserVisualSurrogateCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response: response_sender,
            })
            .map_err(|_| "browser visual surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserVisualSurrogateCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "browser visual surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate response timed out: {error}"))?
    }
}

fn browser_visual_surrogate_controller() -> Result<&'static BrowserVisualSurrogateController, String>
{
    if let Some(controller) = BROWSER_VISUAL_SURROGATE_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = BrowserVisualSurrogateController::spawn()?;
    let _ = BROWSER_VISUAL_SURROGATE_CONTROLLER.set(controller);
    BROWSER_VISUAL_SURROGATE_CONTROLLER
        .get()
        .ok_or_else(|| "browser visual surrogate controller did not initialize".to_string())
}

fn browser_visual_surrogate_controller_if_initialized()
-> Option<&'static BrowserVisualSurrogateController> {
    BROWSER_VISUAL_SURROGATE_CONTROLLER.get()
}

fn hide_browser_visual_surrogate_if_initialized(raw_hwnd: u64) -> Result<(), String> {
    browser_visual_surrogate_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(raw_hwnd))
}

fn hide_browser_dim_overlay_if_initialized(raw_hwnd: u64) -> Result<(), String> {
    browser_dim_overlay_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(raw_hwnd))
}

fn run_browser_visual_surrogate_thread(
    command_receiver: Receiver<BrowserVisualSurrogateCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_browser_visual_surrogate_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_browser_visual_surrogate_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_browser_visual_surrogate_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(BROWSER_SURROGATE_CLASS);
    let instance = {
        // SAFETY: we query the current module handle for class registration and overlay creation.
        unsafe { GetModuleHandleW(null()) }
    };
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(browser_visual_surrogate_window_proc),
        hInstance: instance as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        hbrBackground: null_mut(),
        ..unsafe { zeroed() }
    };
    let class_atom = {
        // SAFETY: we pass a fully initialized class descriptor whose data outlives registration.
        unsafe { RegisterClassW(&window_class) }
    };
    if class_atom == 0 {
        let error = {
            // SAFETY: read the Win32 error code immediately after `RegisterClassW`.
            unsafe { GetLastError() }
        };
        if error != WINDOW_CLASS_ALREADY_EXISTS {
            return Err(last_error_message("RegisterClassW"));
        }
    }

    Ok(instance as HINSTANCE)
}

fn run_browser_visual_surrogate_loop(
    command_receiver: Receiver<BrowserVisualSurrogateCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut surrogates = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_browser_visual_surrogates(&mut surrogates);

        match command_receiver.recv_timeout(BROWSER_DIM_OVERLAY_THREAD_SLICE) {
            Ok(BrowserVisualSurrogateCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response,
            }) => {
                let result = show_browser_visual_surrogate(
                    &mut surrogates,
                    instance,
                    owner_hwnd,
                    rect,
                    alpha,
                );
                let _ = response.send(result);
            }
            Ok(BrowserVisualSurrogateCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_browser_visual_surrogate(&mut surrogates, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_browser_visual_surrogates(&mut surrogates);
    Ok(())
}

fn show_browser_visual_surrogate(
    surrogates: &mut HashMap<u64, BrowserVisualSurrogate>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let Ok(owner) = hwnd_from_raw(owner_hwnd) else {
        return hide_browser_visual_surrogate(surrogates, owner_hwnd);
    };
    if !is_valid_window(owner) {
        return hide_browser_visual_surrogate(surrogates, owner_hwnd);
    }

    let surrogate = match surrogates.get(&owner_hwnd).copied() {
        Some(existing) if is_valid_window(existing.window) => existing,
        Some(existing) => {
            let _ = destroy_browser_visual_surrogate(owner_hwnd, existing);
            let surrogate = create_browser_visual_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
        None => {
            let surrogate = create_browser_visual_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
    };

    let mut surrogate = surrogate;
    if let Err(error) = prepare_browser_visual_surrogate(&mut surrogate, owner) {
        let _ = hide_browser_visual_surrogate(surrogates, owner_hwnd);
        return Err(error);
    }
    if let Err(error) =
        show_browser_visual_surrogate_window(surrogate.window, rect, alpha, surrogate)
    {
        let _ = hide_browser_visual_surrogate(surrogates, owner_hwnd);
        return Err(error);
    }
    surrogates.insert(owner_hwnd, surrogate);
    Ok(())
}

fn hide_browser_visual_surrogate(
    surrogates: &mut HashMap<u64, BrowserVisualSurrogate>,
    owner_hwnd: u64,
) -> Result<(), String> {
    if let Some(surrogate) = surrogates.remove(&owner_hwnd) {
        return destroy_browser_visual_surrogate(owner_hwnd, surrogate);
    }

    Ok(())
}

fn prune_stale_browser_visual_surrogates(surrogates: &mut HashMap<u64, BrowserVisualSurrogate>) {
    let stale_owners = surrogates
        .iter()
        .filter_map(|(owner_hwnd, surrogate)| {
            let owner = hwnd_from_raw(*owner_hwnd).ok();
            (owner.is_none()
                || owner.is_some_and(|hwnd| !is_valid_window(hwnd))
                || !is_valid_window(surrogate.window))
            .then_some(*owner_hwnd)
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        let _ = hide_browser_visual_surrogate(surrogates, owner_hwnd);
    }
}

fn destroy_all_browser_visual_surrogates(surrogates: &mut HashMap<u64, BrowserVisualSurrogate>) {
    let owner_hwnds = surrogates.keys().copied().collect::<Vec<_>>();
    for owner_hwnd in owner_hwnds {
        let _ = hide_browser_visual_surrogate(surrogates, owner_hwnd);
    }
}

fn create_browser_visual_surrogate(
    instance: HINSTANCE,
    owner: HWND,
) -> Result<BrowserVisualSurrogate, String> {
    let class_name = widestring(BROWSER_SURROGATE_CLASS);
    let window = {
        // SAFETY: we create a non-activating owned popup surface for browser surrogate rendering.
        unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                owner,
                null_mut(),
                instance,
                null_mut(),
            )
        }
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }
    let _ = {
        // SAFETY: we associate the browser owner HWND with the surrogate for click activation.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, owner as isize) }
    };

    Ok(BrowserVisualSurrogate {
        window,
        backdrop_thumbnail_id: None,
        browser_thumbnail_id: None,
    })
}

fn prepare_browser_visual_surrogate(
    surrogate: &mut BrowserVisualSurrogate,
    owner: HWND,
) -> Result<(), String> {
    if surrogate.backdrop_thumbnail_id.is_none() {
        let backdrop_source = browser_visual_surrogate_backdrop_source()?;
        surrogate.backdrop_thumbnail_id = Some(register_browser_visual_surrogate_thumbnail(
            surrogate.window,
            backdrop_source,
        )?);
    }

    if surrogate.browser_thumbnail_id.is_none() {
        surrogate.browser_thumbnail_id = Some(register_browser_visual_surrogate_thumbnail(
            surrogate.window,
            owner,
        )?);
    }

    Ok(())
}

fn register_browser_visual_surrogate_thumbnail(
    destination: HWND,
    source: HWND,
) -> Result<isize, String> {
    let mut thumbnail_id = 0_isize;
    let result = {
        // SAFETY: we register a DWM thumbnail from the live browser host into the surrogate HWND.
        unsafe { DwmRegisterThumbnail(destination, source, &mut thumbnail_id) }
    };
    if result < 0 {
        return Err(format!(
            "DwmRegisterThumbnail failed with HRESULT {result:#x}"
        ));
    }

    Ok(thumbnail_id)
}

fn browser_visual_surrogate_backdrop_source() -> Result<HWND, String> {
    let shell_window = {
        // SAFETY: querying the current shell desktop HWND is a pure Win32 lookup.
        unsafe { GetShellWindow() }
    };
    if !is_valid_window(shell_window) {
        return Err("GetShellWindow returned no valid shell backdrop source".to_string());
    }

    Ok(shell_window)
}

fn source_relative_rect(
    source_bounds: RECT,
    screen_rect: flowtile_domain::Rect,
    label: &str,
) -> Result<RECT, String> {
    let width = i32::try_from(screen_rect.width.max(1))
        .map_err(|_| format!("browser visual surrogate {label} width exceeds Win32 limits"))?;
    let height = i32::try_from(screen_rect.height.max(1))
        .map_err(|_| format!("browser visual surrogate {label} height exceeds Win32 limits"))?;
    let right = screen_rect
        .x
        .checked_add(width)
        .ok_or_else(|| format!("browser visual surrogate {label} right edge overflowed"))?;
    let bottom = screen_rect
        .y
        .checked_add(height)
        .ok_or_else(|| format!("browser visual surrogate {label} bottom edge overflowed"))?;

    Ok(RECT {
        left: screen_rect.x - source_bounds.left,
        top: screen_rect.y - source_bounds.top,
        right: right - source_bounds.left,
        bottom: bottom - source_bounds.top,
    })
}

fn browser_visual_surrogate_backdrop_source_rect(
    screen_rect: flowtile_domain::Rect,
) -> Result<RECT, String> {
    let backdrop_source = browser_visual_surrogate_backdrop_source()?;
    let mut source_bounds: RECT = unsafe { zeroed() };
    let has_bounds = {
        // SAFETY: querying window bounds is valid for the already validated shell backdrop source.
        unsafe { GetWindowRect(backdrop_source, &mut source_bounds) }
    };
    if has_bounds == 0 {
        return Err(last_error_message("GetWindowRect"));
    }

    source_relative_rect(source_bounds, screen_rect, "backdrop")
}

fn show_browser_visual_surrogate_window(
    window: HWND,
    rect: flowtile_domain::Rect,
    alpha: u8,
    surrogate: BrowserVisualSurrogate,
) -> Result<(), String> {
    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "browser visual surrogate width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "browser visual surrogate height exceeds Win32 limits".to_string())?;
    let applied = {
        // SAFETY: `window` is the surrogate HWND owned by the worker thread; coordinates are POD.
        unsafe {
            SetWindowPos(
                window,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                width,
                height,
                BROWSER_DIM_OVERLAY_APPLY_FLAGS,
            )
        }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    let backdrop_thumbnail_id = surrogate.backdrop_thumbnail_id.ok_or_else(|| {
        "browser visual surrogate backdrop thumbnail was not initialized".to_string()
    })?;
    let backdrop_source_rect = browser_visual_surrogate_backdrop_source_rect(rect)?;
    let backdrop_thumbnail_properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_RECTSOURCE | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rcSource: backdrop_source_rect,
        opacity: u8::MAX,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let backdrop_result = {
        // SAFETY: we update the backdrop thumbnail to cover the entire surrogate with the matching shell rect.
        unsafe {
            DwmUpdateThumbnailProperties(backdrop_thumbnail_id, &backdrop_thumbnail_properties)
        }
    };
    if backdrop_result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties(backdrop) failed with HRESULT {backdrop_result:#x}"
        ));
    }

    let browser_thumbnail_id = surrogate.browser_thumbnail_id.ok_or_else(|| {
        "browser visual surrogate browser thumbnail was not initialized".to_string()
    })?;
    let browser_thumbnail_properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rcSource: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        opacity: alpha,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let result = {
        // SAFETY: we update the registered DWM thumbnail with the current surrogate bounds and alpha.
        unsafe { DwmUpdateThumbnailProperties(browser_thumbnail_id, &browser_thumbnail_properties) }
    };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }

    let _ = {
        // SAFETY: best-effort non-activating show for the surrogate surface.
        unsafe { ShowWindow(window, SW_SHOWNA) }
    };
    Ok(())
}

fn destroy_browser_visual_surrogate(
    _owner_hwnd: u64,
    surrogate: BrowserVisualSurrogate,
) -> Result<(), String> {
    let mut first_error = None;

    if let Some(thumbnail_id) = surrogate.backdrop_thumbnail_id {
        let result = {
            // SAFETY: unregistering pairs with a successful `DwmRegisterThumbnail` call above.
            unsafe { DwmUnregisterThumbnail(thumbnail_id) }
        };
        if result < 0 {
            first_error = Some(format!(
                "DwmUnregisterThumbnail failed with HRESULT {result:#x}"
            ));
        }
    }

    if let Some(thumbnail_id) = surrogate.browser_thumbnail_id {
        let result = {
            // SAFETY: unregistering pairs with a successful `DwmRegisterThumbnail` call above.
            unsafe { DwmUnregisterThumbnail(thumbnail_id) }
        };
        if result < 0 && first_error.is_none() {
            first_error = Some(format!(
                "DwmUnregisterThumbnail(browser) failed with HRESULT {result:#x}"
            ));
        }
    }

    destroy_browser_visual_surrogate_window(surrogate.window);

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn destroy_browser_visual_surrogate_window(window: HWND) {
    if !is_valid_window(window) {
        return;
    }

    let _ = {
        // SAFETY: best-effort hide for the surrogate HWND before destruction.
        unsafe { ShowWindow(window, SW_HIDE) }
    };
    let _ = {
        // SAFETY: destroying the surrogate is paired with its successful creation on the worker thread.
        unsafe { DestroyWindow(window) }
    };
}

unsafe extern "system" fn browser_visual_surrogate_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN => {
            let owner = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as HWND };
            if is_valid_window(owner) {
                let _ = {
                    // SAFETY: best-effort hide before destroying the clicked surrogate surface.
                    unsafe { ShowWindow(hwnd, SW_HIDE) }
                };
                let _ = {
                    // SAFETY: destroying the clicked surrogate removes it before we foreground the owner.
                    unsafe { DestroyWindow(hwnd) }
                };
                let _ = activate_window(owner as usize as u64);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

enum BrowserDimOverlayCommand {
    Show {
        owner_hwnd: u64,
        rect: flowtile_domain::Rect,
        alpha: u8,
        response: Sender<Result<(), String>>,
    },
    Hide {
        owner_hwnd: u64,
        response: Sender<Result<(), String>>,
    },
}

struct BrowserDimOverlayController {
    sender: Sender<BrowserDimOverlayCommand>,
}

static BROWSER_DIM_OVERLAY_CONTROLLER: OnceLock<BrowserDimOverlayController> = OnceLock::new();

impl BrowserDimOverlayController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<BrowserDimOverlayCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || run_browser_dim_overlay_thread(command_receiver, startup_sender));
        startup_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(&self, owner_hwnd: u64, rect: flowtile_domain::Rect, alpha: u8) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserDimOverlayCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response: response_sender,
            })
            .map_err(|_| "browser dim overlay worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserDimOverlayCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "browser dim overlay worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay response timed out: {error}"))?
    }
}

fn browser_dim_overlay_controller() -> Result<&'static BrowserDimOverlayController, String> {
    if let Some(controller) = BROWSER_DIM_OVERLAY_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = BrowserDimOverlayController::spawn()?;
    let _ = BROWSER_DIM_OVERLAY_CONTROLLER.set(controller);
    BROWSER_DIM_OVERLAY_CONTROLLER
        .get()
        .ok_or_else(|| "browser dim overlay controller did not initialize".to_string())
}

fn browser_dim_overlay_controller_if_initialized() -> Option<&'static BrowserDimOverlayController> {
    BROWSER_DIM_OVERLAY_CONTROLLER.get()
}

fn run_browser_dim_overlay_thread(
    command_receiver: Receiver<BrowserDimOverlayCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_browser_dim_overlay_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_browser_dim_overlay_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_browser_dim_overlay_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(BROWSER_DIM_OVERLAY_CLASS);
    let instance = {
        // SAFETY: we query the current module handle for class registration and overlay creation.
        unsafe { GetModuleHandleW(null()) }
    };
    let brush = {
        // SAFETY: creating a solid brush with a constant RGB color is a synchronous GDI call.
        unsafe { CreateSolidBrush(BROWSER_DIM_OVERLAY_COLOR_RGB) }
    };
    if brush.is_null() {
        return Err(last_error_message("CreateSolidBrush"));
    }

    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        hbrBackground: brush as HBRUSH,
        ..unsafe { zeroed() }
    };
    let class_atom = {
        // SAFETY: we pass a fully initialized class descriptor whose data outlives registration.
        unsafe { RegisterClassW(&window_class) }
    };
    if class_atom == 0 {
        let error = {
            // SAFETY: read the Win32 error code immediately after `RegisterClassW`.
            unsafe { GetLastError() }
        };
        if error != WINDOW_CLASS_ALREADY_EXISTS {
            return Err(last_error_message("RegisterClassW"));
        }
    }

    Ok(instance as HINSTANCE)
}

fn run_browser_dim_overlay_loop(
    command_receiver: Receiver<BrowserDimOverlayCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut overlays = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_browser_dim_overlays(&mut overlays);

        match command_receiver.recv_timeout(BROWSER_DIM_OVERLAY_THREAD_SLICE) {
            Ok(BrowserDimOverlayCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response,
            }) => {
                let result =
                    show_browser_dim_overlay(&mut overlays, instance, owner_hwnd, rect, alpha);
                let _ = response.send(result);
            }
            Ok(BrowserDimOverlayCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_browser_dim_overlay(&mut overlays, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_browser_dim_overlays(&mut overlays);
    Ok(())
}

fn show_browser_dim_overlay(
    overlays: &mut HashMap<u64, HWND>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let Ok(owner) = hwnd_from_raw(owner_hwnd) else {
        return hide_browser_dim_overlay(overlays, owner_hwnd);
    };
    if !is_valid_window(owner) {
        return hide_browser_dim_overlay(overlays, owner_hwnd);
    }

    let overlay = match overlays.get(&owner_hwnd).copied() {
        Some(existing) if is_valid_window(existing) => existing,
        Some(existing) => {
            destroy_browser_dim_overlay_window(existing);
            let overlay = create_browser_dim_overlay_window(instance, owner)?;
            overlays.insert(owner_hwnd, overlay);
            overlay
        }
        None => {
            let overlay = create_browser_dim_overlay_window(instance, owner)?;
            overlays.insert(owner_hwnd, overlay);
            overlay
        }
    };

    show_browser_dim_overlay_window(overlay, rect, alpha)
}

fn hide_browser_dim_overlay(
    overlays: &mut HashMap<u64, HWND>,
    owner_hwnd: u64,
) -> Result<(), String> {
    if let Some(overlay) = overlays.remove(&owner_hwnd) {
        destroy_browser_dim_overlay_window(overlay);
    }

    Ok(())
}

fn prune_stale_browser_dim_overlays(overlays: &mut HashMap<u64, HWND>) {
    let stale_owners = overlays
        .iter()
        .filter_map(|(owner_hwnd, overlay_hwnd)| {
            let owner = hwnd_from_raw(*owner_hwnd).ok();
            (owner.is_none()
                || owner.is_some_and(|hwnd| !is_valid_window(hwnd))
                || !is_valid_window(*overlay_hwnd))
            .then_some(*owner_hwnd)
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        let _ = hide_browser_dim_overlay(overlays, owner_hwnd);
    }
}

fn destroy_all_browser_dim_overlays(overlays: &mut HashMap<u64, HWND>) {
    for (_, overlay) in overlays.drain() {
        destroy_browser_dim_overlay_window(overlay);
    }
}

fn create_browser_dim_overlay_window(instance: HINSTANCE, owner: HWND) -> Result<HWND, String> {
    let class_name = widestring(BROWSER_DIM_OVERLAY_CLASS);
    let window = {
        // SAFETY: we create a non-activating owned popup overlay for browser dimming.
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                owner,
                null_mut(),
                instance,
                null_mut(),
            )
        }
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }

    Ok(window)
}

fn show_browser_dim_overlay_window(
    window: HWND,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let layered = {
        // SAFETY: we set a constant alpha on the valid browser dim overlay HWND.
        unsafe { SetLayeredWindowAttributes(window, 0, alpha, LWA_ALPHA) }
    };
    if layered == 0 {
        return Err(last_error_message("SetLayeredWindowAttributes"));
    }

    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "browser dim overlay width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "browser dim overlay height exceeds Win32 limits".to_string())?;
    let applied = {
        // SAFETY: `window` is the overlay HWND owned by the worker thread; coordinates are POD.
        unsafe {
            SetWindowPos(
                window,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                width,
                height,
                BROWSER_DIM_OVERLAY_APPLY_FLAGS,
            )
        }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    Ok(())
}

fn destroy_browser_dim_overlay_window(window: HWND) {
    if !is_valid_window(window) {
        return;
    }

    let _ = {
        // SAFETY: best-effort hide for the overlay HWND before destruction.
        unsafe { ShowWindow(window, SW_HIDE) }
    };
    let _ = {
        // SAFETY: destroying the overlay is paired with its successful creation on the worker thread.
        unsafe { DestroyWindow(window) }
    };
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

    use crate::{WindowOpacityMode, WindowSwitchAnimation, WindowVisualEmphasis};

    use super::{
        AppliedVisualState, FrameInsets, browser_surrogate_alpha, direct_layered_opacity_alpha,
        ease_out_cubic, frame_insets, interpolate_rect, overlay_dim_alpha,
        overlay_dim_alpha_from_window_opacity, should_clear_layered_style,
        should_refresh_after_layered_enable, should_skip_visual_write, source_relative_rect,
        uses_window_switch_animation, visible_frame_is_compatible,
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
