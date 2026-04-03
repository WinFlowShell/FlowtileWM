#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use flowtile_diagnostics::{AtomicPerfMetric, PerfTelemetrySnapshot};
use flowtile_domain::Rect;
use serde::{Deserialize, Serialize};

#[cfg(not(windows))]
compile_error!("flowtile-windows-adapter currently supports only Windows builds.");

#[cfg(windows)]
mod dpi;
#[cfg(windows)]
mod native_apply;
#[cfg(windows)]
mod native_observer;
#[cfg(windows)]
mod native_snapshot;

pub const PRIMARY_DISCOVERY_API: &str = "SetWinEventHook";
pub const FALLBACK_DISCOVERY_PATH: &str = "full-window-scan";
pub const TILED_VISUAL_OVERLAP_X_PX: i32 = 0;
pub const WINDOW_SWITCH_ANIMATION_DURATION_MS: u16 = 90;
pub const WINDOW_SWITCH_ANIMATION_FRAME_COUNT: u8 = 6;
#[cfg(windows)]
const NATIVE_OBSERVER_COMPONENT: &str = "native-observer";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsAdapterBootstrap {
    pub discovery_api: &'static str,
    pub fallback_path: &'static str,
    pub batches_geometry_operations: bool,
    pub owns_product_policy: bool,
}

pub const fn bootstrap() -> WindowsAdapterBootstrap {
    WindowsAdapterBootstrap {
        discovery_api: PRIMARY_DISCOVERY_API,
        fallback_path: FALLBACK_DISCOVERY_PATH,
        batches_geometry_operations: true,
        owns_product_policy: false,
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlatformSnapshot {
    #[serde(default)]
    pub foreground_hwnd: Option<u64>,
    pub monitors: Vec<PlatformMonitorSnapshot>,
    pub windows: Vec<PlatformWindowSnapshot>,
}

impl PlatformSnapshot {
    pub fn sort_for_stability(&mut self) {
        self.monitors.sort_by(|left, right| {
            right
                .is_primary
                .cmp(&left.is_primary)
                .then_with(|| left.binding.cmp(&right.binding))
        });
        self.windows.sort_by(|left, right| {
            right
                .is_focused
                .cmp(&left.is_focused)
                .then_with(|| left.monitor_binding.cmp(&right.monitor_binding))
                .then_with(|| left.rect.x.cmp(&right.rect.x))
                .then_with(|| left.rect.y.cmp(&right.rect.y))
                .then_with(|| left.hwnd.cmp(&right.hwnd))
        });
    }

    pub fn focused_window(&self) -> Option<&PlatformWindowSnapshot> {
        self.windows.iter().find(|window| window.is_focused)
    }

    pub fn actual_foreground_hwnd(&self) -> Option<u64> {
        self.foreground_hwnd
            .or_else(|| self.focused_window().map(|window| window.hwnd))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlatformMonitorSnapshot {
    pub binding: String,
    pub work_area_rect: Rect,
    pub dpi: u32,
    pub is_primary: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlatformWindowSnapshot {
    pub hwnd: u64,
    pub title: String,
    pub class_name: String,
    pub process_id: u32,
    #[serde(default)]
    pub process_name: Option<String>,
    pub rect: Rect,
    pub monitor_binding: String,
    pub is_visible: bool,
    pub is_focused: bool,
    #[serde(default = "default_management_candidate")]
    pub management_candidate: bool,
}

const fn default_management_candidate() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformWindowRole {
    Noise,
    Transient,
    Auxiliary,
    #[default]
    Primary,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformWindowDisposition {
    RejectedNoise,
    TransientEscapeSurface,
    AuxiliaryAppSurface,
    #[default]
    PendingPrimaryCandidate,
    PromotablePrimaryCandidate,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformPresentationPreflight {
    #[default]
    Unknown,
    Eligible,
    FallbackOnly,
    Rejected,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlatformDiscoveryAssessment {
    #[serde(default)]
    pub role: PlatformWindowRole,
    #[serde(default)]
    pub role_reason: String,
    #[serde(default)]
    pub disposition: PlatformWindowDisposition,
    #[serde(default)]
    pub disposition_reason: String,
    #[serde(default)]
    pub family_key: String,
    #[serde(default)]
    pub presentation_preflight: PlatformPresentationPreflight,
    #[serde(default)]
    pub presentation_reason: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationKind {
    #[default]
    Snapshot,
    Warning,
    Suspend,
    Resume,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservationEnvelope {
    pub kind: ObservationKind,
    pub reason: String,
    #[serde(default)]
    pub snapshot: Option<PlatformSnapshot>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDiff {
    pub created_windows: Vec<PlatformWindowSnapshot>,
    pub destroyed_hwnds: Vec<u64>,
    pub focused_hwnd: Option<u64>,
    pub monitor_topology_changed: bool,
}

impl SnapshotDiff {
    pub fn initial(snapshot: &PlatformSnapshot) -> Self {
        Self {
            created_windows: snapshot.windows.clone(),
            destroyed_hwnds: Vec::new(),
            focused_hwnd: snapshot.actual_foreground_hwnd(),
            monitor_topology_changed: !snapshot.monitors.is_empty(),
        }
    }
}

pub fn diff_snapshots(previous: &PlatformSnapshot, current: &PlatformSnapshot) -> SnapshotDiff {
    let previous_windows = previous
        .windows
        .iter()
        .map(|window| (window.hwnd, window))
        .collect::<HashMap<_, _>>();
    let current_windows = current
        .windows
        .iter()
        .map(|window| (window.hwnd, window))
        .collect::<HashMap<_, _>>();

    let created_windows = current
        .windows
        .iter()
        .filter(|window| !previous_windows.contains_key(&window.hwnd))
        .cloned()
        .collect::<Vec<_>>();
    let destroyed_hwnds = previous
        .windows
        .iter()
        .filter(|window| !current_windows.contains_key(&window.hwnd))
        .map(|window| window.hwnd)
        .collect::<Vec<_>>();
    let focused_hwnd = match (
        previous.actual_foreground_hwnd(),
        current.actual_foreground_hwnd(),
    ) {
        (previous_hwnd, current_hwnd) if previous_hwnd != current_hwnd => current_hwnd,
        _ => None,
    };

    SnapshotDiff {
        created_windows,
        destroyed_hwnds,
        focused_hwnd,
        monitor_topology_changed: previous.monitors != current.monitors,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowSwitchAnimation {
    pub from_rect: Rect,
    pub duration_ms: u16,
    pub frame_count: u8,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowOpacityMode {
    #[default]
    DirectLayered,
    BrowserSurrogate,
    OverlayDim,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowVisualEmphasis {
    #[serde(default)]
    pub opacity_alpha: Option<u8>,
    #[serde(default)]
    pub opacity_mode: WindowOpacityMode,
    #[serde(default)]
    pub force_clear_layered_style: bool,
    #[serde(default)]
    pub disable_visual_effects: bool,
    #[serde(default)]
    pub border_color_rgb: Option<u32>,
    pub border_thickness_px: u8,
    #[serde(default)]
    pub rounded_corners: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowPresentationMode {
    #[default]
    NativeVisible,
    NativeHidden,
    SurrogateVisible,
    SurrogateClipped,
}

impl WindowPresentationMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeVisible => "native-visible",
            Self::NativeHidden => "native-hidden",
            Self::SurrogateVisible => "surrogate-visible",
            Self::SurrogateClipped => "surrogate-clipped",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowSurrogateClip {
    pub destination_rect: Rect,
    pub source_rect: Rect,
    pub native_visible_rect: Rect,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowMonitorSceneSliceKind {
    #[default]
    ForeignMonitorSurrogate,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowMonitorSceneSlice {
    #[serde(default)]
    pub kind: WindowMonitorSceneSliceKind,
    pub monitor_rect: Rect,
    pub destination_rect: Rect,
    pub source_rect: Rect,
    pub native_visible_rect: Rect,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowMonitorScene {
    #[serde(default)]
    pub home_visible_rect: Option<Rect>,
    #[serde(default)]
    pub slices: Vec<WindowMonitorSceneSlice>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowPresentation {
    #[serde(default)]
    pub mode: WindowPresentationMode,
    #[serde(default)]
    pub surrogate: Option<WindowSurrogateClip>,
    #[serde(default)]
    pub monitor_scene: WindowMonitorScene,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowPresentationOverride {
    pub mode: WindowPresentationMode,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SurrogatePresentationDiagnostics {
    pub active_hosts: usize,
    pub show_requests: u64,
    pub hide_requests: u64,
    pub foreign_scene_active_hosts: usize,
    pub foreign_scene_show_requests: u64,
    pub foreign_scene_hide_requests: u64,
    pub foreign_scene_pruned_hosts: u64,
    pub classifier_rejections: u64,
    pub native_fallbacks: u64,
    pub transient_escapes: u64,
    pub handoff_promotions: u64,
    pub pointer_replay_attempts: u64,
    pub pointer_replay_successes: u64,
    pub pointer_replay_failures: u64,
    pub dwm_thumbnail_backend_uses: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApplyOperation {
    pub hwnd: u64,
    pub rect: Rect,
    #[serde(default = "default_true")]
    pub apply_geometry: bool,
    #[serde(default)]
    pub activate: bool,
    #[serde(default)]
    pub suppress_visual_gap: bool,
    #[serde(default)]
    pub window_switch_animation: Option<WindowSwitchAnimation>,
    #[serde(default)]
    pub visual_emphasis: Option<WindowVisualEmphasis>,
    #[serde(default)]
    pub presentation: WindowPresentation,
}

#[allow(dead_code)]
const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct ApplyBatchResult {
    pub attempted: usize,
    pub applied: usize,
    pub failures: Vec<ApplyFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ApplyFailure {
    pub hwnd: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveObservationOptions {
    pub fallback_scan_interval_ms: u64,
    pub debounce_ms: u64,
}

impl Default for LiveObservationOptions {
    fn default() -> Self {
        Self {
            fallback_scan_interval_ms: 2_000,
            debounce_ms: 150,
        }
    }
}

#[derive(Debug)]
pub enum ObservationStreamError {
    Adapter(WindowsAdapterError),
    ChannelClosed,
    Timeout,
}

impl fmt::Display for ObservationStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(source) => source.fmt(formatter),
            Self::ChannelClosed => formatter.write_str("observation stream channel closed"),
            Self::Timeout => formatter.write_str("timed out waiting for observation event"),
        }
    }
}

impl std::error::Error for ObservationStreamError {}

impl From<WindowsAdapterError> for ObservationStreamError {
    fn from(value: WindowsAdapterError) -> Self {
        Self::Adapter(value)
    }
}

impl From<std::io::Error> for ObservationStreamError {
    fn from(value: std::io::Error) -> Self {
        Self::Adapter(WindowsAdapterError::Io(value))
    }
}

pub(crate) enum ObserverMessage {
    Envelope(ObservationEnvelope),
}

enum ObservationBackend {
    #[cfg(windows)]
    Native(native_observer::NativeObservationRuntime),
}

pub struct ObservationStream {
    backend: ObservationBackend,
    receiver: Receiver<ObserverMessage>,
}

impl ObservationStream {
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ObservationEnvelope, ObservationStreamError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(ObserverMessage::Envelope(envelope)) => Ok(envelope),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(error) = self.try_backend_exit_error()? {
                    return Err(ObservationStreamError::Adapter(error));
                }

                Err(ObservationStreamError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(error) = self.try_backend_exit_error()? {
                    return Err(ObservationStreamError::Adapter(error));
                }

                Err(ObservationStreamError::ChannelClosed)
            }
        }
    }

    fn try_backend_exit_error(
        &mut self,
    ) -> Result<Option<WindowsAdapterError>, WindowsAdapterError> {
        match &mut self.backend {
            #[cfg(windows)]
            ObservationBackend::Native(runtime) => {
                if runtime.is_finished() {
                    Ok(Some(WindowsAdapterError::RuntimeFailed {
                        component: NATIVE_OBSERVER_COMPONENT,
                        message: "observer thread exited".to_string(),
                    }))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

impl Drop for ObservationStream {
    fn drop(&mut self) {
        match &mut self.backend {
            #[cfg(windows)]
            ObservationBackend::Native(runtime) => runtime.shutdown(),
        }
    }
}

#[derive(Debug)]
pub enum WindowsAdapterError {
    Io(std::io::Error),
    RuntimeFailed {
        component: &'static str,
        message: String,
    },
}

impl fmt::Display for WindowsAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::RuntimeFailed { component, message } => {
                write!(formatter, "{component} failed: {message}")
            }
        }
    }
}

impl std::error::Error for WindowsAdapterError {}

impl From<std::io::Error> for WindowsAdapterError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug)]
pub struct WindowsAdapter {
    perf: Arc<AdapterPerfTelemetry>,
}

#[derive(Debug, Default)]
pub(crate) struct AdapterPerfTelemetry {
    scan_snapshot: AtomicPerfMetric,
    apply_operations: AtomicPerfMetric,
    observer_incremental_event: AtomicPerfMetric,
    observer_rescan_snapshot: AtomicPerfMetric,
}

impl AdapterPerfTelemetry {
    fn snapshot(&self) -> PerfTelemetrySnapshot {
        PerfTelemetrySnapshot {
            metrics: vec![
                self.scan_snapshot.snapshot("adapter.scan-snapshot"),
                self.apply_operations.snapshot("adapter.apply-operations"),
                self.observer_incremental_event
                    .snapshot("adapter.observer.incremental-event"),
                self.observer_rescan_snapshot
                    .snapshot("adapter.observer.full-rescan"),
            ],
        }
    }
}

impl WindowsAdapter {
    pub fn new() -> Self {
        Self {
            perf: Arc::new(AdapterPerfTelemetry::default()),
        }
    }

    pub fn spawn_observer(
        &self,
        options: LiveObservationOptions,
    ) -> Result<ObservationStream, WindowsAdapterError> {
        let (sender, receiver) = mpsc::channel::<ObserverMessage>();
        let runtime = native_observer::spawn(options, sender, Arc::clone(&self.perf))?;
        Ok(ObservationStream {
            backend: ObservationBackend::Native(runtime),
            receiver,
        })
    }

    pub fn perf_snapshot(&self) -> PerfTelemetrySnapshot {
        self.perf.snapshot()
    }

    pub fn surrogate_presentation_diagnostics(&self) -> SurrogatePresentationDiagnostics {
        native_apply::surrogate_presentation_diagnostics_snapshot()
    }

    pub fn surrogate_presentation_overrides(&self) -> HashMap<u64, WindowPresentationOverride> {
        native_apply::surrogate_presentation_overrides_snapshot()
    }

    pub fn materialized_presentation_hwnds(&self) -> BTreeSet<u64> {
        native_apply::materialized_presentation_hwnds_snapshot()
    }

    pub fn assess_discovery_windows(
        &self,
        snapshot: &PlatformSnapshot,
    ) -> HashMap<u64, PlatformDiscoveryAssessment> {
        assess_platform_snapshot_for_discovery(snapshot)
    }

    pub fn clear_window_presentations(&self, hwnds: &[u64]) -> Result<(), WindowsAdapterError> {
        native_apply::clear_window_presentations(hwnds).map_err(|message| {
            WindowsAdapterError::RuntimeFailed {
                component: "native-apply",
                message,
            }
        })
    }

    pub fn scan_snapshot(&self) -> Result<PlatformSnapshot, WindowsAdapterError> {
        let started_at = Instant::now();
        let result = native_snapshot::scan_snapshot();
        self.perf
            .scan_snapshot
            .record_duration(started_at.elapsed());
        if result.is_err() {
            self.perf.scan_snapshot.record_error();
        }
        result
    }

    pub fn apply_operations(
        &self,
        operations: &[ApplyOperation],
    ) -> Result<ApplyBatchResult, WindowsAdapterError> {
        if operations.is_empty() {
            self.perf.apply_operations.record_skip();
            return Ok(ApplyBatchResult::default());
        }

        let started_at = Instant::now();
        let result = Ok(native_apply::apply_operations(operations));
        self.perf
            .apply_operations
            .record_duration(started_at.elapsed());
        if result
            .as_ref()
            .is_ok_and(|batch| !batch.failures.is_empty())
        {
            self.perf.apply_operations.record_error();
        }
        result
    }
}

impl Default for WindowsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

pub fn needs_geometry_apply(actual: Rect, desired: Rect) -> bool {
    actual != desired
}

pub fn needs_tiled_gapless_geometry_apply(actual: Rect, desired: Rect) -> bool {
    let overlap = TILED_VISUAL_OVERLAP_X_PX.max(0);
    let desired_right = desired.x.saturating_add(desired.width as i32);
    let actual_right = actual.x.saturating_add(actual.width as i32);
    let desired_left_shift = if desired.x > 0 { overlap } else { 0 };
    let actual_left_shift = desired.x.saturating_sub(actual.x);
    let right_delta = actual_right.saturating_sub(desired_right);

    actual.y != desired.y
        || actual.height != desired.height
        || actual_left_shift != desired_left_shift
        || right_delta.abs() > overlap
}

pub fn needs_activation_apply(actual_focused_hwnd: Option<u64>, desired_focused_hwnd: u64) -> bool {
    actual_focused_hwnd != Some(desired_focused_hwnd)
}

pub fn missing_monitor_bindings(
    snapshot: &PlatformSnapshot,
    known_bindings: &[String],
) -> Vec<String> {
    let actual_bindings = snapshot
        .monitors
        .iter()
        .map(|monitor| monitor.binding.clone())
        .collect::<BTreeSet<_>>();

    known_bindings
        .iter()
        .filter(|binding| !actual_bindings.contains(binding.as_str()))
        .cloned()
        .collect()
}

const DISCOVERY_PENDING_TITLELESS_STABILITY_HINT: &str = "titleless-primary-candidate";
const DISCOVERY_PRIMARY_CANDIDATE_HINT: &str = "primary-candidate";
const DISCOVERY_AUXILIARY_HINT: &str = "family-auxiliary-surface";
const DISCOVERY_KNOWN_INTERNAL_AUXILIARY_HINT: &str = "known-internal-family-surface";
const DISCOVERY_DUPLICATE_TITLELESS_AUXILIARY_HINT: &str = "duplicate-titleless-family-surface";
const DISCOVERY_NON_PRIMARY_FOOTPRINT_AUXILIARY_HINT: &str = "titleless-non-primary-footprint";
const DISCOVERY_NOISE_HINT: &str = "platform-noise-filter";
const DISCOVERY_TRANSIENT_HINT: &str = "transient-escape-surface";
const DISCOVERY_PRIMARY_ROLE_HINT: &str = "primary-role";
const DISCOVERY_PRELIGHT_ELIGIBLE_HINT: &str = "eligible";
const DISCOVERY_PREFLIGHT_FALLBACK_HINT: &str = "fallback-only-classifier";
const DISCOVERY_PREFLIGHT_REJECTED_HINT: &str = "transient-escape-class";
const DISCOVERY_PREFLIGHT_INTERNAL_SURFACE_HINT: &str = "internal-family-surface";

const DISCOVERY_FAMILY_WINDOWS_TERMINAL: &str = "windowsterminal";
const DISCOVERY_FAMILY_EXPLORER: &str = "explorer";
const CLASS_CHROME_WIDGET: &str = "chrome_widgetwin_1";
const CLASS_MOZILLA_WINDOW: &str = "mozillawindowclass";
const CLASS_TERMINAL_HOSTING_WINDOW: &str = "cascadia_hosting_window_class";
const CLASS_TERMINAL_CONTENT_BRIDGE: &str = "windows.ui.composition.desktopwindowcontentbridge";
const CLASS_TERMINAL_XAML_HOST_ISLAND_WINDOW: &str = "xamlexplorerhostislandwindow";
const CLASS_EXPLORER_DETAILS_PANE_HOST: &str = "detailspanehwndhostclass";
const CLASS_EXPLORER_DESKTOP_CHILD_SITE_BRIDGE: &str =
    "microsoft.ui.content.desktopchildsitebridge";
const CLASS_EXPLORER_CTRL_NOTIFY_SINK: &str = "ctrlnotifysink";
const CLASS_XAML_WINDOWED_POPUP: &str = "xaml_windowedpopupclass";
const CLASS_WIN32_MENU: &str = "#32768";
const CLASS_WIN32_DIALOG: &str = "#32770";
const CLASS_TOOLTIPS: &str = "tooltips_class32";
const CLASS_MSCTF_IME_UI: &str = "msctfime ui";
const CLASS_IME: &str = "ime";
const DISCOVERY_AUXILIARY_EDGE_PROXIMITY_PX: i32 = 96;

pub fn assess_platform_snapshot_for_discovery(
    snapshot: &PlatformSnapshot,
) -> HashMap<u64, PlatformDiscoveryAssessment> {
    snapshot
        .windows
        .iter()
        .map(|window| {
            (
                window.hwnd,
                assess_platform_window_for_discovery(snapshot, window),
            )
        })
        .collect()
}

pub fn assess_platform_window_for_discovery(
    snapshot: &PlatformSnapshot,
    window: &PlatformWindowSnapshot,
) -> PlatformDiscoveryAssessment {
    let family_key = platform_window_family_key(window);
    let (role, role_reason) = classify_platform_window_role(snapshot, window);

    let (disposition, disposition_reason, presentation_preflight, presentation_reason) = match role
    {
        PlatformWindowRole::Noise => (
            PlatformWindowDisposition::RejectedNoise,
            DISCOVERY_NOISE_HINT.to_string(),
            PlatformPresentationPreflight::Rejected,
            DISCOVERY_NOISE_HINT.to_string(),
        ),
        PlatformWindowRole::Transient => (
            PlatformWindowDisposition::TransientEscapeSurface,
            DISCOVERY_TRANSIENT_HINT.to_string(),
            PlatformPresentationPreflight::Rejected,
            DISCOVERY_PREFLIGHT_REJECTED_HINT.to_string(),
        ),
        PlatformWindowRole::Auxiliary => {
            let (presentation_preflight, presentation_reason) =
                discovery_presentation_preflight(window);
            (
                PlatformWindowDisposition::AuxiliaryAppSurface,
                role_reason.clone(),
                presentation_preflight,
                presentation_reason,
            )
        }
        PlatformWindowRole::Primary => {
            let (presentation_preflight, presentation_reason) =
                discovery_presentation_preflight(window);
            let disposition = if should_hold_primary_candidate_in_pending(snapshot, window) {
                PlatformWindowDisposition::PendingPrimaryCandidate
            } else {
                PlatformWindowDisposition::PromotablePrimaryCandidate
            };
            let disposition_reason =
                if disposition == PlatformWindowDisposition::PendingPrimaryCandidate {
                    DISCOVERY_PENDING_TITLELESS_STABILITY_HINT
                } else {
                    DISCOVERY_PRIMARY_CANDIDATE_HINT
                };
            (
                disposition,
                disposition_reason.to_string(),
                presentation_preflight,
                presentation_reason,
            )
        }
    };

    PlatformDiscoveryAssessment {
        role,
        role_reason,
        disposition,
        disposition_reason,
        family_key,
        presentation_preflight,
        presentation_reason,
    }
}

fn classify_platform_window_role(
    snapshot: &PlatformSnapshot,
    window: &PlatformWindowSnapshot,
) -> (PlatformWindowRole, String) {
    if !window.management_candidate {
        return (PlatformWindowRole::Noise, DISCOVERY_NOISE_HINT.to_string());
    }

    if is_discovery_transient_escape_surface(window) {
        return (
            PlatformWindowRole::Transient,
            DISCOVERY_TRANSIENT_HINT.to_string(),
        );
    }

    if is_discovery_known_internal_family_surface(window) {
        return (
            PlatformWindowRole::Auxiliary,
            DISCOVERY_KNOWN_INTERNAL_AUXILIARY_HINT.to_string(),
        );
    }

    if is_discovery_duplicate_titleless_family_surface(snapshot, window) {
        return (
            PlatformWindowRole::Auxiliary,
            DISCOVERY_DUPLICATE_TITLELESS_AUXILIARY_HINT.to_string(),
        );
    }

    if is_discovery_auxiliary_app_surface(snapshot, window) {
        return (
            PlatformWindowRole::Auxiliary,
            DISCOVERY_AUXILIARY_HINT.to_string(),
        );
    }

    if is_discovery_titleless_non_primary_footprint(snapshot, window) {
        return (
            PlatformWindowRole::Auxiliary,
            DISCOVERY_NON_PRIMARY_FOOTPRINT_AUXILIARY_HINT.to_string(),
        );
    }

    (
        PlatformWindowRole::Primary,
        DISCOVERY_PRIMARY_ROLE_HINT.to_string(),
    )
}

fn should_hold_primary_candidate_in_pending(
    snapshot: &PlatformSnapshot,
    window: &PlatformWindowSnapshot,
) -> bool {
    if window.title.trim().is_empty() {
        return true;
    }

    let (presentation_preflight, _) = discovery_presentation_preflight(window);
    if presentation_preflight == PlatformPresentationPreflight::FallbackOnly
        && !window.is_focused
        && !window_has_primary_like_footprint(snapshot, window)
    {
        return true;
    }

    false
}

fn discovery_presentation_preflight(
    window: &PlatformWindowSnapshot,
) -> (PlatformPresentationPreflight, String) {
    if is_discovery_known_internal_family_surface(window) {
        return (
            PlatformPresentationPreflight::Rejected,
            DISCOVERY_PREFLIGHT_INTERNAL_SURFACE_HINT.to_string(),
        );
    }

    let class_name = window.class_name.trim().to_ascii_lowercase();

    if matches!(
        class_name.as_str(),
        CLASS_XAML_WINDOWED_POPUP
            | CLASS_WIN32_MENU
            | CLASS_WIN32_DIALOG
            | CLASS_TOOLTIPS
            | CLASS_MSCTF_IME_UI
            | CLASS_IME
    ) {
        return (
            PlatformPresentationPreflight::Rejected,
            DISCOVERY_PREFLIGHT_REJECTED_HINT.to_string(),
        );
    }

    if matches!(
        class_name.as_str(),
        CLASS_CHROME_WIDGET | CLASS_MOZILLA_WINDOW | CLASS_TERMINAL_HOSTING_WINDOW
    ) {
        return (
            PlatformPresentationPreflight::FallbackOnly,
            DISCOVERY_PREFLIGHT_FALLBACK_HINT.to_string(),
        );
    }

    (
        PlatformPresentationPreflight::Eligible,
        DISCOVERY_PRELIGHT_ELIGIBLE_HINT.to_string(),
    )
}

fn is_discovery_known_internal_family_surface(window: &PlatformWindowSnapshot) -> bool {
    let family_key = platform_window_family_key(window);
    let class_name = window.class_name.trim().to_ascii_lowercase();

    match family_key.as_str() {
        DISCOVERY_FAMILY_WINDOWS_TERMINAL => {
            matches!(
                class_name.as_str(),
                CLASS_TERMINAL_CONTENT_BRIDGE | CLASS_TERMINAL_XAML_HOST_ISLAND_WINDOW
            ) || discovery_title_looks_internal_hosting_surface(&window.title)
        }
        DISCOVERY_FAMILY_EXPLORER => matches!(
            class_name.as_str(),
            CLASS_EXPLORER_DETAILS_PANE_HOST
                | CLASS_EXPLORER_DESKTOP_CHILD_SITE_BRIDGE
                | CLASS_EXPLORER_CTRL_NOTIFY_SINK
        ),
        _ => false,
    }
}

fn is_discovery_transient_escape_surface(window: &PlatformWindowSnapshot) -> bool {
    matches!(
        window.class_name.trim().to_ascii_lowercase().as_str(),
        CLASS_XAML_WINDOWED_POPUP
            | CLASS_WIN32_MENU
            | CLASS_WIN32_DIALOG
            | CLASS_TOOLTIPS
            | CLASS_MSCTF_IME_UI
            | CLASS_IME
    )
}

fn is_discovery_auxiliary_app_surface(
    snapshot: &PlatformSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    if candidate.is_focused {
        return false;
    }

    let candidate_title_empty = candidate.title.trim().is_empty();
    let candidate_compact = candidate.rect.height <= 160 || candidate.rect.width <= 220;
    let candidate_family_key = platform_window_family_key(candidate);
    let candidate_hosting_title = discovery_title_looks_internal_hosting_surface(&candidate.title);
    let candidate_fallback_only = discovery_presentation_preflight(candidate).0
        == PlatformPresentationPreflight::FallbackOnly;

    snapshot.windows.iter().any(|container| {
        if container.hwnd == candidate.hwnd
            || container.monitor_binding != candidate.monitor_binding
            || platform_window_family_key(container) != candidate_family_key
            || container.title.trim().is_empty()
            || !window_has_primary_like_footprint(snapshot, container)
        {
            return false;
        }

        let same_family_scene = rect_contains(container.rect, candidate.rect)
            || rects_overlap(container.rect, candidate.rect)
            || rects_are_proximate(
                container.rect,
                candidate.rect,
                DISCOVERY_AUXILIARY_EDGE_PROXIMITY_PX,
            );
        if !same_family_scene {
            return false;
        }

        if candidate_title_empty || candidate_compact {
            return true;
        }

        rects_are_near_duplicates(container.rect, candidate.rect)
            && (candidate_hosting_title || candidate_fallback_only)
    })
}

fn discovery_title_looks_internal_hosting_surface(title: &str) -> bool {
    let normalized = title.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && (normalized.contains("xamlsource")
            || normalized.contains("desktopwindow")
            || normalized.ends_with("contentbridge"))
}

fn is_discovery_duplicate_titleless_family_surface(
    snapshot: &PlatformSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    if candidate.is_focused || !candidate.title.trim().is_empty() {
        return false;
    }

    let candidate_family_key = platform_window_family_key(candidate);
    snapshot.windows.iter().any(|other| {
        other.hwnd != candidate.hwnd
            && platform_window_family_key(other) == candidate_family_key
            && other.monitor_binding == candidate.monitor_binding
            && other.title.trim().is_empty()
            && rects_are_near_duplicates(other.rect, candidate.rect)
    })
}

fn is_discovery_titleless_non_primary_footprint(
    snapshot: &PlatformSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    !candidate.is_focused
        && candidate.title.trim().is_empty()
        && !window_has_primary_like_footprint(snapshot, candidate)
}

fn window_has_primary_like_footprint(
    snapshot: &PlatformSnapshot,
    candidate: &PlatformWindowSnapshot,
) -> bool {
    let Some(monitor) = snapshot
        .monitors
        .iter()
        .find(|monitor| monitor.binding == candidate.monitor_binding)
    else {
        return true;
    };

    let candidate_area = window_area(candidate.rect);
    let monitor_area = window_area(monitor.work_area_rect).max(1);
    let width_ratio = candidate.rect.width as f64 / monitor.work_area_rect.width.max(1) as f64;
    let height_ratio = candidate.rect.height as f64 / monitor.work_area_rect.height.max(1) as f64;
    let area_ratio = candidate_area as f64 / monitor_area as f64;

    area_ratio >= 0.22 || (width_ratio >= 0.30 && height_ratio >= 0.50) || height_ratio >= 0.85
}

fn platform_window_family_key(window: &PlatformWindowSnapshot) -> String {
    normalized_process_name(window.process_name.as_deref())
        .unwrap_or_else(|| format!("pid:{}", window.process_id))
}

fn normalized_process_name(process_name: Option<&str>) -> Option<String> {
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

fn rect_contains(outer: Rect, inner: Rect) -> bool {
    let outer_right = outer
        .x
        .saturating_add(outer.width.min(i32::MAX as u32) as i32);
    let outer_bottom = outer
        .y
        .saturating_add(outer.height.min(i32::MAX as u32) as i32);
    let inner_right = inner
        .x
        .saturating_add(inner.width.min(i32::MAX as u32) as i32);
    let inner_bottom = inner
        .y
        .saturating_add(inner.height.min(i32::MAX as u32) as i32);

    inner.x >= outer.x
        && inner.y >= outer.y
        && inner_right <= outer_right
        && inner_bottom <= outer_bottom
}

fn rects_overlap(left: Rect, right: Rect) -> bool {
    let left_right = left
        .x
        .saturating_add(left.width.min(i32::MAX as u32) as i32);
    let left_bottom = left
        .y
        .saturating_add(left.height.min(i32::MAX as u32) as i32);
    let right_right = right
        .x
        .saturating_add(right.width.min(i32::MAX as u32) as i32);
    let right_bottom = right
        .y
        .saturating_add(right.height.min(i32::MAX as u32) as i32);

    left.x < right_right && left_right > right.x && left.y < right_bottom && left_bottom > right.y
}

fn rects_are_proximate(left: Rect, right: Rect, threshold_px: i32) -> bool {
    let expanded_left = Rect::new(
        left.x.saturating_sub(threshold_px),
        left.y.saturating_sub(threshold_px),
        left.width
            .saturating_add((threshold_px.max(0) as u32).saturating_mul(2)),
        left.height
            .saturating_add((threshold_px.max(0) as u32).saturating_mul(2)),
    );

    rects_overlap(expanded_left, right)
}

fn rects_are_near_duplicates(left: Rect, right: Rect) -> bool {
    let x_delta = left.x.saturating_sub(right.x).abs();
    let y_delta = left.y.saturating_sub(right.y).abs();
    let width_delta = i64::from(left.width)
        .saturating_sub(i64::from(right.width))
        .abs();
    let height_delta = i64::from(left.height)
        .saturating_sub(i64::from(right.height))
        .abs();

    x_delta <= 24 && y_delta <= 24 && width_delta <= 24 && height_delta <= 24
}

fn window_area(rect: Rect) -> u64 {
    u64::from(rect.width).saturating_mul(u64::from(rect.height))
}

#[cfg(test)]
mod tests {
    use flowtile_domain::Rect;

    use super::{
        ObservationEnvelope, ObservationKind, PRIMARY_DISCOVERY_API, PlatformDiscoveryAssessment,
        PlatformMonitorSnapshot, PlatformPresentationPreflight, PlatformSnapshot,
        PlatformWindowDisposition, PlatformWindowRole, PlatformWindowSnapshot, SnapshotDiff,
        WindowsAdapter, assess_platform_snapshot_for_discovery, bootstrap, diff_snapshots,
        missing_monitor_bindings, needs_activation_apply, needs_geometry_apply,
        needs_tiled_gapless_geometry_apply,
    };

    #[test]
    fn keeps_adapter_non_authoritative() {
        let bootstrap = bootstrap();
        assert_eq!(bootstrap.discovery_api, PRIMARY_DISCOVERY_API);
        assert!(bootstrap.batches_geometry_operations);
        assert!(!bootstrap.owns_product_policy);
    }

    #[test]
    fn initial_diff_reports_all_windows_as_discovered() {
        let snapshot = sample_snapshot();
        let diff = SnapshotDiff::initial(&snapshot);

        assert_eq!(diff.created_windows.len(), 2);
        assert!(diff.destroyed_hwnds.is_empty());
        assert_eq!(diff.focused_hwnd, Some(20));
    }

    #[test]
    fn detects_created_destroyed_and_focus_change() {
        let previous = sample_snapshot();
        let mut current = sample_snapshot();
        current.windows.remove(0);
        current.windows.push(PlatformWindowSnapshot {
            hwnd: 30,
            title: "Third".to_string(),
            class_name: "AppWindow".to_string(),
            process_id: 3,
            process_name: Some("third-app".to_string()),
            rect: Rect::new(700, 0, 400, 600),
            monitor_binding: "\\\\.\\DISPLAY1".to_string(),
            is_visible: true,
            is_focused: false,
            management_candidate: true,
        });
        current.windows[0].is_focused = false;

        let diff = diff_snapshots(&previous, &current);
        assert_eq!(diff.destroyed_hwnds, vec![10]);
        assert_eq!(diff.created_windows.len(), 1);
        assert_eq!(diff.created_windows[0].hwnd, 30);
        assert_eq!(diff.focused_hwnd, None);
    }

    #[test]
    fn diff_tracks_explicit_foreground_even_when_window_is_filtered_out() {
        let previous = sample_snapshot();
        let current = PlatformSnapshot {
            foreground_hwnd: Some(900),
            monitors: previous.monitors.clone(),
            windows: previous
                .windows
                .iter()
                .cloned()
                .map(|mut window| {
                    window.is_focused = false;
                    window
                })
                .collect(),
        };

        let diff = diff_snapshots(&previous, &current);
        assert_eq!(diff.focused_hwnd, Some(900));
    }

    #[test]
    fn detects_missing_monitor_bindings() {
        let snapshot = sample_snapshot();
        let missing = missing_monitor_bindings(
            &snapshot,
            &[
                String::from("\\\\.\\DISPLAY1"),
                String::from("\\\\.\\DISPLAY2"),
            ],
        );

        assert_eq!(missing, vec![String::from("\\\\.\\DISPLAY2")]);
    }

    #[test]
    fn geometry_apply_only_for_changed_rects() {
        assert!(!needs_geometry_apply(
            Rect::new(0, 0, 400, 300),
            Rect::new(0, 0, 400, 300)
        ));
        assert!(needs_geometry_apply(
            Rect::new(0, 0, 400, 300),
            Rect::new(10, 0, 400, 300)
        ));
    }

    #[test]
    fn tiled_overlap_tolerance_accepts_gapless_compensation() {
        assert!(!needs_tiled_gapless_geometry_apply(
            Rect::new(100, 0, 400, 600),
            Rect::new(100, 0, 400, 600),
        ));
    }

    #[test]
    fn tiled_overlap_tolerance_accepts_right_side_slack_after_shift() {
        assert!(!needs_tiled_gapless_geometry_apply(
            Rect::new(100, 0, 400, 600),
            Rect::new(100, 0, 400, 600),
        ));
    }

    #[test]
    fn tiled_overlap_tolerance_rejects_missing_left_shift() {
        assert!(needs_tiled_gapless_geometry_apply(
            Rect::new(101, 0, 400, 600),
            Rect::new(100, 0, 400, 600),
        ));
    }

    #[test]
    fn activation_apply_only_for_mismatched_foreground() {
        assert!(!needs_activation_apply(Some(20), 20));
        assert!(needs_activation_apply(Some(10), 20));
        assert!(needs_activation_apply(None, 20));
    }

    #[test]
    fn exposes_perf_snapshot_for_hot_paths() {
        let adapter = WindowsAdapter::new();
        adapter.perf.scan_snapshot.record_skip();

        let perf = adapter.perf_snapshot();
        assert!(
            perf.metrics
                .iter()
                .any(|metric| metric.metric == "adapter.scan-snapshot")
        );
        assert!(
            perf.metrics
                .iter()
                .any(|metric| metric.metric == "adapter.apply-operations")
        );
    }

    #[test]
    fn parses_snapshot_observation_envelope() {
        let envelope = serde_json::from_str::<ObservationEnvelope>(
            r#"{
                "kind":"snapshot",
                "reason":"initial-full-scan",
                "snapshot":{
                    "monitors":[{"binding":"\\\\.\\DISPLAY1","work_area_rect":{"x":0,"y":0,"width":1920,"height":1080},"dpi":96,"is_primary":true}],
                    "windows":[]
                }
            }"#,
        )
        .expect("observation envelope should parse");

        assert_eq!(envelope.kind, ObservationKind::Snapshot);
        assert_eq!(envelope.reason, "initial-full-scan");
        assert_eq!(
            envelope
                .snapshot
                .expect("snapshot should exist")
                .monitors
                .len(),
            1
        );
    }

    #[test]
    fn discovery_assessment_marks_empty_same_family_surface_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "Без имени — Блокнот".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 11,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(0, 0, 900, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 11,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(24, 24, 860, 820),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments.get(&20),
            Some(&PlatformDiscoveryAssessment {
                role: PlatformWindowRole::Auxiliary,
                role_reason: "family-auxiliary-surface".to_string(),
                disposition: PlatformWindowDisposition::AuxiliaryAppSurface,
                disposition_reason: "family-auxiliary-surface".to_string(),
                family_key: "notepad".to_string(),
                presentation_preflight: PlatformPresentationPreflight::Eligible,
                presentation_reason: "eligible".to_string(),
            })
        );
    }

    #[test]
    fn discovery_assessment_keeps_titled_secondary_window_promotable() {
        let snapshot = sample_snapshot();
        let assessments = assess_platform_snapshot_for_discovery(&snapshot);

        assert_eq!(
            assessments
                .get(&10)
                .map(|assessment| (assessment.role, assessment.disposition)),
            Some((
                PlatformWindowRole::Primary,
                PlatformWindowDisposition::PromotablePrimaryCandidate,
            ))
        );
        assert_eq!(
            assessments
                .get(&20)
                .map(|assessment| (assessment.role, assessment.disposition)),
            Some((
                PlatformWindowRole::Primary,
                PlatformWindowDisposition::PromotablePrimaryCandidate,
            ))
        );
    }

    #[test]
    fn discovery_assessment_marks_titled_overlapping_hosting_surface_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "FlowShell".to_string(),
                    class_name: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                    process_id: 11,
                    process_name: Some("WindowsTerminal".to_string()),
                    rect: Rect::new(0, 0, 1200, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "DesktopWindowXamlSource".to_string(),
                    class_name: "Windows.UI.Composition.DesktopWindowContentBridge".to_string(),
                    process_id: 11,
                    process_name: Some("WindowsTerminal".to_string()),
                    rect: Rect::new(117, 51, 1481, 865),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments.get(&20).map(|assessment| (
                assessment.role,
                assessment.disposition,
                assessment.role_reason.as_str(),
                assessment.presentation_preflight,
            )),
            Some((
                PlatformWindowRole::Auxiliary,
                PlatformWindowDisposition::AuxiliaryAppSurface,
                "known-internal-family-surface",
                PlatformPresentationPreflight::Rejected,
            ))
        );
    }

    #[test]
    fn discovery_assessment_keeps_same_family_primary_windows_promotable() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "notes-a.txt - Notepad".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 11,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(0, 0, 900, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "notes-b.txt - Notepad".to_string(),
                    class_name: "Notepad".to_string(),
                    process_id: 11,
                    process_name: Some("notepad".to_string()),
                    rect: Rect::new(930, 0, 900, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments
                .get(&20)
                .map(|assessment| (assessment.role, assessment.disposition)),
            Some((
                PlatformWindowRole::Primary,
                PlatformWindowDisposition::PromotablePrimaryCandidate,
            ))
        );
    }

    #[test]
    fn discovery_assessment_marks_explorer_details_host_surface_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "Explorer".to_string(),
                    class_name: "CabinetWClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(297, 53, 1607, 1011),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "Details pane".to_string(),
                    class_name: "DetailsPaneHwndHostClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(1521, 223, 374, 809),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments.get(&20).map(|assessment| (
                assessment.role,
                assessment.disposition,
                assessment.role_reason.as_str(),
                assessment.presentation_preflight,
            )),
            Some((
                PlatformWindowRole::Auxiliary,
                PlatformWindowDisposition::AuxiliaryAppSurface,
                "known-internal-family-surface",
                PlatformPresentationPreflight::Rejected,
            ))
        );
    }

    #[test]
    fn discovery_assessment_marks_titled_explorer_bridge_surface_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "Explorer".to_string(),
                    class_name: "CabinetWClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(297, 53, 1607, 1011),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "Child bridge".to_string(),
                    class_name: "Microsoft.UI.Content.DesktopChildSiteBridge".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(1537, 223, 374, 801),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments.get(&20).map(|assessment| (
                assessment.role,
                assessment.disposition,
                assessment.role_reason.as_str(),
                assessment.presentation_preflight,
            )),
            Some((
                PlatformWindowRole::Auxiliary,
                PlatformWindowDisposition::AuxiliaryAppSurface,
                "known-internal-family-surface",
                PlatformPresentationPreflight::Rejected,
            ))
        );
    }

    #[test]
    fn discovery_assessment_marks_duplicate_titleless_family_cluster_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "Explorer".to_string(),
                    class_name: "CabinetWClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(0, 0, 1200, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "".to_string(),
                    class_name: "CabinetWClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(1420, 140, 374, 662),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 21,
                    title: "".to_string(),
                    class_name: "CabinetWClass".to_string(),
                    process_id: 77,
                    process_name: Some("explorer".to_string()),
                    rect: Rect::new(1424, 144, 370, 658),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments
                .get(&20)
                .map(|assessment| (assessment.role, assessment.disposition)),
            Some((
                PlatformWindowRole::Auxiliary,
                PlatformWindowDisposition::AuxiliaryAppSurface,
            ))
        );
        assert_eq!(
            assessments
                .get(&21)
                .map(|assessment| assessment.role_reason.as_str()),
            Some("duplicate-titleless-family-surface")
        );
    }

    #[test]
    fn discovery_assessment_marks_titleless_non_primary_footprint_as_auxiliary() {
        let snapshot = PlatformSnapshot {
            foreground_hwnd: Some(10),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "IDE".to_string(),
                    class_name: "SunAwtFrame".to_string(),
                    process_id: 91,
                    process_name: Some("idea64".to_string()),
                    rect: Rect::new(0, 0, 1200, 900),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 11,
                    title: "".to_string(),
                    class_name: "SunAwtWindow".to_string(),
                    process_id: 91,
                    process_name: Some("idea64".to_string()),
                    rect: Rect::new(1600, 40, 176, 50),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
            ],
        };

        let assessments = assess_platform_snapshot_for_discovery(&snapshot);
        assert_eq!(
            assessments
                .get(&11)
                .map(|assessment| (assessment.role, assessment.role_reason.as_str())),
            Some((
                PlatformWindowRole::Auxiliary,
                "titleless-non-primary-footprint",
            ))
        );
    }

    fn sample_snapshot() -> PlatformSnapshot {
        PlatformSnapshot {
            foreground_hwnd: Some(20),
            monitors: vec![PlatformMonitorSnapshot {
                binding: "\\\\.\\DISPLAY1".to_string(),
                work_area_rect: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
                is_primary: true,
            }],
            windows: vec![
                PlatformWindowSnapshot {
                    hwnd: 10,
                    title: "First".to_string(),
                    class_name: "AppWindow".to_string(),
                    process_id: 1,
                    process_name: Some("first-app".to_string()),
                    rect: Rect::new(0, 0, 500, 600),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: false,
                    management_candidate: true,
                },
                PlatformWindowSnapshot {
                    hwnd: 20,
                    title: "Second".to_string(),
                    class_name: "AppWindow".to_string(),
                    process_id: 2,
                    process_name: Some("second-app".to_string()),
                    rect: Rect::new(500, 0, 500, 600),
                    monitor_binding: "\\\\.\\DISPLAY1".to_string(),
                    is_visible: true,
                    is_focused: true,
                    management_candidate: true,
                },
            ],
        }
    }
}
