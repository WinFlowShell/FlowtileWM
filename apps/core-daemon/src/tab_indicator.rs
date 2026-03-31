use std::{
    collections::HashMap,
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};

use flowtile_domain::{ColumnMode, Rect, WindowId, WmState};
use flowtile_layout_engine::{LayoutError, recompute_workspace};
use flowtile_wm_core::CoreDaemonRuntime;

use crate::win32_overlay::{
    create_layered_overlay_window, hide_overlay_window, pump_messages, register_basic_window_class,
    rgb_color, show_overlay_window,
};

#[cfg(not(windows))]
compile_error!("flowtile-core-daemon tab indicator surface currently supports only Windows.");

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND},
    Graphics::Gdi::{CreateSolidBrush, DeleteObject, HBRUSH},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{DestroyWindow, SW_SHOWNA},
};

const THREAD_SLICE: Duration = Duration::from_millis(16);
const ACTIVE_CLASS: &str = "FlowtileTabIndicatorActive";
const INACTIVE_CLASS: &str = "FlowtileTabIndicatorInactive";
const INDICATOR_ALPHA: u8 = 232;
const INDICATOR_WIDTH_PX: u32 = 6;
const INDICATOR_SIDE_INSET_PX: i32 = 10;
const INDICATOR_GAP_PX: i32 = 4;
const MIN_SEGMENT_HEIGHT_PX: i32 = 6;

#[derive(Debug)]
pub(crate) enum TabIndicatorError {
    Layout(LayoutError),
    Platform(String),
}

impl std::fmt::Display for TabIndicatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(source) => write!(formatter, "{source:?}"),
            Self::Platform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TabIndicatorError {}

impl From<LayoutError> for TabIndicatorError {
    fn from(value: LayoutError) -> Self {
        Self::Layout(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TabIndicatorKey {
    state_version: u64,
    management_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IndicatorKind {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TabIndicatorSegment {
    rect: Rect,
    kind: IndicatorKind,
}

pub(crate) struct TabIndicatorController {
    overlay: TabIndicatorOverlay,
    last_key: Option<TabIndicatorKey>,
}

impl TabIndicatorController {
    pub(crate) fn spawn() -> Result<Self, TabIndicatorError> {
        Ok(Self {
            overlay: TabIndicatorOverlay::spawn()?,
            last_key: None,
        })
    }

    pub(crate) fn sync(&mut self, runtime: &CoreDaemonRuntime) -> Result<(), TabIndicatorError> {
        let key = TabIndicatorKey {
            state_version: runtime.state().state_version().get(),
            management_enabled: runtime.management_enabled(),
        };
        if self.last_key == Some(key) {
            return Ok(());
        }

        let segments = if runtime.management_enabled() {
            build_tab_indicator_segments(runtime.state())?
        } else {
            Vec::new()
        };
        self.overlay.sync(segments)?;
        self.last_key = Some(key);
        Ok(())
    }
}

fn build_tab_indicator_segments(state: &WmState) -> Result<Vec<TabIndicatorSegment>, LayoutError> {
    if state.overview.is_open {
        return Ok(Vec::new());
    }

    let mut segments = Vec::new();
    for monitor in state.monitors.values() {
        let Some(workspace_id) = state.active_workspace_id_for_monitor(monitor.id) else {
            continue;
        };
        let Some(workspace) = state.workspaces.get(&workspace_id) else {
            continue;
        };
        let projection = recompute_workspace(state, workspace_id)?;
        let geometry_by_window_id = projection
            .window_geometries
            .iter()
            .map(|geometry| (geometry.window_id, geometry.rect))
            .collect::<HashMap<_, _>>();

        for column_id in &workspace.strip.ordered_column_ids {
            let Some(column) = state.layout.columns.get(column_id) else {
                continue;
            };
            if column.mode != ColumnMode::Tabbed || column.ordered_window_ids.len() < 2 {
                continue;
            }

            let selected_window_id = column
                .tab_selection
                .or(column.active_window_id)
                .or_else(|| column.ordered_window_ids.first().copied());
            let Some(selected_window_id) = selected_window_id else {
                continue;
            };
            let Some(column_rect) = geometry_by_window_id.get(&selected_window_id).copied() else {
                continue;
            };
            segments.extend(build_column_segments(
                column_rect,
                &column.ordered_window_ids,
                selected_window_id,
            ));
        }
    }

    Ok(segments)
}

fn build_column_segments(
    column_rect: Rect,
    ordered_window_ids: &[WindowId],
    selected_window_id: WindowId,
) -> Vec<TabIndicatorSegment> {
    let count = ordered_window_ids.len();
    if count < 2 {
        return Vec::new();
    }

    let total_height = i32::try_from(column_rect.height).unwrap_or(i32::MAX);
    let gap = compute_gap(total_height, count);
    let available_height = total_height.saturating_sub(gap.saturating_mul((count - 1) as i32));
    let base_height = available_height
        .checked_div(count as i32)
        .unwrap_or(MIN_SEGMENT_HEIGHT_PX)
        .max(MIN_SEGMENT_HEIGHT_PX);
    let remainder = available_height.saturating_sub(base_height.saturating_mul(count as i32));
    let indicator_width = INDICATOR_WIDTH_PX.max(1);
    let x = column_rect
        .x
        .saturating_add(column_rect.width.min(i32::MAX as u32) as i32)
        .saturating_sub(INDICATOR_SIDE_INSET_PX)
        .saturating_sub(indicator_width.min(i32::MAX as u32) as i32);

    let mut y = column_rect.y;
    let mut segments = Vec::with_capacity(count);
    for (index, window_id) in ordered_window_ids.iter().copied().enumerate() {
        let mut height = base_height;
        if (index as i32) < remainder {
            height = height.saturating_add(1);
        }

        segments.push(TabIndicatorSegment {
            rect: Rect::new(x, y, indicator_width, height.max(1) as u32),
            kind: if window_id == selected_window_id {
                IndicatorKind::Active
            } else {
                IndicatorKind::Inactive
            },
        });
        y = y.saturating_add(height).saturating_add(gap);
    }

    segments
}

fn compute_gap(total_height: i32, count: usize) -> i32 {
    if count <= 1 {
        return 0;
    }

    let desired_gap = INDICATOR_GAP_PX.max(1);
    let minimum_required = MIN_SEGMENT_HEIGHT_PX.saturating_mul(count as i32);
    let remaining = total_height.saturating_sub(minimum_required);
    if remaining <= 0 {
        return 1;
    }

    desired_gap.min(remaining / (count.saturating_sub(1) as i32).max(1))
}

enum OverlayCommand {
    Sync(Vec<TabIndicatorSegment>, Sender<Result<(), String>>),
    Shutdown,
}

struct TabIndicatorOverlay {
    sender: Sender<OverlayCommand>,
    worker: Option<JoinHandle<()>>,
}

impl TabIndicatorOverlay {
    fn spawn() -> Result<Self, TabIndicatorError> {
        let (command_sender, command_receiver) = mpsc::channel::<OverlayCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        let worker = thread::spawn(move || run_overlay_thread(command_receiver, startup_sender));
        startup_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| {
                TabIndicatorError::Platform(format!("tab indicator startup timed out: {error}"))
            })?
            .map_err(TabIndicatorError::Platform)?;

        Ok(Self {
            sender: command_sender,
            worker: Some(worker),
        })
    }

    fn sync(&self, segments: Vec<TabIndicatorSegment>) -> Result<(), TabIndicatorError> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(OverlayCommand::Sync(segments, response_sender))
            .map_err(|_| {
                TabIndicatorError::Platform(
                    "tab indicator worker is no longer available".to_string(),
                )
            })?;
        response_receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| {
                TabIndicatorError::Platform(format!("tab indicator response timed out: {error}"))
            })?
            .map_err(TabIndicatorError::Platform)
    }
}

impl Drop for TabIndicatorOverlay {
    fn drop(&mut self) {
        let _ = self.sender.send(OverlayCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct IndicatorWindow {
    hwnd: HWND,
    kind: IndicatorKind,
}

struct IndicatorClasses {
    instance: HINSTANCE,
    active_brush: HBRUSH,
    inactive_brush: HBRUSH,
}

fn run_overlay_thread(
    command_receiver: Receiver<OverlayCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_indicator_classes() {
        Ok(classes) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_overlay_loop(command_receiver, &classes);
            let _ = {
                // SAFETY: brushes are deleted after all overlay windows have been destroyed.
                unsafe { DeleteObject(classes.active_brush as _) }
            };
            let _ = {
                // SAFETY: brushes are deleted after all overlay windows have been destroyed.
                unsafe { DeleteObject(classes.inactive_brush as _) }
            };
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_indicator_classes() -> Result<IndicatorClasses, String> {
    let instance = {
        // SAFETY: querying the current module handle is required for class registration.
        unsafe { GetModuleHandleW(null()) }
    };
    let active_brush = {
        // SAFETY: GDI brush creation is synchronous and uses a constant colorref.
        unsafe { CreateSolidBrush(rgb_color(0x4C, 0xA8, 0xFF)) }
    };
    let inactive_brush = {
        // SAFETY: GDI brush creation is synchronous and uses a constant colorref.
        unsafe { CreateSolidBrush(rgb_color(0x68, 0x70, 0x7A)) }
    };
    if active_brush.is_null() || inactive_brush.is_null() {
        return Err("CreateSolidBrush failed for tab indicator classes".to_string());
    }

    register_basic_window_class(instance as HINSTANCE, ACTIVE_CLASS, active_brush)?;
    register_basic_window_class(instance as HINSTANCE, INACTIVE_CLASS, inactive_brush)?;

    Ok(IndicatorClasses {
        instance: instance as HINSTANCE,
        active_brush,
        inactive_brush,
    })
}

fn run_overlay_loop(
    command_receiver: Receiver<OverlayCommand>,
    classes: &IndicatorClasses,
) -> Result<(), String> {
    let mut windows = Vec::<IndicatorWindow>::new();
    loop {
        pump_messages()?;
        match command_receiver.recv_timeout(THREAD_SLICE) {
            Ok(OverlayCommand::Sync(segments, response)) => {
                let result = sync_indicator_windows(&mut windows, segments, classes);
                let _ = response.send(result);
            }
            Ok(OverlayCommand::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    for indicator in windows {
        let _ = {
            // SAFETY: paired with successful window creation on this thread.
            unsafe { DestroyWindow(indicator.hwnd) }
        };
    }

    Ok(())
}

fn sync_indicator_windows(
    windows: &mut Vec<IndicatorWindow>,
    segments: Vec<TabIndicatorSegment>,
    classes: &IndicatorClasses,
) -> Result<(), String> {
    while windows.len() < segments.len() {
        windows.push(IndicatorWindow {
            hwnd: null_mut(),
            kind: IndicatorKind::Inactive,
        });
    }

    for (index, segment) in segments.iter().copied().enumerate() {
        if windows[index].hwnd.is_null() || windows[index].kind != segment.kind {
            if !windows[index].hwnd.is_null() {
                let _ = {
                    // SAFETY: paired with successful creation on this thread.
                    unsafe { DestroyWindow(windows[index].hwnd) }
                };
            }
            windows[index] = IndicatorWindow {
                hwnd: create_indicator_window(classes.instance, segment.kind)?,
                kind: segment.kind,
            };
        }
        position_indicator_window(windows[index].hwnd, segment.rect)?;
    }

    for window in windows.iter().skip(segments.len()) {
        if !window.hwnd.is_null() {
            hide_overlay_window(window.hwnd);
        }
    }

    Ok(())
}

fn create_indicator_window(instance: HINSTANCE, kind: IndicatorKind) -> Result<HWND, String> {
    let class_name = match kind {
        IndicatorKind::Active => ACTIVE_CLASS,
        IndicatorKind::Inactive => INACTIVE_CLASS,
    };
    create_layered_overlay_window(instance, class_name, INDICATOR_ALPHA)
}

fn position_indicator_window(window: HWND, rect: Rect) -> Result<(), String> {
    show_overlay_window(window, rect, SW_SHOWNA)
}

#[cfg(test)]
mod tests {
    use flowtile_domain::{
        Column, ColumnMode, Rect, RuntimeMode, Size, WidthSemantics, WindowClassification,
        WindowLayer, WindowNode, WmState,
    };

    use super::{IndicatorKind, build_tab_indicator_segments};

    #[test]
    fn tab_indicator_segments_follow_tab_selection() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        let monitor_id = state.add_monitor(Rect::new(0, 0, 1200, 800), 96, true);
        let workspace_id = state
            .active_workspace_id_for_monitor(monitor_id)
            .expect("workspace should exist");
        let window_ids = [
            state.allocate_window_id(),
            state.allocate_window_id(),
            state.allocate_window_id(),
        ];
        let column_id = state.allocate_column_id();

        let mut column = Column::new(
            column_id,
            ColumnMode::Tabbed,
            WidthSemantics::Fixed(420),
            window_ids.to_vec(),
        );
        column.tab_selection = Some(window_ids[1]);
        state.layout.columns.insert(column_id, column);
        state
            .workspaces
            .get_mut(&workspace_id)
            .expect("workspace should exist")
            .strip
            .ordered_column_ids
            .push(column_id);

        for window_id in window_ids {
            state.windows.insert(
                window_id,
                WindowNode {
                    id: window_id,
                    current_hwnd_binding: Some(window_id.get()),
                    classification: WindowClassification::Application,
                    layer: WindowLayer::Tiled,
                    workspace_id,
                    column_id: Some(column_id),
                    is_managed: true,
                    is_floating: false,
                    is_fullscreen: false,
                    restore_target: None,
                    last_known_rect: Rect::new(0, 0, 420, 800),
                    desired_size: Size::new(420, 800),
                },
            );
        }

        let segments = build_tab_indicator_segments(&state).expect("segments should build");
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[1].kind, IndicatorKind::Active);
        assert_eq!(segments[0].kind, IndicatorKind::Inactive);
        assert_eq!(segments[2].kind, IndicatorKind::Inactive);
    }

    #[test]
    fn tab_indicator_is_hidden_while_overview_is_open() {
        let mut state = WmState::new(RuntimeMode::WmOnly);
        state.overview.is_open = true;

        let segments = build_tab_indicator_segments(&state).expect("segments should build");
        assert!(segments.is_empty());
    }
}
