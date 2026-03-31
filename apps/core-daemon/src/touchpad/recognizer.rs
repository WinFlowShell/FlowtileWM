use std::collections::HashMap;

use super::bindings::TouchpadGesture;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RawTouchContact {
    pub(super) contact_id: u8,
    pub(super) x: i32,
    pub(super) y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawTouchPoint {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ParsedRawTouchpadReport {
    pub(super) scan_time: u16,
    pub(super) contact_count: usize,
    pub(super) contact: Option<RawTouchContact>,
}

#[derive(Default)]
pub(super) struct RawTouchpadFrameAssembler {
    active_scan_time: Option<u16>,
    expected_contacts: usize,
    contacts: HashMap<u8, RawTouchContact>,
    recognizer: SwipeRecognizer,
}

#[derive(Default)]
struct SwipeRecognizer {
    session: Option<SwipeSession>,
}

struct SwipeSession {
    finger_count: usize,
    start_centroid: RawTouchPoint,
    last_centroid: RawTouchPoint,
}

impl RawTouchpadFrameAssembler {
    pub(super) fn process_report(
        &mut self,
        report: ParsedRawTouchpadReport,
    ) -> Option<TouchpadGesture> {
        if self
            .active_scan_time
            .is_some_and(|active_scan_time| active_scan_time != report.scan_time)
        {
            let gesture = self.flush_frame();
            self.begin_frame(report.scan_time, report.contact_count);
            if let Some(contact) = report.contact {
                self.contacts.insert(contact.contact_id, contact);
            }
            if self.should_flush_current_frame(report.contact_count) {
                return gesture.or_else(|| self.flush_frame());
            }
            return gesture;
        }

        if self.active_scan_time.is_none() {
            self.begin_frame(report.scan_time, report.contact_count);
        }

        if let Some(contact) = report.contact {
            self.contacts.insert(contact.contact_id, contact);
        }

        if self.should_flush_current_frame(report.contact_count) {
            return self.flush_frame();
        }

        None
    }

    fn begin_frame(&mut self, scan_time: u16, contact_count: usize) {
        self.active_scan_time = Some(scan_time);
        self.expected_contacts = contact_count;
        self.contacts.clear();
    }

    fn should_flush_current_frame(&self, reported_contact_count: usize) -> bool {
        reported_contact_count == 0
            || (self.expected_contacts > 0 && self.contacts.len() >= self.expected_contacts)
    }

    fn flush_frame(&mut self) -> Option<TouchpadGesture> {
        self.active_scan_time = None;
        self.expected_contacts = 0;
        let contacts = self
            .contacts
            .drain()
            .map(|(_, contact)| contact)
            .collect::<Vec<_>>();
        self.recognizer.process_contacts(&contacts)
    }
}

impl SwipeRecognizer {
    fn process_contacts(&mut self, contacts: &[RawTouchContact]) -> Option<TouchpadGesture> {
        let finger_count = contacts.len();
        if !(3..=4).contains(&finger_count) {
            return self.finish_current_session();
        }

        let centroid = centroid_for_contacts(contacts);
        let session = self.session.get_or_insert(SwipeSession {
            finger_count,
            start_centroid: centroid,
            last_centroid: centroid,
        });
        session.finger_count = session.finger_count.max(finger_count);
        session.last_centroid = centroid;
        None
    }

    fn finish_current_session(&mut self) -> Option<TouchpadGesture> {
        let session = self.session.take()?;
        recognize_swipe(session)
    }
}

fn recognize_swipe(session: SwipeSession) -> Option<TouchpadGesture> {
    const SWIPE_DISTANCE_THRESHOLD: i32 = 120;
    const DOMINANCE_RATIO_NUMERATOR: i32 = 3;
    const DOMINANCE_RATIO_DENOMINATOR: i32 = 2;

    let delta_x = session.last_centroid.x - session.start_centroid.x;
    let delta_y = session.last_centroid.y - session.start_centroid.y;
    let abs_x = delta_x.abs();
    let abs_y = delta_y.abs();

    if abs_x < SWIPE_DISTANCE_THRESHOLD && abs_y < SWIPE_DISTANCE_THRESHOLD {
        return None;
    }

    let horizontal = abs_x * DOMINANCE_RATIO_DENOMINATOR >= abs_y * DOMINANCE_RATIO_NUMERATOR;
    let vertical = abs_y * DOMINANCE_RATIO_DENOMINATOR >= abs_x * DOMINANCE_RATIO_NUMERATOR;

    match (session.finger_count, horizontal, vertical) {
        (3, true, false) if delta_x > 0 => Some(TouchpadGesture::ThreeFingerSwipeRight),
        (3, true, false) if delta_x < 0 => Some(TouchpadGesture::ThreeFingerSwipeLeft),
        (3, false, true) if delta_y > 0 => Some(TouchpadGesture::ThreeFingerSwipeDown),
        (3, false, true) if delta_y < 0 => Some(TouchpadGesture::ThreeFingerSwipeUp),
        (4, true, false) if delta_x > 0 => Some(TouchpadGesture::FourFingerSwipeRight),
        (4, true, false) if delta_x < 0 => Some(TouchpadGesture::FourFingerSwipeLeft),
        (4, false, true) if delta_y > 0 => Some(TouchpadGesture::FourFingerSwipeDown),
        (4, false, true) if delta_y < 0 => Some(TouchpadGesture::FourFingerSwipeUp),
        _ => None,
    }
}

fn centroid_for_contacts(contacts: &[RawTouchContact]) -> RawTouchPoint {
    let sum_x = contacts
        .iter()
        .map(|contact| i64::from(contact.x))
        .sum::<i64>();
    let sum_y = contacts
        .iter()
        .map(|contact| i64::from(contact.y))
        .sum::<i64>();
    let count = i64::try_from(contacts.len()).unwrap_or(1);

    RawTouchPoint {
        x: (sum_x / count) as i32,
        y: (sum_y / count) as i32,
    }
}

pub(super) fn parse_sample_touchpad_report(report: &[u8]) -> Option<ParsedRawTouchpadReport> {
    let offset = match report.len() {
        len if len >= 10 => 1,
        len if len >= 9 => 0,
        _ => return None,
    };

    let header = *report.get(offset)?;
    let contact_id = (header >> 2) & 0x03;
    let tip_switch = (header & 0b0000_0010) != 0;
    let confidence = (header & 0b0000_0001) != 0;
    let x = i32::from(u16::from_le_bytes([
        *report.get(offset + 1)?,
        *report.get(offset + 2)?,
    ]));
    let y = i32::from(u16::from_le_bytes([
        *report.get(offset + 3)?,
        *report.get(offset + 4)?,
    ]));
    let scan_time = u16::from_le_bytes([*report.get(offset + 5)?, *report.get(offset + 6)?]);
    let contact_count = usize::from(*report.get(offset + 7)?);

    Some(ParsedRawTouchpadReport {
        scan_time,
        contact_count,
        contact: (tip_switch && confidence).then_some(RawTouchContact { contact_id, x, y }),
    })
}
