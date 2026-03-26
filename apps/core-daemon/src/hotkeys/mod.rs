use std::sync::mpsc::Sender;

use flowtile_config_rules::HotkeyBinding;
use flowtile_domain::BindControlMode;

use crate::control::ControlMessage;

#[cfg(not(windows))]
compile_error!("flowtile-core-daemon hotkey runtime currently supports only Windows builds.");

#[cfg(windows)]
mod low_level;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod trigger;

#[cfg(windows)]
use native::{NativeHotkeyRuntime, spawn_native};

enum HotkeyBackend {
    #[cfg(windows)]
    Native(NativeHotkeyRuntime),
}

pub struct HotkeyListener {
    backend: HotkeyBackend,
}

#[derive(Debug)]
pub enum HotkeyListenerError {
    Io(std::io::Error),
    Startup(String),
}

impl std::fmt::Display for HotkeyListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::Startup(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for HotkeyListenerError {}

impl From<std::io::Error> for HotkeyListenerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl HotkeyListener {
    pub fn spawn(
        bindings: &[HotkeyBinding],
        bind_control_mode: BindControlMode,
        command_sender: Sender<ControlMessage>,
    ) -> Result<Option<Self>, HotkeyListenerError> {
        ensure_bind_control_mode_supported(bind_control_mode)?;

        #[cfg(windows)]
        {
            spawn_native(bindings, command_sender).map(|runtime| {
                runtime.map(|runtime| Self {
                    backend: HotkeyBackend::Native(runtime),
                })
            })
        }
    }
}

pub fn ensure_bind_control_mode_supported(
    bind_control_mode: BindControlMode,
) -> Result<(), HotkeyListenerError> {
    match bind_control_mode {
        BindControlMode::Coexistence => Ok(()),
        _ => Err(HotkeyListenerError::Startup(format!(
            "bind control mode '{}' is not supported by this build yet; only 'coexistence' is available",
            bind_control_mode.as_str()
        ))),
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        match &mut self.backend {
            #[cfg(windows)]
            HotkeyBackend::Native(runtime) => runtime.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use flowtile_domain::BindControlMode;

    use super::ensure_bind_control_mode_supported;

    #[test]
    fn rejects_unsupported_bind_control_mode_until_deeper_runtime_exists() {
        let error = ensure_bind_control_mode_supported(BindControlMode::ManagedShell)
            .expect_err("managed-shell should be rejected for now");
        assert!(error.to_string().contains("managed-shell"));
    }
}
