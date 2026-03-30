fn create_preview_window(instance: HINSTANCE) -> Result<HWND, String> {
    let class_name = widestring(PREVIEW_CLASS);
    let window = {
        // SAFETY: creates a no-activate top-level preview surface used as a DWM thumbnail host.
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
    initialize_preview_window_state(window)?;
    Ok(window)
}

fn render_preview_scene_frame(
    surface: &mut WorkspacePreviewSurface,
    frame: &OverviewRenderFrame,
    insert_after: Option<HWND>,
) -> Result<(), String> {
    let visible_scene_rect = frame.monitor_rect;
    let was_host_visible = surface.placement.visible;

    if !was_host_visible {
        update_preview_host_region(surface, &[])?;
        position_window_with_order(
            surface.hwnd,
            visible_scene_rect,
            true,
            true,
            insert_after,
            &mut surface.placement,
        )?;
        reset_preview_thumbnail_cache(surface);
    }

    let (visible_hwnds, host_rects) = sync_scene_thumbnails(surface, frame, visible_scene_rect);
    let click_targets = preview_click_targets_for_frame(frame, visible_scene_rect, &visible_hwnds);
    let workspace_targets = workspace_drop_targets_for_frame(frame, visible_scene_rect);

    if host_rects.is_empty() {
        update_preview_window_state(surface.hwnd, Vec::new(), Vec::new());
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.host_region_rects.clear();
        return Ok(());
    }

    update_preview_window_state(surface.hwnd, click_targets, workspace_targets);
    update_preview_host_region(surface, &host_rects)?;

    position_window_with_order(
        surface.hwnd,
        visible_scene_rect,
        true,
        true,
        insert_after,
        &mut surface.placement,
    )?;
    Ok(())
}


fn sync_scene_thumbnails(
    surface: &mut WorkspacePreviewSurface,
    frame: &OverviewRenderFrame,
    visible_scene_rect: Rect,
) -> (HashSet<u64>, Vec<Rect>) {
    let desired_hwnds = frame
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.windows.iter().map(|window| window.hwnd))
        .collect::<Vec<_>>();
    let stale_hwnds = surface
        .thumbnails
        .keys()
        .copied()
        .filter(|hwnd| !desired_hwnds.contains(hwnd))
        .collect::<Vec<_>>();
    for hwnd in stale_hwnds {
        if let Some(thumbnail) = surface.thumbnails.remove(&hwnd) {
            let _ = unregister_thumbnail(thumbnail.handle);
        }
        surface.last_thumbnail_failures.remove(&hwnd);
        surface.last_thumbnail_diagnostics.remove(&hwnd);
    }

    let mut visible_hwnds = HashSet::new();
    let mut host_rects = Vec::new();
    for workspace in ordered_frame_workspaces(frame) {
        let Some(visible_canvas_rect) = intersect_rect(workspace.canvas_rect, visible_scene_rect)
        else {
            continue;
        };

        for window in &workspace.windows {
            let Some(source) = hwnd_from_raw(window.hwnd) else {
                continue;
            };
            let valid = {
                // SAFETY: `IsWindow` is a read-only validity check for a HWND reconstructed from state.
                unsafe { IsWindow(source) != 0 }
            };
            if !valid {
                if let Some(thumbnail) = surface.thumbnails.remove(&window.hwnd) {
                    let _ = unregister_thumbnail(thumbnail.handle);
                }
                surface.last_thumbnail_failures.remove(&window.hwnd);
                surface.last_thumbnail_diagnostics.remove(&window.hwnd);
                continue;
            }

            let full_window_geometry = full_window_preview_geometry_for_preview(source, window.rect);
            let destination_window_rect = window.rect;
            let Some(clipped_rect) = intersect_rect(destination_window_rect, visible_canvas_rect)
            else {
                if let Some(thumbnail) = surface.thumbnails.get_mut(&window.hwnd) {
                    if thumbnail.visible {
                        if hide_thumbnail(thumbnail.handle).is_ok() {
                            thumbnail.visible = false;
                            thumbnail.visible_projection = None;
                        } else if let Some(stale_thumbnail) =
                            surface.thumbnails.remove(&window.hwnd)
                        {
                            log_thumbnail_failure(
                                surface,
                                window.hwnd,
                                "hide",
                                "DwmUpdateThumbnailProperties failed while hiding preview",
                            );
                            let _ = unregister_thumbnail(stale_thumbnail.handle);
                        }
                    }
                }
                continue;
            };

            let (thumbnail_handle, thumbnail_was_visible, thumbnail_visible_projection) = {
                let thumbnail = if let Some(thumbnail) = surface.thumbnails.get_mut(&window.hwnd) {
                    thumbnail
                } else {
                    match register_thumbnail(surface.hwnd, source) {
                        Ok(handle) => {
                            surface
                                .thumbnails
                                .entry(window.hwnd)
                                .or_insert(PreviewThumbnailState {
                                    handle,
                                    visible_projection: None,
                                    visible: false,
                                })
                        }
                        Err(error) => {
                            log_thumbnail_failure(surface, window.hwnd, "register", &error);
                            continue;
                        }
                    }
                };
                (
                    thumbnail.handle,
                    thumbnail.visible,
                    thumbnail.visible_projection,
                )
            };

            let source_size = match thumbnail_source_size(thumbnail_handle) {
                Ok(size) => size,
                Err(error) => {
                    log_thumbnail_failure(surface, window.hwnd, "source-size", &error);
                    continue;
                }
            };
            let Some(projection) =
                thumbnail_projection(
                    destination_window_rect,
                    clipped_rect,
                    visible_scene_rect,
                    source_size,
                )
            else {
                continue;
            };
            log_thumbnail_geometry_diagnostic(
                surface,
                window.hwnd,
                window.rect,
                full_window_geometry,
                clipped_rect,
                source_size,
                projection,
            );

            if thumbnail_was_visible && thumbnail_visible_projection == Some(projection) {
                clear_thumbnail_failure(surface, window.hwnd);
                visible_hwnds.insert(window.hwnd);
                host_rects.push(projection.destination_rect);
                continue;
            }

            if update_thumbnail(thumbnail_handle, projection).is_ok() {
                if let Some(thumbnail) = surface.thumbnails.get_mut(&window.hwnd) {
                    thumbnail.visible = true;
                    thumbnail.visible_projection = Some(projection);
                }
                clear_thumbnail_failure(surface, window.hwnd);
                visible_hwnds.insert(window.hwnd);
                host_rects.push(projection.destination_rect);
            } else if let Some(stale_thumbnail) = surface.thumbnails.remove(&window.hwnd) {
                log_thumbnail_failure(
                    surface,
                    window.hwnd,
                    "update",
                    "DwmUpdateThumbnailProperties failed while showing preview",
                );
                let _ = unregister_thumbnail(stale_thumbnail.handle);
            }
        }
    }

    (visible_hwnds, host_rects)
}

fn log_thumbnail_failure(
    surface: &mut WorkspacePreviewSurface,
    hwnd: u64,
    stage: &str,
    error: &str,
) {
    let message = format!("stage={stage} hwnd={hwnd} error={error}");
    if surface.last_thumbnail_failures.get(&hwnd) == Some(&message) {
        return;
    }

    write_runtime_log(format!("overview-surface: thumbnail-failure {message}"));
    surface.last_thumbnail_failures.insert(hwnd, message);
}

fn clear_thumbnail_failure(surface: &mut WorkspacePreviewSurface, hwnd: u64) {
    surface.last_thumbnail_failures.remove(&hwnd);
}

fn reset_preview_thumbnail_cache(surface: &mut WorkspacePreviewSurface) {
    for thumbnail in surface.thumbnails.values_mut() {
        thumbnail.visible = false;
        thumbnail.visible_projection = None;
    }
    surface.last_thumbnail_diagnostics.clear();
}

fn log_thumbnail_geometry_diagnostic(
    surface: &mut WorkspacePreviewSurface,
    hwnd: u64,
    scene_rect: Rect,
    full_window_geometry: Option<FullWindowPreviewGeometry>,
    clipped_rect: Rect,
    source_size: Rect,
    projection: ThumbnailProjection,
) {
    if !overview_thumbnail_diagnostics_enabled() {
        return;
    }

    let message = match full_window_geometry {
        Some(geometry) => {
            let left_inset = geometry.visible_rect.x.saturating_sub(geometry.outer_rect.x).max(0);
            let top_inset = geometry.visible_rect.y.saturating_sub(geometry.outer_rect.y).max(0);
            let right_inset = rect_right(geometry.outer_rect)
                .saturating_sub(rect_right(geometry.visible_rect))
                .max(0);
            let bottom_inset = rect_bottom(geometry.outer_rect)
                .saturating_sub(rect_bottom(geometry.visible_rect))
                .max(0);
            format!(
                "scene={} outer={} visible={} expanded={} clipped={} source_size={} projection_dest={} projection_source={} client_only={} insets=({}, {}, {}, {})",
                format_rect(scene_rect),
                format_rect(geometry.outer_rect),
                format_rect(geometry.visible_rect),
                format_rect(geometry.destination_rect),
                format_rect(clipped_rect),
                format_rect(source_size),
                format_rect(projection.destination_rect),
                format_rect(projection.source_rect),
                overview_thumbnail_client_only_enabled(),
                left_inset,
                top_inset,
                right_inset,
                bottom_inset,
            )
        }
        None => format!(
            "scene={} outer=none visible=none expanded=fallback clipped={} source_size={} projection_dest={} projection_source={} client_only={}",
            format_rect(scene_rect),
            format_rect(clipped_rect),
            format_rect(source_size),
            format_rect(projection.destination_rect),
            format_rect(projection.source_rect),
            overview_thumbnail_client_only_enabled(),
        ),
    };

    if surface.last_thumbnail_diagnostics.get(&hwnd) == Some(&message) {
        return;
    }

    write_runtime_log(format!(
        "overview-surface: thumbnail-geometry hwnd={} {}",
        hwnd, message
    ));
    surface.last_thumbnail_diagnostics.insert(hwnd, message);
}

fn format_rect(rect: Rect) -> String {
    format!("({},{} {}x{})", rect.x, rect.y, rect.width, rect.height)
}

fn spring_progress(elapsed: Duration) -> f64 {
    let time = elapsed.as_secs_f64();
    let omega = SPRING_STIFFNESS.sqrt();
    let progress = 1.0 - (1.0 + omega * time) * (-omega * time).exp();
    progress.clamp(0.0, 1.0)
}

fn initialize_preview_window_state(hwnd: HWND) -> Result<(), String> {
    let state = Box::new(PreviewWindowState {
        click_targets: Vec::new(),
        workspace_targets: Vec::new(),
        drag_session: None,
    });
    let raw_state = Box::into_raw(state);
    let previous = {
        // SAFETY: stores process-local preview state pointer for this HWND.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_state as isize) }
    };
    if previous != 0 {
        let _ = {
            // SAFETY: ownership returns to Rust if the user-data slot was unexpectedly occupied.
            unsafe { Box::from_raw(raw_state) }
        };
        return Err("overview preview user data was already initialized".to_string());
    }
    Ok(())
}


fn update_preview_window_state(
    hwnd: HWND,
    click_targets: Vec<PreviewClickTarget>,
    workspace_targets: Vec<WorkspaceDropTarget>,
) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let state_changed = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe {
            (*state_ptr).click_targets != click_targets
                || (*state_ptr).workspace_targets != workspace_targets
        }
    };
    if !state_changed {
        return;
    }

    {
        // SAFETY: mutates the owned preview state for this HWND in place.
        unsafe {
            (*state_ptr).click_targets = click_targets;
            (*state_ptr).workspace_targets = workspace_targets;
        }
    }
    let _ = {
        // SAFETY: requests a repaint after preview content changes while the HWND stays visible.
        unsafe { InvalidateRect(hwnd, null(), 1) }
    };
}

fn update_preview_window_region(
    window: HWND,
    cached_rects: &mut Vec<Rect>,
    rects: &[Rect],
) -> Result<(), String> {
    if cached_rects.as_slice() == rects {
        return Ok(());
    }

    let region = build_preview_window_region(rects)?;
    let applied = {
        // SAFETY: transfers ownership of `region` to the live preview host window on success.
        unsafe { SetWindowRgn(window, region, 1) }
    };
    if applied == 0 {
        let _ = {
            // SAFETY: cleanup is required only on failure because ownership was not transferred.
            unsafe { DeleteObject(region as HGDIOBJ) }
        };
        return Err(last_error_message("SetWindowRgn"));
    }

    *cached_rects = rects.to_vec();
    Ok(())
}

fn update_preview_host_region(
    surface: &mut WorkspacePreviewSurface,
    rects: &[Rect],
) -> Result<(), String> {
    update_preview_window_region(surface.hwnd, &mut surface.host_region_rects, rects)
}

fn build_preview_window_region(rects: &[Rect]) -> Result<HRGN, String> {
    let Some(first_rect) = rects.first().copied() else {
        let region = {
            // SAFETY: creates a minimal empty region for a host without visible thumbnails.
            unsafe { CreateRectRgn(0, 0, 0, 0) }
        };
        if region.is_null() {
            return Err(last_error_message("CreateRectRgn"));
        }
        return Ok(region);
    };

    let region = build_rect_region(first_rect)?;
    for rect in rects.iter().copied().skip(1) {
        let next = build_rect_region(rect)?;
        let combined = {
            // SAFETY: combines two owned regions into the destination region.
            unsafe { CombineRgn(region, region, next, RGN_OR) }
        };
        let _ = {
            // SAFETY: `next` is no longer needed after the combine attempt.
            unsafe { DeleteObject(next as HGDIOBJ) }
        };
        if combined == 0 {
            let _ = {
                // SAFETY: cleanup of the destination region on failure.
                unsafe { DeleteObject(region as HGDIOBJ) }
            };
            return Err(last_error_message("CombineRgn"));
        }
    }

    Ok(region)
}

fn build_rect_region(rect: Rect) -> Result<HRGN, String> {
    let region = {
        // SAFETY: creates a rectangular region matching one visible thumbnail rect.
        unsafe { CreateRectRgn(rect.x, rect.y, rect_right(rect), rect_bottom(rect)) }
    };
    if region.is_null() {
        return Err(last_error_message("CreateRectRgn"));
    }
    Ok(region)
}

fn preview_window_state_ptr(hwnd: HWND) -> *mut PreviewWindowState {
    // SAFETY: reads the preview state pointer previously stored in `GWLP_USERDATA`.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewWindowState }
}


fn preview_click_target(hwnd: HWND, x: i32, y: i32) -> Option<u64> {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return None;
    }

    let click_targets = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { &(*state_ptr).click_targets }
    };
    hit_test_preview_targets(click_targets, x, y)
}

fn preview_target_at_point_for_window(hwnd: HWND, x: i32, y: i32) -> Option<PreviewClickTarget> {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return None;
    }

    let click_targets = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { &(*state_ptr).click_targets }
    };
    preview_target_at_point(click_targets, x, y)
}

fn preview_workspace_target(hwnd: HWND, x: i32, y: i32) -> Option<WorkspaceId> {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return None;
    }

    let workspace_targets = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { &(*state_ptr).workspace_targets }
    };
    hit_test_workspace_drop_targets(workspace_targets, x, y)
}

fn begin_preview_drag_session(hwnd: HWND, x: i32, y: i32, dragged_raw_hwnd: u64) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    {
        // SAFETY: mutates the owned preview state for this HWND in place.
        unsafe {
            (*state_ptr).drag_session = Some(PreviewDragSession {
                dragged_raw_hwnd,
                origin_x: x,
                origin_y: y,
                moved: false,
            });
        }
    }
    let _ = {
        // SAFETY: captures mouse input for this top-level preview host until button release.
        unsafe { SetCapture(hwnd) }
    };
}

fn update_preview_drag_session(hwnd: HWND, x: i32, y: i32) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let threshold_reached = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe {
            (*state_ptr).drag_session.as_ref().is_some_and(|session| {
                (x.saturating_sub(session.origin_x)).abs() >= OVERVIEW_DRAG_START_THRESHOLD_PX
                    || (y.saturating_sub(session.origin_y)).abs()
                        >= OVERVIEW_DRAG_START_THRESHOLD_PX
            })
        }
    };
    if !threshold_reached {
        return;
    }

    {
        // SAFETY: mutates the owned preview state for this HWND in place.
        unsafe {
            if let Some(session) = (*state_ptr).drag_session.as_mut() {
                session.moved = true;
            }
        }
    }
}

fn cancel_preview_drag_session(hwnd: HWND) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let should_release_capture = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe {
            let had_session = (*state_ptr).drag_session.is_some();
            (*state_ptr).drag_session = None;
            had_session
        }
    };
    if should_release_capture {
        let _ = {
            // SAFETY: releases mouse capture previously taken by this preview host.
            unsafe { ReleaseCapture() }
        };
    }
}

fn finish_preview_pointer_interaction(hwnd: HWND, x: i32, y: i32) -> PreviewPointerOutcome {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return PreviewPointerOutcome::None;
    }

    let session = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { (*state_ptr).drag_session.take() }
    };
    if session.is_some() {
        let _ = {
            // SAFETY: releases mouse capture previously taken by this preview host.
            unsafe { ReleaseCapture() }
        };
    }
    let Some(session) = session else {
        return preview_click_target(hwnd, x, y)
            .map(PreviewPointerOutcome::ActivateWindow)
            .unwrap_or(PreviewPointerOutcome::Dismiss);
    };

    if !session.moved {
        return PreviewPointerOutcome::ActivateWindow(session.dragged_raw_hwnd);
    }

    let Some(target_workspace_id) = preview_workspace_target(hwnd, x, y) else {
        return PreviewPointerOutcome::None;
    };
    let insert_after_raw_hwnd =
        preview_target_at_point_for_window(hwnd, x, y).map(|target| target.hwnd);
    PreviewPointerOutcome::MoveColumn {
        dragged_raw_hwnd: session.dragged_raw_hwnd,
        target_workspace_id,
        insert_after_raw_hwnd,
    }
}

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam as u32;
    let x = (packed & 0xFFFF) as i16 as i32;
    let y = ((packed >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn dispatch_overview_activate_window(raw_hwnd: u64) {
    let Some(control_sender) = OVERVIEW_CONTROL_SENDER.get() else {
        return;
    };
    let _ = control_sender.send(ControlMessage::OverviewActivateWindow { raw_hwnd });
}

fn dispatch_overview_move_column(
    dragged_raw_hwnd: u64,
    target_workspace_id: WorkspaceId,
    insert_after_raw_hwnd: Option<u64>,
) {
    let Some(control_sender) = OVERVIEW_CONTROL_SENDER.get() else {
        return;
    };
    let _ = control_sender.send(ControlMessage::OverviewMoveColumn {
        dragged_raw_hwnd,
        target_workspace_id,
        insert_after_raw_hwnd,
    });
}

fn dispatch_overview_dismiss() {
    let Some(control_sender) = OVERVIEW_CONTROL_SENDER.get() else {
        return;
    };
    let _ = control_sender.send(ControlMessage::OverviewDismiss);
}

