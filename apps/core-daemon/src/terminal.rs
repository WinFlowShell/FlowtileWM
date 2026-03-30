const DEFAULT_TERMINAL_SHORTCUT_PATH: &str =
    r"C:\Users\mayo\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\PowerShell.lnk";

pub(crate) fn open_default_terminal() -> Result<(), String> {
    #[cfg(windows)]
    {
        spawn_terminal_shortcut(DEFAULT_TERMINAL_SHORTCUT_PATH).map_err(|error| {
            format!("failed to open terminal shortcut {DEFAULT_TERMINAL_SHORTCUT_PATH}: {error}")
        })
    }

    #[cfg(not(windows))]
    {
        Err("open-terminal is only supported on Windows".to_string())
    }
}

#[cfg(windows)]
fn spawn_terminal_shortcut(shortcut_path: &str) -> Result<(), String> {
    use std::{
        ffi::OsStr,
        os::windows::ffi::OsStrExt,
        path::Path,
        ptr::{null, null_mut},
    };

    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    if !Path::new(shortcut_path).exists() {
        return Err("shortcut path does not exist".to_string());
    }

    let operation = wide("open");
    let file = wide(shortcut_path);
    let result = {
        // SAFETY: The strings are valid null-terminated UTF-16 buffers, and the call only asks
        // the Windows shell to open an existing shortcut with the normal show mode.
        unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        }
    } as isize;
    if result <= 32 {
        return Err(format!("ShellExecuteW failed with shell code {result}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_TERMINAL_SHORTCUT_PATH;

    #[test]
    fn uses_explicit_powershell_shortcut_target() {
        assert_eq!(
            DEFAULT_TERMINAL_SHORTCUT_PATH,
            r"C:\Users\mayo\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\PowerShell.lnk"
        );
    }
}
