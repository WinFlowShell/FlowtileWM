#![forbid(unsafe_code)]

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use flowtile_domain::{BindControlMode, ColumnMode, ConfigProjection, WidthSemantics, WindowLayer};
use kdl::{KdlDocument, KdlNode, KdlValue};

pub const PREFERRED_CONFIG_FORMAT: &str = "KDL";
pub const FALLBACK_CONFIG_FORMAT: &str = "TOML";
pub const DEFAULT_CONFIG_PATH: &str = "config/flowtile.kdl";
const DEFAULT_HOTKEY_BINDINGS: [(&str, &str); 5] = [
    ("Win+H", "focus-prev"),
    ("Win+J", "focus-next"),
    ("Win+Ctrl+Shift+F", "toggle-floating"),
    ("Win+Ctrl+Shift+Space", "toggle-fullscreen"),
    ("Win+Ctrl+Shift+Backspace", "disable-management-and-unwind"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigBootstrap {
    pub preferred_format: &'static str,
    pub fallback_format: &'static str,
    pub default_path: &'static str,
    pub supports_live_reload: bool,
    pub supports_rollback: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HotkeyBinding {
    pub trigger: String,
    pub command: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowRuleMatch {
    pub process_name: Option<String>,
    pub class_substring: Option<String>,
    pub title_substring: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowRuleActions {
    pub layer: Option<WindowLayer>,
    pub column_mode: Option<ColumnMode>,
    pub width_semantics: Option<WidthSemantics>,
    pub managed: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowRule {
    pub id: String,
    pub priority: i32,
    pub enabled: bool,
    pub matchers: WindowRuleMatch,
    pub actions: WindowRuleActions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedConfig {
    pub projection: ConfigProjection,
    pub hotkeys: Vec<HotkeyBinding>,
    pub rules: Vec<WindowRule>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowRuleInput {
    pub process_name: Option<String>,
    pub class_name: String,
    pub title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowRuleDecision {
    pub layer: WindowLayer,
    pub managed: bool,
    pub column_mode: ColumnMode,
    pub width_semantics: WidthSemantics,
    pub width_semantics_explicit: bool,
    pub matched_rule_ids: Vec<String>,
}

impl WindowRuleDecision {
    pub fn from_projection(projection: &ConfigProjection) -> Self {
        Self {
            layer: WindowLayer::Tiled,
            managed: true,
            column_mode: projection.default_column_mode,
            width_semantics: projection.default_column_width,
            width_semantics_explicit: false,
            matched_rule_ids: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::Parse(message) => formatter.write_str(message),
            Self::Validation(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub const fn bootstrap() -> ConfigBootstrap {
    ConfigBootstrap {
        preferred_format: PREFERRED_CONFIG_FORMAT,
        fallback_format: FALLBACK_CONFIG_FORMAT,
        default_path: DEFAULT_CONFIG_PATH,
        supports_live_reload: true,
        supports_rollback: true,
    }
}

pub fn default_loaded_config(
    config_generation: u64,
    source_path: impl Into<String>,
) -> LoadedConfig {
    LoadedConfig {
        projection: ConfigProjection {
            config_version: config_generation,
            source_path: source_path.into(),
            ..ConfigProjection::default()
        },
        hotkeys: default_hotkeys(),
        rules: Vec::new(),
    }
}

fn default_hotkeys() -> Vec<HotkeyBinding> {
    DEFAULT_HOTKEY_BINDINGS
        .iter()
        .map(|(trigger, command)| HotkeyBinding {
            trigger: (*trigger).to_string(),
            command: (*command).to_string(),
        })
        .collect()
}

pub fn load_or_default(
    path: impl AsRef<Path>,
    config_generation: u64,
) -> Result<LoadedConfig, ConfigError> {
    let path = path.as_ref();
    match load_from_path(path, config_generation) {
        Ok(config) => Ok(config),
        Err(ConfigError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(
            default_loaded_config(config_generation, path.display().to_string()),
        ),
        Err(error) => Err(error),
    }
}

pub fn load_from_path(
    path: impl AsRef<Path>,
    config_generation: u64,
) -> Result<LoadedConfig, ConfigError> {
    let path = path.as_ref();
    let source = fs::read_to_string(path)?;
    match source.parse::<KdlDocument>() {
        Ok(document) => parse_kdl_document(document, path, config_generation),
        Err(_) => parse_kdl_like_source(&source, path, config_generation),
    }
}

fn parse_kdl_document(
    document: KdlDocument,
    path: &Path,
    config_generation: u64,
) -> Result<LoadedConfig, ConfigError> {
    let mut projection = ConfigProjection {
        config_version: config_generation,
        source_path: path.display().to_string(),
        ..ConfigProjection::default()
    };
    let mut hotkeys = Vec::new();
    let mut rules = Vec::new();

    for node in document.nodes() {
        match node.name().value() {
            "general" => {
                for child in child_nodes(node) {
                    if child.name().value() == "mode"
                        && let Some(mode) = first_string(child)?
                    {
                        projection.source_path = path.display().to_string();
                        if !matches!(mode.as_str(), "wm-only" | "extended-shell" | "safe-mode") {
                            return Err(ConfigError::Validation(format!(
                                "unsupported general mode '{mode}'"
                            )));
                        }
                    }
                }
            }
            "layout" => parse_layout(node, &mut projection)?,
            "input" => parse_input(node, &mut projection, &mut hotkeys)?,
            "rules" => parse_rules(node, &mut rules)?,
            _ => {}
        }
    }

    rules.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    projection.active_rule_count = rules.iter().filter(|rule| rule.enabled).count();

    Ok(LoadedConfig {
        projection,
        hotkeys,
        rules,
    })
}

fn parse_kdl_like_source(
    source: &str,
    path: &Path,
    config_generation: u64,
) -> Result<LoadedConfig, ConfigError> {
    let mut projection = ConfigProjection {
        config_version: config_generation,
        source_path: path.display().to_string(),
        ..ConfigProjection::default()
    };
    let mut hotkeys = Vec::new();
    let mut rules = Vec::new();
    let mut current_section: Option<String> = None;
    let mut current_rule: Option<WindowRule> = None;
    let mut in_actions = false;

    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if let Some(prefix) = line.strip_suffix('{') {
            let tokens = tokenize_kdl_like(prefix.trim())?;
            if current_section.is_none() {
                current_section = tokens.first().cloned();
                continue;
            }

            if current_section.as_deref() == Some("rules")
                && current_rule.is_none()
                && tokens.first().is_some_and(|token| token == "rule")
            {
                let Some(rule_id) = tokens.get(1).cloned() else {
                    return Err(ConfigError::Validation("rule id is missing".to_string()));
                };
                current_rule = Some(WindowRule {
                    id: rule_id,
                    priority: 0,
                    enabled: true,
                    matchers: WindowRuleMatch {
                        process_name: None,
                        class_substring: None,
                        title_substring: None,
                    },
                    actions: WindowRuleActions {
                        layer: None,
                        column_mode: None,
                        width_semantics: None,
                        managed: None,
                    },
                });
                continue;
            }

            if current_section.as_deref() == Some("rules")
                && current_rule.is_some()
                && tokens.first().is_some_and(|token| token == "actions")
            {
                in_actions = true;
            }

            continue;
        }

        if line == "}" {
            if in_actions {
                in_actions = false;
                continue;
            }
            if let Some(rule) = current_rule.take() {
                rules.push(rule);
                continue;
            }
            current_section = None;
            continue;
        }

        let tokens = tokenize_kdl_like(line)?;
        if tokens.is_empty() {
            continue;
        }

        match current_section.as_deref() {
            Some("general") => {
                if tokens[0] == "mode"
                    && let Some(mode) = tokens.get(1)
                    && !matches!(mode.as_str(), "wm-only" | "extended-shell" | "safe-mode")
                {
                    return Err(ConfigError::Validation(format!(
                        "unsupported general mode '{mode}'"
                    )));
                }
            }
            Some("layout") => match tokens[0].as_str() {
                "strip-scroll-step" => {
                    let Some(step) = tokens.get(1) else {
                        return Err(ConfigError::Validation(
                            "layout strip-scroll-step is missing value".to_string(),
                        ));
                    };
                    projection.strip_scroll_step = step.parse::<u32>().map_err(|_| {
                        ConfigError::Validation(
                            "layout strip-scroll-step must be a positive integer".to_string(),
                        )
                    })?;
                }
                "default-column-mode" => {
                    let Some(mode) = tokens.get(1) else {
                        return Err(ConfigError::Validation(
                            "layout default-column-mode is missing value".to_string(),
                        ));
                    };
                    projection.default_column_mode = parse_column_mode(mode)?;
                }
                "default-column-width" => {
                    projection.default_column_width = parse_width_tokens(&tokens[1..])?;
                }
                _ => {}
            },
            Some("input") => match tokens[0].as_str() {
                "bind-control-mode" => {
                    let Some(mode) = tokens.get(1) else {
                        return Err(ConfigError::Validation(
                            "input bind-control-mode is missing value".to_string(),
                        ));
                    };
                    projection.bind_control_mode = parse_bind_control_mode(mode)?;
                }
                "hotkey" => {
                    let Some(trigger) = tokens.get(1).cloned() else {
                        return Err(ConfigError::Validation(
                            "input hotkey is missing trigger".to_string(),
                        ));
                    };
                    let Some(command) = tokens.get(2).cloned() else {
                        return Err(ConfigError::Validation(
                            "input hotkey is missing command".to_string(),
                        ));
                    };
                    hotkeys.push(HotkeyBinding { trigger, command });
                }
                _ => {}
            },
            Some("rules") if current_rule.is_some() && !in_actions => {
                let rule = current_rule.as_mut().expect("rule should exist");
                match tokens[0].as_str() {
                    "priority" => {
                        let Some(priority) = tokens.get(1) else {
                            return Err(ConfigError::Validation(
                                "rule priority is missing value".to_string(),
                            ));
                        };
                        rule.priority = priority.parse::<i32>().map_err(|_| {
                            ConfigError::Validation(
                                "rule priority must be a valid integer".to_string(),
                            )
                        })?;
                    }
                    "enabled" => {
                        let Some(enabled) = tokens.get(1) else {
                            return Err(ConfigError::Validation(
                                "rule enabled flag is missing value".to_string(),
                            ));
                        };
                        rule.enabled = match enabled.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                return Err(ConfigError::Validation(
                                    "rule enabled must be true or false".to_string(),
                                ));
                            }
                        };
                    }
                    "match-process-name" => rule.matchers.process_name = tokens.get(1).cloned(),
                    "match-class-substring" => {
                        rule.matchers.class_substring = tokens.get(1).cloned()
                    }
                    "match-title-substring" => {
                        rule.matchers.title_substring = tokens.get(1).cloned()
                    }
                    _ => {}
                }
            }
            Some("rules") if current_rule.is_some() && in_actions => {
                let actions = &mut current_rule.as_mut().expect("rule should exist").actions;
                match tokens[0].as_str() {
                    "layer" => {
                        let Some(layer) = tokens.get(1) else {
                            return Err(ConfigError::Validation(
                                "rule action layer is missing value".to_string(),
                            ));
                        };
                        actions.layer = Some(parse_layer(layer)?);
                    }
                    "column-mode" => {
                        let Some(mode) = tokens.get(1) else {
                            return Err(ConfigError::Validation(
                                "rule action column-mode is missing value".to_string(),
                            ));
                        };
                        actions.column_mode = Some(parse_column_mode(mode)?);
                    }
                    "width" => actions.width_semantics = Some(parse_width_tokens(&tokens[1..])?),
                    "managed" => {
                        let Some(managed) = tokens.get(1) else {
                            return Err(ConfigError::Validation(
                                "rule action managed is missing value".to_string(),
                            ));
                        };
                        actions.managed = Some(match managed.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                return Err(ConfigError::Validation(
                                    "rule action managed must be true or false".to_string(),
                                ));
                            }
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if let Some(rule) = current_rule.take() {
        rules.push(rule);
    }

    rules.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    projection.active_rule_count = rules.iter().filter(|rule| rule.enabled).count();

    Ok(LoadedConfig {
        projection,
        hotkeys,
        rules,
    })
}

pub fn classify_window(
    rules: &[WindowRule],
    input: &WindowRuleInput,
    projection: &ConfigProjection,
) -> WindowRuleDecision {
    let mut decision = WindowRuleDecision::from_projection(projection);
    for rule in rules {
        if !rule.enabled || !rule_matches(rule, input) {
            continue;
        }

        decision.matched_rule_ids.push(rule.id.clone());
        if let Some(layer) = rule.actions.layer {
            decision.layer = layer;
        }
        if let Some(column_mode) = rule.actions.column_mode {
            decision.column_mode = column_mode;
        }
        if let Some(width_semantics) = rule.actions.width_semantics {
            decision.width_semantics = width_semantics;
            decision.width_semantics_explicit = true;
        }
        if let Some(managed) = rule.actions.managed {
            decision.managed = managed;
        }
    }

    decision
}

fn parse_layout(node: &KdlNode, projection: &mut ConfigProjection) -> Result<(), ConfigError> {
    for child in child_nodes(node) {
        match child.name().value() {
            "strip-scroll-step" => {
                if let Some(step) = first_integer(child)? {
                    projection.strip_scroll_step = u32::try_from(step).map_err(|_| {
                        ConfigError::Validation(
                            "layout strip-scroll-step must be positive".to_string(),
                        )
                    })?;
                }
            }
            "default-column-mode" => {
                if let Some(value) = first_string(child)? {
                    projection.default_column_mode = parse_column_mode(&value)?;
                }
            }
            "default-column-width" => {
                projection.default_column_width = parse_width_node(child)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn parse_input(
    node: &KdlNode,
    projection: &mut ConfigProjection,
    hotkeys: &mut Vec<HotkeyBinding>,
) -> Result<(), ConfigError> {
    for child in child_nodes(node) {
        match child.name().value() {
            "bind-control-mode" => {
                if let Some(value) = first_string(child)? {
                    projection.bind_control_mode = parse_bind_control_mode(&value)?;
                }
            }
            "hotkey" => {
                let trigger = nth_string(child, 0)?.ok_or_else(|| {
                    ConfigError::Validation("input hotkey is missing trigger".to_string())
                })?;
                let command = nth_string(child, 1)?.ok_or_else(|| {
                    ConfigError::Validation("input hotkey is missing command".to_string())
                })?;
                hotkeys.push(HotkeyBinding { trigger, command });
            }
            _ => {}
        }
    }

    Ok(())
}

fn parse_rules(node: &KdlNode, rules: &mut Vec<WindowRule>) -> Result<(), ConfigError> {
    for child in child_nodes(node) {
        if child.name().value() != "rule" {
            continue;
        }

        let id = first_string(child)?
            .ok_or_else(|| ConfigError::Validation("rule id is missing".to_string()))?;
        let mut rule = WindowRule {
            id,
            priority: 0,
            enabled: true,
            matchers: WindowRuleMatch {
                process_name: None,
                class_substring: None,
                title_substring: None,
            },
            actions: WindowRuleActions {
                layer: None,
                column_mode: None,
                width_semantics: None,
                managed: None,
            },
        };

        for grandchild in child_nodes(child) {
            match grandchild.name().value() {
                "priority" => {
                    if let Some(priority) = first_integer(grandchild)? {
                        rule.priority = i32::try_from(priority).map_err(|_| {
                            ConfigError::Validation("rule priority is out of range".to_string())
                        })?;
                    }
                }
                "enabled" => {
                    if let Some(enabled) = first_bool(grandchild)? {
                        rule.enabled = enabled;
                    }
                }
                "match-process-name" => rule.matchers.process_name = first_string(grandchild)?,
                "match-class-substring" => {
                    rule.matchers.class_substring = first_string(grandchild)?
                }
                "match-title-substring" => {
                    rule.matchers.title_substring = first_string(grandchild)?
                }
                "actions" => parse_rule_actions(grandchild, &mut rule.actions)?,
                _ => {}
            }
        }

        rules.push(rule);
    }

    Ok(())
}

fn parse_rule_actions(node: &KdlNode, actions: &mut WindowRuleActions) -> Result<(), ConfigError> {
    for child in child_nodes(node) {
        match child.name().value() {
            "layer" => {
                if let Some(layer) = first_string(child)? {
                    actions.layer = Some(parse_layer(&layer)?);
                }
            }
            "column-mode" => {
                if let Some(mode) = first_string(child)? {
                    actions.column_mode = Some(parse_column_mode(&mode)?);
                }
            }
            "width" => actions.width_semantics = Some(parse_width_node(child)?),
            "managed" => actions.managed = first_bool(child)?,
            _ => {}
        }
    }

    Ok(())
}

fn parse_column_mode(value: &str) -> Result<ColumnMode, ConfigError> {
    match value {
        "normal" => Ok(ColumnMode::Normal),
        "tabbed" => Ok(ColumnMode::Tabbed),
        "maximized-column" => Ok(ColumnMode::MaximizedColumn),
        "custom-width" => Ok(ColumnMode::CustomWidth),
        other => Err(ConfigError::Validation(format!(
            "unsupported column mode '{other}'"
        ))),
    }
}

fn parse_bind_control_mode(value: &str) -> Result<BindControlMode, ConfigError> {
    BindControlMode::parse(value)
        .ok_or_else(|| ConfigError::Validation(format!("unsupported bind control mode '{value}'")))
}

fn parse_layer(value: &str) -> Result<WindowLayer, ConfigError> {
    match value {
        "tiled" => Ok(WindowLayer::Tiled),
        "floating" => Ok(WindowLayer::Floating),
        "fullscreen" => Ok(WindowLayer::Fullscreen),
        other => Err(ConfigError::Validation(format!(
            "unsupported window layer '{other}'"
        ))),
    }
}

fn parse_width_node(node: &KdlNode) -> Result<WidthSemantics, ConfigError> {
    let Some(kind) = nth_string(node, 0)? else {
        return Err(ConfigError::Validation(
            "width node is missing width kind".to_string(),
        ));
    };

    match kind.as_str() {
        "fixed" => {
            let value = nth_integer(node, 1)?.ok_or_else(|| {
                ConfigError::Validation("fixed width requires a numeric value".to_string())
            })?;
            Ok(WidthSemantics::Fixed(u32::try_from(value).map_err(
                |_| ConfigError::Validation("fixed width must be positive".to_string()),
            )?))
        }
        "fraction" => {
            let numerator = nth_integer(node, 1)?.ok_or_else(|| {
                ConfigError::Validation("fraction width requires numerator".to_string())
            })?;
            let denominator = nth_integer(node, 2)?.ok_or_else(|| {
                ConfigError::Validation("fraction width requires denominator".to_string())
            })?;
            Ok(WidthSemantics::MonitorFraction {
                numerator: u32::try_from(numerator).map_err(|_| {
                    ConfigError::Validation("fraction numerator must be positive".to_string())
                })?,
                denominator: u32::try_from(denominator).map_err(|_| {
                    ConfigError::Validation("fraction denominator must be positive".to_string())
                })?,
            })
        }
        "full" => Ok(WidthSemantics::Full),
        other => Err(ConfigError::Validation(format!(
            "unsupported width kind '{other}'"
        ))),
    }
}

fn parse_width_tokens(tokens: &[String]) -> Result<WidthSemantics, ConfigError> {
    let Some(kind) = tokens.first() else {
        return Err(ConfigError::Validation(
            "width entry is missing width kind".to_string(),
        ));
    };

    match kind.as_str() {
        "fixed" => {
            let Some(value) = tokens.get(1) else {
                return Err(ConfigError::Validation(
                    "fixed width requires a numeric value".to_string(),
                ));
            };
            Ok(WidthSemantics::Fixed(value.parse::<u32>().map_err(
                |_| ConfigError::Validation("fixed width must be a positive integer".to_string()),
            )?))
        }
        "fraction" => {
            let Some(numerator) = tokens.get(1) else {
                return Err(ConfigError::Validation(
                    "fraction width requires numerator".to_string(),
                ));
            };
            let Some(denominator) = tokens.get(2) else {
                return Err(ConfigError::Validation(
                    "fraction width requires denominator".to_string(),
                ));
            };
            Ok(WidthSemantics::MonitorFraction {
                numerator: numerator.parse::<u32>().map_err(|_| {
                    ConfigError::Validation(
                        "fraction numerator must be a positive integer".to_string(),
                    )
                })?,
                denominator: denominator.parse::<u32>().map_err(|_| {
                    ConfigError::Validation(
                        "fraction denominator must be a positive integer".to_string(),
                    )
                })?,
            })
        }
        "full" => Ok(WidthSemantics::Full),
        other => Err(ConfigError::Validation(format!(
            "unsupported width kind '{other}'"
        ))),
    }
}

fn tokenize_kdl_like(line: &str) -> Result<Vec<String>, ConfigError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for character in line.chars() {
        match character {
            '"' => {
                if in_quotes {
                    tokens.push(current.clone());
                    current.clear();
                    in_quotes = false;
                } else {
                    if !current.trim().is_empty() {
                        tokens.push(current.trim().to_string());
                        current.clear();
                    }
                    in_quotes = true;
                }
            }
            character if character.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(character),
        }
    }

    if in_quotes {
        return Err(ConfigError::Parse(
            "unterminated quoted string in config".to_string(),
        ));
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

fn rule_matches(rule: &WindowRule, input: &WindowRuleInput) -> bool {
    if let Some(process_name) = &rule.matchers.process_name {
        let Some(candidate_process) = &input.process_name else {
            return false;
        };
        if !contains_ci(candidate_process, process_name) {
            return false;
        }
    }

    if let Some(class_substring) = &rule.matchers.class_substring
        && !contains_ci(&input.class_name, class_substring)
    {
        return false;
    }

    if let Some(title_substring) = &rule.matchers.title_substring
        && !contains_ci(&input.title, title_substring)
    {
        return false;
    }

    true
}

fn contains_ci(value: &str, needle: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn child_nodes(node: &KdlNode) -> &[KdlNode] {
    node.children().map(KdlDocument::nodes).unwrap_or(&[])
}

fn first_string(node: &KdlNode) -> Result<Option<String>, ConfigError> {
    nth_string(node, 0)
}

fn nth_string(node: &KdlNode, index: usize) -> Result<Option<String>, ConfigError> {
    Ok(match nth_value(node, index) {
        Some(KdlValue::String(value)) => Some(value.to_string()),
        Some(other) => {
            return Err(ConfigError::Validation(format!(
                "node '{}' expects string argument at position {} but found {other:?}",
                node.name().value(),
                index
            )));
        }
        None => None,
    })
}

fn first_integer(node: &KdlNode) -> Result<Option<i64>, ConfigError> {
    nth_integer(node, 0)
}

fn nth_integer(node: &KdlNode, index: usize) -> Result<Option<i64>, ConfigError> {
    Ok(match nth_value(node, index) {
        Some(KdlValue::Integer(value)) => Some(i64::try_from(*value).map_err(|_| {
            ConfigError::Validation(format!(
                "node '{}' integer argument at position {} is out of i64 range",
                node.name().value(),
                index
            ))
        })?),
        Some(other) => {
            return Err(ConfigError::Validation(format!(
                "node '{}' expects integer argument at position {} but found {other:?}",
                node.name().value(),
                index
            )));
        }
        None => None,
    })
}

fn first_bool(node: &KdlNode) -> Result<Option<bool>, ConfigError> {
    Ok(match nth_value(node, 0) {
        Some(KdlValue::Bool(value)) => Some(*value),
        Some(other) => {
            return Err(ConfigError::Validation(format!(
                "node '{}' expects bool argument but found {other:?}",
                node.name().value()
            )));
        }
        None => None,
    })
}

fn nth_value(node: &KdlNode, index: usize) -> Option<&KdlValue> {
    node.entries().get(index).map(|entry| entry.value())
}

pub fn ensure_default_config(path: impl AsRef<Path>) -> Result<PathBuf, ConfigError> {
    let path = path.as_ref();
    if path.exists() {
        return Ok(path.to_path_buf());
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, default_config_source())?;
    Ok(path.to_path_buf())
}

pub fn default_config_source() -> String {
    let mut lines = vec![
        "general {".to_string(),
        "  mode \"wm-only\"".to_string(),
        "}".to_string(),
        String::new(),
        "layout {".to_string(),
        "  strip-scroll-step 240".to_string(),
        "  default-column-mode \"normal\"".to_string(),
        "  default-column-width \"fraction\" 1 2".to_string(),
        "}".to_string(),
        String::new(),
        "input {".to_string(),
        "  bind-control-mode \"coexistence\"".to_string(),
    ];

    lines.extend(
        DEFAULT_HOTKEY_BINDINGS
            .iter()
            .map(|(trigger, command)| format!("  hotkey \"{trigger}\" \"{command}\"")),
    );

    lines.extend([
        "}".to_string(),
        String::new(),
        "rules {".to_string(),
        "  rule \"float-dialogs\" {".to_string(),
        "    priority 100".to_string(),
        "    enabled true".to_string(),
        "    match-class-substring \"Dialog\"".to_string(),
        "    actions {".to_string(),
        "      layer \"floating\"".to_string(),
        "    }".to_string(),
        "  }".to_string(),
        String::new(),
        "  rule \"float-settings\" {".to_string(),
        "    priority 110".to_string(),
        "    enabled true".to_string(),
        "    match-title-substring \"Settings\"".to_string(),
        "    actions {".to_string(),
        "      layer \"floating\"".to_string(),
        "    }".to_string(),
        "  }".to_string(),
        "}".to_string(),
        String::new(),
    ]);

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        DEFAULT_CONFIG_PATH, WindowRuleInput, bootstrap, classify_window, default_config_source,
        default_loaded_config, ensure_default_config, load_from_path, load_or_default,
        parse_kdl_like_source,
    };
    use flowtile_domain::{BindControlMode, ColumnMode, WidthSemantics, WindowLayer};

    #[test]
    fn exposes_expected_bootstrap_contract() {
        let bootstrap = bootstrap();
        assert_eq!(bootstrap.preferred_format, super::PREFERRED_CONFIG_FORMAT);
        assert_eq!(bootstrap.default_path, DEFAULT_CONFIG_PATH);
        assert!(bootstrap.supports_live_reload);
        assert!(bootstrap.supports_rollback);
    }

    #[test]
    fn returns_default_projection_when_config_is_missing() {
        let config = load_or_default(unique_test_path("missing"), 5).expect("config should load");
        assert_eq!(config.projection.config_version, 5);
        assert_eq!(
            config.projection.bind_control_mode,
            BindControlMode::Coexistence
        );
        assert_eq!(config.projection.strip_scroll_step, 240);
        assert_eq!(config.projection.default_column_mode, ColumnMode::Normal);
        assert_eq!(config.hotkeys.len(), 5);
        assert_eq!(config.hotkeys, super::default_hotkeys());
    }

    #[test]
    fn parses_kdl_layout_and_rules() {
        let path = unique_test_path("kdl");
        std::fs::create_dir_all(path.parent().expect("temp dir should exist"))
            .expect("temp dir should be created");
        std::fs::write(&path, default_config_source()).expect("config should be written");

        let config = load_from_path(&path, 7).expect("kdl config should parse");
        assert_eq!(config.projection.config_version, 7);
        assert_eq!(
            config.projection.bind_control_mode,
            BindControlMode::Coexistence
        );
        assert_eq!(config.projection.strip_scroll_step, 240);
        assert_eq!(
            config.projection.default_column_width,
            WidthSemantics::MonitorFraction {
                numerator: 1,
                denominator: 2
            }
        );
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.projection.active_rule_count, 2);
    }

    #[test]
    fn rules_assign_floating_layer_by_class() {
        let config = default_loaded_config(1, DEFAULT_CONFIG_PATH);
        let mut enriched = config.clone();
        enriched.rules.push(super::WindowRule {
            id: "float-dialogs".to_string(),
            priority: 10,
            enabled: true,
            matchers: super::WindowRuleMatch {
                process_name: None,
                class_substring: Some("Dialog".to_string()),
                title_substring: None,
            },
            actions: super::WindowRuleActions {
                layer: Some(WindowLayer::Floating),
                column_mode: Some(ColumnMode::Tabbed),
                width_semantics: Some(WidthSemantics::Fixed(420)),
                managed: Some(true),
            },
        });

        let decision = classify_window(
            &enriched.rules,
            &WindowRuleInput {
                process_name: Some("notepad".to_string()),
                class_name: "FileOpenDialog".to_string(),
                title: "Open".to_string(),
            },
            &enriched.projection,
        );

        assert_eq!(decision.layer, WindowLayer::Floating);
        assert_eq!(decision.column_mode, ColumnMode::Tabbed);
        assert_eq!(decision.width_semantics, WidthSemantics::Fixed(420));
        assert!(decision.width_semantics_explicit);
        assert_eq!(decision.matched_rule_ids, vec!["float-dialogs".to_string()]);
    }

    #[test]
    fn default_rule_decision_keeps_width_non_explicit() {
        let config = default_loaded_config(1, DEFAULT_CONFIG_PATH);
        let decision = classify_window(
            &config.rules,
            &WindowRuleInput {
                process_name: Some("notepad".to_string()),
                class_name: "Notepad".to_string(),
                title: "Untitled - Notepad".to_string(),
            },
            &config.projection,
        );

        assert_eq!(
            decision.width_semantics,
            config.projection.default_column_width
        );
        assert!(!decision.width_semantics_explicit);
    }

    #[test]
    fn materializes_default_config_file_when_requested() {
        let path = unique_test_path("ensure");
        let created_path = ensure_default_config(&path).expect("config file should be created");
        let source = std::fs::read_to_string(&created_path).expect("config should be readable");

        assert_eq!(created_path, path);
        assert!(source.contains("bind-control-mode \"coexistence\""));
        assert!(source.contains("strip-scroll-step 240"));
        assert!(source.contains("hotkey \"Win+H\" \"focus-prev\""));
        assert!(source.contains("hotkey \"Win+J\" \"focus-next\""));
        assert!(!source.contains("hotkey \"Alt+J\" \"focus-next\""));
        assert!(source.contains("rule \"float-dialogs\""));
    }

    #[test]
    fn parses_bind_control_mode_from_kdl_like_fallback() {
        let source = r#"
input {
  bind-control-mode "managed-shell"
}
"#;

        let config = parse_kdl_like_source(source, std::path::Path::new(DEFAULT_CONFIG_PATH), 9)
            .expect("kdl-like fallback should parse bind control mode");

        assert_eq!(
            config.projection.bind_control_mode,
            BindControlMode::ManagedShell
        );
    }

    fn unique_test_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir()
            .join("flowtilewm-config-tests")
            .join(format!("{label}-{nonce}.kdl"))
    }
}
