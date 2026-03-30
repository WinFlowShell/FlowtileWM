fn initialize_overview_classes() -> Result<OverviewClasses, String> {
    let instance = {
        // SAFETY: required to register Win32 window classes in the current module.
        unsafe { GetModuleHandleW(null()) }
    };
    register_window_class(
        instance as HINSTANCE,
        BACKDROP_CLASS,
        backdrop_window_proc,
        None,
    )?;
    register_window_class(
        instance as HINSTANCE,
        PREVIEW_CLASS,
        preview_window_proc,
        None,
    )?;

    Ok(OverviewClasses {
        instance: instance as HINSTANCE,
    })
}

fn register_window_class(
    instance: HINSTANCE,
    class_name: &str,
    window_proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    brush: Option<HBRUSH>,
) -> Result<(), String> {
    let wide_class_name = widestring(class_name);
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: wide_class_name.as_ptr(),
        hbrBackground: brush.unwrap_or(null_mut()),
        ..unsafe { zeroed() }
    };
    let atom = {
        // SAFETY: the class descriptor references live memory for the duration of the call.
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

unsafe extern "system" fn backdrop_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            dispatch_overview_dismiss();
            0
        }
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = {
                // SAFETY: `PAINTSTRUCT` is a plain Win32 struct and valid when zero-initialized.
                unsafe { zeroed() }
            };
            let hdc = {
                // SAFETY: `BeginPaint` is the documented entry for painting this HWND on `WM_PAINT`.
                unsafe { BeginPaint(hwnd, &mut paint) }
            };
            paint_backdrop(hwnd, hdc);
            let _ = {
                // SAFETY: `EndPaint` completes the matching paint cycle for this `WM_PAINT`.
                unsafe { EndPaint(hwnd, &paint) }
            };
            0
        }
        WM_NCDESTROY => {
            clear_backdrop_snapshot(hwnd);
            let user_data = {
                // SAFETY: reads back the pointer previously stored in `GWLP_USERDATA`.
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
            };
            if user_data != 0 {
                let _ = {
                    // SAFETY: clears the user-data slot before the HWND is fully destroyed.
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) }
                };
                let _ = {
                    // SAFETY: ownership of the boxed state belongs to the window and is released once.
                    unsafe { Box::from_raw(user_data as *mut BackdropWindowState) }
                };
            }

            // SAFETY: destruction still finishes through the default Win32 procedure.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => {
            // SAFETY: all non-paint messages fall back to the default Win32 procedure.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

unsafe extern "system" fn preview_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_LBUTTONDOWN => {
            let point = point_from_lparam(lparam);
            if let Some(target) = preview_target_at_point_for_window(hwnd, point.0, point.1) {
                begin_preview_drag_session(hwnd, point.0, point.1, target.hwnd);
            }
            0
        }
        WM_MOUSEMOVE => {
            let point = point_from_lparam(lparam);
            update_preview_drag_session(hwnd, point.0, point.1);
            0
        }
        WM_CAPTURECHANGED => {
            cancel_preview_drag_session(hwnd);
            0
        }
        WM_LBUTTONUP => {
            let point = point_from_lparam(lparam);
            match finish_preview_pointer_interaction(hwnd, point.0, point.1) {
                PreviewPointerOutcome::ActivateWindow(raw_hwnd) => {
                    dispatch_overview_activate_window(raw_hwnd);
                }
                PreviewPointerOutcome::MoveColumn {
                    dragged_raw_hwnd,
                    target_workspace_id,
                    insert_after_raw_hwnd,
                } => {
                    dispatch_overview_move_column(
                        dragged_raw_hwnd,
                        target_workspace_id,
                        insert_after_raw_hwnd,
                    );
                }
                PreviewPointerOutcome::Dismiss => {
                    dispatch_overview_dismiss();
                }
                PreviewPointerOutcome::None => {}
            }
            0
        }
        WM_RBUTTONUP | WM_MBUTTONUP => {
            cancel_preview_drag_session(hwnd);
            dispatch_overview_dismiss();
            0
        }
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = {
                // SAFETY: `PAINTSTRUCT` is a plain Win32 struct and valid when zero-initialized.
                unsafe { zeroed() }
            };
            let _hdc = {
                // SAFETY: `BeginPaint` is the documented entry for painting this HWND on `WM_PAINT`.
                unsafe { BeginPaint(hwnd, &mut paint) }
            };
            let _ = {
                // SAFETY: `EndPaint` completes the matching paint cycle for this `WM_PAINT`.
                unsafe { EndPaint(hwnd, &paint) }
            };
            0
        }
        WM_NCDESTROY => {
            let user_data = {
                // SAFETY: reads back the pointer previously stored in `GWLP_USERDATA`.
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
            };
            if user_data != 0 {
                let _ = {
                    // SAFETY: clears the user-data slot before the HWND is fully destroyed.
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) }
                };
                let _ = {
                    // SAFETY: ownership of the boxed state belongs to the window and is released once.
                    unsafe { Box::from_raw(user_data as *mut PreviewWindowState) }
                };
            }

            // SAFETY: destruction still finishes through the default Win32 procedure.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => {
            // SAFETY: all unhandled messages use the default Win32 procedure.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

