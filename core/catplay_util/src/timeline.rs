//! Connect timeline: when each stage of bringing up CarPlay was first reached, measured from kernel boot.
//!
//! Stages are marked from different crates. Each stage is logged once per process, the first time it is
//! reached. When the first iPhone frame reaches the car, one `connect_timeline:` line summarises the boot,
//! so cold starts are easy to compare. If CarPlay never comes up (for example the car never opens the USB
//! session), the per-stage lines show how far it got.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// `catplay_c2a` started.
    AppStart,
    /// The car started a CarPlay session over USB (iAP2 is done and the USB network link is up).
    CarUsb,
    /// The car's CarPlay session is up and the dongle is transmitting to it.
    CarSession,
    /// The iPhone opened iAP2 over Bluetooth.
    PhoneBluetooth,
    /// The iPhone made its first connection over Wi-Fi.
    PhoneWifi,
    /// The iPhone's CarPlay session is up.
    PhoneSession,
    /// The first iPhone video frame was sent to the car.
    FirstFrame,
}

impl Stage {
    const ALL: [Stage; 7] = [
        Stage::AppStart,
        Stage::CarUsb,
        Stage::CarSession,
        Stage::PhoneBluetooth,
        Stage::PhoneWifi,
        Stage::PhoneSession,
        Stage::FirstFrame,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::AppStart => "app_start",
            Stage::CarUsb => "car_usb",
            Stage::CarSession => "car_session",
            Stage::PhoneBluetooth => "phone_bluetooth",
            Stage::PhoneWifi => "phone_wifi",
            Stage::PhoneSession => "phone_session",
            Stage::FirstFrame => "first_frame",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

struct Timeline {
    /// Process clock reading when the timeline was created.
    origin: Instant,
    /// Time since kernel boot at `origin`, if the system exposes it.
    origin_since_boot: Option<Duration>,
    marks: [Option<Duration>; Stage::ALL.len()],
}

impl Timeline {
    fn new(origin: Instant, origin_since_boot: Option<Duration>) -> Self {
        Self {
            origin,
            origin_since_boot,
            marks: [None; Stage::ALL.len()],
        }
    }

    fn clock(&self) -> &'static str {
        match self.origin_since_boot {
            Some(_) => "since boot",
            None => "since app start",
        }
    }

    /// Records `stage` at `now` if it was not reached before, and returns the line to log.
    fn mark(&mut self, stage: Stage, now: Instant) -> Option<String> {
        let slot = &mut self.marks[stage as usize];
        if slot.is_some() {
            return None;
        }

        let at = self.origin_since_boot.unwrap_or_default() + now.saturating_duration_since(self.origin);
        *slot = Some(at);

        Some(match stage {
            Stage::FirstFrame => self.summary(),
            _ => format!("connect_timeline: {} at {} {}", stage.name(), secs(at), self.clock()),
        })
    }

    fn summary(&self) -> String {
        let stages: Vec<String> = Stage::ALL
            .iter()
            .map(|stage| match self.marks[*stage as usize] {
                Some(at) => format!("{}={}", stage.name(), secs(at)),
                None => format!("{}=-", stage.name()),
            })
            .collect();

        format!("connect_timeline: {} ({})", stages.join(" "), self.clock())
    }
}

fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// Time since boot from `/proc/uptime` (10 ms resolution); `None` where it is not available.
fn since_boot() -> Option<Duration> {
    parse_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)
}

fn parse_uptime(uptime: &str) -> Option<Duration> {
    let secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

static TIMELINE: Mutex<Option<Timeline>> = Mutex::new(None);
/// Stages already marked, so repeated marks (e.g. every video frame) skip the lock.
static REACHED: AtomicU8 = AtomicU8::new(0);

/// Marks `stage` as reached now. Only the first mark of each stage counts.
pub fn mark(stage: Stage) {
    if REACHED.load(Ordering::Relaxed) & stage.bit() != 0 {
        return;
    }

    let now = Instant::now();
    let line = {
        let Ok(mut timeline) = TIMELINE.lock() else {
            return;
        };
        REACHED.fetch_or(stage.bit(), Ordering::Relaxed);
        timeline
            .get_or_insert_with(|| Timeline::new(now, since_boot()))
            .mark(stage, now)
    };

    if let Some(line) = line {
        log::info!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_stage_is_reported_once() {
        let origin = Instant::now();
        let mut timeline = Timeline::new(origin, Some(Duration::from_millis(2_000)));

        assert_eq!(
            timeline
                .mark(Stage::CarUsb, origin + Duration::from_millis(1_250))
                .as_deref(),
            Some("connect_timeline: car_usb at 3.25s since boot")
        );
        assert_eq!(timeline.mark(Stage::CarUsb, origin + Duration::from_millis(9_000)), None);
    }

    #[test]
    fn first_frame_summarises_all_stages() {
        let origin = Instant::now();
        let mut timeline = Timeline::new(origin, Some(Duration::from_millis(2_000)));
        let at = |ms| origin + Duration::from_millis(ms);

        timeline.mark(Stage::AppStart, at(0));
        timeline.mark(Stage::PhoneBluetooth, at(900));
        timeline.mark(Stage::CarUsb, at(1_500));
        timeline.mark(Stage::CarSession, at(3_010));
        timeline.mark(Stage::PhoneSession, at(4_000));

        assert_eq!(
            timeline.mark(Stage::FirstFrame, at(4_600)).as_deref(),
            Some(
                "connect_timeline: app_start=2.00s car_usb=3.50s car_session=5.01s phone_bluetooth=2.90s \
                 phone_wifi=- phone_session=6.00s first_frame=6.60s (since boot)"
            )
        );
        assert_eq!(timeline.mark(Stage::FirstFrame, at(5_000)), None, "only the first frame counts");
    }

    #[test]
    fn falls_back_to_time_since_app_start() {
        let origin = Instant::now();
        let mut timeline = Timeline::new(origin, None);

        assert_eq!(
            timeline
                .mark(Stage::PhoneWifi, origin + Duration::from_millis(500))
                .as_deref(),
            Some("connect_timeline: phone_wifi at 0.50s since app start")
        );
    }

    #[test]
    fn parses_proc_uptime() {
        assert_eq!(parse_uptime("12.50 40.10\n"), Some(Duration::from_millis(12_500)));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("abc 1.0"), None);
    }
}
