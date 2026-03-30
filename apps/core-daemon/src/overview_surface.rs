use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
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
#[cfg(test)]
use crate::overview_engine::{
    OVERVIEW_DEFAULT_ZOOM, OVERVIEW_WORKSPACE_GAP_RATIO, WindowPreviewScene, WindowRenderFrame,
    WorkspacePreviewScene, WorkspaceRenderFrame, build_overview_scene, build_preview_stack_layout,
    overview_window_rect, preview_click_targets, preview_shell_targets,
    preview_shell_targets_for_frame, preview_window_rects, scale_rect_to_overview,
    window_rect_for_mode, workspace_open_close_source_rect,
};
use crate::overview_engine::{
    OverviewRenderFrame, OverviewScene, PreviewClickTarget, SceneFrameMode, WorkspaceDropTarget,
    hit_test_preview_targets, hit_test_workspace_drop_targets, intersect_rect,
    ordered_frame_workspaces, overview_viewport_column_rect, preview_click_targets_for_frame,
    preview_target_at_point, rect_bottom, rect_relative_to, rect_right, render_frame_for_scene,
    render_frame_for_transition, workspace_drop_targets_for_frame,
};
use flowtile_domain::{Rect, WorkspaceId};
use flowtile_layout_engine::LayoutError;
use flowtile_windows_adapter::WINDOW_SWITCH_ANIMATION_DURATION_MS;

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
            DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DWMWA_EXTENDED_FRAME_BOUNDS, DwmFlush,
            DwmGetWindowAttribute, DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
            DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
        },
        Gdi::{
            AC_SRC_OVER, BLENDFUNCTION, BeginPaint, BitBlt, CombineRgn, CreateCompatibleBitmap,
            CreateCompatibleDC, CreateRectRgn, DC_BRUSH, DeleteDC, DeleteObject, EndPaint,
            FillRect, GdiAlphaBlend, GetDC, GetStockObject, HBITMAP, HBRUSH, HDC, HGDIOBJ, HRGN,
            InvalidateRect, PAINTSTRUCT, PaintDesktop, RGN_OR, ReleaseDC, SRCCOPY, SelectObject,
            SetDCBrushColor, SetWindowRgn,
        },
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
    UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
        GWLP_USERDATA, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowLongPtrW,
        GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        HWND_NOTOPMOST, HWND_TOPMOST, IsWindow, IsWindowVisible, MA_NOACTIVATE, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_CAPTURECHANGED,
        WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
        WM_NCDESTROY, WM_PAINT, WM_QUIT, WM_RBUTTONUP, WNDCLASSW, WS_CLIPCHILDREN,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};

const THREAD_SLICE: Duration = Duration::from_millis(16);
const OPEN_CLOSE_ANIMATION_MAX_DURATION: Duration =
    Duration::from_millis(WINDOW_SWITCH_ANIMATION_DURATION_MS as u64);
const INTRA_OVERVIEW_ANIMATION_MAX_DURATION: Duration =
    Duration::from_millis(WINDOW_SWITCH_ANIMATION_DURATION_MS as u64);
const SPRING_STIFFNESS: f64 = 3200.0;
const SPRING_EPSILON: f64 = 0.0001;
const BACKDROP_CLASS: &str = "FlowtileOverviewBackdrop";
const PREVIEW_CLASS: &str = "FlowtileOverviewPreview";
const OVERVIEW_DRAG_START_THRESHOLD_PX: i32 = 6;
const OVERVIEW_BACKDROP_COLOR: u32 = rgb_color(0x26, 0x26, 0x26);
const WORKSPACE_PREVIEW_BACKGROUND_COLOR: u32 = rgb_color(0x16, 0x16, 0x16);
const OVERVIEW_VIEWPORT_COLUMN_ALPHA: u8 = 168;
const SHELL_OVERLAY_PREPARE_TIMEOUT: Duration = Duration::from_millis(180);
const SHELL_OVERLAY_BASELINE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const SHELL_OVERLAY_RESTORE_SETTLE: Duration = Duration::from_millis(80);
const SCREEN_CLIPPING_HOST_PROCESS: &str = "screenclippinghost";
const SNIPPING_TOOL_PROCESS: &str = "snippingtool";
const OVERVIEW_THUMBNAIL_DIAGNOSTICS_ENV: &str = "FLOWTILE_OVERVIEW_THUMBNAIL_DIAGNOSTICS";
const OVERVIEW_THUMBNAIL_CLIENT_ONLY_ENV: &str = "FLOWTILE_OVERVIEW_THUMBNAIL_CLIENT_ONLY";

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OverlayWindowPlacement {
    rect: Option<Rect>,
    visible: bool,
    topmost: bool,
    child_insert_after: Option<isize>,
}

pub(crate) fn install_overview_control_sender(control_sender: Sender<ControlMessage>) {
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

enum OverlayCommand {
    Show(OverviewScene, Sender<Result<(), String>>),
    Hide(Sender<Result<(), String>>),
    LowerForShellOverlay(Sender<Result<(), String>>),
    Shutdown,
}

pub(crate) struct OverviewOverlay {
    sender: Sender<OverlayCommand>,
    worker: Option<JoinHandle<()>>,
}

impl OverviewOverlay {
    pub(crate) fn spawn() -> Result<Self, OverviewSurfaceError> {
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

    pub(crate) fn show(&self, scene: OverviewScene) -> Result<(), OverviewSurfaceError> {
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

    pub(crate) fn hide(&self) -> Result<(), OverviewSurfaceError> {
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
    hwnd: HWND,
    thumbnails: HashMap<u64, PreviewThumbnailState>,
    last_thumbnail_failures: HashMap<u64, String>,
    last_thumbnail_diagnostics: HashMap<u64, String>,
    placement: OverlayWindowPlacement,
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
struct PreviewWindowState {
    click_targets: Vec<PreviewClickTarget>,
    workspace_targets: Vec<WorkspaceDropTarget>,
    drag_session: Option<PreviewDragSession>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviewDragSession {
    dragged_raw_hwnd: u64,
    origin_x: i32,
    origin_y: i32,
    moved: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewPointerOutcome {
    ActivateWindow(u64),
    MoveColumn {
        dragged_raw_hwnd: u64,
        target_workspace_id: WorkspaceId,
        insert_after_raw_hwnd: Option<u64>,
    },
    Dismiss,
    None,
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

#[derive(Default)]
struct OverviewSessionState {
    current_scene: Option<OverviewScene>,
    shell_escape: Option<ShellOverlayEscapeState>,
}

impl OverviewSessionState {
    fn current_scene(&self) -> Option<&OverviewScene> {
        self.current_scene.as_ref()
    }

    fn record_scene(&mut self, scene: OverviewScene) {
        self.current_scene = Some(scene);
    }

    fn take_scene(&mut self) -> Option<OverviewScene> {
        self.current_scene.take()
    }

    fn scene_matches(&self, scene: &OverviewScene) -> bool {
        self.current_scene.as_ref() == Some(scene)
    }

    fn shell_escape_active(&self) -> bool {
        self.shell_escape.is_some()
    }

    fn clear_shell_escape(&mut self) {
        self.shell_escape = None;
    }

    fn begin_shell_escape(
        &mut self,
        started_at: Instant,
        baseline_windows: ShellScreenshotWindows,
    ) {
        self.shell_escape = Some(ShellOverlayEscapeState::new(
            started_at,
            baseline_windows.result_window_hwnds,
            baseline_windows.foreground_screenshot_hwnd,
        ));
    }

    fn should_restore_shell_escape(&mut self) -> bool {
        let Some(state) = self.shell_escape.as_mut() else {
            return false;
        };
        if self.current_scene.is_none() {
            return false;
        }

        state.should_restore(&shell_screenshot_windows(), Instant::now())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ShellScreenshotWindows {
    overlay_present: bool,
    result_window_hwnds: HashSet<u64>,
    foreground_screenshot_hwnd: Option<u64>,
}

include!("overview_surface/windowing.rs");
include!("overview_surface/backdrop.rs");
include!("overview_surface/common.rs");
include!("overview_surface/worker.rs");
include!("overview_surface/shell.rs");
include!("overview_surface/preview.rs");
include!("overview_surface/thumbnail.rs");

#[cfg(test)]
#[path = "overview_surface/tests.rs"]
mod tests;
