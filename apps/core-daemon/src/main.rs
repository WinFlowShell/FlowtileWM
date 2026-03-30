mod cli;
mod control;
mod diag;
mod hotkeys;
mod ipc;
mod manual_resize;
mod overview_controller;
mod overview_engine;
mod overview_surface;
mod projection;
mod tab_indicator;
mod terminal;
mod touchpad;
mod watch;
mod window_actions;

use std::{env, process::ExitCode};

use cli::{DaemonCommand, parse_command, print_usage};
use diag::write_runtime_log;
use flowtile_wm_core::{CoreDaemonBootstrap, CoreDaemonRuntime};
use hotkeys::ensure_bind_control_mode_supported;
use touchpad::ensure_touchpad_override_supported;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, GetLastError},
    System::Threading::GetCurrentProcess,
    UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE,
        DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED, GetDpiAwarenessContextForProcess,
        GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    },
};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    write_runtime_log(format!("main: args={arguments:?}"));

    if let Err(message) = ensure_process_dpi_awareness() {
        write_runtime_log(format!("main: dpi-awareness-error={message}"));
        eprintln!("{message}");
        return ExitCode::from(1);
    }
    write_runtime_log("main: dpi-awareness-ok");

    match parse_command(arguments) {
        Ok(command) => {
            write_runtime_log(format!("main: parsed-command={command:?}"));
            run(command)
        }
        Err(message) => {
            write_runtime_log(format!("main: parse-error={message}"));
            eprintln!("{message}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run(command: DaemonCommand) -> ExitCode {
    match command {
        DaemonCommand::Bootstrap { runtime_mode } => {
            write_runtime_log(format!("run: bootstrap runtime_mode={runtime_mode:?}"));
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
            write_runtime_log(format!(
                "run: run-once runtime_mode={runtime_mode:?} dry_run={dry_run}"
            ));
            let mut runtime = CoreDaemonRuntime::new(runtime_mode);
            if let Err(error) = ensure_bind_control_mode_supported(runtime.bind_control_mode()) {
                write_runtime_log(format!("run: bind-control-startup-error={error}"));
                eprintln!("bind control mode startup failed: {error}");
                return ExitCode::from(1);
            }
            if let Err(error) = ensure_touchpad_override_supported(runtime.touchpad_config()) {
                write_runtime_log(format!("run: touchpad-startup-error={error}"));
                eprintln!("touchpad override startup failed: {error}");
                return ExitCode::from(1);
            }
            match runtime.scan_and_sync(dry_run) {
                Ok(report) => {
                    write_runtime_log("run: run-once scan-and-sync-ok");
                    println!("flowtile-core-daemon run-once");
                    for line in report.summary_lines() {
                        println!("{line}");
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    write_runtime_log(format!("run: run-once scan-and-sync-error={error:?}"));
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
        } => {
            write_runtime_log(format!(
                "run: watch runtime_mode={runtime_mode:?} dry_run={dry_run} interval_ms={interval_ms} iterations={iterations:?} poll_only={poll_only}"
            ));
            watch::run_watch(runtime_mode, dry_run, interval_ms, iterations, poll_only)
        }
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
        return ensure_current_dpi_awareness();
    }

    let error = {
        // SAFETY: `GetLastError` is read immediately after the failed Win32 call above.
        unsafe { GetLastError() }
    };
    if error == ERROR_ACCESS_DENIED {
        return ensure_current_dpi_awareness();
    }

    Err(format!(
        "SetProcessDpiAwarenessContext failed with Win32 error {error}"
    ))
}

#[cfg(windows)]
fn ensure_current_dpi_awareness() -> Result<(), String> {
    let process_context = {
        // SAFETY: This is a read-only query for the current process DPI awareness context.
        unsafe { GetDpiAwarenessContextForProcess(GetCurrentProcess()) }
    };
    if awareness_context_equals(process_context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        return Ok(());
    }

    let _ = {
        // SAFETY: This sets the current startup thread to PMv2 so early adapter calls use the
        // correct coordinate space even before any worker threads are spawned.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    };
    let thread_context = {
        // SAFETY: This is a read-only query for the current thread DPI awareness context.
        unsafe { GetThreadDpiAwarenessContext() }
    };

    if awareness_context_equals(process_context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        && awareness_context_equals(thread_context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    {
        Ok(())
    } else {
        Err(format!(
            "flowtile-core-daemon must run in Per Monitor DPI Aware v2; process awareness is {}, thread awareness is {}",
            awareness_context_label(process_context),
            awareness_context_label(thread_context),
        ))
    }
}

#[cfg(windows)]
fn awareness_context_label(context: DPI_AWARENESS_CONTEXT) -> &'static str {
    if awareness_context_equals(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        "per-monitor-v2"
    } else if awareness_context_equals(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
        "per-monitor-v1"
    } else if awareness_context_equals(context, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
        "system-aware"
    } else if awareness_context_equals(context, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED) {
        "unaware-gdi-scaled"
    } else if awareness_context_equals(context, DPI_AWARENESS_CONTEXT_UNAWARE) {
        "unaware"
    } else {
        "unknown"
    }
}

#[cfg(windows)]
fn awareness_context_equals(left: DPI_AWARENESS_CONTEXT, right: DPI_AWARENESS_CONTEXT) -> bool {
    let equal = {
        // SAFETY: Both values are DPI awareness context handles returned by or defined for Win32.
        unsafe { AreDpiAwarenessContextsEqual(left, right) }
    };
    equal != 0
}

#[cfg(not(windows))]
fn ensure_process_dpi_awareness() -> Result<(), String> {
    Ok(())
}
