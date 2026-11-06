//! A whole workflow, from request to draft pull request, against a fake `claude`, a fake `gh` and
//! a local bare repo standing in for GitHub.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{AttemptState, MASK, Reason, WorkflowStep};
use keel_engine::{Config, Engine, Error, StartRun, full_workflow, request_from_issue};
use keel_harness::Tools;
use keel_harness::github::Issue;
use keel_store::{ProjectSettings, Store};

const TOKEN: &str = "tok-SECRET-alice";

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    project_id: String,
    repo: PathBuf,
    remote: PathBuf,
    log: PathBuf,
}

fn git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A step asked for a structured answer gives a spec. Other read-only steps just say something, and
/// build steps also change a file, unless told not to.
fn fake_claude(log: &Path, flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}}'; exit 0;;
esac
prompt="$(cat)"
system=""; tools=""; schema=""; prev=""
for a in "$@"; do
  [ "$prev" = "--append-system-prompt-file" ] && system="$a"
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  prev="$a"
done
{{ echo "=== CLAUDE tools=$tools"; echo "$prompt"; echo "--- system"; cat "$system"; }} >> "{log}/claude.log"
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}}"
secret=""
[ -f "{flags}/secret" ] && secret=" $(cat "{flags}/secret")"
if [ -n "$schema" ]; then
  output="{{\"summary\":\"SPEC-OUTPUT: criteria 1 and 2$secret\",\"criteria\":[{{\"id\":\"C1\",\"when\":\"it runs\",\"then\":\"it works\"}}],\"changes\":[],\"limits\":[],\"decided\":[],\"questions\":[]}}"
  echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"total_cost_usd\":0.01,\"structured_output\":$output,\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}},\"modelUsage\":{{\"claude-fake\":{{}}}}}}"
  exit 0
fi
if [ "$tools" = "Read,Grep,Glob" ]; then
  text="SPEC-OUTPUT: criteria 1 and 2$secret"
else
  text="Fixed it.$secret"
  if [ ! -f "{flags}/no-change" ]; then
    echo work >> fixed.txt
    git add -A && git commit -qm "fix$secret"
  fi
fi
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$text\",\"num_turns\":1,\"total_cost_usd\":0.01,\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}},\"modelUsage\":{{\"claude-fake\":{{}}}}}}"
"##,
        log = log.display(),
        flags = flags.display(),
    )
}

fn fake_gh(log: &Path, flags: &Path) -> String {
    format!(
        r##"#!/bin/bash
echo "gh $* [token:${{GH_TOKEN:+set}}]" >> "{log}/gh.log"
case "$1 $2" in
"auth status") echo '{{"hosts":{{"github.com":[{{"login":"alice","active":false,"state":"success","scopes":"repo, read:org, gist, admin:org"}},{{"login":"bob","active":true,"state":"success","scopes":"repo"}}]}}}}'; exit 0;;
"auth token") echo "{TOKEN}"; exit 0;;
"repo view")
  if [ -f "{flags}/repo-hidden" ]; then echo "GraphQL: Could not resolve to a Repository with the name 'acme/app'. (repository)" >&2; exit 1; fi
  echo '{{"defaultBranchRef":{{"name":"main"}},"viewerPermission":"WRITE","isPrivate":true}}'; exit 0;;
"api repos/acme/app/branches/main") echo false; exit 0;;
"issue list") echo '[{{"number":7,"title":"Broken","author":{{"login":"carol"}},"url":"https://github.com/acme/app/issues/7","body":"It breaks"}}]'; exit 0;;
"pr create")
  [ "$GH_TOKEN" = "{TOKEN}" ] || {{ echo "HTTP 401: Bad credentials" >&2; exit 1; }}
  if [ -f "{flags}/pr-sso" ]; then echo "Resource protected by organization SAML enforcement" >&2; exit 1; fi
  if [ -f "{flags}/pr-exists" ]; then echo "a pull request for branch already exists:" >&2; echo "https://github.com/acme/app/pull/99" >&2; exit 1; fi
  prev=""
  for a in "$@"; do [ "$prev" = "--body-file" ] && cp "$a" "{log}/pr-body.md"; prev="$a"; done
  echo "$*" >> "{log}/pr.log"
  echo "https://github.com/acme/app/pull/12"; exit 0;;
esac
echo "unexpected: $*" >&2; exit 1
"##,
        log = log.display(),
        flags = flags.display(),
    )
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let log = dir.path().join("log");
    let flags = dir.path().join("flags");
    for path in [&bin, &log, &flags] {
        std::fs::create_dir(path).unwrap();
    }
    write_script(&bin.join("claude"), &fake_claude(&log, &flags));
    write_script(&bin.join("gh"), &fake_gh(&log, &flags));

    let remotes = dir.path().join("remotes");
    let remote = remotes.join("acme/app.git");
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
    // The user is in the middle of something else. Runs must start from what's published.
    git(&repo, &["checkout", "-q", "-b", "wip"]);
    std::fs::write(repo.join("wip.txt"), "unfinished\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "wip"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let engine = engine_for(dir.path(), &store);
    let project = engine.add_project(&repo).unwrap();
    assert_eq!(project.github_repo.as_deref(), Some("acme/app"));
    engine
        .update_project(
            &project.id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
            },
        )
        .unwrap();
    World {
        dir,
        store,
        engine,
        project_id: project.id,
        repo,
        remote,
        log,
    }
}

fn engine_for(root: &Path, store: &Arc<Store>) -> Engine {
    let tools = Tools::from_dirs(&[
        root.join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    let mut config = Config::new(root.join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    config.github_git_base = format!("file://{}/remotes", root.display());
    Engine::new(store.clone(), config, tools, |_| {})
}

impl World {
    fn start(&self, check: Option<&str>) -> String {
        self.engine
            .start_run(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: "Fix the bug".into(),
                workflow: full_workflow("en", "opus", "sonnet", check),
                language: "en".into(),
                source_issue: None,
                saved_workflow: None,
                started_by: None,
            })
            .unwrap()
    }

    fn step(&self, run: &str, id: &str) -> Option<keel_store::Attempt> {
        self.store
            .attempts_of_run(run)
            .unwrap()
            .into_iter()
            .rev()
            .find(|a| a.step_id == id)
    }

    fn wait(&self, run: &str, id: &str, state: AttemptState) -> keel_store::Attempt {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(attempt) = self.step(run, id)
                && attempt.state == state.as_str()
            {
                return attempt;
            }
            assert!(
                Instant::now() < deadline,
                "{id} never reached {state:?}: {:?}",
                self.step(run, id).map(|a| (a.state, a.reason))
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_run_finished(&self, run: &str) -> keel_store::Run {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let current = self.store.run(run).unwrap();
            if current.finished_at.is_some() {
                return current;
            }
            assert!(Instant::now() < deadline, "run never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.log.join(name)).unwrap_or_default()
    }

    fn flag(&self, name: &str) {
        std::fs::write(self.dir.path().join("flags").join(name), "").unwrap();
    }

    fn unflag(&self, name: &str) {
        std::fs::remove_file(self.dir.path().join("flags").join(name)).unwrap();
    }
}

fn reason(attempt: &keel_store::Attempt) -> Reason {
    serde_json::from_value(attempt.reason.clone().expect("has a reason")).unwrap()
}

/// Every file Keel wrote, read as bytes, for looking for a secret.
fn all_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            all_files(&path, found);
        } else {
            found.push(path);
        }
    }
}

#[test]
fn a_request_goes_all_the_way_to_a_draft_pull_request() {
    let w = world();
    let run = w.start(Some("test -f fixed.txt"));

    // Each agent step has its own model, and the spec reaches the builder.
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    assert_eq!(w.step(&run, "spec").unwrap().model_requested, "opus");
    assert_eq!(w.step(&run, "builder").unwrap().model_requested, "sonnet");
    assert!(
        w.step(&run, "builder")
            .unwrap()
            .prompt
            .contains("SPEC-OUTPUT: criteria 1 and 2")
    );
    assert_eq!(w.step(&run, "check").unwrap().exit_code, Some(0));

    // Nothing leaves until a person says so.
    assert!(w.log("pr.log").is_empty());
    assert!(git(&w.remote, &["branch", "--list", "keel/*"]).is_empty());

    w.engine
        .answer_approval(&approval.id, true, "Looks right")
        .unwrap();
    let finished = w.wait_run_finished(&run);
    assert_eq!(
        finished.pr_url.as_deref(),
        Some("https://github.com/acme/app/pull/12")
    );
    assert_eq!(finished.pr_number, Some(12));

    // A draft, on Keel's own branch, against the default branch.
    let branch = finished.branch.unwrap();
    assert!(branch.starts_with("keel/"));
    let pr = w.log("pr.log");
    assert!(
        pr.contains("--draft")
            && pr.contains(&format!("--head {branch}"))
            && pr.contains("--base main"),
        "{pr}"
    );
    assert_eq!(
        git(&w.remote, &["rev-parse", &branch]),
        git(
            Path::new(&w.step(&run, "builder").unwrap().workspace),
            &["rev-parse", "HEAD"]
        )
    );

    // The default branch is untouched, and the run started from it, not from the user's checkout.
    let builder = w.step(&run, "builder").unwrap();
    assert_eq!(
        builder.sha_before.as_deref(),
        Some(git(&w.remote, &["rev-parse", "main"]).as_str())
    );
    assert!(!Path::new(&builder.workspace).join("wip.txt").exists());
    assert_eq!(
        git(&w.repo, &["branch", "--show-current"]),
        "wip",
        "the user's checkout is left alone"
    );

    // The description says what happened.
    let body = w.log("pr-body.md");
    assert!(body.contains("Fixed it."));
    assert!(body.contains("`fixed.txt`"));
    assert!(body.contains("`test -f fixed.txt`: passed"));
    assert!(body.contains("Looks right"));
    assert!(body.contains("claude-fake"));

    // The run says where it went, and the audit has it.
    let delivered = w.step(&run, "pull-request").unwrap();
    let events = w.store.events(&delivered.id, 0).unwrap();
    let event = events.iter().find(|e| e.kind == "delivered").unwrap();
    assert_eq!(
        delivered.account.as_deref(),
        Some("alice"),
        "the run says which account did it"
    );
    assert_eq!(event.data["account"], "alice");
    assert_eq!(event.data["draft"], true);
}

#[test]
fn agents_get_the_original_and_a_pull_request_the_masked_copy() {
    let w = world();
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    std::fs::write(w.dir.path().join("flags/secret"), &secret).unwrap();
    let run = w.start(Some("test -f fixed.txt"));
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);

    // `{{steps.spec.output}}` carries it whole, or the next agent couldn't do its work.
    let spec_output = format!("SPEC-OUTPUT: criteria 1 and 2 {secret}");
    assert!(
        w.step(&run, "builder")
            .unwrap()
            .prompt
            .contains(&spec_output)
    );
    assert!(w.log("claude.log").contains(&spec_output));

    w.engine
        .answer_approval(&approval.id, true, &format!("Use API_KEY={secret}"))
        .unwrap();
    let finished = w.wait_run_finished(&run);

    // What goes to GitHub as text.
    let body = w.log("pr-body.md");
    assert!(!body.contains(&secret), "{body}");
    assert!(body.contains(&format!("Fixed it. {MASK}")), "{body}");
    assert!(body.contains(&format!("Use API_KEY={MASK}")), "{body}");
    let pr = w.log("pr.log");
    assert!(!pr.contains(&secret), "{pr}");
    assert!(pr.contains(&format!("--title fix {MASK}")), "{pr}");

    // The record keeps what really happened.
    assert_eq!(
        w.step(&run, "builder").unwrap().result_text.unwrap(),
        format!("Fixed it. {secret}")
    );
    let decisions = w.engine.decisions(&w.project_id).unwrap();
    assert_eq!(decisions[0].text, format!("Use API_KEY={secret}"));

    // Masking doesn't reach what's in the branch. A person reviewing the draft has to catch that.
    let subject = git(
        &w.remote,
        &["log", "-1", "--format=%s", &finished.branch.unwrap()],
    );
    assert!(subject.contains(&secret), "{subject}");
}

#[test]
fn the_token_is_never_stored_and_the_active_account_never_changes() {
    let w = world();
    let run = w.start(Some("true"));
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    w.wait_run_finished(&run);

    let mut files = Vec::new();
    all_files(&w.dir.path().join("data"), &mut files);
    files.extend([
        w.dir.path().join("keel.db"),
        w.dir.path().join("keel.db-wal"),
    ]);
    let git_dirs_ok = files.iter().filter(|f| f.is_file()).all(|file| {
        let bytes = std::fs::read(file).unwrap_or_default();
        !String::from_utf8_lossy(&bytes).contains(TOKEN)
    });
    assert!(git_dirs_ok, "the token ended up in a file Keel wrote");

    let gh = w.log("gh.log");
    assert!(gh.contains("auth token --user alice"));
    assert!(!gh.contains("auth switch"), "{gh}");
    assert!(!gh.contains("pr merge"), "{gh}");
    assert!(!w.log("claude.log").contains(TOKEN));
}

#[test]
fn a_failed_check_stops_the_run_and_can_be_retried_alone() {
    let w = world();
    let run = w.start(Some("test -f approved-by-test || exit 3"));
    let check = w.wait(&run, "check", AttemptState::Failed);
    assert_eq!(reason(&check), Reason::CheckFailed { exit_code: Some(3) });
    assert_eq!(check.exit_code, Some(3));
    assert!(
        w.step(&run, "approval").is_none(),
        "nothing runs after a failed check"
    );
    let result = w
        .store
        .events(&check.id, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "checkResult")
        .unwrap();
    assert_eq!(result.data["exitCode"], 3);

    std::fs::write(Path::new(&check.workspace).join("approved-by-test"), "").unwrap();
    let agents_before = w.log("claude.log").matches("=== CLAUDE").count();
    w.engine.retry(&check.id, false).unwrap();
    w.wait(&run, "approval", AttemptState::WaitingApproval);
    assert_eq!(
        w.log("claude.log").matches("=== CLAUDE").count(),
        agents_before,
        "agents don't run again"
    );
    assert_eq!(w.step(&run, "check").unwrap().number, 2);
}

#[test]
fn checks_run_with_no_credentials_in_the_environment() {
    // SAFETY: only this test sets this variable, and nothing reads it concurrently.
    unsafe { std::env::set_var("GH_TOKEN", "must-not-leak") };
    let w = world();
    let run = w.start(Some("env > env.txt"));
    let check = w.wait(&run, "approval", AttemptState::WaitingApproval);
    let env = std::fs::read_to_string(Path::new(&check.workspace).join("env.txt")).unwrap();
    assert!(!env.contains("must-not-leak"));
    unsafe { std::env::remove_var("GH_TOKEN") };
}

#[test]
fn rejecting_ends_the_run_and_the_reason_reaches_the_next_one() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine
        .answer_approval(
            &approval.id,
            false,
            "Use the existing retry helper, not a new one",
        )
        .unwrap();
    assert_eq!(w.step(&run, "approval").unwrap().state, "rejected");
    assert!(w.step(&run, "pull-request").is_none());
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
    assert!(
        git(&w.remote, &["branch", "--list", "keel/*"]).is_empty(),
        "nothing was pushed"
    );
    assert!(!w.log("claude.log").contains("existing retry helper"));

    let next = w.start(None);
    w.wait(&next, "approval", AttemptState::WaitingApproval);
    let log = w.log("claude.log");
    assert_eq!(
        log.matches("Use the existing retry helper").count(),
        2,
        "both agent steps of the next run get it"
    );
    let builder = w.step(&next, "builder").unwrap();
    assert_eq!(builder.system_prompt_sources[0]["source"], "brain");
    assert_eq!(
        builder.system_prompt_sources[0]["decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_retired_decision_stops_reaching_agents() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine
        .answer_approval(&approval.id, false, "An outdated rule")
        .unwrap();
    let decision = w.engine.decisions(&w.project_id).unwrap().remove(0);
    w.engine.retire_decision(decision.seq).unwrap();
    let next = w.start(None);
    w.wait(&next, "approval", AttemptState::WaitingApproval);
    assert!(!w.log("claude.log").contains("An outdated rule"));
}

#[test]
fn an_approval_waits_through_a_restart() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);

    let reopened = engine_for(w.dir.path(), &w.store);
    reopened.recover().unwrap();
    assert_eq!(w.step(&run, "approval").unwrap().state, "waitingApproval");
    assert_eq!(
        reopened.active_count().unwrap(),
        0,
        "nothing is running while it waits"
    );

    reopened.answer_approval(&approval.id, true, "").unwrap();
    assert_eq!(w.wait_run_finished(&run).pr_number, Some(12));
}

#[test]
fn a_run_cut_between_two_steps_continues_on_reopening() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    // The app died right after the approval was recorded, before the next step started.
    w.store
        .set_state(&approval.id, AttemptState::Succeeded, None)
        .unwrap();
    let reopened = engine_for(w.dir.path(), &w.store);
    reopened.recover().unwrap();
    assert_eq!(w.wait_run_finished(&run).pr_number, Some(12));
}

#[test]
fn answering_twice_is_refused() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine
        .answer_approval(&approval.id, false, "Not this way")
        .unwrap();
    assert!(matches!(
        w.engine.answer_approval(&approval.id, true, ""),
        Err(Error::InvalidState)
    ));
    assert!(w.step(&run, "pull-request").is_none());
}

#[test]
fn rejecting_a_change_takes_a_reason_and_approving_does_not() {
    let w = world();
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    for blank in ["", "  \n\t "] {
        assert!(matches!(
            w.engine.answer_approval(&approval.id, false, blank),
            Err(Error::Blocked(Reason::RejectionNeedsReason))
        ));
    }
    // Refused means nothing happened: it still waits, and can still be answered.
    assert_eq!(w.step(&run, "approval").unwrap().state, "waitingApproval");
    assert!(w.store.run(&run).unwrap().finished_at.is_none());
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());

    w.engine.answer_approval(&approval.id, true, "").unwrap();
    assert_eq!(w.wait_run_finished(&run).pr_number, Some(12));
}

#[test]
fn a_run_that_changed_nothing_closes_without_a_reason() {
    let w = world();
    w.flag("no-change");
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, false, "").unwrap();
    assert_eq!(w.step(&run, "approval").unwrap().state, "rejected");
    assert!(w.store.run(&run).unwrap().finished_at.is_some());
    assert!(w.step(&run, "pull-request").is_none());
    assert!(w.engine.decisions(&w.project_id).unwrap().is_empty());
}

#[test]
fn a_failed_delivery_says_why_and_retries_without_repeating_the_agents() {
    let w = world();
    w.flag("pr-sso");
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let failed = w.wait(&run, "pull-request", AttemptState::Blocked);
    let Reason::DeliveryFailed { kind, .. } = reason(&failed) else {
        panic!("{:?}", failed.reason)
    };
    assert_eq!(kind, "sso");

    w.unflag("pr-sso");
    let agents_before = w.log("claude.log").matches("=== CLAUDE").count();
    w.engine.retry(&failed.id, false).unwrap();
    assert_eq!(w.wait_run_finished(&run).pr_number, Some(12));
    assert_eq!(
        w.log("claude.log").matches("=== CLAUDE").count(),
        agents_before
    );
}

#[test]
fn a_pull_request_that_already_exists_counts_as_delivered() {
    let w = world();
    w.flag("pr-exists");
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    assert_eq!(w.wait_run_finished(&run).pr_number, Some(99));
}

#[test]
fn nothing_to_deliver_is_said_plainly() {
    let w = world();
    w.flag("no-change");
    let run = w.start(None);
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let failed = w.wait(&run, "pull-request", AttemptState::Blocked);
    assert_eq!(reason(&failed), Reason::NothingToDeliver);
    assert!(w.log("pr.log").is_empty());
}

#[test]
fn a_workflow_that_cannot_deliver_is_refused_before_any_agent_runs() {
    let w = world();
    w.engine
        .update_project(&w.project_id, &ProjectSettings::default())
        .unwrap();
    let error = w
        .engine
        .start_run(StartRun {
            project_id: w.project_id.clone(),
            title: None,
            request: "x".into(),
            workflow: full_workflow("en", "opus", "sonnet", None),
            language: "en".into(),
            source_issue: None,
            saved_workflow: None,
            started_by: None,
        })
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Blocked(Reason::GithubAccountMissing)
    ));
    assert!(w.log("claude.log").is_empty());
}

#[test]
fn github_status_shows_accounts_permission_and_excess_scopes_without_switching() {
    let w = world();
    let status = w
        .engine
        .github_status(&w.project_id, Some("alice"))
        .unwrap();
    assert_eq!(status.repo.as_deref(), Some("acme/app"));
    assert_eq!(status.accounts.len(), 2);
    assert!(status.accounts.iter().any(|a| a.login == "bob" && a.active));
    assert_eq!(status.excess_scopes, ["admin:org"]);
    let info = status.info.unwrap();
    assert_eq!(info.default_branch, "main");
    assert_eq!(info.default_branch_protected, Some(false));
    assert!(!w.log("gh.log").contains("auth switch"));
}

#[test]
fn choosing_an_account_compares_the_map_with_what_is_on_github() {
    let w = world();
    let account = |login: Option<&str>| keel_store::ProjectSettings {
        github_account: login.map(str::to_owned),
        ..Default::default()
    };
    let run = w.start(None);
    let attempt = w.wait(&run, "approval", AttemptState::WaitingApproval);

    // The map came while the project had no account.
    w.engine
        .update_project(&w.project_id, &account(None))
        .unwrap();
    let commit = git(&w.repo, &["rev-parse", "main"]);
    let readme = vec!["README.md".to_owned()];
    w.store
        .add_brain_note(&keel_store::NewBrainNote {
            project_id: &w.project_id,
            kind: "map",
            state: "active",
            text: "The readme says what it is",
            evidence: None,
            paths: &readme,
            commit_sha: Some(commit.trim()),
            run_id: &run,
            attempt_id: &attempt.id,
        })
        .unwrap();
    assert!(!w.engine.brain(&w.project_id).unwrap().map[0].stale);

    // Someone published a change since this clone last looked.
    let other = w.dir.path().join("other");
    git(
        w.dir.path(),
        &["clone", "-q", &w.remote.to_string_lossy(), "other"],
    );
    git(&other, &["config", "user.name", "Other"]);
    git(&other, &["config", "user.email", "o@example.com"]);
    std::fs::write(other.join("README.md"), "changed\n").unwrap();
    git(&other, &["commit", "-qam", "change"]);
    git(&other, &["push", "-q", "origin", "main"]);

    w.engine
        .update_project(&w.project_id, &account(Some("alice")))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !w.engine.brain(&w.project_id).unwrap().map[0].stale {
        assert!(Instant::now() < deadline, "{}", w.log("gh.log"));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!w.log("gh.log").contains("auth switch"));
}

#[test]
fn choosing_an_account_asks_github_nothing_without_a_map_to_compare() {
    let w = world();
    let settings = keel_store::ProjectSettings {
        github_account: Some("bob".into()),
        ..Default::default()
    };
    w.engine.update_project(&w.project_id, &settings).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!w.log("gh.log").contains("auth token"));
}

#[test]
fn github_status_checks_the_chosen_account_before_it_is_saved() {
    let w = world();
    let status = w.engine.github_status(&w.project_id, Some("bob")).unwrap();
    assert_eq!(status.info.unwrap().permission, "WRITE");
    let gh = w.log("gh.log");
    assert!(gh.contains("auth token --user bob"), "{gh}");
    assert!(!gh.contains("auth token --user alice"), "{gh}");
    assert!(!gh.contains("auth switch"), "{gh}");
    let nobody = w.engine.github_status(&w.project_id, None).unwrap();
    assert!(nobody.info.is_none());
    assert_eq!(nobody.accounts.len(), 2);
}

#[test]
fn github_status_says_when_an_account_cannot_see_the_repo() {
    let w = world();
    w.flag("repo-hidden");
    let status = w
        .engine
        .github_status(&w.project_id, Some("alice"))
        .unwrap();
    assert!(status.info.is_none());
    assert_eq!(status.accounts.len(), 2);
    match status.error {
        Some(Reason::DeliveryFailed { kind, .. }) => assert_eq!(kind, "noPermission"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn github_status_never_asks_for_the_token_of_an_account_gh_does_not_have() {
    let w = world();
    let status = w
        .engine
        .github_status(&w.project_id, Some("--hostname=evil.example"))
        .unwrap();
    assert!(status.info.is_none());
    assert!(status.error.is_none());
    assert!(!w.log("gh.log").contains("auth token"));
}

#[test]
fn an_issue_is_imported_as_data_and_linked_from_the_pull_request() {
    let w = world();
    let issues = w.engine.issues(&w.project_id).unwrap();
    assert_eq!(
        issues[0],
        Issue {
            number: 7,
            title: "Broken".into(),
            author: "carol".into(),
            url: "https://github.com/acme/app/issues/7".into(),
            body: "It breaks".into()
        }
    );
    let issue = issues[0].clone();
    let run = w
        .engine
        .start_run(StartRun {
            project_id: w.project_id.clone(),
            title: Some(format!("#{} {}", issue.number, issue.title)),
            request: request_from_issue(&issue, ""),
            workflow: full_workflow("en", "opus", "sonnet", None),
            language: "en".into(),
            source_issue: Some(issue),
            saved_workflow: None,
            started_by: None,
        })
        .unwrap();
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    assert!(w.log("claude.log").contains("hasn't been verified"));
    w.engine.answer_approval(&approval.id, true, "").unwrap();
    let finished = w.wait_run_finished(&run);
    assert_eq!(finished.title, "#7 Broken");
    assert_eq!(finished.source_issue.unwrap()["number"], 7);
    assert!(w.log("pr-body.md").contains("Refs #7"));
}

#[test]
fn step_kinds_appear_in_order_with_a_shared_workspace() {
    let w = world();
    let run = w.start(Some("true"));
    let approval = w.wait(&run, "approval", AttemptState::WaitingApproval);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    let kinds: Vec<&str> = attempts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["agent", "agent", "check", "approval"]);
    assert!(attempts.iter().all(|a| a.workspace == approval.workspace));
    let workflow: Vec<WorkflowStep> =
        serde_json::from_value(w.store.run(&run).unwrap().workflow).unwrap();
    assert_eq!(workflow.len(), 5, "the run keeps a copy of its workflow");
}
