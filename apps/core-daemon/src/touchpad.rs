mod assessment;
mod bindings;
mod native;
mod recognizer;
mod runtime;

pub(crate) use assessment::{assess_touchpad_override, ensure_touchpad_override_supported};
pub(crate) use bindings::ipc_command_for_touchpad_gesture;
pub(crate) use runtime::TouchpadListener;

#[derive(Debug)]
pub(crate) enum TouchpadListenerError {
    Startup(String),
}

impl std::fmt::Display for TouchpadListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Startup(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TouchpadListenerError {}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use flowtile_config_rules::{TouchpadConfig, TouchpadGestureBinding};

    use super::{
        TouchpadListener,
        assessment::{
            SystemTouchGestureSetting, TOUCHPAD_BACKEND_UNAVAILABLE_STATUS,
            TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS, TouchpadOverrideAssessment,
            assess_touchpad_override_with_system_setting, ensure_touchpad_override_supported,
        },
        bindings::{TouchpadBindingSet, TouchpadGesture, ipc_command_for_touchpad_gesture},
        recognizer::{
            ParsedRawTouchpadReport, RawTouchContact, RawTouchpadFrameAssembler,
            parse_sample_touchpad_report,
        },
    };
    use crate::control::{ControlMessage, WatchCommand};

    #[test]
    fn disabled_touchpad_override_needs_no_runtime() {
        let config = TouchpadConfig {
            override_enabled: false,
            gestures: Vec::new(),
        };

        assert!(ensure_touchpad_override_supported(&config).is_ok());
        assert!(
            TouchpadListener::spawn(&config, mpsc::channel().0)
                .expect("disabled touchpad override should not fail")
                .is_none()
        );
    }

    #[test]
    fn enabled_touchpad_override_is_ready_after_system_precondition_is_met() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-up".to_string(),
                    command: "focus-workspace-down".to_string(),
                }],
            },
            SystemTouchGestureSetting::Disabled,
        );

        assert_eq!(assessment.status, "ready");
        assert_eq!(assessment.summary_label(), "enabled");
        assert_eq!(assessment.normalized_gesture_count, 1);
    }

    #[test]
    fn unknown_system_setting_is_reported_as_unavailable_precondition() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-down".to_string(),
                    command: "focus-workspace-up".to_string(),
                }],
            },
            SystemTouchGestureSetting::Unknown("registry read failed".to_string()),
        );

        assert_eq!(assessment.summary_label(), "windows-setting-unknown");
        assert!(
            assessment
                .detail
                .expect("detail should exist")
                .contains("registry read failed")
        );
    }

    #[test]
    fn rejects_unknown_touchpad_command() {
        let config = TouchpadConfig {
            override_enabled: true,
            gestures: vec![TouchpadGestureBinding {
                gesture: "three-finger-swipe-up".to_string(),
                command: "move-column-left".to_string(),
            }],
        };

        let error = ensure_touchpad_override_supported(&config)
            .expect_err("unsupported command should fail validation");
        assert!(error.to_string().contains("unsupported command"));
    }

    #[test]
    fn rejects_duplicate_touchpad_gesture_bindings() {
        let config = TouchpadConfig {
            override_enabled: true,
            gestures: vec![
                TouchpadGestureBinding {
                    gesture: "three-finger-swipe-up".to_string(),
                    command: "focus-workspace-down".to_string(),
                },
                TouchpadGestureBinding {
                    gesture: "three-finger-swipe-up".to_string(),
                    command: "focus-workspace-up".to_string(),
                },
            ],
        };

        let error = ensure_touchpad_override_supported(&config)
            .expect_err("duplicate gesture should fail validation");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn summary_maps_backend_unavailable_status_to_explicit_label() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-down".to_string(),
                    command: "focus-workspace-up".to_string(),
                }],
            },
            SystemTouchGestureSetting::Disabled,
        );

        let unavailable_assessment = TouchpadOverrideAssessment {
            requested: assessment.requested,
            configured_gesture_count: assessment.configured_gesture_count,
            normalized_gesture_count: assessment.normalized_gesture_count,
            status: TOUCHPAD_BACKEND_UNAVAILABLE_STATUS,
            detail: Some("raw input backend is unavailable".to_string()),
        };

        assert_eq!(
            unavailable_assessment.summary_label(),
            "backend-unavailable"
        );
    }

    #[test]
    fn assessment_reports_windows_setting_required_when_system_gestures_are_still_enabled() {
        let assessment = assess_touchpad_override_with_system_setting(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-up".to_string(),
                    command: "focus-workspace-down".to_string(),
                }],
            },
            SystemTouchGestureSetting::Enabled,
        );

        assert_eq!(assessment.summary_label(), "windows-setting-required");
        assert_eq!(assessment.status, TOUCHPAD_SYSTEM_SETTING_REQUIRED_STATUS);
        assert!(
            assessment
                .detail
                .expect("detail should exist")
                .contains("Three- and four-finger touch gestures")
        );
    }

    #[test]
    fn runtime_dispatches_workspace_swipe_into_control_channel() {
        let bindings = TouchpadBindingSet::from_config(&TouchpadConfig {
            override_enabled: true,
            gestures: vec![TouchpadGestureBinding {
                gesture: "three-finger-swipe-up".to_string(),
                command: "focus-workspace-down".to_string(),
            }],
        })
        .expect("bindings should normalize");
        let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
        let listener = TouchpadListener::spawn_runtime_only(bindings, control_sender);

        listener
            .dispatch_gesture(TouchpadGesture::ThreeFingerSwipeUp)
            .expect("gesture should dispatch");

        let message = control_receiver
            .recv()
            .expect("command should be forwarded");
        assert!(matches!(
            message,
            ControlMessage::Watch(WatchCommand::FocusWorkspaceDown)
        ));
    }

    #[test]
    fn resolves_workspace_swipe_to_existing_ipc_command() {
        let command = ipc_command_for_touchpad_gesture(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-down".to_string(),
                    command: "focus-workspace-up".to_string(),
                }],
            },
            "three-finger-swipe-down",
        )
        .expect("gesture should resolve")
        .expect("binding should exist");

        assert_eq!(command, "focus_workspace_up");
    }

    #[test]
    fn runtime_dispatches_horizontal_window_swipe_into_control_channel() {
        let bindings = TouchpadBindingSet::from_config(&TouchpadConfig {
            override_enabled: true,
            gestures: vec![TouchpadGestureBinding {
                gesture: "three-finger-swipe-left".to_string(),
                command: "focus-next".to_string(),
            }],
        })
        .expect("bindings should normalize");
        let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
        let listener = TouchpadListener::spawn_runtime_only(bindings, control_sender);

        listener
            .dispatch_gesture(TouchpadGesture::ThreeFingerSwipeLeft)
            .expect("gesture should dispatch");

        let message = control_receiver
            .recv()
            .expect("command should be forwarded");
        assert!(matches!(
            message,
            ControlMessage::Watch(WatchCommand::FocusNext)
        ));
    }

    #[test]
    fn resolves_horizontal_window_swipe_to_existing_ipc_command() {
        let command = ipc_command_for_touchpad_gesture(
            &TouchpadConfig {
                override_enabled: true,
                gestures: vec![TouchpadGestureBinding {
                    gesture: "three-finger-swipe-right".to_string(),
                    command: "focus-prev".to_string(),
                }],
            },
            "three-finger-swipe-right",
        )
        .expect("gesture should resolve")
        .expect("binding should exist");

        assert_eq!(command, "focus_prev");
    }

    #[test]
    fn four_finger_vertical_swipes_resolve_to_directional_overview_commands() {
        let config = TouchpadConfig {
            override_enabled: true,
            gestures: vec![
                TouchpadGestureBinding {
                    gesture: "four-finger-swipe-up".to_string(),
                    command: "open-overview".to_string(),
                },
                TouchpadGestureBinding {
                    gesture: "four-finger-swipe-down".to_string(),
                    command: "close-overview".to_string(),
                },
            ],
        };

        let open_command = ipc_command_for_touchpad_gesture(&config, "four-finger-swipe-up")
            .expect("up gesture should resolve")
            .expect("up gesture should be bound");
        let close_command = ipc_command_for_touchpad_gesture(&config, "four-finger-swipe-down")
            .expect("down gesture should resolve")
            .expect("down gesture should be bound");

        assert_eq!(open_command, "open_overview");
        assert_eq!(close_command, "close_overview");
    }

    #[test]
    fn runtime_dispatches_directional_overview_gestures_into_control_channel() {
        let bindings = TouchpadBindingSet::from_config(&TouchpadConfig {
            override_enabled: true,
            gestures: vec![
                TouchpadGestureBinding {
                    gesture: "four-finger-swipe-up".to_string(),
                    command: "open-overview".to_string(),
                },
                TouchpadGestureBinding {
                    gesture: "four-finger-swipe-down".to_string(),
                    command: "close-overview".to_string(),
                },
            ],
        })
        .expect("bindings should normalize");
        let (control_sender, control_receiver) = mpsc::channel::<ControlMessage>();
        let listener = TouchpadListener::spawn_runtime_only(bindings, control_sender);

        listener
            .dispatch_gesture(TouchpadGesture::FourFingerSwipeUp)
            .expect("open gesture should dispatch");
        listener
            .dispatch_gesture(TouchpadGesture::FourFingerSwipeDown)
            .expect("close gesture should dispatch");

        let first = control_receiver.recv().expect("open command should arrive");
        let second = control_receiver
            .recv()
            .expect("close command should arrive");
        assert!(matches!(
            first,
            ControlMessage::Watch(WatchCommand::OpenOverview)
        ));
        assert!(matches!(
            second,
            ControlMessage::Watch(WatchCommand::CloseOverview)
        ));
    }

    #[test]
    fn parses_sample_touchpad_report_with_report_id() {
        let report = [
            0x01,
            0b0000_0111,
            0x20,
            0x03,
            0x10,
            0x02,
            0x34,
            0x12,
            0x03,
            0x00,
        ];
        let parsed = parse_sample_touchpad_report(&report).expect("sample report should parse");

        assert_eq!(
            parsed,
            ParsedRawTouchpadReport {
                scan_time: 0x1234,
                contact_count: 3,
                contact: Some(RawTouchContact {
                    contact_id: 1,
                    x: 0x0320,
                    y: 0x0210,
                }),
            }
        );
    }

    #[test]
    fn assembler_turns_three_finger_frames_into_swipe() {
        let mut assembler = RawTouchpadFrameAssembler::default();
        let start_contacts = [
            RawTouchContact {
                contact_id: 0,
                x: 100,
                y: 200,
            },
            RawTouchContact {
                contact_id: 1,
                x: 140,
                y: 210,
            },
            RawTouchContact {
                contact_id: 2,
                x: 180,
                y: 220,
            },
        ];
        let end_contacts = [
            RawTouchContact {
                contact_id: 0,
                x: 320,
                y: 205,
            },
            RawTouchContact {
                contact_id: 1,
                x: 360,
                y: 215,
            },
            RawTouchContact {
                contact_id: 2,
                x: 400,
                y: 225,
            },
        ];

        for contact in start_contacts {
            assert!(
                assembler
                    .process_report(ParsedRawTouchpadReport {
                        scan_time: 10,
                        contact_count: 3,
                        contact: Some(contact),
                    })
                    .is_none()
            );
        }
        for contact in end_contacts {
            assert!(
                assembler
                    .process_report(ParsedRawTouchpadReport {
                        scan_time: 11,
                        contact_count: 3,
                        contact: Some(contact),
                    })
                    .is_none()
            );
        }

        let gesture = assembler.process_report(ParsedRawTouchpadReport {
            scan_time: 12,
            contact_count: 0,
            contact: None,
        });

        assert_eq!(gesture, Some(TouchpadGesture::ThreeFingerSwipeRight));
    }
}
