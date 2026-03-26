#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::Duration,
};

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
pub const TILED_VISUAL_OVERLAP_X_PX: i32 = 1;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowVisualEmphasis {
    pub opacity_alpha: u8,
    #[serde(default)]
    pub border_color_rgb: Option<u32>,
    pub border_thickness_px: u8,
    #[serde(default)]
    pub rounded_corners: bool,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowsAdapter;

impl WindowsAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn spawn_observer(
        &self,
        options: LiveObservationOptions,
    ) -> Result<ObservationStream, WindowsAdapterError> {
        let (sender, receiver) = mpsc::channel::<ObserverMessage>();
        let runtime = native_observer::spawn(options, sender)?;
        Ok(ObservationStream {
            backend: ObservationBackend::Native(runtime),
            receiver,
        })
    }

    pub fn scan_snapshot(&self) -> Result<PlatformSnapshot, WindowsAdapterError> {
        native_snapshot::scan_snapshot()
    }

    pub fn apply_operations(
        &self,
        operations: &[ApplyOperation],
    ) -> Result<ApplyBatchResult, WindowsAdapterError> {
        if operations.is_empty() {
            return Ok(ApplyBatchResult::default());
        }

        Ok(native_apply::apply_operations(operations))
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

#[cfg(test)]
mod tests {
    use flowtile_domain::Rect;

    use super::{
        ObservationEnvelope, ObservationKind, PRIMARY_DISCOVERY_API, PlatformMonitorSnapshot,
        PlatformSnapshot, PlatformWindowSnapshot, SnapshotDiff, bootstrap, diff_snapshots,
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
            Rect::new(99, 0, 400, 600),
            Rect::new(100, 0, 400, 600),
        ));
    }

    #[test]
    fn tiled_overlap_tolerance_accepts_right_side_slack_after_shift() {
        assert!(!needs_tiled_gapless_geometry_apply(
            Rect::new(99, 0, 402, 600),
            Rect::new(100, 0, 400, 600),
        ));
    }

    #[test]
    fn tiled_overlap_tolerance_rejects_missing_left_shift() {
        assert!(needs_tiled_gapless_geometry_apply(
            Rect::new(100, 0, 400, 600),
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
