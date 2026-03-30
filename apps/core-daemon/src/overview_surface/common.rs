fn position_backdrop(
    window: HWND,
    rect: Rect,
    placement: &mut OverlayWindowPlacement,
) -> Result<(), String> {
    position_window(window, rect, true, placement)
}

fn overview_thumbnail_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        overview_env_flag(OVERVIEW_THUMBNAIL_DIAGNOSTICS_ENV)
    })
}

fn overview_thumbnail_client_only_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os(OVERVIEW_THUMBNAIL_CLIENT_ONLY_ENV)
            .map(|value| {
                let normalized = value.to_string_lossy().trim().to_ascii_lowercase();
                !(normalized == "0" || normalized == "false")
            })
            .unwrap_or(true)
    })
}

fn overview_env_flag(name: &str) -> bool {
    std::env::var_os(name)
        .map(|value| {
            let normalized = value.to_string_lossy().trim().to_ascii_lowercase();
            !(normalized.is_empty() || normalized == "0" || normalized == "false")
        })
        .unwrap_or(false)
}

fn position_window(
    window: HWND,
    rect: Rect,
    show: bool,
    placement: &mut OverlayWindowPlacement,
) -> Result<(), String> {
    position_window_with_order(window, rect, show, true, None, placement)
}

fn resolve_z_order_target(
    insert_after: Option<HWND>,
    topmost: bool,
    placement: &OverlayWindowPlacement,
    flags: &mut u32,
) -> HWND {
    if insert_after.is_some() {
        // `SetWindowPos` places `window` behind `hWndInsertAfter`, not above it.
        // Overview needs the inverse layering, so relative stacking is expressed
        // by call order and an explicit move to the top of the topmost band.
        return if topmost { HWND_TOPMOST } else { null_mut() };
    }

    if placement.topmost != topmost {
        return if topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
    }

    *flags |= SWP_NOZORDER;
    null_mut()
}

fn position_window_with_order(
    window: HWND,
    rect: Rect,
    show: bool,
    topmost: bool,
    insert_after: Option<HWND>,
    placement: &mut OverlayWindowPlacement,
) -> Result<(), String> {
    let width =
        i32::try_from(rect.width.max(1)).map_err(|_| "overview width overflowed".to_string())?;
    let height =
        i32::try_from(rect.height.max(1)).map_err(|_| "overview height overflowed".to_string())?;
    let ordered_after = insert_after.map(|hwnd| hwnd as isize);
    if placement.visible
        && placement.rect == Some(rect)
        && placement.topmost == topmost
        && placement.child_insert_after == ordered_after
    {
        return Ok(());
    }

    let mut flags = SWP_NOACTIVATE;
    if show && !placement.visible {
        flags |= SWP_SHOWWINDOW;
    }

    let insert_after = resolve_z_order_target(insert_after, topmost, placement, &mut flags);

    let applied = {
        // SAFETY: `window` is a valid popup surface owned by this thread.
        unsafe { SetWindowPos(window, insert_after, rect.x, rect.y, width, height, flags) }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    placement.rect = Some(rect);
    placement.topmost = topmost;
    placement.child_insert_after = ordered_after;
    if show {
        placement.visible = true;
    }

    Ok(())
}

fn hide_overlay_window(window: HWND, placement: &mut OverlayWindowPlacement) {
    if !placement.visible {
        return;
    }

    let _ = {
        // SAFETY: best-effort hide for the overview-owned popup surface.
        unsafe { ShowWindow(window, SW_HIDE) }
    };
    placement.visible = false;
    placement.child_insert_after = None;
}


fn pump_messages() -> Result<(), String> {
    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 structure and valid when zero-initialized.
        unsafe { zeroed() }
    };
    loop {
        let has_message = {
            // SAFETY: polls the current thread queue and removes available messages.
            unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) }
        };
        if has_message == 0 {
            break;
        }
        if message.message == WM_QUIT {
            return Ok(());
        }
        let _ = {
            // SAFETY: translate and dispatch the message that was just dequeued.
            unsafe { TranslateMessage(&message) }
        };
        unsafe { DispatchMessageW(&message) };
    }
    Ok(())
}

fn rect_to_win32(rect: Rect, label: &str) -> Result<RECT, String> {
    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| format!("{label} width exceeds Win32 limits"))?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| format!("{label} height exceeds Win32 limits"))?;
    let right = rect
        .x
        .checked_add(width)
        .ok_or_else(|| format!("{label} right edge overflowed"))?;
    let bottom = rect
        .y
        .checked_add(height)
        .ok_or_else(|| format!("{label} bottom edge overflowed"))?;
    Ok(RECT {
        left: rect.x,
        top: rect.y,
        right,
        bottom,
    })
}

fn paint_solid_rect(hdc: HDC, rect: RECT, color: u32) {
    let brush = {
        // SAFETY: queries a process-global stock brush handle owned by GDI.
        unsafe { GetStockObject(DC_BRUSH) }
    };
    if brush.is_null() {
        return;
    }

    let _ = {
        // SAFETY: updates the color of the stock DC brush for the current target DC.
        unsafe { SetDCBrushColor(hdc, color) }
    };
    let _ = {
        // SAFETY: fills the target rectangle with the selected stock DC brush color.
        unsafe { FillRect(hdc, &rect, brush as HBRUSH) }
    };
}

fn paint_alpha_rect(hdc: HDC, rect: RECT, color: u32, alpha: u8) {
    if alpha == 0 {
        return;
    }
    if alpha == u8::MAX {
        paint_solid_rect(hdc, rect, color);
        return;
    }

    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let memory_dc = {
        // SAFETY: creates a compatible memory DC for a temporary 1x1 source bitmap.
        unsafe { CreateCompatibleDC(hdc) }
    };
    if memory_dc.is_null() {
        paint_solid_rect(hdc, rect, color);
        return;
    }

    let bitmap = {
        // SAFETY: allocates a minimal compatible bitmap used only as a solid-color alpha source.
        unsafe { CreateCompatibleBitmap(hdc, 1, 1) }
    };
    if bitmap.is_null() {
        let _ = {
            // SAFETY: releases the temporary memory DC created above.
            unsafe { DeleteDC(memory_dc) }
        };
        paint_solid_rect(hdc, rect, color);
        return;
    }

    let previous_bitmap = {
        // SAFETY: selects the temporary bitmap into the memory DC for solid fill and alpha blit.
        unsafe { SelectObject(memory_dc, bitmap as HGDIOBJ) }
    };
    if previous_bitmap.is_null() {
        let _ = {
            // SAFETY: releases the temporary bitmap and DC on early failure.
            unsafe { DeleteObject(bitmap as HGDIOBJ) }
        };
        let _ = {
            // SAFETY: releases the temporary memory DC created above.
            unsafe { DeleteDC(memory_dc) }
        };
        paint_solid_rect(hdc, rect, color);
        return;
    }

    let source_rect = RECT {
        left: 0,
        top: 0,
        right: 1,
        bottom: 1,
    };
    paint_solid_rect(memory_dc, source_rect, color);

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: 0,
    };
    let blended = {
        // SAFETY: alpha-blends the temporary solid-color bitmap into the destination rect.
        unsafe { GdiAlphaBlend(hdc, rect.left, rect.top, width, height, memory_dc, 0, 0, 1, 1, blend) }
    };
    if blended == 0 {
        paint_solid_rect(hdc, rect, color);
    }

    let _ = {
        // SAFETY: restores the previous bitmap selection before cleanup.
        unsafe { SelectObject(memory_dc, previous_bitmap) }
    };
    let _ = {
        // SAFETY: releases the temporary bitmap once after the blit completes.
        unsafe { DeleteObject(bitmap as HGDIOBJ) }
    };
    let _ = {
        // SAFETY: releases the temporary memory DC created above.
        unsafe { DeleteDC(memory_dc) }
    };
}

fn hwnd_from_raw(raw_hwnd: u64) -> Option<HWND> {
    isize::try_from(raw_hwnd).ok().map(|hwnd| hwnd as HWND)
}

const fn rgb_color(red: u8, green: u8, blue: u8) -> u32 {
    (red as u32) | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn widestring(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: reads the current thread-local Win32 last-error code.
        unsafe { GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}
