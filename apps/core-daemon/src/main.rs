mod cli;
mod control;
mod hotkeys;
mod ipc;
mod projection;
mod watch;

use std::{env, process::ExitCode};

use cli::{DaemonCommand, parse_command, print_usage};
use flowtile_wm_core::{CoreDaemonBootstrap, CoreDaemonRuntime};
use hotkeys::ensure_bind_control_mode_supported;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, GetLastError},
    UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
};

fn main() -> ExitCode {
    if let Err(message) = ensure_process_dpi_awareness() {
        eprintln!("{message}");
        return ExitCode::from(1);
    }

    match parse_command(env::args().skip(1).collect()) {
        Ok(command) => run(command),
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run(command: DaemonCommand) -> ExitCode {
    match command {
        DaemonCommand::Bootstrap { runtime_mode } => {
            let bootstrap = CoreDaemonBootstrap::new(runtime_mode);
            println!("flowtile-core-daemon bootstrap");
            for line in bootstrap.summary_lines() {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        DaemonCommand::RunOnce {
            runtime_mode,
            dry_run,
        } => {
            let mut runtime = CoreDaemonRuntime::new(runtime_mode);
            if let Err(error) = ensure_bind_control_mode_supported(runtime.bind_control_mode()) {
                eprintln!("bind control mode startup failed: {error}");
                return ExitCode::from(1);
            }
            match runtime.scan_and_sync(dry_run) {
                Ok(report) => {
                    println!("flowtile-core-daemon run-once");
                    for line in report.summary_lines() {
                        println!("{line}");
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error:?}");
                    ExitCode::from(1)
                }
            }
        }
        DaemonCommand::Watch {
            runtime_mode,
            dry_run,
            interval_ms,
            iterations,
            poll_only,
        } => watch::run_watch(runtime_mode, dry_run, interval_ms, iterations, poll_only),
    }
}

#[cfg(windows)]
fn ensure_process_dpi_awareness() -> Result<(), String> {
    let applied = {
        // SAFETY: This sets the process DPI awareness once at startup before the daemon creates
        // long-lived Win32 integrations. The requested context is the documented PMv2 baseline.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    };
    if applied != 0 {
        return Ok(());
    }

    let error = {
        // SAFETY: `GetLastError` is read immediately after the failed Win32 call above.
        unsafe { GetLastError() }
    };
    if error == ERROR_ACCESS_DENIED {
        return Ok(());
    }

    Err(format!(
        "SetProcessDpiAwarenessContext failed with Win32 error {error}"
    ))
}

#[cfg(not(windows))]
fn ensure_process_dpi_awareness() -> Result<(), String> {
    Ok(())
}
