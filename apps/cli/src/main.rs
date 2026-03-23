use std::{env, process::ExitCode, time::Duration};

use flowtile_domain::RuntimeMode;
use flowtile_ipc::{CommandClient, IpcRequest, bootstrap as ipc_bootstrap, connect_event_stream};
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
        "events" => Ok(CliCommand::Events),
        "focus-next" => Ok(CliCommand::simple("focus_next")),
        "focus-prev" => Ok(CliCommand::simple("focus_prev")),
        "scroll-left" => Ok(CliCommand::simple("scroll_strip_left")),
        "scroll-right" => Ok(CliCommand::simple("scroll_strip_right")),
        "toggle-floating" => Ok(CliCommand::simple("toggle_floating")),
        "toggle-tabbed" => Ok(CliCommand::simple("toggle_tabbed")),
        "toggle-maximized" => Ok(CliCommand::simple("toggle_maximized")),
        "toggle-fullscreen" => Ok(CliCommand::simple("toggle_fullscreen")),
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
    println!("  flowtile-cli events");
    println!("  flowtile-cli focus-next | focus-prev");
    println!("  flowtile-cli scroll-left | scroll-right");
    println!(
        "  flowtile-cli toggle-floating | toggle-tabbed | toggle-maximized | toggle-fullscreen"
    );
    println!("  flowtile-cli toggle-overview");
    println!("  flowtile-cli reload-config");
    println!("  flowtile-cli dump-diagnostics");
}

enum CliCommand {
    Help,
    Status,
    Snapshot,
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
