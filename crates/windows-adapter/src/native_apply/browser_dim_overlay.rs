use std::{
    collections::BTreeSet,
    collections::HashMap,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND},
    Graphics::Gdi::{CreateSolidBrush, HBRUSH},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, LWA_ALPHA, RegisterClassW,
        SW_HIDE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW,
        SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

use super::{
    hwnd_from_raw, is_valid_window, last_error_message, pump_overlay_messages, widestring,
};

const BROWSER_DIM_OVERLAY_CLASS: &str = "FlowTileBrowserDimOverlay";
const BROWSER_DIM_OVERLAY_THREAD_SLICE: Duration = Duration::from_millis(16);
const BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_DIM_OVERLAY_COLOR_RGB: u32 = 0x000000;
const BROWSER_DIM_OVERLAY_APPLY_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const WINDOW_CLASS_ALREADY_EXISTS: u32 = 1410;

enum BrowserDimOverlayCommand {
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

struct BrowserDimOverlayController {
    sender: Sender<BrowserDimOverlayCommand>,
}

static BROWSER_DIM_OVERLAY_CONTROLLER: OnceLock<BrowserDimOverlayController> = OnceLock::new();

fn browser_dim_overlay_active_owners() -> &'static Mutex<BTreeSet<u64>> {
    static ACTIVE_OWNERS: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();
    ACTIVE_OWNERS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(crate) fn active_browser_dim_overlay_owner_hwnds_snapshot() -> BTreeSet<u64> {
    browser_dim_overlay_active_owners()
        .lock()
        .expect("browser dim overlay active owners mutex poisoned")
        .clone()
}

impl BrowserDimOverlayController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<BrowserDimOverlayCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || run_browser_dim_overlay_thread(command_receiver, startup_sender));
        startup_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(&self, owner_hwnd: u64, rect: flowtile_domain::Rect, alpha: u8) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserDimOverlayCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response: response_sender,
            })
            .map_err(|_| "browser dim overlay worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(BrowserDimOverlayCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "browser dim overlay worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(BROWSER_DIM_OVERLAY_RESPONSE_TIMEOUT)
            .map_err(|error| format!("browser dim overlay response timed out: {error}"))?
    }
}

pub(crate) fn show_browser_dim_overlay(
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    browser_dim_overlay_controller()?.show(owner_hwnd, rect, alpha)
}

pub(crate) fn hide_browser_dim_overlay_if_initialized(raw_hwnd: u64) -> Result<(), String> {
    browser_dim_overlay_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(raw_hwnd))
}

fn browser_dim_overlay_controller() -> Result<&'static BrowserDimOverlayController, String> {
    if let Some(controller) = BROWSER_DIM_OVERLAY_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = BrowserDimOverlayController::spawn()?;
    let _ = BROWSER_DIM_OVERLAY_CONTROLLER.set(controller);
    BROWSER_DIM_OVERLAY_CONTROLLER
        .get()
        .ok_or_else(|| "browser dim overlay controller did not initialize".to_string())
}

fn browser_dim_overlay_controller_if_initialized() -> Option<&'static BrowserDimOverlayController> {
    BROWSER_DIM_OVERLAY_CONTROLLER.get()
}

fn run_browser_dim_overlay_thread(
    command_receiver: Receiver<BrowserDimOverlayCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_browser_dim_overlay_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_browser_dim_overlay_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_browser_dim_overlay_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(BROWSER_DIM_OVERLAY_CLASS);
    let instance = { unsafe { GetModuleHandleW(null()) } };
    let brush = { unsafe { CreateSolidBrush(BROWSER_DIM_OVERLAY_COLOR_RGB) } };
    if brush.is_null() {
        return Err(last_error_message("CreateSolidBrush"));
    }

    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        hbrBackground: brush as HBRUSH,
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

fn run_browser_dim_overlay_loop(
    command_receiver: Receiver<BrowserDimOverlayCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut overlays = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_browser_dim_overlays(&mut overlays);

        match command_receiver.recv_timeout(BROWSER_DIM_OVERLAY_THREAD_SLICE) {
            Ok(BrowserDimOverlayCommand::Show {
                owner_hwnd,
                rect,
                alpha,
                response,
            }) => {
                let result = show_browser_dim_overlay_internal(
                    &mut overlays,
                    instance,
                    owner_hwnd,
                    rect,
                    alpha,
                );
                let _ = response.send(result);
            }
            Ok(BrowserDimOverlayCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_browser_dim_overlay_internal(&mut overlays, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_browser_dim_overlays(&mut overlays);
    Ok(())
}

fn show_browser_dim_overlay_internal(
    overlays: &mut HashMap<u64, HWND>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let Ok(owner) = hwnd_from_raw(owner_hwnd) else {
        return hide_browser_dim_overlay_internal(overlays, owner_hwnd);
    };
    if !is_valid_window(owner) {
        return hide_browser_dim_overlay_internal(overlays, owner_hwnd);
    }

    let overlay = match overlays.get(&owner_hwnd).copied() {
        Some(existing) if is_valid_window(existing) => existing,
        Some(existing) => {
            destroy_browser_dim_overlay_window(existing);
            let overlay = create_browser_dim_overlay_window(instance, owner)?;
            overlays.insert(owner_hwnd, overlay);
            overlay
        }
        None => {
            let overlay = create_browser_dim_overlay_window(instance, owner)?;
            overlays.insert(owner_hwnd, overlay);
            overlay
        }
    };

    show_browser_dim_overlay_window(overlay, rect, alpha)?;
    browser_dim_overlay_active_owners()
        .lock()
        .expect("browser dim overlay active owners mutex poisoned")
        .insert(owner_hwnd);
    Ok(())
}

fn hide_browser_dim_overlay_internal(
    overlays: &mut HashMap<u64, HWND>,
    owner_hwnd: u64,
) -> Result<(), String> {
    if let Some(overlay) = overlays.remove(&owner_hwnd) {
        let _ = browser_dim_overlay_active_owners()
            .lock()
            .expect("browser dim overlay active owners mutex poisoned")
            .remove(&owner_hwnd);
        destroy_browser_dim_overlay_window(overlay);
    }

    Ok(())
}

fn prune_stale_browser_dim_overlays(overlays: &mut HashMap<u64, HWND>) {
    let stale_owners = overlays
        .iter()
        .filter_map(|(owner_hwnd, overlay_hwnd)| {
            let owner = hwnd_from_raw(*owner_hwnd).ok();
            (owner.is_none()
                || owner.is_some_and(|hwnd| !is_valid_window(hwnd))
                || !is_valid_window(*overlay_hwnd))
            .then_some(*owner_hwnd)
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        let _ = hide_browser_dim_overlay_internal(overlays, owner_hwnd);
    }
}

fn destroy_all_browser_dim_overlays(overlays: &mut HashMap<u64, HWND>) {
    for (_, overlay) in overlays.drain() {
        destroy_browser_dim_overlay_window(overlay);
    }
}

fn create_browser_dim_overlay_window(instance: HINSTANCE, owner: HWND) -> Result<HWND, String> {
    let class_name = widestring(BROWSER_DIM_OVERLAY_CLASS);
    let window = {
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
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

    Ok(window)
}

fn show_browser_dim_overlay_window(
    window: HWND,
    rect: flowtile_domain::Rect,
    alpha: u8,
) -> Result<(), String> {
    let layered = { unsafe { SetLayeredWindowAttributes(window, 0, alpha, LWA_ALPHA) } };
    if layered == 0 {
        return Err(last_error_message("SetLayeredWindowAttributes"));
    }

    let width = i32::try_from(rect.width.max(1))
        .map_err(|_| "browser dim overlay width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(rect.height.max(1))
        .map_err(|_| "browser dim overlay height exceeds Win32 limits".to_string())?;
    let applied = {
        unsafe {
            SetWindowPos(
                window,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                width,
                height,
                BROWSER_DIM_OVERLAY_APPLY_FLAGS,
            )
        }
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    Ok(())
}

fn destroy_browser_dim_overlay_window(window: HWND) {
    if !is_valid_window(window) {
        return;
    }

    let _ = { unsafe { ShowWindow(window, SW_HIDE) } };
    let _ = { unsafe { DestroyWindow(window) } };
}

#[cfg(test)]
mod tests {
    use super::{
        active_browser_dim_overlay_owner_hwnds_snapshot, browser_dim_overlay_active_owners,
    };

    #[test]
    fn active_owner_snapshot_reflects_browser_dim_overlay_state() {
        {
            let mut owners = browser_dim_overlay_active_owners()
                .lock()
                .expect("browser dim overlay active owners mutex poisoned");
            owners.clear();
            owners.extend([101_u64, 202_u64]);
        }

        assert_eq!(
            active_browser_dim_overlay_owner_hwnds_snapshot(),
            [101_u64, 202_u64].into_iter().collect()
        );

        browser_dim_overlay_active_owners()
            .lock()
            .expect("browser dim overlay active owners mutex poisoned")
            .clear();
    }
}
