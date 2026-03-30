fn run_overlay_thread(
    command_receiver: Receiver<OverlayCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_overview_classes() {
        Ok(classes) => {
            match create_backdrop_window(classes.instance) {
                Ok(backdrop) => {
                    let _ = startup_sender.send(Ok(()));
                    let _ = run_overlay_loop(command_receiver, &classes, backdrop);
                    let _ = {
                        // SAFETY: paired with successful backdrop creation on this thread.
                        unsafe { DestroyWindow(backdrop) }
                    };
                }
                Err(error) => {
                    let _ = startup_sender.send(Err(error));
                }
            }
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}


fn run_overlay_loop(
    command_receiver: Receiver<OverlayCommand>,
    classes: &OverviewClasses,
    backdrop: HWND,
) -> Result<(), String> {
    let mut preview = None::<WorkspacePreviewSurface>;
    let mut session = OverviewSessionState::default();
    let mut backdrop_placement = OverlayWindowPlacement::default();

    loop {
        pump_messages()?;
        if session.should_restore_shell_escape() {
            if let Some(scene) = session.current_scene() {
                clear_backdrop_snapshot(backdrop);
                render_scene_frame(
                    backdrop,
                    &mut backdrop_placement,
                    &mut preview,
                    scene,
                    classes,
                    SceneFrameMode::Final,
                )?;
            }
            session.clear_shell_escape();
        }
        match command_receiver.recv_timeout(THREAD_SLICE) {
            Ok(OverlayCommand::Show(scene, response)) => {
                let result = if session.scene_matches(&scene) {
                    Ok(())
                } else if let Some(current) = session.current_scene() {
                    animate_scene_transition(
                        backdrop,
                        &mut backdrop_placement,
                        &mut preview,
                        current,
                        &scene,
                        classes,
                    )
                } else {
                    animate_scene_open(
                        backdrop,
                        &mut backdrop_placement,
                        &mut preview,
                        &scene,
                        classes,
                    )
                };
                let result = result.and_then(|_| {
                    if session.shell_escape_active() {
                        freeze_scene_for_shell_overlay(
                            backdrop,
                            &mut backdrop_placement,
                            &mut preview,
                            &scene,
                        )?;
                    }
                    Ok(())
                });
                if result.is_ok() {
                    session.record_scene(scene);
                }
                let _ = response.send(result);
            }
            Ok(OverlayCommand::Hide(response)) => {
                session.clear_shell_escape();
                let result = if let Some(scene) = session.take_scene() {
                    animate_scene_close(
                        backdrop,
                        &mut backdrop_placement,
                        &mut preview,
                        &scene,
                        classes,
                    )
                } else {
                    hide_scene(backdrop, &mut backdrop_placement, &mut preview)
                };
                let _ = response.send(result);
            }
            Ok(OverlayCommand::LowerForShellOverlay(response)) => {
                let result = if let Some(scene) = session.current_scene() {
                    let baseline_windows = shell_screenshot_windows();
                    let result = freeze_scene_for_shell_overlay(
                        backdrop,
                        &mut backdrop_placement,
                        &mut preview,
                        scene,
                    );
                    if result.is_ok() {
                        session.begin_shell_escape(Instant::now(), baseline_windows);
                    }
                    result
                } else {
                    Ok(())
                };
                let _ = response.send(result);
            }
            Ok(OverlayCommand::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = hide_scene(backdrop, &mut backdrop_placement, &mut preview);
    let _ = destroy_all_preview_surfaces(&mut preview);
    Ok(())
}

fn animate_scene_open(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
) -> Result<(), String> {
    animate_scene(backdrop, backdrop_placement, preview, scene, classes, true)
}

fn animate_scene_close(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
) -> Result<(), String> {
    animate_scene(backdrop, backdrop_placement, preview, scene, classes, false)?;
    hide_scene(backdrop, backdrop_placement, preview)
}

fn animate_scene_transition(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    from_scene: &OverviewScene,
    to_scene: &OverviewScene,
    classes: &OverviewClasses,
) -> Result<(), String> {
    let animation_start = Instant::now();
    loop {
        let elapsed = animation_start.elapsed();
        let progress = spring_progress(elapsed);
        let progress_milli = (progress * 1000.0).round().clamp(0.0, 1000.0) as u16;
        let frame = render_frame_for_transition(from_scene, to_scene, progress_milli);
        render_overview_frame(backdrop, backdrop_placement, preview, &frame, classes)?;
        pump_messages()?;

        if (1.0 - progress) <= SPRING_EPSILON || elapsed >= INTRA_OVERVIEW_ANIMATION_MAX_DURATION {
            break;
        }

        thread::sleep(THREAD_SLICE);
    }

    render_scene_frame(
        backdrop,
        backdrop_placement,
        preview,
        to_scene,
        classes,
        SceneFrameMode::Final,
    )
}

fn animate_scene(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
    opening: bool,
) -> Result<(), String> {
    let animation_start = Instant::now();
    loop {
        let progress = spring_progress(animation_start.elapsed());
        let progress_milli = (progress * 1000.0).round().clamp(0.0, 1000.0) as u16;
        let mode = if opening {
            SceneFrameMode::Opening { progress_milli }
        } else {
            SceneFrameMode::Closing { progress_milli }
        };
        render_scene_frame(backdrop, backdrop_placement, preview, scene, classes, mode)?;
        pump_messages()?;

        if (1.0 - progress) <= SPRING_EPSILON
            || animation_start.elapsed() >= OPEN_CLOSE_ANIMATION_MAX_DURATION
        {
            break;
        }

        thread::sleep(THREAD_SLICE);
    }

    let mode = if opening {
        SceneFrameMode::Opening {
            progress_milli: 1000,
        }
    } else {
        SceneFrameMode::Closing {
            progress_milli: 1000,
        }
    };
    render_scene_frame(backdrop, backdrop_placement, preview, scene, classes, mode)?;
    Ok(())
}

fn render_scene_frame(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
    mode: SceneFrameMode,
) -> Result<(), String> {
    let frame = render_frame_for_scene(scene, mode);
    render_overview_frame(backdrop, backdrop_placement, preview, &frame, classes)
}
fn render_overview_frame(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    frame: &OverviewRenderFrame,
    classes: &OverviewClasses,
) -> Result<(), String> {
    position_backdrop(backdrop, frame.monitor_rect, backdrop_placement)?;
    set_backdrop_viewport_column(
        backdrop,
        overview_viewport_column_rect(frame)
            .map(|column_rect| rect_relative_to(column_rect, frame.monitor_rect)),
    );

    if frame.workspaces.is_empty() {
        hide_scene(backdrop, backdrop_placement, preview)?;
        let _ = {
            // SAFETY: synchronizes DWM composition after the frame surfaces have been updated.
            unsafe { DwmFlush() }
        };
        return Ok(());
    }

    let surface = preview.get_or_insert_with(|| WorkspacePreviewSurface {
        hwnd: null_mut(),
        thumbnails: HashMap::new(),
        last_thumbnail_failures: HashMap::new(),
        last_thumbnail_diagnostics: HashMap::new(),
        placement: OverlayWindowPlacement::default(),
        host_region_rects: Vec::new(),
    });
    if surface.hwnd.is_null() {
        surface.hwnd = create_preview_window(classes.instance)?;
    }
    render_preview_scene_frame(surface, frame, Some(backdrop))?;

    let _ = {
        // SAFETY: synchronizes DWM composition after the frame surfaces have been updated.
        unsafe { DwmFlush() }
    };
    Ok(())
}

fn hide_scene(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
) -> Result<(), String> {
    clear_backdrop_snapshot(backdrop);
    clear_backdrop_viewport_column(backdrop);
    if let Some(surface) = preview.as_mut() {
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.host_region_rects.clear();
    }
    hide_overlay_window(backdrop, backdrop_placement);
    Ok(())
}

fn destroy_all_preview_surfaces(
    preview: &mut Option<WorkspacePreviewSurface>,
) -> Result<(), String> {
    if let Some(surface) = preview.take() {
        destroy_workspace_surface(surface)?;
    }
    Ok(())
}

fn freeze_scene_for_shell_overlay(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    preview: &mut Option<WorkspacePreviewSurface>,
    scene: &OverviewScene,
) -> Result<(), String> {
    let snapshot = capture_screen_bitmap(scene.monitor_rect)?;
    set_backdrop_snapshot(backdrop, snapshot);
    clear_backdrop_viewport_column(backdrop);
    position_window_with_order(
        backdrop,
        scene.monitor_rect,
        true,
        false,
        None,
        backdrop_placement,
    )?;
    if let Some(surface) = preview.as_mut() {
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.host_region_rects.clear();
    }
    let _ = {
        // SAFETY: synchronizes DWM composition after switching to the frozen backdrop.
        unsafe { DwmFlush() }
    };
    Ok(())
}

