//! Boot journal: one small file per boot under `<persist_dir>/journal/`, so what happened in the car survives
//! the power cut that wipes the RAM log (`/var/volatile/log`).
//!
//! Each file holds a single line built from the connect timeline: when each stage was reached, the error
//! count, the last error and how long CatPlay stayed up. `cat /persist/journal/*` lists one boot per line,
//! oldest first. Only the newest [KEEP] boots are kept.
//!
//! The persist partition is NOR flash that also holds the Bluetooth pairing keys, so writes are small,
//! rate limited, and atomic: a temporary file is written and synced, then renamed over the old entry.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

use catplay_util::timeline::{self, Change, Snapshot};
use log::warn;

const DIR_NAME: &str = "journal";
const PREFIX: &str = "boot-";
const SUFFIX: &str = ".txt";
const TMP_NAME: &str = ".boot.tmp";
/// Boots kept on flash; each entry is a few hundred bytes.
const KEEP: usize = 100;
/// While nothing happens, refresh the entry this often so it shows how long the dongle stayed up.
const ALIVE_EVERY: Duration = Duration::from_secs(300);
/// Errors alone are written at most this often, so an error loop cannot wear out the flash.
const ERROR_EVERY: Duration = Duration::from_secs(60);

pub struct BootJournal {
    path: PathBuf,
    seq: u64,
}

impl BootJournal {
    /// Creates `<persist_dir>/journal/`, picks the next boot number and removes the oldest entries.
    pub fn open(persist_dir: &Path) -> io::Result<Self> {
        let dir = persist_dir.join(DIR_NAME);
        fs::create_dir_all(&dir)?;
        // Left behind if the power was cut between writing and renaming.
        let _ = fs::remove_file(dir.join(TMP_NAME));

        let mut seqs = existing_seqs(&dir)?;
        seqs.sort_unstable();
        let seq = seqs.last().map_or(1, |last| last + 1);

        // Keep KEEP - 1 old entries, plus the one for this boot.
        let excess = seqs.len().saturating_sub(KEEP - 1);
        for old in &seqs[..excess] {
            let _ = fs::remove_file(dir.join(file_name(*old)));
        }

        Ok(Self {
            path: dir.join(file_name(seq)),
            seq,
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Replaces this boot's entry atomically.
    pub fn write(&self, line: &str) -> io::Result<()> {
        let tmp = self.path.with_file_name(TMP_NAME);
        let mut file = File::create(&tmp)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)
    }
}

fn file_name(seq: u64) -> String {
    format!("{PREFIX}{seq:06}{SUFFIX}")
}

fn existing_seqs(dir: &Path) -> io::Result<Vec<u64>> {
    let mut seqs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(seq) = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
            .and_then(|digits| digits.parse().ok())
        {
            seqs.push(seq);
        }
    }
    Ok(seqs)
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// One journal line: `boot=7 version=0.1.0 up=312.0s app_start=6.1s car_usb=17.5s … errors=1 last_error="…"`.
pub fn render(seq: u64, version: &str, snap: &Snapshot) -> String {
    let mut parts = vec![
        format!("boot={seq}"),
        format!("version={version}"),
        format!("up={}", secs(snap.now)),
    ];
    for (stage, at) in &snap.stages {
        parts.push(format!("{}={}", stage.name(), at.map_or_else(|| "-".to_string(), secs)));
    }
    parts.push(format!("errors={}", snap.errors));
    if let Some(error) = &snap.last_error {
        parts.push(format!("last_error=\"{}\"", error.replace(['"', '\n', '\r'], "'")));
    }
    if !snap.since_boot {
        parts.push("clock=app".to_string());
    }
    parts.join(" ")
}

/// Opens this boot's entry and keeps it up to date from a background thread, following the connect timeline.
/// Returns the boot number.
pub fn start(persist_dir: &Path) -> io::Result<u64> {
    let journal = BootJournal::open(persist_dir)?;
    let seq = journal.seq();

    let (tx, rx) = mpsc::channel();
    if !timeline::set_listener(move |change| {
        let _ = tx.send(change);
    }) {
        return Err(io::Error::other("the timeline already has a listener"));
    }

    std::thread::Builder::new()
        .name("boot-journal".into())
        .spawn(move || run(journal, rx))?;
    Ok(seq)
}

fn run(journal: BootJournal, rx: mpsc::Receiver<Change>) {
    let flush = || {
        let Some(snap) = timeline::snapshot() else { return };
        if let Err(err) = journal.write(&render(journal.seq(), env!("CARGO_PKG_VERSION"), &snap)) {
            warn!("Boot journal: write failed: {err}");
        }
    };

    flush();
    let mut last_write = Instant::now();
    let mut error_pending = false;

    loop {
        let wait = match error_pending {
            true => ERROR_EVERY.saturating_sub(last_write.elapsed()),
            false => ALIVE_EVERY,
        };
        match rx.recv_timeout(wait) {
            // New stages are written right away: the power may be cut any moment.
            Ok(Change::Stage) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Change::Error) if last_write.elapsed() < ERROR_EVERY => {
                error_pending = true;
                continue;
            }
            Ok(Change::Error) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        flush();
        last_write = Instant::now();
        error_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_util::timeline::Stage;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("catplay-boot-journal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn snapshot(errors: u32, last_error: Option<&str>, since_boot: bool) -> Snapshot {
        Snapshot {
            stages: vec![
                (Stage::AppStart, Some(Duration::from_millis(6_120))),
                (Stage::CarUsb, Some(Duration::from_millis(17_460))),
                (Stage::PhoneWifi, None),
            ],
            now: Duration::from_secs(312),
            since_boot,
            errors,
            last_error: last_error.map(str::to_string),
        }
    }

    #[test]
    fn renders_one_line_per_boot() {
        assert_eq!(
            render(7, "0.1.0", &snapshot(0, None, true)),
            "boot=7 version=0.1.0 up=312.0s app_start=6.1s car_usb=17.5s phone_wifi=- errors=0"
        );
        assert_eq!(
            render(8, "0.1.0", &snapshot(2, Some("tx: \"bad\"\nthing"), false)),
            "boot=8 version=0.1.0 up=312.0s app_start=6.1s car_usb=17.5s phone_wifi=- errors=2 last_error=\"tx: 'bad''thing\" clock=app"
        );
    }

    #[test]
    fn numbers_boots_and_keeps_the_newest() {
        let persist = temp_dir("rotate");
        let dir = persist.join(DIR_NAME);
        fs::create_dir_all(&dir).unwrap();
        for seq in 1..=105 {
            fs::write(dir.join(file_name(seq)), "old\n").unwrap();
        }
        fs::write(dir.join("notes.txt"), "not an entry\n").unwrap();
        fs::write(dir.join(TMP_NAME), "half written").unwrap();

        let journal = BootJournal::open(&persist).unwrap();
        assert_eq!(journal.seq(), 106);

        let mut left = existing_seqs(&dir).unwrap();
        left.sort_unstable();
        assert_eq!(left.len(), KEEP - 1, "room is left for this boot's entry");
        assert_eq!(left.first(), Some(&7));
        assert!(dir.join("notes.txt").exists(), "unrelated files are left alone");
        assert!(!dir.join(TMP_NAME).exists(), "a stale temporary file is removed");
        fs::remove_dir_all(&persist).unwrap();
    }

    #[test]
    fn starts_at_one_and_rewrites_atomically() {
        let persist = temp_dir("write");
        let journal = BootJournal::open(&persist).unwrap();
        assert_eq!(journal.seq(), 1);

        journal.write("boot=1 first").unwrap();
        journal.write("boot=1 second").unwrap();

        let dir = persist.join(DIR_NAME);
        assert_eq!(fs::read_to_string(dir.join(file_name(1))).unwrap(), "boot=1 second\n");
        assert!(!dir.join(TMP_NAME).exists());
        assert_eq!(BootJournal::open(&persist).unwrap().seq(), 2, "the next boot gets the next number");
        fs::remove_dir_all(&persist).unwrap();
    }
}
