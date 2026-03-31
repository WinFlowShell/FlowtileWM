use std::{
    collections::HashMap,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use flowtile_domain::{Rect, ResizeEdge};
use flowtile_wm_core::{
    ActiveTiledResizeTarget, CoreDaemonRuntime, RuntimeCycleReport, RuntimeError,
};

use crate::{
    hotkeys::is_super_held_by_low_level_runtime,
    win32_overlay::{
        create_layered_overlay_window, hide_overlay_window, last_error_message, pump_messages,
        register_basic_window_class, show_overlay_window,
    },
};

#[cfg(not(windows))]
compile_error!(
    "flowtile-core-daemon manual resize runtime currently supports only Windows builds."
);

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, POINT},
    Graphics::Gdi::{CreateSolidBrush, DeleteObject, HBRUSH},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        CallNextHookEx, DestroyWindow, GetCursorPos, GetMessageW, GetWindowRect, MSG,
        MSLLHOOKSTRUCT, PostThreadMessageW, SW_SHOW, SetWindowsHookExW, UnhookWindowsHookEx,
        WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_QUIT,
    },
};

const EDGE_GRAB_TOLERANCE_PX: i32 = 20;
const PREVIEW_ALPHA: u8 = 96;
const PREVIEW_THREAD_SLICE: Duration = Duration::from_millis(16);
const PREVIEW_WINDOW_CLASS: &str = "FlowtileColumnWidthPreview";

static MOUSE_HOOK_RUNTIMES: OnceLock<Mutex<HashMap<u32, Arc<Mutex<MouseHookState>>>>> =
    OnceLock::new();

#[derive(Debug)]
pub(crate) enum ManualResizeError {
    Runtime(RuntimeError),
    Platform(String),
}

impl std::fmt::Display for ManualResizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(source) => write!(formatter, "{source:?}"),
            Self::Platform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ManualResizeError {}

impl From<RuntimeError> for ManualResizeError {
    fn from(value: RuntimeError) -> Self {
        Self::Runtime(value)
    }
}

pub(crate) struct ManualResizeController {
    overlay: ResizePreviewOverlay,
    mouse_hook: ResizeMouseHook,
    active: bool,
}

impl ManualResizeController {
    pub(crate) fn spawn() -> Result<Self, ManualResizeError> {
        Ok(Self {
            overlay: ResizePreviewOverlay::spawn()?,
            mouse_hook: ResizeMouseHook::spawn()?,
            active: false,
        })
    }

    pub(crate) fn tick(
        &mut self,
        runtime: &mut CoreDaemonRuntime,
        dry_run: bool,
    ) -> Result<Option<RuntimeCycleReport>, ManualResizeError> {
        if !self.active {
            self.mouse_hook.set_target_rect(
                runtime
                    .active_tiled_resize_target()?
                    .map(resolve_resize_hit_rect),
            )?;
        }

        while let Some(event) = self.mouse_hook.try_recv()? {
            match event {
                MouseHookEvent::BeginDrag { edge, pointer_x } => {
                    if !self.active && runtime.begin_column_width_resize(edge, pointer_x)? {
                        self.active = true;
                        self.sync_preview(runtime)?;
                    }
                }
                MouseHookEvent::Release { pointer_x } => {
                    if self.active {
                        let report = runtime.commit_column_width_resize(pointer_x, dry_run)?;
                        self.active = false;
                        self.overlay.hide()?;
                        self.mouse_hook.set_target_rect(
                            runtime
                                .active_tiled_resize_target()?
                                .map(resolve_resize_hit_rect),
                        )?;
                        return Ok(Some(report));
                    }
                }
            }
        }

        if self.active {
            if !is_super_down() {
                runtime.cancel_column_width_resize()?;
                self.active = false;
                self.overlay.hide()?;
                self.mouse_hook.set_target_rect(
                    runtime
                        .active_tiled_resize_target()?
                        .map(resolve_resize_hit_rect),
                )?;
                return Ok(None);
            }

            let pointer = current_pointer_position()?;
            runtime.update_column_width_resize(pointer.x)?;
            self.sync_preview(runtime)?;
            return Ok(None);
        }

        self.overlay.hide()?;
        Ok(None)
    }

    fn sync_preview(&mut self, runtime: &CoreDaemonRuntime) -> Result<(), ManualResizeError> {
        if let Some(preview_rect) = runtime.manual_width_resize_preview_rect() {
            self.overlay.show(preview_rect)?;
        } else {
            self.overlay.hide()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerPosition {
    x: i32,
    y: i32,
}

fn detect_resize_edge(rect: Rect, pointer: PointerPosition) -> Option<ResizeEdge> {
    let top = rect.y;
    let bottom = rect.y.saturating_add(rect.height as i32);
    if pointer.y < top || pointer.y > bottom {
        return None;
    }

    let left_distance = pointer.x.saturating_sub(rect.x).abs();
    let right_x = rect.x.saturating_add(rect.width as i32);
    let right_distance = pointer.x.saturating_sub(right_x).abs();
    let near_left = left_distance <= EDGE_GRAB_TOLERANCE_PX;
    let near_right = right_distance <= EDGE_GRAB_TOLERANCE_PX;

    match (near_left, near_right) {
        (true, true) if left_distance <= right_distance => Some(ResizeEdge::Left),
        (true, true) => Some(ResizeEdge::Right),
        (true, false) => Some(ResizeEdge::Left),
        (false, true) => Some(ResizeEdge::Right),
        (false, false) => None,
    }
}

fn resolve_resize_hit_rect(target: ActiveTiledResizeTarget) -> Rect {
    target
        .hwnd
        .and_then(query_outer_window_rect)
        .unwrap_or(target.rect)
}

fn is_super_down() -> bool {
    is_super_held_by_low_level_runtime()
}

fn current_pointer_position() -> Result<PointerPosition, ManualResizeError> {
    let mut point: POINT = {
        // SAFETY: `POINT` is a plain Win32 structure and valid when zero-initialized.
        unsafe { zeroed() }
    };
    let ok = {
        // SAFETY: `point` points to writable memory for the synchronous Win32 API call.
        unsafe { GetCursorPos(&mut point) }
    };
    if ok == 0 {
        return Err(ManualResizeError::Platform(last_error_message(
            "GetCursorPos",
        )));
    }

    Ok(PointerPosition {
        x: point.x,
        y: point.y,
    })
}

fn query_outer_window_rect(raw_hwnd: u64) -> Option<Rect> {
    let hwnd = isize::try_from(raw_hwnd).ok()? as HWND;
    let mut rect = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = {
        // SAFETY: `rect` points to writable memory for a synchronous read-only query on a tracked HWND.
        unsafe { GetWindowRect(hwnd, &mut rect) }
    };
    if ok == 0 || rect.right <= rect.left || rect.bottom <= rect.top {
        return None;
    }

    Some(Rect::new(
        rect.left,
        rect.top,
        (rect.right - rect.left) as u32,
        (rect.bottom - rect.top) as u32,
    ))
}

enum OverlayCommand {
    Show(Rect),
    Hide,
    Shutdown,
}

struct ResizePreviewOverlay {
    sender: Sender<OverlayCommand>,
    worker: Option<JoinHandle<()>>,
}

impl ResizePreviewOverlay {
    fn spawn() -> Result<Self, ManualResizeError> {
        let (command_sender, command_receiver) = mpsc::channel::<OverlayCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<u32, String>>();
        let worker =
            thread::spawn(move || run_preview_overlay_thread(command_receiver, startup_sender));
        let thread_id = startup_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| {
                ManualResizeError::Platform(format!(
                    "column-width preview overlay startup timed out: {error}"
                ))
            })?
            .map_err(ManualResizeError::Platform)?;

        if thread_id == 0 {
            return Err(ManualResizeError::Platform(
                "column-width preview overlay did not provide a thread id".to_string(),
            ));
        }

        Ok(Self {
            sender: command_sender,
            worker: Some(worker),
        })
    }

    fn show(&mut self, rect: Rect) -> Result<(), ManualResizeError> {
        self.sender.send(OverlayCommand::Show(rect)).map_err(|_| {
            ManualResizeError::Platform(
                "column-width preview overlay is no longer available".to_string(),
            )
        })
    }

    fn hide(&mut self) -> Result<(), ManualResizeError> {
        self.sender.send(OverlayCommand::Hide).map_err(|_| {
            ManualResizeError::Platform(
                "column-width preview overlay is no longer available".to_string(),
            )
        })
    }
}

impl Drop for ResizePreviewOverlay {
    fn drop(&mut self) {
        let _ = self.sender.send(OverlayCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_preview_overlay_thread(
    command_receiver: Receiver<OverlayCommand>,
    startup_sender: Sender<Result<u32, String>>,
) {
    let thread_id = {
        // SAFETY: `GetCurrentThreadId` reads the current worker thread id without side effects.
        unsafe { GetCurrentThreadId() }
    };
    match initialize_preview_overlay() {
        Ok((window, brush)) => {
            let _ = startup_sender.send(Ok(thread_id));
            let _ = run_preview_overlay_loop(command_receiver, window);
            let _ = {
                // SAFETY: destroying the window is paired with its successful creation on this thread.
                unsafe { DestroyWindow(window) }
            };
            let _ = {
                // SAFETY: deleting the brush releases the GDI resource created above.
                unsafe { DeleteObject(brush as _) }
            };
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_preview_overlay() -> Result<(HWND, HBRUSH), String> {
    let instance = {
        // SAFETY: we query the current module handle for class registration and window creation.
        unsafe { GetModuleHandleW(null()) }
    };
    let brush = {
        // SAFETY: creating a solid brush with a constant RGB color is a synchronous GDI call.
        unsafe { CreateSolidBrush(0x00F2A34A) }
    };
    if brush.is_null() {
        return Err(last_error_message("CreateSolidBrush"));
    }

    if let Err(error) =
        register_basic_window_class(instance as HINSTANCE, PREVIEW_WINDOW_CLASS, brush as HBRUSH)
    {
        let _ = unsafe { DeleteObject(brush as _) };
        return Err(error);
    }

    let window =
        create_layered_overlay_window(instance as HINSTANCE, PREVIEW_WINDOW_CLASS, PREVIEW_ALPHA)?;
    hide_overlay_window(window);
    Ok((window, brush))
}

fn run_preview_overlay_loop(
    command_receiver: Receiver<OverlayCommand>,
    window: HWND,
) -> Result<(), String> {
    loop {
        pump_messages()?;

        match command_receiver.recv_timeout(PREVIEW_THREAD_SLICE) {
            Ok(OverlayCommand::Show(rect)) => show_overlay_window(window, rect, SW_SHOW)?,
            Ok(OverlayCommand::Hide) => hide_overlay_window(window),
            Ok(OverlayCommand::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseHookEvent {
    BeginDrag { edge: ResizeEdge, pointer_x: i32 },
    Release { pointer_x: i32 },
}

struct ResizeMouseHook {
    thread_id: u32,
    event_receiver: Receiver<MouseHookEvent>,
    shared_state: Arc<Mutex<MouseHookState>>,
    worker: Option<JoinHandle<()>>,
}

impl ResizeMouseHook {
    fn spawn() -> Result<Self, ManualResizeError> {
        let (event_sender, event_receiver) = mpsc::channel::<MouseHookEvent>();
        let shared_state = Arc::new(Mutex::new(MouseHookState {
            target_rect: None,
            drag_intercepted: false,
            event_sender,
        }));
        let worker_state = Arc::clone(&shared_state);
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<u32, String>>();
        let worker =
            thread::spawn(move || run_resize_mouse_hook_thread(worker_state, startup_sender));
        let thread_id = startup_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| {
                ManualResizeError::Platform(format!(
                    "manual resize mouse hook startup timed out: {error}"
                ))
            })?
            .map_err(ManualResizeError::Platform)?;

        Ok(Self {
            thread_id,
            event_receiver,
            shared_state,
            worker: Some(worker),
        })
    }

    fn set_target_rect(&self, rect: Option<Rect>) -> Result<(), ManualResizeError> {
        let mut state = self.shared_state.lock().map_err(|_| {
            ManualResizeError::Platform("manual resize mouse hook state is poisoned".to_string())
        })?;
        state.target_rect = rect;
        Ok(())
    }

    fn try_recv(&self) -> Result<Option<MouseHookEvent>, ManualResizeError> {
        match self.event_receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(ManualResizeError::Platform(
                "manual resize mouse hook disconnected".to_string(),
            )),
        }
    }
}

impl Drop for ResizeMouseHook {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct MouseHookState {
    target_rect: Option<Rect>,
    drag_intercepted: bool,
    event_sender: Sender<MouseHookEvent>,
}

fn mouse_hook_runtimes() -> &'static Mutex<HashMap<u32, Arc<Mutex<MouseHookState>>>> {
    MOUSE_HOOK_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_resize_mouse_hook_thread(
    state: Arc<Mutex<MouseHookState>>,
    startup_sender: Sender<Result<u32, String>>,
) {
    let thread_id = {
        // SAFETY: `GetCurrentThreadId` reads the current worker thread id without side effects.
        unsafe { GetCurrentThreadId() }
    };

    {
        let mut runtimes = match mouse_hook_runtimes().lock() {
            Ok(runtimes) => runtimes,
            Err(_) => {
                let _ = startup_sender.send(Err(
                    "manual resize mouse hook registry is poisoned".to_string()
                ));
                return;
            }
        };
        runtimes.insert(thread_id, state);
    }

    let module = unsafe { GetModuleHandleW(null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), module, 0) };
    if hook.is_null() {
        let _ = startup_sender.send(Err(last_error_message("SetWindowsHookExW")));
        unregister_mouse_hook_runtime(thread_id);
        return;
    }

    let _ = startup_sender.send(Ok(thread_id));
    let mut message: MSG = unsafe { zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if status <= 0 {
            break;
        }
    }

    let _ = unsafe { UnhookWindowsHookEx(hook) };
    unregister_mouse_hook_runtime(thread_id);
}

fn unregister_mouse_hook_runtime(thread_id: u32) {
    if let Ok(mut runtimes) = mouse_hook_runtimes().lock() {
        runtimes.remove(&thread_id);
    }
}

unsafe extern "system" fn low_level_mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code < 0 || lparam == 0 {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }

    let message = wparam as u32;
    if !matches!(
        message,
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP
    ) {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    let runtime = mouse_hook_runtimes()
        .lock()
        .ok()
        .and_then(|runtimes| runtimes.get(&thread_id).cloned());
    let Some(runtime) = runtime else {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    };

    let hook_data = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    let pointer = PointerPosition {
        x: hook_data.pt.x,
        y: hook_data.pt.y,
    };
    let Ok(mut state) = runtime.lock() else {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    };

    match message {
        WM_LBUTTONDOWN | WM_NCLBUTTONDOWN => {
            if !is_super_down() {
                return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
            }
            let Some(target_rect) = state.target_rect else {
                return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
            };
            let Some(edge) = detect_resize_edge(target_rect, pointer) else {
                return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
            };
            if state
                .event_sender
                .send(MouseHookEvent::BeginDrag {
                    edge,
                    pointer_x: pointer.x,
                })
                .is_ok()
            {
                state.drag_intercepted = true;
                return 1;
            }
        }
        WM_LBUTTONUP | WM_NCLBUTTONUP => {
            if state.drag_intercepted {
                state.drag_intercepted = false;
                let _ = state.event_sender.send(MouseHookEvent::Release {
                    pointer_x: pointer.x,
                });
                return 1;
            }
        }
        _ => {}
    }

    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}
