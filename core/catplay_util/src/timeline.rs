//! Connect timeline: when each stage of bringing up CarPlay was first reached, measured from kernel boot.
//!
//! Stages are marked from different crates. Each stage is logged once per process, the first time it is
//! reached. When the first iPhone frame reaches the car, one `connect_timeline:` line summarises the boot,
//! so cold starts are easy to compare. If CarPlay never comes up (for example the car never opens the USB
//! session), the per-stage lines show how far it got.
//!
//! Errors can be counted with [record_error], and [set_listener] lets a writer (e.g. a persistent boot journal)
//! follow changes and read them back with [snapshot].

use std::{
    fmt::Display,
    sync::{
        Mutex, OnceLock,
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

/// What a listener is told about; read the details with [snapshot].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// A stage was reached for the first time.
    Stage,
    /// An error was recorded.
    Error,
}

/// The timeline at one moment.
#[derive(Clone, PartialEq, Debug)]
pub struct Snapshot {
    /// Every stage in order, with the time it was first reached.
    pub stages: Vec<(Stage, Option<Duration>)>,
    /// Time of this snapshot, on the same clock as the stages.
    pub now: Duration,
    /// Whether the clock counts from kernel boot (otherwise from app start).
    pub since_boot: bool,
    pub errors: u32,
    pub last_error: Option<String>,
}

struct Timeline {
    /// Process clock reading when the timeline was created.
    origin: Instant,
    /// Time since kernel boot at `origin`, if the system exposes it.
    origin_since_boot: Option<Duration>,
    marks: [Option<Duration>; Stage::ALL.len()],
    errors: u32,
    last_error: Option<String>,
}

impl Timeline {
    fn new(origin: Instant, origin_since_boot: Option<Duration>) -> Self {
        Self {
            origin,
            origin_since_boot,
            marks: [None; Stage::ALL.len()],
            errors: 0,
            last_error: None,
        }
    }

    fn at(&self, now: Instant) -> Duration {
        self.origin_since_boot.unwrap_or_default() + now.saturating_duration_since(self.origin)
    }

    fn clock(&self) -> &'static str {
        match self.origin_since_boot {
            Some(_) => "since boot",
            None => "since app start",
        }
    }

    /// Records `stage` at `now` if it was not reached before, and returns the line to log.
    fn mark(&mut self, stage: Stage, now: Instant) -> Option<String> {
        if self.marks[stage as usize].is_some() {
            return None;
        }

        let at = self.at(now);
        self.marks[stage as usize] = Some(at);

        Some(match stage {
            Stage::FirstFrame => self.summary(),
            _ => format!("connect_timeline: {} at {} {}", stage.name(), secs(at), self.clock()),
        })
    }

    fn record_error(&mut self, error: &str) {
        self.errors = self.errors.saturating_add(1);
        self.last_error = Some(error.chars().take(MAX_ERROR_CHARS).collect());
    }

    fn snapshot(&self, now: Instant) -> Snapshot {
        Snapshot {
            stages: Stage::ALL
                .iter()
                .map(|stage| (*stage, self.marks[*stage as usize]))
                .collect(),
            now: self.at(now),
            since_boot: self.origin_since_boot.is_some(),
            errors: self.errors,
            last_error: self.last_error.clone(),
        }
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

/// Errors are kept short: they end up in a one-line journal entry.
const MAX_ERROR_CHARS: usize = 120;

static TIMELINE: Mutex<Option<Timeline>> = Mutex::new(None);
static LISTENER: OnceLock<Box<dyn Fn(Change) + Send + Sync>> = OnceLock::new();
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
        notify(Change::Stage);
    }
}

/// Counts an error and keeps its text (shortened) as the last error.
pub fn record_error(error: &dyn Display) {
    let now = Instant::now();
    {
        let Ok(mut timeline) = TIMELINE.lock() else {
            return;
        };
        timeline
            .get_or_insert_with(|| Timeline::new(now, since_boot()))
            .record_error(&error.to_string());
    }
    notify(Change::Error);
}

/// The timeline now, or `None` before anything was marked or recorded.
pub fn snapshot() -> Option<Snapshot> {
    let now = Instant::now();
    TIMELINE
        .lock()
        .ok()?
        .as_ref()
        .map(|timeline| timeline.snapshot(now))
}

/// Registers the one listener that is called after every change. It runs on the caller's thread,
/// so it must not block (hand the work to another thread). Returns false if one was already set.
pub fn set_listener(listener: impl Fn(Change) + Send + Sync + 'static) -> bool {
    LISTENER.set(Box::new(listener)).is_ok()
}

fn notify(change: Change) {
    if let Some(listener) = LISTENER.get() {
        listener(change);
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
    fn snapshot_has_marks_errors_and_now() {
        let origin = Instant::now();
        let mut timeline = Timeline::new(origin, Some(Duration::from_millis(1_000)));
        timeline.mark(Stage::CarUsb, origin + Duration::from_millis(500));
        timeline.record_error("first");
        timeline.record_error(&"x".repeat(500));

        let snap = timeline.snapshot(origin + Duration::from_millis(2_000));
        assert_eq!(snap.now, Duration::from_millis(3_000));
        assert!(snap.since_boot);
        assert_eq!(snap.stages.len(), Stage::ALL.len());
        assert_eq!(snap.stages[1], (Stage::CarUsb, Some(Duration::from_millis(1_500))));
        assert_eq!(snap.stages[0], (Stage::AppStart, None));
        assert_eq!(snap.errors, 2);
        assert_eq!(snap.last_error.map(|e| e.len()), Some(MAX_ERROR_CHARS), "long errors are shortened");
    }

    #[test]
    fn parses_proc_uptime() {
        assert_eq!(parse_uptime("12.50 40.10\n"), Some(Duration::from_millis(12_500)));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("abc 1.0"), None);
    }
}
