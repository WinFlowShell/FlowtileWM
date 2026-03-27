use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{BOOL, CloseHandle, GetLastError, HWND, LPARAM, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
        },
    },
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetForegroundWindow,
            GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
            MONITORINFOF_PRIMARY, WS_CAPTION, WS_EX_TOOLWINDOW, WS_POPUP, WS_THICKFRAME,
        },
    },
};

use crate::{
    PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot, WindowsAdapterError, dpi,
};
use flowtile_domain::Rect as DomainRect;

const CLASS_FILTER_CORE_WINDOW: &str = "Windows.UI.Core.CoreWindow";
const DEFAULT_DPI: u32 = 96;
const TRANSIENT_POPUP_MAX_WIDTH_PX: u32 = 480;
const TRANSIENT_POPUP_MAX_HEIGHT_PX: u32 = 360;
const ANCHORED_TRANSIENT_MAX_WIDTH_RATIO: f32 = 0.6;
const ANCHORED_TRANSIENT_MAX_HEIGHT_RATIO: f32 = 0.4;
const ANCHORED_TRANSIENT_MAX_AREA_RATIO: f32 = 0.25;
const EMBEDDED_PANEL_MAX_WIDTH_RATIO: f32 = 0.8;
const EMBEDDED_PANEL_MAX_HEIGHT_RATIO: f32 = 0.9;
const EMBEDDED_PANEL_MAX_AREA_RATIO: f32 = 0.5;
const ANCHORED_TRANSIENT_EDGE_PROXIMITY_PX: i32 = 96;
const TRANSIENT_SURFACE_MAX_WIDTH_PX: u32 = 720;
const TRANSIENT_SURFACE_MAX_HEIGHT_PX: u32 = 180;
const TRANSIENT_SURFACE_MAX_AREA_PX: u32 = 120_000;
const SHELL_OVERLAY_PROCESS_SCREEN_CLIPPING_HOST: &str = "screenclippinghost";
const SHELL_OVERLAY_PROCESS_SHELL_EXPERIENCE_HOST: &str = "shellexperiencehost";
const SHELL_OVERLAY_PROCESS_START_MENU_EXPERIENCE_HOST: &str = "startmenuexperiencehost";
const SHELL_OVERLAY_PROCESS_SEARCH_HOST: &str = "searchhost";
const SHELL_OVERLAY_PROCESS_SEARCH_APP: &str = "searchapp";
const SHELL_OVERLAY_PROCESS_TEXT_INPUT_HOST: &str = "textinputhost";
const SHELL_OVERLAY_PROCESS_LOCK_APP: &str = "lockapp";
const SHELL_OVERLAY_PROCESS_SNIPPING_TOOL: &str = "snippingtool";
pub(crate) fn scan_snapshot() -> Result<PlatformSnapshot, WindowsAdapterError> {
    dpi::ensure_current_thread_per_monitor_v2("native-scan").map_err(|message| {
        WindowsAdapterError::RuntimeFailed {
            component: "native-scan",
            message,
        }
    })?;

    let mut monitor_context = MonitorEnumContext::default();
    let enumerated_monitors = {
        // SAFETY: The callback pointer is a valid static function and `monitor_context` stays
        // alive for the duration of the synchronous Win32 enumeration call.
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(enum_monitors),
                &mut monitor_context as *mut _ as LPARAM,
            )
        }
    };
    if enumerated_monitors == 0 {
        return Err(WindowsAdapterError::RuntimeFailed {
            component: "native-scan",
            message: last_error_message("EnumDisplayMonitors"),
        });
    }

    let shell_hwnd = {
        // SAFETY: `GetShellWindow` is a parameterless Win32 query.
        unsafe { GetShellWindow() }
    };
    let foreground_hwnd = {
        // SAFETY: `GetForegroundWindow` is a parameterless Win32 query.
        unsafe { GetForegroundWindow() }
    };

    let mut window_context = WindowEnumContext {
        shell_hwnd,
        foreground_hwnd,
        monitors_by_handle: monitor_context.monitors_by_handle.clone(),
        windows: Vec::new(),
    };
    let enumerated_windows = {
        // SAFETY: The callback pointer is a valid static function and `window_context` stays
        // alive for the duration of the synchronous Win32 enumeration call.
        unsafe { EnumWindows(Some(enum_windows), &mut window_context as *mut _ as LPARAM) }
    };
    if enumerated_windows == 0 {
        return Err(WindowsAdapterError::RuntimeFailed {
            component: "native-scan",
            message: last_error_message("EnumWindows"),
        });
    }

    let mut snapshot = PlatformSnapshot {
        foreground_hwnd: hwnd_to_raw(foreground_hwnd),
        monitors: monitor_context.monitors,
        windows: window_context.windows,
    };
    filter_embedded_transient_windows(&mut snapshot);
    snapshot.sort_for_stability();
    Ok(snapshot)
}

pub(crate) fn refresh_window(snapshot: &mut PlatformSnapshot, hwnd_raw: u64) -> Result<(), String> {
    dpi::ensure_current_thread_per_monitor_v2("native-refresh-window")?;
    let hwnd = hwnd_from_raw(hwnd_raw).map_err(|message| message.to_string())?;
    let foreground_hwnd = {
        // SAFETY: `GetForegroundWindow` is a parameterless Win32 query.
        unsafe { GetForegroundWindow() }
    };

    if let Some(window) = capture_window_snapshot(hwnd, foreground_hwnd) {
        upsert_monitor(snapshot, &window.monitor_binding);
        upsert_window(snapshot, window);
        filter_embedded_transient_windows(snapshot);
    } else {
        remove_window(snapshot, hwnd_raw);
    }

    refresh_focus(snapshot)?;
    snapshot.sort_for_stability();
    Ok(())
}

pub(crate) fn remove_window(snapshot: &mut PlatformSnapshot, hwnd: u64) {
    snapshot.windows.retain(|window| window.hwnd != hwnd);
}

pub(crate) fn refresh_focus(snapshot: &mut PlatformSnapshot) -> Result<(), String> {
    dpi::ensure_current_thread_per_monitor_v2("native-refresh-focus")?;
    let foreground_hwnd = {
        // SAFETY: `GetForegroundWindow` is a parameterless Win32 query.
        unsafe { GetForegroundWindow() }
    };
    let focused_raw = hwnd_to_raw(foreground_hwnd).unwrap_or_default();
    snapshot.foreground_hwnd = hwnd_to_raw(foreground_hwnd);

    for window in &mut snapshot.windows {
        window.is_focused = window.hwnd == focused_raw;
    }

    Ok(())
}

fn upsert_window(snapshot: &mut PlatformSnapshot, window: PlatformWindowSnapshot) {
    if let Some(existing) = snapshot
        .windows
        .iter_mut()
        .find(|existing| existing.hwnd == window.hwnd)
    {
        *existing = window;
    } else {
        snapshot.windows.push(window);
    }
}

fn upsert_monitor(snapshot: &mut PlatformSnapshot, binding: &str) {
    let monitor = snapshot_monitor(binding);
    if let Some(monitor) = monitor {
        if let Some(existing) = snapshot
            .monitors
            .iter_mut()
            .find(|existing| existing.binding.eq_ignore_ascii_case(binding))
        {
            *existing = monitor;
        } else {
            snapshot.monitors.push(monitor);
        }
    }
}

fn snapshot_monitor(binding: &str) -> Option<PlatformMonitorSnapshot> {
    let mut context = MonitorEnumContext::default();
    let enumerated_monitors = {
        // SAFETY: The callback pointer is a valid static function and `context` stays alive for
        // the duration of the synchronous Win32 enumeration call.
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(enum_monitors),
                &mut context as *mut _ as LPARAM,
            )
        }
    };
    if enumerated_monitors == 0 {
        return None;
    }

    context
        .monitors
        .into_iter()
        .find(|monitor| monitor.binding.eq_ignore_ascii_case(binding))
}

unsafe extern "system" fn enum_monitors(
    monitor_handle: HMONITOR,
    _: HDC,
    _: *mut RECT,
    user_data: LPARAM,
) -> BOOL {
    let context = {
        // SAFETY: `user_data` is a pointer to `MonitorEnumContext` that was passed into the
        // synchronous `EnumDisplayMonitors` call.
        unsafe { &mut *(user_data as *mut MonitorEnumContext) }
    };

    if let Some(monitor) = describe_monitor(monitor_handle)
        && context.bindings.insert(monitor.binding.clone())
    {
        context
            .monitors_by_handle
            .insert(monitor_handle as isize, monitor.clone());
        context.monitors.push(monitor);
    }

    1
}

unsafe extern "system" fn enum_windows(window_handle: HWND, user_data: LPARAM) -> BOOL {
    let context = {
        // SAFETY: `user_data` is a pointer to `WindowEnumContext` that was passed into the
        // synchronous `EnumWindows` call.
        unsafe { &mut *(user_data as *mut WindowEnumContext) }
    };

    if window_handle.is_null() || window_handle == context.shell_hwnd {
        return 1;
    }

    if let Some(window) = capture_window_snapshot(window_handle, context.foreground_hwnd) {
        if let Some(monitor) = describe_monitor_for_window(window_handle) {
            context
                .monitors_by_handle
                .entry(monitor.0 as isize)
                .or_insert(monitor.1);
        }
        context.windows.push(window);
    }

    1
}

fn capture_window_snapshot(
    window_handle: HWND,
    foreground_hwnd: HWND,
) -> Option<PlatformWindowSnapshot> {
    if !is_real_user_window(window_handle) {
        return None;
    }

    let rect = query_window_rect(window_handle)?;
    if rect.width == 0 || rect.height == 0 {
        return None;
    }

    if is_small_popup_candidate(window_handle, rect) {
        return None;
    }

    let (_, monitor) = describe_monitor_for_window(window_handle)?;
    let title = query_window_title(window_handle);
    let class_name = query_window_class(window_handle);
    if class_name == CLASS_FILTER_CORE_WINDOW {
        return None;
    }

    let process_id = query_process_id(window_handle);
    let process_name = query_process_name(process_id);
    let management_candidate =
        !is_transient_shell_overlay(&class_name, &title, process_name.as_deref());
    Some(PlatformWindowSnapshot {
        hwnd: hwnd_to_raw(window_handle)?,
        title,
        class_name,
        process_id,
        process_name,
        rect,
        monitor_binding: monitor.binding,
        is_visible: true,
        is_focused: window_handle == foreground_hwnd,
        management_candidate,
    })
}

fn is_transient_shell_overlay(class_name: &str, title: &str, process_name: Option<&str>) -> bool {
    let class_name = class_name.to_ascii_lowercase();
    if matches!(
        class_name.as_str(),
        "multitaskingviewframe" | "taskswitcherwnd"
    ) {
        return true;
    }

    let process_name = normalized_process_name(process_name).unwrap_or_default();
    if matches!(
        process_name.as_str(),
        SHELL_OVERLAY_PROCESS_SCREEN_CLIPPING_HOST
            | SHELL_OVERLAY_PROCESS_SHELL_EXPERIENCE_HOST
            | SHELL_OVERLAY_PROCESS_START_MENU_EXPERIENCE_HOST
            | SHELL_OVERLAY_PROCESS_SEARCH_HOST
            | SHELL_OVERLAY_PROCESS_SEARCH_APP
            | SHELL_OVERLAY_PROCESS_TEXT_INPUT_HOST
            | SHELL_OVERLAY_PROCESS_LOCK_APP
            | SHELL_OVERLAY_PROCESS_SNIPPING_TOOL
    ) {
        return true;
    }

    let title = title.to_ascii_lowercase();
    title.contains("task switching")
        || title.contains("task view")
        || title.contains("панель инструментов записи")
        || title.contains("ножницы")
}

fn is_real_user_window(window_handle: HWND) -> bool {
    let is_visible = {
        // SAFETY: `IsWindowVisible` is a pure Win32 query on a window handle.
        unsafe { IsWindowVisible(window_handle) != 0 }
    };
    if !is_visible {
        return false;
    }

    let is_iconic = {
        // SAFETY: `IsIconic` is a pure Win32 query on a window handle.
        unsafe { IsIconic(window_handle) != 0 }
    };
    if is_iconic || is_window_cloaked(window_handle) {
        return false;
    }

    let owner = {
        // SAFETY: `GetWindow` with `GW_OWNER` is a pure Win32 query on a window handle.
        unsafe { GetWindow(window_handle, GW_OWNER) }
    };
    if !owner.is_null() {
        return false;
    }

    let ex_style = {
        // SAFETY: `GetWindowLongPtrW` reads the window extended style.
        unsafe { GetWindowLongPtrW(window_handle, GWL_EXSTYLE) as u32 }
    };
    if (ex_style & WS_EX_TOOLWINDOW) != 0 {
        return false;
    }

    let style = {
        // SAFETY: `GetWindowLongPtrW` reads the regular style flags for the same validated HWND.
        unsafe { GetWindowLongPtrW(window_handle, GWL_STYLE) as u32 }
    };
    !looks_like_transient_popup_window(style)
}

fn looks_like_transient_popup_window(style: u32) -> bool {
    let has_popup = (style & WS_POPUP) != 0;
    let has_caption = (style & WS_CAPTION) != 0;
    let has_thickframe = (style & WS_THICKFRAME) != 0;

    has_popup && !has_caption && !has_thickframe
}

fn is_small_popup_candidate(window_handle: HWND, rect: DomainRect) -> bool {
    let style = {
        // SAFETY: `GetWindowLongPtrW` reads the regular style flags for the validated HWND.
        unsafe { GetWindowLongPtrW(window_handle, GWL_STYLE) as u32 }
    };
    if !looks_like_transient_popup_window(style) {
        return false;
    }

    rect.width <= TRANSIENT_POPUP_MAX_WIDTH_PX && rect.height <= TRANSIENT_POPUP_MAX_HEIGHT_PX
}

fn filter_embedded_transient_windows(snapshot: &mut PlatformSnapshot) {
    let transient_hwnds = snapshot
        .windows
        .iter()
        .filter(|candidate| is_process_anchored_transient_candidate(snapshot, candidate))
        .map(|window| window.hwnd)
        .collect::<HashSet<_>>();

    if transient_hwnds.is_empty() {
        return;
    }

    snapshot
        .windows
        .retain(|window| !transient_hwnds.contains(&window.hwnd));
}

fn is_process_anchored_transient_candidate(
    snapshot: &PlatformSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    if candidate.is_focused {
        return false;
    }
    if candidate.title.trim().is_empty() {
        return snapshot.windows.iter().any(|container| {
            if container.hwnd == candidate.hwnd
                || container.monitor_binding != candidate.monitor_binding
                || !same_application_family(container, candidate)
                || container.rect.width < candidate.rect.width
                || container.rect.height < candidate.rect.height
                || !embedded_panel_ratio_ok(container.rect, candidate.rect)
            {
                return false;
            }

            rect_contains(container.rect, candidate.rect)
                || rects_overlap(container.rect, candidate.rect)
        });
    }
    if has_engine_internal_surface_signature(candidate) {
        return snapshot.windows.iter().any(|container| {
            if container.hwnd == candidate.hwnd
                || container.monitor_binding != candidate.monitor_binding
                || !same_application_family(container, candidate)
                || container.rect.width < candidate.rect.width
                || container.rect.height < candidate.rect.height
            {
                return false;
            }

            rect_contains(container.rect, candidate.rect)
                || rects_overlap(container.rect, candidate.rect)
        });
    }
    let has_transient_signature = has_transient_surface_signature(candidate);
    if !has_transient_signature {
        return false;
    }

    snapshot.windows.iter().any(|container| {
        if container.hwnd == candidate.hwnd
            || container.monitor_binding != candidate.monitor_binding
            || !same_application_family(container, candidate)
            || container.rect.width < candidate.rect.width
            || container.rect.height < candidate.rect.height
            || !anchored_transient_ratio_ok(container.rect, candidate.rect)
        {
            return false;
        }

        rect_contains(container.rect, candidate.rect)
            || rects_overlap(container.rect, candidate.rect)
            || rects_are_proximate(
                container.rect,
                candidate.rect,
                ANCHORED_TRANSIENT_EDGE_PROXIMITY_PX,
            )
    })
}

fn has_transient_surface_signature(candidate: &PlatformWindowSnapshot) -> bool {
    let empty_title = candidate.title.trim().is_empty();
    let compact_height = candidate.rect.height <= 96;
    let compact_surface = candidate.rect.height <= TRANSIENT_SURFACE_MAX_HEIGHT_PX
        && candidate.rect.width <= TRANSIENT_SURFACE_MAX_WIDTH_PX
        && candidate.rect.width.saturating_mul(candidate.rect.height)
            <= TRANSIENT_SURFACE_MAX_AREA_PX;

    compact_surface && (empty_title || compact_height)
}

fn has_engine_internal_surface_signature(candidate: &PlatformWindowSnapshot) -> bool {
    let Some(process_name) = normalized_process_name(candidate.process_name.as_deref()) else {
        return false;
    };
    if !matches!(
        process_name.as_str(),
        "msedge" | "chrome" | "brave" | "opera" | "vivaldi" | "chromium" | "firefox"
    ) {
        return false;
    }

    let title = candidate.title.trim().to_ascii_lowercase();
    matches!(title.as_str(), "chrome legacy window")
}

fn same_application_family(
    container: &PlatformWindowSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    if container.process_id == candidate.process_id {
        return true;
    }

    let Some(container_name) = normalized_process_name(container.process_name.as_deref()) else {
        return false;
    };
    let Some(candidate_name) = normalized_process_name(candidate.process_name.as_deref()) else {
        return false;
    };

    container_name == candidate_name
}

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

fn rect_contains(outer: DomainRect, inner: DomainRect) -> bool {
    let outer_right = outer
        .x
        .saturating_add(outer.width.min(i32::MAX as u32) as i32);
    let outer_bottom = outer
        .y
        .saturating_add(outer.height.min(i32::MAX as u32) as i32);
    let inner_right = inner
        .x
        .saturating_add(inner.width.min(i32::MAX as u32) as i32);
    let inner_bottom = inner
        .y
        .saturating_add(inner.height.min(i32::MAX as u32) as i32);

    inner.x >= outer.x
        && inner.y >= outer.y
        && inner_right <= outer_right
        && inner_bottom <= outer_bottom
}

fn anchored_transient_ratio_ok(container: DomainRect, candidate: DomainRect) -> bool {
    let width_ratio = candidate.width as f32 / container.width.max(1) as f32;
    let height_ratio = candidate.height as f32 / container.height.max(1) as f32;
    let candidate_area = candidate.width as f32 * candidate.height as f32;
    let container_area = container.width.max(1) as f32 * container.height.max(1) as f32;
    let area_ratio = candidate_area / container_area;

    width_ratio <= ANCHORED_TRANSIENT_MAX_WIDTH_RATIO
        && height_ratio <= ANCHORED_TRANSIENT_MAX_HEIGHT_RATIO
        && area_ratio <= ANCHORED_TRANSIENT_MAX_AREA_RATIO
}

fn embedded_panel_ratio_ok(container: DomainRect, candidate: DomainRect) -> bool {
    let width_ratio = candidate.width as f32 / container.width.max(1) as f32;
    let height_ratio = candidate.height as f32 / container.height.max(1) as f32;
    let candidate_area = candidate.width as f32 * candidate.height as f32;
    let container_area = container.width.max(1) as f32 * container.height.max(1) as f32;
    let area_ratio = candidate_area / container_area;

    width_ratio <= EMBEDDED_PANEL_MAX_WIDTH_RATIO
        && height_ratio <= EMBEDDED_PANEL_MAX_HEIGHT_RATIO
        && area_ratio <= EMBEDDED_PANEL_MAX_AREA_RATIO
}

fn rects_overlap(left: DomainRect, right: DomainRect) -> bool {
    let left_right = left
        .x
        .saturating_add(left.width.min(i32::MAX as u32) as i32);
    let left_bottom = left
        .y
        .saturating_add(left.height.min(i32::MAX as u32) as i32);
    let right_right = right
        .x
        .saturating_add(right.width.min(i32::MAX as u32) as i32);
    let right_bottom = right
        .y
        .saturating_add(right.height.min(i32::MAX as u32) as i32);

    left.x < right_right && left_right > right.x && left.y < right_bottom && left_bottom > right.y
}

fn rects_are_proximate(left: DomainRect, right: DomainRect, threshold_px: i32) -> bool {
    let expanded_left = DomainRect::new(
        left.x.saturating_sub(threshold_px),
        left.y.saturating_sub(threshold_px),
        left.width
            .saturating_add((threshold_px.max(0) as u32).saturating_mul(2)),
        left.height
            .saturating_add((threshold_px.max(0) as u32).saturating_mul(2)),
    );

    rects_overlap(expanded_left, right)
}

fn is_window_cloaked(window_handle: HWND) -> bool {
    let mut cloaked = 0u32;
    let result = {
        // SAFETY: We pass a valid pointer to a `u32` buffer and the documented attribute size.
        unsafe {
            DwmGetWindowAttribute(
                window_handle,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut _ as *mut c_void,
                size_of::<u32>() as u32,
            )
        }
    };
    result >= 0 && cloaked != 0
}

fn query_window_rect(window_handle: HWND) -> Option<DomainRect> {
    let outer_rect = query_outer_window_rect(window_handle)?;
    let visible_rect = query_extended_frame_rect(window_handle)
        .filter(|visible_rect| visible_frame_is_compatible(outer_rect, *visible_rect))
        .unwrap_or(outer_rect);

    Some(domain_rect_from_win32(visible_rect))
}

fn query_extended_frame_rect(window_handle: HWND) -> Option<RECT> {
    let mut rect: RECT = {
        // SAFETY: `RECT` is a plain Win32 structure and is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let result = {
        // SAFETY: We pass a valid pointer to a writable `RECT` buffer with the documented size.
        unsafe {
            DwmGetWindowAttribute(
                window_handle,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut _ as *mut c_void,
                size_of::<RECT>() as u32,
            )
        }
    };
    if result < 0 {
        return None;
    }

    Some(rect)
}

fn query_outer_window_rect(window_handle: HWND) -> Option<RECT> {
    let mut rect: RECT = {
        // SAFETY: `RECT` is a plain Win32 structure and is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let ok = {
        // SAFETY: `rect` points to writable memory for the synchronous Win32 call.
        unsafe { GetWindowRect(window_handle, &mut rect) != 0 }
    };
    if !ok {
        return None;
    }

    Some(rect)
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

fn domain_rect_from_win32(rect: RECT) -> DomainRect {
    DomainRect::new(
        rect.left,
        rect.top,
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

fn query_window_title(window_handle: HWND) -> String {
    let length = {
        // SAFETY: `GetWindowTextLengthW` is a pure Win32 query on a window handle.
        unsafe { GetWindowTextLengthW(window_handle) }
    };
    if length <= 0 {
        return String::new();
    }

    let mut buffer = vec![0u16; usize::try_from(length).unwrap_or_default() + 1];
    let copied = {
        // SAFETY: `buffer` has enough space for the title and terminating null returned by Win32.
        unsafe { GetWindowTextW(window_handle, buffer.as_mut_ptr(), buffer.len() as i32) }
    };
    wide_buffer_to_string(&buffer, copied)
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_POPUP, WS_THICKFRAME};

    use super::{
        PlatformSnapshot, PlatformWindowSnapshot, TRANSIENT_POPUP_MAX_HEIGHT_PX,
        TRANSIENT_POPUP_MAX_WIDTH_PX, anchored_transient_ratio_ok, embedded_panel_ratio_ok,
        filter_embedded_transient_windows, is_transient_shell_overlay,
        looks_like_transient_popup_window, visible_frame_is_compatible,
    };
    use flowtile_domain::Rect as DomainRect;

    #[test]
    fn visible_frame_is_rejected_when_it_escapes_outer_rect() {
        let outer = RECT {
            left: 0,
            top: 0,
            right: 1200,
            bottom: 800,
        };
        let incompatible = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };

        assert!(!visible_frame_is_compatible(outer, incompatible));
    }

    #[test]
    fn transient_shell_overlay_is_detected_by_multitasking_class() {
        assert!(is_transient_shell_overlay(
            "MultitaskingViewFrame",
            "",
            Some("explorer"),
        ));
    }

    #[test]
    fn transient_shell_overlay_is_detected_by_capture_process() {
        assert!(is_transient_shell_overlay(
            "ApplicationFrameWindow",
            "Screen clip",
            Some("ScreenClippingHost"),
        ));
        assert!(is_transient_shell_overlay(
            "Microsoft.UI.Content.DesktopChildSiteBridge",
            "Панель инструментов записи",
            Some("SnippingTool.exe"),
        ));
    }

    #[test]
    fn ordinary_app_window_is_not_treated_as_transient_shell_overlay() {
        assert!(!is_transient_shell_overlay(
            "Notepad",
            "notes.txt - Notepad",
            Some("notepad"),
        ));
    }

    #[test]
    fn popup_without_caption_or_thickframe_is_not_treated_as_real_user_window() {
        assert!(looks_like_transient_popup_window(WS_POPUP));
    }

    #[test]
    fn overlapped_or_resizable_window_is_not_treated_as_transient_popup() {
        assert!(!looks_like_transient_popup_window(WS_POPUP | WS_CAPTION));
        assert!(!looks_like_transient_popup_window(WS_POPUP | WS_THICKFRAME));
    }

    #[test]
    fn transient_popup_size_thresholds_stay_conservative() {
        let popup_rect = DomainRect::new(
            0,
            0,
            TRANSIENT_POPUP_MAX_WIDTH_PX,
            TRANSIENT_POPUP_MAX_HEIGHT_PX,
        );
        assert!(popup_rect.width <= TRANSIENT_POPUP_MAX_WIDTH_PX);
        assert!(popup_rect.height <= TRANSIENT_POPUP_MAX_HEIGHT_PX);
    }

    #[test]
    fn popup_with_caption_is_not_classified_as_small_transient_fast_path() {
        assert!(!looks_like_transient_popup_window(WS_POPUP | WS_CAPTION));
    }

    #[test]
    fn anchored_transient_ratio_detects_small_surface_inside_parent() {
        assert!(anchored_transient_ratio_ok(
            DomainRect::new(0, 0, 1200, 800),
            DomainRect::new(300, 200, 320, 180),
        ));
        assert!(!anchored_transient_ratio_ok(
            DomainRect::new(0, 0, 1200, 800),
            DomainRect::new(100, 100, 1100, 700),
        ));
    }

    #[test]
    fn embedded_panel_ratio_detects_medium_popup_inside_parent() {
        assert!(embedded_panel_ratio_ok(
            DomainRect::new(0, 0, 1888, 988),
            DomainRect::new(49, 150, 567, 650),
        ));
        assert!(!embedded_panel_ratio_ok(
            DomainRect::new(0, 0, 1888, 988),
            DomainRect::new(0, 0, 1700, 960),
        ));
    }

    #[test]
    fn embedded_transient_window_is_removed_from_snapshot() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(0, 0, 1200, 800),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(250, 180, 320, 180),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].hwnd, 100);
    }

    #[test]
    fn titled_compact_window_is_not_removed_as_transient() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(0, 0, 1200, 800),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Downloads".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(250, 180, 320, 180),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 2);
    }

    #[test]
    fn anchored_transient_window_outside_parent_edge_is_removed_from_snapshot() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(0, 0, 1200, 800),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(1180, 240, 380, 40),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].hwnd, 100);
    }

    #[test]
    fn anchored_transient_window_from_sibling_process_is_removed_from_snapshot() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge.exe".to_string()),
                    rect: DomainRect::new(0, 0, 1200, 800),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 43,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(1180, 240, 380, 40),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].hwnd, 100);
    }

    #[test]
    fn nearby_large_window_of_same_process_is_not_removed_as_transient() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(0, 0, 1200, 800),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 102,
                    title: "Second window".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(1220, 0, 900, 760),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 2);
    }

    #[test]
    fn chromium_engine_internal_legacy_window_is_removed_from_snapshot() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(0, 0, 1888, 988),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "Chrome Legacy Window".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(69, 117, 1830, 882),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].hwnd, 100);
    }

    #[test]
    fn empty_title_browser_profile_panel_is_removed_from_snapshot() {
        let mut snapshot = PlatformSnapshot {
            foreground_hwnd: Some(100),
            monitors: Vec::new(),
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 100,
                    title: "Main".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 42,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(16, 16, 1888, 988),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 101,
                    title: "".to_string(),
                    class_name: "Chrome_WidgetWin_1".to_string(),
                    process_id: 43,
                    process_name: Some("msedge".to_string()),
                    rect: DomainRect::new(49, 150, 567, 650),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        filter_embedded_transient_windows(&mut snapshot);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].hwnd, 100);
    }
}

fn query_window_class(window_handle: HWND) -> String {
    let mut buffer = vec![0u16; 256];
    let copied = {
        // SAFETY: `buffer` points to writable memory for the synchronous class name query.
        unsafe { GetClassNameW(window_handle, buffer.as_mut_ptr(), buffer.len() as i32) }
    };
    wide_buffer_to_string(&buffer, copied)
}

fn describe_monitor_for_window(window_handle: HWND) -> Option<(HMONITOR, PlatformMonitorSnapshot)> {
    let monitor_handle = {
        // SAFETY: `MonitorFromWindow` is a pure Win32 query using the current window handle.
        unsafe { MonitorFromWindow(window_handle, MONITOR_DEFAULTTONEAREST) }
    };
    if monitor_handle.is_null() {
        return None;
    }

    describe_monitor(monitor_handle).map(|monitor| (monitor_handle, monitor))
}

fn describe_monitor(monitor_handle: HMONITOR) -> Option<PlatformMonitorSnapshot> {
    if monitor_handle.is_null() {
        return None;
    }

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..{
                // SAFETY: `MONITORINFO` is a plain old data structure from Win32 and
                // zero-initialization is valid before we immediately set `cbSize`.
                unsafe { unsafe_zeroed_monitor_info() }
            }
        },
        szDevice: [0; 32],
    };
    let ok = {
        // SAFETY: `info` is a valid `MONITORINFOEXW` buffer and can be passed as a
        // `MONITORINFO` pointer according to the Win32 contract.
        unsafe {
            GetMonitorInfoW(
                monitor_handle,
                &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
            ) != 0
        }
    };
    if !ok {
        return None;
    }

    let mut dpi_x = DEFAULT_DPI;
    let mut dpi_y = DEFAULT_DPI;
    let dpi_result = {
        // SAFETY: We pass writable `u32` buffers for the monitor DPI query.
        unsafe { GetDpiForMonitor(monitor_handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    };
    if dpi_result < 0 {
        dpi_x = DEFAULT_DPI;
        dpi_y = DEFAULT_DPI;
    }

    Some(PlatformMonitorSnapshot {
        binding: wide_array_to_string(&info.szDevice),
        work_area_rect: DomainRect::new(
            info.monitorInfo.rcWork.left,
            info.monitorInfo.rcWork.top,
            (info.monitorInfo.rcWork.right - info.monitorInfo.rcWork.left).max(0) as u32,
            (info.monitorInfo.rcWork.bottom - info.monitorInfo.rcWork.top).max(0) as u32,
        ),
        dpi: dpi_x.max(dpi_y),
        is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
    })
}

fn query_process_id(window_handle: HWND) -> u32 {
    let mut process_id = 0u32;
    let _ = {
        // SAFETY: `process_id` points to writable memory for the synchronous Win32 query.
        unsafe { GetWindowThreadProcessId(window_handle, &mut process_id) }
    };
    process_id
}

fn query_process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }

    let process_handle = {
        // SAFETY: `OpenProcess` is called with read-only query rights for an existing PID.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) }
    };
    if process_handle.is_null() {
        return None;
    }

    let mut buffer = vec![0u16; 260];
    let mut length = buffer.len() as u32;
    let queried = {
        // SAFETY: `buffer` points to writable UTF-16 storage and `length` is its capacity.
        unsafe {
            QueryFullProcessImageNameW(process_handle, 0, buffer.as_mut_ptr(), &mut length) != 0
        }
    };
    let _ = {
        // SAFETY: `process_handle` came from `OpenProcess` and is closed exactly once here.
        unsafe { CloseHandle(process_handle) }
    };
    if !queried || length == 0 {
        return None;
    }

    let path = String::from_utf16_lossy(&buffer[..usize::try_from(length).ok()?]);
    let stem = Path::new(&path).file_stem()?.to_string_lossy().into_owned();
    if stem.is_empty() { None } else { Some(stem) }
}

fn wide_buffer_to_string(buffer: &[u16], copied: i32) -> String {
    let length = usize::try_from(copied.max(0)).unwrap_or_default();
    String::from_utf16_lossy(&buffer[..length])
}

fn wide_array_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn hwnd_from_raw(hwnd: u64) -> Result<HWND, &'static str> {
    isize::try_from(hwnd)
        .map(|value| value as HWND)
        .map_err(|_| "window handle does not fit pointer width")
}

fn hwnd_to_raw(hwnd: HWND) -> Option<u64> {
    Some(hwnd as usize as u64)
}

fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: Reading the thread-local Win32 last-error code immediately after a failed API
        // call is the intended contract of `GetLastError`.
        unsafe { GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}

unsafe fn unsafe_zeroed_monitor_info() -> MONITORINFO {
    // SAFETY: `MONITORINFO` is a plain old data structure from Win32 and zero-initialization is
    // valid before we immediately set `cbSize`.
    unsafe { zeroed() }
}

#[derive(Default)]
struct MonitorEnumContext {
    bindings: HashSet<String>,
    monitors: Vec<PlatformMonitorSnapshot>,
    monitors_by_handle: HashMap<isize, PlatformMonitorSnapshot>,
}

struct WindowEnumContext {
    shell_hwnd: HWND,
    foreground_hwnd: HWND,
    monitors_by_handle: HashMap<isize, PlatformMonitorSnapshot>,
    windows: Vec<PlatformWindowSnapshot>,
}
