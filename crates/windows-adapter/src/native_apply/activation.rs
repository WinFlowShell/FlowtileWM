use windows_sys::Win32::{
    Foundation::HWND,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
            SetActiveWindow, SetFocus, VK_MENU,
        },
        WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
            SW_SHOW, SetForegroundWindow, ShowWindow,
        },
    },
};

use super::hwnd_from_raw;

pub(super) fn activate_window(raw_hwnd: u64) -> Result<(), String> {
    let hwnd = hwnd_from_raw(raw_hwnd)?;
    let current_thread_id = { unsafe { GetCurrentThreadId() } };
    let foreground_hwnd = { unsafe { GetForegroundWindow() } };
    let target_thread_id = window_thread_id(hwnd);
    let foreground_thread_id = if foreground_hwnd.is_null() {
        0
    } else {
        window_thread_id(foreground_hwnd)
    };
    let mut attached_pairs = Vec::new();

    attach_thread_pair(current_thread_id, target_thread_id, &mut attached_pairs);
    attach_thread_pair(current_thread_id, foreground_thread_id, &mut attached_pairs);
    attach_thread_pair(target_thread_id, foreground_thread_id, &mut attached_pairs);

    if is_iconic(hwnd) {
        let _ = { unsafe { ShowWindow(hwnd, SW_RESTORE) } };
    } else {
        let _ = { unsafe { ShowWindow(hwnd, SW_SHOW) } };
    }

    let _ = { unsafe { BringWindowToTop(hwnd) } };
    let _ = { unsafe { SetActiveWindow(hwnd) } };
    let _ = { unsafe { SetFocus(hwnd) } };
    let _ = { unsafe { SetForegroundWindow(hwnd) } };
    let _ = { unsafe { BringWindowToTop(hwnd) } };
    let _ = { unsafe { SetForegroundWindow(hwnd) } };

    let mut activation_succeeded = unsafe { GetForegroundWindow() == hwnd };
    if !activation_succeeded {
        unlock_foreground_with_alt();
        let _ = { unsafe { BringWindowToTop(hwnd) } };
        let _ = { unsafe { SetForegroundWindow(hwnd) } };
        activation_succeeded = unsafe { GetForegroundWindow() == hwnd };
    }

    for (left, right) in attached_pairs.into_iter().rev() {
        let _ = { unsafe { AttachThreadInput(left, right, 0) } };
    }

    if activation_succeeded {
        Ok(())
    } else {
        Err(format!(
            "platform activation path failed to foreground hwnd {raw_hwnd}"
        ))
    }
}

fn attach_thread_pair(left: u32, right: u32, attached_pairs: &mut Vec<(u32, u32)>) {
    if left == 0 || right == 0 || left == right {
        return;
    }

    let attached = { unsafe { AttachThreadInput(left, right, 1) } };
    if attached != 0 {
        attached_pairs.push((left, right));
    }
}

fn window_thread_id(hwnd: HWND) -> u32 {
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) }
}

fn is_iconic(hwnd: HWND) -> bool {
    let iconic = { unsafe { IsIconic(hwnd) } };
    iconic != 0
}

fn unlock_foreground_with_alt() {
    let mut inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: 0,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];

    let _ = {
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_mut_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        }
    };
}
