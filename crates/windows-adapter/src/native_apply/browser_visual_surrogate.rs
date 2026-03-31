use std::{
    collections::HashMap,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Dwm::{
        DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
        DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmRegisterThumbnail,
        DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetShellWindow,
        GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, RegisterClassW, SW_HIDE, SW_SHOWNA,
        SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW,
        SetWindowPos, ShowWindow, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_NCLBUTTONDOWN, WM_RBUTTONDOWN,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};

use super::{
    activate_window, hwnd_from_raw, is_valid_window, last_error_message, pump_overlay_messages,
    widestring,
};

const BROWSER_SURROGATE_CLASS: &str = "FlowTileBrowserVisualSurrogate";
const BROWSER_SURROGATE_THREAD_SLICE: Duration = Duration::from_millis(16);
const BROWSER_SURROGATE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_SURROGATE_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const WINDOW_CLASS_ALREADY_EXISTS: u32 = 1410;

#[derive(Clone, Copy, Debug)]
struct BrowserVisualSurrogate {
    window: HWND,
    backdrop_thumbnail_id: Option<isize>,
    browser_thumbnail_id: Option<isize>,
}

enum BrowserVisualSurrogateCommand {
    Show {
        owner_hwnd: u64,
        rect: flowtile_domain::Rect,
        alpha: u8,
        response: Sender<Result<(), String>>,
    },
    Hide {
        owner_hwnd: u64,
        response: Sender<Result<(), String>>,
    },
}

struct BrowserVisualSurrogateController {
    sender: Sender<BrowserVisualSurrogateCommand>,
}

static BROWSER_VISUAL_SURROGATE_CONTROLLER: OnceLock<BrowserVisualSurrogateController> =
    OnceLock::new();

impl BrowserVisualSurrogateController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<BrowserVisualSurrogateCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            run_browser_visual_surrogate_thread(command_receiver, startup_sender)
        });
        startup_receiver
            .recv_timeout(BROWSER_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(&self, owner_hwnd: u64, rect: flowtile_domain::Rect, alpha: u8) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserVisualSurrogateCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response: response_sender,
            })
            .map_err(|_| "browser visual surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserVisualSurrogateCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "browser visual surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser visual surrogate response timed out: {error}"))?
    }
}

pub(crate) fn show_browser_visual_surrogate(
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    browser_visual_surrogate_controller()?.show(owner_hwnd, rect, alpha)
}

pub(crate) fn hide_browser_visual_surrogate_if_initialized(raw_hwnd: u64) -> Result<(), String> {
    browser_visual_surrogate_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(raw_hwnd))
}

fn browser_visual_surrogate_controller() -> Result<&'static BrowserVisualSurrogateController, String>
{
    if let Some(controller) = BROWSER_VISUAL_SURROGATE_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = BrowserVisualSurrogateController::spawn()?;
    let _ = BROWSER_VISUAL_SURROGATE_CONTROLLER.set(controller);
    BROWSER_VISUAL_SURROGATE_CONTROLLER
        .get()
        .ok_or_else(|| "browser visual surrogate controller did not initialize".to_string())
}

fn browser_visual_surrogate_controller_if_initialized()
-> Option<&'static BrowserVisualSurrogateController> {
    BROWSER_VISUAL_SURROGATE_CONTROLLER.get()
}

fn run_browser_visual_surrogate_thread(
    command_receiver: Receiver<BrowserVisualSurrogateCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_browser_visual_surrogate_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_browser_visual_surrogate_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_browser_visual_surrogate_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(BROWSER_SURROGATE_CLASS);
    let instance = { unsafe { GetModuleHandleW(null()) } };
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(browser_visual_surrogate_window_proc),
        hInstance: instance as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        hbrBackground: null_mut(),
        ..unsafe { zeroed() }
    };
    let class_atom = { unsafe { RegisterClassW(&window_class) } };
    if class_atom == 0 {
        let error = { unsafe { GetLastError() } };
        if error != WINDOW_CLASS_ALREADY_EXISTS {
            return Err(last_error_message("RegisterClassW"));
        }
    }

    Ok(instance as HINSTANCE)
}

fn run_browser_visual_surrogate_loop(
    command_receiver: Receiver<BrowserVisualSurrogateCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut surrogates = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_browser_visual_surrogates(&mut surrogates);

        match command_receiver.recv_timeout(BROWSER_SURROGATE_THREAD_SLICE) {
            Ok(BrowserVisualSurrogateCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response,
            }) => {
                let result = show_browser_visual_surrogate_internal(
                    &mut surrogates,
                    instance,
                    owner_hwnd,
                    rect,
                    alpha,
                );
                let _ = response.send(result);
            }
            Ok(BrowserVisualSurrogateCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_browser_visual_surrogate_internal(&mut surrogates, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_browser_visual_surrogates(&mut surrogates);
    Ok(())
}

fn show_browser_visual_surrogate_internal(
    surrogates: &mut HashMap<u64, BrowserVisualSurrogate>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let Ok(owner) = hwnd_from_raw(owner_hwnd) else {
        return hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
    };
    if !is_valid_window(owner) {
        return hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
    }

    let surrogate = match surrogates.get(&owner_hwnd).copied() {
        Some(existing) if is_valid_window(existing.window) => existing,
        Some(existing) => {
            let _ = destroy_browser_visual_surrogate(existing);
            let surrogate = create_browser_visual_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
        None => {
            let surrogate = create_browser_visual_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
    };

    let mut surrogate = surrogate;
    if let Err(error) = prepare_browser_visual_surrogate(&mut surrogate, owner) {
        let _ = hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
        return Err(error);
    }
    if let Err(error) =
        show_browser_visual_surrogate_window(surrogate.window, rect, alpha, surrogate)
    {
        let _ = hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
        return Err(error);
    }
    surrogates.insert(owner_hwnd, surrogate);
    Ok(())
}

fn hide_browser_visual_surrogate_internal(
    surrogates: &mut HashMap<u64, BrowserVisualSurrogate>,
    owner_hwnd: u64,
) -> Result<(), String> {
    if let Some(surrogate) = surrogates.remove(&owner_hwnd) {
        return destroy_browser_visual_surrogate(surrogate);
    }

    Ok(())
}

fn prune_stale_browser_visual_surrogates(surrogates: &mut HashMap<u64, BrowserVisualSurrogate>) {
    let stale_owners = surrogates
        .iter()
        .filter_map(|(owner_hwnd, surrogate)| {
            let owner = hwnd_from_raw(*owner_hwnd).ok();
            (owner.is_none()
                || owner.is_some_and(|hwnd| !is_valid_window(hwnd))
                || !is_valid_window(surrogate.window))
            .then_some(*owner_hwnd)
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        let _ = hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
    }
}

fn destroy_all_browser_visual_surrogates(surrogates: &mut HashMap<u64, BrowserVisualSurrogate>) {
    let owner_hwnds = surrogates.keys().copied().collect::<Vec<_>>();
    for owner_hwnd in owner_hwnds {
        let _ = hide_browser_visual_surrogate_internal(surrogates, owner_hwnd);
    }
}

fn create_browser_visual_surrogate(
    instance: HINSTANCE,
    owner: HWND,
) -> Result<BrowserVisualSurrogate, String> {
    let class_name = widestring(BROWSER_SURROGATE_CLASS);
    let window = {
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
                owner,
                null_mut(),
                instance,
                null_mut(),
            )
        }
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }
    let _ = { unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, owner as isize) } };

    Ok(BrowserVisualSurrogate {
        window,
        backdrop_thumbnail_id: None,
        browser_thumbnail_id: None,
    })
}

fn prepare_browser_visual_surrogate(
    surrogate: &mut BrowserVisualSurrogate,
    owner: HWND,
) -> Result<(), String> {
    if surrogate.backdrop_thumbnail_id.is_none() {
        let backdrop_source = browser_visual_surrogate_backdrop_source()?;
        surrogate.backdrop_thumbnail_id = Some(register_browser_visual_surrogate_thumbnail(
            surrogate.window,
            backdrop_source,
        )?);
    }

    if surrogate.browser_thumbnail_id.is_none() {
        surrogate.browser_thumbnail_id = Some(register_browser_visual_surrogate_thumbnail(
            surrogate.window,
            owner,
        )?);
    }

    Ok(())
}

fn register_browser_visual_surrogate_thumbnail(
    destination: HWND,
    source: HWND,
) -> Result<isize, String> {
    let mut thumbnail_id = 0_isize;
    let result = { unsafe { DwmRegisterThumbnail(destination, source, &mut thumbnail_id) } };
    if result < 0 {
        return Err(format!(
            "DwmRegisterThumbnail failed with HRESULT {result:#x}"
        ));
    }

    Ok(thumbnail_id)
}

fn browser_visual_surrogate_backdrop_source() -> Result<HWND, String> {
    let shell_window = { unsafe { GetShellWindow() } };
    if !is_valid_window(shell_window) {
        return Err("GetShellWindow returned no valid shell backdrop source".to_string());
    }

    Ok(shell_window)
}

pub(crate) fn source_relative_rect(
    source_bounds: RECT,
    screen_rect: flowtile_domain::Rect,
    label: &str,
) -> Result<RECT, String> {
    let width = i32::try_from(screen_rect.width.max(1))
        .map_err(|_| format!("browser visual surrogate {label} width exceeds Win32 limits"))?;
    let height = i32::try_from(screen_rect.height.max(1))
        .map_err(|_| format!("browser visual surrogate {label} height exceeds Win32 limits"))?;
    let right = screen_rect
        .x
        .checked_add(width)
        .ok_or_else(|| format!("browser visual surrogate {label} right edge overflowed"))?;
    let bottom = screen_rect
        .y
        .checked_add(height)
        .ok_or_else(|| format!("browser visual surrogate {label} bottom edge overflowed"))?;

    Ok(RECT {
        left: screen_rect.x - source_bounds.left,
        top: screen_rect.y - source_bounds.top,
        right: right - source_bounds.left,
        bottom: bottom - source_bounds.top,
    })
}

fn browser_visual_surrogate_backdrop_source_rect(
    screen_rect: flowtile_domain::Rect,
) -> Result<RECT, String> {
    let backdrop_source = browser_visual_surrogate_backdrop_source()?;
    let mut source_bounds: RECT = unsafe { zeroed() };
    let has_bounds = { unsafe { GetWindowRect(backdrop_source, &mut source_bounds) } };
    if has_bounds == 0 {
        return Err(last_error_message("GetWindowRect"));
    }

    source_relative_rect(source_bounds, screen_rect, "backdrop")
}

fn show_browser_visual_surrogate_window(
    window: HWND,
    rect: flowtile_domain::Rect,
    alpha: u8,
    surrogate: BrowserVisualSurrogate,
) -> Result<(), String> {
    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "browser visual surrogate width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "browser visual surrogate height exceeds Win32 limits".to_string())?;
    let applied = {
        unsafe {
            SetWindowPos(
                window,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                width,
                height,
                BROWSER_SURROGATE_APPLY_FLAGS,
            )
        }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    let backdrop_thumbnail_id = surrogate.backdrop_thumbnail_id.ok_or_else(|| {
        "browser visual surrogate backdrop thumbnail was not initialized".to_string()
    })?;
    let backdrop_source_rect = browser_visual_surrogate_backdrop_source_rect(rect)?;
    let backdrop_thumbnail_properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_RECTSOURCE | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rcSource: backdrop_source_rect,
        opacity: u8::MAX,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let backdrop_result = {
        unsafe {
            DwmUpdateThumbnailProperties(backdrop_thumbnail_id, &backdrop_thumbnail_properties)
        }
    };
    if backdrop_result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties(backdrop) failed with HRESULT {backdrop_result:#x}"
        ));
    }

    let browser_thumbnail_id = surrogate.browser_thumbnail_id.ok_or_else(|| {
        "browser visual surrogate browser thumbnail was not initialized".to_string()
    })?;
    let browser_thumbnail_properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rcSource: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        opacity: alpha,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let result = {
        unsafe { DwmUpdateThumbnailProperties(browser_thumbnail_id, &browser_thumbnail_properties) }
    };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }

    let _ = { unsafe { ShowWindow(window, SW_SHOWNA) } };
    Ok(())
}

fn destroy_browser_visual_surrogate(surrogate: BrowserVisualSurrogate) -> Result<(), String> {
    let mut first_error = None;

    if let Some(thumbnail_id) = surrogate.backdrop_thumbnail_id {
        let result = { unsafe { DwmUnregisterThumbnail(thumbnail_id) } };
        if result < 0 {
            first_error = Some(format!(
                "DwmUnregisterThumbnail failed with HRESULT {result:#x}"
            ));
        }
    }

    if let Some(thumbnail_id) = surrogate.browser_thumbnail_id {
        let result = { unsafe { DwmUnregisterThumbnail(thumbnail_id) } };
        if result < 0 && first_error.is_none() {
            first_error = Some(format!(
                "DwmUnregisterThumbnail(browser) failed with HRESULT {result:#x}"
            ));
        }
    }

    destroy_browser_visual_surrogate_window(surrogate.window);

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn destroy_browser_visual_surrogate_window(window: HWND) {
    if !is_valid_window(window) {
        return;
    }

    let _ = { unsafe { ShowWindow(window, SW_HIDE) } };
    let _ = { unsafe { DestroyWindow(window) } };
}

unsafe extern "system" fn browser_visual_surrogate_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN => {
            let owner = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as HWND };
            if is_valid_window(owner) {
                let _ = { unsafe { ShowWindow(hwnd, SW_HIDE) } };
                let _ = { unsafe { DestroyWindow(hwnd) } };
                let _ = activate_window(owner as usize as u64);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
