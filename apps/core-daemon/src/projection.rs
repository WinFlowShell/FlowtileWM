use std::collections::HashMap;

use flowtile_domain::{FocusOrigin, WindowClassification, WindowLayer};
use flowtile_ipc::{
    ConfigProjection, DiagnosticsProjection, FocusProjection, InsetsProjection, OutputProjection,
    OverviewProjection, RectProjection, SnapshotProjection,
    SurrogatePresentationDiagnosticsProjection, WindowProjection, WorkspaceProjection,
};
use flowtile_windows_adapter::PlatformWindowSnapshot;
use flowtile_wm_core::CoreDaemonRuntime;

use crate::touchpad::assess_touchpad_override;

pub fn build_snapshot_projection(runtime: &CoreDaemonRuntime) -> SnapshotProjection {
    let state = runtime.state();
    let touchpad = assess_touchpad_override(runtime.touchpad_config());
    let perf = runtime.perf_snapshot();
    let surrogate_presentation = runtime.surrogate_presentation_diagnostics();
    let metadata_by_hwnd = runtime
        .last_snapshot()
        .map(|snapshot| {
            snapshot
                .windows
                .iter()
                .map(|window| (window.hwnd, window))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    let outputs = state
        .monitors
        .values()
        .map(|monitor| {
            let workspace_count = state
                .workspace_sets
                .get(&monitor.workspace_set_id)
                .map(|workspace_set| workspace_set.ordered_workspace_ids.len())
                .unwrap_or(0);

            OutputProjection {
                monitor_id: monitor.id.get(),
                binding: monitor.platform_binding.clone(),
                dpi: monitor.dpi,
                is_primary: monitor.is_primary_hint,
                work_area: map_rect(monitor.work_area_rect),
                workspace_count,
                active_workspace_id: state
                    .active_workspace_id_for_monitor(monitor.id)
                    .map(|workspace_id| workspace_id.get()),
            }
        })
        .collect::<Vec<_>>();

    let mut workspaces = state
        .workspaces
        .values()
        .map(|workspace| {
            let tiled_window_count = workspace
                .strip
                .ordered_column_ids
                .iter()
                .filter_map(|column_id| state.layout.columns.get(column_id))
                .map(|column| column.ordered_window_ids.len())
                .sum::<usize>();

            WorkspaceProjection {
                workspace_id: workspace.id.get(),
                monitor_id: workspace.monitor_id.get(),
                vertical_index: workspace.vertical_index,
                name: workspace.name.clone(),
                is_active: state.active_workspace_id_for_monitor(workspace.monitor_id)
                    == Some(workspace.id),
                is_empty: state.is_workspace_empty(workspace.id),
                is_tail: workspace.is_ephemeral_empty_tail,
                scroll_offset: workspace.strip.scroll_offset,
                column_count: workspace.strip.ordered_column_ids.len(),
                tiled_window_count,
                floating_window_count: workspace.floating_layer.ordered_window_ids.len(),
            }
        })
        .collect::<Vec<_>>();
    workspaces.sort_by(|left, right| {
        left.monitor_id
            .cmp(&right.monitor_id)
            .then_with(|| left.vertical_index.cmp(&right.vertical_index))
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
    });

    let mut windows = state
        .windows
        .values()
        .filter_map(|window| {
            let metadata = window
                .current_hwnd_binding
                .and_then(|hwnd| metadata_by_hwnd.get(&hwnd).copied());
            let workspace = state.workspaces.get(&window.workspace_id)?;

            Some(WindowProjection {
                window_id: window.id.get(),
                monitor_id: workspace.monitor_id.get(),
                workspace_id: window.workspace_id.get(),
                column_id: window.column_id.map(|column_id| column_id.get()),
                hwnd: window.current_hwnd_binding,
                title: metadata
                    .map(|window| window.title.clone())
                    .unwrap_or_else(|| fallback_window_title(window.current_hwnd_binding)),
                class_name: metadata
                    .map(|window| window.class_name.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                process_name: metadata.and_then(|window| window.process_name.clone()),
                layer: window_layer_name(window.layer).to_string(),
                classification: window_classification_name(window.classification).to_string(),
                is_managed: window.is_managed,
                is_focused: state.focus.focused_window_id == Some(window.id),
            })
        })
        .collect::<Vec<_>>();
    windows.sort_by(|left, right| {
        left.monitor_id
            .cmp(&right.monitor_id)
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
            .then_with(|| left.column_id.cmp(&right.column_id))
            .then_with(|| left.window_id.cmp(&right.window_id))
    });

    let focused_workspace_id = state
        .focus
        .focused_window_id
        .and_then(|window_id| {
            state
                .windows
                .get(&window_id)
                .map(|window| window.workspace_id.get())
        })
        .or_else(|| {
            state
                .focus
                .focused_monitor_id
                .and_then(|monitor_id| state.active_workspace_id_for_monitor(monitor_id))
                .map(|workspace_id| workspace_id.get())
        });

    SnapshotProjection {
        version_line: flowtile_domain::VERSION_LINE.to_string(),
        runtime_mode: state.runtime.boot_mode.as_str().to_string(),
        state_version: state.state_version().get(),
        outputs,
        workspaces,
        windows,
        focus: FocusProjection {
            monitor_id: state
                .focus
                .focused_monitor_id
                .map(|monitor_id| monitor_id.get()),
            workspace_id: focused_workspace_id,
            column_id: state
                .focus
                .focused_column_id
                .map(|column_id| column_id.get()),
            window_id: state
                .focus
                .focused_window_id
                .map(|window_id| window_id.get()),
            origin: focus_origin_name(state.focus.focus_origin).to_string(),
        },
        overview: OverviewProjection {
            is_open: state.overview.is_open,
            monitor_id: state.overview.monitor_id.map(|monitor_id| monitor_id.get()),
            selection_workspace_id: state
                .overview
                .selection
                .map(|workspace_id| workspace_id.get()),
            projection_version: state.overview.projection_version,
        },
        diagnostics: DiagnosticsProjection {
            total_records: state.diagnostics_summary.total_records,
            last_transition_label: state.diagnostics_summary.last_transition_label.clone(),
            degraded_flags: state.runtime.degraded_flags.clone(),
            management_enabled: runtime.management_enabled(),
            touchpad_override_status: touchpad.summary_label().to_string(),
            touchpad_override_detail: touchpad.detail.clone(),
            perf,
            surrogate_presentation: SurrogatePresentationDiagnosticsProjection {
                active_hosts: surrogate_presentation.active_hosts,
                show_requests: surrogate_presentation.show_requests,
                hide_requests: surrogate_presentation.hide_requests,
                classifier_rejections: surrogate_presentation.classifier_rejections,
                native_fallbacks: surrogate_presentation.native_fallbacks,
                transient_escapes: surrogate_presentation.transient_escapes,
                handoff_promotions: surrogate_presentation.handoff_promotions,
                pointer_replay_attempts: surrogate_presentation.pointer_replay_attempts,
                pointer_replay_successes: surrogate_presentation.pointer_replay_successes,
                pointer_replay_failures: surrogate_presentation.pointer_replay_failures,
                dwm_thumbnail_backend_uses: surrogate_presentation.dwm_thumbnail_backend_uses,
                last_event: surrogate_presentation.last_event,
            },
        },
        config: ConfigProjection {
            config_version: state.config_projection.config_version,
            source_path: state.config_projection.source_path.clone(),
            bind_control_mode: state
                .config_projection
                .bind_control_mode
                .as_str()
                .to_string(),
            touchpad_override_enabled: touchpad.requested,
            touchpad_gesture_count: touchpad.configured_gesture_count,
            active_rule_count: state.config_projection.active_rule_count,
            strip_scroll_step: state.config_projection.strip_scroll_step,
            default_column_mode: state
                .config_projection
                .default_column_mode
                .as_str()
                .to_string(),
            outer_padding: InsetsProjection {
                left: state.config_projection.layout_spacing.outer_padding.left,
                top: state.config_projection.layout_spacing.outer_padding.top,
                right: state.config_projection.layout_spacing.outer_padding.right,
                bottom: state.config_projection.layout_spacing.outer_padding.bottom,
            },
            column_gap: state.config_projection.layout_spacing.column_gap,
            window_gap: state.config_projection.layout_spacing.window_gap,
            floating_margin: state.config_projection.layout_spacing.floating_margin,
        },
    }
}

fn map_rect(rect: flowtile_domain::Rect) -> RectProjection {
    RectProjection {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn fallback_window_title(hwnd: Option<u64>) -> String {
    hwnd.map(|hwnd| format!("HWND {hwnd}"))
        .unwrap_or_else(|| "window".to_string())
}

fn focus_origin_name(origin: FocusOrigin) -> &'static str {
    match origin {
        FocusOrigin::ReducerDefault => "reducer-default",
        FocusOrigin::UserCommand => "user-command",
        FocusOrigin::PlatformObservation => "platform-observation",
    }
}

fn window_layer_name(layer: WindowLayer) -> &'static str {
    match layer {
        WindowLayer::Tiled => "tiled",
        WindowLayer::Floating => "floating",
        WindowLayer::Fullscreen => "fullscreen",
    }
}

fn window_classification_name(classification: WindowClassification) -> &'static str {
    match classification {
        WindowClassification::Application => "application",
        WindowClassification::Utility => "utility",
        WindowClassification::Overlay => "overlay",
    }
}

#[allow(dead_code)]
fn _metadata_title(window: &PlatformWindowSnapshot) -> &str {
    &window.title
}
