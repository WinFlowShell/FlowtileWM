use std::collections::{HashMap, HashSet};

use flowtile_domain::{Rect, WindowLayer};
use flowtile_windows_adapter::{
    ApplyOperation, PlatformSnapshot, WindowOpacityMode, WindowVisualEmphasis, needs_geometry_apply,
};

pub(super) fn operations_are_activation_only(
    snapshot: &PlatformSnapshot,
    operations: &[ApplyOperation],
) -> bool {
    if operations.is_empty() {
        return false;
    }

    let actual_windows = snapshot
        .windows
        .iter()
        .map(|window| (window.hwnd, window.rect))
        .collect::<HashMap<_, _>>();

    operations.iter().all(|operation| {
        operation.activate
            && actual_windows
                .get(&operation.hwnd)
                .is_some_and(|actual_rect| !needs_geometry_apply(*actual_rect, operation.rect))
    })
}

pub(super) fn should_animate_tiled_geometry(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    ) || should_animate_workspace_switch(reason)
}

fn should_animate_workspace_switch(reason: &str) -> bool {
    let token = normalize_reason_token(reason);
    token.contains("focus-workspace-up") || token.contains("focus-workspace-down")
}

pub(super) fn should_defer_post_apply_retry(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    )
}

pub(super) fn should_force_activation_reassert(reason: &str) -> bool {
    matches!(
        reason,
        "manual-column-width-commit" | "manual-cycle-column-width"
    )
}

pub(super) fn visual_emphasis_has_effect(emphasis: &WindowVisualEmphasis) -> bool {
    emphasis.opacity_alpha.is_some()
        || emphasis.force_clear_layered_style
        || emphasis.border_color_rgb.is_some()
        || !emphasis.disable_visual_effects
}

pub(super) fn should_suppress_visual_gap(
    layer: WindowLayer,
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    layer == WindowLayer::Tiled
        && supports_nonessential_tiled_window_effects(process_name, class_name, title)
}

fn supports_nonessential_tiled_window_effects(
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    classify_window_visual_safety(process_name, class_name, title)
        == WindowVisualSafety::SafeFullEmphasis
}

pub(super) fn supports_tiled_window_switch_animation(
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> bool {
    matches!(
        classify_window_visual_safety(process_name, class_name, title),
        WindowVisualSafety::SafeFullEmphasis | WindowVisualSafety::BrowserOpacityOnly
    )
}

pub(super) struct WindowTraceLine<'a> {
    pub stage: &'a str,
    pub remaining_label: Option<&'a str>,
    pub hwnd: u64,
    pub process_id: u32,
    pub process_name: &'a str,
    pub layer: Option<WindowLayer>,
    pub title: String,
    pub focused: bool,
    pub management_candidate: bool,
    pub managed: bool,
    pub workspace_id: Option<u64>,
    pub column_id: Option<u64>,
    pub observed_rect: Rect,
    pub operation: Option<&'a ApplyOperation>,
}

pub(super) fn format_window_trace_line(trace: WindowTraceLine<'_>) -> String {
    let target_rect = trace.operation.map(|item| item.rect);
    let delta = target_rect
        .map(|rect| {
            format!(
                "({},{ } {}x{})",
                rect.x as i64 - trace.observed_rect.x as i64,
                rect.y as i64 - trace.observed_rect.y as i64,
                rect.width as i64 - trace.observed_rect.width as i64,
                rect.height as i64 - trace.observed_rect.height as i64
            )
        })
        .unwrap_or_else(|| "none".to_string())
        .replace(", ", ",");

    format!(
        "window-trace[{stage}]: hwnd={} process={} pid={} layer={} title=\"{}\" focused={} candidate={} managed={} workspace={:?} column={:?} observed=({},{} {}x{}) target={} delta={} apply_geometry={} activate={} animated={} suppress_gap={} status={}",
        trace.hwnd,
        sanitize_log_text(trace.process_name),
        trace.process_id,
        trace.layer.map(window_layer_name).unwrap_or("untracked"),
        trace.title,
        trace.focused,
        trace.management_candidate,
        trace.managed,
        trace.workspace_id,
        trace.column_id,
        trace.observed_rect.x,
        trace.observed_rect.y,
        trace.observed_rect.width,
        trace.observed_rect.height,
        target_rect
            .map(|rect| format!("({},{} {}x{})", rect.x, rect.y, rect.width, rect.height))
            .unwrap_or_else(|| "none".to_string()),
        delta,
        trace.operation.is_some_and(|item| item.apply_geometry),
        trace.operation.is_some_and(|item| item.activate),
        trace
            .operation
            .is_some_and(|item| item.window_switch_animation.is_some()),
        trace.operation.is_some_and(|item| item.suppress_visual_gap),
        trace
            .remaining_label
            .unwrap_or_else(|| if trace.operation.is_some() {
                "planned"
            } else {
                "steady"
            }),
        stage = trace.stage
    )
}

pub(super) fn window_layer_name(layer: WindowLayer) -> &'static str {
    match layer {
        WindowLayer::Tiled => "tiled",
        WindowLayer::Floating => "floating",
        WindowLayer::Fullscreen => "fullscreen",
    }
}

pub(super) fn sanitize_log_text(text: &str) -> String {
    text.replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('"', "'")
}

pub(super) fn should_auto_unwind_after_desync(
    remaining_operations: &[ApplyOperation],
    consecutive_desync_cycles: u32,
) -> bool {
    if consecutive_desync_cycles < 3 {
        return false;
    }

    let affected_windows = remaining_operations
        .iter()
        .map(|operation| operation.hwnd)
        .collect::<HashSet<_>>();

    affected_windows.len() > 1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WindowVisualSafety {
    SafeFullEmphasis,
    BrowserOpacityOnly,
    SkipVisualEmphasis,
}

pub(super) fn build_visual_emphasis(
    is_active_window: bool,
    process_name: Option<&str>,
    class_name: &str,
    title: &str,
) -> WindowVisualEmphasis {
    match classify_window_visual_safety(process_name, class_name, title) {
        WindowVisualSafety::SafeFullEmphasis => WindowVisualEmphasis {
            opacity_alpha: inactive_window_opacity_alpha(is_active_window),
            opacity_mode: WindowOpacityMode::DirectLayered,
            // Active managed windows must always converge back to a fully opaque baseline even
            // if a previous opacity pass or an older daemon run left the HWND layered.
            force_clear_layered_style: is_active_window,
            disable_visual_effects: false,
            border_color_rgb: is_active_window.then_some(rgb_color(0x4C, 0xA8, 0xFF)),
            border_thickness_px: 3,
            rounded_corners: true,
        },
        WindowVisualSafety::BrowserOpacityOnly => WindowVisualEmphasis {
            opacity_alpha: inactive_window_opacity_alpha(is_active_window),
            opacity_mode: WindowOpacityMode::OverlayDim,
            force_clear_layered_style: is_active_window,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        },
        WindowVisualSafety::SkipVisualEmphasis => WindowVisualEmphasis {
            opacity_alpha: None,
            opacity_mode: WindowOpacityMode::DirectLayered,
            force_clear_layered_style: false,
            disable_visual_effects: true,
            border_color_rgb: None,
            border_thickness_px: 3,
            rounded_corners: false,
        },
    }
}

fn inactive_window_opacity_alpha(is_active_window: bool) -> Option<u8> {
    if is_active_window { None } else { Some(208) }
}

pub(super) fn classify_window_visual_safety(
    process_name: Option<&str>,
    class_name: &str,
    _title: &str,
) -> WindowVisualSafety {
    let normalized_class_name = normalize_class_name(class_name);
    let normalized_process_name = normalize_process_name(process_name);
    if normalized_class_name.is_none() && normalized_process_name.is_none() {
        return WindowVisualSafety::SkipVisualEmphasis;
    }
    if matches!(
        normalized_class_name.as_deref(),
        Some("chrome_widgetwin_0" | "chrome_widgetwin_1" | "mozillawindowclass")
    ) {
        return WindowVisualSafety::BrowserOpacityOnly;
    }

    if matches!(
        normalized_class_name.as_deref(),
        Some("org.wezfurlong.wezterm")
    ) {
        return WindowVisualSafety::SkipVisualEmphasis;
    }

    let Some(process_name) = normalized_process_name else {
        return WindowVisualSafety::SafeFullEmphasis;
    };
    if matches!(
        process_name.as_str(),
        "msedge"
            | "chrome"
            | "brave"
            | "opera"
            | "vivaldi"
            | "chromium"
            | "firefox"
            | "librewolf"
            | "waterfox"
    ) {
        return WindowVisualSafety::BrowserOpacityOnly;
    }

    if matches!(process_name.as_str(), "wezterm-gui") {
        return WindowVisualSafety::SkipVisualEmphasis;
    }

    WindowVisualSafety::SafeFullEmphasis
}

fn normalize_process_name(process_name: Option<&str>) -> Option<String> {
    let process_name = process_name?.trim();
    if process_name.is_empty() {
        return None;
    }

    let lowered = process_name.to_ascii_lowercase();
    Some(
        lowered
            .strip_suffix(".exe")
            .unwrap_or(lowered.as_str())
            .to_string(),
    )
}

fn normalize_class_name(class_name: &str) -> Option<String> {
    let class_name = class_name.trim();
    if class_name.is_empty() {
        return None;
    }

    Some(class_name.to_ascii_lowercase())
}

const fn rgb_color(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

pub(super) fn normalize_reason_token(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
}
