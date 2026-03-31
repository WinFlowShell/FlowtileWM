use std::{
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
};

use flowtile_config_rules::TouchpadConfig;

use crate::control::ControlMessage;

use super::{
    TouchpadListenerError,
    assessment::ensure_touchpad_override_supported,
    bindings::{TouchpadBindingSet, TouchpadGesture},
};

#[cfg(windows)]
use super::native::NativeTouchpadRuntime;

#[derive(Debug)]
pub(super) enum TouchpadRuntimeEvent {
    Gesture(TouchpadGesture),
    Shutdown,
}

#[derive(Debug)]
pub(super) struct TouchpadGestureRuntime {
    pub(super) event_sender: Sender<TouchpadRuntimeEvent>,
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
    pub(super) fn dispatch_gesture(
        &self,
        gesture: TouchpadGesture,
    ) -> Result<(), TouchpadListenerError> {
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

    pub(super) fn spawn_runtime_only(
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
    pub(super) fn dispatch_gesture(
        &self,
        gesture: TouchpadGesture,
    ) -> Result<(), TouchpadListenerError> {
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
