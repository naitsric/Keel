//! Frees the copy a run worked in, once the run is long over.
//!
//! A run's folder holds two kinds of thing. Its record: each step's raw output, diff, prompt and
//! settings are files right in the folder, with the pull request's text and the pictures its steps
//! took. And what its steps worked in: the clone, the copies of other projects it read, the change
//! split into files for an agent, and what the Bash hook kept. Only the second kind goes, and the
//! folder itself stays, so everything the app reads of a run is where it was.
//!
//! Deleting can't be taken back, and what's in a clone was written by an agent. So a folder goes
//! only if it is where Keel itself put that run, and nothing in it is followed out of it.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use keel_core::{AttemptState, PrState, ToolProfile};
use keel_harness::github;
use keel_store::{Attempt, Run};

use crate::{Engine, Inner, Result, TRASH, let_owner_change, locked, new_id, workflows};

/// How long after a run ended its copy stays.
const KEEP_DAYS: i64 = 7;
/// How long between two passes while the app stays open.
const PASS_HOURS: i64 = 24;
/// How often the thread looks at the clock. A Mac that sleeps doesn't count the hours it slept, so
/// one long wait could last days.
const LOOK: Duration = Duration::from_secs(15 * 60);

/// What's in a run's folder that only a step uses: its clone, the other projects it reads, its
/// change in files for an agent to read, and what the Bash hook keeps between commands.
const WORKING: [&str; 4] = ["workspace", "context", "change", "hook"];

/// A run whose copy should have gone and didn't, for whoever keeps a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyNotFreed {
    /// Empty when Keel couldn't even read which runs have ended.
    pub run_id: String,
    pub why: String,
}

impl std::fmt::Display for CopyNotFreed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.run_id.is_empty() {
            write!(f, "couldn't free old runs' copies: {}", self.why)
        } else {
            write!(f, "couldn't free run {}'s copy: {}", self.run_id, self.why)
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopiesFreed {
    /// The runs whose copy went in this pass.
    pub freed: Vec<String>,
    pub failed: Vec<CopyNotFreed>,
}

impl Engine {
    /// Starts the thread that frees old runs' copies: now, and about once a day while the app stays
    /// open. Call it once, after `recover`. `failed` hears of each run that couldn't be freed.
    pub fn start_freeing_old_copies(&self, failed: impl Fn(&CopyNotFreed) + Send + 'static) {
        // It doesn't keep the engine alive.
        let inner = Arc::downgrade(&self.inner);
        std::thread::spawn(move || {
            let mut last: Option<DateTime<Utc>> = None;
            while let Some(inner) = inner.upgrade() {
                let now = Utc::now();
                let due = last.is_none_or(|last| {
                    // A clock set back mustn't put it off for good.
                    now < last || now - last >= chrono::Duration::hours(PASS_HOURS)
                });
                if due {
                    last = Some(now);
                    inner.free_old_copies(now).failed.iter().for_each(&failed);
                }
                drop(inner);
                std::thread::sleep(LOOK);
            }
        });
    }

    /// One pass, here and now: frees the copy of every run that ended 7 days ago or more and whose
    /// work is somewhere else by now, or was discarded: see `may_go`. A run that fails is in
    /// `failed`, and the rest go on.
    pub fn free_old_copies(&self) -> CopiesFreed {
        self.inner.free_old_copies(Utc::now())
    }

    /// Whether Keel freed this run's copy.
    pub fn copy_freed(&self, run_id: &str) -> Result<bool> {
        Ok(self.inner.copy_freed(&self.inner.store.run(run_id)?))
    }
}

impl Inner {
    /// Whether the copy this run worked in is gone. There's no note of it anywhere: a run that
    /// ended and got a branch had a clone, and it isn't there.
    pub(crate) fn copy_freed(&self, run: &Run) -> bool {
        run.finished_at.is_some() && run.branch.is_some() && !self.workspace(&run.id).exists()
    }

    fn free_old_copies(&self, now: DateTime<Utc>) -> CopiesFreed {
        let mut done = CopiesFreed::default();
        let ended = match self.store.ended_run_ids() {
            Ok(ended) => ended,
            Err(error) => {
                done.failed.push(CopyNotFreed {
                    run_id: String::new(),
                    why: error.to_string(),
                });
                return done;
            }
        };
        for run_id in ended {
            let (freed, problems) = self.free_copy(&run_id, now);
            if freed {
                self.changed(&run_id);
                done.freed.push(run_id.clone());
            }
            if !problems.is_empty() {
                done.failed.push(CopyNotFreed {
                    run_id,
                    why: problems.join("; "),
                });
            }
        }
        done
    }

    /// Frees one run's copy if it may go. Whether anything went, and what went wrong.
    fn free_copy(&self, run_id: &str, now: DateTime<Utc>) -> (bool, Vec<String>) {
        let read = || -> Result<(Run, Vec<Attempt>)> {
            Ok((self.store.run(run_id)?, self.store.attempts_of_run(run_id)?))
        };
        let (run, attempts) = match read() {
            Ok((run, attempts)) if may_go(&run, &attempts, now) => (run, attempts),
            Ok(_) => return (false, Vec::new()),
            Err(error) => return (false, vec![error.to_string()]),
        };
        let (mut freed, mut problems) = (false, Vec::new());
        // Every step of a run works in the same folder, so this is one.
        for workspace in stored_workspaces(&attempts) {
            let folder = match own_folder(&self.runs_folders(), run_id, &workspace) {
                Ok(Some(folder)) => folder,
                Ok(None) => continue,
                Err(why) => {
                    problems.push(why);
                    continue;
                }
            };
            // Before the lock, which every run's next step waits for. And only with something
            // left to free, so GitHub isn't asked every day about a run freed long ago.
            if run.pr_state == Some(PrState::Closed)
                && has_working(&folder)
                && !self.still_closed(&run)
            {
                continue;
            }
            let trash = folder.join(TRASH);
            {
                // What `retry` holds while it opens a run again and starts its step. Read again
                // under it, so a run a person just opened again keeps its copy.
                let _guard = locked(&self.advancing);
                match read() {
                    Ok((run, attempts)) if may_go(&run, &attempts, now) => {
                        let kept = record_files(&attempts);
                        match move_aside(&folder, &trash, &kept) {
                            Ok(moved) => freed |= moved,
                            Err(why) => problems.push(why),
                        }
                    }
                    Ok(_) => return (freed, problems),
                    Err(error) => {
                        problems.push(error.to_string());
                        return (freed, problems);
                    }
                }
            }
            // Outside the lock: a clone can take a while to delete. Also what an earlier pass, cut
            // short, left there.
            if let Err(why) = empty(&trash) {
                problems.push(why);
            }
        }
        (freed, problems)
    }

    /// Whether a pull request Keel last saw closed is still closed, or merged, asked of GitHub here
    /// and now. A closed one can be opened again, Keel stops looking at one closed a month ago,
    /// and this runs when the app opens, before any look. What the store says of the last look
    /// isn't enough either: a look is written down before it's made, so also when it fails. With
    /// no answer the copy stays, and the next pass asks again. It only reads.
    fn still_closed(&self, run: &Run) -> bool {
        let Some((repo, number)) = run.pr_url.as_deref().and_then(github::pull_request_of) else {
            return false;
        };
        let account = self
            .store
            .project(&run.project_id)
            .ok()
            .and_then(|project| project.github_account);
        let (Some(account), Ok(gh)) = (account, self.gh()) else {
            return false;
        };
        gh.token(&account)
            .and_then(|token| gh.pr_status(&token, &repo, number))
            .is_ok_and(|status| matches!(status.state, PrState::Closed | PrState::Merged))
    }

    /// Where Keel keeps runs: where new ones go, and under the data folder, where they went before.
    fn runs_folders(&self) -> [PathBuf; 2] {
        [
            self.config.runs_dir.clone(),
            self.config.data_dir.join("runs"),
        ]
    }
}

/// Whether a run's copy can go now: it ended 7 days ago or more, no step of it is at work, and
/// what it made is somewhere else by now or isn't wanted.
fn may_go(run: &Run, attempts: &[Attempt], now: DateTime<Utc>) -> bool {
    ended_long_ago(run.finished_at.as_deref(), now)
        && !at_work(attempts)
        && nothing_to_lose(run.pr_url.is_some(), run.pr_state, attempts)
}

fn ended_long_ago(finished_at: Option<&str>, now: DateTime<Utc>) -> bool {
    finished_at
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|ended| now.signed_duration_since(ended) >= chrono::Duration::days(KEEP_DAYS))
}

fn at_work(attempts: &[Attempt]) -> bool {
    attempts
        .iter()
        .any(|a| AttemptState::parse(&a.state).is_none_or(AttemptState::is_active))
}

/// Whether the clone holds nothing a person would miss. Only delivering copies a run's commits out
/// of its clone, which has no remote. So:
/// - with a pull request, it has to be merged, or closed: `still_closed` asks about a closed one
///   again before anything goes. An open one or a draft keeps the copy however old, and so does
///   one Keel hasn't looked at yet;
/// - with none, the run was discarded or committed nothing. One that ended on its own with
///   commits, like a single build step or a workflow with no delivery, has them nowhere else,
///   and keeps its copy however old.
fn nothing_to_lose(
    has_pull_request: bool,
    pr_state: Option<PrState>,
    attempts: &[Attempt],
) -> bool {
    match (has_pull_request, pr_state) {
        (false, _) => discarded(attempts) || !committed(attempts),
        (true, Some(PrState::Merged | PrState::Closed)) => true,
        (true, Some(PrState::Open | PrState::Draft) | None) => false,
    }
}

/// Whether a person ended the run rather than take its work: rejected at an approval, left, or
/// closed, by them or by an assistant over MCP. Going by each step's latest attempt, as the store
/// does to say where a run stands, so a step left and then tried again doesn't count.
fn discarded(attempts: &[Attempt]) -> bool {
    attempts.iter().any(|a| {
        matches!(
            AttemptState::parse(&a.state),
            Some(AttemptState::Rejected | AttemptState::Abandoned)
        ) && workflows::is_latest_of_its_step(attempts, a)
    })
}

/// Whether any step may have left a commit, going by the commit each step started at and ended at.
/// A step that can write and has no end on record, like one cut short when the Mac went down, may
/// have.
fn committed(attempts: &[Attempt]) -> bool {
    let Some(base) = attempts.iter().find_map(|a| a.sha_before.as_deref()) else {
        return false;
    };
    attempts.iter().any(|a| {
        let Some(before) = a.sha_before.as_deref() else {
            return false;
        };
        let can_write = a.kind == "agent" && a.tools == ToolProfile::Build.as_str();
        before != base
            || match a.sha_after.as_deref() {
                Some(after) => after != base,
                None => can_write,
            }
    })
}

/// Whether anything a step worked in is still in the run's folder.
fn has_working(folder: &Path) -> bool {
    WORKING
        .iter()
        .any(|name| std::fs::symlink_metadata(folder.join(name)).is_ok())
}

/// Where the store says the run's steps worked, each once.
fn stored_workspaces(attempts: &[Attempt]) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    for attempt in attempts.iter().filter(|a| !a.workspace.is_empty()) {
        let workspace = PathBuf::from(&attempt.workspace);
        if !all.contains(&workspace) {
            all.push(workspace);
        }
    }
    all
}

/// The files of a run's record that the store points at.
fn record_files(attempts: &[Attempt]) -> Vec<PathBuf> {
    attempts
        .iter()
        .flat_map(|a| [Some(a.raw_path.clone()), a.diff_path.clone()])
        .flatten()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The run's own folder, resolved, given where the store says its steps worked. `None` when
/// there's no such folder anymore. An error, and nothing is deleted, unless the folder is exactly
/// `<one of Keel's runs folders>/<run id>` once every link on the way is resolved: not a folder
/// elsewhere, not another run's, not a link that leads out.
fn own_folder(
    runs_folders: &[PathBuf],
    run_id: &str,
    workspace: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut parts = Path::new(run_id).components();
    let is_a_name = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(name)), None) if name == run_id
    );
    if !is_a_name {
        return Err(format!("its id, {run_id:?}, isn't a folder's name"));
    }
    let not_own = || {
        format!(
            "{} isn't this run's folder in Keel's runs folders, so it was left alone",
            workspace.display()
        )
    };
    let stored = match workspace.parent() {
        Some(stored)
            if workspace.is_absolute()
                && workspace.file_name().is_some_and(|n| n == WORKING[0]) =>
        {
            stored
        }
        _ => return Err(not_own()),
    };
    let real = match stored.canonicalize() {
        Ok(real) => real,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", stored.display())),
    };
    let own = runs_folders
        .iter()
        .filter_map(|runs| runs.canonicalize().ok())
        .any(|runs| real == runs.join(run_id));
    if own { Ok(Some(real)) } else { Err(not_own()) }
}

/// Whether `path` is a folder: `false` if nothing's there, and an error if it's anything else, a
/// link included. Keel made these as folders, and a link could lead anywhere.
fn is_folder(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(format!(
            "{} isn't a folder, so it was left alone",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Moves what the run's steps worked in out of the way, to be deleted after. At once, so a run
/// never has half a clone. A folder that holds part of the run's record stays. Whether anything
/// moved.
fn move_aside(folder: &Path, trash: &Path, record: &[PathBuf]) -> Result<bool, String> {
    let mut moved = false;
    for name in WORKING {
        let path = folder.join(name);
        if !is_folder(&path)? {
            continue;
        }
        let holds = record.iter().find(|file| {
            let real = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
            real.starts_with(&path)
        });
        if let Some(file) = holds {
            return Err(format!(
                "{} holds part of the run's record, {}, so it was left alone",
                path.display(),
                file.display()
            ));
        }
        let to = trash.join(new_id());
        make_trash(trash)?;
        if std::fs::rename(&path, &to).is_err() {
            // Freeing what a run built uses the same trash, and takes it away once it's empty.
            make_trash(trash)?;
            std::fs::rename(&path, &to).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        moved = true;
    }
    Ok(moved)
}

/// The run's trash, there and a folder: never a link someone left in its place.
fn make_trash(trash: &Path) -> Result<(), String> {
    match std::fs::create_dir(trash) {
        Ok(()) => Ok(()),
        // If it was taken away in between, the move says so.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => is_folder(trash).map(|_| ()),
        Err(error) => Err(format!("{}: {error}", trash.display())),
    }
}

/// Deletes what's waiting in a run's trash, and the trash once it's empty.
fn empty(trash: &Path) -> Result<(), String> {
    if !is_folder(trash)? {
        return Ok(());
    }
    let entries = std::fs::read_dir(trash).map_err(|e| format!("{}: {e}", trash.display()))?;
    let mut failed = None;
    for entry in entries.flatten() {
        if let Err(error) = delete(&entry.path()) {
            failed.get_or_insert(format!("{}: {error}", entry.path().display()));
        }
    }
    match failed {
        Some(why) => Err(why),
        None => {
            let _ = std::fs::remove_dir(trash);
            Ok(())
        }
    }
}

/// Deletes a folder and all that's in it, even what a tool made read-only. A link in it is taken
/// away as a link: what it points to is never opened, changed or deleted.
fn delete(path: &Path) -> io::Result<()> {
    if delete_once(path).is_ok() {
        return Ok(());
    }
    let_owner_change(path);
    match delete_once(path) {
        // Freeing what a run built empties the same trash, and may have got there first.
        Err(_) if std::fs::symlink_metadata(path).is_err() => Ok(()),
        other => other,
    }
}

fn delete_once(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        // `remove_dir_all` opens each folder without following links, and unlinks what's in it.
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    const RUN: &str = "0b9e0b0e-1111-4222-8333-444455556666";

    /// Two runs folders, as the app has, and somewhere that isn't Keel's.
    struct Disk {
        dir: tempfile::TempDir,
        runs: [PathBuf; 2],
    }

    fn disk() -> Disk {
        let dir = tempfile::tempdir().unwrap();
        let runs = [dir.path().join("keel/runs"), dir.path().join("data/runs")];
        for folder in &runs {
            std::fs::create_dir_all(folder).unwrap();
        }
        Disk { dir, runs }
    }

    fn a_run_folder(at: &Path) -> PathBuf {
        std::fs::create_dir_all(at.join("workspace/src")).unwrap();
        std::fs::write(at.join("workspace/src/main.rs"), "fn main() {}\n").unwrap();
        at.join("workspace")
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-19T12:00:00.000Z")
            .unwrap()
            .to_utc()
    }

    /// As `keel_store::now` writes a time, this long before `now`.
    fn ago(time: chrono::Duration) -> String {
        (now() - time).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    const BASE: &str = "base";

    /// Attempts as the store reads them back: step, kind, tools, state, and the commit each ended
    /// at. All start at the run's first commit unless a test says otherwise.
    fn steps(rows: &[(&str, &str, &str, AttemptState, Option<&str>)]) -> Vec<Attempt> {
        let plain: Vec<_> = rows
            .iter()
            .map(|(step, kind, tools, state, _)| (*step, *kind, *tools, *state))
            .collect();
        let mut attempts = crate::testing::recorded(&plain);
        for (attempt, (_, kind, _, _, after)) in attempts.iter_mut().zip(rows) {
            if matches!(*kind, "agent" | "setup" | "check") {
                attempt.sha_before = Some(BASE.into());
            }
            attempt.sha_after = after.map(str::to_owned);
        }
        attempts
    }

    fn built(
        state: AttemptState,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        AttemptState,
        Option<&'static str>,
    ) {
        ("builder", "agent", "build", state, Some("commit"))
    }

    #[test]
    fn a_copy_goes_seven_days_after_its_run_ended_and_not_a_moment_before() {
        let days = chrono::Duration::days;
        assert!(ended_long_ago(Some(&ago(days(7))), now()));
        let a_second_short = days(7) - chrono::Duration::seconds(1);
        assert!(!ended_long_ago(Some(&ago(a_second_short)), now()));
        assert!(!ended_long_ago(Some(&ago(days(6))), now()));
        assert!(ended_long_ago(Some(&ago(days(8))), now()));
    }

    #[test]
    fn a_run_that_has_not_ended_keeps_its_copy_however_old() {
        assert!(!ended_long_ago(None, now()));
        assert!(!ended_long_ago(Some("last week"), now()), "unreadable");
    }

    #[test]
    fn only_a_pull_request_merged_or_closed_lets_the_copy_go() {
        let delivered = steps(&[built(AttemptState::Succeeded)]);
        for (state, expected) in [
            (Some(PrState::Merged), true),
            // Asked of GitHub again before anything goes.
            (Some(PrState::Closed), true),
            (Some(PrState::Open), false),
            (Some(PrState::Draft), false),
            // Keel opened it, or found it there, and hasn't looked since.
            (None, false),
        ] {
            assert_eq!(
                nothing_to_lose(true, state, &delivered),
                expected,
                "{state:?}"
            );
        }
    }

    #[test]
    fn with_no_pull_request_a_run_that_ended_on_its_own_with_commits_keeps_its_copy() {
        use AttemptState::{Abandoned, Failed, Rejected, Succeeded};
        let goes = |attempts: &[Attempt]| nothing_to_lose(false, None, attempts);
        let human = |state| ("human", "approval", "none", state, None);
        let reads = ("reader", "agent", "readOnly", Succeeded, Some(BASE));

        // A single build step, and a workflow with no delivery: the commits are only there.
        assert!(!goes(&steps(&[built(Succeeded)])));
        assert!(!goes(&steps(&[built(Succeeded), human(Succeeded)])));
        assert!(!goes(&steps(&[built(Succeeded), reads])));

        // Discarded: rejected, left, or closed.
        assert!(goes(&steps(&[built(Succeeded), human(Rejected)])));
        assert!(goes(&steps(&[built(Abandoned)])));
        assert!(goes(&steps(&[
            built(Succeeded),
            ("check", "check", "check", Abandoned, None)
        ])));
        // Left, tried again, and then it ended on its own: not discarded anymore.
        assert!(!goes(&steps(&[built(Abandoned), built(Succeeded)])));
        assert!(!goes(&steps(&[built(Failed), built(Succeeded)])));

        // Nothing committed: a task that only reads, a map, trying a check, a builder that changed
        // nothing, and a run no step of which ever got a clone.
        assert!(goes(&steps(&[reads])));
        assert!(goes(&steps(&[reads, human(Succeeded)])));
        assert!(goes(&steps(&[
            ("setup", "setup", "setup", Succeeded, None),
            ("try-check", "check", "check", Failed, None),
        ])));
        assert!(goes(&steps(&[(
            "builder",
            "agent",
            "build",
            Succeeded,
            Some(BASE)
        )])));
        assert!(goes(&[]));
    }

    #[test]
    fn a_step_that_can_write_and_never_said_where_it_ended_may_have_committed() {
        use AttemptState::{Interrupted, Succeeded};
        let goes = |attempts: &[Attempt]| nothing_to_lose(false, None, attempts);
        let reads = ("reader", "agent", "readOnly", Succeeded, Some(BASE));
        assert!(!goes(&steps(&[
            ("builder", "agent", "build", Interrupted, None),
            reads
        ])));
        // One that only reads can't have, wherever it stopped.
        assert!(goes(&steps(&[
            ("reader", "agent", "readOnly", Interrupted, None),
            reads
        ])));
        // And a later step that started from another commit says an earlier one committed.
        let mut later = steps(&[("setup", "setup", "setup", Succeeded, None), reads]);
        later[1].sha_before = Some("commit".into());
        later[1].sha_after = Some("commit".into());
        assert!(!goes(&later));
    }

    #[test]
    fn a_run_with_a_step_at_work_keeps_its_copy() {
        for (state, expected) in [
            (AttemptState::Running, true),
            (AttemptState::Queued, true),
            (AttemptState::Preparing, true),
            (AttemptState::Stalled, true),
            (AttemptState::Succeeded, false),
            (AttemptState::Abandoned, false),
        ] {
            let attempts = steps(&[
                built(AttemptState::Succeeded),
                ("learner", "agent", "readOnly", state, None),
            ]);
            assert_eq!(at_work(&attempts), expected, "{state:?}");
        }
    }

    #[test]
    fn a_run_folder_in_either_of_keels_runs_folders_is_its_own() {
        let disk = disk();
        for runs in &disk.runs {
            let workspace = a_run_folder(&runs.join(RUN));
            assert_eq!(
                own_folder(&disk.runs, RUN, &workspace),
                Ok(Some(runs.join(RUN).canonicalize().unwrap()))
            );
        }
    }

    #[test]
    fn a_run_folder_that_is_gone_has_nothing_to_free() {
        let disk = disk();
        let workspace = disk.runs[0].join(RUN).join("workspace");
        assert_eq!(own_folder(&disk.runs, RUN, &workspace), Ok(None));
    }

    #[test]
    fn a_stored_folder_outside_keels_runs_folders_is_never_its_own() {
        let disk = disk();
        let elsewhere = disk.dir.path().join("documents");
        let other_run = "another-run";
        let refused = [
            // Somewhere that isn't Keel's, even under the run's id.
            a_run_folder(&elsewhere.join(RUN)),
            // Another run's folder.
            a_run_folder(&disk.runs[0].join(other_run)),
            // The runs folder itself, and the folder above it.
            a_run_folder(&disk.runs[0]),
            a_run_folder(disk.runs[0].parent().unwrap()),
            // Its own folder, but not where its steps work.
            disk.runs[0].join(RUN).join("screenshots"),
            // Deeper than a run's folder.
            a_run_folder(&disk.runs[0].join(RUN).join("nested")),
            // A way round through `..` that ends at another run's.
            a_run_folder(&disk.runs[0].join(RUN))
                .join("../..")
                .join(other_run)
                .join("workspace"),
            PathBuf::from("workspace"),
            PathBuf::from("/"),
        ];
        for workspace in refused {
            let answer = own_folder(&disk.runs, RUN, &workspace);
            assert!(answer.is_err(), "{}: {answer:?}", workspace.display());
        }
        for id in ["", ".", "..", "a/b", "../documents", "/"] {
            let workspace = disk.runs[0].join(id).join("workspace");
            let answer = own_folder(&disk.runs, id, &workspace);
            assert!(answer.is_err(), "run id {id:?}: {answer:?}");
        }
    }

    #[test]
    fn a_run_folder_that_is_a_link_to_somewhere_else_is_never_its_own() {
        let disk = disk();
        let elsewhere = disk.dir.path().join("documents");
        a_run_folder(&elsewhere);
        symlink(&elsewhere, disk.runs[0].join(RUN)).unwrap();
        let workspace = disk.runs[0].join(RUN).join("workspace");
        assert!(own_folder(&disk.runs, RUN, &workspace).is_err());
        // Nor a runs folder that isn't one of Keel's, whatever links into Keel's.
        let fake = disk.dir.path().join("fake/runs");
        std::fs::create_dir_all(&fake).unwrap();
        let workspace = a_run_folder(&fake.join(RUN));
        assert!(own_folder(&disk.runs, RUN, &workspace).is_err());
    }

    #[test]
    fn a_working_folder_that_is_a_link_is_left_alone_with_what_it_points_to() {
        let disk = disk();
        let folder = disk.runs[0].join(RUN);
        std::fs::create_dir_all(&folder).unwrap();
        let elsewhere = disk.dir.path().join("documents");
        a_run_folder(&elsewhere);
        symlink(elsewhere.join("workspace"), folder.join("workspace")).unwrap();
        let trash = folder.join(TRASH);
        assert!(move_aside(&folder, &trash, &[]).is_err());
        assert!(elsewhere.join("workspace/src/main.rs").exists());
        assert!(folder.join("workspace").exists(), "the link stays too");
        assert!(!trash.exists());
    }

    #[test]
    fn a_trash_that_is_a_link_is_never_emptied_or_filled() {
        let disk = disk();
        let folder = disk.runs[0].join(RUN);
        a_run_folder(&folder);
        let elsewhere = disk.dir.path().join("documents");
        std::fs::create_dir_all(elsewhere.join("taxes")).unwrap();
        std::fs::write(elsewhere.join("taxes/2026.txt"), "kept\n").unwrap();
        let trash = folder.join(TRASH);
        symlink(&elsewhere, &trash).unwrap();
        assert!(empty(&trash).is_err());
        assert!(move_aside(&folder, &trash, &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("taxes/2026.txt")).unwrap(),
            "kept\n"
        );
        assert!(folder.join("workspace/src/main.rs").exists());
    }

    #[test]
    fn a_folder_that_holds_part_of_the_record_stays() {
        let disk = disk();
        let folder = disk.runs[0].join(RUN).canonicalize_after_creating();
        a_run_folder(&folder);
        let raw = folder.join("workspace/builder-1.jsonl");
        std::fs::write(&raw, "{}\n").unwrap();
        let trash = folder.join(TRASH);
        assert!(move_aside(&folder, &trash, std::slice::from_ref(&raw)).is_err());
        assert!(raw.exists());
        // Beside the clone, where Keel writes it, it's no reason to keep the clone.
        let beside = folder.join("builder-1.jsonl");
        std::fs::rename(&raw, &beside).unwrap();
        assert_eq!(
            move_aside(&folder, &trash, std::slice::from_ref(&beside)),
            Ok(true)
        );
        assert!(beside.exists() && !folder.join("workspace").exists());
    }

    #[test]
    fn deleting_takes_a_link_away_and_leaves_what_it_points_to() {
        let disk = disk();
        let elsewhere = disk.dir.path().join("documents");
        std::fs::create_dir_all(elsewhere.join("taxes")).unwrap();
        std::fs::write(elsewhere.join("taxes/2026.txt"), "kept\n").unwrap();
        std::fs::set_permissions(
            elsewhere.join("taxes"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();

        let trash = disk.runs[0].join(RUN).join(TRASH);
        let clone = trash.join("a-clone");
        // One link in a folder anyone can delete from, and one in a folder a tool made read-only,
        // which takes changing its permissions first.
        std::fs::create_dir_all(clone.join("locked")).unwrap();
        symlink(&elsewhere, clone.join("to-folder")).unwrap();
        symlink(elsewhere.join("taxes/2026.txt"), clone.join("to-file")).unwrap();
        symlink(elsewhere.join("taxes"), clone.join("locked/to-folder")).unwrap();
        std::fs::write(clone.join("locked/file.txt"), "gone\n").unwrap();
        std::fs::set_permissions(clone.join("locked"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        // And a link standing right in the trash.
        symlink(&elsewhere, trash.join("a-link")).unwrap();

        assert_eq!(empty(&trash), Ok(()));
        assert!(!trash.exists());
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("taxes/2026.txt")).unwrap(),
            "kept\n"
        );
        let mode = std::fs::metadata(elsewhere.join("taxes"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o500, "its permissions are as they were");
        std::fs::set_permissions(
            elsewhere.join("taxes"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    trait CanonicalAfterCreating {
        fn canonicalize_after_creating(&self) -> PathBuf;
    }

    impl CanonicalAfterCreating for PathBuf {
        /// As `own_folder` hands a run's folder on: with every link resolved.
        fn canonicalize_after_creating(&self) -> PathBuf {
            std::fs::create_dir_all(self).unwrap();
            self.canonicalize().unwrap()
        }
    }
}
