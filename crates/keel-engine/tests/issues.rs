//! Watching a project's issues with a label: what comes to the Backlog, what leaves it, and what Keel
//! says when it can't look. Against a fake `gh` whose GraphQL answer is whatever is in `issues`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{BoardColumn, CardLane};
use keel_engine::{Config, Engine, Error, IssueWatchProblem};
use keel_harness::Tools;
use keel_store::{ProjectSettings, Store};

struct World {
    _dir: tempfile::TempDir,
    engine: Engine,
    project_id: String,
    log: PathBuf,
    flags: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `api graphql` answers with what's in `issues`, and fails like GitHub does without it.
fn fake_gh(log: &Path, flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
echo "gh $*" >> "{log}/gh.log"
case "$1 $2" in
"auth token") echo "tok-SECRET"; exit 0;;
"repo view") echo '{{"defaultBranchRef":{{"name":"main"}},"viewerPermission":"WRITE","isPrivate":true}}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"api graphql")
  [ -f "{flags}/slow" ] && sleep 0.6
  if [ -f "{flags}/issues" ]; then cat "{flags}/issues"; exit 0; fi
  echo "HTTP 502: Bad Gateway (https://api.github.com/graphql)" >&2; exit 1;;
esac
echo "unexpected: $*" >&2; exit 1
"##,
        log = log.display(),
        flags = flags.display(),
    )
}

fn world() -> World {
    world_with(|_| {})
}

fn world_with(tune: impl FnOnce(&mut Config)) -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    let flags = dir.path().join("flags");
    for path in [&bin, &log, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    write_script(&bin.join("gh"), &fake_gh(&log, &flags));
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "app\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.keep_awake = false;
    tune(&mut config);
    let engine = Engine::new(store, config, tools, |_: &str| {});
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
        _dir: dir,
        engine,
        project_id: project.id,
        log,
        flags,
    }
}

fn issue(number: i64) -> String {
    format!(
        r#"{{"number":{number},"title":"Issue {number}","url":"https://github.com/acme/app/issues/{number}","body":"Body {number}","author":{{"login":"bob"}}}}"#
    )
}

/// How GitHub answers: `open` newest first, as asked, and the state of each of `closed`.
fn answer(label: bool, open: &[i64], closed: &[i64]) -> String {
    let nodes: Vec<String> = open.iter().map(|n| issue(*n)).collect();
    let states: String = closed
        .iter()
        .map(|n| format!(r#","i{n}":{{"state":"CLOSED"}}"#))
        .collect();
    let label = if label { r#"{"name":"keel"}"# } else { "null" };
    format!(
        r#"{{"data":{{"repository":{{"label":{label},"open":{{"nodes":[{}]}}{states}}}}}}}"#,
        nodes.join(",")
    )
}

impl World {
    fn github_says(&self, open: &[i64], closed: &[i64]) {
        std::fs::write(self.flags.join("issues"), answer(true, open, closed)).unwrap();
    }

    fn watch(&self, label: &str) {
        self.engine
            .set_issue_label(&self.project_id, Some(label))
            .unwrap();
    }

    fn look(&self) -> keel_engine::IssueWatch {
        self.engine
            .look_at_issues(&self.project_id)
            .unwrap()
            .unwrap()
    }

    /// The issue numbers in the Backlog, top to bottom.
    fn backlog(&self) -> Vec<i64> {
        self.engine
            .board(&self.project_id)
            .unwrap()
            .cards
            .into_iter()
            .filter(|c| c.column == BoardColumn::Backlog)
            .filter_map(|c| c.issue.map(|i| i.number))
            .collect()
    }

    fn card_of(&self, number: i64) -> String {
        self.engine
            .board(&self.project_id)
            .unwrap()
            .cards
            .into_iter()
            .find(|c| c.issue.as_ref().is_some_and(|i| i.number == number))
            .unwrap()
            .id
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.log.join("gh.log")).unwrap_or_default()
    }
}

#[test]
fn open_issues_with_the_label_come_to_the_backlog_oldest_first_and_only_once() {
    let w = world();
    w.watch("keel");
    w.github_says(&[6, 5], &[]);
    let watch = w.look();
    assert_eq!((watch.open, watch.brought, watch.removed), (2, 2, 0));
    assert_eq!(watch.label, "keel");
    assert_eq!(watch.problem, None);
    assert_eq!(w.backlog(), [5, 6]);
    let card = w
        .engine
        .board(&w.project_id)
        .unwrap()
        .cards
        .into_iter()
        .find(|c| c.issue.as_ref().is_some_and(|i| i.number == 5))
        .unwrap();
    assert_eq!(card.title, "Issue 5");
    assert_eq!(
        card.body, "",
        "the issue's text stays in its copy, as by hand"
    );

    w.github_says(&[7, 6, 5], &[]);
    let watch = w.look();
    assert_eq!(watch.brought, 1);
    assert_eq!(w.backlog(), [5, 6, 7]);
    assert_eq!(w.engine.issue_watch(&w.project_id), Some(watch));
    // The label goes as asked, and the token never in the arguments.
    let log = w.gh_log();
    assert!(log.contains(" -f label=keel"), "{log}");
    assert!(!log.contains("tok-SECRET"), "{log}");
}

#[test]
fn a_card_the_person_removed_does_not_come_back() {
    let w = world();
    w.watch("keel");
    w.github_says(&[5], &[]);
    w.look();
    w.engine.card_delete(&w.card_of(5)).unwrap();
    assert_eq!(w.look().brought, 0);
    assert!(w.backlog().is_empty());
}

#[test]
fn an_issue_put_on_the_board_by_hand_and_removed_does_not_come_back_either() {
    let w = world();
    let issue = keel_harness::github::Issue {
        number: 5,
        title: "Issue 5".into(),
        author: "bob".into(),
        url: "https://github.com/acme/app/issues/5".into(),
        body: String::new(),
    };
    let card = w.engine.card_import_issue(&w.project_id, &issue).unwrap();
    w.engine.card_delete(&card).unwrap();
    w.watch("keel");
    w.github_says(&[5], &[]);
    assert_eq!(w.look().brought, 0);
}

#[test]
fn a_card_whose_issue_closed_before_it_started_goes_and_comes_back_if_it_is_opened_again() {
    let w = world();
    w.watch("keel");
    w.github_says(&[6, 5], &[]);
    w.look();
    // It asks about the cards that never started, whatever their label is now.
    w.github_says(&[6], &[5]);
    let watch = w.look();
    assert!(w.gh_log().contains("i5:issue(number:5){state}"));
    assert_eq!((watch.brought, watch.removed), (0, 1));
    assert_eq!(w.backlog(), [6]);

    w.github_says(&[6, 5], &[]);
    assert_eq!(w.look().brought, 1);
    assert_eq!(w.backlog(), [6, 5]);
}

#[test]
fn a_card_in_todo_that_never_started_goes_too() {
    let w = world();
    w.watch("keel");
    w.github_says(&[5], &[]);
    w.look();
    // Todo without a session to start anything: it waits there.
    w.engine
        .card_move(&w.card_of(5), CardLane::Todo, 0, "en")
        .unwrap();
    w.github_says(&[], &[5]);
    assert_eq!(w.look().removed, 1);
    assert!(w.engine.board(&w.project_id).unwrap().cards.is_empty());
}

#[test]
fn a_project_that_watches_no_label_asks_github_nothing() {
    let w = world();
    w.github_says(&[5], &[]);
    assert_eq!(w.engine.look_at_issues(&w.project_id).unwrap(), None);
    assert_eq!(w.engine.issue_watch(&w.project_id), None);
    assert!(!w.gh_log().contains("graphql"));
    assert!(w.backlog().is_empty());
}

#[test]
fn a_label_the_repo_does_not_have_is_said_and_one_github_cannot_take_is_refused() {
    let w = world();
    w.watch("kee");
    std::fs::write(w.flags.join("issues"), answer(false, &[], &[])).unwrap();
    assert_eq!(w.look().problem, Some(IssueWatchProblem::LabelMissing));

    assert!(matches!(
        w.engine
            .set_issue_label(&w.project_id, Some(&"x".repeat(51))),
        Err(Error::InvalidState)
    ));
    // A new label forgets what the old one found, and an empty one stops watching.
    w.watch("keel");
    assert_eq!(w.engine.issue_watch(&w.project_id), None);
    let project = w.engine.set_issue_label(&w.project_id, Some("  ")).unwrap();
    assert_eq!(project.issue_label, None);
    assert_eq!(w.engine.look_at_issues(&w.project_id).unwrap(), None);
}

#[test]
fn what_stops_a_look_is_said_and_changes_nothing() {
    let w = world();
    w.watch("keel");
    // GitHub failing.
    let watch = w.look();
    assert!(
        matches!(&watch.problem, Some(IssueWatchProblem::Github { detail }) if detail.contains("Bad Gateway")),
        "{watch:?}"
    );
    assert!(w.backlog().is_empty());
    // No account to ask with.
    w.engine
        .update_project(&w.project_id, &ProjectSettings::default())
        .unwrap();
    w.github_says(&[5], &[]);
    assert_eq!(w.look().problem, Some(IssueWatchProblem::NoAccount));
    assert!(w.backlog().is_empty());
}

#[test]
fn the_background_look_brings_new_issues_on_its_own() {
    let w = world_with(|config| config.issue_look_again = Duration::from_millis(30));
    w.watch("keel");
    w.engine.start_issue_watch();
    w.github_says(&[5], &[]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while w.backlog() != [5] {
        assert!(Instant::now() < deadline, "issue 5 never came");
        std::thread::sleep(Duration::from_millis(10));
    }
    w.github_says(&[8, 5], &[]);
    while w.backlog() != [5, 8] {
        assert!(Instant::now() < deadline, "issue 8 never came");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_label_changed_while_keel_asks_github_brings_nothing_and_the_next_look_waits_for_it() {
    let w = world();
    w.watch("keel");
    w.github_says(&[5], &[]);
    std::fs::write(w.flags.join("slow"), "").unwrap();
    let (engine, project) = (w.engine.clone(), w.project_id.clone());
    let first = std::thread::spawn(move || engine.look_at_issues(&project).unwrap());
    std::thread::sleep(Duration::from_millis(200));
    w.watch("other");
    // Asked while the first one waits on GitHub: it waits its turn, then looks for the new label.
    let second = w.look();
    assert_eq!(
        first.join().unwrap(),
        None,
        "what it found was about the old label"
    );
    assert_eq!(second.label, "other");
    assert_eq!(
        w.backlog(),
        [5],
        "brought once, by the look for the label as it is now"
    );
    assert_eq!(w.engine.issue_watch(&w.project_id), Some(second));
}

#[test]
fn a_label_emptied_while_keel_asks_github_brings_nothing() {
    let w = world();
    w.watch("keel");
    w.github_says(&[5], &[]);
    std::fs::write(w.flags.join("slow"), "").unwrap();
    let (engine, project) = (w.engine.clone(), w.project_id.clone());
    let look = std::thread::spawn(move || engine.look_at_issues(&project).unwrap());
    std::thread::sleep(Duration::from_millis(200));
    w.engine.set_issue_label(&w.project_id, None).unwrap();
    assert_eq!(look.join().unwrap(), None);
    assert!(w.backlog().is_empty());
    assert_eq!(w.engine.issue_watch(&w.project_id), None);
}
