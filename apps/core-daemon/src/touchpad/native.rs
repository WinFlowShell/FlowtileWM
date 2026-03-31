use std::{
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
};

use crate::diag::write_touchpad_dump;

use super::{
    TouchpadListenerError,
    recognizer::{RawTouchpadFrameAssembler, parse_sample_touchpad_report},
    runtime::TouchpadRuntimeEvent,
};

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

#[cfg(windows)]
#[derive(Debug)]
pub(super) struct NativeTouchpadRuntime {
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

#[cfg(windows)]
impl NativeTouchpadRuntime {
    pub(super) fn spawn(
        gesture_sender: Sender<TouchpadRuntimeEvent>,
    ) -> Result<Self, TouchpadListenerError> {
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

    pub(super) fn shutdown(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
