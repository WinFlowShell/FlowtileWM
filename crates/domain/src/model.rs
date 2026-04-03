use std::collections::BTreeMap;

use crate::{
    BootstrapProfile, ColumnId, MonitorId, Rect, Size, StateVersion, VERSION_LINE, WindowId,
    WorkspaceId, WorkspaceSetId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMode {
    WmOnly,
    ExtendedShell,
    SafeMode,
}

impl RuntimeMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WmOnly => "wm-only",
            Self::ExtendedShell => "extended-shell",
            Self::SafeMode => "safe-mode",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "wm-only" => Some(Self::WmOnly),
            "extended-shell" => Some(Self::ExtendedShell),
            "safe-mode" => Some(Self::SafeMode),
            _ => None,
        }
    }
}

impl core::fmt::Display for RuntimeMode {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BindControlMode {
    #[default]
    Coexistence,
    ManagedShell,
    DeepOverride,
}

impl BindControlMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coexistence => "coexistence",
            Self::ManagedShell => "managed-shell",
            Self::DeepOverride => "deep-override",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "coexistence" => Some(Self::Coexistence),
            "managed-shell" => Some(Self::ManagedShell),
            "deep-override" => Some(Self::DeepOverride),
            _ => None,
        }
    }
}

impl core::fmt::Display for BindControlMode {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyRole {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StripLayoutMode {
    #[default]
    Columns,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnMode {
    Normal,
    Tabbed,
    MaximizedColumn,
    CustomWidth,
}

impl ColumnMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Tabbed => "tabbed",
            Self::MaximizedColumn => "maximized-column",
            Self::CustomWidth => "custom-width",
        }
    }
}

pub const fn all_column_modes() -> [ColumnMode; 4] {
    [
        ColumnMode::Normal,
        ColumnMode::Tabbed,
        ColumnMode::MaximizedColumn,
        ColumnMode::CustomWidth,
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WidthSemantics {
    Fixed(u32),
    MonitorFraction { numerator: u32, denominator: u32 },
    Full,
}

impl Default for WidthSemantics {
    fn default() -> Self {
        Self::MonitorFraction {
            numerator: 1,
            denominator: 2,
        }
    }
}

impl WidthSemantics {
    pub fn resolve(self, monitor_width: u32) -> u32 {
        match self {
            Self::Fixed(width) => width.max(1),
            Self::MonitorFraction {
                numerator,
                denominator,
            } => if denominator == 0 {
                monitor_width.max(1)
            } else {
                ((monitor_width as u64 * numerator as u64) / denominator as u64) as u32
            }
            .max(1),
            Self::Full => monitor_width.max(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MaximizedState {
    #[default]
    Normal,
    Maximized,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowClassification {
    #[default]
    Application,
    Utility,
    Overlay,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowLayer {
    #[default]
    Tiled,
    Floating,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusOrigin {
    #[default]
    ReducerDefault,
    UserCommand,
    PlatformObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Monitor {
    pub id: MonitorId,
    pub platform_binding: Option<String>,
    pub work_area_rect: Rect,
    pub dpi: u32,
    pub topology_role: TopologyRole,
    pub workspace_set_id: WorkspaceSetId,
    pub is_primary_hint: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSet {
    pub id: WorkspaceSetId,
    pub monitor_id: MonitorId,
    pub ordered_workspace_ids: Vec<WorkspaceId>,
    pub active_workspace_id: WorkspaceId,
    pub last_non_empty_workspace_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub monitor_id: MonitorId,
    pub vertical_index: usize,
    pub remembered_focused_window_id: Option<WindowId>,
    pub remembered_focused_column_id: Option<ColumnId>,
    pub strip: ScrollingStrip,
    pub floating_layer: FloatingLayer,
    pub name: Option<String>,
    pub is_ephemeral_empty_tail: bool,
}

impl Workspace {
    pub fn empty(
        id: WorkspaceId,
        monitor_id: MonitorId,
        vertical_index: usize,
        visible: Rect,
    ) -> Self {
        Self {
            id,
            monitor_id,
            vertical_index,
            remembered_focused_window_id: None,
            remembered_focused_column_id: None,
            strip: ScrollingStrip {
                ordered_column_ids: Vec::new(),
                scroll_offset: 0,
                visible_region: visible,
                layout_mode: StripLayoutMode::Columns,
            },
            floating_layer: FloatingLayer {
                workspace_id: id,
                ordered_window_ids: Vec::new(),
                z_hints: BTreeMap::new(),
            },
            name: None,
            is_ephemeral_empty_tail: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrollingStrip {
    pub ordered_column_ids: Vec<ColumnId>,
    pub scroll_offset: i32,
    pub visible_region: Rect,
    pub layout_mode: StripLayoutMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    pub id: ColumnId,
    pub mode: ColumnMode,
    pub ordered_window_ids: Vec<WindowId>,
    pub active_window_id: Option<WindowId>,
    pub width_semantics: WidthSemantics,
    pub maximized_state: MaximizedState,
    pub tab_selection: Option<WindowId>,
}

impl Column {
    pub fn new(
        id: ColumnId,
        mode: ColumnMode,
        width_semantics: WidthSemantics,
        ordered_window_ids: Vec<WindowId>,
    ) -> Self {
        let active_window_id = ordered_window_ids.first().copied();
        let tab_selection = ordered_window_ids.first().copied();

        Self {
            id,
            mode,
            ordered_window_ids,
            active_window_id,
            width_semantics,
            maximized_state: MaximizedState::Normal,
            tab_selection,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreTarget {
    pub workspace_id: WorkspaceId,
    pub column_id: Option<ColumnId>,
    pub column_index: Option<usize>,
    pub layer: WindowLayer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowNode {
    pub id: WindowId,
    pub current_hwnd_binding: Option<u64>,
    pub classification: WindowClassification,
    pub layer: WindowLayer,
    pub workspace_id: WorkspaceId,
    pub column_id: Option<ColumnId>,
    pub is_managed: bool,
    pub is_floating: bool,
    pub is_fullscreen: bool,
    pub restore_target: Option<RestoreTarget>,
    pub last_known_rect: Rect,
    pub desired_size: Size,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FloatingLayer {
    pub workspace_id: WorkspaceId,
    pub ordered_window_ids: Vec<WindowId>,
    pub z_hints: BTreeMap<WindowId, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusState {
    pub focused_monitor_id: Option<MonitorId>,
    pub active_workspace_by_monitor: BTreeMap<MonitorId, WorkspaceId>,
    pub focused_window_id: Option<WindowId>,
    pub focused_column_id: Option<ColumnId>,
    pub focus_origin: FocusOrigin,
}

impl Default for FocusState {
    fn default() -> Self {
        Self {
            focused_monitor_id: None,
            active_workspace_by_monitor: BTreeMap::new(),
            focused_window_id: None,
            focused_column_id: None,
            focus_origin: FocusOrigin::ReducerDefault,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverviewState {
    pub is_open: bool,
    pub monitor_id: Option<MonitorId>,
    pub selection: Option<WorkspaceId>,
    pub drag_payload: Option<String>,
    pub projection_version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturePolicy {
    pub window_policy_overrides: BTreeMap<WindowId, bool>,
    pub workspace_policy_overrides: BTreeMap<WorkspaceId, bool>,
    pub built_in_capture_exclusions: Vec<String>,
    pub best_effort_display_affinity_targets: Vec<WindowId>,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            window_policy_overrides: BTreeMap::new(),
            workspace_policy_overrides: BTreeMap::new(),
            built_in_capture_exclusions: vec![
                "task-switcher".to_string(),
                "lock-screen".to_string(),
            ],
            best_effort_display_affinity_targets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigProjection {
    pub config_version: u64,
    pub live_reload_enabled: bool,
    pub rollback_supported: bool,
    pub source_path: String,
    pub bind_control_mode: BindControlMode,
    pub strip_scroll_step: u32,
    pub default_column_mode: ColumnMode,
    pub default_column_width: WidthSemantics,
    pub layout_spacing: LayoutSpacing,
    pub managed_monitor_bindings: Vec<String>,
    pub active_rule_count: usize,
}

impl Default for ConfigProjection {
    fn default() -> Self {
        Self {
            config_version: 0,
            live_reload_enabled: true,
            rollback_supported: true,
            source_path: "config/flowtile.kdl".to_string(),
            bind_control_mode: BindControlMode::Coexistence,
            strip_scroll_step: 240,
            default_column_mode: ColumnMode::Normal,
            default_column_width: WidthSemantics::default(),
            layout_spacing: LayoutSpacing::default(),
            managed_monitor_bindings: Vec::new(),
            active_rule_count: 0,
        }
    }
}

impl ConfigProjection {
    pub fn manages_monitor_binding(&self, binding: Option<&str>) -> bool {
        if self.managed_monitor_bindings.is_empty() {
            return true;
        }

        let Some(binding) = binding.map(str::trim).filter(|binding| !binding.is_empty()) else {
            return false;
        };

        self.managed_monitor_bindings
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(binding))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgeInsets {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl EdgeInsets {
    pub const fn all(value: u32) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    pub const fn horizontal(self) -> u32 {
        self.left.saturating_add(self.right)
    }

    pub const fn vertical(self) -> u32 {
        self.top.saturating_add(self.bottom)
    }
}

impl Default for EdgeInsets {
    fn default() -> Self {
        Self::all(16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutSpacing {
    pub outer_padding: EdgeInsets,
    pub column_gap: u32,
    pub window_gap: u32,
    pub floating_margin: u32,
}

impl Default for LayoutSpacing {
    fn default() -> Self {
        Self {
            outer_padding: EdgeInsets::default(),
            column_gap: 12,
            window_gap: 12,
            floating_margin: 16,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsSummary {
    pub last_transition_label: Option<String>,
    pub total_records: u64,
    pub last_state_version: StateVersion,
    pub version_line: &'static str,
}

impl Default for DiagnosticsSummary {
    fn default() -> Self {
        Self {
            last_transition_label: None,
            total_records: 0,
            last_state_version: StateVersion::new(0),
            version_line: VERSION_LINE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct LayoutState {
    pub columns: BTreeMap<ColumnId, Column>,
    pub width_resize_session: Option<WidthResizeSession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WidthResizeSession {
    pub workspace_id: WorkspaceId,
    pub column_id: ColumnId,
    pub window_id: WindowId,
    pub anchor_edge: ResizeEdge,
    pub anchor_x: i32,
    pub current_pointer_x: i32,
    pub initial_column_rect: Rect,
    pub initial_width: u32,
    pub target_width: u32,
    pub clamped_preview_rect: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeState {
    pub state_version: StateVersion,
    pub boot_mode: RuntimeMode,
    pub last_reconcile_at: Option<u64>,
    pub last_full_scan_at: Option<u64>,
    pub degraded_flags: Vec<String>,
    next_monitor_id: u64,
    next_workspace_set_id: u64,
    next_workspace_id: u64,
    next_column_id: u64,
    next_window_id: u64,
}

impl RuntimeState {
    fn new(boot_mode: RuntimeMode) -> Self {
        Self {
            state_version: StateVersion::new(0),
            boot_mode,
            last_reconcile_at: None,
            last_full_scan_at: None,
            degraded_flags: Vec::new(),
            next_monitor_id: 1,
            next_workspace_set_id: 1,
            next_workspace_id: 1,
            next_column_id: 1,
            next_window_id: 1,
        }
    }

    fn allocate_monitor_id(&mut self) -> MonitorId {
        let id = MonitorId::new(self.next_monitor_id);
        self.next_monitor_id += 1;
        id
    }

    fn allocate_workspace_set_id(&mut self) -> WorkspaceSetId {
        let id = WorkspaceSetId::new(self.next_workspace_set_id);
        self.next_workspace_set_id += 1;
        id
    }

    fn allocate_workspace_id(&mut self) -> WorkspaceId {
        let id = WorkspaceId::new(self.next_workspace_id);
        self.next_workspace_id += 1;
        id
    }

    fn allocate_column_id(&mut self) -> ColumnId {
        let id = ColumnId::new(self.next_column_id);
        self.next_column_id += 1;
        id
    }

    fn allocate_window_id(&mut self) -> WindowId {
        let id = WindowId::new(self.next_window_id);
        self.next_window_id += 1;
        id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WmState {
    pub runtime: RuntimeState,
    pub monitors: BTreeMap<MonitorId, Monitor>,
    pub workspace_sets: BTreeMap<WorkspaceSetId, WorkspaceSet>,
    pub workspaces: BTreeMap<WorkspaceId, Workspace>,
    pub windows: BTreeMap<WindowId, WindowNode>,
    pub focus: FocusState,
    pub layout: LayoutState,
    pub overview: OverviewState,
    pub capture_policy: CapturePolicy,
    pub config_projection: ConfigProjection,
    pub diagnostics_summary: DiagnosticsSummary,
}

impl WmState {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            runtime: RuntimeState::new(runtime_mode),
            monitors: BTreeMap::new(),
            workspace_sets: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            windows: BTreeMap::new(),
            focus: FocusState::default(),
            layout: LayoutState::default(),
            overview: OverviewState::default(),
            capture_policy: CapturePolicy::default(),
            config_projection: ConfigProjection::default(),
            diagnostics_summary: DiagnosticsSummary::default(),
        }
    }

    pub fn bootstrap_profile(&self) -> BootstrapProfile {
        BootstrapProfile::from_state(self.runtime.boot_mode, self.runtime.state_version)
    }

    pub const fn state_version(&self) -> StateVersion {
        self.runtime.state_version
    }

    pub fn bump_state_version(&mut self) -> StateVersion {
        self.runtime.state_version = self.runtime.state_version.next();
        self.runtime.state_version
    }

    pub fn allocate_window_id(&mut self) -> WindowId {
        self.runtime.allocate_window_id()
    }

    pub fn allocate_column_id(&mut self) -> ColumnId {
        self.runtime.allocate_column_id()
    }

    pub fn add_monitor(
        &mut self,
        work_area_rect: Rect,
        dpi: u32,
        is_primary_hint: bool,
    ) -> MonitorId {
        let monitor_id = self.runtime.allocate_monitor_id();
        let workspace_set_id = self.runtime.allocate_workspace_set_id();
        let workspace_id = self.runtime.allocate_workspace_id();
        let topology_role = if is_primary_hint {
            TopologyRole::Primary
        } else {
            TopologyRole::Secondary
        };

        self.monitors.insert(
            monitor_id,
            Monitor {
                id: monitor_id,
                platform_binding: Some(format!("monitor-{}", monitor_id.get())),
                work_area_rect,
                dpi,
                topology_role,
                workspace_set_id,
                is_primary_hint,
            },
        );

        self.workspaces.insert(
            workspace_id,
            Workspace::empty(workspace_id, monitor_id, 0, work_area_rect),
        );

        self.workspace_sets.insert(
            workspace_set_id,
            WorkspaceSet {
                id: workspace_set_id,
                monitor_id,
                ordered_workspace_ids: vec![workspace_id],
                active_workspace_id: workspace_id,
                last_non_empty_workspace_id: None,
            },
        );

        self.focus
            .active_workspace_by_monitor
            .insert(monitor_id, workspace_id);

        if self.focus.focused_monitor_id.is_none() {
            self.focus.focused_monitor_id = Some(monitor_id);
        }

        self.normalize_workspace_set(workspace_set_id);
        monitor_id
    }

    pub fn workspace_set_id_for_monitor(&self, monitor_id: MonitorId) -> Option<WorkspaceSetId> {
        self.monitors
            .get(&monitor_id)
            .map(|monitor| monitor.workspace_set_id)
    }

    pub fn active_workspace_id_for_monitor(&self, monitor_id: MonitorId) -> Option<WorkspaceId> {
        self.focus
            .active_workspace_by_monitor
            .get(&monitor_id)
            .copied()
            .or_else(|| {
                self.workspace_set_id_for_monitor(monitor_id)
                    .and_then(|workspace_set_id| {
                        self.workspace_sets
                            .get(&workspace_set_id)
                            .map(|workspace_set| workspace_set.active_workspace_id)
                    })
            })
    }

    pub fn is_workspace_empty(&self, workspace_id: WorkspaceId) -> bool {
        let Some(workspace) = self.workspaces.get(&workspace_id) else {
            return true;
        };

        if !workspace.floating_layer.ordered_window_ids.is_empty() {
            return false;
        }

        workspace.strip.ordered_column_ids.iter().all(|column_id| {
            self.layout
                .columns
                .get(column_id)
                .map(|column| column.ordered_window_ids.is_empty())
                .unwrap_or(true)
        })
    }

    pub fn ensure_tail_workspace(&mut self, monitor_id: MonitorId) -> Option<WorkspaceId> {
        let workspace_set_id = self.workspace_set_id_for_monitor(monitor_id)?;
        self.normalize_workspace_set(workspace_set_id);
        self.workspace_sets
            .get(&workspace_set_id)
            .and_then(|workspace_set| workspace_set.ordered_workspace_ids.last().copied())
    }

    pub fn monitor_ids_in_navigation_order(&self) -> Vec<MonitorId> {
        let mut monitor_ids = self.monitors.keys().copied().collect::<Vec<_>>();
        monitor_ids.sort_by_key(|monitor_id| {
            self.monitors
                .get(monitor_id)
                .map_or((i32::MAX, i32::MAX, u64::MAX), |monitor| {
                    (
                        monitor.work_area_rect.x,
                        monitor.work_area_rect.y,
                        monitor_id.get(),
                    )
                })
        });
        monitor_ids
    }

    pub fn normalize_workspace_set(&mut self, workspace_set_id: WorkspaceSetId) {
        let Some(monitor_id) = self
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.monitor_id)
        else {
            return;
        };
        let monitor_visible_region = self
            .monitors
            .get(&monitor_id)
            .map(|monitor| monitor.work_area_rect)
            .unwrap_or_default();

        let mut ordered_workspace_ids = self
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.ordered_workspace_ids.clone())
            .unwrap_or_default();
        let active_workspace_id = self
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.active_workspace_id);

        if ordered_workspace_ids.is_empty() {
            let workspace_id = self.runtime.allocate_workspace_id();
            self.workspaces.insert(
                workspace_id,
                Workspace::empty(workspace_id, monitor_id, 0, monitor_visible_region),
            );
            ordered_workspace_ids.push(workspace_id);
        }

        let tail_workspace_id_before_collapse = ordered_workspace_ids.last().copied();
        ordered_workspace_ids.retain(|workspace_id| {
            let should_remove = self.is_workspace_empty(*workspace_id)
                && Some(*workspace_id) != active_workspace_id
                && Some(*workspace_id) != tail_workspace_id_before_collapse
                && self
                    .workspaces
                    .get(workspace_id)
                    .is_some_and(|workspace| workspace.name.is_none());
            if should_remove {
                self.workspaces.remove(workspace_id);
            }
            !should_remove
        });

        while ordered_workspace_ids.len() > 1 {
            let last_index = ordered_workspace_ids.len() - 1;
            let last_workspace_id = ordered_workspace_ids[last_index];
            let previous_workspace_id = ordered_workspace_ids[last_index - 1];

            if self.is_workspace_empty(last_workspace_id)
                && self.is_workspace_empty(previous_workspace_id)
            {
                self.workspaces.remove(&last_workspace_id);
                ordered_workspace_ids.pop();
            } else {
                break;
            }
        }

        if let Some(last_workspace_id) = ordered_workspace_ids.last().copied()
            && !self.is_workspace_empty(last_workspace_id)
        {
            let workspace_id = self.runtime.allocate_workspace_id();
            self.workspaces.insert(
                workspace_id,
                Workspace::empty(
                    workspace_id,
                    monitor_id,
                    ordered_workspace_ids.len(),
                    monitor_visible_region,
                ),
            );
            ordered_workspace_ids.push(workspace_id);
        }

        let last_non_empty_workspace_id = ordered_workspace_ids
            .iter()
            .copied()
            .rev()
            .find(|workspace_id| !self.is_workspace_empty(*workspace_id));

        let last_workspace_id = ordered_workspace_ids.last().copied();
        for (index, workspace_id) in ordered_workspace_ids.iter().copied().enumerate() {
            let is_empty = self.is_workspace_empty(workspace_id);
            if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
                workspace.monitor_id = monitor_id;
                workspace.vertical_index = index;
                workspace.strip.visible_region = monitor_visible_region;
                workspace.floating_layer.workspace_id = workspace_id;
                workspace.is_ephemeral_empty_tail =
                    Some(workspace_id) == last_workspace_id && is_empty;
            }
        }

        let active_workspace_id = self
            .workspace_sets
            .get(&workspace_set_id)
            .map(|workspace_set| workspace_set.active_workspace_id)
            .filter(|workspace_id| ordered_workspace_ids.contains(workspace_id))
            .unwrap_or_else(|| ordered_workspace_ids[0]);

        if let Some(workspace_set) = self.workspace_sets.get_mut(&workspace_set_id) {
            workspace_set.ordered_workspace_ids = ordered_workspace_ids;
            workspace_set.active_workspace_id = active_workspace_id;
            workspace_set.last_non_empty_workspace_id = last_non_empty_workspace_id;
        }

        self.focus
            .active_workspace_by_monitor
            .insert(monitor_id, active_workspace_id);
    }
}
