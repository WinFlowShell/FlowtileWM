use std::collections::HashMap;

use flowtile_config_rules::{TouchpadConfig, TouchpadGestureBinding};

use crate::control::WatchCommand;

use super::TouchpadListenerError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TouchpadGesture {
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
    pub(super) fn parse(value: &str) -> Result<Self, TouchpadListenerError> {
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
pub(super) struct TouchpadBindingSet {
    bindings: HashMap<TouchpadGesture, WatchCommand>,
}

impl TouchpadBindingSet {
    pub(super) fn from_config(config: &TouchpadConfig) -> Result<Self, TouchpadListenerError> {
        let mut bindings = HashMap::new();
        for binding in &config.gestures {
            let normalized = normalize_binding(binding)?;
            bindings.insert(normalized.gesture, normalized.command);
        }

        Ok(Self { bindings })
    }

    pub(super) fn len(&self) -> usize {
        self.bindings.len()
    }

    pub(super) fn command_for(&self, gesture: TouchpadGesture) -> Option<WatchCommand> {
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

    command.as_ipc_command_name().map(Some).ok_or_else(|| {
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
