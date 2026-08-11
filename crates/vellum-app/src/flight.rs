//! The flight recorder: what the application was doing just before it died.
//!
//! # Why this exists
//!
//! Velm has twice run the user's machine out of memory — **14.24 GB**, then **15.06
//! GB** — and neither occurrence has ever been reproduced. Every unattended run sits
//! flat between 0.15 and 0.35 GB. The difference is sustained interaction, and the one
//! thing an unattended run cannot do is be the user.
//!
//! A log does not survive that. The session ends in *Force Quit* — `SIGKILL`, no
//! unwinding, no `Drop`, no flush — so anything still in a buffer is lost, and the
//! evidence that matters is exactly the last few seconds before the kill.
//!
//! So this writes **plain text, one line at a time, flushed on every line**, to a file
//! per session. A killed process loses at most the line in flight. The next start reads
//! the previous session's file, notices it has no `EXIT` marker, and reports how it
//! ended — see [`Recorder::previous_session`].
//!
//! # What it records
//!
//! - A `SAMPLE` every second: resident memory and the size of every cache that can
//!   grow with use, so a climb names a subsystem instead of a process.
//! - An `EVENT` whenever the user does something that allocates — paste, import, open a
//!   board, switch a tab, place an item. The last events before a runaway are the
//!   reproduction steps, which is the thing that has been missing.
//! - An `ALARM` the first time resident memory crosses each power-of-two gigabyte, with
//!   the full breakdown at that instant. A leak is a *rate*, and the alarms are what
//!   make the rate visible after the fact.
//!
//! # Deliberately not JSON
//!
//! The reader is a human — the user, pasting the tail of a file into a bug report. A
//! fixed-column text line is greppable with no tooling, and this file is the one thing
//! in the app that has to work when everything else has gone wrong.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many session files to keep. Enough to cover "it happened a few runs ago"
/// without the directory becoming its own storage problem — each is a few KB.
const KEEP_SESSIONS: usize = 12;

/// Everything the recorder samples once a second.
///
/// A struct rather than a dozen arguments because the call site is the frame loop and
/// the order of a dozen `usize`s is exactly the kind of thing that silently swaps two
/// columns and makes the record lie.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub rss: u64,
    pub texture_bytes: usize,
    pub texture_peak: usize,
    pub textures: usize,
    pub glyph_bitmaps: usize,
    pub chrome_textures: usize,
    pub previews: usize,
    pub parked_boards: usize,
    pub items: usize,
    pub drawn: usize,
    pub text_layouts: usize,
    pub ink_meshes: usize,
    pub undo_depth: usize,
    pub fps: f64,
    pub zoom: f64,
}

/// Appends to one session's record and keeps the peak seen.
pub struct Recorder {
    file: Option<std::fs::File>,
    path: PathBuf,
    started: Instant,
    peak_rss: u64,
    /// Gigabytes already alarmed on, so each threshold is reported once.
    alarmed_gb: u64,
    /// The last events, kept in memory so an alarm can print what led up to it.
    recent: std::collections::VecDeque<String>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("path", &self.path)
            .field("peak_rss", &self.peak_rss)
            .finish()
    }
}

/// How many recent events an alarm reprints.
const RECENT_EVENTS: usize = 24;

impl Recorder {
    /// Opens a record for this session under `<data>/diagnostics/`.
    ///
    /// Never fails the application: a recorder that cannot write is a recorder that
    /// records nothing, which is strictly better than an app that will not start
    /// because its diagnostics directory is read-only.
    pub fn open(directory: &Path) -> Self {
        let dir = directory.join("diagnostics");
        let path = dir.join(format!("session-{}.log", stamp_for_filename()));

        let file = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(&path))
            .map_err(|error| {
                log::warn!("no flight recorder: {} ({error})", path.display());
                error
            })
            .ok();

        let mut recorder = Self {
            file,
            path,
            started: Instant::now(),
            peak_rss: 0,
            alarmed_gb: 0,
            recent: std::collections::VecDeque::new(),
        };
        recorder.write_line(&format!(
            "START   {} pid={} version={}",
            wall_clock(),
            std::process::id(),
            env!("CARGO_PKG_VERSION")
        ));
        recorder
    }

    /// Where this session is being recorded, for a message to the user.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn peak_rss(&self) -> u64 {
        self.peak_rss
    }

    /// Notes something the user did. Keep these short and stable — they are read as a
    /// reproduction script, so `paste` and `open-board` beat prose.
    pub fn event(&mut self, what: &str) {
        let line = format!("EVENT   {:>8.2}s {what}", self.started.elapsed().as_secs_f64());
        if self.recent.len() == RECENT_EVENTS {
            self.recent.pop_front();
        }
        self.recent.push_back(format!(
            "{:>8.2}s {what}",
            self.started.elapsed().as_secs_f64()
        ));
        self.write_line(&line);
    }

    /// Records one second's worth of memory, and alarms if it has crossed a gigabyte
    /// it has not crossed before.
    pub fn sample(&mut self, sample: Sample) {
        self.peak_rss = self.peak_rss.max(sample.rss);

        let mut line = String::with_capacity(220);
        let _ = write!(
            line,
            "SAMPLE  {:>8.2}s rss={}M tex={}M/{}M×{} glyphs={} chrome={} previews={} \
             parked={} items={} drawn={} layouts={} ink={} undo={} fps={:.0} zoom={:.0}%",
            self.started.elapsed().as_secs_f64(),
            sample.rss / 1_048_576,
            sample.texture_bytes / 1_048_576,
            sample.texture_peak / 1_048_576,
            sample.textures,
            sample.glyph_bitmaps,
            sample.chrome_textures,
            sample.previews,
            sample.parked_boards,
            sample.items,
            sample.drawn,
            sample.text_layouts,
            sample.ink_meshes,
            sample.undo_depth,
            sample.fps,
            sample.zoom * 100.0,
        );
        self.write_line(&line);

        // One alarm per gigabyte crossed. The *first* one is the useful one — it is
        // the closest record to the moment the behaviour changed, while the machine is
        // still responsive enough to be writing files at all.
        let gb = sample.rss / 1_073_741_824;
        if gb >= 1 && gb > self.alarmed_gb {
            self.alarmed_gb = gb;
            self.write_line(&format!(
                "ALARM   {:>8.2}s crossed {gb} GB — budget is 0.4 GB. Last {} events follow.",
                self.started.elapsed().as_secs_f64(),
                self.recent.len()
            ));
            // A distinct prefix, so counting alarms stays a matter of counting lines.
            let recent: Vec<String> = self.recent.iter().cloned().collect();
            for event in recent {
                self.write_line(&format!("LEADUP  {event}"));
            }
            log::error!(
                "resident memory crossed {gb} GB — see {}",
                self.path.display()
            );
        }
    }

    /// Marks a clean shutdown. Its **absence** is what tells the next run that the
    /// session was killed rather than quit.
    pub fn finish(&mut self, reason: &str) {
        self.write_line(&format!(
            "EXIT    {:>8.2}s {reason} peak_rss={}M",
            self.started.elapsed().as_secs_f64(),
            self.peak_rss / 1_048_576
        ));
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }

    /// One line, flushed. Flushing every line is the whole design: a `SIGKILL` gives
    /// no chance to flush later, and a buffered record of a crash is no record.
    fn write_line(&mut self, line: &str) {
        let Some(file) = self.file.as_mut() else { return };
        if writeln!(file, "{line}").and_then(|()| file.flush()).is_err() {
            // A recorder that has started failing will keep failing; stop trying rather
            // than emitting an error per line for the rest of the session.
            self.file = None;
        }
    }
}

/// What the previous session's record says about how it ended.
#[derive(Debug, Clone)]
pub struct PreviousSession {
    pub path: PathBuf,
    /// `false` when the record has no `EXIT` line — the session was killed.
    pub clean: bool,
    /// Highest `rss=` seen in the record, in bytes.
    pub peak_rss: u64,
    /// The last few lines, which is what a report should quote.
    pub tail: Vec<String>,
}

/// Reads the most recent *other* session record, if there is one.
///
/// Called at startup so a force-quit is reported rather than forgotten. Returns `None`
/// when this is the first run, when nothing can be read, or when the only record is the
/// one this session just opened.
pub fn previous_session(directory: &Path, current: &Path) -> Option<PreviousSession> {
    let dir = directory.join("diagnostics");
    let mut records: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path != current && path.extension().is_some_and(|e| e == "log"))
        .collect();
    // The filename carries a sortable timestamp, so this is newest-last.
    records.sort();
    let path = records.pop()?;

    let text = std::fs::read_to_string(&path).ok()?;
    let clean = text.lines().any(|line| line.starts_with("EXIT"));
    let peak_rss = text
        .lines()
        .filter_map(peak_of_line)
        .max()
        .unwrap_or(0);
    let tail: Vec<String> = text
        .lines()
        .rev()
        .take(12)
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Some(PreviousSession { path, clean, peak_rss, tail })
}

/// Pulls `rss=<n>M` out of a sample line, in bytes.
fn peak_of_line(line: &str) -> Option<u64> {
    let rest = line.split_once(" rss=")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok().map(|mb| mb * 1_048_576)
}

/// Deletes all but the newest [`KEEP_SESSIONS`] records.
pub fn prune(directory: &Path) {
    let dir = directory.join("diagnostics");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut records: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "log"))
        .collect();
    if records.len() <= KEEP_SESSIONS {
        return;
    }
    records.sort();
    let doomed = records.len() - KEEP_SESSIONS;
    for path in records.into_iter().take(doomed) {
        let _ = std::fs::remove_file(path);
    }
}

/// Seconds since the epoch, zero-padded, so filenames sort chronologically as strings.
///
/// The trailing counter is not decoration. Two recorders opened in the same second in
/// the same process would otherwise land on the same filename, which made
/// [`previous_session`] — whose whole job is to find the file that is *not* the current
/// one — report that there had never been a previous session.
fn stamp_for_filename() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{secs:014}-{}-{sequence:04}", std::process::id())
}

/// Seconds since the epoch, for the `START` line. Not a formatted date: turning one
/// into a calendar without a dependency is more code than this file deserves, and the
/// elapsed column is what actually gets read.
fn wall_clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("epoch={secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_killed_session_is_reported_as_unclean_with_its_peak() {
        let home = tempfile::tempdir().unwrap();

        // A session that ends without `finish` — a force quit.
        let mut recorder = Recorder::open(home.path());
        recorder.event("paste");
        recorder.sample(Sample { rss: 300 * 1_048_576, ..Sample::default() });
        recorder.sample(Sample { rss: 9_000 * 1_048_576, ..Sample::default() });
        let killed = recorder.path().to_path_buf();
        drop(recorder);

        // The next start opens its own record and reads the one before it.
        let next = Recorder::open(home.path());
        let previous = previous_session(home.path(), next.path()).expect("a previous session");

        assert_eq!(previous.path, killed);
        assert!(!previous.clean, "a session with no EXIT line was reported clean");
        assert_eq!(previous.peak_rss, 9_000 * 1_048_576);
        assert!(
            previous.tail.iter().any(|line| line.contains("paste")),
            "the tail lost the events that led up to it: {:?}",
            previous.tail
        );
    }

    #[test]
    fn a_clean_session_is_reported_clean() {
        let home = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::open(home.path());
        recorder.sample(Sample { rss: 200 * 1_048_576, ..Sample::default() });
        recorder.finish("quit");
        drop(recorder);

        let next = Recorder::open(home.path());
        let previous = previous_session(home.path(), next.path()).expect("a previous session");
        assert!(previous.clean);
    }

    /// The alarm is the point of the whole file: crossing a gigabyte must be recorded
    /// *with what led up to it*, once per gigabyte rather than once a second.
    #[test]
    fn crossing_a_gigabyte_alarms_once_with_the_recent_events() {
        let home = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::open(home.path());
        recorder.event("import-miro");
        recorder.event("zoom");

        recorder.sample(Sample { rss: 1_500 * 1_048_576, ..Sample::default() });
        recorder.sample(Sample { rss: 1_600 * 1_048_576, ..Sample::default() });
        recorder.sample(Sample { rss: 2_200 * 1_048_576, ..Sample::default() });
        let path = recorder.path().to_path_buf();
        recorder.finish("quit");

        let text = std::fs::read_to_string(&path).unwrap();
        let alarms: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("ALARM"))
            .collect();
        assert_eq!(alarms.len(), 2, "expected one alarm per gigabyte: {alarms:?}");
        assert!(alarms[0].contains("crossed 1 GB"));
        assert!(alarms[1].contains("crossed 2 GB"));
        assert!(
            text.contains("import-miro"),
            "the alarm did not carry the events that preceded it"
        );
    }

    #[test]
    fn the_peak_parser_reads_a_sample_line() {
        let mut recorder = Recorder::open(tempfile::tempdir().unwrap().path());
        recorder.sample(Sample { rss: 1_234 * 1_048_576, ..Sample::default() });
        // Reparsing our own format is the property that matters: a change to the line
        // that the parser does not follow makes every peak read as zero.
        let line = format!("SAMPLE  {:>8.2}s rss={}M tex=0M/0M×0", 1.0, 1_234);
        assert_eq!(peak_of_line(&line), Some(1_234 * 1_048_576));
        assert_eq!(peak_of_line("START   epoch=1"), None);
    }

    #[test]
    fn old_records_are_pruned_but_the_newest_are_kept() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("diagnostics");
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..KEEP_SESSIONS + 5 {
            std::fs::write(dir.join(format!("session-{index:014}-1.log")), "START\n").unwrap();
        }

        prune(home.path());

        let left: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(left.len(), KEEP_SESSIONS);
        assert!(
            left.iter().any(|p| p.to_string_lossy().contains(&format!("{:014}", KEEP_SESSIONS + 4))),
            "pruning kept the oldest instead of the newest"
        );
    }
}
