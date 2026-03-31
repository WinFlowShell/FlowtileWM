mod window_filtering;

use std::{
    collections::{HashMap, HashSet},
    env,
    mem::{size_of, zeroed},
    sync::OnceLock,
};

use windows_sys::Win32::{
    Foundation::{BOOL, GetLastError, HWND, LPARAM, RECT},
    Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
        MONITORINFOEXW, MonitorFromWindow,
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{
            EnumWindows, GetForegroundWindow, GetShellWindow, MONITORINFOF_PRIMARY,
        },
    },
};

use self::window_filtering::{capture_window_snapshot, filter_embedded_transient_windows};
use crate::{
    PlatformMonitorSnapshot, PlatformSnapshot, PlatformWindowSnapshot, WindowsAdapterError, dpi,
};
use flowtile_domain::Rect as DomainRect;

const DEFAULT_DPI: u32 = 96;

#[derive(Clone, Copy, Debug, Default)]
struct ShellLayoutOverrides {
    use_monitor_bounds: bool,
    reserved_top_px: u32,
    reserved_right_px: u32,
    reserved_bottom_px: u32,
    reserved_left_px: u32,
}

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

    let shell_layout = shell_layout_overrides();
    let base_rect = if shell_layout.use_monitor_bounds {
        win32_rect_to_domain_rect(info.monitorInfo.rcMonitor)
    } else {
        win32_rect_to_domain_rect(info.monitorInfo.rcWork)
    };

    Some(PlatformMonitorSnapshot {
        binding: wide_array_to_string(&info.szDevice),
        work_area_rect: inset_domain_rect(
            base_rect,
            shell_layout.reserved_left_px,
            shell_layout.reserved_top_px,
            shell_layout.reserved_right_px,
            shell_layout.reserved_bottom_px,
        ),
        dpi: dpi_x.max(dpi_y),
        is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
    })
}

fn shell_layout_overrides() -> &'static ShellLayoutOverrides {
    static OVERRIDES: OnceLock<ShellLayoutOverrides> = OnceLock::new();
    OVERRIDES.get_or_init(|| ShellLayoutOverrides {
        use_monitor_bounds: env_flag("FLOWSHELL_USE_MONITOR_BOUNDS"),
        reserved_top_px: env_u32("FLOWSHELL_RESERVED_TOP_PX"),
        reserved_right_px: env_u32("FLOWSHELL_RESERVED_RIGHT_PX"),
        reserved_bottom_px: env_u32("FLOWSHELL_RESERVED_BOTTOM_PX"),
        reserved_left_px: env_u32("FLOWSHELL_RESERVED_LEFT_PX"),
    })
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
}

fn env_u32(name: &str) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn win32_rect_to_domain_rect(rect: RECT) -> DomainRect {
    DomainRect::new(
        rect.left,
        rect.top,
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

fn inset_domain_rect(rect: DomainRect, left: u32, top: u32, right: u32, bottom: u32) -> DomainRect {
    let horizontal = left.saturating_add(right);
    let vertical = top.saturating_add(bottom);
    let offset_x = i32::try_from(left).unwrap_or(i32::MAX);
    let offset_y = i32::try_from(top).unwrap_or(i32::MAX);

    DomainRect::new(
        rect.x.saturating_add(offset_x),
        rect.y.saturating_add(offset_y),
        rect.width.saturating_sub(horizontal),
        rect.height.saturating_sub(vertical),
    )
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
