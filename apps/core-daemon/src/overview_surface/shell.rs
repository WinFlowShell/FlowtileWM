fn shell_screenshot_windows() -> ShellScreenshotWindows {
    let mut windows = ShellScreenshotWindows::default();
    let _ = {
        // SAFETY: the callback receives a pointer to `present` that remains valid for the
        // duration of the synchronous enumeration call.
        unsafe {
            EnumWindows(
                Some(shell_overlay_enum_proc),
                &mut windows as *mut ShellScreenshotWindows as LPARAM,
            )
        }
    };
    windows.foreground_screenshot_hwnd = foreground_screenshot_ui_hwnd();
    windows
}

unsafe extern "system" fn shell_overlay_enum_proc(hwnd: HWND, user_data: LPARAM) -> BOOL {
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

    let class_name = query_window_class(hwnd);
    let title = query_window_title(hwnd);
    let process_name = query_process_name_for_window(hwnd);
    let windows = {
        // SAFETY: `user_data` is a pointer to the enum accumulator passed into the synchronous call.
        unsafe { &mut *(user_data as *mut ShellScreenshotWindows) }
    };
    if is_shell_screenshot_overlay(&class_name, &title, process_name.as_deref()) {
        windows.overlay_present = true;
        return 1;
    }

    if is_shell_screenshot_result_window(&class_name, &title, process_name.as_deref()) {
        windows.result_window_hwnds.insert(hwnd as usize as u64);
    }

    1
}

fn is_shell_screenshot_overlay(class_name: &str, title: &str, process_name: Option<&str>) -> bool {
    let normalized_process = normalized_process_name(process_name).unwrap_or_default();
    if normalized_process == SCREEN_CLIPPING_HOST_PROCESS {
        return true;
    }

    let title = title.to_lowercase();
    if title.contains("screen clip")
        || title.contains("screen clipping")
        || title.contains("screen snip")
        || title.contains("ножницы")
        || title.contains("панель инструментов записи")
        || title.contains("recording toolbar")
    {
        return true;
    }

    let class_name = class_name.to_ascii_lowercase();
    normalized_process == SNIPPING_TOOL_PROCESS
        && matches!(
            class_name.as_str(),
            "applicationframewindow"
                | "microsoft.ui.content.desktopchildsitebridge"
                | "windows.ui.core.corewindow"
        )
        && (title.contains("screen")
            || title.contains("clip")
            || title.contains("snip")
            || title.contains("record")
            || title.contains("панель")
            || title.contains("ножниц"))
}

fn is_shell_screenshot_result_window(
    class_name: &str,
    title: &str,
    process_name: Option<&str>,
) -> bool {
    let _ = class_name;
    let _ = title;
    normalized_process_name(process_name).unwrap_or_default() == SNIPPING_TOOL_PROCESS
}

fn foreground_screenshot_ui_hwnd() -> Option<u64> {
    let hwnd = {
        // SAFETY: read-only query of the current desktop foreground window.
        unsafe { GetForegroundWindow() }
    };
    if hwnd.is_null() {
        return None;
    }

    let class_name = query_window_class(hwnd);
    let title = query_window_title(hwnd);
    let process_name = query_process_name_for_window(hwnd);
    if is_shell_screenshot_overlay(&class_name, &title, process_name.as_deref())
        || is_shell_screenshot_result_window(&class_name, &title, process_name.as_deref())
        || is_shell_screenshot_process(process_name.as_deref())
    {
        return Some(hwnd as usize as u64);
    }

    None
}

fn is_shell_screenshot_process(process_name: Option<&str>) -> bool {
    matches!(
        normalized_process_name(process_name).as_deref(),
        Some(SCREEN_CLIPPING_HOST_PROCESS | SNIPPING_TOOL_PROCESS)
    )
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

fn query_process_name_for_window(hwnd: HWND) -> Option<String> {
    let mut process_id = 0_u32;
    {
        // SAFETY: read-only ownership query for the enumerated HWND.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    }
    query_process_name(process_id)
}

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

fn query_window_class(hwnd: HWND) -> String {
    let mut buffer = vec![0_u16; 256];
    let copied = {
        // SAFETY: reads the class name of the enumerated HWND into the stack-owned buffer.
        unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) }
    };
    if copied <= 0 {
        return String::new();
    }

    String::from_utf16_lossy(&buffer[..usize::try_from(copied).unwrap_or_default()])
}

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

