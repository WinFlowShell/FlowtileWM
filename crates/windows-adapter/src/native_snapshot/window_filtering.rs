use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    path::Path,
};

use flowtile_domain::Rect as DomainRect;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, RECT},
    Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetWindow, GetWindowLongPtrW,
        GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, WS_CAPTION, WS_EX_TOOLWINDOW, WS_POPUP, WS_THICKFRAME,
    },
};

use crate::{PlatformSnapshot, PlatformWindowSnapshot};

use super::{describe_monitor_for_window, hwnd_to_raw, wide_buffer_to_string};

const CLASS_FILTER_CORE_WINDOW: &str = "Windows.UI.Core.CoreWindow";
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

pub(super) fn capture_window_snapshot(
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

pub(super) fn filter_embedded_transient_windows(snapshot: &mut PlatformSnapshot) {
    let transient_hwnds = snapshot
        .windows
        .iter()
        .filter(|candidate| is_process_anchored_transient_candidate(snapshot, candidate))
        .map(|window| window.hwnd)
        .collect::<std::collections::HashSet<_>>();

    if transient_hwnds.is_empty() {
        return;
    }

    snapshot
        .windows
        .retain(|window| !transient_hwnds.contains(&window.hwnd));
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
    let is_visible = { unsafe { IsWindowVisible(window_handle) != 0 } };
    if !is_visible {
        return false;
    }

    let is_iconic = { unsafe { IsIconic(window_handle) != 0 } };
    if is_iconic || is_window_cloaked(window_handle) {
        return false;
    }

    let owner = { unsafe { GetWindow(window_handle, GW_OWNER) } };
    if !owner.is_null() {
        return false;
    }

    let ex_style = { unsafe { GetWindowLongPtrW(window_handle, GWL_EXSTYLE) as u32 } };
    if (ex_style & WS_EX_TOOLWINDOW) != 0 {
        return false;
    }

    let style = { unsafe { GetWindowLongPtrW(window_handle, GWL_STYLE) as u32 } };
    !looks_like_transient_popup_window(style)
}

fn looks_like_transient_popup_window(style: u32) -> bool {
    let has_popup = (style & WS_POPUP) != 0;
    let has_caption = (style & WS_CAPTION) != 0;
    let has_thickframe = (style & WS_THICKFRAME) != 0;

    has_popup && !has_caption && !has_thickframe
}

fn is_small_popup_candidate(window_handle: HWND, rect: DomainRect) -> bool {
    let style = { unsafe { GetWindowLongPtrW(window_handle, GWL_STYLE) as u32 } };
    if !looks_like_transient_popup_window(style) {
        return false;
    }

    rect.width <= TRANSIENT_POPUP_MAX_WIDTH_PX && rect.height <= TRANSIENT_POPUP_MAX_HEIGHT_PX
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
    let mut rect: RECT = unsafe { zeroed() };
    let result = {
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
    let mut rect: RECT = unsafe { zeroed() };
    let ok = { unsafe { GetWindowRect(window_handle, &mut rect) != 0 } };
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
    let length = { unsafe { GetWindowTextLengthW(window_handle) } };
    if length <= 0 {
        return String::new();
    }

    let mut buffer = vec![0u16; usize::try_from(length).unwrap_or_default() + 1];
    let copied =
        { unsafe { GetWindowTextW(window_handle, buffer.as_mut_ptr(), buffer.len() as i32) } };
    wide_buffer_to_string(&buffer, copied)
}

fn query_window_class(window_handle: HWND) -> String {
    let mut buffer = vec![0u16; 256];
    let copied =
        { unsafe { GetClassNameW(window_handle, buffer.as_mut_ptr(), buffer.len() as i32) } };
    wide_buffer_to_string(&buffer, copied)
}

fn query_process_id(window_handle: HWND) -> u32 {
    let mut process_id = 0u32;
    let _ = { unsafe { GetWindowThreadProcessId(window_handle, &mut process_id) } };
    process_id
}

fn query_process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }

    let process_handle =
        { unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) } };
    if process_handle.is_null() {
        return None;
    }

    let mut buffer = vec![0u16; 260];
    let mut length = buffer.len() as u32;
    let queried = {
        unsafe {
            QueryFullProcessImageNameW(process_handle, 0, buffer.as_mut_ptr(), &mut length) != 0
        }
    };
    let _ = { unsafe { CloseHandle(process_handle) } };
    if !queried || length == 0 {
        return None;
    }

    let path = String::from_utf16_lossy(&buffer[..usize::try_from(length).ok()?]);
    let stem = Path::new(&path).file_stem()?.to_string_lossy().into_owned();
    if stem.is_empty() { None } else { Some(stem) }
}

#[cfg(test)]
mod tests {
    use flowtile_domain::Rect as DomainRect;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_POPUP, WS_THICKFRAME};

    use super::{
        PlatformSnapshot, PlatformWindowSnapshot, TRANSIENT_POPUP_MAX_HEIGHT_PX,
        TRANSIENT_POPUP_MAX_WIDTH_PX, anchored_transient_ratio_ok, embedded_panel_ratio_ok,
        filter_embedded_transient_windows, is_transient_shell_overlay,
        looks_like_transient_popup_window, visible_frame_is_compatible,
    };

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
