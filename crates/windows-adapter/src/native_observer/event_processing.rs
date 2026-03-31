use std::{
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW,
    EVENT_SYSTEM_FOREGROUND,
};

use crate::{
    AdapterPerfTelemetry, ObservationEnvelope, ObservationKind, ObserverMessage, PlatformSnapshot,
    native_snapshot,
};

const PERIODIC_SCAN_BACKOFF_MULTIPLIER: u32 = 2;

pub(super) enum IncrementalApplyResult {
    Continue,
    Rescanned,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RescanSnapshotResult {
    Changed,
    Unchanged,
    Warning,
    Stop,
}

pub(super) fn apply_incremental_event(
    event_type: u32,
    hwnd: u64,
    sender: &Sender<ObserverMessage>,
    snapshot: &mut PlatformSnapshot,
    perf: &AdapterPerfTelemetry,
) -> IncrementalApplyResult {
    let started_at = Instant::now();
    let reason = event_reason(event_type);
    let hwnd_known_before = snapshot_contains_hwnd(snapshot, hwnd);
    let updated = match event_type {
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => {
            native_snapshot::remove_window(snapshot, hwnd);
            native_snapshot::refresh_focus(snapshot)
        }
        _ => native_snapshot::refresh_window(snapshot, hwnd),
    };

    let mut refresh_failed = false;
    let result = match updated {
        Ok(()) => {
            let hwnd_known_after = snapshot_contains_hwnd(snapshot, hwnd);
            if should_rescan_after_incremental_event(
                event_type,
                hwnd_known_before,
                hwnd_known_after,
            ) {
                match rescan_snapshot("event-recovery-full-scan", sender, snapshot, perf) {
                    RescanSnapshotResult::Stop => IncrementalApplyResult::Stop,
                    RescanSnapshotResult::Changed
                    | RescanSnapshotResult::Unchanged
                    | RescanSnapshotResult::Warning => IncrementalApplyResult::Rescanned,
                }
            } else if sender
                .send(ObserverMessage::Envelope(snapshot_envelope(
                    reason,
                    snapshot.clone(),
                )))
                .is_ok()
            {
                IncrementalApplyResult::Continue
            } else {
                IncrementalApplyResult::Stop
            }
        }
        Err(message) => {
            refresh_failed = true;
            if sender
                .send(ObserverMessage::Envelope(warning_envelope(
                    reason, &message,
                )))
                .is_err()
            {
                return IncrementalApplyResult::Stop;
            }
            match rescan_snapshot("event-recovery-full-scan", sender, snapshot, perf) {
                RescanSnapshotResult::Stop => IncrementalApplyResult::Stop,
                RescanSnapshotResult::Changed
                | RescanSnapshotResult::Unchanged
                | RescanSnapshotResult::Warning => IncrementalApplyResult::Rescanned,
            }
        }
    };

    perf.observer_incremental_event
        .record_duration(started_at.elapsed());
    if refresh_failed {
        perf.observer_incremental_event.record_error();
    }
    result
}

fn snapshot_contains_hwnd(snapshot: &PlatformSnapshot, hwnd: u64) -> bool {
    snapshot.windows.iter().any(|window| window.hwnd == hwnd)
}

pub(super) fn should_rescan_after_incremental_event(
    event_type: u32,
    hwnd_known_before: bool,
    hwnd_known_after: bool,
) -> bool {
    match event_type {
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE => !(hwnd_known_before && hwnd_known_after),
        EVENT_OBJECT_CREATE | EVENT_OBJECT_DESTROY | EVENT_SYSTEM_FOREGROUND => {
            !hwnd_known_after && !hwnd_known_before
        }
        _ => false,
    }
}

pub(super) fn next_periodic_scan_interval(
    current_interval: Duration,
    minimum_interval: Duration,
    maximum_interval: Duration,
) -> Duration {
    current_interval
        .max(minimum_interval)
        .saturating_mul(PERIODIC_SCAN_BACKOFF_MULTIPLIER)
        .min(maximum_interval.max(minimum_interval))
}

pub(super) fn rescan_snapshot(
    reason: &str,
    sender: &Sender<ObserverMessage>,
    snapshot: &mut PlatformSnapshot,
    perf: &AdapterPerfTelemetry,
) -> RescanSnapshotResult {
    let started_at = Instant::now();
    let result = match native_snapshot::scan_snapshot() {
        Ok(new_snapshot) => {
            if *snapshot == new_snapshot {
                perf.observer_rescan_snapshot.record_skip();
                RescanSnapshotResult::Unchanged
            } else {
                *snapshot = new_snapshot.clone();
                if sender
                    .send(ObserverMessage::Envelope(snapshot_envelope(
                        reason,
                        new_snapshot,
                    )))
                    .is_ok()
                {
                    RescanSnapshotResult::Changed
                } else {
                    RescanSnapshotResult::Stop
                }
            }
        }
        Err(error) => sender
            .send(ObserverMessage::Envelope(warning_envelope(
                reason,
                &error.to_string(),
            )))
            .map(|_| RescanSnapshotResult::Warning)
            .unwrap_or(RescanSnapshotResult::Stop),
    };
    perf.observer_rescan_snapshot
        .record_duration(started_at.elapsed());
    if matches!(result, RescanSnapshotResult::Warning) {
        perf.observer_rescan_snapshot.record_error();
    }
    result
}

pub(super) fn snapshot_envelope(reason: &str, snapshot: PlatformSnapshot) -> ObservationEnvelope {
    ObservationEnvelope {
        kind: ObservationKind::Snapshot,
        reason: reason.to_string(),
        snapshot: Some(snapshot),
        message: None,
    }
}

fn warning_envelope(reason: &str, message: &str) -> ObservationEnvelope {
    ObservationEnvelope {
        kind: ObservationKind::Warning,
        reason: reason.to_string(),
        snapshot: None,
        message: Some(message.to_string()),
    }
}

fn event_reason(event_type: u32) -> &'static str {
    match event_type {
        EVENT_SYSTEM_FOREGROUND => "win-event-foreground",
        EVENT_OBJECT_CREATE => "win-event-create",
        EVENT_OBJECT_DESTROY => "win-event-destroy",
        EVENT_OBJECT_SHOW => "win-event-show",
        EVENT_OBJECT_HIDE => "win-event-hide",
        windows_sys::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_LOCATIONCHANGE => {
            "win-event-location-change"
        }
        _ => "win-event-update",
    }
}
