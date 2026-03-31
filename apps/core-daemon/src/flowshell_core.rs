use std::{
    env,
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    ptr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_PIPE_BUSY, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FlushFileBuffers, OPEN_EXISTING, ReadFile, WriteFile,
    },
    System::Pipes::{PIPE_READMODE_MESSAGE, SetNamedPipeHandleState, WaitNamedPipeW},
};

const BUFFER_SIZE: u32 = 64 * 1024;
const PROTOCOL_VERSION: u32 = 1;
const COMMAND_PIPE_ENV_VAR: &str = "FLOWSHELL_CORE_COMMAND_PIPE";
const DEFAULT_COMMAND_PIPE_NAME: &str = "flowshellcore-ipc-v1";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const WALLPAPER_SELECTOR_ENTRYPOINT_ID: &str = "wallpaper-selector";

pub(crate) fn open_wallpaper_selector() -> Result<(), String> {
    launch_entrypoint(WALLPAPER_SELECTOR_ENTRYPOINT_ID)
}

fn launch_entrypoint(entrypoint_id: &str) -> Result<(), String> {
    let request = IpcRequest::new(
        request_id("launch-entrypoint"),
        "launch_entrypoint",
        json!({ "entrypoint_id": entrypoint_id }),
    );
    let response = transact(&command_pipe_name(), &request).map_err(|error| error.to_string())?;
    ensure_success(&request, &response)
}

fn command_pipe_name() -> String {
    env::var(COMMAND_PIPE_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_COMMAND_PIPE_NAME.to_string())
}

fn request_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("flowtilewm-{prefix}-{millis}")
}

fn transact(pipe_name: &str, request: &IpcRequest) -> Result<IpcResponse, FlowShellCoreIpcError> {
    let connection = connect_pipe(pipe_name, DEFAULT_TIMEOUT)?;
    connection.write_message(&serde_json::to_string(request)?)?;
    Ok(serde_json::from_str(&connection.read_message()?)?)
}

fn ensure_success(request: &IpcRequest, response: &IpcResponse) -> Result<(), String> {
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "FlowShellCore protocol mismatch: expected {PROTOCOL_VERSION}, got {}",
            response.protocol_version
        ));
    }
    if response.request_id != request.request_id {
        return Err(format!(
            "FlowShellCore request mismatch: expected '{}', got '{}'",
            request.request_id, response.request_id
        ));
    }
    if !response.ok {
        let error = response
            .error
            .as_ref()
            .map(|error| format!("{}: {}", error.code, error.message))
            .unwrap_or_else(|| "request failed".to_string());
        return Err(format!("FlowShellCore launch_entrypoint failed: {error}"));
    }
    if response
        .result
        .as_ref()
        .and_then(|result| result.get("accepted"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Err("FlowShellCore launch_entrypoint was rejected".to_string());
    }

    Ok(())
}

fn connect_pipe(
    pipe_name: &str,
    timeout: Duration,
) -> Result<PipeConnection, FlowShellCoreIpcError> {
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

            return Ok(PipeConnection { handle });
        }

        let last_error = unsafe { GetLastError() };
        if last_error != ERROR_PIPE_BUSY {
            return Err(io::Error::from_raw_os_error(last_error as i32).into());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "FlowShellCore named pipe connection timed out",
            )
            .into());
        }

        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(u32::MAX as u128) as u32;
        let waited = unsafe { WaitNamedPipeW(full_pipe_name.as_ptr(), remaining_ms) };
        if waited == 0 && Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "FlowShellCore named pipe connection timed out",
            )
            .into());
        }
    }
}

fn full_pipe_name(pipe_name: &str) -> Vec<u16> {
    OsStr::new(&format!(r"\\.\pipe\{pipe_name}"))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

struct PipeConnection {
    handle: HANDLE,
}

impl PipeConnection {
    fn read_message(&self) -> Result<String, FlowShellCoreIpcError> {
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

    fn write_message(&self, message: &str) -> Result<(), FlowShellCoreIpcError> {
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
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial FlowShellCore named pipe write",
            )
            .into());
        }

        let flushed = unsafe { FlushFileBuffers(self.handle) };
        if flushed == 0 {
            return Err(io::Error::last_os_error().into());
        }

        Ok(())
    }
}

impl Drop for PipeConnection {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[derive(Debug)]
enum FlowShellCoreIpcError {
    Io(io::Error),
    InvalidUtf8(std::string::FromUtf8Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for FlowShellCoreIpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::InvalidUtf8(source) => source.fmt(formatter),
            Self::Json(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for FlowShellCoreIpcError {}

impl From<io::Error> for FlowShellCoreIpcError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<std::string::FromUtf8Error> for FlowShellCoreIpcError {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Self::InvalidUtf8(value)
    }
}

impl From<serde_json::Error> for FlowShellCoreIpcError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Serialize)]
struct IpcRequest {
    protocol_version: u32,
    request_id: String,
    message_type: String,
    payload: Value,
}

impl IpcRequest {
    fn new(request_id: impl Into<String>, message_type: impl Into<String>, payload: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            message_type: message_type.into(),
            payload,
        }
    }
}

#[derive(Debug, Deserialize)]
struct IpcResponse {
    protocol_version: u32,
    request_id: String,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<IpcError>,
}

#[derive(Debug, Deserialize)]
struct IpcError {
    code: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::{
        COMMAND_PIPE_ENV_VAR, DEFAULT_COMMAND_PIPE_NAME, IpcRequest, IpcResponse,
        WALLPAPER_SELECTOR_ENTRYPOINT_ID, command_pipe_name, ensure_success,
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn command_pipe_name_defaults_to_flowshellcore_pipe() {
        let _guard = ENV_LOCK.lock().expect("env lock should be acquired");
        unsafe {
            std::env::remove_var(COMMAND_PIPE_ENV_VAR);
        }

        assert_eq!(command_pipe_name(), DEFAULT_COMMAND_PIPE_NAME);
    }

    #[test]
    fn command_pipe_name_honors_environment_override() {
        let _guard = ENV_LOCK.lock().expect("env lock should be acquired");
        unsafe {
            std::env::set_var(COMMAND_PIPE_ENV_VAR, "custom-core-pipe");
        }

        assert_eq!(command_pipe_name(), "custom-core-pipe");

        unsafe {
            std::env::remove_var(COMMAND_PIPE_ENV_VAR);
        }
    }

    #[test]
    fn wallpaper_selector_request_targets_expected_entrypoint() {
        let request = IpcRequest::new(
            "req-1",
            "launch_entrypoint",
            json!({ "entrypoint_id": WALLPAPER_SELECTOR_ENTRYPOINT_ID }),
        );

        assert_eq!(request.message_type, "launch_entrypoint");
        assert_eq!(
            request
                .payload
                .get("entrypoint_id")
                .and_then(|value| value.as_str()),
            Some(WALLPAPER_SELECTOR_ENTRYPOINT_ID)
        );
    }

    #[test]
    fn failed_launch_response_surfaces_error() {
        let request = IpcRequest::new(
            "req-1",
            "launch_entrypoint",
            json!({ "entrypoint_id": WALLPAPER_SELECTOR_ENTRYPOINT_ID }),
        );
        let response = IpcResponse {
            protocol_version: 1,
            request_id: "req-1".to_string(),
            ok: false,
            result: None,
            error: Some(super::IpcError {
                code: "not_found".to_string(),
                message: "entrypoint is missing".to_string(),
            }),
        };

        let error = ensure_success(&request, &response).expect_err("response should fail");
        assert!(error.contains("not_found"));
        assert!(error.contains("entrypoint is missing"));
    }
}
