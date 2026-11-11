//! Pictures a step left of how the change looks, for the person who approves it. A step saves them
//! in a folder of the run's private copy that git ignores, so they never end up in a commit, and
//! Keel shows them as they are: nothing here takes a picture.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};
use keel_store::Attempt;
use serde::{Serialize, Serializer};

/// The folder, inside the run's private copy.
pub const FOLDER: &str = ".keel-screenshots";
/// What tells a command where that folder is.
pub const VARIABLE: &str = "KEEL_SCREENSHOTS";
// The approval says these two in words, in `run.screenshotsLeftOut`.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SHOWN: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshot {
    pub name: String,
    /// What its first bytes say it is, whatever its name says.
    pub mime: &'static str,
    #[serde(serialize_with = "as_base64")]
    pub data: Vec<u8>,
    pub taken_at: String,
    /// A step changed the code after it was saved, so it may not show what's being approved.
    pub before_last_change: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshots {
    pub shown: Vec<Screenshot>,
    /// What's in the folder and isn't shown: not a picture Keel shows, too big, a link, or one past
    /// the limit. Nothing is left out without saying so.
    pub left_out: Vec<String>,
}

fn as_base64<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(data))
}

pub fn folder(workspace: &Path) -> PathBuf {
    workspace.join(FOLDER)
}

/// When a step last changed the code, as far as Keel can tell: the end of the latest attempt that
/// changed files. A picture saved during that attempt may come from before its last edit, so it
/// counts as earlier.
pub fn last_change(attempts: &[Attempt]) -> Option<DateTime<Utc>> {
    attempts
        .iter()
        .filter(|attempt| {
            attempt
                .files_changed
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .is_some_and(|files| !files.is_empty())
        })
        .filter_map(|attempt| attempt.finished_at.as_deref())
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .max()
}

/// The pictures in the run's folder, by name. Keel reads them outside the sandbox, so a link, which
/// could point anywhere on this Mac, is never followed, and neither is a folder that is one.
pub fn read(workspace: &Path, last_change: Option<DateTime<Utc>>) -> Screenshots {
    let folder = folder(workspace);
    if !std::fs::symlink_metadata(&folder).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Screenshots::default();
    }
    let Ok(entries) = std::fs::read_dir(&folder) else {
        return Screenshots::default();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut found = Screenshots::default();
    for name in names {
        match picture(&folder.join(&name)) {
            Some((mime, data, taken)) if found.shown.len() < MAX_SHOWN => {
                found.shown.push(Screenshot {
                    name,
                    mime,
                    data,
                    taken_at: taken.to_rfc3339_opts(SecondsFormat::Millis, true),
                    before_last_change: last_change.is_some_and(|change| taken < change),
                });
            }
            _ => found.left_out.push(name),
        }
    }
    found
}

/// One file, if it's a picture Keel shows. It's opened without following a link and without waiting
/// on a pipe, and what's checked is what was opened, so nothing can be swapped in between.
fn picture(path: &Path) -> Option<(&'static str, Vec<u8>, DateTime<Utc>)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
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
    let mime = kind(&data)?;
    Some((mime, data, meta.modified().ok()?.into()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder(dir.path())).unwrap();
        dir
    }

    fn save(dir: &Path, name: &str, data: &[u8]) {
        std::fs::write(folder(dir).join(name), data).unwrap();
    }

    fn names(shots: &Screenshots) -> Vec<&str> {
        shots.shown.iter().map(|shot| shot.name.as_str()).collect()
    }

    #[test]
    fn shows_the_pictures_by_name_and_says_what_it_left_out() {
        let dir = workspace();
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
        let shots = read(dir.path(), None);
        assert_eq!(
            names(&shots),
            ["board.webp", "run.dark.jpg", "run.light.png"]
        );
        assert_eq!(
            shots.shown.iter().map(|shot| shot.mime).collect::<Vec<_>>(),
            ["image/webp", "image/jpeg", "image/png"]
        );
        assert_eq!(shots.shown[2].data, PNG);
        assert_eq!(shots.left_out, ["fake.png", "notes.txt", "vector.svg"]);
        assert!(shots.shown.iter().all(|shot| !shot.before_last_change));
    }

    #[test]
    fn without_the_folder_there_is_nothing_to_show() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path(), None), Screenshots::default());
        assert_eq!(read(&dir.path().join("gone"), None), Screenshots::default());
    }

    #[test]
    fn never_follows_a_link_out_of_the_run() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private.png"), PNG).unwrap();

        let dir = workspace();
        std::os::unix::fs::symlink(
            outside.path().join("private.png"),
            folder(dir.path()).join("linked.png"),
        )
        .unwrap();
        std::fs::hard_link(
            outside.path().join("private.png"),
            folder(dir.path()).join("hard.png"),
        )
        .unwrap();
        let shots = read(dir.path(), None);
        assert!(shots.shown.is_empty(), "{:?}", names(&shots));
        assert_eq!(shots.left_out, ["hard.png", "linked.png"]);

        // Nor a folder that is a link.
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), folder(linked.path())).unwrap();
        assert_eq!(read(linked.path(), None), Screenshots::default());
    }

    #[test]
    fn leaves_out_what_is_too_big_or_past_the_limit_and_says_so() {
        let dir = workspace();
        let mut big = PNG.to_vec();
        big.resize(MAX_BYTES as usize + 1, 0);
        save(dir.path(), "big.png", &big);
        for index in 0..MAX_SHOWN + 1 {
            save(dir.path(), &format!("view-{index}.png"), PNG);
        }
        let shots = read(dir.path(), None);
        assert_eq!(shots.shown.len(), MAX_SHOWN);
        assert_eq!(
            shots.left_out,
            ["big.png".to_owned(), format!("view-{MAX_SHOWN}.png")]
        );
    }

    #[test]
    fn says_which_were_saved_before_the_code_last_changed() {
        let dir = workspace();
        save(dir.path(), "old.png", PNG);
        save(dir.path(), "new.png", PNG);
        let now = SystemTime::now();
        let old = File::options()
            .write(true)
            .open(folder(dir.path()).join("old.png"))
            .unwrap();
        old.set_modified(now - Duration::from_secs(120)).unwrap();
        let change: DateTime<Utc> = (now - Duration::from_secs(60)).into();
        let shots = read(dir.path(), Some(change));
        let before: Vec<_> = shots
            .shown
            .iter()
            .map(|shot| (shot.name.as_str(), shot.before_last_change))
            .collect();
        assert_eq!(before, [("new.png", false), ("old.png", true)]);
    }

    fn attempt(files: Option<serde_json::Value>, finished_at: Option<&str>) -> Attempt {
        Attempt {
            id: "a".into(),
            run_id: "r".into(),
            step_id: "builder".into(),
            kind: "agent".into(),
            role: "worker".into(),
            round: 0,
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
            files_changed: files,
            result_text: None,
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            turns: None,
            duration_ms: None,
            started_at: String::new(),
            finished_at: finished_at.map(str::to_owned),
            last_activity_at: None,
        }
    }

    #[test]
    fn the_code_last_changed_when_the_latest_step_that_changed_files_ended() {
        let changed = |at| attempt(Some(serde_json::json!(["calc.py"])), Some(at));
        let attempts = [
            changed("2026-11-10T10:00:00.000Z"),
            changed("2026-11-10T11:00:00.000Z"),
            // Later, and it changed nothing.
            attempt(
                Some(serde_json::json!([])),
                Some("2026-11-10T12:00:00.000Z"),
            ),
            attempt(None, Some("2026-11-10T13:00:00.000Z")),
            // Still going.
            attempt(Some(serde_json::json!(["calc.py"])), None),
        ];
        assert_eq!(
            last_change(&attempts).map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            Some("2026-11-10T11:00:00.000Z".to_owned())
        );
        assert_eq!(last_change(&attempts[2..]), None);
    }

    #[test]
    fn the_data_goes_out_as_base64() {
        let shot = Screenshot {
            name: "a.png".into(),
            mime: "image/png",
            data: PNG.to_vec(),
            taken_at: "2026-11-10T10:00:00.000Z".into(),
            before_last_change: false,
        };
        let value = serde_json::to_value(&shot).unwrap();
        assert_eq!(value["data"], STANDARD.encode(PNG));
        assert_eq!(value["beforeLastChange"], false);
        assert_eq!(value["takenAt"], "2026-11-10T10:00:00.000Z");
    }
}
