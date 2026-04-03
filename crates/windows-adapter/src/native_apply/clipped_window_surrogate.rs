use std::{
    collections::{BTreeSet, HashMap},
    env,
    fs::{self, OpenOptions},
    io::Write,
    mem::zeroed,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Dwm::{
        DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
        DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmRegisterThumbnail,
        DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, GW_ENABLEDPOPUP, GW_OWNER, GWL_EXSTYLE,
            GWL_STYLE, GWLP_USERDATA, GetClassNameW, GetForegroundWindow, GetWindow,
            GetWindowLongPtrW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
            IsWindowVisible, RegisterClassW, SW_HIDE, SW_SHOWNA, SetWindowLongPtrW, WM_LBUTTONDOWN,
            WM_MBUTTONDOWN, WM_NCDESTROY, WM_NCLBUTTONDOWN, WM_RBUTTONDOWN, WNDCLASSW, WS_CAPTION,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_THICKFRAME,
        },
    },
};

use super::{
    activate_window, hwnd_from_raw, is_valid_window, last_error_message, pump_overlay_messages,
    spill_mask_overlay::hide_spill_mask_overlay_if_initialized,
    visual_effects::sync_window_native_clip_mask, widestring,
};

const CLIPPED_WINDOW_SURROGATE_CLASS: &str = "FlowTileClippedWindowSurrogate";
const CLIPPED_WINDOW_SURROGATE_THREAD_SLICE: Duration = Duration::from_millis(16);
const CLIPPED_WINDOW_SURROGATE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIPPED_WINDOW_SURROGATE_APPLY_FLAGS: u32 =
    windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOOWNERZORDER
        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW;
const WINDOW_CLASS_ALREADY_EXISTS: u32 = 1410;
const CLASS_CHROME_WIDGET: &str = "chrome_widgetwin_1";
const CLASS_MOZILLA_WINDOW: &str = "mozillawindowclass";
const CLASS_TERMINAL_HOSTING_WINDOW: &str = "cascadia_hosting_window_class";
#[cfg(test)]
const CLASS_APPLICATION_FRAME_WINDOW: &str = "applicationframewindow";
#[cfg(test)]
const CLASS_WINDOWS_CORE_WINDOW: &str = "windows.ui.core.corewindow";
#[cfg(test)]
const CLASS_XAML_EXPLORER_HOST_ISLAND_WINDOW: &str = "xamlexplorerhostislandwindow";
const CLASS_XAML_WINDOWED_POPUP: &str = "xaml_windowedpopupclass";
const CLASS_WIN32_MENU: &str = "#32768";
const CLASS_WIN32_DIALOG: &str = "#32770";
const CLASS_TOOLTIPS: &str = "tooltips_class32";
const CLASS_MSCTF_IME_UI: &str = "msctfime ui";
const CLASS_IME: &str = "ime";
const CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS_ENV: &str =
    "FLOWTILE_CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS";
const EARLY_LOG_PATH_ENV: &str = "FLOWTILE_EARLY_LOG_PATH";

#[derive(Clone, Copy, Debug)]
struct ClippedWindowSurrogate {
    window: HWND,
    thumbnail_id: Option<isize>,
}

#[derive(Clone, Copy, Debug)]
struct ClippedWindowSurrogateWindowState {
    owner_hwnd: u64,
    native_visible_rect: flowtile_domain::Rect,
    native_clip_rect: flowtile_domain::Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClippedWindowSurrogatePointerReplayAction {
    LeftClick,
    RightClick,
    MiddleClick,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ClippedWindowSurrogateEscapeFacts {
    owner_is_foreground: bool,
    has_enabled_popup: bool,
    foreground_is_owned_popup: bool,
    foreground_is_same_process_transient: bool,
}

enum ClippedWindowSurrogateCommand {
    Show {
        owner_hwnd: u64,
        destination_rect: flowtile_domain::Rect,
        source_rect: flowtile_domain::Rect,
        native_visible_rect: flowtile_domain::Rect,
        response: Sender<Result<(), String>>,
    },
    Hide {
        owner_hwnd: u64,
        response: Sender<Result<(), String>>,
    },
}

struct ClippedWindowSurrogateController {
    sender: Sender<ClippedWindowSurrogateCommand>,
}

static CLIPPED_WINDOW_SURROGATE_CONTROLLER: OnceLock<ClippedWindowSurrogateController> =
    OnceLock::new();
static CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS: OnceLock<
    Mutex<crate::SurrogatePresentationDiagnostics>,
> = OnceLock::new();
static CLIPPED_WINDOW_SURROGATE_PRESENTATION_OVERRIDES: OnceLock<
    Mutex<HashMap<u64, crate::WindowPresentationOverride>>,
> = OnceLock::new();
static CLIPPED_WINDOW_SURROGATE_ACTIVE_OWNERS: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();

fn clipped_window_surrogate_diagnostics() -> &'static Mutex<crate::SurrogatePresentationDiagnostics>
{
    CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS.get_or_init(|| Mutex::new(Default::default()))
}

fn clipped_window_surrogate_presentation_overrides()
-> &'static Mutex<HashMap<u64, crate::WindowPresentationOverride>> {
    CLIPPED_WINDOW_SURROGATE_PRESENTATION_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn clipped_window_surrogate_active_owners() -> &'static Mutex<BTreeSet<u64>> {
    CLIPPED_WINDOW_SURROGATE_ACTIVE_OWNERS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(crate) fn surrogate_presentation_diagnostics_snapshot()
-> crate::SurrogatePresentationDiagnostics {
    clipped_window_surrogate_diagnostics()
        .lock()
        .expect("clipped window surrogate diagnostics lock should not be poisoned")
        .clone()
}

pub(crate) fn surrogate_presentation_overrides_snapshot()
-> HashMap<u64, crate::WindowPresentationOverride> {
    clipped_window_surrogate_presentation_overrides()
        .lock()
        .expect("clipped window surrogate presentation override lock should not be poisoned")
        .clone()
}

pub(crate) fn active_clipped_window_surrogate_owner_hwnds_snapshot() -> BTreeSet<u64> {
    clipped_window_surrogate_active_owners()
        .lock()
        .expect("clipped window surrogate active owners lock should not be poisoned")
        .clone()
}

fn set_clipped_window_surrogate_presentation_override(
    raw_hwnd: u64,
    mode: crate::WindowPresentationMode,
    reason: impl Into<String>,
) {
    clipped_window_surrogate_presentation_overrides()
        .lock()
        .expect("clipped window surrogate presentation override lock should not be poisoned")
        .insert(
            raw_hwnd,
            crate::WindowPresentationOverride {
                mode,
                reason: reason.into(),
            },
        );
}

pub(crate) fn clear_clipped_window_surrogate_presentation_override(raw_hwnd: u64) {
    let _ = clipped_window_surrogate_presentation_overrides()
        .lock()
        .expect("clipped window surrogate presentation override lock should not be poisoned")
        .remove(&raw_hwnd);
}

pub(crate) fn record_clipped_window_surrogate_native_fallback(raw_hwnd: u64, reason: &str) {
    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.classifier_rejections = diagnostics.classifier_rejections.saturating_add(1);
        diagnostics.native_fallbacks = diagnostics.native_fallbacks.saturating_add(1);
    });
    set_clipped_window_surrogate_presentation_override(
        raw_hwnd,
        crate::WindowPresentationMode::NativeVisible,
        format!("native-fallback:{reason}"),
    );
    log_clipped_window_surrogate_event("native-fallback", raw_hwnd, format!("reason={reason}"));
}

fn update_clipped_window_surrogate_diagnostics(
    update: impl FnOnce(&mut crate::SurrogatePresentationDiagnostics),
) {
    let mut diagnostics = clipped_window_surrogate_diagnostics()
        .lock()
        .expect("clipped window surrogate diagnostics lock should not be poisoned");
    update(&mut diagnostics);
}

fn log_clipped_window_surrogate_event(event: &str, raw_hwnd: u64, details: impl AsRef<str>) {
    let details = details.as_ref();
    let message = if details.is_empty() {
        format!("adapter: clipped-window-surrogate event={event} hwnd={raw_hwnd}")
    } else {
        format!("adapter: clipped-window-surrogate event={event} hwnd={raw_hwnd} {details}")
    };
    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.last_event = Some(message.clone());
    });
    write_clipped_window_surrogate_runtime_log(&message);
}

fn write_clipped_window_surrogate_runtime_log(message: &str) {
    if env::var(CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS_ENV)
        .map(|value| value == "0")
        .unwrap_or(false)
    {
        return;
    }

    let Some(path) = env::var_os(EARLY_LOG_PATH_ENV).map(PathBuf::from) else {
        return;
    };

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "[{timestamp_ms}] {message}");
}

impl ClippedWindowSurrogateController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<ClippedWindowSurrogateCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            run_clipped_window_surrogate_thread(command_receiver, startup_sender)
        });
        startup_receiver
            .recv_timeout(CLIPPED_WINDOW_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("clipped window surrogate startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(
        &self,
        owner_hwnd: u64,
        destination_rect: flowtile_domain::Rect,
        source_rect: flowtile_domain::Rect,
        native_visible_rect: flowtile_domain::Rect,
    ) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(ClippedWindowSurrogateCommand::Show {
                owner_hwnd,
                destination_rect,
                source_rect,
                native_visible_rect,
                response: response_sender,
            })
            .map_err(|_| "clipped window surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(CLIPPED_WINDOW_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("clipped window surrogate response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(ClippedWindowSurrogateCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "clipped window surrogate worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(CLIPPED_WINDOW_SURROGATE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("clipped window surrogate response timed out: {error}"))?
    }
}

pub(crate) fn show_clipped_window_surrogate(
    owner_hwnd: u64,
    destination_rect: flowtile_domain::Rect,
    source_rect: flowtile_domain::Rect,
    native_visible_rect: flowtile_domain::Rect,
) -> Result<(), String> {
    clipped_window_surrogate_controller()?.show(
        owner_hwnd,
        destination_rect,
        source_rect,
        native_visible_rect,
    )
}

pub(crate) fn hide_clipped_window_surrogate_if_initialized(raw_hwnd: u64) -> Result<(), String> {
    clipped_window_surrogate_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(raw_hwnd))
}

pub(crate) fn should_use_clipped_window_surrogate(raw_hwnd: u64) -> bool {
    clipped_window_surrogate_classifier_reason(raw_hwnd) == "eligible"
}

pub(crate) fn clipped_window_surrogate_classifier_reason(raw_hwnd: u64) -> &'static str {
    let Ok(hwnd) = hwnd_from_raw(raw_hwnd) else {
        return "invalid-hwnd";
    };
    if !is_valid_window(hwnd) {
        return "invalid-window";
    }

    let class_name = query_window_class(hwnd);
    let style = query_window_style(hwnd).unwrap_or_default();
    let ex_style = query_window_ex_style(hwnd).unwrap_or_default();
    let has_owner = query_window_owner(hwnd)
        .is_some_and(|owner| !owner.is_null() && owner != hwnd && is_valid_window(owner));

    clipped_window_surrogate_candidate_reason(class_name.as_deref(), style, ex_style, has_owner)
}

fn clipped_window_surrogate_controller() -> Result<&'static ClippedWindowSurrogateController, String>
{
    if let Some(controller) = CLIPPED_WINDOW_SURROGATE_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = ClippedWindowSurrogateController::spawn()?;
    let _ = CLIPPED_WINDOW_SURROGATE_CONTROLLER.set(controller);
    CLIPPED_WINDOW_SURROGATE_CONTROLLER
        .get()
        .ok_or_else(|| "clipped window surrogate controller did not initialize".to_string())
}

fn clipped_window_surrogate_controller_if_initialized()
-> Option<&'static ClippedWindowSurrogateController> {
    CLIPPED_WINDOW_SURROGATE_CONTROLLER.get()
}

fn run_clipped_window_surrogate_thread(
    command_receiver: Receiver<ClippedWindowSurrogateCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_clipped_window_surrogate_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_clipped_window_surrogate_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_clipped_window_surrogate_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(CLIPPED_WINDOW_SURROGATE_CLASS);
    let instance = { unsafe { GetModuleHandleW(null()) } };
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(clipped_window_surrogate_window_proc),
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

fn run_clipped_window_surrogate_loop(
    command_receiver: Receiver<ClippedWindowSurrogateCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut surrogates = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_clipped_window_surrogates(&mut surrogates);

        match command_receiver.recv_timeout(CLIPPED_WINDOW_SURROGATE_THREAD_SLICE) {
            Ok(ClippedWindowSurrogateCommand::Show {
                owner_hwnd,
                destination_rect,
                source_rect,
                native_visible_rect,
                response,
            }) => {
                let result = show_clipped_window_surrogate_internal(
                    &mut surrogates,
                    instance,
                    owner_hwnd,
                    destination_rect,
                    source_rect,
                    native_visible_rect,
                );
                let _ = response.send(result);
            }
            Ok(ClippedWindowSurrogateCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_clipped_window_surrogate_internal(&mut surrogates, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_clipped_window_surrogates(&mut surrogates);
    Ok(())
}

fn show_clipped_window_surrogate_internal(
    surrogates: &mut HashMap<u64, ClippedWindowSurrogate>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    destination_rect: flowtile_domain::Rect,
    source_rect: flowtile_domain::Rect,
    native_visible_rect: flowtile_domain::Rect,
) -> Result<(), String> {
    if destination_rect.width == 0 || destination_rect.height == 0 {
        return hide_clipped_window_surrogate_internal(surrogates, owner_hwnd);
    }

    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.show_requests = diagnostics.show_requests.saturating_add(1);
    });

    let owner = hwnd_from_raw(owner_hwnd)?;
    if !is_valid_window(owner) {
        return hide_clipped_window_surrogate_internal(surrogates, owner_hwnd);
    }

    let created_new_surrogate = !surrogates.contains_key(&owner_hwnd);

    let surrogate = match surrogates.get(&owner_hwnd).copied() {
        Some(existing) if is_valid_window(existing.window) => existing,
        Some(existing) => {
            let _ = destroy_clipped_window_surrogate(existing);
            let surrogate = create_clipped_window_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
        None => {
            let surrogate = create_clipped_window_surrogate(instance, owner)?;
            surrogates.insert(owner_hwnd, surrogate);
            surrogate
        }
    };

    let mut surrogate = surrogate;
    if surrogate.thumbnail_id.is_none() {
        surrogate.thumbnail_id = Some(register_clipped_window_surrogate_thumbnail(
            surrogate.window,
            owner,
        )?);
        update_clipped_window_surrogate_diagnostics(|diagnostics| {
            diagnostics.dwm_thumbnail_backend_uses =
                diagnostics.dwm_thumbnail_backend_uses.saturating_add(1);
        });
    }
    update_clipped_window_surrogate_window_state(
        surrogate.window,
        ClippedWindowSurrogateWindowState {
            owner_hwnd,
            native_visible_rect,
            native_clip_rect: destination_rect,
        },
    );

    show_clipped_window_surrogate_window(
        owner,
        surrogate.window,
        destination_rect,
        source_rect,
        surrogate,
    )?;
    clear_clipped_window_surrogate_presentation_override(owner_hwnd);
    surrogates.insert(owner_hwnd, surrogate);
    if created_new_surrogate {
        update_clipped_window_surrogate_diagnostics(|diagnostics| {
            diagnostics.active_hosts = diagnostics.active_hosts.saturating_add(1);
        });
        clipped_window_surrogate_active_owners()
            .lock()
            .expect("clipped window surrogate active owners lock should not be poisoned")
            .insert(owner_hwnd);
    }
    if created_new_surrogate {
        log_clipped_window_surrogate_event(
            "show",
            owner_hwnd,
            format!(
                "destination=({},{} {}x{}) source=({},{} {}x{}) backend=dwm-thumbnail",
                destination_rect.x,
                destination_rect.y,
                destination_rect.width,
                destination_rect.height,
                source_rect.x,
                source_rect.y,
                source_rect.width,
                source_rect.height
            ),
        );
    }
    Ok(())
}

fn hide_clipped_window_surrogate_internal(
    surrogates: &mut HashMap<u64, ClippedWindowSurrogate>,
    owner_hwnd: u64,
) -> Result<(), String> {
    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.hide_requests = diagnostics.hide_requests.saturating_add(1);
    });
    if let Some(surrogate) = surrogates.remove(&owner_hwnd) {
        update_clipped_window_surrogate_diagnostics(|diagnostics| {
            diagnostics.active_hosts = diagnostics.active_hosts.saturating_sub(1);
        });
        let _ = clipped_window_surrogate_active_owners()
            .lock()
            .expect("clipped window surrogate active owners lock should not be poisoned")
            .remove(&owner_hwnd);
        log_clipped_window_surrogate_event("hide", owner_hwnd, "");
        return destroy_clipped_window_surrogate(surrogate);
    }

    Ok(())
}

fn prune_stale_clipped_window_surrogates(surrogates: &mut HashMap<u64, ClippedWindowSurrogate>) {
    let foreground_window = unsafe { GetForegroundWindow() };
    let stale_owners = surrogates
        .iter()
        .filter_map(|(owner_hwnd, surrogate)| {
            let owner = match hwnd_from_raw(*owner_hwnd).ok() {
                Some(hwnd) if is_valid_window(hwnd) => hwnd,
                _ => {
                    clear_clipped_window_surrogate_presentation_override(*owner_hwnd);
                    let _ = clipped_window_surrogate_active_owners()
                        .lock()
                        .expect(
                            "clipped window surrogate active owners lock should not be poisoned",
                        )
                        .remove(owner_hwnd);
                    return Some(*owner_hwnd);
                }
            };
            if !is_valid_window(surrogate.window) {
                clear_clipped_window_surrogate_presentation_override(*owner_hwnd);
                let _ = clipped_window_surrogate_active_owners()
                    .lock()
                    .expect("clipped window surrogate active owners lock should not be poisoned")
                    .remove(owner_hwnd);
                return Some(*owner_hwnd);
            }

            if should_escape_clipped_window_surrogate(owner, foreground_window) {
                if let Some(state) = clipped_window_surrogate_window_state(surrogate.window) {
                    update_clipped_window_surrogate_diagnostics(|diagnostics| {
                        diagnostics.transient_escapes =
                            diagnostics.transient_escapes.saturating_add(1);
                    });
                    log_clipped_window_surrogate_event(
                        "transient-escape",
                        state.owner_hwnd,
                        "reason=foreground-or-transient-surface",
                    );
                    set_clipped_window_surrogate_presentation_override(
                        state.owner_hwnd,
                        crate::WindowPresentationMode::NativeVisible,
                        "transient-escape".to_string(),
                    );
                    let _ = promote_clipped_window_surrogate_owner_to_native(
                        state.owner_hwnd,
                        state.native_visible_rect,
                        state.native_clip_rect,
                    );
                }
                return Some(*owner_hwnd);
            }

            None
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        let _ = hide_clipped_window_surrogate_internal(surrogates, owner_hwnd);
    }
}

fn destroy_all_clipped_window_surrogates(surrogates: &mut HashMap<u64, ClippedWindowSurrogate>) {
    let owner_hwnds = surrogates.keys().copied().collect::<Vec<_>>();
    for owner_hwnd in owner_hwnds {
        let _ = hide_clipped_window_surrogate_internal(surrogates, owner_hwnd);
        clear_clipped_window_surrogate_presentation_override(owner_hwnd);
    }
}

fn create_clipped_window_surrogate(
    instance: HINSTANCE,
    owner: HWND,
) -> Result<ClippedWindowSurrogate, String> {
    let class_name = widestring(CLIPPED_WINDOW_SURROGATE_CLASS);
    let window = {
        unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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

    Ok(ClippedWindowSurrogate {
        window,
        thumbnail_id: None,
    })
}

fn register_clipped_window_surrogate_thumbnail(
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

fn show_clipped_window_surrogate_window(
    owner: HWND,
    window: HWND,
    destination_rect: flowtile_domain::Rect,
    source_rect: flowtile_domain::Rect,
    surrogate: ClippedWindowSurrogate,
) -> Result<(), String> {
    let width = i32::try_from(destination_rect.width.max(1))
        .map_err(|_| "clipped window surrogate width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(destination_rect.height.max(1))
        .map_err(|_| "clipped window surrogate height exceeds Win32 limits".to_string())?;
    let source_right = source_rect
        .x
        .checked_add(i32::try_from(source_rect.width.max(1)).map_err(|_| {
            "clipped window surrogate source width exceeds Win32 limits".to_string()
        })?)
        .ok_or_else(|| "clipped window surrogate source right edge overflowed".to_string())?;
    let source_bottom = source_rect
        .y
        .checked_add(i32::try_from(source_rect.height.max(1)).map_err(|_| {
            "clipped window surrogate source height exceeds Win32 limits".to_string()
        })?)
        .ok_or_else(|| "clipped window surrogate source bottom edge overflowed".to_string())?;

    let applied = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
            window,
            clipped_window_surrogate_insert_after(owner),
            destination_rect.x,
            destination_rect.y,
            width,
            height,
            CLIPPED_WINDOW_SURROGATE_APPLY_FLAGS,
        )
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    let thumbnail_id = surrogate
        .thumbnail_id
        .ok_or_else(|| "clipped window surrogate thumbnail was not initialized".to_string())?;
    let thumbnail_properties = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_RECTSOURCE
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
            left: source_rect.x,
            top: source_rect.y,
            right: source_right,
            bottom: source_bottom,
        },
        opacity: u8::MAX,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let result = { unsafe { DwmUpdateThumbnailProperties(thumbnail_id, &thumbnail_properties) } };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }

    let _ =
        { unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(window, SW_SHOWNA) } };
    Ok(())
}

fn clipped_window_surrogate_insert_after(owner: HWND) -> HWND {
    if owner_requires_topmost_surrogate(query_window_ex_style(owner).unwrap_or_default()) {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    }
}

fn owner_requires_topmost_surrogate(owner_ex_style: u32) -> bool {
    (owner_ex_style & WS_EX_TOPMOST) != 0
}

fn update_clipped_window_surrogate_window_state(
    window: HWND,
    state: ClippedWindowSurrogateWindowState,
) {
    let existing = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(window, GWLP_USERDATA)
            as *mut ClippedWindowSurrogateWindowState
    };
    if existing.is_null() {
        let boxed_state = Box::new(state);
        let raw_state = Box::into_raw(boxed_state);
        let _ = unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, raw_state as isize) };
        return;
    }

    unsafe {
        *existing = state;
    }
}

fn clipped_window_surrogate_window_state(hwnd: HWND) -> Option<ClippedWindowSurrogateWindowState> {
    let state_ptr = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *mut ClippedWindowSurrogateWindowState
    };
    if state_ptr.is_null() {
        None
    } else {
        Some(unsafe { *state_ptr })
    }
}

fn clear_clipped_window_surrogate_window_state(hwnd: HWND) {
    let state_ptr = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *mut ClippedWindowSurrogateWindowState
    };
    if state_ptr.is_null() {
        return;
    }

    let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    let _ = unsafe { Box::from_raw(state_ptr) };
}

fn promote_clipped_window_surrogate_owner_to_native(
    owner_hwnd: u64,
    native_visible_rect: flowtile_domain::Rect,
    native_clip_rect: flowtile_domain::Rect,
) -> Result<(), String> {
    let owner = hwnd_from_raw(owner_hwnd)?;
    if !is_valid_window(owner) {
        return Err(format!(
            "clipped window surrogate owner hwnd {owner_hwnd} is no longer valid"
        ));
    }

    let width = i32::try_from(native_visible_rect.width.max(1))
        .map_err(|_| "native visible width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(native_visible_rect.height.max(1))
        .map_err(|_| "native visible height exceeds Win32 limits".to_string())?;
    let positioned = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
            owner,
            HWND_TOP,
            native_visible_rect.x,
            native_visible_rect.y,
            width,
            height,
            windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOOWNERZORDER
                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
        )
    };
    if positioned == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    sync_window_native_clip_mask(owner, Some(native_visible_rect), Some(native_clip_rect))?;
    hide_spill_mask_overlay_if_initialized(owner_hwnd)?;

    activate_window(owner_hwnd)?;
    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.handoff_promotions = diagnostics.handoff_promotions.saturating_add(1);
    });
    log_clipped_window_surrogate_event(
        "handoff-to-native",
        owner_hwnd,
        format!(
            "target=({},{} {}x{})",
            native_visible_rect.x,
            native_visible_rect.y,
            native_visible_rect.width,
            native_visible_rect.height,
        ),
    );

    Ok(())
}

fn should_escape_clipped_window_surrogate(owner: HWND, foreground_window: HWND) -> bool {
    let owner_process_id = query_window_process_id(owner);
    let facts = ClippedWindowSurrogateEscapeFacts {
        owner_is_foreground: owner == foreground_window,
        has_enabled_popup: has_visible_owned_popup(owner),
        foreground_is_owned_popup: is_visible_owned_popup_window(owner, foreground_window),
        foreground_is_same_process_transient: owner_process_id
            .filter(|process_id| *process_id != 0)
            .is_some_and(|process_id| {
                is_same_process_transient_escape_window(process_id, owner, foreground_window)
            }),
    };

    should_escape_clipped_window_surrogate_from_facts(facts)
}

fn has_visible_owned_popup(owner: HWND) -> bool {
    let popup = unsafe { GetWindow(owner, GW_ENABLEDPOPUP) };
    !popup.is_null()
        && popup != owner
        && is_valid_window(popup)
        && unsafe { IsWindowVisible(popup) != 0 }
}

fn is_visible_owned_popup_window(owner: HWND, candidate: HWND) -> bool {
    if candidate.is_null() || candidate == owner || !is_valid_window(candidate) {
        return false;
    }

    let actual_owner = query_window_owner(candidate).unwrap_or_default();
    actual_owner == owner && unsafe { IsWindowVisible(candidate) != 0 }
}

fn is_same_process_transient_escape_window(
    owner_process_id: u32,
    owner: HWND,
    candidate: HWND,
) -> bool {
    if candidate.is_null() || candidate == owner || !is_valid_window(candidate) {
        return false;
    }

    let Some(candidate_process_id) = query_window_process_id(candidate) else {
        return false;
    };
    if candidate_process_id == 0 || candidate_process_id != owner_process_id {
        return false;
    }

    let class_name = query_window_class(candidate);
    let style = query_window_style(candidate).unwrap_or_default();
    let ex_style = query_window_ex_style(candidate).unwrap_or_default();

    is_transient_escape_window(class_name.as_deref(), style, ex_style)
}

fn should_escape_clipped_window_surrogate_from_facts(
    facts: ClippedWindowSurrogateEscapeFacts,
) -> bool {
    facts.owner_is_foreground
        || facts.has_enabled_popup
        || facts.foreground_is_owned_popup
        || facts.foreground_is_same_process_transient
}

fn query_window_class(hwnd: HWND) -> Option<String> {
    let mut buffer = [0_u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return None;
    }

    String::from_utf16(&buffer[..copied as usize]).ok()
}

fn query_window_style(hwnd: HWND) -> Option<u32> {
    if hwnd.is_null() {
        return None;
    }

    Some(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 })
}

fn query_window_ex_style(hwnd: HWND) -> Option<u32> {
    if hwnd.is_null() {
        return None;
    }

    Some(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 })
}

fn query_window_owner(hwnd: HWND) -> Option<HWND> {
    if hwnd.is_null() {
        return None;
    }

    Some(unsafe { GetWindow(hwnd, GW_OWNER) })
}

fn query_window_process_id(hwnd: HWND) -> Option<u32> {
    if hwnd.is_null() || !is_valid_window(hwnd) {
        return None;
    }

    let mut process_id = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    Some(process_id)
}

#[cfg(test)]
fn is_safe_clipped_window_surrogate_candidate(
    class_name: Option<&str>,
    style: u32,
    ex_style: u32,
    has_owner: bool,
) -> bool {
    clipped_window_surrogate_candidate_reason(class_name, style, ex_style, has_owner) == "eligible"
}

fn clipped_window_surrogate_candidate_reason(
    class_name: Option<&str>,
    style: u32,
    ex_style: u32,
    has_owner: bool,
) -> &'static str {
    if has_owner {
        return "owned-window";
    }
    if (ex_style & WS_EX_TOOLWINDOW) != 0 {
        return "tool-window";
    }
    if looks_like_transient_popup_window(style) {
        return "transient-popup-style";
    }
    if is_transient_escape_window(class_name, style, ex_style) {
        return "transient-escape-class";
    }
    if class_name.is_some_and(is_problematic_clipped_window_class) {
        return "problematic-class";
    }

    "eligible"
}

fn looks_like_transient_popup_window(style: u32) -> bool {
    let has_popup = (style & WS_POPUP) != 0;
    let has_caption = (style & WS_CAPTION) != 0;
    let has_thickframe = (style & WS_THICKFRAME) != 0;

    has_popup && !has_caption && !has_thickframe
}

fn is_transient_escape_window(class_name: Option<&str>, style: u32, ex_style: u32) -> bool {
    (ex_style & WS_EX_TOOLWINDOW) != 0
        || looks_like_transient_popup_window(style)
        || class_name.is_some_and(is_transient_escape_class)
}

fn is_transient_escape_class(class_name: &str) -> bool {
    matches!(
        class_name.trim().to_ascii_lowercase().as_str(),
        CLASS_XAML_WINDOWED_POPUP
            | CLASS_WIN32_MENU
            | CLASS_WIN32_DIALOG
            | CLASS_TOOLTIPS
            | CLASS_MSCTF_IME_UI
            | CLASS_IME
    )
}

fn is_problematic_clipped_window_class(class_name: &str) -> bool {
    matches!(
        class_name.trim().to_ascii_lowercase().as_str(),
        CLASS_CHROME_WIDGET | CLASS_MOZILLA_WINDOW | CLASS_TERMINAL_HOSTING_WINDOW
    )
}

fn destroy_clipped_window_surrogate(surrogate: ClippedWindowSurrogate) -> Result<(), String> {
    let mut first_error = None;

    if let Some(thumbnail_id) = surrogate.thumbnail_id {
        let result = { unsafe { DwmUnregisterThumbnail(thumbnail_id) } };
        if result < 0 {
            first_error = Some(format!(
                "DwmUnregisterThumbnail failed with HRESULT {result:#x}"
            ));
        }
    }

    destroy_clipped_window_surrogate_window(surrogate.window);

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn destroy_clipped_window_surrogate_window(window: HWND) {
    if !is_valid_window(window) {
        return;
    }

    let _ = { unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(window, SW_HIDE) } };
    let _ = { unsafe { DestroyWindow(window) } };
}

unsafe extern "system" fn clipped_window_surrogate_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN => {
            let pointer_replay = pointer_replay_action_for_message(message);
            let mut owner_hwnd = None;
            let mut handoff_completed = false;
            if let Some(state) = clipped_window_surrogate_window_state(hwnd) {
                owner_hwnd = Some(state.owner_hwnd);
                handoff_completed = promote_clipped_window_surrogate_owner_to_native(
                    state.owner_hwnd,
                    state.native_visible_rect,
                    state.native_clip_rect,
                )
                .is_ok();
                if handoff_completed {
                    set_clipped_window_surrogate_presentation_override(
                        state.owner_hwnd,
                        crate::WindowPresentationMode::NativeVisible,
                        "pointer-handoff".to_string(),
                    );
                }
            }
            let _ =
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd, SW_HIDE) };
            let _ = unsafe { DestroyWindow(hwnd) };
            if handoff_completed {
                if let (Some(owner_hwnd), Some(pointer_replay)) = (owner_hwnd, pointer_replay) {
                    replay_pointer_handoff_action(owner_hwnd, pointer_replay);
                }
            } else if let Some(owner_hwnd) = owner_hwnd {
                log_clipped_window_surrogate_event(
                    "pointer-replay-skipped",
                    owner_hwnd,
                    "reason=handoff-failed",
                );
            }
            0
        }
        WM_NCDESTROY => {
            clear_clipped_window_surrogate_window_state(hwnd);
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn pointer_replay_action_for_message(
    message: u32,
) -> Option<ClippedWindowSurrogatePointerReplayAction> {
    match message {
        WM_LBUTTONDOWN | WM_NCLBUTTONDOWN => {
            Some(ClippedWindowSurrogatePointerReplayAction::LeftClick)
        }
        WM_RBUTTONDOWN => Some(ClippedWindowSurrogatePointerReplayAction::RightClick),
        WM_MBUTTONDOWN => Some(ClippedWindowSurrogatePointerReplayAction::MiddleClick),
        _ => None,
    }
}

fn pointer_replay_action_name(action: ClippedWindowSurrogatePointerReplayAction) -> &'static str {
    match action {
        ClippedWindowSurrogatePointerReplayAction::LeftClick => "left-click",
        ClippedWindowSurrogatePointerReplayAction::RightClick => "right-click",
        ClippedWindowSurrogatePointerReplayAction::MiddleClick => "middle-click",
    }
}

fn replay_pointer_handoff_action(
    owner_hwnd: u64,
    action: ClippedWindowSurrogatePointerReplayAction,
) {
    update_clipped_window_surrogate_diagnostics(|diagnostics| {
        diagnostics.pointer_replay_attempts = diagnostics.pointer_replay_attempts.saturating_add(1);
    });
    match best_effort_replay_pointer_action(action) {
        Ok(()) => {
            update_clipped_window_surrogate_diagnostics(|diagnostics| {
                diagnostics.pointer_replay_successes =
                    diagnostics.pointer_replay_successes.saturating_add(1);
            });
            log_clipped_window_surrogate_event(
                "pointer-replay-ok",
                owner_hwnd,
                format!("action={}", pointer_replay_action_name(action)),
            );
        }
        Err(error) => {
            update_clipped_window_surrogate_diagnostics(|diagnostics| {
                diagnostics.pointer_replay_failures =
                    diagnostics.pointer_replay_failures.saturating_add(1);
            });
            log_clipped_window_surrogate_event(
                "pointer-replay-error",
                owner_hwnd,
                format!(
                    "action={} error={}",
                    pointer_replay_action_name(action),
                    error
                ),
            );
        }
    }
}

fn best_effort_replay_pointer_action(
    action: ClippedWindowSurrogatePointerReplayAction,
) -> Result<(), String> {
    let (down_flags, up_flags) = match action {
        ClippedWindowSurrogatePointerReplayAction::LeftClick => {
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        }
        ClippedWindowSurrogatePointerReplayAction::RightClick => {
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        }
        ClippedWindowSurrogatePointerReplayAction::MiddleClick => {
            (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)
        }
    };

    let mut inputs = [
        pointer_mouse_input(down_flags),
        pointer_mouse_input(up_flags),
    ];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        return Err(last_error_message("SendInput"));
    }

    Ok(())
}

fn pointer_mouse_input(flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use flowtile_domain::Rect;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_NCLBUTTONDOWN, WM_RBUTTONDOWN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    use super::{
        CLASS_APPLICATION_FRAME_WINDOW, CLASS_CHROME_WIDGET, CLASS_MOZILLA_WINDOW,
        CLASS_TERMINAL_HOSTING_WINDOW, CLASS_WINDOWS_CORE_WINDOW,
        CLASS_XAML_EXPLORER_HOST_ISLAND_WINDOW, ClippedWindowSurrogateEscapeFacts,
        clear_clipped_window_surrogate_presentation_override,
        clipped_window_surrogate_candidate_reason, is_problematic_clipped_window_class,
        is_safe_clipped_window_surrogate_candidate, is_transient_escape_window,
        owner_requires_topmost_surrogate, pointer_replay_action_for_message,
        record_clipped_window_surrogate_native_fallback, should_escape_clipped_window_surrogate,
        should_escape_clipped_window_surrogate_from_facts,
        surrogate_presentation_overrides_snapshot,
    };
    use crate::WindowPresentationMode;

    #[test]
    fn source_rect_destination_can_share_same_partial_width() {
        let destination = Rect::new(928, 16, 672, 868);
        let source = Rect::new(0, 0, 672, 868);

        assert_eq!(destination.width, source.width);
        assert_eq!(destination.height, source.height);
    }

    #[test]
    fn foreground_owner_requires_surrogate_escape() {
        assert!(should_escape_clipped_window_surrogate(
            100_isize as _,
            100_isize as _,
        ));
    }

    #[test]
    fn browser_like_classes_are_rejected_for_clipped_surrogate() {
        assert!(is_problematic_clipped_window_class(CLASS_CHROME_WIDGET));
        assert!(is_problematic_clipped_window_class(CLASS_MOZILLA_WINDOW));
        assert!(is_problematic_clipped_window_class(
            CLASS_TERMINAL_HOSTING_WINDOW
        ));
        assert!(!is_problematic_clipped_window_class(
            CLASS_APPLICATION_FRAME_WINDOW
        ));
        assert!(!is_problematic_clipped_window_class(
            CLASS_WINDOWS_CORE_WINDOW
        ));
        assert!(!is_problematic_clipped_window_class(
            CLASS_XAML_EXPLORER_HOST_ISLAND_WINDOW
        ));
        assert!(!is_problematic_clipped_window_class("FlowtileAppWindow"));
    }

    #[test]
    fn tool_windows_are_rejected_for_clipped_surrogate() {
        assert!(!is_safe_clipped_window_surrogate_candidate(
            Some("FlowtileAppWindow"),
            0,
            WS_EX_TOOLWINDOW,
            false,
        ));
    }

    #[test]
    fn transient_popup_windows_are_rejected_for_clipped_surrogate() {
        assert!(!is_safe_clipped_window_surrogate_candidate(
            Some("#32768"),
            WS_POPUP,
            0,
            false,
        ));
        assert_eq!(
            clipped_window_surrogate_candidate_reason(Some("#32768"), WS_POPUP, 0, false),
            "transient-popup-style"
        );
    }

    #[test]
    fn foreground_same_process_transient_requires_surrogate_escape() {
        let facts = ClippedWindowSurrogateEscapeFacts {
            foreground_is_same_process_transient: true,
            ..Default::default()
        };

        assert!(should_escape_clipped_window_surrogate_from_facts(facts));
    }

    #[test]
    fn foreground_owned_popup_requires_surrogate_escape() {
        let facts = ClippedWindowSurrogateEscapeFacts {
            foreground_is_owned_popup: true,
            ..Default::default()
        };

        assert!(should_escape_clipped_window_surrogate_from_facts(facts));
    }

    #[test]
    fn menu_dialog_tooltip_and_ime_classes_are_treated_as_transient_escape_windows() {
        for class_name in [
            "#32768",
            "#32770",
            "tooltips_class32",
            "msctfime ui",
            "ime",
            "xaml_windowedpopupclass",
        ] {
            assert!(is_transient_escape_window(Some(class_name), 0, 0));
        }
    }

    #[test]
    fn transient_escape_classes_publish_classifier_reason() {
        for class_name in [
            "#32768",
            "#32770",
            "tooltips_class32",
            "msctfime ui",
            "ime",
            "xaml_windowedpopupclass",
        ] {
            assert_eq!(
                clipped_window_surrogate_candidate_reason(Some(class_name), 0, 0, false),
                "transient-escape-class"
            );
        }
    }

    #[test]
    fn owner_topmost_state_controls_surrogate_topmost_policy() {
        assert!(owner_requires_topmost_surrogate(WS_EX_TOPMOST));
        assert!(!owner_requires_topmost_surrogate(0));
    }

    #[test]
    fn classifier_reason_tracks_problematic_classes() {
        assert_eq!(
            clipped_window_surrogate_candidate_reason(Some("Chrome_WidgetWin_1"), 0, 0, false),
            "problematic-class"
        );
        assert_eq!(
            clipped_window_surrogate_candidate_reason(Some("ApplicationFrameWindow"), 0, 0, false),
            "eligible"
        );
        assert_eq!(
            clipped_window_surrogate_candidate_reason(Some("FlowtileAppWindow"), 0, 0, false),
            "eligible"
        );
    }

    #[test]
    fn pointer_replay_mapping_treats_non_client_left_click_as_left_click() {
        assert!(pointer_replay_action_for_message(WM_LBUTTONDOWN).is_some());
        assert!(pointer_replay_action_for_message(WM_NCLBUTTONDOWN).is_some());
        assert!(pointer_replay_action_for_message(WM_RBUTTONDOWN).is_some());
    }

    #[test]
    fn native_fallback_records_effective_native_override() {
        clear_clipped_window_surrogate_presentation_override(777);
        record_clipped_window_surrogate_native_fallback(777, "problematic-class");

        let overrides = surrogate_presentation_overrides_snapshot();
        let override_projection = overrides
            .get(&777)
            .expect("native fallback should publish an effective presentation override");

        assert_eq!(
            override_projection.mode,
            WindowPresentationMode::NativeVisible
        );
        assert_eq!(
            override_projection.reason,
            "native-fallback:problematic-class"
        );

        clear_clipped_window_surrogate_presentation_override(777);
    }
}
