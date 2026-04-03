use std::{
    collections::{BTreeSet, HashMap},
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use crate::WindowMonitorSceneSlice;
use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, RECT},
    Graphics::Dwm::{
        DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
        DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmRegisterThumbnail,
        DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_NOTOPMOST, HWND_TOPMOST,
        RegisterClassW, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW,
        SetWindowPos, ShowWindow, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_POPUP,
    },
};

use super::{
    hwnd_from_raw, is_valid_window, last_error_message, pump_overlay_messages,
    visual_effects::query_window_ex_style, widestring,
};

const WINDOW_MONITOR_SCENE_CLASS: &str = "FlowTileWindowMonitorScene";
const WINDOW_MONITOR_SCENE_THREAD_SLICE: Duration = Duration::from_millis(16);
const WINDOW_MONITOR_SCENE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const WINDOW_MONITOR_SCENE_APPLY_FLAGS: u32 = SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
const WINDOW_CLASS_ALREADY_EXISTS: u32 = 1410;

#[derive(Clone, Copy, Debug)]
struct WindowMonitorSceneHost {
    window: HWND,
    thumbnail_id: Option<isize>,
}

enum WindowMonitorSceneCommand {
    Show {
        owner_hwnd: u64,
        slices: Vec<WindowMonitorSceneSlice>,
        response: Sender<Result<(), String>>,
    },
    Hide {
        owner_hwnd: u64,
        response: Sender<Result<(), String>>,
    },
}

struct WindowMonitorSceneController {
    sender: Sender<WindowMonitorSceneCommand>,
}

static WINDOW_MONITOR_SCENE_CONTROLLER: OnceLock<WindowMonitorSceneController> = OnceLock::new();
static WINDOW_MONITOR_SCENE_DIAGNOSTICS: OnceLock<Mutex<WindowMonitorSceneDiagnostics>> =
    OnceLock::new();
static WINDOW_MONITOR_SCENE_ACTIVE_OWNERS: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WindowMonitorSceneDiagnostics {
    pub active_hosts: usize,
    pub show_requests: u64,
    pub hide_requests: u64,
    pub pruned_hosts: u64,
    pub dwm_thumbnail_backend_uses: u64,
    pub last_event: Option<String>,
}

fn window_monitor_scene_diagnostics() -> &'static Mutex<WindowMonitorSceneDiagnostics> {
    WINDOW_MONITOR_SCENE_DIAGNOSTICS.get_or_init(|| Mutex::new(Default::default()))
}

fn window_monitor_scene_active_owners() -> &'static Mutex<BTreeSet<u64>> {
    WINDOW_MONITOR_SCENE_ACTIVE_OWNERS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(crate) fn window_monitor_scene_diagnostics_snapshot() -> WindowMonitorSceneDiagnostics {
    window_monitor_scene_diagnostics()
        .lock()
        .expect("window monitor scene diagnostics lock should not be poisoned")
        .clone()
}

pub(crate) fn active_window_monitor_scene_owner_hwnds_snapshot() -> BTreeSet<u64> {
    window_monitor_scene_active_owners()
        .lock()
        .expect("window monitor scene active owners lock should not be poisoned")
        .clone()
}

fn update_window_monitor_scene_diagnostics(
    update: impl FnOnce(&mut WindowMonitorSceneDiagnostics),
) {
    let mut diagnostics = window_monitor_scene_diagnostics()
        .lock()
        .expect("window monitor scene diagnostics lock should not be poisoned");
    update(&mut diagnostics);
}

fn log_window_monitor_scene_event(event: &str, raw_hwnd: u64, details: impl AsRef<str>) {
    let details = details.as_ref();
    let message = if details.is_empty() {
        format!("adapter: window-monitor-scene event={event} hwnd={raw_hwnd}")
    } else {
        format!("adapter: window-monitor-scene event={event} hwnd={raw_hwnd} {details}")
    };
    update_window_monitor_scene_diagnostics(|diagnostics| {
        diagnostics.last_event = Some(message);
    });
}

impl WindowMonitorSceneController {
    fn spawn() -> Result<Self, String> {
        let (command_sender, command_receiver) = mpsc::channel::<WindowMonitorSceneCommand>();
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || run_window_monitor_scene_thread(command_receiver, startup_sender));
        startup_receiver
            .recv_timeout(WINDOW_MONITOR_SCENE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("window monitor scene startup timed out: {error}"))??;

        Ok(Self {
            sender: command_sender,
        })
    }

    fn show(&self, owner_hwnd: u64, slices: &[WindowMonitorSceneSlice]) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(WindowMonitorSceneCommand::Show {
                owner_hwnd,
                slices: slices.to_vec(),
                response: response_sender,
            })
            .map_err(|_| "window monitor scene worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(WINDOW_MONITOR_SCENE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("window monitor scene response timed out: {error}"))?
    }

    fn hide(&self, owner_hwnd: u64) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.sender
            .send(WindowMonitorSceneCommand::Hide {
                owner_hwnd,
                response: response_sender,
            })
            .map_err(|_| "window monitor scene worker is no longer available".to_string())?;
        response_receiver
            .recv_timeout(WINDOW_MONITOR_SCENE_RESPONSE_TIMEOUT)
            .map_err(|error| format!("window monitor scene response timed out: {error}"))?
    }
}

pub(crate) fn show_window_monitor_scene(
    owner_hwnd: u64,
    slices: &[WindowMonitorSceneSlice],
) -> Result<(), String> {
    if slices.is_empty() {
        return hide_window_monitor_scene_if_initialized(owner_hwnd);
    }

    window_monitor_scene_controller()?.show(owner_hwnd, slices)
}

pub(crate) fn hide_window_monitor_scene_if_initialized(owner_hwnd: u64) -> Result<(), String> {
    window_monitor_scene_controller_if_initialized()
        .map_or(Ok(()), |controller| controller.hide(owner_hwnd))
}

fn window_monitor_scene_controller() -> Result<&'static WindowMonitorSceneController, String> {
    if let Some(controller) = WINDOW_MONITOR_SCENE_CONTROLLER.get() {
        return Ok(controller);
    }

    let controller = WindowMonitorSceneController::spawn()?;
    let _ = WINDOW_MONITOR_SCENE_CONTROLLER.set(controller);
    WINDOW_MONITOR_SCENE_CONTROLLER
        .get()
        .ok_or_else(|| "window monitor scene controller did not initialize".to_string())
}

fn window_monitor_scene_controller_if_initialized() -> Option<&'static WindowMonitorSceneController>
{
    WINDOW_MONITOR_SCENE_CONTROLLER.get()
}

fn run_window_monitor_scene_thread(
    command_receiver: Receiver<WindowMonitorSceneCommand>,
    startup_sender: Sender<Result<(), String>>,
) {
    match initialize_window_monitor_scene_class() {
        Ok(instance) => {
            let _ = startup_sender.send(Ok(()));
            let _ = run_window_monitor_scene_loop(command_receiver, instance);
        }
        Err(error) => {
            let _ = startup_sender.send(Err(error));
        }
    }
}

fn initialize_window_monitor_scene_class() -> Result<HINSTANCE, String> {
    let class_name = widestring(WINDOW_MONITOR_SCENE_CLASS);
    let instance = unsafe { GetModuleHandleW(null()) };
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        hbrBackground: null_mut(),
        ..unsafe { zeroed() }
    };
    let class_atom = unsafe { RegisterClassW(&window_class) };
    if class_atom == 0 {
        let error = unsafe { GetLastError() };
        if error != WINDOW_CLASS_ALREADY_EXISTS {
            return Err(last_error_message("RegisterClassW"));
        }
    }

    Ok(instance as HINSTANCE)
}

fn run_window_monitor_scene_loop(
    command_receiver: Receiver<WindowMonitorSceneCommand>,
    instance: HINSTANCE,
) -> Result<(), String> {
    let mut hosts = HashMap::new();

    loop {
        pump_overlay_messages()?;
        prune_stale_window_monitor_scene_hosts(&mut hosts);

        match command_receiver.recv_timeout(WINDOW_MONITOR_SCENE_THREAD_SLICE) {
            Ok(WindowMonitorSceneCommand::Show {
                owner_hwnd,
                slices,
                response,
            }) => {
                let result =
                    show_window_monitor_scene_internal(&mut hosts, instance, owner_hwnd, &slices);
                let _ = response.send(result);
            }
            Ok(WindowMonitorSceneCommand::Hide {
                owner_hwnd,
                response,
            }) => {
                let result = hide_window_monitor_scene_internal(&mut hosts, owner_hwnd);
                let _ = response.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    destroy_all_window_monitor_scene_hosts(&mut hosts);
    Ok(())
}

fn show_window_monitor_scene_internal(
    hosts: &mut HashMap<u64, Vec<WindowMonitorSceneHost>>,
    instance: HINSTANCE,
    owner_hwnd: u64,
    slices: &[WindowMonitorSceneSlice],
) -> Result<(), String> {
    update_window_monitor_scene_diagnostics(|diagnostics| {
        diagnostics.show_requests = diagnostics.show_requests.saturating_add(1);
    });
    let Ok(owner) = hwnd_from_raw(owner_hwnd) else {
        return hide_window_monitor_scene_internal(hosts, owner_hwnd);
    };
    if !is_valid_window(owner) {
        return hide_window_monitor_scene_internal(hosts, owner_hwnd);
    }

    let scene_hosts = hosts.entry(owner_hwnd).or_default();
    if scene_hosts.iter().any(|host| !is_valid_window(host.window)) {
        destroy_window_monitor_scene_hosts(scene_hosts);
    }
    sync_window_monitor_scene_host_count(scene_hosts, instance, owner, slices.len())?;

    for (host, slice) in scene_hosts.iter_mut().zip(slices.iter()) {
        if host.thumbnail_id.is_none() {
            host.thumbnail_id = Some(register_window_monitor_scene_thumbnail(host.window, owner)?);
            update_window_monitor_scene_diagnostics(|diagnostics| {
                diagnostics.dwm_thumbnail_backend_uses =
                    diagnostics.dwm_thumbnail_backend_uses.saturating_add(1);
            });
        }
        show_window_monitor_scene_host(owner, *host, slice)?;
    }
    update_window_monitor_scene_diagnostics(|diagnostics| {
        diagnostics.active_hosts = hosts.values().map(Vec::len).sum();
    });
    window_monitor_scene_active_owners()
        .lock()
        .expect("window monitor scene active owners lock should not be poisoned")
        .insert(owner_hwnd);
    log_window_monitor_scene_event("show", owner_hwnd, format!("slice-count={}", slices.len()));

    Ok(())
}

fn hide_window_monitor_scene_internal(
    hosts: &mut HashMap<u64, Vec<WindowMonitorSceneHost>>,
    owner_hwnd: u64,
) -> Result<(), String> {
    update_window_monitor_scene_diagnostics(|diagnostics| {
        diagnostics.hide_requests = diagnostics.hide_requests.saturating_add(1);
    });
    if let Some(mut scene_hosts) = hosts.remove(&owner_hwnd) {
        destroy_window_monitor_scene_hosts(&mut scene_hosts);
        update_window_monitor_scene_diagnostics(|diagnostics| {
            diagnostics.active_hosts = hosts.values().map(Vec::len).sum();
        });
        let _ = window_monitor_scene_active_owners()
            .lock()
            .expect("window monitor scene active owners lock should not be poisoned")
            .remove(&owner_hwnd);
        log_window_monitor_scene_event("hide", owner_hwnd, "");
    }

    Ok(())
}

fn prune_stale_window_monitor_scene_hosts(hosts: &mut HashMap<u64, Vec<WindowMonitorSceneHost>>) {
    let stale_owners = hosts
        .iter()
        .filter_map(|(owner_hwnd, scene_hosts)| {
            let owner = hwnd_from_raw(*owner_hwnd).ok();
            (owner.is_none()
                || owner.is_some_and(|hwnd| !is_valid_window(hwnd))
                || scene_hosts.iter().any(|host| !is_valid_window(host.window)))
            .then_some(*owner_hwnd)
        })
        .collect::<Vec<_>>();

    for owner_hwnd in stale_owners {
        update_window_monitor_scene_diagnostics(|diagnostics| {
            diagnostics.pruned_hosts = diagnostics.pruned_hosts.saturating_add(1);
        });
        log_window_monitor_scene_event("prune-stale", owner_hwnd, "");
        let _ = hide_window_monitor_scene_internal(hosts, owner_hwnd);
    }
}

fn destroy_all_window_monitor_scene_hosts(hosts: &mut HashMap<u64, Vec<WindowMonitorSceneHost>>) {
    for (_, mut scene_hosts) in hosts.drain() {
        destroy_window_monitor_scene_hosts(&mut scene_hosts);
    }
}

fn sync_window_monitor_scene_host_count(
    scene_hosts: &mut Vec<WindowMonitorSceneHost>,
    instance: HINSTANCE,
    owner: HWND,
    target_count: usize,
) -> Result<(), String> {
    while scene_hosts.len() > target_count {
        if let Some(host) = scene_hosts.pop() {
            destroy_window_monitor_scene_host(host);
        }
    }

    while scene_hosts.len() < target_count {
        scene_hosts.push(create_window_monitor_scene_host(instance, owner)?);
    }

    Ok(())
}

fn create_window_monitor_scene_host(
    instance: HINSTANCE,
    owner: HWND,
) -> Result<WindowMonitorSceneHost, String> {
    let class_name = widestring(WINDOW_MONITOR_SCENE_CLASS);
    let window = unsafe {
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
    };
    if window.is_null() {
        return Err(last_error_message("CreateWindowExW"));
    }

    Ok(WindowMonitorSceneHost {
        window,
        thumbnail_id: None,
    })
}

fn register_window_monitor_scene_thumbnail(
    destination: HWND,
    source: HWND,
) -> Result<isize, String> {
    let mut thumbnail_id = 0_isize;
    let result = unsafe { DwmRegisterThumbnail(destination, source, &mut thumbnail_id) };
    if result < 0 {
        return Err(format!(
            "DwmRegisterThumbnail failed with HRESULT {result:#x}"
        ));
    }

    Ok(thumbnail_id)
}

fn show_window_monitor_scene_host(
    owner: HWND,
    host: WindowMonitorSceneHost,
    slice: &WindowMonitorSceneSlice,
) -> Result<(), String> {
    let width = i32::try_from(slice.destination_rect.width.max(1))
        .map_err(|_| "window monitor scene width exceeds Win32 limits".to_string())?;
    let height = i32::try_from(slice.destination_rect.height.max(1))
        .map_err(|_| "window monitor scene height exceeds Win32 limits".to_string())?;
    let source_right =
        slice
            .source_rect
            .x
            .checked_add(i32::try_from(slice.source_rect.width.max(1)).map_err(|_| {
                "window monitor scene source width exceeds Win32 limits".to_string()
            })?)
            .ok_or_else(|| "window monitor scene source right edge overflowed".to_string())?;
    let source_bottom =
        slice
            .source_rect
            .y
            .checked_add(i32::try_from(slice.source_rect.height.max(1)).map_err(|_| {
                "window monitor scene source height exceeds Win32 limits".to_string()
            })?)
            .ok_or_else(|| "window monitor scene source bottom edge overflowed".to_string())?;

    let applied = unsafe {
        SetWindowPos(
            host.window,
            window_monitor_scene_insert_after(owner),
            slice.destination_rect.x,
            slice.destination_rect.y,
            width,
            height,
            WINDOW_MONITOR_SCENE_APPLY_FLAGS,
        )
    };
    if applied == 0 {
        return Err(last_error_message("SetWindowPos"));
    }

    let thumbnail_id = host
        .thumbnail_id
        .ok_or_else(|| "window monitor scene thumbnail was not initialized".to_string())?;
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
            left: slice.source_rect.x,
            top: slice.source_rect.y,
            right: source_right,
            bottom: source_bottom,
        },
        opacity: u8::MAX,
        fVisible: 1,
        fSourceClientAreaOnly: 0,
    };
    let result = unsafe { DwmUpdateThumbnailProperties(thumbnail_id, &thumbnail_properties) };
    if result < 0 {
        return Err(format!(
            "DwmUpdateThumbnailProperties failed with HRESULT {result:#x}"
        ));
    }

    let _ = unsafe { ShowWindow(host.window, SW_SHOWNA) };
    Ok(())
}

fn window_monitor_scene_insert_after(owner: HWND) -> HWND {
    if owner_requires_topmost_window_monitor_scene(query_window_ex_style(owner).unwrap_or_default())
    {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    }
}

fn owner_requires_topmost_window_monitor_scene(owner_ex_style: u32) -> bool {
    (owner_ex_style & WS_EX_TOPMOST) != 0
}

fn destroy_window_monitor_scene_hosts(scene_hosts: &mut Vec<WindowMonitorSceneHost>) {
    for host in scene_hosts.drain(..) {
        destroy_window_monitor_scene_host(host);
    }
}

fn destroy_window_monitor_scene_host(host: WindowMonitorSceneHost) {
    if let Some(thumbnail_id) = host.thumbnail_id {
        let _ = unsafe { DwmUnregisterThumbnail(thumbnail_id) };
    }

    if !is_valid_window(host.window) {
        return;
    }

    let _ = unsafe { ShowWindow(host.window, SW_HIDE) };
    let _ = unsafe { DestroyWindow(host.window) };
}

#[cfg(test)]
mod tests {
    use flowtile_domain::Rect;

    use crate::{WindowMonitorSceneSlice, WindowMonitorSceneSliceKind};

    use super::{
        show_window_monitor_scene, update_window_monitor_scene_diagnostics,
        window_monitor_scene_diagnostics_snapshot,
    };

    #[test]
    fn empty_slice_list_short_circuits_as_hide_path() {
        assert!(show_window_monitor_scene(123, &[]).is_ok());
    }

    #[test]
    fn scene_slice_kind_round_trip_can_be_instantiated_for_foreign_monitor_surrogates() {
        let slice = WindowMonitorSceneSlice {
            kind: WindowMonitorSceneSliceKind::ForeignMonitorSurrogate,
            monitor_rect: Rect::new(1600, 0, 1440, 1200),
            destination_rect: Rect::new(1600, 16, 228, 868),
            source_rect: Rect::new(672, 0, 228, 868),
            native_visible_rect: Rect::new(928, 16, 900, 868),
        };

        assert_eq!(
            slice.kind,
            WindowMonitorSceneSliceKind::ForeignMonitorSurrogate
        );
    }

    #[test]
    fn diagnostics_snapshot_reflects_scene_updates() {
        update_window_monitor_scene_diagnostics(|diagnostics| {
            diagnostics.active_hosts = 2;
            diagnostics.show_requests = 3;
            diagnostics.hide_requests = 1;
            diagnostics.pruned_hosts = 1;
            diagnostics.last_event =
                Some("adapter: window-monitor-scene event=test hwnd=1".to_string());
        });

        let snapshot = window_monitor_scene_diagnostics_snapshot();
        assert_eq!(snapshot.active_hosts, 2);
        assert_eq!(snapshot.show_requests, 3);
        assert_eq!(snapshot.hide_requests, 1);
        assert_eq!(snapshot.pruned_hosts, 1);
        assert_eq!(
            snapshot.last_event.as_deref(),
            Some("adapter: window-monitor-scene event=test hwnd=1")
        );
    }
}
