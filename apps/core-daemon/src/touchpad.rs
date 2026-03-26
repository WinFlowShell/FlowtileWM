use std::{
    collections::HashMap,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
};

use flowtile_config_rules::{TouchpadConfig, TouchpadGestureBinding};

use crate::control::{ControlMessage, WatchCommand};
use crate::diag::write_touchpad_dump;

const TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS: &str = "windows-touch-gestures-enabled";
const TOUCHPAD_SYSTEM_SETTING_UNKNOWN_STATUS: &str = "windows-touch-gesture-setting-unknown";
const TOUCHPAD_BACKEND_UNAVAILABLE_STATUS: &str = "touchpad-backend-unavailable";
const TOUCHPAD_SETTINGS_URI: &str = "ms-settings:devices-touch";

#[cfg(windows)]
use windows_sys::Win32::{
    Devices::HumanInterfaceDevice::{HID_USAGE_DIGITIZER_TOUCH_PAD, HID_USAGE_PAGE_DIGITIZER},
    Foundation::{GetLastError, HINSTANCE},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::{
            GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
            RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIM_TYPEHID, RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
            HWND_MESSAGE, MSG, PostThreadMessageW, RegisterClassW, TranslateMessage, WM_INPUT,
            WM_INPUT_DEVICE_CHANGE, WM_QUIT, WNDCLASSW,
        },
    },
};

#[derive(Debug)]
pub(crate) enum TouchpadListenerError {
    Startup(String),
}

impl std::fmt::Display for TouchpadListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Startup(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TouchpadListenerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TouchpadOverrideAssessment {
    pub requested: bool,
    pub configured_gesture_count: usize,
    pub normalized_gesture_count: usize,
    pub status: &'static str,
    pub detail: Option<String>,
}

impl TouchpadOverrideAssessment {
    pub(crate) fn summary_label(&self) -> &'static str {
        match self.status {
            "disabled" => "disabled",
            "ready" => "enabled",
            "invalid-config" => "invalid-config",
            TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS => "windows-setting-required",
            TOUCHPAD_SYSTEM_SETTING_UNKNOWN_STATUS => "windows-setting-unknown",
            TOUCHPAD_BACKEND_UNAVAILABLE_STATUS => "backend-unavailable",
            _ => self.status,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SystemTouchGestureSetting {
    Disabled,
    Enabled,
    Unknown(String),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TouchpadGesture {
    ThreeFingerSwipeLeft,
    ThreeFingerSwipeRight,
    ThreeFingerSwipeUp,
    ThreeFingerSwipeDown,
    FourFingerSwipeLeft,
    FourFingerSwipeRight,
    FourFingerSwipeUp,
    FourFingerSwipeDown,
}

impl TouchpadGesture {
    fn parse(value: &str) -> Result<Self, TouchpadListenerError> {
        match value {
            "three-finger-swipe-left" => Ok(Self::ThreeFingerSwipeLeft),
            "three-finger-swipe-right" => Ok(Self::ThreeFingerSwipeRight),
            "three-finger-swipe-up" => Ok(Self::ThreeFingerSwipeUp),
            "three-finger-swipe-down" => Ok(Self::ThreeFingerSwipeDown),
            "four-finger-swipe-left" => Ok(Self::FourFingerSwipeLeft),
            "four-finger-swipe-right" => Ok(Self::FourFingerSwipeRight),
            "four-finger-swipe-up" => Ok(Self::FourFingerSwipeUp),
            "four-finger-swipe-down" => Ok(Self::FourFingerSwipeDown),
            _ => Err(TouchpadListenerError::Startup(format!(
                "unsupported touchpad gesture '{}'",
                value
            ))),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TouchpadBindingSet {
    bindings: HashMap<TouchpadGesture, WatchCommand>,
}

impl TouchpadBindingSet {
    fn from_config(config: &TouchpadConfig) -> Result<Self, TouchpadListenerError> {
        let mut bindings = HashMap::new();
        for binding in &config.gestures {
            let normalized = normalize_binding(binding)?;
            bindings.insert(normalized.gesture, normalized.command);
        }

        Ok(Self { bindings })
    }

    fn len(&self) -> usize {
        self.bindings.len()
    }

    fn command_for(&self, gesture: TouchpadGesture) -> Option<WatchCommand> {
        self.bindings.get(&gesture).copied()
    }
}

pub(crate) fn ipc_command_for_touchpad_gesture(
    config: &TouchpadConfig,
    gesture: &str,
) -> Result<Option<&'static str>, TouchpadListenerError> {
    let bindings = TouchpadBindingSet::from_config(config)?;
    let gesture = TouchpadGesture::parse(gesture)?;
    let Some(command) = bindings.command_for(gesture) else {
        return Ok(None);
    };

    command
        .as_ipc_command_name()
        .map(Some)
        .ok_or_else(|| {
            TouchpadListenerError::Startup(format!(
                "touchpad gesture '{}' resolves to unsupported IPC command '{}'",
                gesture_name(gesture),
                command.as_hotkey_command_name()
            ))
        })
}

fn gesture_name(gesture: TouchpadGesture) -> &'static str {
    match gesture {
        TouchpadGesture::ThreeFingerSwipeLeft => "three-finger-swipe-left",
        TouchpadGesture::ThreeFingerSwipeRight => "three-finger-swipe-right",
        TouchpadGesture::ThreeFingerSwipeUp => "three-finger-swipe-up",
        TouchpadGesture::ThreeFingerSwipeDown => "three-finger-swipe-down",
        TouchpadGesture::FourFingerSwipeLeft => "four-finger-swipe-left",
        TouchpadGesture::FourFingerSwipeRight => "four-finger-swipe-right",
        TouchpadGesture::FourFingerSwipeUp => "four-finger-swipe-up",
        TouchpadGesture::FourFingerSwipeDown => "four-finger-swipe-down",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NormalizedTouchpadBinding {
    gesture: TouchpadGesture,
    command: WatchCommand,
}

fn normalize_binding(
    binding: &TouchpadGestureBinding,
) -> Result<NormalizedTouchpadBinding, TouchpadListenerError> {
    let gesture = TouchpadGesture::parse(&binding.gesture)?;
    let Some(command) = WatchCommand::from_input_command(&binding.command) else {
        return Err(TouchpadListenerError::Startup(format!(
            "touchpad gesture '{}' uses unsupported command '{}'",
            binding.gesture, binding.command
        )));
    };

    Ok(NormalizedTouchpadBinding { gesture, command })
}

#[derive(Debug)]
enum TouchpadRuntimeEvent {
    Gesture(TouchpadGesture),
    Shutdown,
}

#[derive(Debug)]
struct TouchpadGestureRuntime {
    event_sender: Sender<TouchpadRuntimeEvent>,
    worker: Option<JoinHandle<()>>,
}

impl TouchpadGestureRuntime {
    fn spawn(bindings: TouchpadBindingSet, command_sender: Sender<ControlMessage>) -> Self {
        let (event_sender, event_receiver) = mpsc::channel::<TouchpadRuntimeEvent>();
        let worker = thread::spawn(move || {
            while let Ok(event) = event_receiver.recv() {
                match event {
                    TouchpadRuntimeEvent::Gesture(gesture) => {
                        let Some(command) = bindings.command_for(gesture) else {
                            continue;
                        };
                        if command_sender.send(ControlMessage::Watch(command)).is_err() {
                            break;
                        }
                    }
                    TouchpadRuntimeEvent::Shutdown => break,
                }
            }
        });

        Self {
            event_sender,
            worker: Some(worker),
        }
    }

    #[cfg(test)]
    fn dispatch_gesture(&self, gesture: TouchpadGesture) -> Result<(), TouchpadListenerError> {
        self.event_sender
            .send(TouchpadRuntimeEvent::Gesture(gesture))
            .map_err(|_| {
                TouchpadListenerError::Startup(
                    "touchpad runtime worker is no longer available".to_string(),
                )
            })
    }

    fn shutdown(&mut self) {
        let _ = self.event_sender.send(TouchpadRuntimeEvent::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct NativeTouchpadRuntime {
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

#[cfg(windows)]
impl NativeTouchpadRuntime {
    fn spawn(gesture_sender: Sender<TouchpadRuntimeEvent>) -> Result<Self, TouchpadListenerError> {
        let (startup_sender, startup_receiver) = mpsc::channel::<Result<u32, String>>();
        let worker = thread::spawn(move || run_touchpad_thread(gesture_sender, startup_sender));

        let thread_id = startup_receiver
            .recv()
            .map_err(|_| {
                TouchpadListenerError::Startup(
                    "touchpad listener thread ended before startup completed".to_string(),
                )
            })?
            .map_err(TouchpadListenerError::Startup)?;

        Ok(Self {
            thread_id,
            worker: Some(worker),
        })
    }

    fn shutdown(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawTouchContact {
    contact_id: u8,
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawTouchPoint {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedRawTouchpadReport {
    scan_time: u16,
    contact_count: usize,
    contact: Option<RawTouchContact>,
}

#[derive(Default)]
struct RawTouchpadFrameAssembler {
    active_scan_time: Option<u16>,
    expected_contacts: usize,
    contacts: HashMap<u8, RawTouchContact>,
    recognizer: SwipeRecognizer,
}

#[derive(Default)]
struct SwipeRecognizer {
    session: Option<SwipeSession>,
}

struct SwipeSession {
    finger_count: usize,
    start_centroid: RawTouchPoint,
    last_centroid: RawTouchPoint,
}

impl RawTouchpadFrameAssembler {
    fn process_report(&mut self, report: ParsedRawTouchpadReport) -> Option<TouchpadGesture> {
        if self
            .active_scan_time
            .is_some_and(|active_scan_time| active_scan_time != report.scan_time)
        {
            let gesture = self.flush_frame();
            self.begin_frame(report.scan_time, report.contact_count);
            if let Some(contact) = report.contact {
                self.contacts.insert(contact.contact_id, contact);
            }
            if self.should_flush_current_frame(report.contact_count) {
                return gesture.or_else(|| self.flush_frame());
            }
            return gesture;
        }

        if self.active_scan_time.is_none() {
            self.begin_frame(report.scan_time, report.contact_count);
        }

        if let Some(contact) = report.contact {
            self.contacts.insert(contact.contact_id, contact);
        }

        if self.should_flush_current_frame(report.contact_count) {
            return self.flush_frame();
        }

        None
    }

    fn begin_frame(&mut self, scan_time: u16, contact_count: usize) {
        self.active_scan_time = Some(scan_time);
        self.expected_contacts = contact_count;
        self.contacts.clear();
    }

    fn should_flush_current_frame(&self, reported_contact_count: usize) -> bool {
        reported_contact_count == 0
            || (self.expected_contacts > 0 && self.contacts.len() >= self.expected_contacts)
    }

    fn flush_frame(&mut self) -> Option<TouchpadGesture> {
        self.active_scan_time = None;
        self.expected_contacts = 0;
        let contacts = self
            .contacts
            .drain()
            .map(|(_, contact)| contact)
            .collect::<Vec<_>>();
        self.recognizer.process_contacts(&contacts)
    }
}

impl SwipeRecognizer {
    fn process_contacts(&mut self, contacts: &[RawTouchContact]) -> Option<TouchpadGesture> {
        let finger_count = contacts.len();
        if !(3..=4).contains(&finger_count) {
            return self.finish_current_session();
        }

        let centroid = centroid_for_contacts(contacts);
        let session = self.session.get_or_insert(SwipeSession {
            finger_count,
            start_centroid: centroid,
            last_centroid: centroid,
        });
        session.finger_count = session.finger_count.max(finger_count);
        session.last_centroid = centroid;
        None
    }

    fn finish_current_session(&mut self) -> Option<TouchpadGesture> {
        let session = self.session.take()?;
        recognize_swipe(session)
    }
}

fn recognize_swipe(session: SwipeSession) -> Option<TouchpadGesture> {
    const SWIPE_DISTANCE_THRESHOLD: i32 = 120;
    const DOMINANCE_RATIO_NUMERATOR: i32 = 3;
    const DOMINANCE_RATIO_DENOMINATOR: i32 = 2;

    let delta_x = session.last_centroid.x - session.start_centroid.x;
    let delta_y = session.last_centroid.y - session.start_centroid.y;
    let abs_x = delta_x.abs();
    let abs_y = delta_y.abs();

    if abs_x < SWIPE_DISTANCE_THRESHOLD && abs_y < SWIPE_DISTANCE_THRESHOLD {
        return None;
    }

    let horizontal = abs_x * DOMINANCE_RATIO_DENOMINATOR >= abs_y * DOMINANCE_RATIO_NUMERATOR;
    let vertical = abs_y * DOMINANCE_RATIO_DENOMINATOR >= abs_x * DOMINANCE_RATIO_NUMERATOR;

    match (session.finger_count, horizontal, vertical) {
        (3, true, false) if delta_x > 0 => Some(TouchpadGesture::ThreeFingerSwipeRight),
        (3, true, false) if delta_x < 0 => Some(TouchpadGesture::ThreeFingerSwipeLeft),
        (3, false, true) if delta_y > 0 => Some(TouchpadGesture::ThreeFingerSwipeDown),
        (3, false, true) if delta_y < 0 => Some(TouchpadGesture::ThreeFingerSwipeUp),
        (4, true, false) if delta_x > 0 => Some(TouchpadGesture::FourFingerSwipeRight),
        (4, true, false) if delta_x < 0 => Some(TouchpadGesture::FourFingerSwipeLeft),
        (4, false, true) if delta_y > 0 => Some(TouchpadGesture::FourFingerSwipeDown),
        (4, false, true) if delta_y < 0 => Some(TouchpadGesture::FourFingerSwipeUp),
        _ => None,
    }
}

fn centroid_for_contacts(contacts: &[RawTouchContact]) -> RawTouchPoint {
    let sum_x = contacts
        .iter()
        .map(|contact| i64::from(contact.x))
        .sum::<i64>();
    let sum_y = contacts
        .iter()
        .map(|contact| i64::from(contact.y))
        .sum::<i64>();
    let count = i64::try_from(contacts.len()).unwrap_or(1);

    RawTouchPoint {
        x: (sum_x / count) as i32,
        y: (sum_y / count) as i32,
    }
}

fn parse_sample_touchpad_report(report: &[u8]) -> Option<ParsedRawTouchpadReport> {
    let offset = match report.len() {
        len if len >= 10 => 1,
        len if len >= 9 => 0,
        _ => return None,
    };

    let header = *report.get(offset)?;
    let contact_id = (header >> 2) & 0x03;
    let tip_switch = (header & 0b0000_0010) != 0;
    let confidence = (header & 0b0000_0001) != 0;
    let x = i32::from(u16::from_le_bytes([
        *report.get(offset + 1)?,
        *report.get(offset + 2)?,
    ]));
    let y = i32::from(u16::from_le_bytes([
        *report.get(offset + 3)?,
        *report.get(offset + 4)?,
    ]));
    let scan_time = u16::from_le_bytes([*report.get(offset + 5)?, *report.get(offset + 6)?]);
    let contact_count = usize::from(*report.get(offset + 7)?);

    Some(ParsedRawTouchpadReport {
        scan_time,
        contact_count,
        contact: (tip_switch && confidence).then_some(RawTouchContact { contact_id, x, y }),
    })
}

#[derive(Debug)]
pub(crate) struct TouchpadListener {
    _bindings: TouchpadBindingSet,
    runtime: TouchpadGestureRuntime,
    #[cfg(windows)]
    native: Option<NativeTouchpadRuntime>,
}

impl TouchpadListener {
    pub(crate) fn spawn(
        config: &TouchpadConfig,
        command_sender: Sender<ControlMessage>,
    ) -> Result<Option<Self>, TouchpadListenerError> {
        ensure_touchpad_override_supported(config)?;
        if !config.override_enabled {
            return Ok(None);
        }

        let bindings = TouchpadBindingSet::from_config(config)?;
        Self::spawn_native_runtime(bindings, command_sender).map(Some)
    }

    fn spawn_runtime_only(
        bindings: TouchpadBindingSet,
        command_sender: Sender<ControlMessage>,
    ) -> Self {
        let runtime = TouchpadGestureRuntime::spawn(bindings.clone(), command_sender);
        Self {
            _bindings: bindings,
            runtime,
            #[cfg(windows)]
            native: None,
        }
    }

    fn spawn_native_runtime(
        bindings: TouchpadBindingSet,
        command_sender: Sender<ControlMessage>,
    ) -> Result<Self, TouchpadListenerError> {
        let mut listener = Self::spawn_runtime_only(bindings, command_sender);
        #[cfg(windows)]
        {
            let native = NativeTouchpadRuntime::spawn(listener.runtime.event_sender.clone())?;
            listener.native = Some(native);
            Ok(listener)
        }
        #[cfg(not(windows))]
        {
            let gesture_count = listener._bindings.len();
            drop(listener);
            Err(TouchpadListenerError::Startup(format!(
                "touchpad gesture runtime is requested with {} normalized binding(s), but non-Windows builds do not support the touchpad backend",
                gesture_count
            )))
        }
    }

    #[cfg(test)]
    fn dispatch_gesture(&self, gesture: TouchpadGesture) -> Result<(), TouchpadListenerError> {
        self.runtime.dispatch_gesture(gesture)
    }
}

impl Drop for TouchpadListener {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(native) = self.native.as_mut() {
            native.shutdown();
        }
        self.runtime.shutdown();
    }
}

pub(crate) fn ensure_touchpad_override_supported(
    config: &TouchpadConfig,
) -> Result<(), TouchpadListenerError> {
    let assessment = assess_touchpad_override(config);
    match assessment.status {
        "disabled" => Ok(()),
        "invalid-config"
        | TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS
        | TOUCHPAD_SYSTEM_SETTING_UNKNOWN_STATUS => Err(TouchpadListenerError::Startup(
            assessment
                .detail
                .unwrap_or_else(|| "touchpad override configuration is invalid".to_string()),
        )),
        _ => Ok(()),
    }
}

pub(crate) fn assess_touchpad_override(config: &TouchpadConfig) -> TouchpadOverrideAssessment {
    assess_touchpad_override_with_system_setting(config, read_system_touch_gesture_setting())
}

fn assess_touchpad_override_with_system_setting(
    config: &TouchpadConfig,
    system_setting: SystemTouchGestureSetting,
) -> TouchpadOverrideAssessment {
    if !config.override_enabled {
        return TouchpadOverrideAssessment {
            requested: false,
            configured_gesture_count: config.gestures.len(),
            normalized_gesture_count: 0,
            status: "disabled",
            detail: None,
        };
    }

    if config.gestures.is_empty() {
        return TouchpadOverrideAssessment {
            requested: true,
            configured_gesture_count: 0,
            normalized_gesture_count: 0,
            status: "invalid-config",
            detail: Some(
                "touchpad override is enabled but no touchpad gestures are configured".to_string(),
            ),
        };
    }

    let bindings = match TouchpadBindingSet::from_config(config) {
        Ok(bindings) => bindings,
        Err(error) => {
            return TouchpadOverrideAssessment {
                requested: true,
                configured_gesture_count: config.gestures.len(),
                normalized_gesture_count: 0,
                status: "invalid-config",
                detail: Some(error.to_string()),
            };
        }
    };

    if bindings.len() != config.gestures.len() {
        return TouchpadOverrideAssessment {
            requested: true,
            configured_gesture_count: config.gestures.len(),
            normalized_gesture_count: bindings.len(),
            status: "invalid-config",
            detail: Some(
                "touchpad gesture configuration contains duplicate gesture bindings".to_string(),
            ),
        };
    }

    match system_setting {
        SystemTouchGestureSetting::Enabled => {
            return TouchpadOverrideAssessment {
                requested: true,
                configured_gesture_count: config.gestures.len(),
                normalized_gesture_count: bindings.len(),
                status: TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS,
                detail: Some(format!(
                    "Windows still owns three/four-finger touch gestures. Set Settings > Bluetooth & devices > Touch > Three- and four-finger touch gestures to Off and restart or reload the daemon ({TOUCHPAD_SETTINGS_URI})"
                )),
            };
        }
        SystemTouchGestureSetting::Unknown(message) => {
            return TouchpadOverrideAssessment {
                requested: true,
                configured_gesture_count: config.gestures.len(),
                normalized_gesture_count: bindings.len(),
                status: TOUCHPAD_SYSTEM_SETTING_UNKNOWN_STATUS,
                detail: Some(format!(
                    "FlowtileWM could not verify the Windows touch gesture setting: {message}. Open {TOUCHPAD_SETTINGS_URI} and ensure Three- and four-finger touch gestures is Off"
                )),
            };
        }
        SystemTouchGestureSetting::Disabled => {}
    }

    TouchpadOverrideAssessment {
        requested: true,
        configured_gesture_count: config.gestures.len(),
        normalized_gesture_count: bindings.len(),
        status: "ready",
        detail: None,
    }
}

#[cfg(windows)]
fn read_system_touch_gesture_setting() -> SystemTouchGestureSetting {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
    };

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn read_dword(root: HKEY, subkey: &str, value_name: &str) -> Result<u32, u32> {
        let mut data = 0_u32;
        let mut data_size = std::mem::size_of::<u32>() as u32;
        let subkey = wide(subkey);
        let value_name = wide(value_name);
        let status = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut data_size,
            )
        };

        if status == 0 { Ok(data) } else { Err(status) }
    }

    match read_dword(
        HKEY_CURRENT_USER as HKEY,
        "Control Panel\\Desktop",
        "TouchGestureSetting",
    ) {
        Ok(0) => return SystemTouchGestureSetting::Disabled,
        Ok(1) => return SystemTouchGestureSetting::Enabled,
        Ok(other) => {
            return SystemTouchGestureSetting::Unknown(format!(
                "registry value HKCU\\Control Panel\\Desktop\\TouchGestureSetting had unexpected DWORD value {other}"
            ));
        }
        Err(2) => {}
        Err(error) => {
            return SystemTouchGestureSetting::Unknown(format!(
                "failed to read HKCU\\Control Panel\\Desktop\\TouchGestureSetting (Win32 error {error})"
            ));
        }
    }

    let three_finger = read_dword(
        HKEY_CURRENT_USER as HKEY,
        "Software\\Microsoft\\Windows\\CurrentVersion\\PrecisionTouchPad",
        "ThreeFingerSlideEnabled",
    );
    let four_finger = read_dword(
        HKEY_CURRENT_USER as HKEY,
        "Software\\Microsoft\\Windows\\CurrentVersion\\PrecisionTouchPad",
        "FourFingerSlideEnabled",
    );

    match (three_finger, four_finger) {
        (Ok(0), Ok(0)) => SystemTouchGestureSetting::Disabled,
        (Ok(_), Ok(_)) => SystemTouchGestureSetting::Enabled,
        (Err(a), Err(b)) => SystemTouchGestureSetting::Unknown(format!(
            "failed to read both PrecisionTouchPad slide flags (ThreeFingerSlideEnabled: Win32 error {a}, FourFingerSlideEnabled: Win32 error {b})"
        )),
        (Err(error), _) => SystemTouchGestureSetting::Unknown(format!(
            "failed to read PrecisionTouchPad ThreeFingerSlideEnabled (Win32 error {error})"
        )),
        (_, Err(error)) => SystemTouchGestureSetting::Unknown(format!(
            "failed to read PrecisionTouchPad FourFingerSlideEnabled (Win32 error {error})"
        )),
    }
}

#[cfg(not(windows))]
fn read_system_touch_gesture_setting() -> SystemTouchGestureSetting {
    SystemTouchGestureSetting::Unknown(
        "system touch gesture setting is only implemented for Windows".to_string(),
    )
}

#[cfg(windows)]
fn run_touchpad_thread(
    gesture_sender: Sender<TouchpadRuntimeEvent>,
    startup_sender: mpsc::Sender<Result<u32, String>>,
) {
    let thread_id = unsafe { GetCurrentThreadId() };
    let class_name = wide_string("FlowtileTouchpadRawInputWindow");
    let window_title = wide_string("FlowtileWM Touchpad Raw Input");

    let window_class = WNDCLASSW {
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: 0 as HINSTANCE,
        lpszClassName: class_name.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    let _ = unsafe { RegisterClassW(&window_class) };

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };

    if hwnd.is_null() {
        let error = unsafe { GetLastError() };
        let _ = startup_sender.send(Err(format!(
            "CreateWindowExW for touchpad raw-input listener failed with Win32 error {error}"
        )));
        return;
    }

    let devices = [RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_DIGITIZER,
        usUsage: HID_USAGE_DIGITIZER_TOUCH_PAD,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    }];
    let registered = unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if registered == 0 {
        let error = unsafe { GetLastError() };
        unsafe {
            DestroyWindow(hwnd);
        }
        let _ = startup_sender.send(Err(format!(
            "RegisterRawInputDevices(Digitizer/TouchPad) failed with Win32 error {error}"
        )));
        return;
    }

    let _ = startup_sender.send(Ok(thread_id));
    let mut assembler = RawTouchpadFrameAssembler::default();
    let mut message = unsafe { std::mem::zeroed::<MSG>() };
    loop {
        let status = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if status <= 0 {
            break;
        }

        if message.hwnd == hwnd {
            match message.message {
                WM_INPUT => handle_raw_input_message(
                    message.lParam as HRAWINPUT,
                    &gesture_sender,
                    &mut assembler,
                ),
                WM_INPUT_DEVICE_CHANGE => {}
                _ => {}
            }
        }

        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    unsafe {
        DestroyWindow(hwnd);
    }
}

#[cfg(windows)]
fn handle_raw_input_message(
    hrawinput: HRAWINPUT,
    gesture_sender: &Sender<TouchpadRuntimeEvent>,
    assembler: &mut RawTouchpadFrameAssembler,
) {
    let mut size = 0_u32;
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let probe = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            std::ptr::null_mut(),
            &mut size,
            header_size,
        )
    };
    if probe == u32::MAX || size == 0 {
        return;
    }

    let mut buffer = vec![0_u8; size as usize];
    let status = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            buffer.as_mut_ptr().cast(),
            &mut size,
            header_size,
        )
    };
    if status == u32::MAX || size < header_size {
        return;
    }

    let raw_input = unsafe { &*(buffer.as_ptr() as *const RAWINPUT) };
    if raw_input.header.dwType != RIM_TYPEHID {
        return;
    }

    let hid = unsafe { &raw_input.data.hid };
    let report_size = hid.dwSizeHid as usize;
    let report_count = hid.dwCount as usize;
    if report_size == 0 || report_count == 0 {
        return;
    }

    let total_size = report_size.saturating_mul(report_count);
    let report_bytes = unsafe { std::slice::from_raw_parts(hid.bRawData.as_ptr(), total_size) };
    for report in report_bytes.chunks(report_size) {
        let hex = report
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let parsed = parse_sample_touchpad_report(report);
        write_touchpad_dump(format!(
            "raw-input report_size={} report_count={} bytes=[{}] parsed={parsed:?}",
            report_size, report_count, hex
        ));

        let Some(parsed) = parsed else {
            continue;
        };
        if let Some(gesture) = assembler.process_report(parsed) {
            write_touchpad_dump(format!("recognized-gesture={gesture:?}"));
            let _ = gesture_sender.send(TouchpadRuntimeEvent::Gesture(gesture));
        }
    }
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use flowtile_config_rules::{TouchpadConfig, TouchpadGestureBinding};

    use super::{
        ParsedRawTouchpadReport, RawTouchContact, RawTouchpadFrameAssembler,
        SystemTouchGestureSetting, TOUCHPAD_BACKEND_UNAVAILABLE_STATUS,
        TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS, TouchpadBindingSet, TouchpadGesture,
        TouchpadListener, TouchpadOverrideAssessment, assess_touchpad_override_with_system_setting,
        ensure_touchpad_override_supported, ipc_command_for_touchpad_gesture,
        parse_sample_touchpad_report,
    };
    use crate::control::{ControlMessage, WatchCommand};

    #[test]
    fn disabled_touchpad_override_needs_no_runtime() {
        let config = TouchpadConfig {
            override_enabled: false,
            gestures: Vec::new(),
        };

        assert!(ensure_touchpad_override_supported(&config).is_ok());
        assert!(
            TouchpadListener::spawn(&config, mpsc::channel().0)
                .expect("disabled touchpad override should not fail")
                .is_none()
        );
    }

    #[test]
    fn enabled_touchpad_override_is_ready_after_system_precondition_is_met() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-prev".to_string(),
                }],
            },
            SystemTouchGestureSetting::Disabled,
        );

        assert_eq!(assessment.status, "ready");
        assert_eq!(assessment.summary_label(), "enabled");
        assert_eq!(assessment.normalized_gesture_count, 1);
    }

    #[test]
    fn unknown_system_setting_is_reported_as_unavailable_precondition() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-prev".to_string(),
                }],
            },
            SystemTouchGestureSetting::Unknown("registry read failed".to_string()),
        );

        assert_eq!(assessment.summary_label(), "windows-setting-unknown");
        assert!(
            assessment
                .detail
                .expect("detail should exist")
                .contains("registry read failed")
        );
    }

    #[test]
    fn rejects_unknown_touchpad_command() {
        let config = TouchpadConfig {
            override_enabled: true,
            gestures: vec![TouchpadGestureBinding {
                gesture: "three-finger-swipe-left".to_string(),
                command: "move-column-left".to_string(),
            }],
        };

        let error = ensure_touchpad_override_supported(&config)
            .expect_err("unsupported command should fail validation");
        assert!(error.to_string().contains("unsupported command"));
    }

    #[test]
    fn rejects_duplicate_touchpad_gesture_bindings() {
        let config = TouchpadConfig {
            override_enabled: true,
            gestures: vec![
                TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-prev".to_string(),
                },
                TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-next".to_string(),
                },
            ],
        };

        let error = ensure_touchpad_override_supported(&config)
            .expect_err("duplicate gesture should fail validation");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn summary_maps_backend_unavailable_status_to_explicit_label() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-right".to_string(),
                    command: "focus-next".to_string(),
                }],
            },
            SystemTouchGestureSetting::Disabled,
        );

        let unavailable_assessment = TouchpadOverrideAssessment {
            requested: assessment.requested,
            configured_gesture_count: assessment.configured_gesture_count,
            normalized_gesture_count: assessment.normalized_gesture_count,
            status: TOUCHPAD_BACKEND_UNAVAILABLE_STATUS,
            detail: Some("raw input backend is unavailable".to_string()),
        };

        assert_eq!(
            unavailable_assessment.summary_label(),
            "backend-unavailable"
        );
    }

    #[test]
    fn assessment_reports_windows_setting_required_when_system_gestures_are_still_enabled() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-prev".to_string(),
                }],
            },
            SystemTouchGestureSetting::Enabled,
        );

        assert_eq!(assessment.summary_label(), "windows-setting-required");
        assert_eq!(assessment.status, TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS);
        assert!(
            assessment
                .detail
                .expect("detail should exist")
                .contains("Three- and four-finger touch gestures")
        );
    }

    #[test]
    fn runtime_dispatches_normalized_touchpad_gesture_into_control_channel() {
        let bindings = TouchpadBindingSet::from_config(&TouchpadConfig {
            override_enabled: true,
            gestures: vec![TouchpadGestureBinding {
                gesture: "three-finger-swipe-right".to_string(),
                command: "focus-next".to_string(),
            }],
        })
        .expect("bindings should normalize");
        let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
        let listener = TouchpadListener::spawn_runtime_only(bindings, control_sender);

        listener
            .dispatch_gesture(TouchpadGesture::ThreeFingerSwipeRight)
            .expect("gesture should dispatch");

        let message = control_receiver
            .recv()
            .expect("command should be forwarded");
        assert!(matches!(
            message,
            ControlMessage::Watch(WatchCommand::FocusNext)
        ));
    }

    #[test]
    fn resolves_touchpad_gesture_to_existing_ipc_command() {
        let command = ipc_command_for_touchpad_gesture(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-left".to_string(),
                    command: "focus-prev".to_string(),
                }],
            },
            "three-finger-swipe-left",
        )
        .expect("gesture should resolve")
        .expect("binding should exist");

        assert_eq!(command, "focus_prev");
    }

    #[test]
    fn parses_sample_touchpad_report_with_report_id() {
        let report = [
            0x01,
            0b0000_0111,
            0x20,
            0x03,
            0x10,
            0x02,
            0x34,
            0x12,
            0x03,
            0x00,
        ];
        let parsed = parse_sample_touchpad_report(&report).expect("sample report should parse");

        assert_eq!(
            parsed,
            ParsedRawTouchpadReport {
                scan_time: 0x1234,
                contact_count: 3,
                contact: Some(RawTouchContact {
                    contact_id: 1,
                    x: 0x0320,
                    y: 0x0210,
                }),
            }
        );
    }

    #[test]
    fn assembler_turns_three_finger_frames_into_swipe() {
        let mut assembler = RawTouchpadFrameAssembler::default();
        let start_contacts = [
            RawTouchContact {
                contact_id: 0,
                x: 100,
                y: 200,
            },
            RawTouchContact {
                contact_id: 1,
                x: 140,
                y: 210,
            },
            RawTouchContact {
                contact_id: 2,
                x: 180,
                y: 220,
            },
        ];
        let end_contacts = [
            RawTouchContact {
                contact_id: 0,
                x: 320,
                y: 205,
            },
            RawTouchContact {
                contact_id: 1,
                x: 360,
                y: 215,
            },
            RawTouchContact {
                contact_id: 2,
                x: 400,
                y: 225,
            },
        ];

        for contact in start_contacts {
            assert!(
                assembler
                    .process_report(ParsedRawTouchpadReport {
                        scan_time: 10,
                        contact_count: 3,
                        contact: Some(contact),
                    })
                    .is_none()
            );
        }
        for contact in end_contacts {
            assert!(
                assembler
                    .process_report(ParsedRawTouchpadReport {
                        scan_time: 11,
                        contact_count: 3,
                        contact: Some(contact),
                    })
                    .is_none()
            );
        }

        let gesture = assembler.process_report(ParsedRawTouchpadReport {
            scan_time: 12,
            contact_count: 0,
            contact: None,
        });

        assert_eq!(gesture, Some(TouchpadGesture::ThreeFingerSwipeRight));
    }
}
