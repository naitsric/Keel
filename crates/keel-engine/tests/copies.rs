//! Freeing the copy an old run worked in, and what's left of the run after: against a fake `claude`,
//! a fake `gh` and a local bare repo standing in for GitHub. Every folder is a temporary one.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keel_core::{AttemptState, PrState, Reason, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, CopiesFreed, Engine, Error, RawCheck, StartRun};
use keel_harness::Tools;
use keel_store::rusqlite_value::Value;
use keel_store::{ProjectSettings, Store};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    flags: PathBuf,
    tools: Tools,
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A step that can write changes a file and commits it, or fails with `fails` there.
fn fake_claude(flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
cat > /dev/null
tools=""; prev=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
if [ -f "{flags}/fails" ]; then exit 1; fi
echo work >> fixed.txt
git add -A && git commit -qm "fix"
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Fixed it.\",\"num_turns\":1,\"total_cost_usd\":0.5,\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}},\"modelUsage\":{{\"claude-fake\":{{}}}}}}"
"##,
        flags = flags.display(),
    )
}

/// Enough of `gh` to open a pull request. Nothing here reaches GitHub.
const FAKE_GH: &str = r##"#!/bin/bash
case "$1 $2" in
"auth token") echo "tok-SECRET"; exit 0;;
"repo view") echo '{"defaultBranchRef":{"name":"main"},"viewerPermission":"WRITE","isPrivate":true}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"pr create") echo "https://github.com/acme/app/pull/12"; exit 0;;
esac
echo "unexpected: $*" >&2; exit 1
"##;

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let flags = dir.path().join("flags");
    for path in [&bin, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    write_script(&bin.join("claude"), &fake_claude(&flags));
    write_script(&bin.join("gh"), FAKE_GH);

    let remote = dir.path().join("remotes/acme/app.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "published\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    // Apart from the data folder, as in the app.
    config.runs_dir = dir.path().join("keel/runs");
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.github_git_base = format!("file://{}/remotes", dir.path().display());
    let engine = Engine::new(store.clone(), config, tools.clone(), |_| {});
    let project = engine.add_project(&repo).unwrap();
    engine
        .update_project(
            &project.id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                ..Default::default()
            },
        )
        .unwrap();
    World {
        dir,
        store,
        engine,
        project_id: project.id,
        flags,
        tools,
    }
}

fn builder() -> WorkflowStep {
    WorkflowStep::Agent(StepSpec {
        id: "builder".into(),
        model: "fake".into(),
        effort: None,
        tools: ToolProfile::Build,
        prompt: "{{request}}".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        risks: vec![],
        model_when_risky: None,
        role: Default::default(),
    })
}

fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// As `keel_store::now` writes a time, this many days back.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn blocked(result: Result<impl std::fmt::Debug, Error>) -> Reason {
    match result {
        Err(Error::Blocked(reason)) => reason,
        other => panic!("expected a refusal with a reason, got {other:?}"),
    }
}

impl World {
    /// A run whose builder is done and which waits for a person, with a delivery after or not.
    fn waiting(&self, delivers: bool) -> String {
        let mut workflow = vec![builder(), WorkflowStep::Approval { id: "human".into() }];
        if delivers {
            workflow.push(WorkflowStep::Deliver { id: "pr".into() });
        }
        let run = self
            .engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "Fix the bug".into(),
                workflow,
                saved_workflow: None,
                language: "en".into(),
                source_issue: None,
                started_by: None,
            })
            .unwrap();
        self.wait_step(&run, "human", AttemptState::WaitingApproval);
        run
    }

    /// A run that ended with a draft pull request, which is how Keel opens them.
    fn delivered(&self) -> String {
        let run = self.waiting(true);
        self.answer(&run, true);
        assert_eq!(self.store.run(&run).unwrap().pr_state, Some(PrState::Draft));
        run
    }

    /// A run a person rejected, so it ended and never had a pull request.
    fn rejected(&self) -> String {
        let run = self.waiting(false);
        self.answer(&run, false);
        assert_eq!(self.store.run(&run).unwrap().pr_url, None);
        run
    }

    /// Answers the approval, and waits for the run to end and for what its build left to be freed,
    /// which goes through the same trash.
    fn answer(&self, run: &str, approve: bool) {
        let human = self.step(run, "human");
        let text = if approve { "" } else { "Not this way" };
        self.engine
            .answer_approval(&human.id, approve, text)
            .unwrap();
        until("the run ending", || {
            self.store.run(run).unwrap().finished_at.is_some()
        });
        let folder = self.folder(run);
        until("its build being freed", || {
            !folder.join("workspace/.keel-cache").exists() && !folder.join("trash").exists()
        });
    }

    fn wait_step(&self, run: &str, id: &str, state: AttemptState) -> keel_store::Attempt {
        until(&format!("{id} reaching {state:?}"), || {
            self.store
                .attempts_of_run(run)
                .unwrap()
                .iter()
                .any(|a| a.step_id == id && a.state == state.as_str())
        });
        self.step(run, id)
    }

    fn step(&self, run: &str, id: &str) -> keel_store::Attempt {
        let attempts = self.store.attempts_of_run(run).unwrap();
        let found = attempts.into_iter().rev().find(|a| a.step_id == id);
        found.unwrap_or_else(|| panic!("no step {id}"))
    }

    /// The run's own folder, where the store says its steps worked.
    fn folder(&self, run: &str) -> PathBuf {
        let workspace = PathBuf::from(self.step(run, "builder").workspace);
        assert_eq!(
            workspace,
            self.dir
                .path()
                .join("keel/runs")
                .join(run)
                .join("workspace")
        );
        workspace.parent().unwrap().to_path_buf()
    }

    fn workspace(&self, run: &str) -> PathBuf {
        self.folder(run).join("workspace")
    }

    fn ended(&self, run: &str, days: i64) {
        self.store
            .patch_run(run, &[("finished_at", Value::Text(days_ago(days)))])
            .unwrap();
    }

    /// As if Keel had looked at the pull request and found this.
    fn pull_request_is(&self, run: &str, state: PrState) {
        self.store
            .patch_run(run, &[("pr_state", Value::Text(state.as_str().to_owned()))])
            .unwrap();
    }

    /// One pass, with nothing gone wrong.
    fn freed(&self) -> Vec<String> {
        let CopiesFreed { freed, failed } = self.engine.free_old_copies();
        assert_eq!(failed, vec![], "nothing fails");
        freed
    }

    /// Somewhere that isn't Keel's, with a file a person would miss.
    fn documents(&self) -> PathBuf {
        let documents = self.dir.path().join("documents");
        std::fs::create_dir_all(documents.join("taxes")).unwrap();
        std::fs::write(documents.join("taxes/2026.txt"), "mine\n").unwrap();
        documents
    }
}

fn mine_is_intact(documents: &Path) {
    assert_eq!(
        std::fs::read_to_string(documents.join("taxes/2026.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_run_merged_eight_days_ago_frees_its_copy_and_keeps_its_record() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    let builder = w.step(&run, "builder");
    let folder = w.folder(&run);
    // What a run across projects and a builder's hook leave beside the clone.
    for beside in ["context/other-project/src", "change/abc", "hook"] {
        std::fs::create_dir_all(folder.join(beside)).unwrap();
        std::fs::write(folder.join(beside).join("file"), "working\n").unwrap();
    }
    std::fs::create_dir_all(folder.join("screenshots").join(&builder.id)).unwrap();
    std::fs::write(
        folder.join("screenshots").join(&builder.id).join("0"),
        "png",
    )
    .unwrap();
    let raw = std::fs::read_to_string(&builder.raw_path).unwrap();
    let diff = w.engine.diff(&builder.id).unwrap();
    assert!(diff.contains("fixed.txt"), "{diff}");
    assert!(!w.engine.copy_freed(&run).unwrap());

    w.ended(&run, 8);
    w.pull_request_is(&run, PrState::Merged);
    assert_eq!(w.freed(), vec![run.clone()]);

    for gone in ["workspace", "context", "change", "hook", "trash"] {
        assert!(!folder.join(gone).exists(), "{gone} is still there");
    }
    assert!(w.engine.copy_freed(&run).unwrap());
    // The record is where the app reads it, as it was.
    assert_eq!(std::fs::read_to_string(&builder.raw_path).unwrap(), raw);
    assert_eq!(w.engine.verify_raw(&builder.id).unwrap(), RawCheck::Intact);
    assert_eq!(w.engine.diff(&builder.id).unwrap(), diff);
    for kept in [
        "builder-1.prompt.txt",
        "builder-1.system.md",
        "builder-1.settings.json",
        "pull-request.md",
    ] {
        assert!(folder.join(kept).is_file(), "{kept} went");
    }
    assert!(
        folder
            .join("screenshots")
            .join(&builder.id)
            .join("0")
            .exists()
    );
    assert!(!w.store.events(&builder.id, 0).unwrap().is_empty());
    let after = w.step(&run, "builder");
    assert_eq!(after.prompt, builder.prompt);
    assert_eq!(after.files_changed, builder.files_changed);
    // And the next pass has nothing left to do, or to complain about.
    assert_eq!(w.freed(), Vec::<String>::new());
}

#[test]
fn a_run_that_ended_six_days_ago_keeps_its_copy_until_the_seventh() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.pull_request_is(&run, PrState::Merged);
    w.ended(&run, 6);
    assert_eq!(w.freed(), Vec::<String>::new());
    assert!(w.workspace(&run).join("fixed.txt").exists());
    assert!(!w.engine.copy_freed(&run).unwrap());

    w.ended(&run, 7);
    assert_eq!(w.freed(), vec![run.clone()]);
    assert!(!w.workspace(&run).exists());
}

#[test]
fn a_run_whose_pull_request_is_open_or_a_draft_keeps_its_copy_however_old() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.ended(&run, 400);
    // A draft, as Keel opened it.
    assert_eq!(w.freed(), Vec::<String>::new());
    w.pull_request_is(&run, PrState::Open);
    assert_eq!(w.freed(), Vec::<String>::new());
    // One Keel hasn't looked at since could be either.
    w.store
        .patch_run(&run, &[("pr_state", Value::Null)])
        .unwrap();
    assert_eq!(w.freed(), Vec::<String>::new());
    assert!(w.workspace(&run).join("fixed.txt").exists());

    w.pull_request_is(&run, PrState::Closed);
    assert_eq!(w.freed(), vec![run.clone()]);
    assert!(!w.workspace(&run).exists());
}

#[test]
fn a_run_with_no_pull_request_frees_its_copy_eight_days_after_it_ended() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.rejected();
    let builder = w.step(&run, "builder");
    assert_eq!(w.freed(), Vec::<String>::new(), "it ended just now");
    w.ended(&run, 8);
    assert_eq!(w.freed(), vec![run.clone()]);
    assert!(!w.workspace(&run).exists());
    assert!(w.engine.diff(&builder.id).unwrap().contains("fixed.txt"));
    assert_eq!(w.engine.verify_raw(&builder.id).unwrap(), RawCheck::Intact);
}

#[test]
fn a_run_that_has_not_ended_keeps_its_copy() {
    keel_harness::needs!(sandbox);
    let w = world();
    let old = w.rejected();
    w.ended(&old, 8);
    // One waits for the person, and one failed and waits for them to try again or close it.
    let waiting = w.waiting(true);
    std::fs::write(w.flags.join("fails"), "").unwrap();
    let failed = w
        .engine
        .start_run(StartRun::one_step(
            w.project_id.clone(),
            match builder() {
                WorkflowStep::Agent(spec) => spec,
                _ => unreachable!(),
            },
            "en".into(),
        ))
        .unwrap();
    w.wait_step(&failed, "builder", AttemptState::Failed);
    // However long ago they started.
    for run in [&waiting, &failed] {
        w.store
            .patch_run(run, &[("created_at", Value::Text(days_ago(400)))])
            .unwrap();
        assert_eq!(w.store.run(run).unwrap().finished_at, None);
    }

    assert_eq!(w.freed(), vec![old.clone()]);
    assert!(!w.workspace(&old).exists());
    assert!(w.workspace(&waiting).join("fixed.txt").exists());
    assert!(w.workspace(&failed).join(".git").exists());
    // And the one that waited still delivers from its copy.
    w.answer(&waiting, true);
    assert!(w.store.run(&waiting).unwrap().pr_url.is_some());
}

#[test]
fn a_stored_folder_outside_keels_runs_folders_is_never_deleted() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.rejected();
    w.ended(&run, 8);
    let stored = w.workspace(&run);
    // The same store, opened by a Keel that keeps its runs somewhere else: what the store names
    // is a folder like a run's, with the run's id, that isn't in either of its runs folders.
    let mut config = Config::new(w.dir.path().join("other-data"));
    config.runs_dir = w.dir.path().join("other-keel/runs");
    config.keep_awake = false;
    for runs in [&config.runs_dir, &config.data_dir.join("runs")] {
        std::fs::create_dir_all(runs).unwrap();
    }
    let elsewhere = Engine::new(w.store.clone(), config, w.tools.clone(), |_| {});

    let pass = elsewhere.free_old_copies();
    assert_eq!(pass.freed, Vec::<String>::new());
    assert_eq!(pass.failed.len(), 1, "{:?}", pass.failed);
    assert_eq!(pass.failed[0].run_id, run);
    assert!(
        pass.failed[0].why.contains(&*stored.to_string_lossy()),
        "it names the folder: {}",
        pass.failed[0]
    );
    assert!(stored.join("fixed.txt").exists());
    assert!(stored.join(".git").exists());
    // The Keel it belongs to frees it.
    assert_eq!(w.freed(), vec![run.clone()]);
}

#[test]
fn a_run_folder_swapped_for_a_link_leaves_what_it_points_to() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.rejected();
    w.ended(&run, 8);
    let folder = w.folder(&run);
    let documents = w.documents();
    std::fs::rename(&folder, documents.join("moved")).unwrap();
    symlink(documents.join("moved"), &folder).unwrap();

    let pass = w.engine.free_old_copies();
    assert_eq!(pass.freed, Vec::<String>::new());
    assert_eq!(pass.failed.len(), 1, "{:?}", pass.failed);
    assert!(documents.join("moved/workspace/fixed.txt").exists());
    mine_is_intact(&documents);
}

#[test]
fn a_link_inside_a_clone_that_points_outside_leaves_its_target_intact() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.rejected();
    w.ended(&run, 8);
    let workspace = w.workspace(&run);
    let documents = w.documents();
    // As an agent could have left them: to a folder, to a file, and one in a folder made read-only,
    // as Go leaves its modules.
    symlink(&documents, workspace.join("docs")).unwrap();
    symlink(
        documents.join("taxes/2026.txt"),
        workspace.join("taxes.txt"),
    )
    .unwrap();
    std::fs::create_dir_all(workspace.join("pkg/mod")).unwrap();
    symlink(documents.join("taxes"), workspace.join("pkg/mod/taxes")).unwrap();
    for locked in ["pkg/mod", "pkg"] {
        std::fs::set_permissions(
            workspace.join(locked),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
    }
    let before = std::fs::metadata(documents.join("taxes")).unwrap();

    assert_eq!(w.freed(), vec![run.clone()]);
    assert!(!workspace.exists());
    assert!(!w.folder(&run).join("trash").exists());
    mine_is_intact(&documents);
    let after = std::fs::metadata(documents.join("taxes")).unwrap();
    assert_eq!(after.permissions(), before.permissions());
    assert_eq!(std::fs::read_dir(&documents).unwrap().count(), 1);
}

#[test]
fn one_run_that_cannot_be_freed_does_not_stop_the_others() {
    keel_harness::needs!(sandbox);
    let w = world();
    let runs = [w.rejected(), w.rejected(), w.rejected()];
    for run in &runs {
        w.ended(run, 8);
    }
    // The one in the middle has something that isn't Keel's where its clone goes.
    let documents = w.documents();
    let folder = w.folder(&runs[1]);
    std::fs::rename(folder.join("workspace"), folder.join("aside")).unwrap();
    symlink(&documents, folder.join("workspace")).unwrap();

    let pass = w.engine.free_old_copies();
    assert_eq!(pass.freed, vec![runs[0].clone(), runs[2].clone()]);
    assert_eq!(pass.failed.len(), 1, "{:?}", pass.failed);
    assert_eq!(pass.failed[0].run_id, runs[1]);
    assert!(!pass.failed[0].why.is_empty());
    for freed in [&runs[0], &runs[2]] {
        assert!(!w.workspace(freed).exists());
    }
    mine_is_intact(&documents);
    assert!(folder.join("workspace").is_symlink());
}

#[test]
fn opening_the_app_frees_old_copies_on_its_own_and_says_which_it_could_not() {
    keel_harness::needs!(sandbox);
    let w = world();
    let runs = [w.rejected(), w.rejected()];
    for run in &runs {
        w.ended(run, 8);
    }
    let documents = w.documents();
    let folder = w.folder(&runs[0]);
    std::fs::rename(folder.join("workspace"), folder.join("aside")).unwrap();
    symlink(&documents, folder.join("workspace")).unwrap();

    let told = Arc::new(Mutex::new(Vec::new()));
    let hears = told.clone();
    w.engine
        .start_freeing_old_copies(move |failed| hears.lock().unwrap().push(failed.clone()));
    until("the old copy being freed", || {
        !w.workspace(&runs[1]).exists() && !w.folder(&runs[1]).join("trash").exists()
    });
    let told = told.lock().unwrap().clone();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].run_id, runs[0]);
    mine_is_intact(&documents);
}

#[test]
fn trying_a_step_again_in_a_freed_copy_says_it_was_freed_and_starts_nothing() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.ended(&run, 8);
    w.pull_request_is(&run, PrState::Closed);
    assert_eq!(w.freed(), vec![run.clone()]);
    let ended_at = w.store.run(&run).unwrap().finished_at;
    let attempts = w.store.attempts_of_run(&run).unwrap().len();

    // A delivery tried again, and a builder, would each need the clone.
    for step in ["pr", "builder"] {
        let again = w.engine.retry(&w.step(&run, step).id, false);
        assert_eq!(blocked(again), Reason::CopyFreed, "{step}");
    }
    assert_eq!(w.store.attempts_of_run(&run).unwrap().len(), attempts);
    assert_eq!(w.store.run(&run).unwrap().finished_at, ended_at);
    assert!(!w.workspace(&run).exists(), "no new clone was made");
}

#[test]
fn the_whole_change_of_a_freed_run_says_it_was_freed_instead_of_reading_as_no_change() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.rejected();
    assert!(w.engine.run_diff(&run).unwrap().contains("fixed.txt"));
    w.ended(&run, 8);
    assert_eq!(w.freed(), vec![run.clone()]);
    assert_eq!(blocked(w.engine.run_diff(&run)), Reason::CopyFreed);
    // Each step still has what it changed.
    let builder = w.step(&run, "builder");
    assert!(w.engine.diff(&builder.id).unwrap().contains("fixed.txt"));
}

#[test]
fn a_delivery_can_be_tried_again_until_the_copy_goes() {
    keel_harness::needs!(sandbox);
    let w = world();
    let run = w.delivered();
    w.pull_request_is(&run, PrState::Closed);
    w.ended(&run, 6);
    assert_eq!(w.freed(), Vec::<String>::new());
    let first = w.step(&run, "pr");
    w.engine.retry(&first.id, false).unwrap();
    until("the delivery being tried again", || {
        let again = w.step(&run, "pr");
        again.id != first.id && again.state == AttemptState::Succeeded.as_str()
    });
    assert!(w.workspace(&run).join("fixed.txt").exists());
}
