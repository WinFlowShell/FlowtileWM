use std::{
    collections::{HashMap, HashSet},
    mem::zeroed,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use windows_sys::Win32::{
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, SendInput, VK_CONTROL, VK_LCONTROL, VK_LMENU,
            VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE, PeekMessageW,
            SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

use crate::control::{ControlMessage, WatchCommand};
use crate::diag::write_runtime_log;

use super::native::{NativeHotkeyRegistration, last_error_message};

static LOW_LEVEL_HOOK_RUNTIMES: OnceLock<Mutex<HashMap<u32, Arc<LowLevelHotkeyRuntime>>>> =
    OnceLock::new();
static SUPER_HELD: AtomicBool = AtomicBool::new(false);

const LOW_LEVEL_REPEAT_INITIAL_DELAY: Duration = Duration::from_millis(180);
const LOW_LEVEL_REPEAT_INTERVAL: Duration = Duration::from_millis(45);

struct LowLevelHotkeyRuntime {
    command_sender: Sender<ControlMessage>,
    state: Mutex<LowLevelHotkeyState>,
    repeat_loop: Mutex<Option<ActiveRepeatLoop>>,
}

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

        if let Some(action) = decision.replay {
            replay_action(action);
        }
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

impl Drop for LowLevelHotkeyRuntime {
    fn drop(&mut self) {
        self.stop_repeat_loop();
    }
}

#[derive(Default)]
struct LowLevelHotkeyState {
    fallback_registrations: Vec<NativeHotkeyRegistration>,
    pressed_keys: HashSet<u32>,
    active_trigger: Option<ActiveLowLevelTrigger>,
    pending_win_prefix: Option<PendingWinPrefix>,
    suppressed_key_releases: HashSet<u32>,
}

impl LowLevelHotkeyState {
    fn new(fallback_registrations: Vec<NativeHotkeyRegistration>) -> Self {
        Self {
            fallback_registrations,
            pressed_keys: HashSet::new(),
            active_trigger: None,
            pending_win_prefix: None,
            suppressed_key_releases: HashSet::new(),
        }
    }

    fn handle_key_event(&mut self, vk: u32, message: u32, injected: bool) -> HookDecision {
        if injected || !is_keyboard_message(message) {
            return HookDecision::default();
        }

        if is_win_vk(vk) {
            if is_key_down_message(message) {
                SUPER_HELD.store(true, Ordering::Relaxed);
            } else if is_key_up_message(message) {
                SUPER_HELD.store(false, Ordering::Relaxed);
            }
        }

        if is_key_down_message(message) {
            let inserted = self.pressed_keys.insert(vk);

            if let Some(decision) = self.handle_pending_win_key_down(vk, inserted) {
                return decision;
            }

            if let Some(active) = &self.active_trigger
                && active.primary_key == vk
            {
                return HookDecision {
                    command: None,
                    suppress: true,
                    replay: None,
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
                        replay: None,
                    };
                }
            }
        } else if is_key_up_message(message) {
            let pending_prefix_decision = self.handle_pending_win_key_up(vk);
            let suppress = self
                .active_trigger
                .as_ref()
                .is_some_and(|active| active.primary_key == vk)
                || self.suppressed_key_releases.remove(&vk);
            self.pressed_keys.remove(&vk);

            if self.active_trigger.as_ref().is_some_and(|active| {
                !self.pressed_keys.contains(&active.primary_key)
                    || (active_modifier_mask(&self.pressed_keys) & active.required_modifiers)
                        != active.required_modifiers
            }) {
                self.active_trigger = None;
            }

            if let Some(mut decision) = pending_prefix_decision {
                decision.suppress |= suppress;
                return decision;
            }

            return HookDecision {
                command: None,
                suppress,
                replay: None,
            };
        }

        HookDecision::default()
    }

    fn repeat_command_while_held(&self) -> Option<WatchCommand> {
        self.active_trigger
            .as_ref()
            .and_then(|active| active.repeat_while_held.then_some(active.command))
    }

    fn handle_pending_win_key_down(&mut self, vk: u32, inserted: bool) -> Option<HookDecision> {
        if let Some(pending) = self.pending_win_prefix {
            if vk == pending.win_vk || is_win_vk(vk) {
                return Some(HookDecision {
                    command: None,
                    suppress: true,
                    replay: None,
                });
            }

            if active_modifier_mask(&self.pressed_keys) == MOD_WIN
                && let Some((command, required_modifiers, repeat_while_held)) =
                    self.find_pure_win_registration(vk).map(|registration| {
                        (
                            registration.command,
                            registration.required_modifiers,
                            registration.command.repeats_while_held(),
                        )
                    })
            {
                self.pending_win_prefix = None;
                self.active_trigger = Some(ActiveLowLevelTrigger {
                    command,
                    primary_key: vk,
                    required_modifiers,
                    repeat_while_held,
                });
                self.suppressed_key_releases.insert(pending.win_vk);
                return Some(HookDecision {
                    command: Some(command),
                    suppress: true,
                    replay: None,
                });
            }

            self.pending_win_prefix = None;
            return Some(HookDecision {
                command: None,
                suppress: true,
                replay: Some(ReplayAction::ReplayWinChord {
                    win_vk: pending.win_vk,
                    key_vk: vk,
                }),
            });
        }

        if inserted
            && is_win_vk(vk)
            && self.has_pure_win_bindings()
            && active_modifier_mask(&self.pressed_keys) == MOD_WIN
            && self.active_trigger.is_none()
        {
            self.pending_win_prefix = Some(PendingWinPrefix { win_vk: vk });
            return Some(HookDecision {
                command: None,
                suppress: true,
                replay: None,
            });
        }

        None
    }

    fn handle_pending_win_key_up(&mut self, vk: u32) -> Option<HookDecision> {
        if self
            .pending_win_prefix
            .as_ref()
            .is_some_and(|pending| pending.win_vk == vk)
        {
            self.pending_win_prefix = None;
            return Some(HookDecision {
                command: None,
                suppress: true,
                replay: Some(ReplayAction::WinTap { win_vk: vk }),
            });
        }

        None
    }

    fn has_pure_win_bindings(&self) -> bool {
        self.fallback_registrations
            .iter()
            .any(|registration| registration.required_modifiers == MOD_WIN)
    }

    fn find_pure_win_registration(&self, key: u32) -> Option<&NativeHotkeyRegistration> {
        self.fallback_registrations.iter().find(|registration| {
            registration.required_modifiers == MOD_WIN && registration.key == key
        })
    }
}

pub(crate) fn is_super_held_by_low_level_runtime() -> bool {
    SUPER_HELD.load(Ordering::Relaxed)
}

#[derive(Clone, Debug)]
struct ActiveLowLevelTrigger {
    command: WatchCommand,
    primary_key: u32,
    required_modifiers: u32,
    repeat_while_held: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingWinPrefix {
    win_vk: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayAction {
    WinTap { win_vk: u32 },
    ReplayWinChord { win_vk: u32, key_vk: u32 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HookDecision {
    command: Option<WatchCommand>,
    suppress: bool,
    replay: Option<ReplayAction>,
}

struct ActiveRepeatLoop {
    command: WatchCommand,
    stop_sender: mpsc::Sender<()>,
    worker: JoinHandle<()>,
}

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

fn low_level_hook_runtimes() -> &'static Mutex<HashMap<u32, Arc<LowLevelHotkeyRuntime>>> {
    LOW_LEVEL_HOOK_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup_low_level_runtime(thread_id: u32) -> Option<Arc<LowLevelHotkeyRuntime>> {
    let Ok(runtimes) = low_level_hook_runtimes().lock() else {
        return None;
    };

    runtimes.get(&thread_id).cloned().or_else(|| {
        (runtimes.len() == 1)
            .then(|| runtimes.values().next().cloned())
            .flatten()
    })
}

fn replay_action(action: ReplayAction) {
    match action {
        ReplayAction::WinTap { win_vk } => {
            send_virtual_key(win_vk, false);
            send_virtual_key(win_vk, true);
        }
        ReplayAction::ReplayWinChord { win_vk, key_vk } => {
            send_virtual_key(win_vk, false);
            send_virtual_key(key_vk, false);
        }
    }
}

fn send_virtual_key(vk: u32, key_up: bool) {
    let mut flags = if key_up { KEYEVENTF_KEYUP } else { 0 };
    if is_win_vk(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk as u16,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    let _ = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
}

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

fn unregister_low_level_runtime(thread_id: u32) {
    if let Ok(mut runtimes) = low_level_hook_runtimes().lock() {
        runtimes.remove(&thread_id);
    }
}

pub(super) fn install_low_level_hook(
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

pub(super) fn shutdown_low_level_hook(thread_id: u32, hook: HHOOK) {
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    unregister_low_level_runtime(thread_id);
}

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
    let runtime = lookup_low_level_runtime(thread_id);
    let Some(runtime) = runtime else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    let hook_data = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let injected = (hook_data.flags & LLKHF_INJECTED) != 0;
    let decision = runtime.handle_key_event(hook_data.vkCode, message, injected);
    if let Some(command) = decision.command {
        write_runtime_log(format!(
            "hotkey: low-level-dispatch thread_id={} vk={} message={} command={}",
            thread_id,
            hook_data.vkCode,
            message,
            command.as_hotkey_command_name()
        ));
        let _ = runtime.command_sender.send(ControlMessage::Watch(command));
    }
    if decision.suppress {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

pub(super) fn ensure_message_queue() {
    let mut message: MSG = unsafe { zeroed() };
    let _ = unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };
}

fn is_keyboard_message(message: u32) -> bool {
    matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
}

fn is_key_down_message(message: u32) -> bool {
    matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN)
}

fn is_key_up_message(message: u32) -> bool {
    matches!(message, WM_KEYUP | WM_SYSKEYUP)
}

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

fn is_control_vk(vk: u32) -> bool {
    vk == u32::from(VK_CONTROL) || vk == u32::from(VK_LCONTROL) || vk == u32::from(VK_RCONTROL)
}

fn is_alt_vk(vk: u32) -> bool {
    vk == u32::from(VK_MENU) || vk == u32::from(VK_LMENU) || vk == u32::from(VK_RMENU)
}

fn is_shift_vk(vk: u32) -> bool {
    vk == u32::from(VK_SHIFT) || vk == u32::from(VK_LSHIFT) || vk == u32::from(VK_RSHIFT)
}

fn is_win_vk(vk: u32) -> bool {
    vk == u32::from(VK_LWIN) || vk == u32::from(VK_RWIN)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            MOD_CONTROL, MOD_NOREPEAT, MOD_WIN, VK_LCONTROL, VK_LSHIFT, VK_LWIN,
        },
        WindowsAndMessaging::{WM_KEYDOWN, WM_KEYUP},
    };

    use crate::control::{ControlMessage, WatchCommand};

    use super::{ActiveRepeatLoop, HookDecision, LowLevelHotkeyState, ReplayAction};
    use crate::hotkeys::native::NativeHotkeyRegistration;

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
                replay: None,
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
                replay: None,
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
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'F'), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(state.repeat_command_while_held(), None);
    }

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
                replay: None,
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

    #[test]
    fn pure_win_chord_fires_without_replay_and_suppresses_win_release() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+H".to_string(),
            command: WatchCommand::FocusPrev,
            register_modifiers: MOD_WIN,
            required_modifiers: MOD_WIN,
            key: u32::from(b'H'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'H'), WM_KEYDOWN, false),
            HookDecision {
                command: Some(WatchCommand::FocusPrev),
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'H'), WM_KEYUP, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
    }

    #[test]
    fn pure_win_j_chord_focuses_next_without_input_leakage() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+J".to_string(),
            command: WatchCommand::FocusNext,
            register_modifiers: MOD_WIN,
            required_modifiers: MOD_WIN,
            key: u32::from(b'J'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'J'), WM_KEYDOWN, false),
            HookDecision {
                command: Some(WatchCommand::FocusNext),
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'J'), WM_KEYUP, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
    }

    #[test]
    fn unmatched_pure_win_key_replays_prefix_back_to_shell() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+H".to_string(),
            command: WatchCommand::FocusPrev,
            register_modifiers: MOD_WIN,
            required_modifiers: MOD_WIN,
            key: u32::from(b'H'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'E'), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: Some(ReplayAction::ReplayWinChord {
                    win_vk: u32::from(VK_LWIN),
                    key_vk: u32::from(b'E'),
                }),
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(b'E'), WM_KEYUP, false),
            HookDecision::default()
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false),
            HookDecision::default()
        );
    }

    #[test]
    fn standalone_win_tap_replays_start_keypath() {
        let mut state = LowLevelHotkeyState::new(vec![NativeHotkeyRegistration {
            trigger: "Win+H".to_string(),
            command: WatchCommand::FocusPrev,
            register_modifiers: MOD_WIN,
            required_modifiers: MOD_WIN,
            key: u32::from(b'H'),
        }]);

        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYDOWN, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: None,
            }
        );
        assert_eq!(
            state.handle_key_event(u32::from(VK_LWIN), WM_KEYUP, false),
            HookDecision {
                command: None,
                suppress: true,
                replay: Some(ReplayAction::WinTap {
                    win_vk: u32::from(VK_LWIN),
                }),
            }
        );
    }

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
