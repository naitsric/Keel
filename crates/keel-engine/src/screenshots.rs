//! Pictures a step left of how the change looks, for the person who approves it. A step saves them
//! in a folder of the run's private copy that git ignores, so they never end up in a commit. Keel
//! empties that folder before each step that can write to it, and when the step ends it takes
//! what's there out of the copy and keeps it with the run. So each picture belongs to the step it
//! came after, and nothing a later step does can change it. Nothing here takes a picture.
//!
//! Keel reads the folder outside the sandbox, while something the step started could still be
//! changing it. So the folder is opened once, without following a link, and each file in it is
//! opened through that, without following one either: what's read is what was opened.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use keel_store::Attempt;
use serde::{Deserialize, Serialize, Serializer};

/// The folder, inside the run's private copy.
pub const FOLDER: &str = ".keel-screenshots";
/// What tells a command where that folder is.
pub const VARIABLE: &str = "KEEL_SCREENSHOTS";
/// The event that says what Keel took from the folder when a step ended.
pub const EVENT: &str = "screenshots";
// The approval says these two in words, in `run.screenshotsLeftOut`.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SHOWN: usize = 8;
/// Names of what isn't shown that are listed. Any more are only counted.
const MAX_LISTED: usize = 20;
/// Names in a step's folder that are read to choose from. Any more are only counted.
const MAX_NAMES: usize = 1000;

/// A picture a step saved, as its approval shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshot {
    pub name: String,
    /// What its first bytes say it is, whatever its name says.
    pub mime: &'static str,
    #[serde(serialize_with = "as_base64")]
    pub data: Vec<u8>,
    /// The step it was saved during.
    pub step: String,
    /// A step that changed the code ended with that one or after it, so it may not show what's
    /// being approved.
    pub before_last_change: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshots {
    /// This round's pictures, the latest step's first.
    pub shown: Vec<Screenshot>,
    /// What else this round's steps saved and isn't shown: not a picture Keel shows, too big, a
    /// link, or one past the limit. Nothing is left out without saying so.
    pub left_out: Vec<String>,
    /// How many more were left out than `left_out` names.
    pub more_left_out: usize,
}

/// What Keel took from the folder when a step ended, as the step's event keeps it. Each picture
/// kept is a copy with the run, named by its place in `kept`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Taken {
    pub kept: Vec<Kept>,
    pub left_out: Vec<String>,
    pub more_left_out: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kept {
    pub name: String,
    pub mime: String,
}

fn as_base64<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(data))
}

/// The first names of what's left out, and a count of the rest.
#[derive(Default)]
struct LeftOut {
    names: Vec<String>,
    more: usize,
}

impl LeftOut {
    fn add(&mut self, name: String) {
        if self.names.len() < MAX_LISTED {
            self.names.push(name);
        } else {
            self.more += 1;
        }
    }
}

pub fn folder(workspace: &Path) -> PathBuf {
    workspace.join(FOLDER)
}

/// Where a run keeps what it took: beside its private copy, where no step can reach.
fn kept_root(run_dir: &Path) -> PathBuf {
    run_dir.join("screenshots")
}

fn copy_path(run_dir: &Path, attempt_id: &str, index: usize) -> PathBuf {
    kept_root(run_dir).join(attempt_id).join(index.to_string())
}

/// Before a step that can write to the folder. Whatever is there goes, so anything there when the
/// step ends was saved during it.
pub fn empty(workspace: &Path, run_dir: &Path) -> io::Result<()> {
    if let Some(aside) = move_aside(workspace, run_dir)? {
        discard(&aside);
    }
    std::fs::create_dir(folder(workspace))
}

/// When that step ends. Keeps a copy of each picture it saved that Keel shows, and says what else
/// was there. The folder leaves the run's copy, and the next step that can write gets an empty one.
/// `None` when the step saved nothing.
pub fn take(workspace: &Path, run_dir: &Path, attempt_id: &str) -> io::Result<Option<Taken>> {
    let Some(aside) = move_aside(workspace, run_dir)? else {
        return Ok(None);
    };
    let taken = keep(&aside, &kept_root(run_dir).join(attempt_id));
    discard(&aside);
    taken
}

/// Moves whatever has the folder's name, a folder or not, out of the run's copy. A rename never
/// follows a link, and no step can reach where it goes.
fn move_aside(workspace: &Path, run_dir: &Path) -> io::Result<Option<PathBuf>> {
    let root = kept_root(run_dir);
    std::fs::create_dir_all(&root)?;
    let aside = root.join(format!(".{}", uuid::Uuid::new_v4()));
    match std::fs::rename(folder(workspace), &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// As far as it goes: it's out of the run's copy, and nothing reads it again.
fn discard(path: &Path) {
    let _ = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => Ok(()),
    };
}

/// Copies the pictures Keel shows from a step's folder into `into`, by name, up to the limit, and
/// lists the rest. Nothing past the limit is opened.
fn keep(path: &Path, into: &Path) -> io::Result<Option<Taken>> {
    let Ok(folder) = Folder::open(path) else {
        // A link or a file where the folder was. Nothing behind it is the step's to show.
        return Ok(Some(Taken {
            left_out: vec![FOLDER.to_owned()],
            ..Taken::default()
        }));
    };
    let (names, unread) = folder.names()?;
    if names.is_empty() && unread == 0 {
        return Ok(None);
    }
    let mut kept = Vec::new();
    let mut left_out = LeftOut {
        names: Vec::new(),
        more: unread,
    };
    for name in names {
        let shown_as = String::from_utf8_lossy(name.to_bytes()).into_owned();
        let found = if kept.len() < MAX_SHOWN {
            folder.file(&name).ok().and_then(picture)
        } else {
            None
        };
        let Some((mime, data)) = found else {
            left_out.add(shown_as);
            continue;
        };
        if kept.is_empty() {
            std::fs::create_dir_all(into)?;
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(into.join(kept.len().to_string()))?
            .write_all(&data)?;
        kept.push(Kept {
            name: shown_as,
            mime: mime.to_owned(),
        });
    }
    Ok(Some(Taken {
        kept,
        left_out: left_out.names,
        more_left_out: left_out.more,
    }))
}

/// A folder opened once, without following a link. Everything in it is reached through that, so
/// swapping the folder or anything in it for a link afterwards changes nothing that's read.
struct Folder(OwnedFd);

impl Folder {
    fn open(path: &Path) -> io::Result<Self> {
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        Ok(Self(opened.into()))
    }

    /// The names in it, in order: the first `MAX_NAMES` it lists, and how many more there are.
    fn names(&self) -> io::Result<(Vec<CString>, usize)> {
        // `fdopendir` takes the descriptor it's given, and closes it with the listing.
        let copy = self.0.try_clone()?.into_raw_fd();
        // SAFETY: `copy` is an open descriptor of a folder, and nothing else owns it.
        let dir = unsafe { libc::fdopendir(copy) };
        if dir.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` didn't take it, so it's still ours to close.
            unsafe { libc::close(copy) };
            return Err(error);
        }
        // SAFETY: `dir` stays open until `closedir` below. A copy shares its place in the listing
        // with the original, so it starts from the top.
        unsafe { libc::rewinddir(dir) };
        let mut names = Vec::new();
        let mut more = 0;
        let ended = loop {
            // Only errno tells the end of the listing from a failure.
            // SAFETY: errno belongs to this thread.
            unsafe { *libc::__error() = 0 };
            // SAFETY: as above.
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break io::Error::last_os_error();
            }
            // SAFETY: an entry's name ends in a zero, and stays valid until the next `readdir`.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            if names.len() < MAX_NAMES {
                names.push(name.to_owned());
            } else {
                more += 1;
            }
        };
        // SAFETY: opened above, closed once.
        unsafe { libc::closedir(dir) };
        if ended.raw_os_error().is_some_and(|code| code != 0) {
            return Err(ended);
        }
        names.sort();
        Ok((names, more))
    }

    /// One file in it, opened without following a link and without waiting on a pipe.
    fn file(&self, name: &CStr) -> io::Result<File> {
        // SAFETY: a name from this folder's own listing, which has no slash in it, opened relative
        // to the folder.
        let opened = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if opened < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just opened, and nothing else owns it.
        Ok(unsafe { File::from_raw_fd(opened) })
    }
}

/// The file, if it's a picture Keel shows. What's checked is what was opened, and its size is
/// known before anything is read.
fn picture(file: File) -> Option<(&'static str, Vec<u8>)> {
    let meta = file.metadata().ok()?;
    // A second name for a file elsewhere would be a way out of the run's copy.
    if !meta.file_type().is_file() || meta.nlink() != 1 || meta.len() > MAX_BYTES {
        return None;
    }
    let mut data = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut data).ok()?;
    if data.len() as u64 > MAX_BYTES {
        return None;
    }
    Some((kind(&data)?, data))
}

/// Only formats a browser shows as a plain picture. SVG can hold a script, so it isn't one.
fn kind(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// What an approval shows: the pictures this round's steps saved, the latest step's first, up to
/// the limit. An earlier round's were for its own approval, so they neither show nor take a place.
/// `taken` is each step's event, by attempt, in the order they happened.
pub fn for_approval(
    run_dir: &Path,
    attempts: &[Attempt],
    round: i64,
    taken: &[(String, Taken)],
) -> Screenshots {
    let mut steps: Vec<(&Attempt, &Taken)> = taken
        .iter()
        .rev()
        .filter_map(|(id, taken)| Some((attempts.iter().find(|a| &a.id == id)?, taken)))
        .filter(|(attempt, _)| attempt.round == round)
        .collect();
    // So a later step's pictures are never the ones past the limit.
    steps.sort_by_key(|(attempt, _)| std::cmp::Reverse(ended(attempt)));
    let mut found = Screenshots::default();
    let mut left_out = LeftOut::default();
    for (attempt, taken) in steps {
        let before_last_change = code_changed_since(attempts, attempt);
        for (index, kept) in taken.kept.iter().enumerate() {
            let copy = if found.shown.len() < MAX_SHOWN {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(copy_path(run_dir, &attempt.id, index))
                    .ok()
                    .and_then(picture)
            } else {
                None
            };
            match copy {
                Some((mime, data)) => found.shown.push(Screenshot {
                    name: kept.name.clone(),
                    mime,
                    data,
                    step: attempt.step_id.clone(),
                    before_last_change,
                }),
                None => left_out.add(kept.name.clone()),
            }
        }
        for name in &taken.left_out {
            left_out.add(name.clone());
        }
        left_out.more += taken.more_left_out;
    }
    found.left_out = left_out.names;
    found.more_left_out = left_out.more;
    found
}

fn ended(attempt: &Attempt) -> Option<DateTime<Utc>> {
    attempt
        .finished_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

fn changed_code(attempt: &Attempt) -> bool {
    attempt
        .files_changed
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .is_some_and(|files| !files.is_empty())
}

/// Whether a step that changed the code ended when the one that saved a picture did, or later. It
/// goes by Keel's own record of when each step ended and what it changed, never by a file's date,
/// which a step can set. The step that saved it counts too: it may have saved it before its last
/// edit.
fn code_changed_since(attempts: &[Attempt], saved_by: &Attempt) -> bool {
    let since = ended(saved_by);
    attempts
        .iter()
        .filter(|attempt| changed_code(attempt))
        .any(|attempt| match (ended(attempt), since) {
            (Some(end), Some(since)) => end >= since,
            _ => true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    /// A run's folder, with its private copy in it.
    fn run() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace(dir.path())).unwrap();
        dir
    }

    fn workspace(run: &Path) -> PathBuf {
        run.join("workspace")
    }

    fn save(run: &Path, name: &str, data: &[u8]) {
        std::fs::write(folder(&workspace(run)).join(name), data).unwrap();
    }

    fn kept_names(taken: &Taken) -> Vec<&str> {
        taken.kept.iter().map(|kept| kept.name.as_str()).collect()
    }

    fn names(shots: &Screenshots) -> Vec<(&str, &str)> {
        shots
            .shown
            .iter()
            .map(|shot| (shot.name.as_str(), shot.step.as_str()))
            .collect()
    }

    #[test]
    fn takes_the_pictures_by_name_and_says_what_it_left_out() {
        let dir = run();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        save(dir.path(), "run.light.png", PNG);
        save(dir.path(), "run.dark.jpg", b"\xFF\xD8\xFF\xE0rest");
        save(dir.path(), "board.webp", b"RIFF\0\0\0\0WEBPVP8 ");
        save(dir.path(), "notes.txt", b"not a picture");
        // Its name says picture, and its bytes don't.
        save(dir.path(), "fake.png", b"<svg onload=alert(1)>");
        save(
            dir.path(),
            "vector.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
        );
        let taken = take(&workspace(dir.path()), dir.path(), "a1")
            .unwrap()
            .unwrap();
        assert_eq!(
            kept_names(&taken),
            ["board.webp", "run.dark.jpg", "run.light.png"]
        );
        assert_eq!(
            taken
                .kept
                .iter()
                .map(|kept| kept.mime.as_str())
                .collect::<Vec<_>>(),
            ["image/webp", "image/jpeg", "image/png"]
        );
        assert_eq!(taken.left_out, ["fake.png", "notes.txt", "vector.svg"]);
        assert_eq!(
            std::fs::read(copy_path(dir.path(), "a1", 2)).unwrap(),
            PNG,
            "kept beside the copy"
        );
        // Out of the run's copy, and nothing else left behind.
        assert!(!folder(&workspace(dir.path())).exists());
        let beside: Vec<_> = std::fs::read_dir(kept_root(dir.path()))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(beside, ["a1"]);

        // The next step starts with an empty one.
        empty(&workspace(dir.path()), dir.path()).unwrap();
        assert_eq!(
            std::fs::read_dir(folder(&workspace(dir.path())))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_step_that_saved_nothing_has_nothing_taken() {
        let dir = run();
        assert_eq!(
            take(&workspace(dir.path()), dir.path(), "a1").unwrap(),
            None
        );
        empty(&workspace(dir.path()), dir.path()).unwrap();
        assert_eq!(
            take(&workspace(dir.path()), dir.path(), "a2").unwrap(),
            None
        );
        assert!(!kept_root(dir.path()).join("a2").exists());
    }

    #[test]
    fn emptying_takes_away_whatever_was_there() {
        let dir = run();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        save(dir.path(), "old.png", PNG);
        std::fs::create_dir(folder(&workspace(dir.path())).join("locked")).unwrap();
        std::fs::set_permissions(
            folder(&workspace(dir.path())).join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        assert_eq!(
            std::fs::read_dir(folder(&workspace(dir.path())))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            take(&workspace(dir.path()), dir.path(), "a1").unwrap(),
            None
        );
        // What couldn't be removed is beside the copy, out of any step's reach. Put back so the
        // test can clean up.
        for entry in std::fs::read_dir(kept_root(dir.path())).unwrap() {
            let locked = entry.unwrap().path().join("locked");
            if locked.exists() {
                std::fs::set_permissions(
                    &locked,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn never_follows_a_link_out_of_the_run() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private.png"), PNG).unwrap();

        let dir = run();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        symlink(
            outside.path().join("private.png"),
            folder(&workspace(dir.path())).join("linked.png"),
        )
        .unwrap();
        std::fs::hard_link(
            outside.path().join("private.png"),
            folder(&workspace(dir.path())).join("hard.png"),
        )
        .unwrap();
        let taken = take(&workspace(dir.path()), dir.path(), "a1")
            .unwrap()
            .unwrap();
        assert!(taken.kept.is_empty(), "{:?}", kept_names(&taken));
        assert_eq!(taken.left_out, ["hard.png", "linked.png"]);
        assert_eq!(
            std::fs::read(outside.path().join("private.png")).unwrap(),
            PNG
        );
    }

    #[test]
    fn a_folder_that_is_a_link_is_never_read_nor_emptied() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private.png"), PNG).unwrap();
        let dir = run();
        symlink(outside.path(), folder(&workspace(dir.path()))).unwrap();
        let taken = take(&workspace(dir.path()), dir.path(), "a1")
            .unwrap()
            .unwrap();
        assert!(taken.kept.is_empty());
        assert_eq!(taken.left_out, [FOLDER]);
        assert!(!kept_root(dir.path()).join("a1").exists());

        // Emptying takes away the link, and nothing it points to.
        symlink(outside.path(), folder(&workspace(dir.path()))).unwrap();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        let made = std::fs::symlink_metadata(folder(&workspace(dir.path()))).unwrap();
        assert!(made.is_dir() && !made.file_type().is_symlink());
        assert_eq!(
            std::fs::read(outside.path().join("private.png")).unwrap(),
            PNG
        );
    }

    /// What a step left running could do between Keel opening the folder and reading it: put a
    /// link to somewhere else where the folder, or a file in it, was.
    #[test]
    fn swapping_in_a_link_after_it_is_opened_changes_nothing_read() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("mine.png"), b"\x89PNG\r\n\x1a\nsecret").unwrap();
        std::fs::write(
            outside.path().join("private.png"),
            b"\x89PNG\r\n\x1a\nsecret",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shots");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("mine.png"), PNG).unwrap();
        std::fs::write(path.join("other.png"), PNG).unwrap();

        let opened = Folder::open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("moved")).unwrap();
        symlink(outside.path(), &path).unwrap();
        let (listed, more) = opened.names().unwrap();
        assert_eq!(listed, [c"mine.png".to_owned(), c"other.png".to_owned()]);
        assert_eq!(more, 0);
        let (_, data) = picture(opened.file(c"mine.png").unwrap()).unwrap();
        assert_eq!(data, PNG);
        assert!(
            opened.file(c"private.png").is_err(),
            "not in the folder opened"
        );

        // A file swapped for a link isn't followed either.
        let moved = dir.path().join("moved");
        std::fs::remove_file(moved.join("other.png")).unwrap();
        symlink(outside.path().join("private.png"), moved.join("other.png")).unwrap();
        assert!(opened.file(c"other.png").is_err());
    }

    #[test]
    fn leaves_out_what_is_too_big_or_past_the_limit_and_says_so() {
        let dir = run();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        let mut big = PNG.to_vec();
        big.resize(MAX_BYTES as usize + 1, 0);
        save(dir.path(), "big.png", &big);
        for index in 0..MAX_SHOWN + 1 {
            save(dir.path(), &format!("view-{index}.png"), PNG);
        }
        let taken = take(&workspace(dir.path()), dir.path(), "a1")
            .unwrap()
            .unwrap();
        assert_eq!(taken.kept.len(), MAX_SHOWN);
        assert_eq!(
            taken.left_out,
            ["big.png".to_owned(), format!("view-{MAX_SHOWN}.png")]
        );
        assert!(!copy_path(dir.path(), "a1", MAX_SHOWN).exists());
    }

    #[test]
    fn names_only_so_many_of_what_it_left_out() {
        let dir = run();
        empty(&workspace(dir.path()), dir.path()).unwrap();
        for index in 0..MAX_LISTED + 5 {
            save(dir.path(), &format!("note-{index:02}.txt"), b"text");
        }
        let taken = take(&workspace(dir.path()), dir.path(), "a1")
            .unwrap()
            .unwrap();
        assert_eq!(taken.left_out.len(), MAX_LISTED);
        assert_eq!(taken.left_out[0], "note-00.txt");
        assert_eq!(taken.more_left_out, 5);
    }

    fn attempt(id: &str, step: &str, round: i64, files: &[&str], finished_at: &str) -> Attempt {
        Attempt {
            id: id.into(),
            run_id: "r".into(),
            step_id: step.into(),
            kind: "agent".into(),
            role: "worker".into(),
            round,
            output: None,
            run_files: None,
            exit_code: None,
            number: 1,
            state: "succeeded".into(),
            reason: None,
            model_requested: String::new(),
            model_actual: None,
            effort: None,
            tools: "build".into(),
            max_turns: None,
            max_budget_usd: None,
            language: "en".into(),
            cli_version: None,
            api_key_source: None,
            account: None,
            prompt: String::new(),
            system_prompt_sources: serde_json::json!([]),
            settings: serde_json::json!({}),
            session_id: String::new(),
            resumed_from: None,
            pid: None,
            pid_started: None,
            workspace: String::new(),
            raw_path: String::new(),
            raw_offset: 0,
            raw_sha256: None,
            diff_path: None,
            sha_before: None,
            sha_after: None,
            files_changed: Some(serde_json::json!(files)),
            result_text: None,
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            turns: None,
            duration_ms: None,
            started_at: String::new(),
            finished_at: Some(finished_at.to_owned()),
            last_activity_at: None,
        }
    }

    /// Runs a step that saves these, the way the runner does. Only a `.png` is a picture.
    fn step(run: &Path, id: &str, saves: &[&str]) -> (String, Taken) {
        empty(&workspace(run), run).unwrap();
        for name in saves {
            let data: &[u8] = if name.ends_with(".png") { PNG } else { b"text" };
            save(run, name, data);
        }
        let taken = take(&workspace(run), run, id).unwrap().unwrap_or_default();
        (id.to_owned(), taken)
    }

    #[test]
    fn shows_this_rounds_pictures_latest_step_first_and_which_step_saved_each() {
        let dir = run();
        let old: Vec<String> = (0..MAX_SHOWN).map(|i| format!("old-{i}.png")).collect();
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let taken = [
            step(dir.path(), "b0", &old),
            step(dir.path(), "b1", &["run.png"]),
            step(dir.path(), "s1", &["screen.png", "notes.txt"]),
        ];
        let attempts = [
            attempt("b0", "builder", 0, &["calc.py"], "2026-11-10T09:00:00.000Z"),
            attempt("b1", "builder", 1, &["calc.py"], "2026-11-10T10:00:00.000Z"),
            attempt("s1", "screens", 1, &[], "2026-11-10T11:00:00.000Z"),
        ];
        let shots = for_approval(dir.path(), &attempts, 1, &taken);
        // The earlier round's take no place, and aren't listed as left out.
        assert_eq!(
            names(&shots),
            [("screen.png", "screens"), ("run.png", "builder")]
        );
        assert_eq!(shots.shown[0].data, PNG);
        assert_eq!(shots.shown[0].mime, "image/png");
        assert_eq!(shots.left_out, ["notes.txt"]);
        assert_eq!(shots.more_left_out, 0);
        // The builder may have edited after saving its own; the check came after the last change.
        assert_eq!(
            shots
                .shown
                .iter()
                .map(|shot| shot.before_last_change)
                .collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn a_change_after_it_comes_from_keels_record_not_a_files_date() {
        let dir = run();
        let taken = [step(dir.path(), "s1", &["screen.png"])];
        // Whatever date the step gave the file, a builder changed the code after the check ended.
        let saved = File::options()
            .write(true)
            .open(copy_path(dir.path(), "s1", 0))
            .unwrap();
        saved
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(86_400))
            .unwrap();
        let attempts = [
            attempt("s1", "screens", 0, &[], "2026-11-10T10:00:00.000Z"),
            attempt("b1", "builder", 0, &["calc.py"], "2026-11-10T11:00:00.000Z"),
            // Later, and it changed nothing.
            attempt("r1", "review", 0, &[], "2026-11-10T12:00:00.000Z"),
        ];
        let shots = for_approval(dir.path(), &attempts, 0, &taken);
        assert!(shots.shown[0].before_last_change);
        let unchanged = [attempts[0].clone(), attempts[2].clone()];
        let shots = for_approval(dir.path(), &unchanged, 0, &taken);
        assert!(!shots.shown[0].before_last_change);
    }

    #[test]
    fn this_rounds_steps_share_the_limit_and_the_latest_keep_their_places() {
        let dir = run();
        let first: Vec<String> = (0..5).map(|i| format!("a-{i}.png")).collect();
        let second: Vec<String> = (0..5).map(|i| format!("b-{i}.png")).collect();
        let notes: Vec<String> = (0..MAX_LISTED).map(|i| format!("n-{i:02}.txt")).collect();
        let first: Vec<&str> = first.iter().map(String::as_str).collect();
        let mut second: Vec<&str> = second.iter().map(String::as_str).collect();
        second.extend(notes.iter().map(String::as_str));
        let taken = [
            step(dir.path(), "a", &first),
            step(dir.path(), "b", &second),
        ];
        let attempts = [
            attempt("a", "builder", 0, &[], "2026-11-10T10:00:00.000Z"),
            attempt("b", "screens", 0, &[], "2026-11-10T11:00:00.000Z"),
        ];
        let shots = for_approval(dir.path(), &attempts, 0, &taken);
        assert_eq!(shots.shown.len(), MAX_SHOWN);
        assert!(shots.shown[..5].iter().all(|shot| shot.step == "screens"));
        assert_eq!(shots.shown[5].name, "a-0.png");
        // b's notes, then a's last two pictures: only so many by name.
        assert_eq!(shots.left_out.len(), MAX_LISTED);
        assert_eq!(shots.left_out[0], "n-00.txt");
        assert_eq!(shots.more_left_out, 2);
    }

    #[test]
    fn what_a_step_saved_goes_out_as_base64_with_its_step() {
        let shot = Screenshot {
            name: "a.png".into(),
            mime: "image/png",
            data: PNG.to_vec(),
            step: "screens".into(),
            before_last_change: false,
        };
        let value = serde_json::to_value(&shot).unwrap();
        assert_eq!(value["data"], STANDARD.encode(PNG));
        assert_eq!(value["beforeLastChange"], false);
        assert_eq!(value["step"], "screens");
        let taken = Taken {
            kept: vec![Kept {
                name: "a.png".into(),
                mime: "image/png".into(),
            }],
            left_out: vec!["notes.txt".into()],
            more_left_out: 3,
        };
        let back: Taken = serde_json::from_value(serde_json::to_value(&taken).unwrap()).unwrap();
        assert_eq!(back, taken);
    }
}
