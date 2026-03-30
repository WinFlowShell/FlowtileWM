fn create_backdrop_window(instance: HINSTANCE) -> Result<HWND, String> {
    let class_name = widestring(BACKDROP_CLASS);
    let window = {
        // SAFETY: creates a no-activate popup surface used only as overview backdrop.
        unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                null(),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        }
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }

    initialize_backdrop_window_state(window)?;

    Ok(window)
}


fn initialize_backdrop_window_state(hwnd: HWND) -> Result<(), String> {
    let state = Box::new(BackdropWindowState {
        shell_snapshot_bitmap: null_mut(),
        viewport_column_rect: None,
    });
    let raw_state = Box::into_raw(state);
    let previous = {
        // SAFETY: stores process-local backdrop state pointer for this HWND.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_state as isize) }
    };
    if previous != 0 {
        let _ = {
            // SAFETY: ownership returns to Rust if the user-data slot was unexpectedly occupied.
            unsafe { Box::from_raw(raw_state) }
        };
        return Err("overview backdrop user data was already initialized".to_string());
    }
    Ok(())
}

fn backdrop_window_state_ptr(hwnd: HWND) -> *mut BackdropWindowState {
    // SAFETY: reads the backdrop state pointer previously stored in `GWLP_USERDATA`.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BackdropWindowState }
}

fn paint_backdrop(hwnd: HWND, hdc: HDC) {
    let state_ptr = backdrop_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        paint_backdrop_fill(hwnd, hdc);
        return;
    }

    let snapshot_bitmap = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { (*state_ptr).shell_snapshot_bitmap }
    };
    if snapshot_bitmap.is_null() {
        paint_backdrop_fill(hwnd, hdc);
    } else {
        paint_backdrop_snapshot(hwnd, hdc, snapshot_bitmap);
    }

    let viewport_column_rect = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { (*state_ptr).viewport_column_rect }
    };
    if let Some(viewport_column_rect) = viewport_column_rect {
        paint_backdrop_viewport_column(hdc, viewport_column_rect);
    }
}

fn paint_backdrop_fill(hwnd: HWND, hdc: HDC) {
    let mut client_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = {
        // SAFETY: queries the client bounds of the window being painted.
        unsafe { GetClientRect(hwnd, &mut client_rect) }
    };
    let painted = {
        // SAFETY: paints the current desktop wallpaper/pattern into the backdrop DC without
        // exposing underlying live windows.
        unsafe { PaintDesktop(hdc) }
    };
    if painted == 0 {
        paint_solid_rect(hdc, client_rect, OVERVIEW_BACKDROP_COLOR);
    }
}

fn paint_backdrop_snapshot(hwnd: HWND, hdc: HDC, bitmap: HBITMAP) {
    let mut client_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = {
        // SAFETY: queries the client bounds of the window being painted.
        unsafe { GetClientRect(hwnd, &mut client_rect) }
    };
    let width = (client_rect.right - client_rect.left).max(1);
    let height = (client_rect.bottom - client_rect.top).max(1);

    let memory_dc = {
        // SAFETY: creates a compatible memory DC for the current paint target.
        unsafe { CreateCompatibleDC(hdc) }
    };
    if memory_dc.is_null() {
        paint_backdrop_fill(hwnd, hdc);
        return;
    }

    let previous_bitmap = {
        // SAFETY: selects the captured bitmap into the memory DC for a read-only blit.
        unsafe { SelectObject(memory_dc, bitmap as HGDIOBJ) }
    };
    if previous_bitmap.is_null() {
        let _ = {
            // SAFETY: releases the temporary memory DC created for this paint cycle.
            unsafe { DeleteDC(memory_dc) }
        };
        paint_backdrop_fill(hwnd, hdc);
        return;
    }

    let _ = {
        // SAFETY: copies the stored snapshot bitmap into the backdrop paint target.
        unsafe { BitBlt(hdc, 0, 0, width, height, memory_dc, 0, 0, SRCCOPY) }
    };
    let _ = {
        // SAFETY: restores the previous object selection before deleting the temporary DC.
        unsafe { SelectObject(memory_dc, previous_bitmap) }
    };
    let _ = {
        // SAFETY: releases the temporary memory DC created for this paint cycle.
        unsafe { DeleteDC(memory_dc) }
    };
}

fn paint_backdrop_viewport_column(hdc: HDC, column_rect: Rect) {
    let column_rect = rect_to_win32(column_rect, "backdrop viewport column").unwrap_or(RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    });
    paint_alpha_rect(
        hdc,
        column_rect,
        WORKSPACE_PREVIEW_BACKGROUND_COLOR,
        OVERVIEW_VIEWPORT_COLUMN_ALPHA,
    );
}

fn set_backdrop_snapshot(hwnd: HWND, bitmap: HBITMAP) {
    let state_ptr = backdrop_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        destroy_bitmap(bitmap);
        return;
    }

    let previous_bitmap = {
        // SAFETY: mutates the owned backdrop state for this HWND in place.
        unsafe {
            let previous = (*state_ptr).shell_snapshot_bitmap;
            (*state_ptr).shell_snapshot_bitmap = bitmap;
            previous
        }
    };
    destroy_bitmap(previous_bitmap);
    let _ = {
        // SAFETY: requests a repaint of the whole backdrop client area after the snapshot changes.
        unsafe { InvalidateRect(hwnd, null(), 1) }
    };
}

fn clear_backdrop_snapshot(hwnd: HWND) {
    set_backdrop_snapshot(hwnd, null_mut());
}

fn set_backdrop_viewport_column(hwnd: HWND, rect: Option<Rect>) {
    let state_ptr = backdrop_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let state_changed = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { (*state_ptr).viewport_column_rect != rect }
    };
    if !state_changed {
        return;
    }

    {
        // SAFETY: mutates the owned backdrop state for this HWND in place.
        unsafe {
            (*state_ptr).viewport_column_rect = rect;
        }
    }
    let _ = {
        // SAFETY: requests a repaint of the whole backdrop client area after the column changes.
        unsafe { InvalidateRect(hwnd, null(), 1) }
    };
}

fn clear_backdrop_viewport_column(hwnd: HWND) {
    set_backdrop_viewport_column(hwnd, None);
}

fn destroy_bitmap(bitmap: HBITMAP) {
    if bitmap.is_null() {
        return;
    }
    let _ = {
        // SAFETY: releases the owned GDI bitmap handle once.
        unsafe { DeleteObject(bitmap as HGDIOBJ) }
    };
}

fn capture_screen_bitmap(rect: Rect) -> Result<HBITMAP, String> {
    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "shell snapshot width overflowed".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "shell snapshot height overflowed".to_string())?;
    let screen_dc = {
        // SAFETY: queries the composited screen DC for the current desktop.
        unsafe { GetDC(null_mut()) }
    };
    if screen_dc.is_null() {
        return Err(last_error_message("GetDC"));
    }

    let memory_dc = {
        // SAFETY: creates a compatible memory DC for the screen capture operation.
        unsafe { CreateCompatibleDC(screen_dc) }
    };
    if memory_dc.is_null() {
        let _ = {
            // SAFETY: paired cleanup for the screen DC acquired above.
            unsafe { ReleaseDC(null_mut(), screen_dc) }
        };
        return Err(last_error_message("CreateCompatibleDC"));
    }

    let bitmap = {
        // SAFETY: allocates a compatible bitmap to hold the captured monitor-sized frame.
        unsafe { CreateCompatibleBitmap(screen_dc, width, height) }
    };
    if bitmap.is_null() {
        let _ = {
            // SAFETY: paired cleanup for the temporary memory DC.
            unsafe { DeleteDC(memory_dc) }
        };
        let _ = {
            // SAFETY: paired cleanup for the screen DC acquired above.
            unsafe { ReleaseDC(null_mut(), screen_dc) }
        };
        return Err(last_error_message("CreateCompatibleBitmap"));
    }

    let previous_bitmap = {
        // SAFETY: selects the target bitmap into the memory DC for the capture blit.
        unsafe { SelectObject(memory_dc, bitmap as HGDIOBJ) }
    };
    if previous_bitmap.is_null() {
        destroy_bitmap(bitmap);
        let _ = {
            // SAFETY: paired cleanup for the temporary memory DC.
            unsafe { DeleteDC(memory_dc) }
        };
        let _ = {
            // SAFETY: paired cleanup for the screen DC acquired above.
            unsafe { ReleaseDC(null_mut(), screen_dc) }
        };
        return Err(last_error_message("SelectObject"));
    }

    let copied = {
        // SAFETY: copies the current composited monitor image into the owned bitmap.
        unsafe {
            BitBlt(
                memory_dc, 0, 0, width, height, screen_dc, rect.x, rect.y, SRCCOPY,
            )
        }
    };
    let _ = {
        // SAFETY: restores the previous selection before cleaning up the memory DC.
        unsafe { SelectObject(memory_dc, previous_bitmap) }
    };
    let _ = {
        // SAFETY: paired cleanup for the temporary memory DC.
        unsafe { DeleteDC(memory_dc) }
    };
    let _ = {
        // SAFETY: paired cleanup for the screen DC acquired above.
        unsafe { ReleaseDC(null_mut(), screen_dc) }
    };
    if copied == 0 {
        destroy_bitmap(bitmap);
        return Err(last_error_message("BitBlt"));
    }

    Ok(bitmap)
}

