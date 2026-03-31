use flowtile_config_rules::TouchpadConfig;

use super::{TouchpadListenerError, bindings::TouchpadBindingSet};

pub(super) const TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS: &str = "windows-touch-gestures-enabled";
pub(super) const TOUCHPAD_SYSTEM_SETTING_UNKNOWN_STATUS: &str =
    "windows-touch-gesture-setting-unknown";
pub(super) const TOUCHPAD_BACKEND_UNAVAILABLE_STATUS: &str = "touchpad-backend-unavailable";
const TOUCHPAD_SETTINGS_URI: &str = "ms-settings:devices-touch";

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
pub(super) enum SystemTouchGestureSetting {
    Disabled,
    Enabled,
    Unknown(String),
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

pub(super) fn assess_touchpad_override_with_system_setting(
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
