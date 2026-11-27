//! Adding and configuring projects, as the tools do it. They call the tools' own methods, with the
//! real engine and store and a fake `claude` and `gh`, so they need no socket and run wherever the
//! rest do. `tests/http.rs` has what a client sees over the wire.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{AttemptState, CardLane, NeededDomain, Reason, WorkflowStep};
use keel_engine::{Config, Engine};
use keel_harness::Tools;
use keel_store::{McpClient, NewAttempt, NewRun, ProjectSettings, Store};
use serde_json::{Value as Json, json};

use crate::tools::{
    AddProjectArgs, ConfigureProjectArgs, GetRunArgs, ListProjectsArgs, ProposeProjectArgs,
    StopRunArgs,
};
use crate::{INSTRUCTIONS, McpServer, ToolError};

const CLAUDE: &str = r#"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}'; exit 0;;
esac
cat > /dev/null
exit 1
"#;

const GH: &str = r#"#!/bin/bash
case "$1 $2" in
"auth status") echo '{"hosts":{"github.com":[{"login":"alice","active":false,"state":"success","scopes":"repo"},{"login":"bob","active":true,"state":"error","scopes":"repo"}]}}'; exit 0;;
esac
exit 1
"#;

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    server: McpServer,
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn world() -> World {
    world_with(|_| {})
}

fn world_with(set: impl FnOnce(&mut Config)) -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    keel_harness::testing::write_script(&bin.join("claude"), CLAUDE).unwrap();
    keel_harness::testing::write_script(&bin.join("gh"), GH).unwrap();
    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    set(&mut config);
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let server = McpServer::new(engine.clone(), store.clone());
    World {
        dir,
        store,
        engine,
        server,
    }
}

impl World {
    /// A repo with a commit that isn't in Keel, as git names it.
    fn repo(&self, name: &str) -> String {
        let repo = self.dir.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        canonical(&repo)
    }

    fn client(&self, name: &str, can_run: bool) -> McpClient {
        self.server.create_client(name, can_run).unwrap().0
    }

    /// A client the person let add and configure projects.
    fn manager(&self) -> McpClient {
        let client = self.client("Bot", true);
        self.store
            .set_mcp_client_can_manage_projects(&client.id, true)
            .unwrap()
    }

    async fn add(&self, client: &McpClient, args: Json) -> Result<Json, ToolError> {
        let args: AddProjectArgs = serde_json::from_value(args).unwrap();
        self.server.add_project(client.clone(), args).await
    }

    async fn configure(&self, client: &McpClient, args: Json) -> Result<Json, ToolError> {
        let args: ConfigureProjectArgs = serde_json::from_value(args).unwrap();
        self.server.configure_project(client.clone(), args).await
    }

    async fn list(&self, client: &McpClient) -> Json {
        self.server
            .list_projects(client.clone(), ListProjectsArgs {})
            .await
            .unwrap()
    }

    async fn propose(&self, client: &McpClient, path: &str) -> Json {
        self.propose_answer(client, path).await.unwrap()
    }

    async fn propose_answer(&self, client: &McpClient, path: &str) -> Result<Json, ToolError> {
        let args = ProposeProjectArgs { path: path.into() };
        self.server.propose_project(client.clone(), args).await
    }

    /// A run whose builder waits for the person to say whether the project may reach npm.
    fn waiting_for_a_domain(&self, project_id: &str, run_id: &str, attempt_id: &str) {
        let workflow = json!([{ "type": "agent", "id": "builder", "model": "m", "effort": null, "tools": "build", "prompt": "p", "maxTurns": null, "maxBudgetUsd": null }]);
        self.store
            .add_run(&NewRun {
                id: run_id.into(),
                project_id: project_id.into(),
                title: "Upgrade next".into(),
                request: "Upgrade next".into(),
                workflow: workflow.to_string(),
                language: "en".into(),
                source_issue: None,
                request_written: None,
                workflow_id: None,
                workflow_name: None,
                started_by: None,
            })
            .unwrap();
        self.store
            .add_attempt(&NewAttempt {
                id: attempt_id.into(),
                run_id: run_id.into(),
                step_id: "builder".into(),
                kind: "agent".into(),
                role: "worker".into(),
                round: 0,
                number: 1,
                state: AttemptState::Blocked,
                model_requested: "m".into(),
                effort: None,
                tools: "build".into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: "p".into(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: "s".into(),
                resumed_from: None,
                workspace: "/nowhere".into(),
                raw_path: "/nowhere/raw".into(),
                started_at: keel_store::now(),
            })
            .unwrap();
        let reason = Reason::NetworkNeeded {
            needed: vec![NeededDomain {
                domain: "registry.npmjs.org".into(),
                why: Some("to install next".into()),
                tool: None,
                frees_offline: false,
            }],
        };
        self.store
            .set_state(attempt_id, AttemptState::Blocked, Some(&reason))
            .unwrap();
    }

    /// Whether a run that writes this project's map was started.
    fn mapping(&self, project_id: &str) -> bool {
        self.store.runs(50).unwrap().iter().any(|row| {
            let run = self.store.run(&row.id).unwrap();
            let steps: Vec<WorkflowStep> = serde_json::from_value(run.workflow).unwrap();
            run.project_id == project_id
                && steps.iter().any(|step| {
                    matches!(step, WorkflowStep::Agent(spec) if spec.role == keel_core::StepRole::Mapper)
                })
        })
    }

    /// Nothing was added or changed, and nothing is on record as if it had been.
    fn untouched(&self) {
        assert!(self.store.projects().unwrap().is_empty());
        assert!(self.store.runs(10).unwrap().is_empty());
        assert!(self.store.mcp_project_changes(10).unwrap().is_empty());
    }
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Each recorded value as `(field, before, after)`.
fn values(change: &keel_store::McpProjectChange) -> Vec<(&str, Option<&str>, Option<&str>)> {
    change
        .changes
        .iter()
        .map(|c| (c.field.as_str(), c.before.as_deref(), c.after.as_deref()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_the_person_lets_adds_a_project_which_is_mapped_and_on_record() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");

    let answer = w
        .add(
            &bot,
            json!({ "path": path, "githubAccount": "alice", "setupCommand": " pnpm install ", "checkCommand": "pnpm test" }),
        )
        .await
        .unwrap();

    let projects = w.store.projects().unwrap();
    assert_eq!(projects.len(), 1);
    let project = &projects[0];
    assert_eq!(
        project.name, "app",
        "named after its folder, as the app does"
    );
    assert_eq!(project.path, path);
    assert_eq!(project.github_account.as_deref(), Some("alice"));
    assert_eq!(project.setup_command.as_deref(), Some("pnpm install"));
    assert_eq!(project.check_command.as_deref(), Some("pnpm test"));
    assert_eq!(answer["state"], "added");
    assert_eq!(answer["project"]["id"], json!(project.id));
    assert_eq!(answer["project"]["setupCommand"], "pnpm install");
    assert_eq!(answer["map"]["status"], "started", "{answer}");
    assert!(w.mapping(&project.id));
    // So its first map reads the folder; one the person added maps as it always did.
    assert!(w.store.project_added_by_mcp_client(&project.id).unwrap());
    let theirs = w.repo("theirs");
    let theirs = w.engine.add_project(Path::new(&theirs)).unwrap();
    assert!(!w.store.project_added_by_mcp_client(&theirs.id).unwrap());

    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].client_id, bot.id);
    assert_eq!(changes[0].client_name, "Bot");
    assert_eq!(changes[0].project_id, project.id);
    assert_eq!(changes[0].project_name, "app");
    assert_eq!(changes[0].project_path, path);
    assert_eq!(changes[0].github_repo, None);
    assert_eq!(changes[0].action, "added");
    assert_eq!(
        values(&changes[0]),
        [
            ("githubAccount", None, Some("alice")),
            ("setupCommand", None, Some("pnpm install")),
            ("checkCommand", None, Some("pnpm test")),
        ]
    );
    let calls = w.store.mcp_calls(10).unwrap();
    assert_eq!(calls[0].tool, "add_project");
    assert!(calls[0].ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_map_reads_the_folder_even_when_the_account_cannot_read_github() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    // On GitHub as far as the folder says, where the fake `gh` gives no account a token.
    git(
        Path::new(&path),
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );

    let answer = w
        .add(&bot, json!({ "path": path, "githubAccount": "alice" }))
        .await
        .unwrap();

    let project = w.store.projects().unwrap().remove(0);
    assert_eq!(project.github_repo.as_deref(), Some("acme/app"));
    assert_eq!(answer["map"]["status"], "started", "{answer}");
    let run = answer["mapRunId"].as_str().unwrap().to_owned();
    // Until it has its copy, or failed before it. Inside a sandbox, git can't start the one of its
    // own it runs in, so there it fails for that, and still never asks GitHub.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let failed = loop {
        if w.store.run(&run).unwrap().branch.is_some() {
            break None;
        }
        let attempts = w.store.attempts_of_run(&run).unwrap();
        if let Some(reason) = attempts.iter().find_map(|a| a.reason.clone()) {
            break Some(reason);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the map never got its copy of the code"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    if let Some(reason) = failed {
        assert_eq!(reason["code"], "prepareFailed", "{reason}");
        let detail = reason["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("sandbox"), "{reason}");
    }
    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(changes[0].project_path, path);
    assert_eq!(changes[0].github_repo.as_deref(), Some("acme/app"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_map_of_a_project_it_adds_is_a_run_of_the_client_within_its_limit() {
    let w = world();
    let bot = w.manager();
    let bot = w.store.set_mcp_client_max_open_runs(&bot.id, 1).unwrap();
    let first = w.repo("app");
    let second = w.repo("other");

    let answer = w
        .add(&bot, json!({ "path": first, "githubAccount": "alice" }))
        .await
        .unwrap();
    let run = answer["mapRunId"].as_str().unwrap();
    assert!(w.store.run_started_by_mcp_client(run, &bot.id).unwrap());
    assert_eq!(
        w.store.mcp_calls(10).unwrap()[0].run_id.as_deref(),
        Some(run)
    );
    assert_eq!(w.store.open_runs_of_mcp_client(&bot.id).unwrap(), 1);

    let refusal = w
        .add(&bot, json!({ "path": second, "githubAccount": "alice" }))
        .await
        .unwrap_err()
        .0;
    assert!(refusal.contains("as many runs open"), "{refusal}");
    assert_eq!(w.store.projects().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_left_empty_leave_the_project_without_them() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");

    w.add(
        &bot,
        json!({ "path": path, "githubAccount": "alice", "setupCommand": "   " }),
    )
    .await
    .unwrap();

    let project = w.store.projects().unwrap().remove(0);
    assert_eq!(project.setup_command, None);
    assert_eq!(project.check_command, None);
    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(
        values(&changes[0]),
        [("githubAccount", None, Some("alice"))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_permission_nothing_is_added_or_configured() {
    let w = world();
    let reader = w.client("Reader", false);
    let runner = w.client("Runner", true);
    // Even asked for, a client that may only read doesn't get it.
    let refused = w
        .store
        .set_mcp_client_can_manage_projects(&reader.id, true)
        .unwrap();
    assert!(!refused.can_manage_projects);
    let path = w.repo("app");
    let add = json!({ "path": path, "githubAccount": "alice" });

    let refusal = w.add(&reader, add.clone()).await.unwrap_err().0;
    assert!(refusal.contains("may only read"), "{refusal}");
    let refusal = w.add(&runner, add.clone()).await.unwrap_err().0;
    assert!(
        refusal.contains("may not add or configure projects"),
        "{refusal}"
    );
    assert!(refusal.contains("Keel's settings"), "{refusal}");
    w.untouched();

    let project = w.engine.add_project(Path::new(&path)).unwrap();
    let change = json!({ "project": project.id, "checkCommand": "true" });
    let refusal = w.configure(&reader, change.clone()).await.unwrap_err().0;
    assert!(refusal.contains("may only read"), "{refusal}");
    let refusal = w.configure(&runner, change).await.unwrap_err().0;
    assert!(
        refusal.contains("may not add or configure projects"),
        "{refusal}"
    );
    assert_eq!(w.store.project(&project.id).unwrap().check_command, None);
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());
    // Each refusal is a call on record.
    let calls = w.store.mcp_calls(10).unwrap();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|call| !call.ok));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_taken_away_holds_even_for_a_call_already_let_in() {
    let w = world();
    // As the server authenticated it, a moment before the person turned it off.
    let bot = w.manager();
    w.store
        .set_mcp_client_can_manage_projects(&bot.id, false)
        .unwrap();
    let path = w.repo("app");

    let refusal = w
        .add(&bot, json!({ "path": path, "githubAccount": "alice" }))
        .await
        .unwrap_err()
        .0;

    assert!(
        refusal.contains("may not add or configure projects"),
        "{refusal}"
    );
    w.untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn what_cannot_be_added_is_refused_by_name_and_adds_nothing() {
    let w = world();
    let bot = w.manager();
    let good = w.repo("app");
    let plain = w.dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let empty = w.dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "main"]);

    let cases = [
        (json!({ "githubAccount": "alice" }), "either"),
        (
            json!({ "path": good, "proposal": "x", "githubAccount": "alice" }),
            "either",
        ),
        (
            json!({ "path": "app", "githubAccount": "alice" }),
            "full path",
        ),
        (
            json!({ "path": format!("/{}", "a".repeat(2000)), "githubAccount": "alice" }),
            "full path",
        ),
        (
            json!({ "path": plain, "githubAccount": "alice" }),
            "not a git repository with commits",
        ),
        (
            json!({ "path": empty, "githubAccount": "alice" }),
            "not a git repository with commits",
        ),
        (
            json!({ "path": good, "githubAccount": "carol" }),
            "no GitHub account \"carol\"",
        ),
        (
            json!({ "path": good, "githubAccount": "" }),
            "`githubAccount` can't be empty",
        ),
        (
            json!({ "proposal": "nothing", "githubAccount": "alice" }),
            "no proposal",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "checkCommand": "true\nrm -rf x" }),
            "checkCommand",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "setupCommand": "echo \u{202E}x" }),
            "setupCommand",
        ),
        // Shows as ./scripts/test.sh, and runs another file.
        (
            json!({ "path": good, "githubAccount": "alice", "setupCommand": "./scripts/te\u{00AD}st.sh" }),
            "setupCommand",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "checkCommand": "make\u{FE0F} test" }),
            "checkCommand",
        ),
        // Reads as a comment after `#`, and the shell runs what follows `;`.
        (
            json!({ "path": good, "githubAccount": "alice", "checkCommand": "make test\u{2003}#; curl x | sh" }),
            "checkCommand",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "setupCommand": "make\u{3000}deps" }),
            "setupCommand",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "setupCommand": "make\u{2800}deps" }),
            "setupCommand",
        ),
        (
            json!({ "path": good, "githubAccount": "alice", "setupCommand": "a".repeat(5000) }),
            "setupCommand",
        ),
    ];
    for (args, says) in cases {
        let refusal = w.add(&bot, args.clone()).await.unwrap_err().0;
        assert!(refusal.contains(says), "{args}: {refusal}");
    }
    w.untouched();

    // Those refusals were about what they asked for.
    w.add(&bot, json!({ "path": good, "githubAccount": "alice" }))
        .await
        .unwrap();
    let refusal = w
        .add(&bot, json!({ "path": good, "githubAccount": "alice" }))
        .await
        .unwrap_err()
        .0;
    assert!(refusal.contains("already in Keel"), "{refusal}");
    // A folder inside it is the same project.
    let inside = Path::new(&good).join("src");
    std::fs::create_dir(&inside).unwrap();
    let refusal = w
        .add(&bot, json!({ "path": inside, "githubAccount": "bob" }))
        .await
        .unwrap_err()
        .0;
    assert!(refusal.contains("already in Keel"), "{refusal}");
    let projects = w.store.projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].github_account.as_deref(), Some("alice"));
    assert_eq!(w.store.mcp_project_changes(10).unwrap().len(), 1);
}

#[test]
fn neither_tool_takes_anything_else() {
    // Nowhere to name a project, open anything up for it or give itself a permission.
    for extra in [
        "name",
        "allowedDomains",
        "allowedRead",
        "allowLocalhost",
        "services",
        "env",
        "defaultWorkflow",
        "sensitivePaths",
        "issueLabel",
        "canManageProjects",
    ] {
        let mut add = json!({ "path": "/x", "githubAccount": "alice" });
        add[extra] = json!("x");
        let error = serde_json::from_value::<AddProjectArgs>(add).unwrap_err();
        assert!(error.to_string().contains(extra), "{error}");
        let mut change = json!({ "project": "app" });
        change[extra] = json!("x");
        let error = serde_json::from_value::<ConfigureProjectArgs>(change).unwrap_err();
        assert!(error.to_string().contains(extra), "{error}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_from_a_proposal_settles_it() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    let proposal = w.propose(&bot, &path).await;
    let id = proposal["proposalId"].as_str().unwrap();

    let answer = w
        .add(
            &bot,
            json!({ "proposal": id, "githubAccount": "alice", "checkCommand": "true" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["state"], "added");
    assert_eq!(answer["project"]["path"], json!(path));
    let listed = w.list(&bot).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);
    assert_eq!(listed["proposals"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_by_path_settles_a_proposal_of_that_folder_too() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    w.propose(&bot, &path).await;

    w.add(&bot, json!({ "path": path, "githubAccount": "alice" }))
        .await
        .unwrap();

    assert!(w.engine.project_proposals().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_whose_folder_is_no_longer_a_repo_adds_nothing() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    let proposal = w.propose(&bot, &path).await;
    std::fs::remove_dir_all(Path::new(&path).join(".git")).unwrap();

    let args = json!({ "proposal": proposal["proposalId"], "githubAccount": "alice" });
    let refusal = w.add(&bot, args).await.unwrap_err().0;

    assert!(
        refusal.contains("not a git repository with commits"),
        "{refusal}"
    );
    w.untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_project_comes_back_with_its_history_and_what_the_client_gave() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    let project = w.engine.add_project(Path::new(&path)).unwrap();
    w.engine
        .update_project(
            &project.id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                setup_command: Some("make deps".into()),
                check_command: Some("make test".into()),
                allowed_domains: vec!["example.com".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let card = w.engine.card_create(&project.id, "kept", "").unwrap();
    w.engine.remove_project(&project.id).unwrap();
    // Moved to GitHub while it was out of Keel.
    git(
        Path::new(&path),
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );

    let answer = w
        .add(
            &bot,
            json!({ "path": path, "githubAccount": "bob", "checkCommand": "make test" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["state"], "restored");
    assert_eq!(answer["map"], Json::Null);
    assert_eq!(answer["project"]["id"], json!(project.id));
    let back = w.store.projects().unwrap().remove(0);
    assert_eq!(back.github_account.as_deref(), Some("bob"));
    assert_eq!(back.setup_command, None);
    assert_eq!(back.check_command.as_deref(), Some("make test"));
    assert_eq!(back.allowed_domains, ["example.com"]);
    assert_eq!(w.store.card(&card).unwrap().lane, CardLane::Backlog);
    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(changes[0].action, "restored");
    assert_eq!(back.github_repo.as_deref(), Some("acme/app"));
    assert_eq!(changes[0].github_repo.as_deref(), Some("acme/app"));
    // Everything it comes back with or loses, what the client set as it already was included.
    assert_eq!(
        values(&changes[0]),
        [
            ("githubAccount", Some("alice"), Some("bob")),
            ("setupCommand", Some("make deps"), None),
            ("checkCommand", Some("make test"), Some("make test")),
            ("allowedDomains", Some("example.com"), Some("example.com")),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn configuring_changes_only_what_is_given_and_records_before_and_after() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    let project = w.engine.add_project(Path::new(&path)).unwrap();
    w.engine
        .update_project(
            &project.id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                setup_command: Some("make deps".into()),
                check_command: Some("make test".into()),
                allowed_domains: vec!["example.com".into()],
                allow_localhost: true,
                sensitive_paths: vec!["secrets/**".into()],
                ..Default::default()
            },
        )
        .unwrap();

    let answer = w
        .configure(
            &bot,
            json!({ "project": "app", "githubAccount": "bob", "checkCommand": "make check" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["project"]["githubAccount"], "bob");
    assert_eq!(answer["project"]["setupCommand"], "make deps");
    assert_eq!(answer["project"]["checkCommand"], "make check");
    let now = w.store.project(&project.id).unwrap();
    assert_eq!(now.setup_command.as_deref(), Some("make deps"));
    assert_eq!(now.allowed_domains, ["example.com"]);
    assert_eq!(now.sensitive_paths, ["secrets/**"]);
    assert!(now.allow_localhost);

    // Empty takes the command away.
    w.configure(&bot, json!({ "project": project.id, "setupCommand": "" }))
        .await
        .unwrap();
    assert_eq!(w.store.project(&project.id).unwrap().setup_command, None);

    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(changes.len(), 2, "newest first");
    assert!(changes.iter().all(|c| c.action == "configured"));
    assert!(changes.iter().all(|c| c.client_id == bot.id));
    assert_eq!(
        values(&changes[0]),
        [("setupCommand", Some("make deps"), None)]
    );
    assert_eq!(
        values(&changes[1]),
        [
            ("githubAccount", Some("alice"), Some("bob")),
            ("checkCommand", Some("make test"), Some("make check")),
        ]
    );

    // Setting what's already there changes nothing, so there's nothing to record.
    w.configure(&bot, json!({ "project": "app", "githubAccount": "bob" }))
        .await
        .unwrap();
    assert_eq!(w.store.mcp_project_changes(10).unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_cannot_be_configured_is_refused_and_changes_nothing() {
    let w = world();
    let bot = w.manager();
    let path = w.repo("app");
    let project = w.engine.add_project(Path::new(&path)).unwrap();
    let gone = w.repo("gone");
    let gone = w.engine.add_project(Path::new(&gone)).unwrap();
    w.engine.remove_project(&gone.id).unwrap();

    let cases = [
        (json!({ "project": "app" }), "what to change"),
        (
            json!({ "project": "nothing", "checkCommand": "true" }),
            "no project",
        ),
        (
            json!({ "project": gone.id, "checkCommand": "true" }),
            "no project",
        ),
        // One wrong value, and the right one beside it isn't saved either.
        (
            json!({ "project": "app", "githubAccount": "carol", "checkCommand": "true" }),
            "no GitHub account \"carol\"",
        ),
        (
            json!({ "project": "app", "githubAccount": "", "checkCommand": "true" }),
            "no GitHub account",
        ),
        (
            json!({ "project": "app", "checkCommand": "a\u{7}b" }),
            "checkCommand",
        ),
    ];
    for (args, says) in cases {
        let refusal = w.configure(&bot, args.clone()).await.unwrap_err().0;
        assert!(refusal.contains(says), "{args}: {refusal}");
    }
    let now = w.store.project(&project.id).unwrap();
    assert_eq!((now.github_account, now.check_command), (None, None));
    assert_eq!(w.store.project(&gone.id).unwrap().check_command, None);
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());
}

/// A project in Keel that may reach these domains.
fn with_domains(w: &World, domains: &[&str]) -> keel_store::Project {
    let path = w.repo("app");
    let project = w.engine.add_project(Path::new(&path)).unwrap();
    w.engine
        .update_project(
            &project.id,
            &ProjectSettings {
                allowed_domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                ..Default::default()
            },
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn configuring_adds_and_removes_domains_and_records_before_and_after() {
    let w = world();
    let bot = w.manager();
    let project = with_domains(&w, &["example.com", "pypi.org"]);

    let answer = w
        .configure(
            &bot,
            json!({
                "project": "app",
                "addAllowedDomains": ["registry.npmjs.org", "*.example.org", "PYPI.org"],
                "removeAllowedDomains": ["Example.com", "never.example"],
            }),
        )
        .await
        .unwrap();

    let expected = ["pypi.org", "registry.npmjs.org", "*.example.org"];
    assert_eq!(answer["project"]["allowedDomains"], json!(expected));
    assert_eq!(
        w.store.project(&project.id).unwrap().allowed_domains,
        expected
    );
    let changes = w.store.mcp_project_changes(10).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].action, "configured");
    assert_eq!(
        values(&changes[0]),
        [(
            "allowedDomains",
            Some("example.com\npypi.org"),
            Some("pypi.org\nregistry.npmjs.org\n*.example.org")
        )]
    );

    // What's already so changes nothing and records nothing.
    w.configure(
        &bot,
        json!({ "project": "app", "addAllowedDomains": ["pypi.org"], "removeAllowedDomains": ["never.example"] }),
    )
    .await
    .unwrap();
    assert_eq!(w.store.mcp_project_changes(10).unwrap().len(), 1);

    // Taking the last one away leaves none.
    w.configure(
        &bot,
        json!({ "project": "app", "removeAllowedDomains": expected }),
    )
    .await
    .unwrap();
    assert!(
        w.store
            .project(&project.id)
            .unwrap()
            .allowed_domains
            .is_empty()
    );
    assert_eq!(
        values(&w.store.mcp_project_changes(10).unwrap()[0]),
        [(
            "allowedDomains",
            Some("pypi.org\nregistry.npmjs.org\n*.example.org"),
            None
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_client_the_person_lets_sees_the_domains() {
    let w = world();
    let bot = w.manager();
    let runner = w.client("Runner", true);
    let reader = w.client("Reader", false);
    let project = with_domains(&w, &["example.com"]);

    let listed = w.list(&bot).await;
    assert_eq!(
        listed["projects"][0]["allowedDomains"],
        json!(["example.com"])
    );
    for client in [&runner, &reader] {
        let listed = w.list(client).await;
        assert_eq!(listed["projects"][0]["id"], json!(project.id));
        assert!(!listed.to_string().contains("example.com"), "{listed}");
        assert!(!listed.to_string().contains("allowedDomains"), "{listed}");
        let proposed = w.propose_answer(client, &project.path).await;
        if let Ok(proposed) = proposed {
            assert!(!proposed.to_string().contains("example.com"), "{proposed}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn domains_are_refused_without_the_permission_or_when_one_is_no_domain() {
    let w = world();
    let bot = w.manager();
    let runner = w.client("Runner", true);
    let reader = w.client("Reader", false);
    let project = with_domains(&w, &["example.com"]);
    let change = json!({ "project": "app", "addAllowedDomains": ["pypi.org"], "removeAllowedDomains": ["example.com"] });

    for (client, says) in [
        (&runner, "may not add or configure"),
        (&reader, "only read"),
    ] {
        let refusal = w.configure(client, change.clone()).await.unwrap_err().0;
        assert!(refusal.contains(says), "{refusal}");
    }
    for bad in [
        "https://pypi.org",
        "pypi.org/simple",
        "pypi.org:443",
        "pypi .org",
        " pypi.org",
        "",
        "*",
        "*.",
        "-pypi.org",
        "pypi..org",
    ] {
        // The good ones beside it aren't saved either.
        let args = json!({ "project": "app", "checkCommand": "true", "addAllowedDomains": ["good.example", bad] });
        let refusal = w.configure(&bot, args).await.unwrap_err().0;
        assert!(refusal.contains("isn't a domain"), "{bad:?}: {refusal}");
    }
    let both = w
        .configure(
            &bot,
            json!({ "project": "app", "addAllowedDomains": ["pypi.org"], "removeAllowedDomains": ["PyPI.org"] }),
        )
        .await
        .unwrap_err()
        .0;
    assert!(both.contains("both"), "{both}");
    // Empty lists say nothing to change.
    let nothing = w
        .configure(
            &bot,
            json!({ "project": "app", "addAllowedDomains": [], "removeAllowedDomains": [] }),
        )
        .await
        .unwrap_err()
        .0;
    assert!(nothing.contains("what to change"), "{nothing}");

    let now = w.store.project(&project.id).unwrap();
    assert_eq!(now.allowed_domains, ["example.com"]);
    assert_eq!(now.check_command, None);
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());
}

/// So that each one can be read where Keel shows what a client changed.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_adds_only_so_many_domains_in_one_call() {
    let w = world();
    let bot = w.manager();
    let project = with_domains(&w, &["example.com"]);
    let named =
        |count: usize| -> Vec<String> { (0..count).map(|n| format!("cdn{n}.example")).collect() };
    let most = keel_engine::MOST_DOMAINS_ADDED;
    assert_eq!(most, 20);

    let args = json!({ "project": "app", "addAllowedDomains": named(most + 1) });
    let refusal = w.configure(&bot, args).await.unwrap_err().0;
    assert!(refusal.contains("has 21 domains"), "{refusal}");
    assert!(refusal.contains("at most 20"), "{refusal}");
    assert!(refusal.contains("nothing was changed"), "{refusal}");
    assert_eq!(
        w.store.project(&project.id).unwrap().allowed_domains,
        ["example.com"]
    );
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());

    // As many as that are added, and taking away has no limit: it opens nothing.
    w.configure(
        &bot,
        json!({ "project": "app", "addAllowedDomains": named(most) }),
    )
    .await
    .unwrap();
    assert_eq!(
        w.store.project(&project.id).unwrap().allowed_domains.len(),
        most + 1
    );
    let mut all = named(most);
    all.push("example.com".into());
    w.configure(
        &bot,
        json!({ "project": "app", "removeAllowedDomains": all }),
    )
    .await
    .unwrap();
    assert!(
        w.store
            .project(&project.id)
            .unwrap()
            .allowed_domains
            .is_empty()
    );
    // The tool says the limit where a client reads how to use it.
    let schema =
        serde_json::to_string(&schemars::schema_for!(crate::tools::ConfigureProjectArgs)).unwrap();
    assert!(
        schema.contains(&format!("At most {most} in one call")),
        "{schema}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_adds_no_domain_for_this_mac_the_local_network_or_an_address() {
    let w = world();
    let bot = w.manager();
    let project = with_domains(&w, &["example.com"]);
    for local in [
        "localhost",
        "LocalHost",
        "*.localhost",
        "api.localhost",
        "*.local",
        "printer.LOCAL",
        "127.0.0.1",
        "10.0.0.1",
        "169.254.169.254",
        "127.1",
        "2130706433",
        "0x7f000001",
        "127.0.0.0x1",
        "*.0.0.1",
        // A machine on the network this Mac is on, and the names such networks give theirs.
        "router",
        "NAS",
        "*.lan",
        "nas.lan",
        "x.internal",
        "*.internal",
        "*.build.Internal",
        "x.home",
        "x.home.arpa",
        "*.home.arpa",
        "wiki.intranet",
        "*.intranet",
        "git.corp",
        "*.corp",
        "box.localdomain",
        "*.localdomain",
        // No name at all under the wildcard.
        "*.com",
    ] {
        // The good one beside it isn't saved either.
        let args = json!({ "project": "app", "addAllowedDomains": ["good.example", local] });
        let refusal = w.configure(&bot, args).await.unwrap_err().0;
        assert!(refusal.contains("only the person"), "{local:?}: {refusal}");
        assert!(refusal.contains("nothing was changed"), "{refusal}");
    }
    assert_eq!(
        w.store.project(&project.id).unwrap().allowed_domains,
        ["example.com"]
    );
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());

    // Names on the internet that only look like those.
    let public = [
        "internal.example.com",
        "*.internal.example.com",
        "lan.example",
        "corp.example",
        "home.example",
        "arpa.example",
    ];
    w.configure(
        &bot,
        json!({ "project": "app", "addAllowedDomains": public }),
    )
    .await
    .unwrap();
    let mut held = vec!["example.com"];
    held.extend(public);
    assert_eq!(w.store.project(&project.id).unwrap().allowed_domains, held);
    w.configure(
        &bot,
        json!({ "project": "app", "removeAllowedDomains": public }),
    )
    .await
    .unwrap();

    // One the person allowed, the client may take away.
    let project = w
        .engine
        .update_project(
            &project.id,
            &ProjectSettings {
                allowed_domains: vec!["localhost".into(), "10.0.0.1".into()],
                ..Default::default()
            },
        )
        .unwrap();
    w.configure(
        &bot,
        json!({ "project": "app", "removeAllowedDomains": ["localhost", "10.0.0.1"] }),
    )
    .await
    .unwrap();
    assert!(
        w.store
            .project(&project.id)
            .unwrap()
            .allowed_domains
            .is_empty()
    );
}

/// One saved before domains were checked is no domain, and still has to be something to take away.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_takes_away_a_saved_value_that_is_no_domain() {
    let w = world();
    let bot = w.manager();
    let project = with_domains(&w, &["example.com"]);
    let old = ["https://old.example.com/x", "two words"];
    w.store
        .update_project(
            &project.id,
            &ProjectSettings {
                allowed_domains: vec!["example.com".into(), old[0].into(), old[1].into()],
                ..Default::default()
            },
        )
        .unwrap();

    let answer = w
        .configure(
            &bot,
            json!({ "project": "app", "removeAllowedDomains": old }),
        )
        .await
        .unwrap();

    assert_eq!(answer["project"]["allowedDomains"], json!(["example.com"]));
    assert_eq!(w.store.mcp_project_changes(10).unwrap().len(), 1);
    // Adding one is still checked.
    let refusal = w
        .configure(
            &bot,
            json!({ "project": "app", "addAllowedDomains": [old[0]] }),
        )
        .await
        .unwrap_err()
        .0;
    assert!(refusal.contains("isn't a domain"), "{refusal}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_waiting_for_a_domain_says_only_the_person_answers_it() {
    let w = world();
    let bot = w.manager();
    let runner = w.client("Runner", true);
    let reader = w.client("Reader", false);
    let project = with_domains(&w, &[]);
    w.waiting_for_a_domain(&project.id, "run-net", "attempt-net");
    let get = || -> GetRunArgs { serde_json::from_value(json!({ "run": "run-net" })).unwrap() };
    let stop = || -> StopRunArgs { serde_json::from_value(json!({ "run": "run-net" })).unwrap() };

    let run = w.server.get_run(bot.clone(), get()).await.unwrap();

    assert_eq!(run["state"], "waitingForPerson");
    let waiting = &run["waitingFor"];
    assert_eq!(waiting["step"], "builder");
    assert_eq!(waiting["reason"]["code"], "networkNeeded");
    assert_eq!(
        waiting["reason"]["needed"][0],
        json!({ "domain": "registry.npmjs.org", "why": "to install next" })
    );
    let who = waiting["who"].as_str().unwrap();
    assert!(who.contains("Only the person"), "{who}");
    assert!(who.contains("in Keel"), "{who}");
    // Not a step to try again. Stopping it is all a client may do with it.
    let may = waiting["youCan"].to_string();
    assert!(!may.contains("retry_step"), "{may}");
    assert!(may.contains("stop_run"), "{may}");

    // One that may only read may not stop it either.
    let read = w.server.get_run(reader.clone(), get()).await.unwrap();
    let may = read["waitingFor"]["youCan"].to_string();
    assert!(!may.contains("stop_run"), "{may}");
    let refusal = w.server.stop_run(reader, stop()).await.unwrap_err().0;
    assert!(refusal.contains("only read"), "{refusal}");
    assert_eq!(w.store.attempt("attempt-net").unwrap().state, "blocked");

    let stopped = w.server.stop_run(runner, stop()).await.unwrap();

    assert_eq!(stopped["waitingFor"]["state"], "stopped");
    assert_eq!(w.store.attempt("attempt-net").unwrap().state, "stopped");
    // It stopped it, and answered nothing else: no domain allowed and none gone without.
    assert!(
        w.store
            .project(&project.id)
            .unwrap()
            .allowed_domains
            .is_empty()
    );
    let events = w.store.events("attempt-net", 0).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"networkAllowed"), "{kinds:?}");
    assert!(!kinds.contains(&"networkDeclined"), "{kinds:?}");
    // Its record says which client did.
    let asked = events.iter().find(|e| e.kind == "stopRequested").unwrap();
    assert_eq!(asked.data["by"], "Runner");
    // Nothing left to stop.
    assert!(w.server.stop_run(bot, stop()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_projects_names_the_github_accounts_in_keel() {
    let w = world();
    let reader = w.client("Reader", false);

    let listed = w.list(&reader).await;

    assert_eq!(
        listed["githubAccounts"],
        json!([{ "login": "alice", "ok": true }, { "login": "bob", "ok": false }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_projects_still_answers_without_gh() {
    let w = world();
    let reader = w.client("Reader", false);
    w.engine.set_tools(Tools::from_dirs(&[
        PathBuf::from("/nowhere"),
        PathBuf::from("/bin"),
    ]));

    let listed = w.list(&reader).await;

    assert_eq!(listed["projects"], json!([]));
    assert_eq!(listed["githubAccounts"], Json::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gh_that_never_finishes_is_given_up_on_and_leaves_the_client_its_turn() {
    let w = world_with(|config| config.gh_accounts_limit = Duration::from_millis(300));
    let bot = w.manager();
    let there = w.repo("there");
    let there = w.engine.add_project(Path::new(&there)).unwrap();
    let path = w.repo("app");
    keel_harness::testing::write_script(
        &w.dir.path().join("bin/gh"),
        "#!/bin/bash\nsleep 60 &\nwait\n",
    )
    .unwrap();

    // Well under what the fake `gh` takes, so a call that waited for it fails here.
    let soon = Duration::from_secs(20);
    let listed = tokio::time::timeout(soon, w.list(&bot)).await.unwrap();
    assert_eq!(listed["githubAccounts"], Json::Null);
    assert_eq!(listed["projects"][0]["id"], json!(there.id));

    let add = json!({ "path": path, "githubAccount": "alice" });
    let configure = json!({ "project": "there", "githubAccount": "alice", "checkCommand": "true" });
    let refusals = [
        tokio::time::timeout(soon, w.add(&bot, add)).await.unwrap(),
        tokio::time::timeout(soon, w.configure(&bot, configure))
            .await
            .unwrap(),
    ];
    for refusal in refusals {
        let refusal = refusal.unwrap_err().0;
        assert!(
            refusal.contains("couldn't ask `gh` which GitHub accounts"),
            "{refusal}"
        );
        assert!(refusal.contains("longer than"), "{refusal}");
        assert!(
            refusal.contains("nothing was added or changed"),
            "{refusal}"
        );
    }
    assert_eq!(w.store.projects().unwrap().len(), 1);
    let now = w.store.project(&there.id).unwrap();
    assert_eq!((now.github_account, now.check_command), (None, None));
    assert!(w.store.mcp_project_changes(10).unwrap().is_empty());

    // The client's turn is free again: its next change goes through. One with no account
    // doesn't ask `gh` at all.
    let configure = json!({ "project": "there", "checkCommand": "true" });
    tokio::time::timeout(soon, w.configure(&bot, configure))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_says_who_can_add_it_for_this_client() {
    let w = world();
    let runner = w.client("Runner", true);
    let path = w.repo("app");
    let answer = w.propose(&runner, &path).await;
    let who = answer["who"].as_str().unwrap();
    assert!(who.contains("This client may not add it"), "{who}");
    assert!(who.contains("Tell them"), "{who}");

    let answer = w.propose(&w.manager(), &path).await;
    let who = answer["who"].as_str().unwrap();
    assert!(who.contains("add_project"), "{who}");
    assert!(!who.contains("may not"), "{who}");
}

#[test]
fn the_instructions_say_what_a_client_can_do_with_the_permission_and_without_it() {
    assert!(!INSTRUCTIONS.contains("add a project,"), "{INSTRUCTIONS}");
    for says in [
        "add_project",
        "configure_project",
        "githubAccounts",
        "If the person lets this client add and configure projects",
        "Without that permission",
        "can't give yourself",
        "addAllowedDomains",
        "removeAllowedDomains",
        "lacks network to a domain",
    ] {
        assert!(INSTRUCTIONS.contains(says), "{says}: {INSTRUCTIONS}");
    }
}
