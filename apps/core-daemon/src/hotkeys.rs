use std::sync::mpsc::Sender;

use flowtile_config_rules::HotkeyBinding;
use flowtile_domain::BindControlMode;

use crate::{ControlMessage, WatchCommand};

#[cfg(windows)]
use std::{
    collections::{HashMap, HashSet},
    mem::zeroed,
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};
#[cfg(not(windows))]
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
};
#[cfg(windows)]
use windows_sys::Win32::{
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
            UnregisterHotKey, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
            VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE,
            PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
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
        bind_control_mode: BindControlMode,
        command_sender: Sender<ControlMessage>,
    ) -> Result<Option<Self>, HotkeyListenerError> {
        ensure_bind_control_mode_supported(bind_control_mode)?;

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
            HotkeyBackend::Native(runtime) => {
                let _ = { unsafe { PostThreadMessageW(runtime.thread_id, WM_QUIT, 0, 0) } };
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
static LOW_LEVEL_HOOK_RUNTIMES: OnceLock<Mutex<HashMap<u32, Arc<LowLevelHotkeyRuntime>>>> =
    OnceLock::new();

#[cfg(windows)]
const LOW_LEVEL_REPEAT_INITIAL_DELAY: Duration = Duration::from_millis(180);

#[cfg(windows)]
const LOW_LEVEL_REPEAT_INTERVAL: Duration = Duration::from_millis(45);

#[cfg(windows)]
struct LowLevelHotkeyRuntime {
    command_sender: Sender<ControlMessage>,
    state: Mutex<LowLevelHotkeyState>,
    repeat_loop: Mutex<Option<ActiveRepeatLoop>>,
}

#[cfg(windows)]
impl LowLevelHotkeyRuntime {
    fn new(
        fallback_registrations: Vec<NativeHotkeyRegistration>,
        command_sender: Sender<ControlMessage>,
    ) -> Self {
        Self {
            command_sender,
            state: Mutex::new(LowLevelHotkeyState::new(fallback_registrations)),
            repeat_loop: Mutex::new(None),
        }
    }

    fn handle_key_event(&self, vk: u32, message: u32, injected: bool) -> HookDecision {
        let (decision, repeat_command) = match self.state.lock() {
            Ok(mut state) => {
                let decision = state.handle_key_event(vk, message, injected);
                let repeat_command = state.repeat_command_while_held();
                (decision, repeat_command)
            }
            Err(_) => (HookDecision::default(), None),
        };

        self.sync_repeat_loop(repeat_command);
        decision
    }

    fn sync_repeat_loop(&self, command: Option<WatchCommand>) {
        let Ok(mut repeat_loop) = self.repeat_loop.lock() else {
            return;
        };

        if repeat_loop
            .as_ref()
            .is_some_and(|active| Some(active.command) == command)
        {
            return;
        }

        if let Some(active) = repeat_loop.take() {
            active.stop();
        }

        if let Some(command) = command {
            *repeat_loop = Some(ActiveRepeatLoop::spawn(
                command,
                self.command_sender.clone(),
            ));
        }
    }

    fn stop_repeat_loop(&self) {
        if let Ok(mut repeat_loop) = self.repeat_loop.lock()
            && let Some(active) = repeat_loop.take()
        {
            active.stop();
        }
    }
}

#[cfg(windows)]
impl Drop for LowLevelHotkeyRuntime {
    fn drop(&mut self) {
        self.stop_repeat_loop();
    }
}

#[cfg(windows)]
#[derive(Default)]
struct LowLevelHotkeyState {
    fallback_registrations: Vec<NativeHotkeyRegistration>,
    pressed_keys: HashSet<u32>,
    active_trigger: Option<ActiveLowLevelTrigger>,
}

#[cfg(windows)]
impl LowLevelHotkeyState {
    fn new(fallback_registrations: Vec<NativeHotkeyRegistration>) -> Self {
        Self {
            fallback_registrations,
            pressed_keys: HashSet::new(),
            active_trigger: None,
        }
    }

    fn handle_key_event(&mut self, vk: u32, message: u32, injected: bool) -> HookDecision {
        if injected || !is_keyboard_message(message) {
            return HookDecision::default();
        }

        if is_key_down_message(message) {
            let inserted = self.pressed_keys.insert(vk);

            if let Some(active) = &self.active_trigger
                && active.primary_key == vk
            {
                return HookDecision {
                    command: None,
                    suppress: true,
                };
            }

            if inserted {
                let active_modifiers = active_modifier_mask(&self.pressed_keys);
                if let Some(registration) = self.fallback_registrations.iter().find(|candidate| {
                    candidate.key == vk && candidate.required_modifiers == active_modifiers
                }) {
                    self.active_trigger = Some(ActiveLowLevelTrigger {
                        command: registration.command,
                        primary_key: vk,
                        required_modifiers: registration.required_modifiers,
                        repeat_while_held: registration.command.repeats_while_held(),
                    });
                    return HookDecision {
                        command: Some(registration.command),
                        suppress: true,
                    };
                }
            }
        } else if is_key_up_message(message) {
            let suppress = self
                .active_trigger
                .as_ref()
                .is_some_and(|active| active.primary_key == vk);
            self.pressed_keys.remove(&vk);

            if self.active_trigger.as_ref().is_some_and(|active| {
                !self.pressed_keys.contains(&active.primary_key)
                    || (active_modifier_mask(&self.pressed_keys) & active.required_modifiers)
                        != active.required_modifiers
            }) {
                self.active_trigger = None;
            }

            return HookDecision {
                command: None,
                suppress,
            };
        }

        HookDecision::default()
    }

    fn repeat_command_while_held(&self) -> Option<WatchCommand> {
        self.active_trigger
            .as_ref()
            .and_then(|active| active.repeat_while_held.then_some(active.command))
    }
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct ActiveLowLevelTrigger {
    command: WatchCommand,
    primary_key: u32,
    required_modifiers: u32,
    repeat_while_held: bool,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HookDecision {
    command: Option<WatchCommand>,
    suppress: bool,
}

#[cfg(windows)]
struct ActiveRepeatLoop {
    command: WatchCommand,
    stop_sender: mpsc::Sender<()>,
    worker: JoinHandle<()>,
}

#[cfg(windows)]
impl ActiveRepeatLoop {
    fn spawn(command: WatchCommand, command_sender: Sender<ControlMessage>) -> Self {
        let (stop_sender, stop_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            if stop_receiver
                .recv_timeout(LOW_LEVEL_REPEAT_INITIAL_DELAY)
                .is_ok()
            {
                return;
            }

            loop {
                if command_sender.send(ControlMessage::Watch(command)).is_err() {
                    break;
                }
                if stop_receiver
                    .recv_timeout(LOW_LEVEL_REPEAT_INTERVAL)
                    .is_ok()
                {
                    break;
                }
            }
        });

        Self {
            command,
            stop_sender,
            worker,
        }
    }

    fn stop(self) {
        let _ = self.stop_sender.send(());
        let _ = self.worker.join();
    }
}

#[cfg(windows)]
fn low_level_hook_runtimes() -> &'static Mutex<HashMap<u32, Arc<LowLevelHotkeyRuntime>>> {
    LOW_LEVEL_HOOK_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(windows)]
fn register_low_level_runtime(
    thread_id: u32,
    runtime: Arc<LowLevelHotkeyRuntime>,
) -> Result<(), String> {
    low_level_hook_runtimes()
        .lock()
        .map_err(|_| "low-level hotkey runtime registry poisoned".to_string())?
        .insert(thread_id, runtime);
    Ok(())
}

#[cfg(windows)]
fn unregister_low_level_runtime(thread_id: u32) {
    if let Ok(mut runtimes) = low_level_hook_runtimes().lock() {
        runtimes.remove(&thread_id);
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
                    register_modifiers: parsed.register_modifiers,
                    required_modifiers: parsed.required_modifiers,
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

    if startup.active_registration_count == 0 {
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
    let thread_id = unsafe { GetCurrentThreadId() };

    let mut registered_ids = Vec::new();
    let mut registration_by_id = Vec::new();
    let mut fallback_registrations = Vec::new();

    for (index, registration) in registrations.into_iter().enumerate() {
        let hotkey_id = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let registered = unsafe {
            RegisterHotKey(
                std::ptr::null_mut(),
                hotkey_id,
                registration.register_modifiers,
                registration.key,
            ) != 0
        };
        if !registered {
            eprintln!(
                "hotkey warning for {} ({}): {}; using low-level hook fallback",
                registration.trigger,
                watch_command_name(registration.command),
                last_error_message("RegisterHotKey")
            );
            fallback_registrations.push(registration);
            continue;
        }

        registered_ids.push(hotkey_id);
        registration_by_id.push((hotkey_id, registration.command));
    }

    let fallback_count = fallback_registrations.len();
    let mut low_level_hook = None;
    let mut active_low_level_count = 0usize;
    if fallback_count > 0 {
        match install_low_level_hook(thread_id, fallback_registrations, command_sender.clone()) {
            Ok(hook) => {
                low_level_hook = Some(hook);
                active_low_level_count = fallback_count;
            }
            Err(message) => {
                eprintln!("hotkey warning: low-level hook startup failed: {message}");
            }
        }
    }

    if registered_ids.is_empty() && active_low_level_count == 0 {
        let _ = startup_sender.send(Err("no hotkeys could be activated".to_string()));
        return;
    }

    let _ = startup_sender.send(Ok(HotkeyStartup {
        thread_id,
        active_registration_count: registered_ids.len() + active_low_level_count,
    }));

    let mut message: MSG = unsafe { zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if status <= 0 {
            break;
        }
        if message.message != WM_HOTKEY {
            continue;
        }

        let hotkey_id = message.wParam as i32;
        let command = registration_by_id
            .iter()
            .find_map(|(candidate_id, command)| (*candidate_id == hotkey_id).then_some(*command));
        let Some(command) = command else {
            continue;
        };

        if command_sender.send(ControlMessage::Watch(command)).is_err() {
            break;
        }
    }

    if let Some(hook) = low_level_hook {
        let _ = unsafe { UnhookWindowsHookEx(hook) };
        unregister_low_level_runtime(thread_id);
    }

    for hotkey_id in registered_ids {
        let _ = unsafe { UnregisterHotKey(std::ptr::null_mut(), hotkey_id) };
    }
}

#[cfg(windows)]
fn install_low_level_hook(
    thread_id: u32,
    fallback_registrations: Vec<NativeHotkeyRegistration>,
    command_sender: Sender<ControlMessage>,
) -> Result<HHOOK, String> {
    let runtime = Arc::new(LowLevelHotkeyRuntime::new(
        fallback_registrations,
        command_sender,
    ));
    register_low_level_runtime(thread_id, runtime)?;

    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), module, 0) };
    if hook.is_null() {
        unregister_low_level_runtime(thread_id);
        return Err(last_error_message("SetWindowsHookExW"));
    }

    Ok(hook)
}

#[cfg(windows)]
unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if code < 0 || lparam == 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let message = wparam as u32;
    if !is_keyboard_message(message) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    let runtime = low_level_hook_runtimes()
        .lock()
        .ok()
        .and_then(|runtimes| runtimes.get(&thread_id).cloned());
    let Some(runtime) = runtime else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    let hook_data = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let injected = (hook_data.flags & LLKHF_INJECTED) != 0;
    let decision = runtime.handle_key_event(hook_data.vkCode, message, injected);
    if let Some(command) = decision.command {
        let _ = runtime.command_sender.send(ControlMessage::Watch(command));
    }
    if decision.suppress {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(windows)]
fn ensure_message_queue() {
    let mut message: MSG = unsafe { zeroed() };
    let _ = unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };
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

    let mut required_modifiers = 0u32;
    let mut key_token = None;

    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "alt" => required_modifiers |= MOD_ALT,
            "ctrl" | "control" => required_modifiers |= MOD_CONTROL,
            "shift" => required_modifiers |= MOD_SHIFT,
            "win" | "windows" => required_modifiers |= MOD_WIN,
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
        register_modifiers: required_modifiers | MOD_NOREPEAT,
        required_modifiers,
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
fn is_keyboard_message(message: u32) -> bool {
    matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
}

#[cfg(windows)]
fn is_key_down_message(message: u32) -> bool {
    matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN)
}

#[cfg(windows)]
fn is_key_up_message(message: u32) -> bool {
    matches!(message, WM_KEYUP | WM_SYSKEYUP)
}

#[cfg(windows)]
fn active_modifier_mask(pressed_keys: &HashSet<u32>) -> u32 {
    let mut mask = 0u32;
    if pressed_keys.iter().any(|key| is_control_vk(*key)) {
        mask |= MOD_CONTROL;
    }
    if pressed_keys.iter().any(|key| is_alt_vk(*key)) {
        mask |= MOD_ALT;
    }
    if pressed_keys.iter().any(|key| is_shift_vk(*key)) {
        mask |= MOD_SHIFT;
    }
    if pressed_keys.iter().any(|key| is_win_vk(*key)) {
        mask |= MOD_WIN;
    }
    mask
}

#[cfg(windows)]
fn is_control_vk(vk: u32) -> bool {
    vk == u32::from(VK_CONTROL) || vk == u32::from(VK_LCONTROL) || vk == u32::from(VK_RCONTROL)
}

#[cfg(windows)]
fn is_alt_vk(vk: u32) -> bool {
    vk == u32::from(VK_MENU) || vk == u32::from(VK_LMENU) || vk == u32::from(VK_RMENU)
}

#[cfg(windows)]
fn is_shift_vk(vk: u32) -> bool {
    vk == u32::from(VK_SHIFT) || vk == u32::from(VK_LSHIFT) || vk == u32::from(VK_RSHIFT)
}

#[cfg(windows)]
fn is_win_vk(vk: u32) -> bool {
    vk == u32::from(VK_LWIN) || vk == u32::from(VK_RWIN)
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
impl WatchCommand {
    fn repeats_while_held(self) -> bool {
        matches!(self, Self::ScrollLeft | Self::ScrollRight)
    }
}

#[cfg(windows)]
fn last_error_message(api: &str) -> String {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("{api} failed with Win32 error {code}")
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct ParsedTrigger {
    register_modifiers: u32,
    required_modifiers: u32,
    key: u32,
}

#[cfg(windows)]
#[derive(Clone)]
struct NativeHotkeyRegistration {
    trigger: String,
    command: WatchCommand,
    register_modifiers: u32,
    required_modifiers: u32,
    key: u32,
}

#[cfg(windows)]
struct HotkeyStartup {
    thread_id: u32,
    active_registration_count: usize,
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
    use std::time::Duration;

    #[cfg(windows)]
    use flowtile_domain::BindControlMode;

    #[cfg(windows)]
    use super::{
        ActiveRepeatLoop, HookDecision, LowLevelHotkeyState, NativeHotkeyRegistration,
        ensure_bind_control_mode_supported, parse_trigger, resolve_virtual_key,
    };
    #[cfg(windows)]
    use crate::{ControlMessage, WatchCommand};
    #[cfg(windows)]
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            MOD_CONTROL, MOD_NOREPEAT, MOD_WIN, VK_LCONTROL, VK_LSHIFT, VK_LWIN,
        },
        WindowsAndMessaging::{WM_KEYDOWN, WM_KEYUP},
    };

    #[cfg(windows)]
    #[test]
    fn parses_super_control_hotkey() {
        let parsed = parse_trigger("Win+Ctrl+L").expect("trigger should parse");
        assert_eq!(parsed.required_modifiers, MOD_CONTROL | MOD_WIN);
        assert_eq!(
            parsed.register_modifiers,
            MOD_CONTROL | MOD_WIN | MOD_NOREPEAT
        );
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

    #[cfg(windows)]
    #[test]
    fn rejects_unsupported_bind_control_mode_until_deeper_runtime_exists() {
        let error = ensure_bind_control_mode_supported(BindControlMode::ManagedShell)
            .expect_err("managed-shell should be rejected for now");
        assert!(error.to_string().contains("managed-shell"));
    }

    #[cfg(windows)]
    #[test]
    fn low_level_scroll_state_repeats_while_held_and_suppresses_release() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+Ctrl+L".to_string(),
            command: WatchCommand::ScrollRight,
            register_modifiers: MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
            required_modifiers: MOD_CONTROL | MOD_WIN,
            key: u32::from(b'L'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LCONTROL), WM_KEYDOWN, false),
            HookDecision::default()
        );

        let trigger = state.handle_key_event(u32::from(b'L'), WM_KEYDOWN, false);
        assert_eq!(
            trigger,
            HookDecision {
                command: Some(WatchCommand::ScrollRight),
                suppress: true,
            }
        );

        assert_eq!(
            state.repeat_command_while_held(),
            Some(WatchCommand::ScrollRight)
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'L'), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
            }
        );

        assert!(
            state
                .handle_key_event(u32::from(b'L'), WM_KEYUP, false)
                .suppress
        );
        assert_eq!(state.repeat_command_while_held(), None);
        assert!(
            !state
                .handle_key_event(u32::from(VK_LCONTROL), WM_KEYUP, false)
                .suppress
        );
        assert!(
            !state
                .handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false)
                .suppress
        );
    }

    #[cfg(windows)]
    #[test]
    fn low_level_non_repeat_command_fires_once_while_held() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+Ctrl+F".to_string(),
            command: WatchCommand::ToggleFloating,
            register_modifiers: MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
            required_modifiers: MOD_CONTROL | MOD_WIN,
            key: u32::from(b'F'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LCONTROL), WM_KEYDOWN, false),
            HookDecision::default()
        );

        assert_eq!(
            state.handle_key_event(u32::from(b'F'), WM_KEYDOWN, false),
            HookDecision {
                command: Some(WatchCommand::ToggleFloating),
                suppress: true,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'F'), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
            }
        );
        assert_eq!(state.repeat_command_while_held(), None);
    }

    #[cfg(windows)]
    #[test]
    fn low_level_state_does_not_match_with_extra_modifier() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+Ctrl+L".to_string(),
            command: WatchCommand::ScrollRight,
            register_modifiers: MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
            required_modifiers: MOD_CONTROL | MOD_WIN,
            key: u32::from(b'L'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LCONTROL), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LSHIFT), WM_KEYDOWN, false),
            HookDecision::default()
        );

        assert_eq!(
            state.handle_key_event(u32::from(b'L'), WM_KEYDOWN, false),
            HookDecision::default()
        );
    }

    #[cfg(windows)]
    #[test]
    fn low_level_state_ignores_injected_events() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+Ctrl+L".to_string(),
            command: WatchCommand::ScrollRight,
            register_modifiers: MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
            required_modifiers: MOD_CONTROL | MOD_WIN,
            key: u32::from(b'L'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(b'L'), WM_KEYDOWN, true),
            HookDecision::default()
        );
    }

    #[cfg(windows)]
    #[test]
    fn low_level_state_does_not_suppress_modifier_release_after_match() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+Ctrl+L".to_string(),
            command: WatchCommand::ScrollRight,
            register_modifiers: MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
            required_modifiers: MOD_CONTROL | MOD_WIN,
            key: u32::from(b'L'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LCONTROL), WM_KEYDOWN, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'L'), WM_KEYDOWN, false),
            HookDecision {
                command: Some(WatchCommand::ScrollRight),
                suppress: true,
            }
        );

        assert!(
            !state
                .handle_key_event(u32::from(VK_LCONTROL), WM_KEYUP, false)
                .suppress
        );
        assert!(
            !state
                .handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false)
                .suppress
        );
    }

    #[cfg(windows)]
    #[test]
    fn repeat_loop_emits_scroll_commands_until_stopped() {
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let repeat_loop = ActiveRepeatLoop::spawn(WatchCommand::ScrollRight, command_sender);

        let first = command_receiver
            .recv_timeout(Duration::from_millis(300))
            .expect("repeat loop should emit first command");
        assert!(matches!(
            first,
            ControlMessage::Watch(WatchCommand::ScrollRight)
        ));

        let second = command_receiver
            .recv_timeout(Duration::from_millis(120))
            .expect("repeat loop should emit repeated command");
        assert!(matches!(
            second,
            ControlMessage::Watch(WatchCommand::ScrollRight)
        ));

        repeat_loop.stop();
        assert!(
            command_receiver
                .recv_timeout(Duration::from_millis(80))
                .is_err()
        );
    }
}
