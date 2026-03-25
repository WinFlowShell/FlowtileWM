use flowtile_domain::RuntimeMode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DaemonCommand {
    Bootstrap {
        runtime_mode: RuntimeMode,
    },
    RunOnce {
        runtime_mode: RuntimeMode,
        dry_run: bool,
    },
    Watch {
        runtime_mode: RuntimeMode,
        dry_run: bool,
        interval_ms: u64,
        iterations: Option<u64>,
        poll_only: bool,
    },
}

pub(crate) fn parse_command(arguments: Vec<String>) -> Result<DaemonCommand, String> {
    if arguments.is_empty() {
        return default_watch_command(&[]);
    }

    let first = arguments[0].as_str();
    match first {
        "bootstrap" => Ok(DaemonCommand::Bootstrap {
            runtime_mode: parse_runtime_mode_flags(&arguments[1..])?,
        }),
        "run-once" => {
            let (runtime_mode, dry_run, _, _, _) = parse_runtime_flags(&arguments[1..])?;
            Ok(DaemonCommand::RunOnce {
                runtime_mode,
                dry_run,
            })
        }
        "watch" => {
            let (runtime_mode, dry_run, interval_ms, iterations, poll_only) =
                parse_runtime_flags(&arguments[1..])?;
            Ok(DaemonCommand::Watch {
                runtime_mode,
                dry_run,
                interval_ms,
                iterations,
                poll_only,
            })
        }
        value if value.starts_with('-') || RuntimeMode::parse(value).is_some() => {
            default_watch_command(&arguments)
        }
        _ => Err(format!("unsupported command '{}'", arguments[0])),
    }
}

pub(crate) fn print_usage() {
    println!("flowtile-core-daemon");
    println!("usage:");
    println!(
        "  flowtile-core-daemon [--dry-run] [--poll-only] [--interval-ms N] [--iterations N] [wm-only|extended-shell|safe-mode]"
    );
    println!("  flowtile-core-daemon bootstrap [wm-only|extended-shell|safe-mode]");
    println!("  flowtile-core-daemon run-once [--dry-run] [wm-only|extended-shell|safe-mode]");
    println!(
        "  flowtile-core-daemon watch [--dry-run] [--poll-only] [--interval-ms N] [--iterations N] [wm-only|extended-shell|safe-mode]"
    );
}

fn default_watch_command(arguments: &[String]) -> Result<DaemonCommand, String> {
    let (runtime_mode, dry_run, interval_ms, iterations, poll_only) =
        parse_runtime_flags(arguments)?;
    Ok(DaemonCommand::Watch {
        runtime_mode,
        dry_run,
        interval_ms,
        iterations,
        poll_only,
    })
}

fn parse_runtime_mode_flags(arguments: &[String]) -> Result<RuntimeMode, String> {
    let (runtime_mode, _, _, _, _) = parse_runtime_flags(arguments)?;
    Ok(runtime_mode)
}

fn parse_runtime_flags(
    arguments: &[String],
) -> Result<(RuntimeMode, bool, u64, Option<u64>, bool), String> {
    let mut runtime_mode = RuntimeMode::WmOnly;
    let mut dry_run = false;
    let mut interval_ms = 750_u64;
    let mut iterations = None;
    let mut poll_only = false;
    let mut index = 0_usize;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--poll-only" => {
                poll_only = true;
                index += 1;
            }
            "--interval-ms" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err("--interval-ms expects a value".to_string());
                };
                interval_ms = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --interval-ms value '{value}'"))?;
                index += 2;
            }
            "--iterations" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err("--iterations expects a value".to_string());
                };
                iterations = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --iterations value '{value}'"))?,
                );
                index += 2;
            }
            value => {
                runtime_mode = RuntimeMode::parse(value)
                    .ok_or_else(|| format!("unsupported runtime mode '{value}'"))?;
                index += 1;
            }
        }
    }

    Ok((runtime_mode, dry_run, interval_ms, iterations, poll_only))
}

#[cfg(test)]
mod tests {
    use flowtile_domain::RuntimeMode;

    use super::{DaemonCommand, parse_command};

    #[test]
    fn empty_arguments_start_watch_mode() {
        let command = parse_command(Vec::new()).expect("empty invocation should parse");

        assert_eq!(
            command,
            DaemonCommand::Watch {
                runtime_mode: RuntimeMode::WmOnly,
                dry_run: false,
                interval_ms: 750,
                iterations: None,
                poll_only: false,
            }
        );
    }

    #[test]
    fn bare_runtime_mode_uses_watch_command() {
        let command =
            parse_command(vec!["extended-shell".to_string()]).expect("runtime mode should parse");

        assert_eq!(
            command,
            DaemonCommand::Watch {
                runtime_mode: RuntimeMode::ExtendedShell,
                dry_run: false,
                interval_ms: 750,
                iterations: None,
                poll_only: false,
            }
        );
    }

    #[test]
    fn watch_flags_work_without_explicit_subcommand() {
        let command = parse_command(vec![
            "--poll-only".to_string(),
            "--interval-ms".to_string(),
            "25".to_string(),
            "--iterations".to_string(),
            "2".to_string(),
        ])
        .expect("watch flags should parse without explicit subcommand");

        assert_eq!(
            command,
            DaemonCommand::Watch {
                runtime_mode: RuntimeMode::WmOnly,
                dry_run: false,
                interval_ms: 25,
                iterations: Some(2),
                poll_only: true,
            }
        );
    }
}
