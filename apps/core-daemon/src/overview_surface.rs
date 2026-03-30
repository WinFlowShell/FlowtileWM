use std::{
    collections::{HashMap, HashSet},
    mem::zeroed,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::control::ControlMessage;
use crate::diag::write_runtime_log;
use flowtile_domain::{Rect, RuntimeMode, WmState, WorkspaceId};
use flowtile_layout_engine::{LayoutError, recompute_workspace};
use flowtile_wm_core::CoreDaemonRuntime;

#[cfg(not(windows))]
compile_error!("flowtile-core-daemon overview surface currently supports only Windows.");

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{
        BOOL, CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM,
    },
    Graphics::{
        Dwm::{
            DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
            DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmFlush, DwmQueryThumbnailSourceSize,
            DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
        },
        Gdi::{
            BeginPaint, BitBlt, CombineRgn, CreateCompatibleBitmap, CreateCompatibleDC,
            CreateRectRgn, DC_BRUSH, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC,
            GetStockObject, HBITMAP, HBRUSH, HDC, HGDIOBJ, HRGN, InvalidateRect, PAINTSTRUCT,
            RGN_OR, ReleaseDC, SRCCOPY, SelectObject, SetDCBrushColor, SetWindowRgn,
        },
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
        GWLP_USERDATA, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowLongPtrW,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, HWND_NOTOPMOST,
        HWND_TOPMOST, IsWindow, IsWindowVisible, MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW,
        RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW,
        SetWindowPos, ShowWindow, TranslateMessage, WM_ERASEBKGND, WM_LBUTTONUP, WM_MBUTTONUP,
        WM_MOUSEACTIVATE, WM_NCDESTROY, WM_PAINT, WM_QUIT, WM_RBUTTONUP, WNDCLASSW,
        WS_CLIPCHILDREN, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};

const THREAD_SLICE: Duration = Duration::from_millis(16);
const OPEN_CLOSE_ANIMATION_MAX_DURATION: Duration = Duration::from_millis(320);
const INTRA_OVERVIEW_ANIMATION_MAX_DURATION: Duration = Duration::from_millis(320);
const SPRING_STIFFNESS: f64 = 800.0;
const SPRING_EPSILON: f64 = 0.0001;
const BACKDROP_CLASS: &str = "FlowtileOverviewBackdrop";
const PREVIEW_CLASS: &str = "FlowtileOverviewPreview";
const OVERVIEW_DEFAULT_ZOOM: f64 = 0.5;
const OVERVIEW_OPEN_CLOSE_SOURCE_ZOOM: f64 = 0.7;
const OVERVIEW_WORKSPACE_GAP_RATIO: f64 = 0.1;
const OVERVIEW_BACKDROP_COLOR: u32 = rgb_color(0x26, 0x26, 0x26);
const WORKSPACE_PREVIEW_BACKGROUND_COLOR: u32 = rgb_color(0x16, 0x16, 0x16);
const WORKSPACE_PREVIEW_BORDER_PX: i32 = 1;
const WORKSPACE_PREVIEW_WINDOW_FILL_COLOR: u32 = rgb_color(0x2B, 0x2B, 0x2B);
const WORKSPACE_PREVIEW_WINDOW_BORDER_COLOR: u32 = rgb_color(0x66, 0x66, 0x66);
const WORKSPACE_PREVIEW_WINDOW_HEADER_COLOR: u32 = rgb_color(0x39, 0x39, 0x39);
const WORKSPACE_PREVIEW_WINDOW_CONTENT_COLOR: u32 = rgb_color(0x4D, 0x4D, 0x4D);
const WORKSPACE_PREVIEW_WINDOW_CONTENT_SECONDARY_COLOR: u32 = rgb_color(0x3A, 0x3A, 0x3A);
const WORKSPACE_PREVIEW_WINDOW_CONTROL_COLOR: u32 = rgb_color(0x8A, 0x8A, 0x8A);
const SHELL_OVERLAY_PREPARE_TIMEOUT: Duration = Duration::from_millis(180);
const SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const SHELL_OVERLAY_RESTORE_SETTLE: Duration = Duration::from_millis(80);
const SCREEN_CLIPPING_HOST_PROCESS: &str = "screenclippinghost";
const SNIPPING_TOOL_PROCESS: &str = "snippingtool";

static OVERVIEW_CONTROL_SENDER: OnceLock<Sender<ControlMessage>> = OnceLock::new();
static OVERVIEW_VISUAL_SENDER: OnceLock<Sender<OverlayCommand>> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum OverviewSurfaceError {
    Layout(LayoutError),
    Platform(String),
}

impl std::fmt::Display for OverviewSurfaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(source) => write!(formatter, "{source:?}"),
            Self::Platform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OverviewSurfaceError {}

impl From<LayoutError> for OverviewSurfaceError {
    fn from(value: LayoutError) -> Self {
        Self::Layout(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverviewSyncKey {
    state_version: u64,
    management_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverviewScene {
    monitor_rect: Rect,
    workspaces: Vec<WorkspacePreviewScene>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspacePreviewScene {
    workspace_id: WorkspaceId,
    live_rect: Rect,
    canvas_rect: Rect,
    frame_rect: Rect,
    selected: bool,
    windows: Vec<WindowPreviewScene>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowPreviewScene {
    hwnd: u64,
    live_rect: Rect,
    overview_rect: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverviewRenderFrame {
    monitor_rect: Rect,
    workspaces: Vec<WorkspaceRenderFrame>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceRenderFrame {
    workspace_id: WorkspaceId,
    canvas_rect: Rect,
    viewport_rect: Rect,
    windows: Vec<WindowRenderFrame>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowRenderFrame {
    hwnd: u64,
    rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviewStackLayout {
    frame_width: u32,
    frame_height: u32,
    gap: i32,
    active_frame_y: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OverlayWindowPlacement {
    rect: Option<Rect>,
    visible: bool,
    topmost: bool,
    child_insert_after: Option<isize>,
}

pub(crate) struct OverviewSurfaceController {
    overlay: OverviewOverlay,
    last_key: Option<OverviewSyncKey>,
}

impl OverviewSurfaceController {
    pub(crate) fn spawn(
        control_sender: Sender<ControlMessage>,
    ) -> Result<Self, OverviewSurfaceError> {
        install_overview_control_sender(control_sender);
        Ok(Self {
            overlay: OverviewOverlay::spawn()?,
            last_key: None,
        })
    }

    pub(crate) fn sync(&mut self, runtime: &CoreDaemonRuntime) -> Result<(), OverviewSurfaceError> {
        let key = OverviewSyncKey {
            state_version: runtime.state().state_version().get(),
            management_enabled: runtime.management_enabled(),
        };
        if self.last_key == Some(key) {
            return Ok(());
        }

        if !runtime.management_enabled()
            || runtime.state().runtime.boot_mode == RuntimeMode::SafeMode
        {
            self.overlay.hide()?;
            self.last_key = Some(key);
            return Ok(());
        }

        match build_overview_scene(runtime.state())? {
            Some(scene) => self.overlay.show(scene)?,
            None => self.overlay.hide()?,
        }
        self.last_key = Some(key);
        Ok(())
    }
}

fn install_overview_control_sender(control_sender: Sender<ControlMessage>) {
    let _ = OVERVIEW_CONTROL_SENDER.set(control_sender);
}

fn install_overview_visual_sender(visual_sender: Sender<OverlayCommand>) {
    let _ = OVERVIEW_VISUAL_SENDER.set(visual_sender);
}

pub(crate) fn lower_overview_surface_for_shell_overlay() {
    let Some(visual_sender) = OVERVIEW_VISUAL_SENDER.get() else {
        return;
    };
    let (response_sender, response_receiver) = mpsc::channel();
    if visual_sender
        .send(OverlayCommand::LowerForShellOverlay(response_sender))
        .is_err()
    {
        return;
    }
    let _ = response_receiver.recv_timeout(SHELL_OVERLAY_PREPARE_TIMEOUT);
}

fn build_overview_scene(state: &WmState) -> Result<Option<OverviewScene>, LayoutError> {
    if !state.overview.is_open {
        return Ok(None);
    }

    let Some(monitor_id) = state.overview.monitor_id else {
        return Ok(None);
    };
    let Some(monitor) = state.monitors.get(&monitor_id) else {
        return Ok(None);
    };
    let Some(workspace_set_id) = state.workspace_set_id_for_monitor(monitor_id) else {
        return Ok(None);
    };
    let Some(workspace_set) = state.workspace_sets.get(&workspace_set_id) else {
        return Ok(None);
    };
    if workspace_set.ordered_workspace_ids.is_empty() {
        return Ok(None);
    }

    let selection_workspace_id = state
        .overview
        .selection
        .or_else(|| state.active_workspace_id_for_monitor(monitor_id));
    let active_workspace_id = state.active_workspace_id_for_monitor(monitor_id);
    let active_workspace_index = active_workspace_id
        .and_then(|workspace_id| {
            workspace_set
                .ordered_workspace_ids
                .iter()
                .position(|candidate| *candidate == workspace_id)
        })
        .unwrap_or(0) as i32;
    let layout = build_preview_stack_layout(monitor.work_area_rect);
    let mut workspaces = Vec::with_capacity(workspace_set.ordered_workspace_ids.len());
    for (index, workspace_id) in workspace_set
        .ordered_workspace_ids
        .iter()
        .copied()
        .enumerate()
    {
        let projection = recompute_workspace(state, workspace_id)?;
        let stack_offset = index as i32 - active_workspace_index;
        let frame_rect = workspace_preview_frame_rect(monitor.work_area_rect, layout, stack_offset);
        if !rects_intersect(frame_rect, monitor.work_area_rect) {
            continue;
        }

        let live_rect = workspace_live_rect(monitor.work_area_rect, stack_offset);
        let mut windows = projection
            .window_geometries
            .iter()
            .filter_map(|geometry| {
                let window = state.windows.get(&geometry.window_id)?;
                let hwnd = window.current_hwnd_binding?;
                let overview_rect =
                    scale_rect_to_overview(geometry.rect, monitor.work_area_rect, frame_rect);
                Some(WindowPreviewScene {
                    hwnd,
                    live_rect: translate_rect(
                        geometry.rect,
                        live_rect.x.saturating_sub(monitor.work_area_rect.x),
                        live_rect.y.saturating_sub(monitor.work_area_rect.y),
                    ),
                    overview_rect,
                })
            })
            .collect::<Vec<_>>();
        let canvas_rect = workspace_preview_canvas_rect(frame_rect, monitor.work_area_rect);
        windows.retain(|window| rects_intersect(window.overview_rect, canvas_rect));

        let selected = selection_workspace_id == Some(workspace_id);
        workspaces.push(WorkspacePreviewScene {
            workspace_id,
            live_rect,
            canvas_rect,
            frame_rect,
            selected,
            windows,
        });
    }

    Ok(Some(OverviewScene {
        monitor_rect: monitor.work_area_rect,
        workspaces,
    }))
}

fn workspace_live_rect(monitor_rect: Rect, stack_offset: i32) -> Rect {
    let monitor_height = monitor_rect.height.min(i32::MAX as u32) as i32;
    Rect::new(
        monitor_rect.x,
        monitor_rect
            .y
            .saturating_add(stack_offset.saturating_mul(monitor_height)),
        monitor_rect.width,
        monitor_rect.height,
    )
}

fn build_preview_stack_layout(monitor_rect: Rect) -> PreviewStackLayout {
    build_preview_stack_layout_with_zoom(monitor_rect, OVERVIEW_DEFAULT_ZOOM)
}

fn build_preview_stack_layout_with_zoom(monitor_rect: Rect, zoom: f64) -> PreviewStackLayout {
    let zoom = zoom.clamp(OVERVIEW_DEFAULT_ZOOM, 1.0);
    let frame_height = ((monitor_rect.height.max(1) as f64) * zoom)
        .round()
        .max(1.0) as u32;
    let frame_width = ((monitor_rect.width.max(1) as f64) * zoom).round().max(1.0) as u32;
    let gap = ((monitor_rect.height.max(1) as f64) * OVERVIEW_WORKSPACE_GAP_RATIO * zoom)
        .round()
        .max(1.0) as i32;
    let monitor_height = monitor_rect.height.min(i32::MAX as u32) as i32;

    PreviewStackLayout {
        frame_width,
        frame_height,
        gap,
        active_frame_y: monitor_rect
            .y
            .saturating_add((monitor_height - frame_height.min(i32::MAX as u32) as i32) / 2),
    }
}

fn workspace_preview_frame_rect(
    monitor_rect: Rect,
    layout: PreviewStackLayout,
    stack_offset: i32,
) -> Rect {
    let step = layout.frame_height.min(i32::MAX as u32) as i32 + layout.gap;
    Rect::new(
        monitor_rect.x.saturating_add(
            (monitor_rect.width.min(i32::MAX as u32) as i32
                - layout.frame_width.min(i32::MAX as u32) as i32)
                / 2,
        ),
        layout
            .active_frame_y
            .saturating_add(stack_offset.saturating_mul(step)),
        layout.frame_width,
        layout.frame_height,
    )
}

fn workspace_stack_offset(workspace: &WorkspacePreviewScene, monitor_rect: Rect) -> i32 {
    let monitor_height = monitor_rect.height.max(1).min(i32::MAX as u32) as i32;
    workspace
        .live_rect
        .y
        .saturating_sub(monitor_rect.y)
        .div_euclid(monitor_height)
}

fn workspace_open_close_source_rect(workspace: &WorkspacePreviewScene, monitor_rect: Rect) -> Rect {
    let layout =
        build_preview_stack_layout_with_zoom(monitor_rect, OVERVIEW_OPEN_CLOSE_SOURCE_ZOOM);
    workspace_preview_frame_rect(
        monitor_rect,
        layout,
        workspace_stack_offset(workspace, monitor_rect),
    )
}

fn scale_rect_between_rects(
    source_rect: Rect,
    source_container: Rect,
    target_container: Rect,
) -> Rect {
    let scale_x = target_container.width.max(1) as f64 / source_container.width.max(1) as f64;
    let scale_y = target_container.height.max(1) as f64 / source_container.height.max(1) as f64;
    let relative_x = source_rect.x.saturating_sub(source_container.x);
    let relative_y = source_rect.y.saturating_sub(source_container.y);

    Rect::new(
        target_container
            .x
            .saturating_add((relative_x as f64 * scale_x).round() as i32),
        target_container
            .y
            .saturating_add((relative_y as f64 * scale_y).round() as i32),
        ((source_rect.width.max(1) as f64) * scale_x)
            .round()
            .max(1.0) as u32,
        ((source_rect.height.max(1) as f64) * scale_y)
            .round()
            .max(1.0) as u32,
    )
}

fn scale_rect_to_overview(source_rect: Rect, monitor_rect: Rect, target_rect: Rect) -> Rect {
    let scale_x = target_rect.width.max(1) as f64 / monitor_rect.width.max(1) as f64;
    let scale_y = target_rect.height.max(1) as f64 / monitor_rect.height.max(1) as f64;
    let relative_x = source_rect.x.saturating_sub(monitor_rect.x);
    let relative_y = source_rect.y.saturating_sub(monitor_rect.y);

    Rect::new(
        target_rect
            .x
            .saturating_add((relative_x as f64 * scale_x).round() as i32),
        target_rect
            .y
            .saturating_add((relative_y as f64 * scale_y).round() as i32),
        ((source_rect.width.max(1) as f64) * scale_x)
            .round()
            .max(1.0) as u32,
        ((source_rect.height.max(1) as f64) * scale_y)
            .round()
            .max(1.0) as u32,
    )
}

fn workspace_preview_canvas_rect(frame_rect: Rect, monitor_rect: Rect) -> Rect {
    Rect::new(
        monitor_rect.x,
        frame_rect.y,
        monitor_rect.width,
        frame_rect.height,
    )
}

fn rects_intersect(left: Rect, right: Rect) -> bool {
    rect_right(left) > right.x
        && rect_right(right) > left.x
        && rect_bottom(left) > right.y
        && rect_bottom(right) > left.y
}

fn intersect_rect(left: Rect, right: Rect) -> Option<Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = rect_right(left).min(rect_right(right));
    let bottom_edge = rect_bottom(left).min(rect_bottom(right));
    if right_edge <= x || bottom_edge <= y {
        return None;
    }

    Some(Rect::new(
        x,
        y,
        (right_edge - x) as u32,
        (bottom_edge - y) as u32,
    ))
}

fn rect_relative_to(rect: Rect, origin: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_sub(origin.x),
        rect.y.saturating_sub(origin.y),
        rect.width,
        rect.height,
    )
}

fn rect_right(rect: Rect) -> i32 {
    rect.x
        .saturating_add(rect.width.min(i32::MAX as u32) as i32)
}

fn rect_bottom(rect: Rect) -> i32 {
    rect.y
        .saturating_add(rect.height.min(i32::MAX as u32) as i32)
}

enum OverlayCommand {
    Show(OverviewScene, Sender<Result<(), String>>),
    Hide(Sender<Result<(), String>>),
    LowerForShellOverlay(Sender<Result<(), String>>),
    Shutdown,
}

struct OverviewOverlay {
    sender: Sender<OverlayCommand>,
    worker: Option<JoinHandle<()>>,
}

impl OverviewOverlay {
    fn spawn() -> Result<Self, OverviewSurfaceError> {
        let (command_sender, command_receiver) = mpsc::channel::<OverlayCommand>();
        install_overview_visual_sender(command_sender.clone());
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        let worker = thread::spawn(move || run_overlay_thread(command_receiver, startup_sender));
        startup_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| {
                OverviewSurfaceError::Platform(format!(
                    "overview surface startup timed out: {error}"
                ))
            })?
            .map_err(OverviewSurfaceError::Platform)?;

        Ok(Self {
            sender: command_sender,
            worker: Some(worker),
        })
    }

    fn show(&self, scene: OverviewScene) -> Result<(), OverviewSurfaceError> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(OverlayCommand::Show(scene, response_sender))
            .map_err(|_| {
                OverviewSurfaceError::Platform(
                    "overview surface worker is no longer available".to_string(),
                )
            })?;
        response_receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| {
                OverviewSurfaceError::Platform(format!(
                    "overview surface response timed out: {error}"
                ))
            })?
            .map_err(OverviewSurfaceError::Platform)
    }

    fn hide(&self) -> Result<(), OverviewSurfaceError> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(OverlayCommand::Hide(response_sender))
            .map_err(|_| {
                OverviewSurfaceError::Platform(
                    "overview surface worker is no longer available".to_string(),
                )
            })?;
        response_receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| {
                OverviewSurfaceError::Platform(format!(
                    "overview surface response timed out: {error}"
                ))
            })?
            .map_err(OverviewSurfaceError::Platform)
    }
}

impl Drop for OverviewOverlay {
    fn drop(&mut self) {
        let _ = self.sender.send(OverlayCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct OverviewClasses {
    instance: HINSTANCE,
}

struct WorkspacePreviewSurface {
    frame_hwnd: HWND,
    hwnd: HWND,
    thumbnails: HashMap<u64, PreviewThumbnailState>,
    last_thumbnail_failures: HashMap<u64, String>,
    frame_placement: OverlayWindowPlacement,
    placement: OverlayWindowPlacement,
    frame_region_rects: Vec<Rect>,
    host_region_rects: Vec<Rect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreviewThumbnailState {
    handle: isize,
    visible_projection: Option<ThumbnailProjection>,
    visible: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ThumbnailProjection {
    destination_rect: Rect,
    source_rect: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreviewClickTarget {
    hwnd: u64,
    rect: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreviewWindowState {
    role: PreviewWindowRole,
    click_targets: Vec<PreviewClickTarget>,
    painted_windows: Vec<Rect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewWindowRole {
    Frame,
    Host,
}

struct BackdropWindowState {
    shell_snapshot_bitmap: HBITMAP,
    viewport_column_rect: Option<Rect>,
}

#[derive(Clone, Debug)]
struct ShellOverlayEscapeState {
    started_at: Instant,
    overlay_seen: bool,
    overlay_gone_at: Option<Instant>,
    baseline_result_hwnds: HashSet<u64>,
    baseline_foreground_screenshot_hwnd: Option<u64>,
    result_window_seen: bool,
    result_window_gone_at: Option<Instant>,
}

impl ShellOverlayEscapeState {
    fn new(
        started_at: Instant,
        baseline_result_hwnds: HashSet<u64>,
        baseline_foreground_screenshot_hwnd: Option<u64>,
    ) -> Self {
        Self {
            started_at,
            overlay_seen: false,
            overlay_gone_at: None,
            baseline_result_hwnds,
            baseline_foreground_screenshot_hwnd,
            result_window_seen: false,
            result_window_gone_at: None,
        }
    }

    fn should_restore(&mut self, windows: &ShellScreenshotWindows, now: Instant) -> bool {
        let foreground_screenshot_active = windows
            .foreground_screenshot_hwnd
            .is_some_and(|hwnd| Some(hwnd) != self.baseline_foreground_screenshot_hwnd);
        if windows.overlay_present || foreground_screenshot_active {
            self.overlay_seen = true;
            self.overlay_gone_at = None;
            self.result_window_gone_at = None;
            return false;
        }

        let has_new_result_window = windows
            .result_window_hwnds
            .iter()
            .any(|hwnd| !self.baseline_result_hwnds.contains(hwnd));
        if has_new_result_window {
            self.overlay_seen = true;
            self.result_window_seen = true;
            self.result_window_gone_at = None;
            return false;
        }

        if !self.overlay_seen && !self.result_window_seen {
            if !self.baseline_result_hwnds.is_empty()
                || self.baseline_foreground_screenshot_hwnd.is_some()
            {
                return now.duration_since(self.started_at)
                    >= SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT;
            }
            return false;
        }

        if self.result_window_seen {
            let gone_at = self.result_window_gone_at.get_or_insert(now);
            return now.duration_since(*gone_at) >= SHELL_OVERLAY_RESTORE_SETTLE;
        }

        let gone_at = self.overlay_gone_at.get_or_insert(now);
        now.duration_since(*gone_at) >= SHELL_OVERLAY_RESTORE_SETTLE
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ShellScreenshotWindows {
    overlay_present: bool,
    result_window_hwnds: HashSet<u64>,
    foreground_screenshot_hwnd: Option<u64>,
}

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
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let point = point_from_lparam(lparam);
            if let Some(raw_hwnd) = preview_click_target(hwnd, point.0, point.1) {
                dispatch_overview_activate_window(raw_hwnd);
            } else {
                dispatch_overview_dismiss();
            }
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
            paint_preview_rect(hwnd, hdc, client_rect);

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

fn run_overlay_loop(
    command_receiver: Receiver<OverlayCommand>,
    classes: &OverviewClasses,
    backdrop: HWND,
) -> Result<(), String> {
    let mut previews = HashMap::<u64, WorkspacePreviewSurface>::new();
    let mut current_scene = None::<OverviewScene>;
    let mut shell_escape = None::<ShellOverlayEscapeState>;
    let mut backdrop_placement = OverlayWindowPlacement::default();

    loop {
        pump_messages()?;
        let restore_shell_escape =
            if let (Some(_), Some(state)) = (current_scene.as_ref(), shell_escape.as_mut()) {
                let now = Instant::now();
                state.should_restore(&shell_screenshot_windows(), now)
            } else {
                false
            };
        if restore_shell_escape {
            if let Some(scene) = current_scene.as_ref() {
                clear_backdrop_snapshot(backdrop);
                render_scene_frame(
                    backdrop,
                    &mut backdrop_placement,
                    &mut previews,
                    scene,
                    classes,
                    SceneFrameMode::Final,
                )?;
            }
            shell_escape = None;
        }
        match command_receiver.recv_timeout(THREAD_SLICE) {
            Ok(OverlayCommand::Show(scene, response)) => {
                let result = if current_scene.as_ref() == Some(&scene) {
                    Ok(())
                } else if let Some(current) = current_scene.as_ref() {
                    animate_scene_transition(
                        backdrop,
                        &mut backdrop_placement,
                        &mut previews,
                        current,
                        &scene,
                        classes,
                    )
                } else {
                    animate_scene_open(
                        backdrop,
                        &mut backdrop_placement,
                        &mut previews,
                        &scene,
                        classes,
                    )
                };
                let result = result.and_then(|_| {
                    if shell_escape.is_some() {
                        freeze_scene_for_shell_overlay(
                            backdrop,
                            &mut backdrop_placement,
                            &mut previews,
                            &scene,
                        )?;
                    }
                    Ok(())
                });
                if result.is_ok() {
                    current_scene = Some(scene);
                }
                let _ = response.send(result);
            }
            Ok(OverlayCommand::Hide(response)) => {
                shell_escape = None;
                let result = if let Some(scene) = current_scene.take() {
                    animate_scene_close(
                        backdrop,
                        &mut backdrop_placement,
                        &mut previews,
                        &scene,
                        classes,
                    )
                } else {
                    hide_scene(backdrop, &mut backdrop_placement, &mut previews)
                };
                let _ = response.send(result);
            }
            Ok(OverlayCommand::LowerForShellOverlay(response)) => {
                let result = if let Some(scene) = current_scene.as_ref() {
                    let baseline_windows = shell_screenshot_windows();
                    let result = freeze_scene_for_shell_overlay(
                        backdrop,
                        &mut backdrop_placement,
                        &mut previews,
                        scene,
                    );
                    if result.is_ok() {
                        shell_escape = Some(ShellOverlayEscapeState::new(
                            Instant::now(),
                            baseline_windows.result_window_hwnds,
                            baseline_windows.foreground_screenshot_hwnd,
                        ));
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

    let _ = hide_scene(backdrop, &mut backdrop_placement, &mut previews);
    let _ = destroy_all_preview_surfaces(&mut previews);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SceneFrameMode {
    Final,
    Opening { progress_milli: u16 },
    Closing { progress_milli: u16 },
}

fn animate_scene_open(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
) -> Result<(), String> {
    animate_scene(backdrop, backdrop_placement, previews, scene, classes, true)
}

fn animate_scene_close(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
) -> Result<(), String> {
    animate_scene(
        backdrop,
        backdrop_placement,
        previews,
        scene,
        classes,
        false,
    )?;
    hide_scene(backdrop, backdrop_placement, previews)
}

fn animate_scene_transition(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
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
        render_overview_frame(backdrop, backdrop_placement, previews, &frame, classes)?;
        pump_messages()?;

        if (1.0 - progress) <= SPRING_EPSILON || elapsed >= INTRA_OVERVIEW_ANIMATION_MAX_DURATION {
            break;
        }

        thread::sleep(THREAD_SLICE);
    }

    render_scene_frame(
        backdrop,
        backdrop_placement,
        previews,
        to_scene,
        classes,
        SceneFrameMode::Final,
    )
}

fn animate_scene(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
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
        render_scene_frame(backdrop, backdrop_placement, previews, scene, classes, mode)?;
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
    render_scene_frame(backdrop, backdrop_placement, previews, scene, classes, mode)?;
    Ok(())
}

fn render_scene_frame(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
    scene: &OverviewScene,
    classes: &OverviewClasses,
    mode: SceneFrameMode,
) -> Result<(), String> {
    let frame = render_frame_for_scene(scene, mode);
    render_overview_frame(backdrop, backdrop_placement, previews, &frame, classes)
}

fn workspace_canvas_rect(
    workspace: &WorkspacePreviewScene,
    monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_rect = workspace_open_close_source_rect(workspace, monitor_rect);
    match mode {
        SceneFrameMode::Final => workspace.canvas_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, workspace.canvas_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(workspace.canvas_rect, source_rect, progress_milli)
        }
    }
}

fn workspace_viewport_rect(
    workspace: &WorkspacePreviewScene,
    monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_rect = workspace_open_close_source_rect(workspace, monitor_rect);
    match mode {
        SceneFrameMode::Final => workspace.frame_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, workspace.frame_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(workspace.frame_rect, source_rect, progress_milli)
        }
    }
}

fn window_rect_for_mode(
    window: WindowPreviewScene,
    workspace: &WorkspacePreviewScene,
    monitor_rect: Rect,
    mode: SceneFrameMode,
) -> Rect {
    let source_canvas = workspace_open_close_source_rect(workspace, monitor_rect);
    let source_rect =
        scale_rect_between_rects(window.live_rect, workspace.live_rect, source_canvas);
    match mode {
        SceneFrameMode::Final => window.overview_rect,
        SceneFrameMode::Opening { progress_milli } => {
            interpolate_rect(source_rect, window.overview_rect, progress_milli)
        }
        SceneFrameMode::Closing { progress_milli } => {
            interpolate_rect(window.overview_rect, source_rect, progress_milli)
        }
    }
}

fn render_frame_for_scene(scene: &OverviewScene, mode: SceneFrameMode) -> OverviewRenderFrame {
    let workspaces = scene
        .workspaces
        .iter()
        .map(|workspace| {
            let canvas_rect = workspace_canvas_rect(workspace, scene.monitor_rect, mode);
            let viewport_rect = workspace_viewport_rect(workspace, scene.monitor_rect, mode);
            let windows = workspace
                .windows
                .iter()
                .map(|window| WindowRenderFrame {
                    hwnd: window.hwnd,
                    rect: window_rect_for_mode(*window, workspace, scene.monitor_rect, mode),
                })
                .collect();
            WorkspaceRenderFrame {
                workspace_id: workspace.workspace_id,
                canvas_rect,
                viewport_rect,
                windows,
            }
        })
        .collect();

    OverviewRenderFrame {
        monitor_rect: scene.monitor_rect,
        workspaces,
    }
}

fn render_frame_for_transition(
    from_scene: &OverviewScene,
    to_scene: &OverviewScene,
    progress_milli: u16,
) -> OverviewRenderFrame {
    let stack_delta = transition_stack_delta(from_scene, to_scene);
    let workspaces = collect_transition_workspace_ids(from_scene, to_scene)
        .into_iter()
        .map(|workspace_id| {
            let from_workspace = from_scene
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id);
            let to_workspace = to_scene
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id);
            let from_canvas = from_workspace
                .map(|workspace| workspace.canvas_rect)
                .or_else(|| {
                    to_workspace.map(|workspace| {
                        translate_rect(workspace.canvas_rect, -stack_delta.0, -stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let to_canvas = to_workspace
                .map(|workspace| workspace.canvas_rect)
                .or_else(|| {
                    from_workspace.map(|workspace| {
                        translate_rect(workspace.canvas_rect, stack_delta.0, stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let from_viewport = from_workspace
                .map(|workspace| workspace.frame_rect)
                .or_else(|| {
                    to_workspace.map(|workspace| {
                        translate_rect(workspace.frame_rect, -stack_delta.0, -stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");
            let to_viewport = to_workspace
                .map(|workspace| workspace.frame_rect)
                .or_else(|| {
                    from_workspace.map(|workspace| {
                        translate_rect(workspace.frame_rect, stack_delta.0, stack_delta.1)
                    })
                })
                .expect("transition workspace must exist in at least one scene");

            WorkspaceRenderFrame {
                workspace_id,
                canvas_rect: interpolate_rect(from_canvas, to_canvas, progress_milli),
                viewport_rect: interpolate_rect(from_viewport, to_viewport, progress_milli),
                windows: render_transition_window_frames(
                    from_workspace,
                    to_workspace,
                    stack_delta,
                    progress_milli,
                ),
            }
        })
        .collect();

    OverviewRenderFrame {
        monitor_rect: interpolate_rect(
            from_scene.monitor_rect,
            to_scene.monitor_rect,
            progress_milli,
        ),
        workspaces,
    }
}

fn transition_stack_delta(from_scene: &OverviewScene, to_scene: &OverviewScene) -> (i32, i32) {
    for to_workspace in &to_scene.workspaces {
        let Some(from_workspace) = from_scene
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == to_workspace.workspace_id)
        else {
            continue;
        };
        return (
            to_workspace.frame_rect.x - from_workspace.frame_rect.x,
            to_workspace.frame_rect.y - from_workspace.frame_rect.y,
        );
    }

    (0, 0)
}

fn collect_transition_workspace_ids(
    from_scene: &OverviewScene,
    to_scene: &OverviewScene,
) -> Vec<WorkspaceId> {
    let mut ids = to_scene
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_id)
        .collect::<Vec<_>>();
    for workspace in &from_scene.workspaces {
        if !ids.contains(&workspace.workspace_id) {
            ids.push(workspace.workspace_id);
        }
    }
    ids
}

fn render_transition_window_frames(
    from_workspace: Option<&WorkspacePreviewScene>,
    to_workspace: Option<&WorkspacePreviewScene>,
    stack_delta: (i32, i32),
    progress_milli: u16,
) -> Vec<WindowRenderFrame> {
    let window_delta =
        transition_window_delta_for_workspace(from_workspace, to_workspace, stack_delta);
    let mut hwnds = to_workspace
        .map(|workspace| {
            workspace
                .windows
                .iter()
                .map(|window| window.hwnd)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(workspace) = from_workspace {
        for window in &workspace.windows {
            if !hwnds.contains(&window.hwnd) {
                hwnds.push(window.hwnd);
            }
        }
    }

    hwnds
        .into_iter()
        .filter_map(|hwnd| {
            let from_rect = from_workspace.and_then(|workspace| {
                workspace
                    .windows
                    .iter()
                    .find(|window| window.hwnd == hwnd)
                    .map(|window| window.overview_rect)
            });
            let to_rect = to_workspace.and_then(|workspace| {
                workspace
                    .windows
                    .iter()
                    .find(|window| window.hwnd == hwnd)
                    .map(|window| window.overview_rect)
            });
            let from_rect = from_rect.or_else(|| {
                to_rect.map(|rect| translate_rect(rect, -window_delta.0, -window_delta.1))
            })?;
            let to_rect = to_rect
                .or_else(|| Some(translate_rect(from_rect, window_delta.0, window_delta.1)))?;
            Some(WindowRenderFrame {
                hwnd,
                rect: interpolate_rect(from_rect, to_rect, progress_milli),
            })
        })
        .collect()
}

fn transition_window_delta_for_workspace(
    from_workspace: Option<&WorkspacePreviewScene>,
    to_workspace: Option<&WorkspacePreviewScene>,
    fallback_delta: (i32, i32),
) -> (i32, i32) {
    let (Some(from_workspace), Some(to_workspace)) = (from_workspace, to_workspace) else {
        return fallback_delta;
    };

    for to_window in &to_workspace.windows {
        let Some(from_window) = from_workspace
            .windows
            .iter()
            .find(|window| window.hwnd == to_window.hwnd)
        else {
            continue;
        };
        return (
            to_window
                .overview_rect
                .x
                .saturating_sub(from_window.overview_rect.x),
            to_window
                .overview_rect
                .y
                .saturating_sub(from_window.overview_rect.y),
        );
    }

    (
        to_workspace
            .frame_rect
            .x
            .saturating_sub(from_workspace.frame_rect.x),
        to_workspace
            .frame_rect
            .y
            .saturating_sub(from_workspace.frame_rect.y),
    )
}

fn render_overview_frame(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
    frame: &OverviewRenderFrame,
    classes: &OverviewClasses,
) -> Result<(), String> {
    position_backdrop(backdrop, frame.monitor_rect, backdrop_placement)?;
    set_backdrop_viewport_column(
        backdrop,
        overview_viewport_column_rect(frame)
            .map(|column_rect| rect_relative_to(column_rect, frame.monitor_rect)),
    );

    let desired_ids = frame
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.get())
        .collect::<Vec<_>>();
    let mut ordered_workspaces = frame.workspaces.iter().collect::<Vec<_>>();
    ordered_workspaces.sort_by(|left, right| {
        preview_center_distance(*right, frame.monitor_rect)
            .cmp(&preview_center_distance(*left, frame.monitor_rect))
            .then(left.canvas_rect.y.cmp(&right.canvas_rect.y))
    });

    let underlay_rect = overview_viewport_column_rect(frame).unwrap_or(frame.monitor_rect);
    let mut insert_after = Some(backdrop);
    for workspace in ordered_workspaces {
        let key = workspace.workspace_id.get();
        let surface = previews
            .entry(key)
            .or_insert_with(|| WorkspacePreviewSurface {
                frame_hwnd: null_mut(),
                hwnd: null_mut(),
                thumbnails: HashMap::new(),
                last_thumbnail_failures: HashMap::new(),
                frame_placement: OverlayWindowPlacement::default(),
                placement: OverlayWindowPlacement::default(),
                frame_region_rects: Vec::new(),
                host_region_rects: Vec::new(),
            });
        if surface.frame_hwnd.is_null() {
            surface.frame_hwnd = create_preview_window(classes.instance, PreviewWindowRole::Frame)?;
        }
        if surface.hwnd.is_null() {
            surface.hwnd = create_preview_window(classes.instance, PreviewWindowRole::Host)?;
        }
        if let Some(topmost_hwnd) = render_preview_workspace_frame(
            surface,
            workspace,
            underlay_rect,
            frame.monitor_rect,
            insert_after,
        )? {
            insert_after = Some(topmost_hwnd);
        }
    }

    let stale_ids = previews
        .keys()
        .copied()
        .filter(|workspace_id| !desired_ids.contains(workspace_id))
        .collect::<Vec<_>>();
    for workspace_id in stale_ids {
        if let Some(surface) = previews.remove(&workspace_id) {
            destroy_workspace_surface(surface)?;
        }
    }

    let _ = {
        // SAFETY: synchronizes DWM composition after the frame surfaces have been updated.
        unsafe { DwmFlush() }
    };
    Ok(())
}

fn preview_center_distance(workspace: &WorkspaceRenderFrame, monitor_rect: Rect) -> i64 {
    let workspace_center =
        i64::from(workspace.canvas_rect.y) + i64::from(workspace.canvas_rect.height) / 2;
    let monitor_center = i64::from(monitor_rect.y) + i64::from(monitor_rect.height) / 2;
    (workspace_center - monitor_center).abs()
}

fn overview_viewport_column_rect(frame: &OverviewRenderFrame) -> Option<Rect> {
    let viewport_rect = frame
        .workspaces
        .iter()
        .filter_map(|workspace| {
            intersect_rect(workspace.viewport_rect, frame.monitor_rect).map(|viewport_rect| {
                (
                    preview_center_distance(workspace, frame.monitor_rect),
                    viewport_rect,
                )
            })
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, viewport_rect)| viewport_rect)?;

    Some(Rect::new(
        viewport_rect.x,
        frame.monitor_rect.y,
        viewport_rect.width,
        frame.monitor_rect.height,
    ))
}

fn preview_click_targets(
    workspace: &WorkspaceRenderFrame,
    visible_canvas_rect: Rect,
    visible_hwnds: &HashSet<u64>,
) -> Vec<PreviewClickTarget> {
    workspace
        .windows
        .iter()
        .filter(|window| visible_hwnds.contains(&window.hwnd))
        .filter_map(|window| {
            let clipped_rect = intersect_rect(window.rect, visible_canvas_rect)?;
            Some(PreviewClickTarget {
                hwnd: window.hwnd,
                rect: rect_relative_to(clipped_rect, visible_canvas_rect),
            })
        })
        .collect()
}

fn preview_window_rects(
    workspace: &WorkspaceRenderFrame,
    clip_rect: Rect,
    origin_rect: Rect,
) -> Vec<PreviewClickTarget> {
    workspace
        .windows
        .iter()
        .filter_map(|window| {
            let clipped_rect = intersect_rect(window.rect, clip_rect)?;
            Some(PreviewClickTarget {
                hwnd: window.hwnd,
                rect: rect_relative_to(clipped_rect, origin_rect),
            })
        })
        .collect()
}

fn hide_scene(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
) -> Result<(), String> {
    clear_backdrop_snapshot(backdrop);
    clear_backdrop_viewport_column(backdrop);
    for surface in previews.values_mut() {
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.frame_region_rects.clear();
        surface.host_region_rects.clear();
    }
    hide_overlay_window(backdrop, backdrop_placement);
    Ok(())
}

fn destroy_all_preview_surfaces(
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
) -> Result<(), String> {
    let ids = previews.keys().copied().collect::<Vec<_>>();
    for workspace_id in ids {
        if let Some(surface) = previews.remove(&workspace_id) {
            destroy_workspace_surface(surface)?;
        }
    }
    Ok(())
}

fn freeze_scene_for_shell_overlay(
    backdrop: HWND,
    backdrop_placement: &mut OverlayWindowPlacement,
    previews: &mut HashMap<u64, WorkspacePreviewSurface>,
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
    for workspace in &scene.workspaces {
        let Some(surface) = previews.get_mut(&workspace.workspace_id.get()) else {
            continue;
        };
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.frame_region_rects.clear();
        surface.host_region_rects.clear();
    }
    let _ = {
        // SAFETY: synchronizes DWM composition after switching to the frozen backdrop.
        unsafe { DwmFlush() }
    };
    Ok(())
}

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

fn create_preview_window(instance: HINSTANCE, role: PreviewWindowRole) -> Result<HWND, String> {
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
    initialize_preview_window_state(window, role)?;
    Ok(window)
}

fn render_preview_workspace_frame(
    surface: &mut WorkspacePreviewSurface,
    workspace: &WorkspaceRenderFrame,
    underlay_rect: Rect,
    monitor_rect: Rect,
    insert_after: Option<HWND>,
) -> Result<Option<HWND>, String> {
    let Some(visible_canvas_rect) = intersect_rect(workspace.canvas_rect, monitor_rect) else {
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.frame_region_rects.clear();
        surface.host_region_rects.clear();
        return Ok(None);
    };
    let Some(visible_underlay_rect) = intersect_rect(underlay_rect, monitor_rect) else {
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.frame_region_rects.clear();
        surface.host_region_rects.clear();
        return Ok(None);
    };
    let Some(visible_viewport_rect) = intersect_rect(workspace.viewport_rect, monitor_rect) else {
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.frame_region_rects.clear();
        surface.host_region_rects.clear();
        return Ok(None);
    };
    let was_host_visible = surface.placement.visible;

    let painted_windows =
        preview_window_rects(workspace, visible_viewport_rect, visible_underlay_rect)
            .into_iter()
            .map(|target| target.rect)
            .collect::<Vec<_>>();
    let frame_visible = !painted_windows.is_empty();
    if frame_visible {
        update_preview_window_state(surface.frame_hwnd, Vec::new(), painted_windows.clone());
        update_preview_frame_region(surface, &painted_windows)?;
        position_window_with_order(
            surface.frame_hwnd,
            visible_underlay_rect,
            true,
            true,
            insert_after,
            &mut surface.frame_placement,
        )?;
    } else {
        update_preview_window_state(surface.frame_hwnd, Vec::new(), Vec::new());
        update_preview_frame_region(surface, &[])?;
        hide_overlay_window(surface.frame_hwnd, &mut surface.frame_placement);
    }

    if !was_host_visible {
        update_preview_host_region(surface, &[])?;
        position_window_with_order(
            surface.hwnd,
            visible_canvas_rect,
            true,
            true,
            if frame_visible {
                Some(surface.frame_hwnd)
            } else {
                insert_after
            },
            &mut surface.placement,
        )?;
        reset_preview_thumbnail_cache(surface);
    }

    let visible_hwnds = sync_workspace_thumbnails(surface, workspace, visible_canvas_rect);
    let click_targets = preview_click_targets(workspace, visible_canvas_rect, &visible_hwnds);
    let host_rects = click_targets
        .iter()
        .map(|target| target.rect)
        .collect::<Vec<_>>();

    if host_rects.is_empty() {
        update_preview_window_state(surface.hwnd, Vec::new(), Vec::new());
        hide_overlay_window(surface.hwnd, &mut surface.placement);
        reset_preview_thumbnail_cache(surface);
        surface.host_region_rects.clear();
        return Ok(frame_visible.then_some(surface.frame_hwnd));
    }

    update_preview_window_state(surface.hwnd, click_targets, Vec::new());
    update_preview_host_region(surface, &host_rects)?;

    position_window_with_order(
        surface.hwnd,
        visible_canvas_rect,
        true,
        true,
        if frame_visible {
            Some(surface.frame_hwnd)
        } else {
            insert_after
        },
        &mut surface.placement,
    )?;
    Ok(Some(surface.hwnd))
}

fn position_backdrop(
    window: HWND,
    rect: Rect,
    placement: &mut OverlayWindowPlacement,
) -> Result<(), String> {
    position_window(window, rect, true, placement)
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

fn sync_workspace_thumbnails(
    surface: &mut WorkspacePreviewSurface,
    workspace: &WorkspaceRenderFrame,
    visible_canvas_rect: Rect,
) -> HashSet<u64> {
    let desired_hwnds = workspace
        .windows
        .iter()
        .map(|window| window.hwnd)
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
    }

    let mut visible_hwnds = HashSet::new();
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
            continue;
        }

        let Some(clipped_rect) = intersect_rect(window.rect, visible_canvas_rect) else {
            if let Some(thumbnail) = surface.thumbnails.get_mut(&window.hwnd) {
                if thumbnail.visible {
                    if hide_thumbnail(thumbnail.handle).is_ok() {
                        thumbnail.visible = false;
                        thumbnail.visible_projection = None;
                    } else if let Some(stale_thumbnail) = surface.thumbnails.remove(&window.hwnd) {
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

        let source_size = match thumbnail_source_size(thumbnail.handle) {
            Ok(size) => size,
            Err(error) => {
                log_thumbnail_failure(surface, window.hwnd, "source-size", &error);
                continue;
            }
        };
        let Some(projection) =
            thumbnail_projection(window.rect, clipped_rect, visible_canvas_rect, source_size)
        else {
            continue;
        };

        if thumbnail.visible && thumbnail.visible_projection == Some(projection) {
            clear_thumbnail_failure(surface, window.hwnd);
            visible_hwnds.insert(window.hwnd);
            continue;
        }

        if update_thumbnail(thumbnail.handle, projection).is_ok() {
            thumbnail.visible = true;
            thumbnail.visible_projection = Some(projection);
            clear_thumbnail_failure(surface, window.hwnd);
            visible_hwnds.insert(window.hwnd);
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

    visible_hwnds
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
}

fn interpolate_rect(from: Rect, to: Rect, progress_milli: u16) -> Rect {
    if progress_milli == 0 {
        return from;
    }
    if progress_milli >= 1000 {
        return to;
    }

    Rect::new(
        interpolate_i32(from.x, to.x, progress_milli),
        interpolate_i32(from.y, to.y, progress_milli),
        interpolate_u32(from.width.max(1), to.width.max(1), progress_milli),
        interpolate_u32(from.height.max(1), to.height.max(1), progress_milli),
    )
}

fn translate_rect(rect: Rect, delta_x: i32, delta_y: i32) -> Rect {
    Rect::new(
        rect.x.saturating_add(delta_x),
        rect.y.saturating_add(delta_y),
        rect.width,
        rect.height,
    )
}

fn interpolate_i32(from: i32, to: i32, progress_milli: u16) -> i32 {
    let delta = i64::from(to) - i64::from(from);
    let step = (delta * i64::from(progress_milli)) / 1000;
    from.saturating_add(step.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

fn interpolate_u32(from: u32, to: u32, progress_milli: u16) -> u32 {
    let delta = i64::from(to) - i64::from(from);
    let step = (delta * i64::from(progress_milli)) / 1000;
    let value = i64::from(from) + step;
    value.clamp(1, i64::from(u32::MAX)) as u32
}

fn spring_progress(elapsed: Duration) -> f64 {
    let time = elapsed.as_secs_f64();
    let omega = SPRING_STIFFNESS.sqrt();
    let progress = 1.0 - (1.0 + omega * time) * (-omega * time).exp();
    progress.clamp(0.0, 1.0)
}

fn initialize_preview_window_state(hwnd: HWND, role: PreviewWindowRole) -> Result<(), String> {
    let state = Box::new(PreviewWindowState {
        role,
        click_targets: Vec::new(),
        painted_windows: Vec::new(),
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

fn update_preview_window_state(
    hwnd: HWND,
    click_targets: Vec<PreviewClickTarget>,
    painted_windows: Vec<Rect>,
) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let state_changed = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe {
            (*state_ptr).click_targets != click_targets
                || (*state_ptr).painted_windows != painted_windows
        }
    };
    if !state_changed {
        return;
    }

    {
        // SAFETY: mutates the owned preview state for this HWND in place.
        unsafe {
            (*state_ptr).click_targets = click_targets;
            (*state_ptr).painted_windows = painted_windows;
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

fn update_preview_frame_region(
    surface: &mut WorkspacePreviewSurface,
    rects: &[Rect],
) -> Result<(), String> {
    update_preview_window_region(surface.frame_hwnd, &mut surface.frame_region_rects, rects)
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
    paint_solid_rect(hdc, client_rect, OVERVIEW_BACKDROP_COLOR);
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
    paint_solid_rect(hdc, column_rect, WORKSPACE_PREVIEW_BACKGROUND_COLOR);
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

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam as u32;
    let x = (packed & 0xFFFF) as i16 as i32;
    let y = ((packed >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn rect_contains_point(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect_right(rect) && y >= rect.y && y < rect_bottom(rect)
}

fn hit_test_preview_targets(click_targets: &[PreviewClickTarget], x: i32, y: i32) -> Option<u64> {
    click_targets
        .iter()
        .rev()
        .find(|target| rect_contains_point(target.rect, x, y))
        .map(|target| target.hwnd)
}

fn dispatch_overview_activate_window(raw_hwnd: u64) {
    let Some(control_sender) = OVERVIEW_CONTROL_SENDER.get() else {
        return;
    };
    let _ = control_sender.send(ControlMessage::OverviewActivateWindow { raw_hwnd });
}

fn dispatch_overview_dismiss() {
    let Some(control_sender) = OVERVIEW_CONTROL_SENDER.get() else {
        return;
    };
    let _ = control_sender.send(ControlMessage::OverviewDismiss);
}

fn paint_preview_rect(hwnd: HWND, hdc: HDC, _client_rect: RECT) {
    let state_ptr = preview_window_state_ptr(hwnd);
    if state_ptr.is_null() {
        return;
    }

    let (role, painted_windows) = {
        // SAFETY: pointer remains valid until `WM_NCDESTROY` frees it.
        unsafe { ((*state_ptr).role, &(*state_ptr).painted_windows) }
    };
    if role == PreviewWindowRole::Host {
        return;
    }

    for window_rect in painted_windows {
        let window_rect = rect_to_win32(*window_rect, "paint preview window").unwrap_or(RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        });
        paint_preview_window_card(hdc, window_rect);
    }
}

fn paint_preview_window_card(hdc: HDC, window_rect: RECT) {
    paint_solid_rect(hdc, window_rect, WORKSPACE_PREVIEW_WINDOW_BORDER_COLOR);
    let inner_window_rect = inset_win32_rect(window_rect, WORKSPACE_PREVIEW_BORDER_PX);
    paint_solid_rect(hdc, inner_window_rect, WORKSPACE_PREVIEW_WINDOW_FILL_COLOR);

    let Some(header_rect) = preview_window_header_rect(inner_window_rect) else {
        return;
    };
    paint_solid_rect(hdc, header_rect, WORKSPACE_PREVIEW_WINDOW_HEADER_COLOR);
    paint_preview_window_controls(hdc, header_rect);
    paint_preview_window_content(hdc, inner_window_rect, header_rect);
}

fn paint_solid_rect(hdc: HDC, rect: RECT, color: u32) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }

    let brush = stock_dc_brush();
    let _ = {
        // SAFETY: changes the color of the stock DC brush used for the next fill.
        unsafe { SetDCBrushColor(hdc, color) }
    };
    let _ = {
        // SAFETY: fills the provided rectangle with the stock DC brush into the preview DC.
        unsafe { FillRect(hdc, &rect, brush) }
    };
}

fn stock_dc_brush() -> HBRUSH {
    // SAFETY: retrieves the process-global stock DC brush object for immediate drawing.
    unsafe { GetStockObject(DC_BRUSH) as HBRUSH }
}

fn inset_win32_rect(rect: RECT, inset: i32) -> RECT {
    let inset = inset.max(0);
    RECT {
        left: rect.left.saturating_add(inset),
        top: rect.top.saturating_add(inset),
        right: rect.right.saturating_sub(inset),
        bottom: rect.bottom.saturating_sub(inset),
    }
}

fn preview_window_header_rect(window_rect: RECT) -> Option<RECT> {
    let width = win32_rect_width(window_rect);
    let height = win32_rect_height(window_rect);
    if width < 18 || height < 12 {
        return None;
    }

    let header_height = ((height as f64) * 0.14).round() as i32;
    let header_height = header_height.clamp(8, 28);
    Some(RECT {
        left: window_rect.left,
        top: window_rect.top,
        right: window_rect.right,
        bottom: window_rect.top.saturating_add(header_height.min(height)),
    })
}

fn paint_preview_window_controls(hdc: HDC, header_rect: RECT) {
    let header_height = win32_rect_height(header_rect);
    let button_size = (header_height / 4).clamp(2, 5);
    let gap = button_size;
    let top = header_rect
        .top
        .saturating_add(((header_height - button_size) / 2).max(0));
    let mut left = header_rect.left.saturating_add(gap + 1);

    for _ in 0..3 {
        let control_rect = RECT {
            left,
            top,
            right: left.saturating_add(button_size),
            bottom: top.saturating_add(button_size),
        };
        paint_solid_rect(hdc, control_rect, WORKSPACE_PREVIEW_WINDOW_CONTROL_COLOR);
        left = left.saturating_add(button_size + gap);
    }
}

fn paint_preview_window_content(hdc: HDC, window_rect: RECT, header_rect: RECT) {
    let content_rect = RECT {
        left: window_rect.left,
        top: header_rect.bottom,
        right: window_rect.right,
        bottom: window_rect.bottom,
    };
    if win32_rect_width(content_rect) < 24 || win32_rect_height(content_rect) < 18 {
        return;
    }

    let inset_rect = inset_win32_rect(content_rect, 6);
    if win32_rect_width(inset_rect) < 18 || win32_rect_height(inset_rect) < 14 {
        return;
    }

    let primary_height = 3;
    let secondary_height = 2;
    let line_gap = 6;

    let primary_line = RECT {
        left: inset_rect.left,
        top: inset_rect.top,
        right: inset_rect
            .left
            .saturating_add((win32_rect_width(inset_rect) * 3) / 5),
        bottom: inset_rect.top.saturating_add(primary_height),
    };
    paint_solid_rect(hdc, primary_line, WORKSPACE_PREVIEW_WINDOW_CONTENT_COLOR);

    let second_top = primary_line.bottom.saturating_add(line_gap);
    let second_line = RECT {
        left: inset_rect.left,
        top: second_top,
        right: inset_rect
            .left
            .saturating_add((win32_rect_width(inset_rect) * 4) / 5),
        bottom: second_top.saturating_add(secondary_height),
    };
    paint_solid_rect(
        hdc,
        second_line,
        WORKSPACE_PREVIEW_WINDOW_CONTENT_SECONDARY_COLOR,
    );

    let third_top = second_line.bottom.saturating_add(line_gap);
    if third_top.saturating_add(secondary_height) <= inset_rect.bottom {
        let third_line = RECT {
            left: inset_rect.left,
            top: third_top,
            right: inset_rect
                .left
                .saturating_add((win32_rect_width(inset_rect) * 2) / 3),
            bottom: third_top.saturating_add(secondary_height),
        };
        paint_solid_rect(
            hdc,
            third_line,
            WORKSPACE_PREVIEW_WINDOW_CONTENT_SECONDARY_COLOR,
        );
    }
}

fn win32_rect_width(rect: RECT) -> i32 {
    rect.right.saturating_sub(rect.left)
}

fn win32_rect_height(rect: RECT) -> i32 {
    rect.bottom.saturating_sub(rect.top)
}

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
    let source_left =
        ((clip_left * source_width) / window_width).clamp(0, source_width.saturating_sub(1)) as i32;
    let source_top = ((clip_top * source_height) / window_height)
        .clamp(0, source_height.saturating_sub(1)) as i32;
    let source_right = (source_width - (clip_right * source_width) / window_width)
        .clamp(i64::from(source_left + 1), source_width) as i32;
    let source_bottom = (source_height - (clip_bottom * source_height) / window_height)
        .clamp(i64::from(source_top + 1), source_height) as i32;

    Some(ThumbnailProjection {
        destination_rect,
        source_rect: Rect::new(
            source_left,
            source_top,
            (source_right - source_left).max(1) as u32,
            (source_bottom - source_top).max(1) as u32,
        ),
    })
}

fn update_thumbnail(thumbnail: isize, projection: ThumbnailProjection) -> Result<(), String> {
    let destination = rect_to_win32(projection.destination_rect, "thumbnail destination")?;
    let source = rect_to_win32(projection.source_rect, "thumbnail source")?;
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
        fSourceClientAreaOnly: 0,
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
    let _ = {
        // SAFETY: paired with successful frame window creation on this thread.
        unsafe { DestroyWindow(surface.frame_hwnd) }
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use flowtile_domain::{
        Column, ColumnMode, Rect, RuntimeMode, Size, WidthSemantics, WindowClassification,
        WindowLayer, WindowNode, WmState, WorkspaceId,
    };

    use super::{
        HWND, HWND_TOPMOST, OVERVIEW_DEFAULT_ZOOM, OVERVIEW_OPEN_CLOSE_SOURCE_ZOOM,
        OVERVIEW_WORKSPACE_GAP_RATIO, OverlayWindowPlacement, OverviewRenderFrame, OverviewScene,
        PreviewClickTarget, SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT, SHELL_OVERLAY_RESTORE_SETTLE,
        SWP_NOACTIVATE, SWP_NOZORDER, SceneFrameMode, ShellOverlayEscapeState,
        ShellScreenshotWindows, WindowPreviewScene, WindowRenderFrame, WorkspacePreviewScene,
        WorkspaceRenderFrame, build_overview_scene, build_preview_stack_layout,
        hit_test_preview_targets, intersect_rect, is_shell_screenshot_overlay,
        is_shell_screenshot_result_window, overview_viewport_column_rect, preview_click_targets,
        preview_window_rects, rect_bottom, rect_right, render_frame_for_transition,
        resolve_z_order_target, thumbnail_projection, window_rect_for_mode,
        workspace_open_close_source_rect,
    };

    #[test]
    fn closed_overview_has_no_scene() {
        let state = WmState::new(RuntimeMode::WmOnly);

        let scene = build_overview_scene(&state).expect("scene should build");
        assert!(scene.is_none());
    }

    #[test]
    fn overview_scene_centers_active_workspace_stack() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let first_workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, first_workspace_id, 100, 420);
        state.normalize_workspace_set(workspace_set_id);

        let second_workspace_id = state
            .ensure_tail_workspace(monitor_id)
            .expect("tail workspace should exist");
        add_tiled_column(&mut state, second_workspace_id, 200, 520);
        state.normalize_workspace_set(workspace_set_id);

        let third_workspace_id = state
            .ensure_tail_workspace(monitor_id)
            .expect("second tail workspace should exist");
        add_tiled_column(&mut state, third_workspace_id, 300, 480);
        state.normalize_workspace_set(workspace_set_id);

        set_active_workspace(
            &mut state,
            monitor_id,
            workspace_set_id,
            second_workspace_id,
        );

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(second_workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let layout = build_preview_stack_layout(scene.monitor_rect);
        let expected_gap =
            (900.0 * OVERVIEW_WORKSPACE_GAP_RATIO * OVERVIEW_DEFAULT_ZOOM).round() as i32;

        let first = scene
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == first_workspace_id)
            .expect("first workspace preview should remain visible");
        let second = scene
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == second_workspace_id)
            .expect("active workspace preview should exist");
        let third = scene
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == third_workspace_id)
            .expect("next workspace preview should remain visible");

        assert_eq!(second.frame_rect.height, layout.frame_height);
        assert_eq!(second.frame_rect.y, layout.active_frame_y);
        assert_eq!(
            second.frame_rect.x,
            scene.monitor_rect.x
                + (scene.monitor_rect.width.min(i32::MAX as u32) as i32
                    - layout.frame_width.min(i32::MAX as u32) as i32)
                    / 2
        );
        assert_eq!(second.frame_rect.width, layout.frame_width);
        assert_eq!(
            first.frame_rect.y,
            second.frame_rect.y - layout.frame_height as i32 - expected_gap
        );
        assert_eq!(
            third.frame_rect.y,
            second.frame_rect.y + layout.frame_height as i32 + expected_gap
        );
        assert!(first.frame_rect.y < scene.monitor_rect.y);
        assert!(rect_bottom(third.frame_rect) > rect_bottom(scene.monitor_rect));
        assert!(second.selected);
        assert!(!second.windows.is_empty());
    }

    #[test]
    fn overview_scene_preserves_window_proportions_at_fixed_zoom() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, workspace_id, 100, 320);
        add_tiled_column(&mut state, workspace_id, 200, 1000);
        add_tiled_column(&mut state, workspace_id, 300, 260);
        state.normalize_workspace_set(workspace_set_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == workspace_id)
            .expect("active workspace preview should exist");
        let width_scales = workspace
            .windows
            .iter()
            .map(|window| window.overview_rect.width as f64 / window.live_rect.width as f64)
            .collect::<Vec<_>>();
        let height_scales = workspace
            .windows
            .iter()
            .map(|window| window.overview_rect.height as f64 / window.live_rect.height as f64)
            .collect::<Vec<_>>();

        for scale in width_scales {
            assert!((scale - OVERVIEW_DEFAULT_ZOOM).abs() < 0.02);
        }
        for scale in height_scales {
            assert!((scale - OVERVIEW_DEFAULT_ZOOM).abs() < 0.02);
        }
        assert_eq!(workspace.frame_rect.width, 800);
        assert_eq!(workspace.frame_rect.height, 450);
        assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
    }

    #[test]
    fn overview_open_close_animation_keeps_workspace_bounded_below_live_size() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, workspace_id, 100, 960);
        state.normalize_workspace_set(workspace_set_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == workspace_id)
            .expect("workspace preview should exist");
        let source_rect = workspace_open_close_source_rect(workspace, scene.monitor_rect);

        assert_eq!(
            source_rect.width,
            (1600.0 * OVERVIEW_OPEN_CLOSE_SOURCE_ZOOM).round() as u32
        );
        assert_eq!(
            source_rect.height,
            (900.0 * OVERVIEW_OPEN_CLOSE_SOURCE_ZOOM).round() as u32
        );
        assert!(source_rect.width < workspace.live_rect.width);
        assert!(source_rect.height < workspace.live_rect.height);
        assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
        assert_eq!(workspace.canvas_rect.height, workspace.frame_rect.height);
        assert!(source_rect.width > workspace.frame_rect.width);
        assert!(source_rect.height > workspace.frame_rect.height);
    }

    #[test]
    fn selected_workspace_close_animation_scales_width_and_height_together() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, workspace_id, 100, 960);
        state.normalize_workspace_set(workspace_set_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == workspace_id)
            .expect("workspace preview should exist");
        let window = workspace
            .windows
            .first()
            .copied()
            .expect("preview should contain at least one window");

        let mode = SceneFrameMode::Closing {
            progress_milli: 500,
        };
        let animated_rect = window_rect_for_mode(window, workspace, scene.monitor_rect, mode);

        let width_scale = animated_rect.width as f64 / window.live_rect.width as f64;
        let height_scale = animated_rect.height as f64 / window.live_rect.height as f64;
        assert!(
            (width_scale - height_scale).abs() < 0.02,
            "expected uniform scale, got width_scale={width_scale} height_scale={height_scale}"
        );
    }

    #[test]
    fn non_selected_workspace_uses_same_preview_scale_as_selected() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let first_workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, first_workspace_id, 100, 960);
        state.normalize_workspace_set(workspace_set_id);
        let second_workspace_id = state
            .workspace_sets
            .get(&workspace_set_id)
            .and_then(|workspace_set| workspace_set.ordered_workspace_ids.get(1).copied())
            .expect("workspace below should exist");
        add_tiled_column(&mut state, second_workspace_id, 100, 960);
        state.normalize_workspace_set(workspace_set_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(first_workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let selected_workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == first_workspace_id)
            .expect("selected workspace preview should exist");
        let other_workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == second_workspace_id)
            .expect("unselected workspace preview should exist");

        assert_eq!(
            selected_workspace.frame_rect.width,
            other_workspace.frame_rect.width
        );
        assert_eq!(
            selected_workspace.frame_rect.height,
            other_workspace.frame_rect.height
        );
        assert_eq!(
            selected_workspace.canvas_rect.width,
            scene.monitor_rect.width
        );
        assert_eq!(other_workspace.canvas_rect.width, scene.monitor_rect.width);
        assert_eq!(
            selected_workspace.windows[0].overview_rect.width,
            other_workspace.windows[0].overview_rect.width
        );
        assert_eq!(other_workspace.live_rect.height, 900);
        assert_eq!(other_workspace.frame_rect.height, 450);
    }

    #[test]
    fn long_workspace_ribbon_uses_monitor_wide_canvas_without_squeezing_edge_windows() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1600, 900), 96, true);
        let workspace_set_id = state
            .workspace_set_id_for_monitor(monitor_id)
            .expect("workspace set should exist");
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("active workspace should exist");

        add_tiled_column(&mut state, workspace_id, 100, 320);
        add_tiled_column(&mut state, workspace_id, 200, 1200);
        add_tiled_column(&mut state, workspace_id, 300, 640);
        state.normalize_workspace_set(workspace_set_id);

        state.overview.is_open = true;
        state.overview.monitor_id = Some(monitor_id);
        state.overview.selection = Some(workspace_id);

        let scene = build_overview_scene(&state)
            .expect("scene should build")
            .expect("overview scene should exist");
        let workspace = scene
            .workspaces
            .iter()
            .find(|candidate| candidate.workspace_id == workspace_id)
            .expect("workspace preview should exist");
        let rightmost_window = workspace
            .windows
            .iter()
            .max_by_key(|window| rect_right(window.overview_rect))
            .copied()
            .expect("preview should contain windows");
        let widest_live_window = workspace
            .windows
            .iter()
            .max_by_key(|window| window.live_rect.width)
            .copied()
            .expect("preview should contain windows");

        assert_eq!(workspace.frame_rect.width, 800);
        assert_eq!(workspace.canvas_rect.width, scene.monitor_rect.width);
        assert_eq!(workspace.canvas_rect.height, workspace.frame_rect.height);
        assert_eq!(workspace.canvas_rect.y, workspace.frame_rect.y);
        assert!(
            (widest_live_window.overview_rect.width as f64
                / widest_live_window.live_rect.width as f64
                - OVERVIEW_DEFAULT_ZOOM)
                .abs()
                < 0.02
        );
        assert!(rect_right(rightmost_window.overview_rect) > rect_right(workspace.frame_rect));
    }

    #[test]
    fn preview_hit_testing_prefers_last_visible_window() {
        let click_targets = vec![
            PreviewClickTarget {
                hwnd: 100,
                rect: Rect::new(10, 10, 100, 100),
            },
            PreviewClickTarget {
                hwnd: 200,
                rect: Rect::new(40, 40, 100, 100),
            },
        ];

        assert_eq!(hit_test_preview_targets(&click_targets, 20, 20), Some(100));
        assert_eq!(hit_test_preview_targets(&click_targets, 60, 60), Some(200));
        assert_eq!(hit_test_preview_targets(&click_targets, 500, 500), None);
    }

    #[test]
    fn thumbnail_projection_crops_left_edge_without_rescaling() {
        let window_rect = Rect::new(100, 50, 400, 200);
        let clipped_rect = Rect::new(200, 50, 300, 200);
        let canvas_rect = Rect::new(0, 0, 600, 300);
        let source_rect = Rect::new(0, 0, 800, 400);

        let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
            .expect("projection should exist");

        assert_eq!(projection.destination_rect, Rect::new(200, 50, 300, 200));
        assert_eq!(projection.source_rect, Rect::new(200, 0, 600, 400));
    }

    #[test]
    fn thumbnail_projection_crops_right_edge_without_rescaling() {
        let window_rect = Rect::new(100, 50, 400, 200);
        let clipped_rect = Rect::new(100, 50, 260, 200);
        let canvas_rect = Rect::new(0, 0, 360, 300);
        let source_rect = Rect::new(0, 0, 800, 400);

        let projection = thumbnail_projection(window_rect, clipped_rect, canvas_rect, source_rect)
            .expect("projection should exist");

        assert_eq!(projection.destination_rect, Rect::new(100, 50, 260, 200));
        assert_eq!(projection.source_rect, Rect::new(0, 0, 520, 400));
    }

    #[test]
    fn preview_click_targets_ignore_windows_without_visible_thumbnails() {
        let workspace = WorkspaceRenderFrame {
            workspace_id: WorkspaceId::new(1),
            canvas_rect: Rect::new(0, 0, 1600, 320),
            viewport_rect: Rect::new(0, 0, 1600, 320),
            windows: vec![
                WindowRenderFrame {
                    hwnd: 100,
                    rect: Rect::new(512, 290, 288, 320),
                },
                WindowRenderFrame {
                    hwnd: 200,
                    rect: Rect::new(800, 290, 288, 320),
                },
            ],
        };
        let visible_hwnds = HashSet::from([200_u64]);

        let click_targets =
            preview_click_targets(&workspace, workspace.canvas_rect, &visible_hwnds);

        assert_eq!(click_targets.len(), 1);
        assert_eq!(click_targets[0].hwnd, 200);
    }

    #[test]
    fn preview_window_rects_keep_workspace_geometry_without_thumbnails() {
        let workspace = WorkspaceRenderFrame {
            workspace_id: WorkspaceId::new(1),
            canvas_rect: Rect::new(0, 0, 1600, 320),
            viewport_rect: Rect::new(0, 0, 1600, 320),
            windows: vec![
                WindowRenderFrame {
                    hwnd: 100,
                    rect: Rect::new(512, 290, 288, 320),
                },
                WindowRenderFrame {
                    hwnd: 200,
                    rect: Rect::new(800, 290, 288, 320),
                },
            ],
        };

        let window_rects =
            preview_window_rects(&workspace, workspace.canvas_rect, workspace.canvas_rect);

        assert_eq!(window_rects.len(), 2);
        assert_eq!(window_rects[0].hwnd, 100);
        assert_eq!(window_rects[1].hwnd, 200);
    }

    #[test]
    fn viewport_column_spans_full_monitor_height() {
        let frame = OverviewRenderFrame {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![
                WorkspaceRenderFrame {
                    workspace_id: WorkspaceId::new(1),
                    canvas_rect: Rect::new(0, 120, 1600, 320),
                    viewport_rect: Rect::new(512, 120, 576, 320),
                    windows: Vec::new(),
                },
                WorkspaceRenderFrame {
                    workspace_id: WorkspaceId::new(2),
                    canvas_rect: Rect::new(0, 520, 1600, 320),
                    viewport_rect: Rect::new(512, 520, 576, 320),
                    windows: Vec::new(),
                },
            ],
        };

        let column = overview_viewport_column_rect(&frame).expect("column should exist");

        assert_eq!(column.x, 512);
        assert_eq!(column.y, 0);
        assert_eq!(column.width, 576);
        assert_eq!(column.height, 900);
    }

    #[test]
    fn viewport_column_uses_closest_visible_preview_geometry() {
        let frame = OverviewRenderFrame {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![
                WorkspaceRenderFrame {
                    workspace_id: WorkspaceId::new(1),
                    canvas_rect: Rect::new(0, -40, 1600, 320),
                    viewport_rect: Rect::new(420, -40, 520, 320),
                    windows: Vec::new(),
                },
                WorkspaceRenderFrame {
                    workspace_id: WorkspaceId::new(2),
                    canvas_rect: Rect::new(0, 290, 1600, 320),
                    viewport_rect: Rect::new(560, 290, 640, 320),
                    windows: Vec::new(),
                },
            ],
        };

        let column = overview_viewport_column_rect(&frame).expect("column should exist");

        assert_eq!(column.x, 560);
        assert_eq!(column.y, 0);
        assert_eq!(column.width, 640);
        assert_eq!(column.height, 900);
    }

    #[test]
    fn relative_overview_layers_are_promoted_above_the_backdrop() {
        let placement = OverlayWindowPlacement {
            rect: Some(Rect::new(0, 0, 100, 100)),
            visible: true,
            topmost: true,
            child_insert_after: None,
        };
        let mut flags = SWP_NOACTIVATE;
        let anchor = 0x1234usize as HWND;

        let target = resolve_z_order_target(Some(anchor), true, &placement, &mut flags);

        assert_eq!(target as isize, HWND_TOPMOST as isize);
        assert_eq!(flags & SWP_NOZORDER, 0);
    }

    #[test]
    fn shell_escape_restores_after_overlay_disappears() {
        let started_at = Instant::now();
        let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                overlay_present: true,
                ..ShellScreenshotWindows::default()
            },
            started_at
        ));
        assert!(!state.should_restore(&ShellScreenshotWindows::default(), started_at));
        assert!(!state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + SHELL_OVERLAY_RESTORE_SETTLE / 2
        ));
        assert!(state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + SHELL_OVERLAY_RESTORE_SETTLE + Duration::from_millis(1)
        ));
    }

    #[test]
    fn shell_escape_does_not_restore_until_session_is_observed() {
        let started_at = Instant::now();
        let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

        assert!(!state.should_restore(&ShellScreenshotWindows::default(), started_at));
        assert!(!state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + Duration::from_secs(5)
        ));
    }

    #[test]
    fn shell_escape_waits_until_new_snipping_tool_result_window_disappears() {
        let started_at = Instant::now();
        let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                overlay_present: true,
                ..ShellScreenshotWindows::default()
            },
            started_at
        ));
        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                result_window_hwnds: HashSet::from([777_u64]),
                ..ShellScreenshotWindows::default()
            },
            started_at + Duration::from_millis(10)
        ));
        assert!(!state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + Duration::from_millis(10) + SHELL_OVERLAY_RESTORE_SETTLE / 2
        ));
        assert!(!state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at
                + Duration::from_millis(10)
                + SHELL_OVERLAY_RESTORE_SETTLE
                + Duration::from_millis(1)
        ));
        assert!(state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at
                + Duration::from_millis(10)
                + SHELL_OVERLAY_RESTORE_SETTLE * 2
                + Duration::from_millis(1)
        ));
    }

    #[test]
    fn shell_escape_ignores_baseline_snipping_tool_window_when_no_new_result_appears() {
        let started_at = Instant::now();
        let baseline = HashSet::from([111_u64]);
        let mut state = ShellOverlayEscapeState::new(started_at, baseline.clone(), None);

        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                overlay_present: true,
                result_window_hwnds: baseline.clone(),
                ..ShellScreenshotWindows::default()
            },
            started_at
        ));
        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                result_window_hwnds: baseline,
                ..ShellScreenshotWindows::default()
            },
            started_at
        ));
        assert!(state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + SHELL_OVERLAY_RESTORE_SETTLE + Duration::from_millis(1)
        ));
    }

    #[test]
    fn shell_escape_waits_while_foreground_screenshot_ui_is_active() {
        let started_at = Instant::now();
        let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), None);

        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                foreground_screenshot_hwnd: Some(777),
                ..ShellScreenshotWindows::default()
            },
            started_at + Duration::from_millis(1)
        ));
        assert!(!state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + SHELL_OVERLAY_RESTORE_SETTLE / 2
        ));
        assert!(state.should_restore(
            &ShellScreenshotWindows::default(),
            started_at + SHELL_OVERLAY_RESTORE_SETTLE * 2 + Duration::from_millis(1)
        ));
    }

    #[test]
    fn shell_escape_ignores_baseline_foreground_screenshot_window() {
        let started_at = Instant::now();
        let mut state = ShellOverlayEscapeState::new(started_at, HashSet::new(), Some(555));

        assert!(!state.should_restore(
            &ShellScreenshotWindows {
                foreground_screenshot_hwnd: Some(555),
                ..ShellScreenshotWindows::default()
            },
            started_at
        ));
        assert!(state.should_restore(
            &ShellScreenshotWindows {
                foreground_screenshot_hwnd: Some(555),
                ..ShellScreenshotWindows::default()
            },
            started_at + SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT + Duration::from_millis(1)
        ));
    }

    #[test]
    fn preview_targets_are_clipped_to_visible_monitor_canvas() {
        let workspace = WorkspaceRenderFrame {
            workspace_id: WorkspaceId::new(1),
            canvas_rect: Rect::new(0, -50, 1600, 320),
            viewport_rect: Rect::new(0, -50, 1600, 320),
            windows: vec![WindowRenderFrame {
                hwnd: 100,
                rect: Rect::new(0, -50, 1600, 320),
            }],
        };
        let monitor_rect = Rect::new(0, 0, 1600, 900);
        let visible_canvas_rect =
            intersect_rect(workspace.canvas_rect, monitor_rect).expect("canvas should intersect");
        let visible_hwnds = HashSet::from([100_u64]);

        let click_targets = preview_click_targets(&workspace, visible_canvas_rect, &visible_hwnds);

        assert_eq!(click_targets.len(), 1);
        assert_eq!(click_targets[0].rect.y, 0);
        assert!(click_targets[0].rect.height <= visible_canvas_rect.height);
    }

    #[test]
    fn transition_frame_interpolates_workspace_and_window_rects() {
        let from_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 200, 1600, 320),
                frame_rect: Rect::new(512, 200, 576, 320),
                selected: true,
                windows: vec![WindowPreviewScene {
                    hwnd: 100,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(512, 200, 288, 320),
                }],
            }],
        };
        let to_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 320, 1600, 320),
                frame_rect: Rect::new(512, 320, 576, 320),
                selected: true,
                windows: vec![WindowPreviewScene {
                    hwnd: 100,
                    live_rect: Rect::new(0, 0, 800, 900),
                    overview_rect: Rect::new(640, 320, 288, 320),
                }],
            }],
        };

        let frame = render_frame_for_transition(&from_scene, &to_scene, 500);

        assert_eq!(
            frame.workspaces[0],
            WorkspaceRenderFrame {
                workspace_id: WorkspaceId::new(1),
                canvas_rect: Rect::new(0, 260, 1600, 320),
                viewport_rect: Rect::new(512, 260, 576, 320),
                windows: vec![WindowRenderFrame {
                    hwnd: 100,
                    rect: Rect::new(576, 260, 288, 320),
                }],
            }
        );
    }

    #[test]
    fn transition_frame_moves_entering_and_leaving_workspaces_with_stack_delta() {
        let from_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(1),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, -68, 1600, 320),
                    frame_rect: Rect::new(512, -68, 576, 320),
                    selected: false,
                    windows: vec![WindowPreviewScene {
                        hwnd: 101,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, -68, 288, 320),
                    }],
                },
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(2),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, 288, 1600, 320),
                    frame_rect: Rect::new(512, 288, 576, 320),
                    selected: true,
                    windows: vec![WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, 288, 288, 320),
                    }],
                },
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(3),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, 644, 1600, 320),
                    frame_rect: Rect::new(512, 644, 576, 320),
                    selected: false,
                    windows: vec![WindowPreviewScene {
                        hwnd: 301,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, 644, 288, 320),
                    }],
                },
            ],
        };
        let to_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(2),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, -68, 1600, 320),
                    frame_rect: Rect::new(512, -68, 576, 320),
                    selected: false,
                    windows: vec![WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, -68, 288, 320),
                    }],
                },
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(3),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, 288, 1600, 320),
                    frame_rect: Rect::new(512, 288, 576, 320),
                    selected: true,
                    windows: vec![WindowPreviewScene {
                        hwnd: 301,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, 288, 288, 320),
                    }],
                },
                WorkspacePreviewScene {
                    workspace_id: WorkspaceId::new(4),
                    live_rect: Rect::new(0, 0, 1600, 900),
                    canvas_rect: Rect::new(0, 644, 1600, 320),
                    frame_rect: Rect::new(512, 644, 576, 320),
                    selected: false,
                    windows: vec![WindowPreviewScene {
                        hwnd: 401,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(512, 644, 288, 320),
                    }],
                },
            ],
        };

        let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
        let leaving = frame
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == WorkspaceId::new(1))
            .expect("leaving workspace should remain animated");
        let entering = frame
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == WorkspaceId::new(4))
            .expect("entering workspace should be animated from offscreen");

        assert_eq!(leaving.canvas_rect.y, -246);
        assert_eq!(leaving.windows[0].rect.y, -246);
        assert_eq!(entering.canvas_rect.y, 822);
        assert_eq!(entering.windows[0].rect.y, 822);
        assert_eq!(leaving.windows[0].rect.width, 288);
        assert_eq!(entering.windows[0].rect.width, 288);
    }

    #[test]
    fn transition_frame_brings_entering_window_from_ribbon_edge() {
        let from_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![
                    WindowPreviewScene {
                        hwnd: 101,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(200, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(800, 0, 800, 900),
                        overview_rect: Rect::new(600, 288, 400, 320),
                    },
                ],
            }],
        };
        let to_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![
                    WindowPreviewScene {
                        hwnd: 101,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(0, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(800, 0, 800, 900),
                        overview_rect: Rect::new(400, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 301,
                        live_rect: Rect::new(1600, 0, 800, 900),
                        overview_rect: Rect::new(800, 288, 400, 320),
                    },
                ],
            }],
        };

        let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
        let entering = frame.workspaces[0]
            .windows
            .iter()
            .find(|window| window.hwnd == 301)
            .expect("entering window should remain part of the transition");

        assert_eq!(entering.rect.x, 900);
        assert_eq!(entering.rect.y, 288);
        assert_eq!(entering.rect.width, 400);
        assert_eq!(entering.rect.height, 320);
    }

    #[test]
    fn transition_frame_pushes_leaving_window_out_through_ribbon_edge() {
        let from_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![
                    WindowPreviewScene {
                        hwnd: 101,
                        live_rect: Rect::new(0, 0, 800, 900),
                        overview_rect: Rect::new(0, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(800, 0, 800, 900),
                        overview_rect: Rect::new(400, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 301,
                        live_rect: Rect::new(1600, 0, 800, 900),
                        overview_rect: Rect::new(800, 288, 400, 320),
                    },
                ],
            }],
        };
        let to_scene = OverviewScene {
            monitor_rect: Rect::new(0, 0, 1600, 900),
            workspaces: vec![WorkspacePreviewScene {
                workspace_id: WorkspaceId::new(1),
                live_rect: Rect::new(0, 0, 1600, 900),
                canvas_rect: Rect::new(0, 288, 1600, 320),
                frame_rect: Rect::new(512, 288, 576, 320),
                selected: true,
                windows: vec![
                    WindowPreviewScene {
                        hwnd: 201,
                        live_rect: Rect::new(800, 0, 800, 900),
                        overview_rect: Rect::new(200, 288, 400, 320),
                    },
                    WindowPreviewScene {
                        hwnd: 301,
                        live_rect: Rect::new(1600, 0, 800, 900),
                        overview_rect: Rect::new(600, 288, 400, 320),
                    },
                ],
            }],
        };

        let frame = render_frame_for_transition(&from_scene, &to_scene, 500);
        let leaving = frame.workspaces[0]
            .windows
            .iter()
            .find(|window| window.hwnd == 101)
            .expect("leaving window should remain part of the transition");

        assert_eq!(leaving.rect.x, -100);
        assert_eq!(leaving.rect.y, 288);
        assert_eq!(leaving.rect.width, 400);
        assert_eq!(leaving.rect.height, 320);
    }

    #[test]
    fn shell_screenshot_overlay_classifier_tracks_capture_toolbar_windows() {
        assert!(is_shell_screenshot_overlay(
            "ApplicationFrameWindow",
            "Screen clip",
            Some("ScreenClippingHost"),
        ));
        assert!(is_shell_screenshot_overlay(
            "ApplicationFrameWindow",
            "Ножницы",
            Some("Unknown"),
        ));
        assert!(is_shell_screenshot_overlay(
            "Microsoft.UI.Content.DesktopChildSiteBridge",
            "Панель инструментов записи",
            Some("SnippingTool.exe"),
        ));
        assert!(is_shell_screenshot_overlay(
            "Windows.UI.Core.CoreWindow",
            "Recording toolbar",
            Some("SnippingTool.exe"),
        ));
    }

    #[test]
    fn shell_screenshot_result_classifier_tracks_snipping_tool_session_windows() {
        assert!(is_shell_screenshot_result_window(
            "ApplicationFrameWindow",
            "Snipping Tool",
            Some("SnippingTool.exe"),
        ));
        assert!(is_shell_screenshot_result_window(
            "ApplicationFrameWindow",
            "Recording toolbar",
            Some("SnippingTool.exe"),
        ));
        assert!(!is_shell_screenshot_result_window(
            "ApplicationFrameWindow",
            "PowerShell",
            Some("WindowsTerminal.exe"),
        ));
    }

    fn set_active_workspace(
        state: &mut WmState,
        monitor_id: flowtile_domain::MonitorId,
        workspace_set_id: flowtile_domain::WorkspaceSetId,
        workspace_id: WorkspaceId,
    ) {
        state
            .workspace_sets
            .get_mut(&workspace_set_id)
            .expect("workspace set should exist")
            .active_workspace_id = workspace_id;
        state
            .focus
            .active_workspace_by_monitor
            .insert(monitor_id, workspace_id);
        state.normalize_workspace_set(workspace_set_id);
    }

    fn add_tiled_column(state: &mut WmState, workspace_id: WorkspaceId, hwnd: u64, width: u32) {
        let window_id = state.allocate_window_id();
        let column_id = state.allocate_column_id();
        state.layout.columns.insert(
            column_id,
            Column::new(
                column_id,
                ColumnMode::Normal,
                WidthSemantics::Fixed(width),
                vec![window_id],
            ),
        );
        state.windows.insert(
            window_id,
            WindowNode {
                id: window_id,
                current_hwnd_binding: Some(hwnd),
                classification: WindowClassification::Application,
                layer: WindowLayer::Tiled,
                workspace_id,
                column_id: Some(column_id),
                is_managed: true,
                is_floating: false,
                is_fullscreen: false,
                restore_target: None,
                last_known_rect: Rect::new(0, 0, width, 900),
                desired_size: Size::new(width, 900),
            },
        );
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .ordered_column_ids
            .push(column_id);
    }
}
