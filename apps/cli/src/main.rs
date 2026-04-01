use std::{env, process::ExitCode, time::Duration};

use flowtile_domain::RuntimeMode;
use flowtile_ipc::{
    CommandClient, DiagnosticsProjection, IpcRequest, bootstrap as ipc_bootstrap,
    connect_event_stream,
};
use serde_json::{Map, Value, json};

fn main() -> ExitCode {
    match parse_command(env::args().skip(1).collect()) {
        Ok(command) => run(command),
        Err(message) => {
            eprintln!("{message}");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn run(command: CliCommand) -> ExitCode {
    match command {
        CliCommand::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        CliCommand::Status => {
            let ipc = ipc_bootstrap();
            let client = CommandClient::new();
            let response = client.transact(&IpcRequest::new("status-1", "get_focus", json!({})));
            println!("flowtile-cli status");
            println!("transport: {}", ipc.transport);
            println!("protocol version: {}", ipc.protocol_version);
            println!("command pipe: {}", ipc.command_pipe_name);
            println!("event stream pipe: {}", ipc.event_stream_pipe_name);
            println!(
                "runtime modes planned: {}, {}, {}",
                RuntimeMode::WmOnly,
                RuntimeMode::ExtendedShell,
                RuntimeMode::SafeMode
            );
            match response {
                Ok(response) if response.ok => {
                    println!("daemon connection: ok");
                    if let Some(result) = response.result {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&result)
                                .expect("status result should serialize")
                        );
                    }
                    ExitCode::SUCCESS
                }
                Ok(response) => {
                    println!("daemon connection: error");
                    if let Some(error) = response.error {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&error)
                                .expect("status error should serialize")
                        );
                    }
                    ExitCode::from(1)
                }
                Err(error) => {
                    println!("daemon connection: unavailable");
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
        CliCommand::Snapshot => {
            let client = CommandClient::new();
            let requests = [
                ("outputs", "get_outputs"),
                ("workspaces", "get_workspaces"),
                ("windows", "get_windows"),
                ("focus", "get_focus"),
            ];
            let mut combined = Map::<String, Value>::new();

            for (key, command) in requests {
                match client.transact(&IpcRequest::new(
                    format!("snapshot-{command}"),
                    command,
                    json!({}),
                )) {
                    Ok(response) if response.ok => {
                        if let Some(result) = response.result {
                            combined.insert(key.to_string(), result);
                        }
                    }
                    Ok(response) => {
                        if let Some(error) = response.error {
                            eprintln!(
                                "{}",
                                serde_json::to_string_pretty(&error)
                                    .expect("snapshot error should serialize")
                            );
                        }
                        return ExitCode::from(1);
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        return ExitCode::from(1);
                    }
                }
            }

            println!(
                "{}",
                serde_json::to_string_pretty(&Value::Object(combined))
                    .expect("snapshot should serialize")
            );
            ExitCode::SUCCESS
        }
        CliCommand::Perf => {
            let client = CommandClient::new();
            match client.transact(&IpcRequest::new("perf-1", "dump_diagnostics", json!({}))) {
                Ok(response) if response.ok => {
                    let Some(result) = response.result else {
                        eprintln!("missing diagnostics payload");
                        return ExitCode::from(1);
                    };
                    let Some(diagnostics_value) = result.get("diagnostics").cloned() else {
                        eprintln!("missing diagnostics projection");
                        return ExitCode::from(1);
                    };
                    let diagnostics =
                        match serde_json::from_value::<DiagnosticsProjection>(diagnostics_value) {
                            Ok(diagnostics) => diagnostics,
                            Err(error) => {
                                eprintln!("invalid diagnostics projection: {error}");
                                return ExitCode::from(1);
                            }
                        };
                    print_perf_summary(&diagnostics);
                    ExitCode::SUCCESS
                }
                Ok(response) => {
                    if let Some(error) = response.error {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&error)
                                .expect("perf error should serialize")
                        );
                    }
                    ExitCode::from(1)
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
        CliCommand::Events => {
            let connection = match connect_event_stream(Duration::from_secs(3)) {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::from(1);
                }
            };

            loop {
                match connection.read_message() {
                    Ok(message) => {
                        let trimmed = message.trim();
                        if !trimmed.is_empty() {
                            println!("{trimmed}");
                        }
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        return ExitCode::from(1);
                    }
                }
            }
        }
        CliCommand::Command { command, payload } => {
            let client = CommandClient::new();
            match client.transact(&IpcRequest::new("cli-command", command, payload)) {
                Ok(response) => {
                    let value = if response.ok {
                        response.result.unwrap_or_else(|| json!({}))
                    } else {
                        Value::Object(
                            [(
                                "error".to_string(),
                                serde_json::to_value(response.error)
                                    .expect("ipc error should serialize"),
                            )]
                            .into_iter()
                            .collect(),
                        )
                    };
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&value)
                            .expect("command result should serialize")
                    );
                    if response.ok {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn parse_command(arguments: Vec<String>) -> Result<CliCommand, String> {
    if arguments.is_empty() {
        return Ok(CliCommand::Help);
    }

    match arguments[0].as_str() {
        "help" | "--help" => Ok(CliCommand::Help),
        "status" => Ok(CliCommand::Status),
        "snapshot" => Ok(CliCommand::Snapshot),
        "perf" => Ok(CliCommand::Perf),
        "events" => Ok(CliCommand::Events),
        "focus-next" => Ok(CliCommand::simple("focus_next")),
        "focus-prev" => Ok(CliCommand::simple("focus_prev")),
        "focus-workspace-up" => Ok(CliCommand::simple("focus_workspace_up")),
        "focus-workspace-down" => Ok(CliCommand::simple("focus_workspace_down")),
        "scroll-left" => Ok(CliCommand::simple("scroll_strip_left")),
        "scroll-right" => Ok(CliCommand::simple("scroll_strip_right")),
        "move-workspace-up" => Ok(CliCommand::simple("move_workspace_up")),
        "move-workspace-down" => Ok(CliCommand::simple("move_workspace_down")),
        "move-workspace-to-monitor-next" => {
            Ok(CliCommand::simple("move_workspace_to_monitor_next"))
        }
        "move-workspace-to-monitor-previous" => {
            Ok(CliCommand::simple("move_workspace_to_monitor_previous"))
        }
        "move-column-to-workspace-up" => Ok(CliCommand::simple("move_column_to_workspace_up")),
        "move-column-to-workspace-down" => Ok(CliCommand::simple("move_column_to_workspace_down")),
        "cycle-column-width" => Ok(CliCommand::simple("cycle_column_width")),
        "toggle-floating" => Ok(CliCommand::simple("toggle_floating")),
        "toggle-tabbed" => Ok(CliCommand::simple("toggle_tabbed")),
        "toggle-maximized" => Ok(CliCommand::simple("toggle_maximized")),
        "toggle-fullscreen" => Ok(CliCommand::simple("toggle_fullscreen")),
        "open-overview" => Ok(CliCommand::simple("open_overview")),
        "close-overview" => Ok(CliCommand::simple("close_overview")),
        "toggle-overview" => Ok(CliCommand::simple("toggle_overview")),
        "reload-config" => Ok(CliCommand::simple("reload_config")),
        "dump-diagnostics" => Ok(CliCommand::simple("dump_diagnostics")),
        other => Err(format!("unsupported command '{other}'")),
    }
}

fn print_help() {
    println!("flowtile-cli");
    println!("usage:");
    println!("  flowtile-cli help");
    println!("  flowtile-cli status");
    println!("  flowtile-cli snapshot");
    println!("  flowtile-cli perf");
    println!("  flowtile-cli events");
    println!("  flowtile-cli focus-next | focus-prev");
    println!("  flowtile-cli focus-workspace-up | focus-workspace-down");
    println!("  flowtile-cli scroll-left | scroll-right");
    println!("  flowtile-cli move-workspace-up | move-workspace-down");
    println!("  flowtile-cli move-workspace-to-monitor-next | move-workspace-to-monitor-previous");
    println!("  flowtile-cli move-column-to-workspace-up | move-column-to-workspace-down");
    println!("  flowtile-cli cycle-column-width");
    println!(
        "  flowtile-cli toggle-floating | toggle-tabbed | toggle-maximized | toggle-fullscreen"
    );
    println!("  flowtile-cli open-overview | close-overview | toggle-overview");
    println!("  flowtile-cli reload-config");
    println!("  flowtile-cli dump-diagnostics");
}

enum CliCommand {
    Help,
    Status,
    Snapshot,
    Perf,
    Events,
    Command { command: String, payload: Value },
}

impl CliCommand {
    fn simple(command: &str) -> Self {
        Self::Command {
            command: command.to_string(),
            payload: json!({}),
        }
    }
}

fn print_perf_summary(diagnostics: &DiagnosticsProjection) {
    println!("flowtile-cli perf");
    println!("management enabled: {}", diagnostics.management_enabled);
    println!("degraded flags: {}", diagnostics.degraded_flags.len());
    println!(
        "surrogate hosts: {}",
        diagnostics.surrogate_presentation.active_hosts
    );
    println!(
        "surrogate fallback/reject: {}/{}",
        diagnostics.surrogate_presentation.native_fallbacks,
        diagnostics.surrogate_presentation.classifier_rejections
    );
    println!(
        "surrogate replay ok/fail: {}/{}",
        diagnostics.surrogate_presentation.pointer_replay_successes,
        diagnostics.surrogate_presentation.pointer_replay_failures
    );
    if let Some(last_event) = &diagnostics.surrogate_presentation.last_event {
        println!("surrogate last event: {last_event}");
    }
    if diagnostics.perf.metrics.is_empty() {
        println!("perf metrics: none");
        return;
    }

    println!(
        "{:<36} {:>8} {:>12} {:>12} {:>12} {:>12} {:>8} {:>8}",
        "metric", "samples", "total-ms", "avg-us", "max-us", "last-us", "errors", "skips"
    );
    for metric in &diagnostics.perf.metrics {
        println!(
            "{:<36} {:>8} {:>12} {:>12} {:>12} {:>12} {:>8} {:>8}",
            metric.metric,
            metric.samples,
            format_millis(metric.total_duration_us),
            metric.average_duration_us,
            metric.max_duration_us,
            metric.last_duration_us,
            metric.error_count,
            metric.skip_count
        );
    }
}

fn format_millis(duration_us: u64) -> String {
    format!("{:.3}", duration_us as f64 / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, parse_command};

    #[test]
    fn parses_perf_command() {
        let command = parse_command(vec!["perf".to_string()]).expect("perf should parse");
        assert!(matches!(command, CliCommand::Perf));
    }
}
