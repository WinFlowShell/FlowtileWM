use super::*;

impl CoreDaemonRuntime {
    pub fn reload_config(&mut self, dry_run: bool) -> Result<RuntimeCycleReport, RuntimeError> {
        let started_at = Instant::now();
        let result = (|| {
            let config_path = self.store.state().config_projection.source_path.clone();
            let correlation_id = self.next_correlation_id();
            let _ = self.store.dispatch(DomainEvent::config_reload_requested(
                correlation_id,
                flowtile_domain::EventSource::InputCommand,
                Some(config_path.clone()),
            ))?;

            match load_from_path(&config_path, self.next_config_generation) {
                Ok(loaded_config) => {
                    ensure_supported_bind_control_mode(loaded_config.projection.bind_control_mode)?;
                    let changed_sections =
                        diff_config_sections(&self.active_config, &loaded_config);
                    let rule_ids = loaded_config
                        .rules
                        .iter()
                        .map(|rule| rule.id.clone())
                        .collect::<Vec<_>>();
                    self.active_config = loaded_config.clone();
                    self.last_valid_config = loaded_config.clone();
                    self.next_config_generation += 1;

                    let reload_succeeded_correlation = self.next_correlation_id();
                    self.store.dispatch(DomainEvent::config_reload_succeeded(
                        reload_succeeded_correlation,
                        loaded_config.projection.config_version,
                        changed_sections,
                        loaded_config.projection.clone(),
                    ))?;
                    let rules_updated_correlation = self.next_correlation_id();
                    self.store.dispatch(DomainEvent::rules_updated(
                        rules_updated_correlation,
                        loaded_config.projection.config_version,
                        rule_ids,
                        loaded_config.projection.active_rule_count,
                    ))?;

                    let report_correlation = self.next_correlation_id();
                    self.dispatch_command(
                        DomainEvent::config_reload_requested(
                            report_correlation,
                            flowtile_domain::EventSource::ConfigRules,
                            Some(config_path),
                        ),
                        dry_run,
                        "config-reload",
                    )
                }
                Err(error) => {
                    let failure_correlation = self.next_correlation_id();
                    let _ = self.store.dispatch(DomainEvent::config_reload_failed(
                        failure_correlation,
                        "config-reload-failed",
                        error.to_string(),
                    ));
                    self.active_config = self.last_valid_config.clone();
                    self.push_degraded_reason("config-reload-failed".to_string());
                    Err(RuntimeError::Config(error.to_string()))
                }
            }
        })();
        record_perf_metric(&self.perf.config_reload, started_at, &result);
        result
    }
}

fn diff_config_sections(previous: &LoadedConfig, current: &LoadedConfig) -> Vec<String> {
    let mut changed_sections = Vec::new();

    if previous.projection.strip_scroll_step != current.projection.strip_scroll_step
        || previous.projection.default_column_mode != current.projection.default_column_mode
        || previous.projection.default_column_width != current.projection.default_column_width
        || previous.projection.layout_spacing != current.projection.layout_spacing
    {
        changed_sections.push("layout".to_string());
    }
    if previous.projection.bind_control_mode != current.projection.bind_control_mode
        || previous.hotkeys != current.hotkeys
        || previous.touchpad != current.touchpad
    {
        changed_sections.push("input".to_string());
    }
    if previous.rules != current.rules {
        changed_sections.push("rules".to_string());
    }
    if changed_sections.is_empty() {
        changed_sections.push("none".to_string());
    }

    changed_sections
}

fn ensure_supported_bind_control_mode(
    bind_control_mode: BindControlMode,
) -> Result<(), RuntimeError> {
    match bind_control_mode {
        BindControlMode::Coexistence => Ok(()),
        _ => Err(RuntimeError::Config(format!(
            "bind control mode '{}' is not supported by the current runtime slice; only 'coexistence' is available",
            bind_control_mode.as_str()
        ))),
    }
}

pub(super) fn workspace_path(relative_path: &str) -> PathBuf {
    workspace_root().join(relative_path)
}

fn workspace_root() -> PathBuf {
    if let Ok(root) = std::env::var("FLOWTILE_WORKSPACE_ROOT") {
        return PathBuf::from(root);
    }

    if let Ok(current_dir) = std::env::current_dir()
        && let Some(root) = find_workspace_root(&current_dir)
    {
        return root;
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(exe_dir) = current_exe.parent()
        && let Some(root) = find_workspace_root(exe_dir)
    {
        return root;
    }

    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .to_path_buf()
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("config").is_dir() {
            return Some(candidate.to_path_buf());
        }
    }

    None
}
