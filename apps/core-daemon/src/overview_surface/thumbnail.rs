fn register_thumbnail(destination: HWND, source: HWND) -> Result<isize, String> {
    let mut thumbnail = 0_isize;
    let result = {
        // SAFETY: registers a DWM thumbnail from the live source HWND into the preview window.
        unsafe { DwmRegisterThumbnail(destination, source, &mut thumbnail) }
    };
    if result < 0 {
        return Err(format!(
            "DwmRegisterThumbnail failed with HRESULT {result:#x}"
        ));
    }
    Ok(thumbnail)
}

fn thumbnail_source_size(thumbnail: isize) -> Result<Rect, String> {
    let mut size = SIZE { cx: 0, cy: 0 };
    let result = {
        // SAFETY: queries the live source size for a thumbnail that was successfully registered.
        unsafe { DwmQueryThumbnailSourceSize(thumbnail, &mut size) }
    };
    if result < 0 {
        return Err(format!(
            "DwmQueryThumbnailSourceSize failed with HRESULT {result:#x}"
        ));
    }

    let width = size.cx.max(1);
    let height = size.cy.max(1);
    Ok(Rect::new(0, 0, width as u32, height as u32))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FullWindowPreviewGeometry {
    destination_rect: Rect,
    outer_rect: Rect,
    visible_rect: Rect,
}

fn full_window_preview_geometry_for_preview(
    hwnd: HWND,
    visible_destination_rect: Rect,
) -> Option<FullWindowPreviewGeometry> {
    let outer_rect = rect_from_win32(query_outer_window_rect(hwnd)?);
    let visible_rect = rect_from_win32(query_visible_frame_rect(hwnd)?);
    visible_frame_is_compatible(outer_rect, visible_rect)?;
    let destination_rect =
        expand_destination_rect_to_outer_bounds(visible_destination_rect, outer_rect, visible_rect)?;
    Some(FullWindowPreviewGeometry {
        destination_rect,
        outer_rect,
        visible_rect,
    })
}

fn query_outer_window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect: RECT = {
        // SAFETY: `RECT` is a plain Win32 structure and is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let ok = {
        // SAFETY: `rect` points to writable memory for the synchronous Win32 call.
        unsafe { GetWindowRect(hwnd, &mut rect) != 0 }
    };
    ok.then_some(rect)
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
                &mut rect as *mut _ as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
        }
    };
    (result >= 0).then_some(rect)
}

fn rect_from_win32(rect: RECT) -> Rect {
    Rect::new(
        rect.left,
        rect.top,
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

fn visible_frame_is_compatible(outer_rect: Rect, visible_rect: Rect) -> Option<()> {
    (outer_rect.width > 0
        && outer_rect.height > 0
        && visible_rect.width > 0
        && visible_rect.height > 0
        && visible_rect.x >= outer_rect.x
        && visible_rect.y >= outer_rect.y
        && rect_right(visible_rect) <= rect_right(outer_rect)
        && rect_bottom(visible_rect) <= rect_bottom(outer_rect))
    .then_some(())
}

fn expand_destination_rect_to_outer_bounds(
    visible_destination_rect: Rect,
    outer_rect: Rect,
    visible_rect: Rect,
) -> Option<Rect> {
    if visible_destination_rect.width == 0
        || visible_destination_rect.height == 0
        || visible_rect.width == 0
        || visible_rect.height == 0
    {
        return None;
    }

    let scale_x = visible_destination_rect.width as f64 / visible_rect.width as f64;
    let scale_y = visible_destination_rect.height as f64 / visible_rect.height as f64;
    let left_inset = visible_rect.x.saturating_sub(outer_rect.x).max(0) as f64;
    let top_inset = visible_rect.y.saturating_sub(outer_rect.y).max(0) as f64;
    let right_inset = rect_right(outer_rect)
        .saturating_sub(rect_right(visible_rect))
        .max(0) as f64;
    let bottom_inset = rect_bottom(outer_rect)
        .saturating_sub(rect_bottom(visible_rect))
        .max(0) as f64;

    let left = (visible_destination_rect.x as f64 - left_inset * scale_x).floor();
    let top = (visible_destination_rect.y as f64 - top_inset * scale_y).floor();
    let right = (rect_right(visible_destination_rect) as f64 + right_inset * scale_x).ceil();
    let bottom = (rect_bottom(visible_destination_rect) as f64 + bottom_inset * scale_y).ceil();

    Some(Rect::new(
        left.clamp(i32::MIN as f64, i32::MAX as f64) as i32,
        top.clamp(i32::MIN as f64, i32::MAX as f64) as i32,
        (right - left).max(1.0).min(u32::MAX as f64) as u32,
        (bottom - top).max(1.0).min(u32::MAX as f64) as u32,
    ))
}

fn thumbnail_projection(
    window_rect: Rect,
    clipped_rect: Rect,
    visible_canvas_rect: Rect,
    source_rect: Rect,
) -> Option<ThumbnailProjection> {
    let destination_rect = rect_relative_to(clipped_rect, visible_canvas_rect);
    if window_rect.width == 0 || window_rect.height == 0 {
        return None;
    }

    let clip_left = clipped_rect.x.saturating_sub(window_rect.x).max(0) as i64;
    let clip_top = clipped_rect.y.saturating_sub(window_rect.y).max(0) as i64;
    let clip_right = rect_right(window_rect)
        .saturating_sub(rect_right(clipped_rect))
        .max(0) as i64;
    let clip_bottom = rect_bottom(window_rect)
        .saturating_sub(rect_bottom(clipped_rect))
        .max(0) as i64;
    let source_width = i64::from(source_rect.width.max(1));
    let source_height = i64::from(source_rect.height.max(1));
    let window_width = i64::from(window_rect.width.max(1));
    let window_height = i64::from(window_rect.height.max(1));
    let source_origin_x = i64::from(source_rect.x.max(0));
    let source_origin_y = i64::from(source_rect.y.max(0));
    let source_left = source_origin_x
        + ((clip_left * source_width) / window_width).clamp(0, source_width.saturating_sub(1));
    let source_top = source_origin_y
        + ((clip_top * source_height) / window_height).clamp(0, source_height.saturating_sub(1));
    let source_right = source_origin_x
        + (source_width - (clip_right * source_width) / window_width)
            .clamp(source_left - source_origin_x + 1, source_width);
    let source_bottom = source_origin_y
        + (source_height - (clip_bottom * source_height) / window_height)
            .clamp(source_top - source_origin_y + 1, source_height);

    Some(ThumbnailProjection {
        destination_rect,
        source_rect: Rect::new(
            source_left.clamp(0, i64::from(i32::MAX)) as i32,
            source_top.clamp(0, i64::from(i32::MAX)) as i32,
            (source_right - source_left).clamp(1, i64::from(u32::MAX)) as u32,
            (source_bottom - source_top).clamp(1, i64::from(u32::MAX)) as u32,
        ),
    })
}

fn update_thumbnail(thumbnail: isize, projection: ThumbnailProjection) -> Result<(), String> {
    let destination = rect_to_win32(projection.destination_rect, "thumbnail destination")?;
    let source = rect_to_win32(projection.source_rect, "thumbnail source")?;
    let client_only = overview_thumbnail_client_only_enabled();
    let properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_RECTSOURCE
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: destination,
        rcSource: source,
        opacity: u8::MAX,
        fVisible: 1,
        fSourceClientAreaOnly: i32::from(client_only),
    };
    let result = {
        // SAFETY: updates a thumbnail that was successfully registered on this thread.
        unsafe { DwmUpdateThumbnailProperties(thumbnail, &properties) }
    };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }
    Ok(())
}

fn hide_thumbnail(thumbnail: isize) -> Result<(), String> {
    let properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_VISIBLE,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcSource: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        opacity: u8::MAX,
        fVisible: 0,
        fSourceClientAreaOnly: 1,
    };
    let result = {
        // SAFETY: hides a live DWM thumbnail while keeping the registration alive for reuse.
        unsafe { DwmUpdateThumbnailProperties(thumbnail, &properties) }
    };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }
    Ok(())
}

fn destroy_workspace_surface(surface: WorkspacePreviewSurface) -> Result<(), String> {
    let mut first_error = None;
    for thumbnail in surface.thumbnails.into_values() {
        if let Err(error) = unregister_thumbnail(thumbnail.handle)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    let _ = {
        // SAFETY: paired with successful window creation on this thread.
        unsafe { DestroyWindow(surface.hwnd) }
    };
    first_error.map_or(Ok(()), Err)
}

fn unregister_thumbnail(thumbnail: isize) -> Result<(), String> {
    let result = {
        // SAFETY: unregisters a thumbnail that was previously created by this worker.
        unsafe { DwmUnregisterThumbnail(thumbnail) }
    };
    if result < 0 {
        return Err(format!(
            "DwmUnregisterThumbnail failed with HRESULT {result:#x}"
        ));
    }
    Ok(())
}

