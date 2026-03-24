use std::sync::mpsc::Sender;

use flowtile_config_rules::HotkeyBinding;

use crate::{ControlMessage, WatchCommand};

#[cfg(not(windows))]
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
};
#[cfg(windows)]
use std::{
    mem::zeroed,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};
#[cfg(windows)]
use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
            UnregisterHotKey,
        },
        WindowsAndMessaging::{
            GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
        },
    },
};

#[cfg(not(windows))]
const HOTKEY_SCRIPT_NAME: &str = "observe-hotkeys.ps1";

enum HotkeyBackend {
    #[cfg(windows)]
    Native(NativeHotkeyRuntime),
    #[cfg(not(windows))]
    Script(ScriptHotkeyRuntime),
}

pub struct HotkeyListener {
    backend: HotkeyBackend,
}

#[cfg(windows)]
struct NativeHotkeyRuntime {
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

#[cfg(not(windows))]
struct ScriptHotkeyRuntime {
    child: Child,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub enum HotkeyListenerError {
    Io(std::io::Error),
    #[cfg(not(windows))]
    Json(serde_json::Error),
    Startup(String),
    #[cfg(not(windows))]
    MissingStdout,
    #[cfg(not(windows))]
    MissingStderr,
}

impl std::fmt::Display for HotkeyListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            #[cfg(not(windows))]
            Self::Json(source) => source.fmt(formatter),
            Self::Startup(message) => formatter.write_str(message),
            #[cfg(not(windows))]
            Self::MissingStdout => formatter.write_str("hotkey listener missing stdout pipe"),
            #[cfg(not(windows))]
            Self::MissingStderr => formatter.write_str("hotkey listener missing stderr pipe"),
        }
    }
}

impl std::error::Error for HotkeyListenerError {}

impl From<std::io::Error> for HotkeyListenerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(not(windows))]
impl From<serde_json::Error> for HotkeyListenerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl HotkeyListener {
    pub fn spawn(
        bindings: &[HotkeyBinding],
        command_sender: Sender<ControlMessage>,
    ) -> Result<Option<Self>, HotkeyListenerError> {
        #[cfg(windows)]
        {
            spawn_native(bindings, command_sender)
        }

        #[cfg(not(windows))]
        {
            spawn_script(bindings, command_sender)
        }
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        match &mut self.backend {
            #[cfg(windows)]
            HotkeyBackend::Native(runtime) => {
                let _ = {
                    // SAFETY: `thread_id` belongs to the hotkey thread created by this runtime,
                    // and `WM_QUIT` is the documented way to stop its message loop.
                    unsafe { PostThreadMessageW(runtime.thread_id, WM_QUIT, 0, 0) }
                };
                if let Some(worker) = runtime.worker.take() {
                    let _ = worker.join();
                }
            }
            #[cfg(not(windows))]
            HotkeyBackend::Script(runtime) => {
                let _ = runtime.child.kill();
                let _ = runtime.child.wait();

                if let Some(stdout_thread) = runtime.stdout_thread.take() {
                    let _ = stdout_thread.join();
                }
                if let Some(stderr_thread) = runtime.stderr_thread.take() {
                    let _ = stderr_thread.join();
                }
            }
        }
    }
}

#[cfg(windows)]
fn spawn_native(
    bindings: &[HotkeyBinding],
    command_sender: Sender<ControlMessage>,
) -> Result<Option<HotkeyListener>, HotkeyListenerError> {
    let registrations = bindings
        .iter()
        .filter_map(|binding| {
            let command = map_command_name(&binding.command)?;
            match parse_trigger(&binding.trigger) {
                Ok(parsed) => Some(NativeHotkeyRegistration {
                    trigger: binding.trigger.clone(),
                    command,
                    modifiers: parsed.modifiers,
                    key: parsed.key,
                }),
                Err(message) => {
                    eprintln!(
                        "hotkey warning for {} ({}): {}",
                        binding.trigger, binding.command, message
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();

    if registrations.is_empty() {
        return Ok(None);
    }

    let (startup_sender, startup_receiver) = mpsc::channel::<Result<HotkeyStartup, String>>();
    let worker = thread::spawn(move || {
        run_hotkey_thread(registrations, command_sender, startup_sender);
    });

    let startup = startup_receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| {
            HotkeyListenerError::Startup(format!("hotkey listener startup timed out: {error}"))
        })?
        .map_err(HotkeyListenerError::Startup)?;

    if startup.registered_count == 0 {
        let _ = worker.join();
        return Ok(None);
    }

    Ok(Some(HotkeyListener {
        backend: HotkeyBackend::Native(NativeHotkeyRuntime {
            thread_id: startup.thread_id,
            worker: Some(worker),
        }),
    }))
}

#[cfg(windows)]
fn run_hotkey_thread(
    registrations: Vec<NativeHotkeyRegistration>,
    command_sender: Sender<ControlMessage>,
    startup_sender: mpsc::Sender<Result<HotkeyStartup, String>>,
) {
    ensure_message_queue();
    let thread_id = {
        // SAFETY: `GetCurrentThreadId` is a parameterless Win32 query for the current thread.
        unsafe { GetCurrentThreadId() }
    };

    let mut registered_ids = Vec::new();
    let mut registration_by_id = Vec::new();
    for (index, registration) in registrations.into_iter().enumerate() {
        let hotkey_id = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let registered = {
            // SAFETY: We pass the current thread message queue as the registration target and use
            // a stable id/modifier/key tuple derived from validated config bindings.
            unsafe {
                RegisterHotKey(
                    std::ptr::null_mut(),
                    hotkey_id,
                    registration.modifiers,
                    registration.key,
                ) != 0
            }
        };
        if !registered {
            let message = last_error_message("RegisterHotKey");
            eprintln!(
                "hotkey warning for {} ({}): {}",
                registration.trigger,
                watch_command_name(registration.command),
                message
            );
            continue;
        }

        registered_ids.push(hotkey_id);
        registration_by_id.push((hotkey_id, registration));
    }

    let _ = startup_sender.send(Ok(HotkeyStartup {
        thread_id,
        registered_count: registered_ids.len(),
    }));
    if registered_ids.is_empty() {
        return;
    }

    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 message structure that is valid when zero-initialized.
        unsafe { zeroed() }
    };
    loop {
        let status = {
            // SAFETY: `GetMessageW` reads messages for the current thread message queue.
            unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) }
        };
        if status <= 0 {
            break;
        }
        if message.message != WM_HOTKEY {
            continue;
        }

        let hotkey_id = message.wParam as i32;
        let command = registration_by_id
            .iter()
            .find_map(|(candidate_id, registration)| {
                (*candidate_id == hotkey_id).then_some(registration.command)
            });
        let Some(command) = command else {
            continue;
        };

        if command_sender.send(ControlMessage::Watch(command)).is_err() {
            break;
        }
    }

    for hotkey_id in registered_ids {
        let _ = {
            // SAFETY: `hotkey_id` was previously returned as successfully registered for the
            // current thread and is being unregistered exactly once during shutdown.
            unsafe { UnregisterHotKey(std::ptr::null_mut(), hotkey_id) }
        };
    }
}

#[cfg(windows)]
fn ensure_message_queue() {
    let mut message: MSG = {
        // SAFETY: `MSG` is a plain Win32 message structure that is valid when zero-initialized.
        unsafe { zeroed() }
    };
    let _ = {
        // SAFETY: `PeekMessageW` with `PM_NOREMOVE` forces the current thread to own a message
        // queue before hotkeys are registered against it.
        unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) }
    };
}

#[cfg(windows)]
fn parse_trigger(trigger: &str) -> Result<ParsedTrigger, String> {
    let tokens = trigger
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err("empty hotkey trigger".to_string());
    }

    let mut modifiers = 0u32;
    let mut key_token = None;

    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "alt" => modifiers |= MOD_ALT,
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "windows" => modifiers |= MOD_WIN,
            _ => {
                if key_token.is_some() {
                    return Err(format!(
                        "hotkey trigger '{trigger}' contains more than one non-modifier token"
                    ));
                }
                key_token = Some(token.to_string());
            }
        }
    }

    let Some(key_token) = key_token else {
        return Err(format!("hotkey trigger '{trigger}' does not contain a key"));
    };

    Ok(ParsedTrigger {
        modifiers: modifiers | MOD_NOREPEAT,
        key: resolve_virtual_key(&key_token)?,
    })
}

#[cfg(windows)]
fn resolve_virtual_key(token: &str) -> Result<u32, String> {
    let normalized = token.trim().to_ascii_uppercase();
    if normalized.len() == 1 {
        let value = normalized.as_bytes()[0];
        if value.is_ascii_uppercase() || value.is_ascii_digit() {
            return Ok(u32::from(value));
        }
    }

    match normalized.as_str() {
        "SPACE" => Ok(0x20),
        "TAB" => Ok(0x09),
        "ENTER" => Ok(0x0D),
        "ESC" | "ESCAPE" => Ok(0x1B),
        "BACKSPACE" => Ok(0x08),
        "DELETE" | "DEL" => Ok(0x2E),
        "HOME" => Ok(0x24),
        "END" => Ok(0x23),
        "PAGEUP" | "PGUP" => Ok(0x21),
        "PAGEDOWN" | "PGDN" => Ok(0x22),
        "LEFT" => Ok(0x25),
        "UP" => Ok(0x26),
        "RIGHT" => Ok(0x27),
        "DOWN" => Ok(0x28),
        _ if normalized.starts_with('F') => {
            let suffix = normalized.trim_start_matches('F');
            let number = suffix
                .parse::<u32>()
                .map_err(|_| format!("unsupported hotkey key token '{token}'"))?;
            if (1..=24).contains(&number) {
                Ok(0x70 + number - 1)
            } else {
                Err(format!("unsupported hotkey key token '{token}'"))
            }
        }
        _ => Err(format!("unsupported hotkey key token '{token}'")),
    }
}

#[cfg(windows)]
fn watch_command_name(command: WatchCommand) -> &'static str {
    match command {
        WatchCommand::FocusNext => "focus-next",
        WatchCommand::FocusPrev => "focus-prev",
        WatchCommand::ScrollLeft => "scroll-strip-left",
        WatchCommand::ScrollRight => "scroll-strip-right",
        WatchCommand::ToggleFloating => "toggle-floating",
        WatchCommand::ToggleTabbed => "toggle-tabbed",
        WatchCommand::ToggleMaximized => "toggle-maximized",
        WatchCommand::ToggleFullscreen => "toggle-fullscreen",
        WatchCommand::ToggleOverview => "toggle-overview",
        WatchCommand::ReloadConfig => "reload-config",
        WatchCommand::Snapshot => "snapshot",
        WatchCommand::Unwind => "disable-management-and-unwind",
        WatchCommand::Rescan => "rescan",
        WatchCommand::Quit => "quit",
    }
}

#[cfg(windows)]
fn last_error_message(api: &str) -> String {
    let code = {
        // SAFETY: Reading the thread-local Win32 last-error code immediately after a failed API
        // call is the intended contract of `GetLastError`.
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    };
    format!("{api} failed with Win32 error {code}")
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct ParsedTrigger {
    modifiers: u32,
    key: u32,
}

#[cfg(windows)]
#[derive(Clone)]
struct NativeHotkeyRegistration {
    trigger: String,
    command: WatchCommand,
    modifiers: u32,
    key: u32,
}

#[cfg(windows)]
struct HotkeyStartup {
    thread_id: u32,
    registered_count: usize,
}

#[cfg(not(windows))]
fn spawn_script(
    bindings: &[HotkeyBinding],
    command_sender: Sender<ControlMessage>,
) -> Result<Option<HotkeyListener>, HotkeyListenerError> {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    struct HotkeyRegistrationRequest {
        hotkeys: Vec<HotkeyRegistration>,
    }

    #[derive(Serialize)]
    struct HotkeyRegistration {
        trigger: String,
        command: String,
    }

    #[derive(Deserialize)]
    struct HotkeyScriptEvent {
        kind: String,
        #[serde(default)]
        trigger: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }

    let registrations = bindings
        .iter()
        .filter(|binding| map_command_name(&binding.command).is_some())
        .map(|binding| HotkeyRegistration {
            trigger: binding.trigger.clone(),
            command: binding.command.clone(),
        })
        .collect::<Vec<_>>();

    if registrations.is_empty() {
        return Ok(None);
    }

    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(HOTKEY_SCRIPT_NAME);
    let payload = serde_json::to_vec(&HotkeyRegistrationRequest {
        hotkeys: registrations,
    })?;

    let mut command = Command::new("pwsh");
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload)?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or(HotkeyListenerError::MissingStdout)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(HotkeyListenerError::MissingStderr)?;

    let stdout_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<HotkeyScriptEvent>(line) {
                        Ok(event) => match event.kind.as_str() {
                            "command" => {
                                let Some(command_name) = event.command.as_deref() else {
                                    eprintln!(
                                        "hotkey listener emitted command event without command"
                                    );
                                    continue;
                                };
                                let Some(command) = map_command_name(command_name) else {
                                    eprintln!(
                                        "hotkey listener emitted unsupported command '{command_name}'"
                                    );
                                    continue;
                                };
                                if command_sender.send(ControlMessage::Watch(command)).is_err() {
                                    break;
                                }
                            }
                            "warning" => {
                                let trigger = event.trigger.unwrap_or_else(|| "?".to_string());
                                let command = event.command.unwrap_or_else(|| "?".to_string());
                                let message = event
                                    .message
                                    .unwrap_or_else(|| "unknown hotkey warning".to_string());
                                eprintln!("hotkey warning for {trigger} ({command}): {message}");
                            }
                            other => {
                                eprintln!(
                                    "hotkey listener emitted unsupported event kind '{other}'"
                                );
                            }
                        },
                        Err(error) => {
                            eprintln!("hotkey listener returned invalid json: {error}");
                            break;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("failed to read hotkey listener stdout: {error}");
                    break;
                }
            }
        }
    });

    let stderr_thread = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    eprintln!("hotkey listener: {line}");
                }
                Err(error) => {
                    eprintln!("failed to read hotkey listener stderr: {error}");
                    break;
                }
            }
        }
    });

    Ok(Some(HotkeyListener {
        backend: HotkeyBackend::Script(ScriptHotkeyRuntime {
            child,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        }),
    }))
}

fn map_command_name(command: &str) -> Option<WatchCommand> {
    match command {
        "focus-next" => Some(WatchCommand::FocusNext),
        "focus-prev" => Some(WatchCommand::FocusPrev),
        "scroll-strip-left" => Some(WatchCommand::ScrollLeft),
        "scroll-strip-right" => Some(WatchCommand::ScrollRight),
        "toggle-floating" => Some(WatchCommand::ToggleFloating),
        "toggle-tabbed" => Some(WatchCommand::ToggleTabbed),
        "toggle-maximized" => Some(WatchCommand::ToggleMaximized),
        "toggle-fullscreen" => Some(WatchCommand::ToggleFullscreen),
        "toggle-overview" => Some(WatchCommand::ToggleOverview),
        "reload-config" => Some(WatchCommand::ReloadConfig),
        "disable-management-and-unwind" => Some(WatchCommand::Unwind),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::{parse_trigger, resolve_virtual_key};

    #[cfg(windows)]
    #[test]
    fn parses_super_control_hotkey() {
        let parsed = parse_trigger("Win+Ctrl+L").expect("trigger should parse");
        assert_ne!(parsed.modifiers, 0);
        assert_eq!(parsed.key, u32::from(b'L'));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_multiple_non_modifier_tokens() {
        let error = parse_trigger("Win+Ctrl+L+K").expect_err("trigger should fail");
        assert!(error.contains("more than one non-modifier"));
    }

    #[cfg(windows)]
    #[test]
    fn resolves_function_keys() {
        assert_eq!(resolve_virtual_key("F1").expect("F1 should parse"), 0x70);
        assert_eq!(resolve_virtual_key("F24").expect("F24 should parse"), 0x87);
    }
}
