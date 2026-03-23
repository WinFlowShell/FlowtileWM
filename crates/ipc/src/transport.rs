use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    ptr,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, SetNamedPipeHandleState,
        WaitNamedPipeW,
    },
};

use crate::{COMMAND_PIPE_NAME, EVENT_STREAM_PIPE_NAME, IpcRequest, IpcResponse};

const BUFFER_SIZE: u32 = 64 * 1024;

#[derive(Debug)]
pub struct NamedPipeConnection {
    handle: HANDLE,
    disconnect_on_drop: bool,
}

unsafe impl Send for NamedPipeConnection {}

pub struct NamedPipeListener {
    pipe_name: &'static str,
}

pub struct CommandClient {
    pipe_name: &'static str,
    timeout: Duration,
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    InvalidUtf8(std::string::FromUtf8Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::InvalidUtf8(source) => source.fmt(formatter),
            Self::Json(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<io::Error> for TransportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<std::string::FromUtf8Error> for TransportError {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Self::InvalidUtf8(value)
    }
}

impl From<serde_json::Error> for TransportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl NamedPipeListener {
    pub const fn command() -> Self {
        Self {
            pipe_name: COMMAND_PIPE_NAME,
        }
    }

    pub const fn event_stream() -> Self {
        Self {
            pipe_name: EVENT_STREAM_PIPE_NAME,
        }
    }

    pub fn accept(&self) -> Result<NamedPipeConnection, TransportError> {
        let full_pipe_name = full_pipe_name(self.pipe_name);
        let handle = unsafe {
            CreateNamedPipeW(
                full_pipe_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error().into());
        }

        let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
        if connected == 0 {
            let last_error = unsafe { GetLastError() };
            if last_error != ERROR_PIPE_CONNECTED {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(io::Error::from_raw_os_error(last_error as i32).into());
            }
        }

        Ok(NamedPipeConnection {
            handle,
            disconnect_on_drop: true,
        })
    }
}

impl NamedPipeConnection {
    pub fn read_message(&self) -> Result<String, TransportError> {
        let mut chunk = vec![0_u8; BUFFER_SIZE as usize];
        let mut bytes = Vec::new();

        loop {
            let mut read = 0_u32;
            let success = unsafe {
                ReadFile(
                    self.handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            };

            if success != 0 {
                bytes.extend_from_slice(&chunk[..read as usize]);
                break;
            }

            let last_error = unsafe { GetLastError() };
            if last_error == ERROR_MORE_DATA {
                bytes.extend_from_slice(&chunk[..read as usize]);
                continue;
            }
            if last_error == ERROR_BROKEN_PIPE {
                break;
            }

            return Err(io::Error::from_raw_os_error(last_error as i32).into());
        }

        Ok(String::from_utf8(bytes)?)
    }

    pub fn write_message(&self, message: &str) -> Result<(), TransportError> {
        let payload = message.as_bytes();
        let mut written = 0_u32;
        let success = unsafe {
            WriteFile(
                self.handle,
                payload.as_ptr(),
                payload.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };

        if success == 0 {
            return Err(io::Error::last_os_error().into());
        }
        if written as usize != payload.len() {
            return Err(
                io::Error::new(io::ErrorKind::WriteZero, "partial named pipe write").into(),
            );
        }

        let flushed = unsafe { FlushFileBuffers(self.handle) };
        if flushed == 0 {
            return Err(io::Error::last_os_error().into());
        }

        Ok(())
    }
}

impl Drop for NamedPipeConnection {
    fn drop(&mut self) {
        unsafe {
            if self.disconnect_on_drop {
                DisconnectNamedPipe(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

impl CommandClient {
    pub fn new() -> Self {
        Self {
            pipe_name: COMMAND_PIPE_NAME,
            timeout: Duration::from_secs(3),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn transact(&self, request: &IpcRequest) -> Result<IpcResponse, TransportError> {
        let connection = connect_pipe(self.pipe_name, self.timeout)?;
        let payload = serde_json::to_string(request)?;
        connection.write_message(&payload)?;
        let response = connection.read_message()?;
        Ok(serde_json::from_str::<IpcResponse>(&response)?)
    }
}

impl Default for CommandClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn connect_event_stream(timeout: Duration) -> Result<NamedPipeConnection, TransportError> {
    connect_pipe(EVENT_STREAM_PIPE_NAME, timeout)
}

fn connect_pipe(
    pipe_name: &'static str,
    timeout: Duration,
) -> Result<NamedPipeConnection, TransportError> {
    let deadline = Instant::now() + timeout;
    let full_pipe_name = full_pipe_name(pipe_name);

    loop {
        let handle = unsafe {
            CreateFileW(
                full_pipe_name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };

        if handle != INVALID_HANDLE_VALUE {
            let read_mode = PIPE_READMODE_MESSAGE;
            let success = unsafe {
                SetNamedPipeHandleState(handle, &read_mode, ptr::null_mut(), ptr::null_mut())
            };
            if success == 0 {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(io::Error::last_os_error().into());
            }

            return Ok(NamedPipeConnection {
                handle,
                disconnect_on_drop: false,
            });
        }

        let last_error = unsafe { GetLastError() };
        if last_error != ERROR_PIPE_BUSY {
            return Err(io::Error::from_raw_os_error(last_error as i32).into());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "named pipe connection timed out").into(),
            );
        }

        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(u32::MAX as u128) as u32;
        let waited = unsafe { WaitNamedPipeW(full_pipe_name.as_ptr(), remaining_ms) };
        if waited == 0 && Instant::now() >= deadline {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "named pipe connection timed out").into(),
            );
        }
    }
}

fn full_pipe_name(pipe_name: &str) -> Vec<u16> {
    OsStr::new(&format!(r"\\.\pipe\{pipe_name}"))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
