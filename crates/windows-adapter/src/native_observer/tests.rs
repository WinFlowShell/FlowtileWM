use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
};

use super::event_processing::{next_periodic_scan_interval, should_rescan_after_incremental_event};

#[test]
fn create_for_unknown_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_CREATE,
        false,
        false,
    ));
}

#[test]
fn show_for_unknown_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_SHOW,
        false,
        false,
    ));
}

#[test]
fn foreground_for_unknown_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_SYSTEM_FOREGROUND,
        false,
        false,
    ));
}

#[test]
fn create_for_known_hwnd_does_not_force_rescan() {
    assert!(!should_rescan_after_incremental_event(
        EVENT_OBJECT_CREATE,
        true,
        true,
    ));
}

#[test]
fn hide_for_known_hwnd_membership_change_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_HIDE,
        true,
        false,
    ));
}

#[test]
fn show_for_restored_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_SHOW,
        false,
        true,
    ));
}

#[test]
fn location_change_for_unknown_hwnd_does_not_force_rescan() {
    assert!(!should_rescan_after_incremental_event(
        EVENT_OBJECT_LOCATIONCHANGE,
        false,
        false,
    ));
}

#[test]
fn destroy_for_unknown_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_DESTROY,
        false,
        false,
    ));
}

#[test]
fn hide_for_unknown_hwnd_escalates_to_full_scan() {
    assert!(should_rescan_after_incremental_event(
        EVENT_OBJECT_HIDE,
        false,
        false,
    ));
}

#[test]
fn clean_periodic_rescan_uses_backoff() {
    let minimum = Duration::from_secs(2);
    let maximum = Duration::from_secs(16);

    assert_eq!(
        next_periodic_scan_interval(minimum, minimum, maximum),
        Duration::from_secs(4)
    );
    assert_eq!(
        next_periodic_scan_interval(Duration::from_secs(8), minimum, maximum),
        Duration::from_secs(16)
    );
}

#[test]
fn periodic_rescan_backoff_respects_maximum_interval() {
    let minimum = Duration::from_secs(2);
    let maximum = Duration::from_secs(12);

    assert_eq!(
        next_periodic_scan_interval(Duration::from_secs(8), minimum, maximum),
        Duration::from_secs(12)
    );
}
