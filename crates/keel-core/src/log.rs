//! Keel's log: one file in the app's data folder for what would otherwise be lost, such as an
//! error nothing can act on, or a panic. Only ever added to. When it gets big it becomes the older
//! file, replacing the one before, so there are never more than two.
//!
//! What's written goes through the same hiding of secrets as what Keel shows, since an error can
//! quote what git, `gh` or an agent said.

use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::mask_secrets;

const FILE: &str = "keel.log";
const OLDER_FILE: &str = "keel.1.log";
/// Each of the two files stays under this.
const MAX_BYTES: u64 = 1024 * 1024;
/// How much of one message is written. An error can carry everything a command printed, and one
/// of those must not fill the file on its own.
const MAX_MESSAGE_BYTES: usize = 8 * 1024;

/// One writer at a time in this process, so two can't both decide the file is full.
static WRITING: Mutex<()> = Mutex::new(());

/// Where the log is. Cheap to make: it holds nothing open.
#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
    max_bytes: u64,
}

impl Log {
    /// The log of the Keel whose data is in `data_dir`.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(FILE),
            max_bytes: MAX_BYTES,
        }
    }

    /// The same log, becoming the older file at another size.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was written before the file last got full.
    pub fn older_path(&self) -> PathBuf {
        self.path.with_file_name(OLDER_FILE)
    }

    /// Adds `message` with the time on each of its lines, and its secrets hidden. Never fails:
    /// there is nowhere left to say that the log can't be written.
    pub fn write(&self, message: &str) {
        self.add(message, true);
    }

    /// A result its caller goes on without. A failure is written with what was being done, since
    /// nothing else would say it happened.
    pub fn noted<T, E: Display>(&self, what: &str, result: Result<T, E>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.write(&format!("{what}: {error}"));
                None
            }
        }
    }

    fn add(&self, message: &str, wait: bool) {
        let time = timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        );
        // Hidden first, then cut: a secret cut in half wouldn't be recognised as one.
        let text: String = cut(&mask_secrets(message))
            .lines()
            .map(|line| format!("{time} {line}\n"))
            .collect();
        let writing = writing(wait);
        // Only whoever has the turn moves the file aside, or two could each do it and lose the
        // older one. Without the turn this only adds, and the file may end a message over.
        let _ = self.append(text.as_bytes(), writing.is_some());
    }

    fn append(&self, text: &[u8], may_move_aside: bool) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let size = std::fs::metadata(&self.path).map_or(0, |file| file.len());
        if may_move_aside && size > 0 && size + text.len() as u64 > self.max_bytes {
            std::fs::rename(&self.path, self.older_path())?;
        }
        // Private: an error can name the person's folders and repos.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(text)
    }
}

/// The start of a message too long to write whole, on a character's edge, saying how much there
/// was.
fn cut(message: &str) -> std::borrow::Cow<'_, str> {
    if message.len() <= MAX_MESSAGE_BYTES {
        return message.into();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n(cut here: the message was {} bytes)",
        &message[..end],
        message.len()
    )
    .into()
}

/// A panic can come while this thread is writing, and then waiting would never end. So a panic
/// doesn't wait for the turn: see `Log::add`.
fn writing(wait: bool) -> Option<MutexGuard<'static, ()>> {
    match WRITING.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) if wait => {
            Some(WRITING.lock().unwrap_or_else(|e| e.into_inner()))
        }
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Has a panic leave what it said in the log, then go on as before. A release build ends right
/// after a panic, with nothing on screen, so this is all that says why Keel closed. A debug build
/// only loses the thread, so what's written says there was a panic, not that Keel stopped.
pub fn log_panics(log: Log) {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        log.add(&format!("A panic in thread {name}: {info}"), false);
        before(info);
    }));
}

/// UTC, as `2026-11-19T15:04:05.123Z`: the same shape as the times in the database.
fn timestamp(since_epoch: Duration) -> String {
    let seconds = since_epoch.as_secs();
    let (days, in_day) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Days since 1970 as a date, counting from 1 March so a leap day comes last in its year.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600,
        in_day % 3_600 / 60,
        in_day % 60,
        since_epoch.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::MASK;

    fn log() -> (tempfile::TempDir, Log) {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::in_dir(dir.path());
        (dir, log)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Whether a line starts with a time like `2026-11-19T15:04:05.123Z `.
    fn has_time(line: &str) -> bool {
        let Some((time, _)) = line.split_once(' ') else {
            return false;
        };
        let shape = "0000-00-00T00:00:00.000Z";
        time.len() == shape.len()
            && time
                .chars()
                .zip(shape.chars())
                .all(|(found, expected)| match expected {
                    '0' => found.is_ascii_digit(),
                    other => found == other,
                })
    }

    #[test]
    fn what_is_written_is_added_to_the_file_with_the_time_on_every_line() {
        let (_dir, log) = log();
        log.write("first");
        log.write("second\nand its detail");

        let text = read(log.path());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines.iter().all(|line| has_time(line)), "{text}");
        assert!(lines[0].ends_with(" first"));
        assert!(lines[1].ends_with(" second"));
        assert!(lines[2].ends_with(" and its detail"));
    }

    #[test]
    fn secrets_are_hidden_in_the_log_as_in_what_keel_shows() {
        let (_dir, log) = log();
        log.write("git said: API_KEY=made-up-Value-123 is not valid");

        let text = read(log.path());
        assert!(!text.contains("made-up-Value-123"), "{text}");
        assert!(text.contains(MASK), "{text}");
        assert!(text.contains("git said"), "{text}");
    }

    #[test]
    fn a_full_file_becomes_the_older_one_and_only_one_older_is_kept() {
        let (dir, log) = log();
        let log = log.with_max_bytes(200);
        let line = "x".repeat(60);
        log.write(&format!("one {line}"));
        log.write(&format!("two {line}"));
        assert!(!log.older_path().exists(), "it still fits");

        log.write(&format!("three {line}"));
        assert!(read(&log.older_path()).contains(" one "));
        assert!(read(&log.older_path()).contains(" two "));
        assert!(read(log.path()).contains(" three "));
        assert!(!read(log.path()).contains(" one "));

        for round in 0..6 {
            log.write(&format!("more-{round} {line}"));
        }
        assert!(read(log.path()).contains("more-5"));
        let both = read(log.path()) + &read(&log.older_path());
        assert!(!both.contains(" one "), "the oldest is gone");
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 2);
        for file in [log.path().to_owned(), log.older_path()] {
            assert!(std::fs::metadata(file).unwrap().len() <= 200);
        }
    }

    #[test]
    fn a_message_too_long_is_cut_on_a_character_and_says_so() {
        let (_dir, log) = log();
        // Two bytes each, so the limit falls in the middle of one.
        let long = format!("x{}", "ñ".repeat(MAX_MESSAGE_BYTES));
        log.write(&format!("git said: {long}"));
        log.write("and a short one after it");

        let text = read(log.path());
        assert!(text.len() < MAX_MESSAGE_BYTES + 300, "{}", text.len());
        assert!(text.contains(" git said: xññ"), "it keeps how it starts");
        let said = format!("(cut here: the message was {} bytes)", long.len() + 10);
        assert!(text.contains(&said), "{}", &text[text.len() - 200..]);
        assert!(text.lines().all(has_time));
        assert!(text.ends_with(" and a short one after it\n"));
    }

    #[test]
    fn a_panic_that_cant_have_the_turn_still_writes_and_leaves_the_older_file_alone() {
        let (_dir, log) = log();
        let log = log.with_max_bytes(200);
        let line = "x".repeat(60);
        log.write(&format!("one {line}"));
        log.write(&format!("two {line}"));
        log.write(&format!("three {line}"));
        let older = read(&log.older_path());

        // Another thread is in the middle of writing, as when a panic comes during a write.
        let turn = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        log.add(&format!("panic-1 {line}"), false);
        log.add(&format!("panic-2 {line}"), false);
        drop(turn);

        assert_eq!(read(&log.older_path()), older);
        let text = read(log.path());
        assert!(
            text.contains(" three ") && text.contains(" panic-1 ") && text.contains(" panic-2 ")
        );
        // The next write with the turn moves it aside as usual.
        log.write("after");
        assert!(read(&log.older_path()).contains(" panic-2 "));
        assert!(read(log.path()).ends_with(" after\n"));
    }

    #[test]
    fn the_log_can_only_be_read_by_the_person() {
        let (_dir, log) = log();
        log.write("something");
        let mode = std::fs::metadata(log.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_failure_its_caller_goes_on_without_is_written_down() {
        let (_dir, log) = log();
        let kept: Option<u8> = log.noted("keeping how step a1 ended", Ok::<u8, String>(3));
        assert_eq!(kept, Some(3));
        assert_eq!(read(log.path()), "");

        let lost: Option<u8> = log.noted("keeping how step a1 ended", Err("disk is full"));
        assert_eq!(lost, None);
        assert!(read(log.path()).contains("keeping how step a1 ended: disk is full"));
    }

    #[test]
    fn a_log_whose_folder_cant_be_written_is_given_up_on_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        Log::in_dir(&file).write("nowhere to go");
    }

    #[test]
    fn a_panic_leaves_what_it_said_in_the_log() {
        let (_dir, log) = log();
        // Put back afterwards, so no other test's panic is written where this folder was.
        let original = std::panic::take_hook();
        log_panics(log.clone());

        let ended = std::panic::catch_unwind(|| panic!("made-up failure, TOKEN=made-up-Value-123"));

        drop(std::panic::take_hook());
        std::panic::set_hook(original);
        assert!(ended.is_err());
        let text = read(log.path());
        assert!(text.contains("A panic in thread"), "{text}");
        assert!(text.contains("made-up failure"), "{text}");
        assert!(text.contains("log.rs"), "it says where: {text}");
        assert!(!text.contains("made-up-Value-123"), "{text}");
        assert!(text.lines().all(has_time), "{text}");
    }

    #[test]
    fn times_are_utc_dates() {
        assert_eq!(timestamp(Duration::ZERO), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            timestamp(Duration::from_millis(1_700_000_000_123)),
            "2023-11-14T22:13:20.123Z"
        );
        // A leap day, and the days around the end of a year.
        assert_eq!(
            timestamp(Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            timestamp(Duration::from_secs(1_735_689_599)),
            "2024-12-31T23:59:59.000Z"
        );
        assert_eq!(
            timestamp(Duration::from_secs(1_735_689_600)),
            "2025-01-01T00:00:00.000Z"
        );
    }
}
