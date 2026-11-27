//! The server over HTTP, end to end: rmcp's own client and plain HTTP against a server on a free
//! port, with the real engine and a fake `claude`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{
    AttemptState, BoardColumn, CardLane, MASK, StepRole, StepSpec, ToolProfile, WorkflowStep,
};
use keel_engine::{
    AddOutcome, Answer, AnswerChoice, Config, Engine, MAX_PROJECT_PROPOSALS, StartRun, Workflow,
    split_assistant_notes,
};
use keel_harness::Tools;
use keel_mcp::{MAX_DRAFTS, MAX_OPEN_RUNS, MCP_PATH, McpServer, StartError};
use keel_store::rusqlite_value::Value;
use keel_store::{ProjectSettings, Store};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value as Json, json};

/// Everything a client can do. A new tool has to be added here on purpose: nothing that answers an
/// approval, merges, changes saved workflows or what a client may do, or lets anything into the
/// Brain belongs here. draft_workflow is here because what it leaves can't run until the person
/// saves it, and propose_project because what it leaves isn't a project until it's added.
/// add_project and configure_project are the exception to settings staying with the person: only
/// for a client they let, only a project's GitHub account and setup and check commands, and each
/// change on record for them. send_back is
/// here because the run comes back to the same approval, so the person still decides, and
/// retry_step because it only retries what failed or was stopped, never what waits on the person's
/// judgment. Those two are only for a client the person lets give feedback, on runs it started, and
/// only a few times per run. close_run is here because it delivers nothing and decides nothing for
/// the person, and the card keeps its text for them. It's for any client that may start runs, on
/// runs it started. start_task is here because its one agent only reads, and has nothing to
/// deliver. propose_change is here because it only starts the run that agrees what changes where,
/// which reads and waits for the person's approval, and the parts open only once they give it.
/// append_to_card is here because it only adds to what a waiting card of the client's own says,
/// marked as the client's, and changes nothing that's there: it's feedback, so it needs the person's
/// leave too. move_card_to_todo is here because putting a card of its own back in Todo is starting a
/// run, as start_run is, and counts toward the runs the client may have open.
const TOOLS: [&str; 21] = [
    "add_project",
    "append_to_card",
    "close_run",
    "configure_project",
    "draft_workflow",
    "get_brain",
    "get_run",
    "get_step",
    "list_issues",
    "list_projects",
    "list_runs",
    "list_workflows",
    "move_card_to_todo",
    "propose_change",
    "propose_project",
    "retry_step",
    "send_back",
    "start_run",
    "start_task",
    "stop_run",
    "wait_for_run",
];

type Client = RunningService<RoleClient, ()>;

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    engine: Engine,
    server: McpServer,
    addr: SocketAddr,
    project_id: String,
    workflow_id: String,
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

fn fake_claude() -> String {
    r##"#!/bin/bash
case "$1" in
--version) echo '2.1.318 (Claude Code)'; exit 0;;
auth) echo '{"loggedIn":true,"subscriptionType":"max","email":"a@example.com"}'; exit 0;;
esac
cat > /dev/null
tools=""; prev=""; schema=""
for a in "$@"; do
  [ "$prev" = "--tools" ] && tools="$a"
  [ "$prev" = "--json-schema" ] && schema="$a"
  prev="$a"
done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
[ -n "$schema" ] && list="$list\",\"StructuredOutput"
echo "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}"
while [ -f "$(dirname "$0")/hold" ]; do sleep 0.02; done
if [ -n "$schema" ]; then
  case "$schema" in
  *verdict*) out='{"verdict":"changes","findings":["Line 3 divides by zero"],"summary":"One problem","unchecked":["How it looks on screen"]}';;
  *decision*) out='{"decision":"approve","risk":"low","reasons":"It does what was asked","instructions":"Only a note"}';;
  *unverified*) out='{"criteria":[{"id":"C1","result":"met","evidence":""}],"summary":"It works"}';;
  *parts*) out='{"size":"normal","risks":[],"summary":"Log in through lib","criteria":[{"id":"C1","when":"someone logs in","then":"app lets them in"}],"changes":[],"limits":[],"decided":[],"questions":[],"parts":[{"project":"lib","text":"Sends the token"},{"project":"app","text":"Reads the token"}],"together":["Log in and land in app"]}';;
  *) out='{"summary":"It gets fixed","criteria":[{"id":"C1","when":"it runs","then":"it works"}],"changes":[],"limits":[],"decided":[],"questions":[{"id":"Q1","text":"What then?","options":[{"label":"Yes","recommended":true},{"label":"No","recommended":false}]}]}';;
  esac
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"num_turns\":1,\"structured_output\":$out}"
  exit 0
fi
if [ "$tools" = "Read,Grep,Glob" ]; then
  echo '{"type":"result","subtype":"success","is_error":false,"result":"The login checks the password twice.","num_turns":1}'
  exit 0
fi
echo work >> fixed.txt
git add -A && git commit -qm "fix"
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Fixed it.\",\"num_turns\":1,\"total_cost_usd\":0.01,\"usage\":{\"input_tokens\":10,\"output_tokens\":2},\"modelUsage\":{\"claude-fake\":{}}}"
"##
    .to_owned()
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let claude = bin.join("claude");
    keel_harness::testing::write_script(&claude, &fake_claude()).unwrap();

    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
    let tools = Tools::from_dirs(&[bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let mut config = Config::new(dir.path().join("data"));
    config.tick = Duration::from_millis(20);
    config.keep_awake = false;
    let engine = Engine::new(store.clone(), config, tools, |_| {});
    let project = engine.add_project(&repo).unwrap();
    let workflow = engine
        .save_workflow(
            &project.id,
            &Workflow {
                id: String::new(),
                name: "Build and ask".into(),
                builtin: false,
                steps: vec![
                    WorkflowStep::Agent(StepSpec {
                        id: "builder".into(),
                        model: "haiku".into(),
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
                    }),
                    WorkflowStep::Approval { id: "human".into() },
                ],
            },
        )
        .unwrap();
    let server = McpServer::new(engine.clone(), store.clone());
    let addr = server.start(0).await.unwrap();
    World {
        _dir: dir,
        store,
        engine,
        server,
        addr,
        project_id: project.id,
        workflow_id: workflow.id,
    }
}

impl World {
    fn url(&self) -> String {
        format!("http://{}{MCP_PATH}", self.addr)
    }

    fn token(&self, can_run: bool) -> String {
        self.server.create_client("Claude Code", can_run).unwrap().1
    }

    async fn client(&self, token: &str) -> Client {
        let config = StreamableHttpClientTransportConfig::with_uri(self.url()).auth_header(token);
        ().serve(StreamableHttpClientTransport::from_config(config))
            .await
            .unwrap()
    }

    fn post(&self, token: Option<&str>) -> reqwest::RequestBuilder {
        let request = reqwest::Client::new()
            .post(self.url())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(initialize_body());
        match token {
            Some(token) => request.header(AUTHORIZATION, format!("Bearer {token}")),
            None => request,
        }
    }

    fn start_args(&self, request: &str) -> Json {
        json!({ "project": self.project_id, "workflow": self.workflow_id, "request": request })
    }

    /// A client the person let give feedback on its own runs.
    fn steering_token(&self, name: &str) -> String {
        let (client, token) = self.server.create_client(name, true).unwrap();
        self.store
            .set_mcp_client_can_steer(&client.id, true)
            .unwrap();
        token
    }

    /// Another project, whose cards start with a builder and the person's approval, as app's do.
    fn other_project(&self, name: &str) -> String {
        let repo = self._dir.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        let project = self.engine.add_project(&repo).unwrap();
        let workflow = Workflow {
            id: String::new(),
            name: "Build and ask".into(),
            builtin: false,
            steps: vec![
                WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::default())),
                WorkflowStep::Approval { id: "human".into() },
            ],
        };
        let workflow = self.engine.save_workflow(&project.id, &workflow).unwrap();
        self.use_by_default(&project.id, &workflow.id);
        project.id
    }

    fn use_by_default(&self, project: &str, workflow: &str) {
        self.engine
            .update_project(
                project,
                &keel_store::ProjectSettings {
                    default_workflow: Some(workflow.to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn cards_everywhere(&self) -> usize {
        self.store
            .projects()
            .unwrap()
            .iter()
            .map(|project| self.store.cards(&project.id).unwrap().len())
            .sum()
    }

    fn saved(&self, name: &str, steps: Vec<WorkflowStep>) -> String {
        let workflow = Workflow {
            id: String::new(),
            name: name.into(),
            builtin: false,
            steps,
        };
        self.engine
            .save_workflow(&self.project_id, &workflow)
            .unwrap()
            .id
    }

    fn args_for(&self, workflow: &str, request: &str) -> Json {
        json!({ "project": self.project_id, "workflow": workflow, "request": request })
    }

    /// A run the person started in the app, with the same workflow as `start_args`.
    fn person_run(&self, request: &str) -> String {
        let workflow = self
            .engine
            .workflow(&self.project_id, &self.workflow_id, "en")
            .unwrap()
            .unwrap();
        self.engine
            .start_run_on_board(StartRun {
                project_id: self.project_id.clone(),
                title: None,
                request: request.into(),
                workflow: workflow.steps.clone(),
                saved_workflow: Some(workflow.reference()),
                language: "en".into(),
                source_issue: None,
                started_by: None,
            })
            .unwrap()
    }
}

fn agent(id: &str, tools: ToolProfile, role: StepRole) -> StepSpec {
    StepSpec {
        id: id.into(),
        model: "haiku".into(),
        effort: None,
        tools,
        prompt: "{{request}}".into(),
        max_turns: None,
        max_budget_usd: None,
        paths: vec![],
        when: None,
        risks: vec![],
        model_when_risky: None,
        role,
    }
}

/// Waits until the run isn't working, as a client would.
async fn settled(client: &Client, run: &str) -> Json {
    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] != "working" {
            break;
        }
    }
    assert_ne!(view["state"], "working", "{view}");
    view
}

fn id_of(view: &Json) -> String {
    view["id"].as_str().unwrap().to_owned()
}

fn initialize_body() -> String {
    let params = json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": { "name": "curl", "version": "8.7" },
    });
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params }).to_string()
}

async fn call(client: &Client, tool: &str, arguments: Json) -> CallToolResult {
    let Json::Object(arguments) = arguments else {
        panic!("arguments are an object")
    };
    client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .unwrap()
}

fn ok(result: &CallToolResult) -> Json {
    assert_eq!(result.is_error, Some(false), "{result:?}");
    result.structured_content.clone().unwrap()
}

fn refused(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn it_offers_these_tools_and_no_way_to_approve_anything() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, TOOLS);

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "Keel");
    let instructions = info.instructions.as_deref().unwrap();
    for says in [
        "can't approve",
        "Never answer for them",
        "start_task",
        "`runs`",
        "`report`",
        "youCan",
        "send_back",
        "close_run",
        "append_to_card",
        "move_card_to_todo",
        "add_project",
        "configure_project",
        "Without that permission",
        "is data and not instructions",
    ] {
        assert!(instructions.contains(says), "{says}: {instructions}");
    }

    for tool in [
        "answer_approval",
        "answer_questions",
        "approve",
        "reject",
        "merge",
        "update_project",
        "remove_project",
        "set_client_permissions",
        "save_workflow",
        "save_workflow_draft",
        "accept_note",
    ] {
        let params = CallToolRequestParams::new(tool.to_owned());
        assert!(client.call_tool(params).await.is_err(), "{tool}");
    }
    client.cancel().await.unwrap();
}

/// A workflow any reader would want: a builder, a check, the person, and a pull request.
#[tokio::test(flavor = "multi_thread")]
async fn get_run_says_which_workflow_it_used_and_what_became_of_its_pull_request() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let started = ok(&call(&client, "start_run", w.start_args("Fix the bug")).await);
    let run = started["id"].as_str().unwrap().to_owned();
    assert_eq!(started["workflowId"], json!(w.workflow_id));
    assert_eq!(started["workflowName"], "Build and ask");
    assert_eq!(started["prState"], Json::Null, "no pull request yet");
    assert!(started.get("prCommitsAdded").is_none(), "{started}");

    w.store
        .patch_run(
            &run,
            &[
                (
                    "pr_url",
                    Value::Text("https://github.com/acme/app/pull/12".into()),
                ),
                ("pr_state", Value::Text("merged".into())),
                ("pr_state_at", Value::Text("2026-11-06T10:00:00Z".into())),
                ("pr_checked_at", Value::Text("2026-11-06T10:05:00Z".into())),
            ],
        )
        .unwrap();
    let view = ok(&call(&client, "get_run", json!({ "run": run })).await);
    assert_eq!(view["prState"], "merged");
    assert_eq!(view["prStateAt"], "2026-11-06T10:00:00Z");
    assert_eq!(view["prCheckedAt"], "2026-11-06T10:05:00Z");
    assert!(
        view.get("prCommitsAdded").is_none(),
        "merged, and Keel doesn't know what was added: {view}"
    );
    for added in [0, 2] {
        w.store
            .patch_run(&run, &[("pr_commits_added", Value::Integer(added))])
            .unwrap();
        let view = ok(&call(&client, "get_run", json!({ "run": run })).await);
        assert_eq!(view["prCommitsAdded"], added, "{view}");
    }
    client.cancel().await.unwrap();
}

/// A `gh` whose pull request has a check that failed and one that passed.
const GH_WITH_RED_CI: &str = r#"#!/bin/bash
case "$1 $2" in
"auth token") echo "tok"; exit 0;;
"api graphql") echo '{"data":{"repository":{"pullRequest":{"state":"OPEN","isDraft":true,"updatedAt":"2026-11-06T09:00:00Z","commits":{"nodes":[{"commit":{"verdict":{"state":"FAILURE"},"checks":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"},{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}}'; exit 0;;
esac
exit 1
"#;

#[tokio::test(flavor = "multi_thread")]
async fn get_run_and_wait_for_run_say_what_the_pull_requests_ci_says() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let bin = w._dir.path().join("bin");
    keel_harness::testing::write_script(&bin.join("gh"), GH_WITH_RED_CI).unwrap();
    w.engine.set_tools(Tools::from_dirs(&[
        bin,
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]));
    w.engine
        .update_project(
            &w.project_id,
            &ProjectSettings {
                github_account: Some("alice".into()),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    let client = w.client(&w.token(false)).await;
    let run = w.person_run("Fix the bug");
    let waited = settled(&client, &run).await;
    assert_eq!(waited["prCi"], Json::Null, "no pull request yet");
    assert_eq!(waited["report"]["prCi"], Json::Null);

    w.store
        .patch_run(
            &run,
            &[
                (
                    "pr_url",
                    Value::Text("https://github.com/acme/app/pull/12".into()),
                ),
                ("pr_state", Value::Text("draft".into())),
            ],
        )
        .unwrap();
    // Asking is what makes Keel look, as a person opening the run does.
    ok(&call(&client, "get_run", json!({ "run": run })).await);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.store.run(&run).unwrap().pr_ci.is_none() {
        assert!(std::time::Instant::now() < deadline, "Keel never looked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let got = ok(&call(&client, "get_run", json!({ "run": run })).await);
    let waited = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 1 })).await);
    for view in [got, waited] {
        assert_eq!(view["prCi"], "failed", "{view}");
        assert_eq!(view["prCiFailed"], json!(["lint"]));
        assert_eq!(view["report"]["prCi"], "failed");
        assert_eq!(view["report"]["prCiFailed"], json!(["lint"]));
    }
    client.cancel().await.unwrap();
}

fn draft_args(project: &str) -> Json {
    json!({
        "project": project,
        "name": "Proposed by a client",
        "steps": [
            { "type": "agent", "id": "builder", "model": "sonnet", "tools": "build", "prompt": "{{request}}", "effort": "high" },
            { "type": "check", "id": "tests", "run": "make test" },
            { "type": "parallel", "id": "reviews", "steps": [
                { "id": "security", "model": "haiku", "prompt": "Review {{diff}}", "role": { "kind": "reviewer" } },
            ] },
            { "type": "agent", "id": "approver", "model": "opus", "prompt": "Decide", "role": { "kind": "approver", "reworkTo": "builder", "maxRounds": 2 } },
            { "type": "approval", "id": "human" },
            { "type": "deliver", "id": "pr" },
        ],
    })
}

#[tokio::test]
async fn it_tells_a_model_how_to_write_a_draft() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "draft_workflow")
        .unwrap();
    let schema = serde_json::to_string(&tool.input_schema).unwrap();
    for word in [
        "agent",
        "check",
        "parallel",
        "approval",
        "deliver",
        "prompt",
        "reworkTo",
        "maxRounds",
        "specifier",
        "verifier",
        "when",
        "notSmall",
        "somethingToFix",
        "onlyIfChanged",
        "watch",
    ] {
        assert!(schema.contains(word), "{word}: {schema}");
    }
    assert!(!schema.contains("setup"), "{schema}");
    assert!(!schema.contains("mapper"), "{schema}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_can_ask_for_a_step_to_depend_on_the_change_and_it_is_kept_and_shown() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let args = json!({
        "project": "app",
        "name": "Fits",
        "steps": [
            { "type": "agent", "id": "builder", "model": "sonnet", "tools": "build", "prompt": "{{request}}" },
            { "type": "parallel", "id": "reviews", "steps": [
                { "id": "security", "model": "haiku", "prompt": "Review", "role": { "kind": "reviewer" }, "when": "notSmall" },
            ] },
            { "type": "agent", "id": "approver", "model": "opus", "prompt": "Decide", "role": { "kind": "approver", "reworkTo": "builder", "maxRounds": 2 }, "when": "notSmall" },
            { "type": "approval", "id": "human" },
        ],
    });
    let draft = ok(&call(&client, "draft_workflow", args).await);
    assert_eq!(draft["steps"][1]["steps"][0]["when"], "notSmall");
    assert_eq!(draft["steps"][2]["when"], "notSmall");
    assert_eq!(draft["steps"][0]["when"], Json::Null);
    let kept = w.engine.workflow_drafts().unwrap().remove(0);
    let WorkflowStep::Agent(approver) = &kept.steps[2] else {
        panic!()
    };
    assert_eq!(approver.when, Some(keel_core::StepCondition::NotSmall));

    let refusal = call(
        &client,
        "draft_workflow",
        json!({ "project": "app", "name": "Bad", "steps": [{ "type": "agent", "id": "a", "model": "opus", "prompt": "p", "when": "whenever" }] }),
    )
    .await;
    assert!(
        refused(&refusal).contains("whenever"),
        "{}",
        refused(&refusal)
    );
    // Only an agent can have one. On anything else it's refused, and not left to look like it worked.
    let refusal = call(
        &client,
        "draft_workflow",
        json!({ "project": "app", "name": "Bad", "steps": [
            { "type": "agent", "id": "b", "model": "opus", "prompt": "p", "tools": "build" },
            { "type": "approval", "id": "human", "when": "notSmall" },
        ] }),
    )
    .await;
    assert!(refused(&refusal).contains("when"), "{}", refused(&refusal));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_can_ask_for_a_step_that_fixes_what_was_found_and_the_check_again() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let steps = |fix_first: bool| {
        let review = json!({ "type": "agent", "id": "security", "model": "haiku", "prompt": "Review", "role": { "kind": "reviewer" } });
        let ship = json!({ "type": "agent", "id": "ship", "model": "sonnet", "tools": "build", "prompt": "Fix", "when": "somethingToFix" });
        let (second, third) = if fix_first {
            (ship, review)
        } else {
            (review, ship)
        };
        json!([
            { "type": "agent", "id": "builder", "model": "sonnet", "tools": "build", "prompt": "{{request}}" },
            { "type": "check", "id": "check", "run": "make test" },
            second,
            third,
            { "type": "check", "id": "check-ship", "run": "make test", "onlyIfChanged": true },
            { "type": "approval", "id": "human" },
        ])
    };
    let args = |fix_first| json!({ "project": "app", "name": "Ships", "steps": steps(fix_first) });
    let draft = ok(&call(&client, "draft_workflow", args(false)).await);
    assert_eq!(draft["steps"][3]["when"], "somethingToFix");
    assert_eq!(draft["steps"][4]["onlyIfChanged"], true);
    assert_eq!(draft["steps"][1].get("onlyIfChanged"), None);
    let kept = w.engine.workflow_drafts().unwrap().remove(0);
    let WorkflowStep::Agent(ship) = &kept.steps[3] else {
        panic!()
    };
    assert_eq!(ship.when, Some(keel_core::StepCondition::SomethingToFix));
    assert!(matches!(
        &kept.steps[4],
        WorkflowStep::Check {
            only_if_changed: true,
            ..
        }
    ));
    let refusal = call(&client, "draft_workflow", args(true)).await;
    assert!(
        refused(&refusal).contains("fixBeforeReview"),
        "{}",
        refused(&refusal)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_can_follow_its_pull_request_if_a_person_approves_every_round_of_it() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let args = |approval_first: bool| {
        let builder = json!({ "type": "agent", "id": "builder", "model": "sonnet", "tools": "build", "prompt": "{{request}}" });
        let human = json!({ "type": "approval", "id": "human" });
        let (first, second) = if approval_first {
            (human, builder)
        } else {
            (builder, human)
        };
        json!({ "project": "app", "name": "Follows", "steps": [
            first,
            second,
            { "type": "deliver", "id": "pr" },
            { "type": "watch", "id": "watch", "maxRounds": 2 },
        ] })
    };
    let draft = ok(&call(&client, "draft_workflow", args(false)).await);
    assert_eq!(
        draft["steps"][3],
        json!({ "type": "watch", "id": "watch", "maxRounds": 2 })
    );
    let refusal = call(&client, "draft_workflow", args(true)).await;
    assert!(
        refused(&refusal).contains("watchNeedsBuilder"),
        "{}",
        refused(&refusal)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_waits_for_the_person_and_nothing_can_run_it_until_they_save_it() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;

    let draft = ok(&call(&client, "draft_workflow", draft_args("app")).await);
    let draft_id = draft["draftId"].as_str().unwrap().to_owned();
    assert_eq!(draft["state"], "waitingForPerson");
    assert_eq!(draft["steps"].as_array().unwrap().len(), 6);
    let record = w.store.mcp_calls(1).unwrap().remove(0);
    assert_eq!(record.tool, "draft_workflow");
    assert!(record.ok);
    assert!(record.args.contains("make test"), "{}", record.args);

    // It's on record who proposed it, and the agent's fields came through as sent.
    let waiting = w.engine.workflow_drafts().unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].proposed_by, "Claude Code");
    assert_eq!(waiting[0].name, "Proposed by a client");
    assert_eq!(waiting[0].project_id, w.project_id);
    let WorkflowStep::Agent(builder) = &waiting[0].steps[0] else {
        panic!("{:?}", waiting[0].steps[0]);
    };
    assert_eq!(builder.tools, ToolProfile::Build);
    assert_eq!(builder.effort.as_deref(), Some("high"));
    assert_eq!(builder.prompt, "{{request}}");

    // Nothing that runs can see it: not a client, not the engine's list that everything goes through.
    let refusal = refused(
        &call(
            &client,
            "start_run",
            json!({ "project": "app", "workflow": draft_id, "request": "Fix it" }),
        )
        .await,
    );
    assert!(refusal.contains("no workflow"), "{refusal}");
    assert!(w.store.runs(10).unwrap().is_empty());
    assert!(
        !w.engine
            .workflows(&w.project_id, "en")
            .unwrap()
            .iter()
            .any(|workflow| workflow.id == draft_id || workflow.name == "Proposed by a client")
    );
    let listed = ok(&call(&client, "list_workflows", json!({ "project": "app" })).await);
    assert_eq!(listed["drafts"][0]["id"], json!(draft_id));
    assert_eq!(listed["drafts"][0]["proposedBy"], "Claude Code");
    assert!(
        !listed["workflows"]
            .to_string()
            .contains("Proposed by a client"),
        "{listed}"
    );

    // The person saves what they see, which isn't necessarily what was proposed.
    let mut edited = Workflow {
        id: String::new(),
        name: "Checked by me".into(),
        builtin: false,
        steps: waiting[0].steps.clone(),
    };
    edited.steps.remove(1);
    edited.steps.pop();
    let saved = w
        .engine
        .save_workflow_draft(&w.project_id, &draft_id, &edited)
        .unwrap();
    assert!(w.engine.workflow_drafts().unwrap().is_empty());
    let listed = ok(&call(&client, "list_workflows", json!({ "project": "app" })).await);
    assert_eq!(listed["drafts"], json!([]));
    let names = listed["workflows"].to_string();
    assert!(names.contains("Checked by me"), "{names}");
    assert!(!names.contains("make test"), "{names}");
    assert!(!names.contains("\"pr\""), "{names}");

    // Now it's a workflow like any other, for the client too.
    ok(&call(
        &client,
        "start_run",
        json!({ "project": "app", "workflow": saved.id, "request": "Fix it" }),
    )
    .await);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn saving_a_draft_never_replaces_a_workflow_the_person_has() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let draft = ok(&call(&client, "draft_workflow", draft_args("app")).await);
    let draft_id = draft["draftId"].as_str().unwrap();
    let before = w
        .engine
        .workflow(&w.project_id, &w.workflow_id, "en")
        .unwrap()
        .unwrap();

    let steps = w.engine.workflow_drafts().unwrap().remove(0).steps;
    let saved = w
        .engine
        .save_workflow_draft(
            &w.project_id,
            draft_id,
            &Workflow {
                id: w.workflow_id.clone(),
                name: "Same id".into(),
                builtin: false,
                steps,
            },
        )
        .unwrap();

    assert_ne!(saved.id, w.workflow_id);
    assert_eq!(
        w.engine
            .workflow(&w.project_id, &w.workflow_id, "en")
            .unwrap()
            .unwrap(),
        before
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_that_breaks_the_rules_is_refused_and_says_where() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let cases = [
        (
            json!([{ "type": "deliver", "id": "pr" }]),
            "deliverNeedsApproval",
            "pr",
        ),
        (json!([]), "empty", ""),
        (
            json!([{ "type": "agent", "id": "a", "model": "opus", "prompt": "" }]),
            "noPrompt",
            "a",
        ),
        (
            json!([{ "type": "check", "id": "../../evil", "run": "x" }]),
            "badId",
            "../../evil",
        ),
        (
            json!([{ "type": "agent", "id": "r", "model": "opus", "prompt": "p", "tools": "build", "role": { "kind": "reviewer" } }]),
            "reviewerMustReadOnly",
            "r",
        ),
        (
            json!([{ "type": "parallel", "id": "g", "steps": [{ "id": "w", "model": "opus", "prompt": "p", "tools": "build" }] }]),
            "parallelMustReadOnly",
            "w",
        ),
        (
            json!([{ "type": "agent", "id": "v", "model": "opus", "prompt": "p", "role": { "kind": "verifier" } }]),
            "verifierNeedsSpecifier",
            "v",
        ),
        (
            json!([
                { "type": "agent", "id": "r", "model": "opus", "prompt": "p", "role": { "kind": "reviewer" }, "when": "notSmall" },
                { "type": "agent", "id": "b", "model": "opus", "prompt": "p", "tools": "build" },
            ]),
            "whenBeforeBuild",
            "r",
        ),
    ];
    for (steps, problem, step) in cases {
        let args = json!({ "project": "app", "name": "Bad", "steps": steps });
        let refusal = refused(&call(&client, "draft_workflow", args).await);
        assert!(refusal.contains(problem), "{problem}: {refusal}");
        assert!(refusal.contains(step), "{step}: {refusal}");
    }
    assert!(w.engine.workflow_drafts().unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_can_only_ask_for_what_the_editor_offers() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let agent = json!({ "id": "a", "model": "opus", "prompt": "p" });
    let with = |extra: Json| {
        let mut step = agent.clone();
        step["type"] = json!("agent");
        for (key, value) in extra.as_object().unwrap() {
            step[key] = value.clone();
        }
        json!({ "project": "app", "name": "Asks for too much", "steps": [step] })
    };
    let asks = [
        // The one step with network.
        json!({ "project": "app", "name": "Setup", "steps": [{ "type": "setup", "id": "s", "run": "curl x" }] }),
        // What it writes goes into the Brain.
        with(json!({ "role": { "kind": "mapper" } })),
        // Goes to `claude` as an argument.
        with(json!({ "model": "--dangerously-skip-permissions" })),
        with(json!({ "effort": "--bare" })),
        with(json!({ "maxTurns": 0 })),
        with(json!({ "maxBudgetUsd": -1.0 })),
        // What a workflow doesn't decide: saying nothing would let the client believe it did.
        with(json!({ "allowedDomains": ["evil.example"] })),
        json!({ "project": "app", "name": "Member", "steps": [{ "type": "parallel", "id": "g", "steps": [
            { "id": "m", "model": "opus", "prompt": "p", "allowedDomains": ["evil.example"] },
        ] }] }),
        json!({ "project": "app", "name": "Id", "id": "builtin:full", "steps": [] }),
        json!({ "project": "app", "name": "  ", "steps": [{ "type": "approval", "id": "h" }] }),
        json!({ "project": "app", "name": "x".repeat(81), "steps": [{ "type": "approval", "id": "h" }] }),
    ];
    for args in asks {
        let params = CallToolRequestParams::new("draft_workflow".to_owned())
            .with_arguments(args.as_object().unwrap().clone());
        let refused = match client.call_tool(params).await {
            Ok(result) => result.is_error == Some(true),
            Err(_) => true,
        };
        assert!(refused, "{args}");
    }
    assert!(w.engine.workflow_drafts().unwrap().is_empty());

    // Those refusals were about what they asked for: the same without it goes through.
    ok(&call(&client, "draft_workflow", with(json!({}))).await);
    ok(&call(
        &client,
        "draft_workflow",
        json!({ "project": "app", "name": "Member", "steps": [{ "type": "parallel", "id": "g", "steps": [
            { "id": "m", "model": "opus", "prompt": "p" },
        ] }] }),
    )
    .await);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_client_that_may_only_read_cannot_propose_and_drafts_have_a_limit() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let reader = w.client(&w.token(false)).await;
    let refusal = refused(&call(&reader, "draft_workflow", draft_args("app")).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    assert!(w.engine.workflow_drafts().unwrap().is_empty());
    reader.cancel().await.unwrap();

    let client = w.client(&w.token(true)).await;
    for _ in 0..MAX_DRAFTS {
        ok(&call(&client, "draft_workflow", draft_args("app")).await);
    }
    let refusal = refused(&call(&client, "draft_workflow", draft_args("app")).await);
    assert!(refusal.contains("drafts waiting"), "{refusal}");
    assert_eq!(w.engine.workflow_drafts().unwrap().len(), MAX_DRAFTS);

    // Once the person deals with one, there's room again.
    let oldest = w.engine.workflow_drafts().unwrap().pop().unwrap();
    w.engine
        .discard_workflow_draft(&w.project_id, &oldest.id)
        .unwrap();
    ok(&call(&client, "draft_workflow", draft_args("app")).await);
    client.cancel().await.unwrap();
}

impl World {
    /// A repo on this Mac that isn't in Keel, as git names it.
    fn repo_outside_keel(&self, name: &str) -> String {
        let repo = self._dir.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        std::fs::canonicalize(&repo)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

#[tokio::test]
async fn a_proposed_project_waits_for_the_person_and_no_run_can_reach_it_until_they_add_it() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let path = w.repo_outside_keel("other");

    let answer = ok(&call(&client, "propose_project", json!({ "path": path })).await);
    assert_eq!(answer["state"], "waitingForPerson");
    assert_eq!(answer["path"], path);
    assert_eq!(answer["proposedBy"], "Claude Code");
    assert!(answer["who"].as_str().unwrap().contains("Tell them"));

    let listed = ok(&call(&client, "list_projects", json!({})).await);
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["proposals"][0]["path"], path);

    let args = json!({ "project": "other", "workflow": w.workflow_id, "request": "x" });
    let refusal = refused(&call(&client, "start_run", args).await);
    assert!(refusal.contains("no project"), "{refusal}");
    assert!(w.store.runs(10).unwrap().is_empty());

    // Asking again doesn't ask the person again.
    let again = ok(&call(&client, "propose_project", json!({ "path": path })).await);
    assert_eq!(again["proposalId"], answer["proposalId"]);

    let id = answer["proposalId"].as_str().unwrap();
    let added = w.engine.accept_project_proposal(id, "en").unwrap();
    assert_eq!(added.outcome, AddOutcome::New);
    let listed = ok(&call(&client, "list_projects", json!({})).await);
    assert_eq!(listed["projects"].as_array().unwrap().len(), 2, "{listed}");
    assert!(listed["proposals"].as_array().unwrap().is_empty());

    let answer = ok(&call(&client, "propose_project", json!({ "path": path })).await);
    assert_eq!(answer["state"], "alreadyInKeel");
    assert_eq!(answer["project"]["id"], added.project.id);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_proposal_can_only_name_the_folder() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "propose_project")
        .unwrap();
    let schema = serde_json::to_value(&tool.input_schema).unwrap();
    let properties: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(properties, ["path"]);

    let path = w.repo_outside_keel("other");
    for extra in ["githubAccount", "checkCommand", "setupCommand", "name"] {
        let args = json!({ "path": path, extra: "whatever" });
        let refusal = refused(&call(&client, "propose_project", args).await);
        assert!(refusal.contains(extra), "{refusal}");
    }
    assert!(w.engine.project_proposals().unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_proposal_is_refused_when_it_should_be_and_the_call_is_on_record() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let reader = w.client(&w.token(false)).await;
    let path = w.repo_outside_keel("other");
    let refusal = refused(&call(&reader, "propose_project", json!({ "path": path })).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    reader.cancel().await.unwrap();

    let client = w.client(&w.token(true)).await;
    for relative in ["other", "./other", "~/other", "", "../other"] {
        let refusal = refused(&call(&client, "propose_project", json!({ "path": relative })).await);
        assert!(refusal.contains("full path"), "{relative}: {refusal}");
    }
    let long = format!("/{}", "a".repeat(2000));
    let refusal = refused(&call(&client, "propose_project", json!({ "path": long })).await);
    assert!(refusal.contains("full path"), "{refusal}");

    let plain = w._dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let refusal = refused(&call(&client, "propose_project", json!({ "path": plain })).await);
    assert!(refusal.contains("not a git repository"), "{refusal}");
    assert!(w.engine.project_proposals().unwrap().is_empty());

    for i in 0..MAX_PROJECT_PROPOSALS {
        let path = w.repo_outside_keel(&format!("repo-{i}"));
        ok(&call(&client, "propose_project", json!({ "path": path })).await);
    }
    let refusal = refused(&call(&client, "propose_project", json!({ "path": path })).await);
    assert!(refusal.contains("as many as may wait"), "{refusal}");
    client.cancel().await.unwrap();

    let calls = w.store.mcp_calls(100).unwrap();
    let on_record = calls.iter().filter(|c| c.tool == "propose_project");
    assert!(
        on_record
            .clone()
            .any(|c| c.args.contains(&path) && c.error.is_some())
    );
    assert!(on_record.clone().any(|c| c.error.is_none()));
}

/// The permission is checked on every request, so the person's switch works on a session that's
/// already open. `src/project_tests.rs` has what the two tools do.
#[tokio::test]
async fn adding_and_configuring_projects_works_only_while_the_person_lets_the_client() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let (bot, token) = w.server.create_client("Bot", true).unwrap();
    let client = w.client(&token).await;
    let change = json!({ "project": w.project_id, "checkCommand": "true" });
    let refusal = refused(&call(&client, "configure_project", change.clone()).await);
    assert!(
        refusal.contains("may not add or configure projects"),
        "{refusal}"
    );

    w.store
        .set_mcp_client_can_manage_projects(&bot.id, true)
        .unwrap();
    let answer = ok(&call(&client, "configure_project", change.clone()).await);
    assert_eq!(answer["project"]["checkCommand"], "true");
    // Nowhere to give itself anything.
    let params = CallToolRequestParams::new("configure_project").with_arguments(
        json!({ "project": w.project_id, "canManageProjects": true })
            .as_object()
            .unwrap()
            .clone(),
    );
    let took_it = client
        .call_tool(params)
        .await
        .is_ok_and(|result| result.is_error != Some(true));
    assert!(!took_it);

    w.store
        .set_mcp_client_can_manage_projects(&bot.id, false)
        .unwrap();
    let args = json!({ "path": w.repo_outside_keel("other"), "githubAccount": "alice" });
    let refusal = refused(&call(&client, "add_project", args).await);
    assert!(
        refusal.contains("may not add or configure projects"),
        "{refusal}"
    );
    assert_eq!(w.store.projects().unwrap().len(), 1);
    assert_eq!(w.store.mcp_project_changes(10).unwrap().len(), 1);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn it_listens_on_this_mac_only() {
    keel_harness::needs!(sockets);
    let w = world().await;
    assert!(w.addr.ip().is_loopback());
    let status = w.server.status();
    assert!(status.running);
    assert_eq!(status.port, Some(w.addr.port()));
}

#[tokio::test]
async fn a_request_needs_a_token_and_a_revoked_one_stops_at_once() {
    keel_harness::needs!(sockets);
    let w = world().await;
    assert_eq!(w.post(None).send().await.unwrap().status(), 401);
    assert_eq!(
        w.post(Some("keel_wrong")).send().await.unwrap().status(),
        401
    );

    let (client_row, token) = w.server.create_client("Claude Code", true).unwrap();
    let client = w.client(&token).await;
    ok(&call(&client, "list_projects", json!({})).await);

    w.store.revoke_mcp_client(&client_row.id).unwrap();
    assert_eq!(w.post(Some(&token)).send().await.unwrap().status(), 401);
    let params = CallToolRequestParams::new("list_projects");
    assert!(client.call_tool(params).await.is_err());
}

#[tokio::test]
async fn a_web_page_cannot_reach_it_even_with_a_token() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let token = w.token(true);
    let from_a_page = w.post(Some(&token)).header(ORIGIN, "https://evil.example");
    assert_eq!(from_a_page.send().await.unwrap().status(), 403);
    let rebound = w.post(Some(&token)).header(HOST, "evil.example");
    assert_eq!(rebound.send().await.unwrap().status(), 403);
}

#[tokio::test]
async fn a_removed_project_is_not_there_for_a_client() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    w.engine.remove_project(&w.project_id).unwrap();

    let projects = ok(&call(&client, "list_projects", json!({})).await);
    assert_eq!(projects["projects"], json!([]));
    for wanted in [w.project_id.as_str(), "app"] {
        let refused = call(
            &client,
            "start_run",
            json!({ "project": wanted, "workflow": w.workflow_id, "request": "x" }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{wanted}");
    }
}

#[tokio::test]
async fn a_client_that_may_only_read_can_read_and_nothing_else() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(false)).await;

    let projects = ok(&call(&client, "list_projects", json!({})).await);
    assert_eq!(projects["projects"][0]["id"], json!(w.project_id));
    let workflows = ok(&call(
        &client,
        "list_workflows",
        json!({ "project": "app", "language": "es" }),
    )
    .await);
    let names: Vec<&str> = workflows["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workflow| workflow["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Build and ask"), "{names:?}");
    assert!(
        !workflows.to_string().contains("{{request}}"),
        "prompts stay in the app"
    );

    let refusal = refused(&call(&client, "start_run", w.start_args("Fix it")).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    assert!(w.store.runs(10).unwrap().is_empty());
    let refusal = refused(&call(&client, "stop_run", json!({ "run": "nope" })).await);
    assert!(refusal.contains("may only read"), "{refusal}");

    // What it may do is read on every request, so the person's change applies at once.
    let row = w.store.mcp_clients().unwrap().remove(0);
    w.store.set_mcp_client_can_run(&row.id, true).unwrap();
    ok(&call(&client, "start_run", w.start_args("Fix it")).await);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_run_it_starts_shows_who_started_it_and_waits_for_the_person() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;

    let started = ok(&call(&client, "start_run", w.start_args("Fix the bug")).await);
    let run = started["id"].as_str().unwrap().to_owned();
    assert_eq!(started["startedBy"], "Claude Code");

    // The app sees it through the same store.
    let row = w.store.runs(10).unwrap().remove(0);
    assert_eq!(row.id, run);
    assert_eq!(row.started_by.as_deref(), Some("Claude Code"));

    // And so does the board, with a card that points at this run.
    let board = w.engine.board(&w.project_id).unwrap();
    assert_eq!(board.cards.len(), 1);
    assert_eq!(board.cards[0].title, "Fix the bug");
    assert_eq!(board.cards[0].run_id.as_deref(), Some(run.as_str()));
    assert_eq!(started["cardId"], json!(board.cards[0].id));

    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] != "working" {
            break;
        }
    }
    assert_eq!(view["state"], "waitingForPerson", "{view}");
    assert_eq!(view["waitingFor"]["state"], "waitingApproval");
    assert_eq!(view["steps"][0]["modelRequested"], "haiku");
    assert_eq!(view["report"]["answer"], "Fixed it.");

    let attempt = view["steps"][0]["attemptId"].as_str().unwrap();
    let step = ok(&call(&client, "get_step", json!({ "attempt": attempt })).await);
    assert_eq!(step["result"], "Fixed it.");
    assert_eq!(
        step["filesChanged"],
        json!([{ "path": "fixed.txt", "added": 1, "removed": 0 }]),
        "{step}"
    );
    // Its prompt repeats the request, so it comes only when asked for.
    assert!(step.get("prompt").is_none(), "{step}");
    let args = json!({ "attempt": attempt, "part": "prompt" });
    let prompt = ok(&call(&client, "get_step", args).await);
    assert!(
        prompt["text"].as_str().unwrap().contains("Fix the bug"),
        "{prompt}"
    );
    assert!(
        step["diff"].as_str().unwrap().contains("fixed.txt"),
        "{step}"
    );
    assert!(step.get("commandOutput").is_none(), "{step}");

    // Stopping doesn't answer it. It's only for a person, in the app.
    let refusal = refused(&call(&client, "stop_run", json!({ "run": run })).await);
    assert!(refusal.contains("only they can answer"), "{refusal}");
    let waiting = view["waitingFor"]["attemptId"].as_str().unwrap();
    assert_eq!(
        w.store.attempt(waiting).unwrap().state,
        AttemptState::WaitingApproval.as_str()
    );

    w.engine.answer_approval(waiting, true, "").unwrap();
    let mut finished = json!({});
    for _ in 0..20 {
        finished = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if finished["state"] == "finished" {
            break;
        }
    }
    assert_eq!(finished["state"], "finished", "{finished}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_step_asking_the_person_something_reads_as_waiting_for_them() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w
        .engine
        .save_workflow(
            &w.project_id,
            &Workflow {
                id: String::new(),
                name: "Asks first".into(),
                builtin: false,
                steps: vec![WorkflowStep::Agent(StepSpec {
                    id: "spec".into(),
                    model: "haiku".into(),
                    effort: None,
                    tools: ToolProfile::ReadOnly,
                    prompt: "{{request}}".into(),
                    max_turns: None,
                    max_budget_usd: None,
                    paths: vec![],
                    when: None,
                    risks: vec![],
                    model_when_risky: None,
                    role: StepRole::Specifier,
                })],
            },
        )
        .unwrap();
    let client = w.client(&w.token(true)).await;
    let args = json!({ "project": w.project_id, "workflow": workflow.id, "request": "Fix it" });
    let started = ok(&call(&client, "start_run", args).await);
    let run = started["id"].as_str().unwrap().to_owned();

    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] != "working" {
            break;
        }
    }
    assert_eq!(view["state"], "waitingForPerson", "{view}");
    assert_eq!(view["waitingFor"]["state"], "waitingAnswers");
    assert_eq!(view["steps"][0]["role"], "specifier");
    // Stopping isn't a way round it either.
    let refusal = refused(&call(&client, "stop_run", json!({ "run": run })).await);
    assert!(refusal.contains("waiting for the person"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_run_it_starts_counts_as_working_and_the_board_never_holds_it_back() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let hold = w._dir.path().join("bin/hold");
    std::fs::write(&hold, "").unwrap();
    w.engine.set_max_agents(1).unwrap();
    w.engine
        .update_project(
            &w.project_id,
            &keel_store::ProjectSettings {
                default_workflow: Some(w.workflow_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let working = || w.engine.board(&w.project_id).unwrap().working;

    ok(&call(&client, "start_run", w.start_args("First")).await);
    assert_eq!(working(), 1, "it takes up the only place");

    // A card waits for that place, and has to go on waiting.
    let card = w.engine.card_create(&w.project_id, "Waits", "").unwrap();
    w.engine.card_move(&card, CardLane::Todo, 0, "en").unwrap();
    w.engine.start_board();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let column = || {
        w.engine
            .board(&w.project_id)
            .unwrap()
            .cards
            .into_iter()
            .find(|c| c.id == card)
            .unwrap()
            .column
    };
    assert_eq!(column(), BoardColumn::Todo);

    // With no room on the board, the client is still let through.
    ok(&call(&client, "start_run", w.start_args("Second")).await);
    assert_eq!(working(), 2);
    assert_eq!(column(), BoardColumn::Todo);

    std::fs::remove_file(&hold).unwrap();
    for _ in 0..400 {
        if column() == BoardColumn::HumanReview {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(column(), BoardColumn::HumanReview, "and then the card goes");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_client_can_only_have_so_many_runs_open() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    for n in 0..MAX_OPEN_RUNS {
        ok(&call(&client, "start_run", w.start_args(&format!("Run {n}"))).await);
    }
    let refusal = refused(&call(&client, "start_run", w.start_args("One more")).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    assert_eq!(w.store.runs(10).unwrap().len(), MAX_OPEN_RUNS as usize);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn it_says_what_is_wrong_with_a_request() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let no_project = json!({ "project": "nope", "workflow": w.workflow_id, "request": "x" });
    assert!(refused(&call(&client, "start_run", no_project).await).contains("no project"));
    let no_workflow = json!({ "project": "app", "workflow": "nope", "request": "x" });
    assert!(refused(&call(&client, "start_run", no_workflow).await).contains("no workflow"));
    let nothing = json!({ "project": "app", "workflow": w.workflow_id });
    assert!(refused(&call(&client, "start_run", nothing).await).contains("Say what to do"));
    let language =
        json!({ "project": "app", "workflow": w.workflow_id, "request": "x", "language": "fr" });
    assert!(refused(&call(&client, "start_run", language).await).contains("`en` or `es`"));
    assert!(refused(&call(&client, "get_run", json!({ "run": "nope" })).await).contains("no run"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn every_call_is_on_record_also_the_refused_ones() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(false)).await;
    ok(&call(&client, "list_projects", json!({})).await);
    refused(&call(&client, "start_run", w.start_args("Fix it")).await);

    let calls = w.store.mcp_calls(10).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].tool, "start_run");
    assert_eq!(calls[0].client_name, "Claude Code");
    assert!(!calls[0].ok);
    assert!(calls[0].error.as_deref().unwrap().contains("may only read"));
    assert!(calls[0].args.contains("Fix it"));
    assert_eq!(calls[1].tool, "list_projects");
    assert!(calls[1].ok);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_client_gets_secrets_masked_and_the_record_keeps_them() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    let request = format!("Use API_KEY={secret} to test");

    let result = call(&client, "start_run", w.start_args(&request)).await;
    assert!(!format!("{result:?}").contains(&secret), "{result:?}");
    let started = ok(&result);
    assert_eq!(started["request"], format!("Use API_KEY={MASK} to test"));
    let run = started["id"].as_str().unwrap().to_owned();

    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] != "working" {
            break;
        }
    }
    let attempt = view["steps"][0]["attemptId"].as_str().unwrap();
    let step = ok(&call(&client, "get_step", json!({ "attempt": attempt })).await);
    assert!(!step.to_string().contains(&secret), "{step}");
    let args = json!({ "attempt": attempt, "part": "prompt" });
    let prompt = ok(&call(&client, "get_step", args).await);
    assert!(!prompt.to_string().contains(&secret), "{prompt}");
    assert!(prompt["text"].as_str().unwrap().contains(MASK), "{prompt}");

    // A refusal quotes what it was given.
    let wrong = format!("API_KEY={secret}");
    let refusal = refused(&call(&client, "get_brain", json!({ "project": wrong })).await);
    assert!(!refusal.contains(&secret), "{refusal}");
    assert!(refusal.contains(MASK), "{refusal}");

    // The agent worked from the original, and the audit log has what really happened.
    assert_eq!(w.store.run(&run).unwrap().request, request);
    assert!(
        w.store
            .attempt(attempt)
            .unwrap()
            .prompt
            .contains(&format!("API_KEY={secret}"))
    );
    let calls = w.store.mcp_calls(10).unwrap();
    let started = calls.iter().find(|c| c.tool == "start_run").unwrap();
    assert!(started.args.contains(&secret), "{started:?}");
    let refused = calls.iter().find(|c| c.tool == "get_brain").unwrap();
    assert!(refused.args.contains(&secret), "{refused:?}");
    assert!(
        refused.error.as_deref().unwrap().contains(&secret),
        "{refused:?}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_failed_setup_says_why_with_its_secrets_masked() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    w.engine
        .update_project(
            &w.project_id,
            &keel_store::ProjectSettings {
                setup_command: Some(format!(
                    "echo installing; echo 'token {secret}' >&2; echo 'error: no space left' >&2; exit 3"
                )),
                ..keel_store::ProjectSettings::default()
            },
        )
        .unwrap();

    let started = ok(&call(&client, "start_run", w.start_args("Fix the bug")).await);
    let run = started["id"].as_str().unwrap().to_owned();
    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] != "working" {
            break;
        }
    }
    assert_eq!(view["state"], "waitingForPerson", "{view}");
    assert_eq!(view["waitingFor"]["step"], "setup", "{view}");

    let attempt = view["waitingFor"]["attemptId"].as_str().unwrap();
    let step = ok(&call(&client, "get_step", json!({ "attempt": attempt })).await);
    let output = &step["commandOutput"];
    assert_eq!(output["exitCode"], 3, "{step}");
    assert_eq!(output["stdout"], "installing\n", "{step}");
    let stderr = output["stderr"].as_str().unwrap();
    assert!(stderr.contains("error: no space left"), "{stderr}");
    assert!(stderr.contains(MASK), "{stderr}");
    assert!(!step.to_string().contains(&secret), "{step}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn it_says_when_the_port_is_taken_and_stops_when_asked() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    assert_eq!(
        w.server.start(port).await.unwrap_err(),
        StartError::PortInUse { port }
    );
    assert_eq!(
        w.server.status().error,
        Some(StartError::PortInUse { port })
    );

    let addr = w.server.start(0).await.unwrap();
    assert!(w.server.status().running);
    w.server.stop().await;
    assert!(!w.server.status().running);
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn settings_stay_off_until_the_person_turns_them_on() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let settings = w.server.settings().unwrap();
    assert!(!settings.enabled);
    assert_eq!(settings.port, keel_mcp::DEFAULT_PORT);
    w.server
        .save_settings(&keel_mcp::McpSettings {
            enabled: true,
            port: 7500,
        })
        .unwrap();
    assert!(w.server.settings().unwrap().enabled);
    assert_eq!(w.server.settings().unwrap().port, 7500);
}

/// Everything a run can report on: a spec that asks, a build, a check, a review, a verifier, an
/// approver, and the person.
fn reporting_steps() -> Vec<WorkflowStep> {
    vec![
        WorkflowStep::Agent(agent("spec", ToolProfile::ReadOnly, StepRole::Specifier)),
        WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
        WorkflowStep::Check {
            id: "tests".into(),
            run: "true".into(),
            only_if_changed: false,
        },
        WorkflowStep::Parallel {
            id: "reviews".into(),
            steps: vec![agent("security", ToolProfile::ReadOnly, StepRole::Reviewer)],
        },
        WorkflowStep::Agent(agent("verify", ToolProfile::ReadOnly, StepRole::Verifier)),
        WorkflowStep::Agent(agent(
            "approver",
            ToolProfile::ReadOnly,
            StepRole::Approver {
                rework_to: "builder".into(),
                max_rounds: 2,
            },
        )),
        WorkflowStep::Approval { id: "human".into() },
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_reports_what_it_did_and_brings_what_the_person_has_to_decide() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w.saved("Everything", reporting_steps());
    let client = w.client(&w.steering_token("Claude Code")).await;
    let started = ok(&call(&client, "start_run", w.args_for(&workflow, "Fix the bug")).await);
    let run = id_of(&started);
    assert_eq!(started["startedByYou"], true);

    let asking = settled(&client, &run).await;
    let waiting = &asking["waitingFor"];
    assert_eq!(waiting["state"], "waitingAnswers", "{asking}");
    assert_eq!(
        waiting["questions"],
        json!([{
            "id": "Q1",
            "text": "What then?",
            "options": [{ "label": "Yes", "recommended": true }, { "label": "No", "recommended": false }],
        }])
    );
    let who = waiting["who"].as_str().unwrap();
    assert!(who.contains("Only the person answers"), "{who}");
    assert_eq!(waiting["youCan"], json!(["close_run"]));
    assert!(waiting.get("canSendBack").is_none(), "{waiting}");
    assert_eq!(
        asking["report"]["answer"],
        Json::Null,
        "nothing is built yet"
    );

    // The person answers, in Keel.
    let attempt = waiting["attemptId"].as_str().unwrap();
    let yes = Answer {
        question: "Q1".into(),
        choice: AnswerChoice::Option {
            label: "Yes".into(),
        },
        remember: false,
    };
    w.engine.answer_questions(attempt, &[yes]).unwrap();

    let view = settled(&client, &run).await;
    let waiting = &view["waitingFor"];
    assert_eq!(waiting["step"], "human", "{view}");
    assert_eq!(waiting["canSendBack"], true);
    assert_eq!(waiting["youCan"], json!(["send_back", "close_run"]));
    assert!(waiting.get("questions").is_none(), "{waiting}");
    let who = waiting["who"].as_str().unwrap();
    assert!(who.contains("Only the person approves"), "{who}");
    assert_eq!(
        view["report"],
        json!({
            "answer": "Fixed it.",
            "filesChanged": ["fixed.txt"],
            "check": { "step": "tests", "passed": true },
            // It said this was met without saying what proves it, so Keel didn't take its word.
            "notProven": [{
                "id": "C1", "when": "it runs", "then": "it works",
                "result": "unverified", "evidence": "", "corrected": "noEvidence",
                "why": "noEvidence",
            }],
            // What the review couldn't check by reading the code counts too, as in the app.
            "notProvenWhy": { "byReviews": 1, "noEvidence": 1 },
            "reviews": [
                { "step": "security", "verdict": "changes", "findings": ["Line 3 divides by zero"] },
                // What an approver that approved only noted isn't something to do.
                { "step": "approver", "decision": "approve", "risk": "low", "reasons": "It does what was asked", "instructions": null },
            ],
            // A run an assistant started never counts as small.
            "skipped": [],
            "prUrl": null,
            "prState": null,
            "prCi": null,
            "prCiFailed": [],
        })
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_names_what_was_skipped_for_being_small() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w.saved("Everything", reporting_steps());
    let client = w.client(&w.token(true)).await;
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&workflow, "Fix it"),
    )
    .await));
    let asking = settled(&client, &run).await;
    let attempt = asking["waitingFor"]["attemptId"].as_str().unwrap();
    w.engine
        .answer_questions(
            attempt,
            &[Answer {
                question: "Q1".into(),
                choice: AnswerChoice::Option {
                    label: "Yes".into(),
                },
                remember: false,
            }],
        )
        .unwrap();
    let view = settled(&client, &run).await;
    // As a run the person started could have it, which is the only kind that can be small.
    let security = view["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["step"] == "security")
        .unwrap();
    let reason = keel_core::Reason::TreatedAsSmall { lines: 1, files: 1 };
    w.store
        .set_state(
            security["attemptId"].as_str().unwrap(),
            AttemptState::Skipped,
            Some(&reason),
        )
        .unwrap();
    let view = ok(&call(&client, "get_run", json!({ "run": run })).await);
    assert_eq!(view["report"]["skipped"], json!(["security"]));
    assert_eq!(view["report"]["reviews"][0]["step"], "approver", "{view}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_check_says_why_and_only_the_client_that_started_it_may_act_if_allowed() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w.saved(
        "Checked",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Check {
                id: "tests".into(),
                run: "echo '3 failed' >&2; exit 1".into(),
                only_if_changed: false,
            },
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let owner = w.client(&w.steering_token("Owner")).await;
    let run = id_of(&ok(&call(
        &owner,
        "start_run",
        w.args_for(&workflow, "Fix it"),
    )
    .await));
    let view = settled(&owner, &run).await;
    assert_eq!(view["waitingFor"]["state"], "failed", "{view}");
    assert_eq!(view["waitingFor"]["step"], "tests", "{view}");
    // Keel had sent the work back to the builder twice by itself, which reads like any rework:
    // the round went up, and each check that went back says so and why it failed.
    assert_eq!(view["round"], 2, "{view}");
    assert_eq!(view["waitingFor"]["reason"]["failedTimes"], 3, "{view}");
    let checks: Vec<Json> = view["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["step"] == "tests")
        .map(|step| json!([step["round"], step["state"], step["reason"]["code"]]))
        .collect();
    assert_eq!(
        json!(checks),
        json!([
            [0, "returned", "checkFailed"],
            [1, "returned", "checkFailed"],
            [2, "failed", "checkFailed"]
        ]),
        "{view}"
    );
    let check = &view["report"]["check"];
    assert_eq!(check["step"], "tests", "{check}");
    assert_eq!(check["passed"], false, "{check}");
    assert_eq!(check["output"]["exitCode"], 1, "{check}");
    let stderr = check["output"]["stderr"].as_str().unwrap();
    assert!(stderr.contains("3 failed"), "{stderr}");
    assert_eq!(view["report"]["answer"], "Fixed it.");
    assert_eq!(view["startedByYou"], true);
    assert_eq!(
        view["waitingFor"]["youCan"],
        json!(["retry_step", "close_run"])
    );
    let who = view["waitingFor"]["who"].as_str().unwrap();
    assert!(who.contains("youCan"), "{who}");

    // Another client the person let give feedback didn't start it.
    let other = w.client(&w.steering_token("Other")).await;
    let seen = ok(&call(&other, "get_run", json!({ "run": run })).await);
    assert_eq!(seen["startedByYou"], false);
    assert_eq!(seen["waitingFor"]["youCan"], json!([]));
    let reader = w.client(&w.token(false)).await;
    let seen = ok(&call(&reader, "get_run", json!({ "run": run })).await);
    assert_eq!(seen["waitingFor"]["youCan"], json!([]));

    // What the client may do is read on every request.
    let row = w
        .store
        .mcp_clients()
        .unwrap()
        .into_iter()
        .find(|client| client.name == "Owner")
        .unwrap();
    w.store.set_mcp_client_can_steer(&row.id, false).unwrap();
    let seen = ok(&call(&owner, "get_run", json!({ "run": run })).await);
    assert_eq!(seen["startedByYou"], true);
    // Closing its own run comes with starting runs, not with giving feedback.
    assert_eq!(seen["waitingFor"]["youCan"], json!(["close_run"]));
    for client in [owner, other, reader] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_the_person_started_is_not_a_client_s_to_act_on() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let run = w.person_run("Fix it by hand");
    let view = settled(&client, &run).await;
    assert_eq!(view["startedByYou"], false);
    assert_eq!(view["waitingFor"]["state"], "waitingApproval", "{view}");
    assert_eq!(view["waitingFor"]["canSendBack"], true);
    assert_eq!(view["waitingFor"]["youCan"], json!([]));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_on_several_runs_returns_as_soon_as_one_of_them_stops_working() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let asks = w.saved(
        "Just ask",
        vec![WorkflowStep::Approval { id: "human".into() }],
    );
    let hold = w._dir.path().join("bin/hold");
    std::fs::write(&hold, "").unwrap();
    let busy = id_of(&ok(&call(&client, "start_run", w.start_args("Busy")).await));

    let waited = ok(&call(
        &client,
        "wait_for_run",
        json!({ "runs": [busy], "seconds": 1 }),
    )
    .await);
    assert_eq!(waited["ready"], json!([]), "it waited it out");
    assert_eq!(waited["runs"][0]["state"], "working");
    // One that's still working comes back in a few words.
    assert_eq!(waited["runs"][0]["brief"], true, "{waited}");
    assert_eq!(waited["runs"][0]["step"], "builder", "{waited}");
    assert!(waited["runs"][0].get("steps").is_none(), "{waited}");

    let quick = id_of(&ok(
        &call(&client, "start_run", w.args_for(&asks, "Quick")).await
    ));
    let began = std::time::Instant::now();
    let args = json!({ "runs": [busy, quick], "seconds": 30 });
    let waited = ok(&call(&client, "wait_for_run", args).await);
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "it didn't wait it out"
    );
    assert_eq!(waited["ready"], json!([quick]));
    assert_eq!(waited["runs"][0]["id"], json!(busy));
    assert_eq!(waited["runs"][0]["state"], "working");
    assert_eq!(waited["runs"][0]["brief"], true, "{waited}");
    assert_eq!(waited["runs"][1]["state"], "waitingForPerson");
    assert_eq!(waited["runs"][1]["waitingFor"]["step"], "human");
    assert!(waited["runs"][1].get("brief").is_none(), "{waited}");
    assert!(waited["runs"][1]["report"].is_object(), "{waited}");

    // `run` counts as one of them.
    let args = json!({ "run": busy, "runs": [quick], "seconds": 1 });
    let waited = ok(&call(&client, "wait_for_run", args).await);
    assert_eq!(waited["runs"].as_array().unwrap().len(), 2);

    std::fs::remove_file(&hold).unwrap();
    let mut waited = json!({});
    for _ in 0..20 {
        waited = ok(&call(
            &client,
            "wait_for_run",
            json!({ "runs": [busy], "seconds": 5 }),
        )
        .await);
        if waited["ready"] != json!([]) {
            break;
        }
    }
    assert_eq!(waited["ready"], json!([busy]), "{waited}");

    // With several ready, only the first comes in full, so the answer stays short. get_run has the
    // others.
    let args = json!({ "runs": [busy, quick], "seconds": 1 });
    let waited = ok(&call(&client, "wait_for_run", args).await);
    assert_eq!(waited["ready"], json!([busy, quick]), "{waited}");
    assert_eq!(waited["runs"][0]["waitingFor"]["step"], "human", "{waited}");
    assert_eq!(
        waited["runs"][1],
        json!({
            "id": quick, "title": "Quick", "state": "waitingForPerson", "round": 0,
            "step": "human", "stepState": "waitingApproval", "brief": true,
        })
    );

    let refusal = refused(&call(&client, "wait_for_run", json!({ "seconds": 1 })).await);
    assert!(refusal.contains("Say which run"), "{refusal}");
    let refusal = refused(&call(&client, "wait_for_run", json!({ "runs": [busy, "nope"] })).await);
    assert!(refusal.contains("no run \"nope\""), "{refusal}");
    let many: Vec<String> = (0..11).map(|n| format!("run-{n}")).collect();
    let refusal = refused(&call(&client, "wait_for_run", json!({ "runs": many })).await);
    assert!(refusal.contains("up to 10"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_can_find_the_runs_it_started_again() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let mine = w.client(&w.token(true)).await;
    let theirs = w.client(&w.token(true)).await;
    let first = id_of(&ok(&call(&mine, "start_run", w.start_args("Mine")).await));
    let other = id_of(&ok(
        &call(&theirs, "start_run", w.start_args("Theirs")).await
    ));
    w.person_run("The person's");
    let ids = |listed: Json| -> Vec<String> {
        listed["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(ok(&call(&mine, "list_runs", json!({})).await)).len(), 3);
    let listed = ok(&call(&mine, "list_runs", json!({ "mine": true })).await);
    assert_eq!(ids(listed), std::slice::from_ref(&first));
    let listed = ok(&call(
        &mine,
        "list_runs",
        json!({ "mine": true, "project": "app" }),
    )
    .await);
    assert_eq!(ids(listed), [first]);
    let listed = ok(&call(&theirs, "list_runs", json!({ "mine": true })).await);
    assert_eq!(ids(listed), [other]);
    mine.cancel().await.unwrap();
    theirs.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_keeps_to_the_number_of_open_runs_the_person_set() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_max_open_runs(&row.id, 1).unwrap();
    let client = w.client(&token).await;
    ok(&call(&client, "start_run", w.start_args("One")).await);
    let refusal = refused(&call(&client, "start_run", w.start_args("Two")).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    assert!(refusal.contains("Keel's settings"), "{refusal}");
    assert_eq!(w.store.runs(10).unwrap().len(), 1);

    // It's read on every request, so the person's change applies at once.
    w.store.set_mcp_client_max_open_runs(&row.id, 2).unwrap();
    ok(&call(&client, "start_run", w.start_args("Two")).await);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn what_an_agent_wrote_reaches_the_report_with_its_secrets_masked() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let run = id_of(&ok(
        &call(&client, "start_run", w.start_args("Fix it")).await
    ));
    let view = settled(&client, &run).await;
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    let builder = view["steps"][0]["attemptId"].as_str().unwrap();
    w.store
        .patch_attempt(
            builder,
            &[(
                "result_text",
                Value::Text(format!("Done. I used API_KEY={secret} to test.")),
            )],
        )
        .unwrap();
    let view = ok(&call(&client, "get_run", json!({ "run": run })).await);
    assert_eq!(
        view["report"]["answer"],
        format!("Done. I used API_KEY={MASK} to test.")
    );
    assert!(!view.to_string().contains(&secret), "{view}");
    client.cancel().await.unwrap();
}

impl World {
    /// A builder whose check fails, then the person.
    fn failing_check(&self) -> String {
        self.saved(
            "Checked",
            vec![
                WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
                WorkflowStep::Check {
                    id: "tests".into(),
                    run: "echo '3 failed' >&2; exit 1".into(),
                    only_if_changed: false,
                },
                WorkflowStep::Approval { id: "human".into() },
            ],
        )
    }

    /// The run's card, once it's in that column.
    async fn card_in(&self, run: &str, column: BoardColumn) -> keel_engine::BoardCard {
        let id = self.store.card_of_run(run).unwrap().unwrap();
        let mut card = None;
        for _ in 0..100 {
            let board = self.engine.board(&self.project_id).unwrap();
            card = board.cards.into_iter().find(|c| c.id == id);
            if card.as_ref().is_some_and(|c| c.column == column) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let card = card.unwrap();
        assert_eq!(card.column, column, "{card:?}");
        card
    }

    fn hold(&self) -> PathBuf {
        let hold = self._dir.path().join("bin/hold");
        std::fs::write(&hold, "").unwrap();
        hold
    }
}

fn waiting_attempt(view: &Json) -> String {
    view["waitingFor"]["attemptId"].as_str().unwrap().to_owned()
}

/// What only a client the person lets give feedback may use. close_run only needs `can_run`: see
/// `a_client_closes_only_the_runs_it_started_and_only_while_it_may_run`.
const FEEDBACK: [&str; 2] = ["send_back", "retry_step"];

fn feedback_args(tool: &str, run: &str) -> Json {
    match tool {
        "send_back" => json!({ "run": run, "instructions": "Return 0 instead" }),
        _ => json!({ "run": run }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_needs_the_person_s_permission_and_a_run_of_the_client_s_own() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let owner = w.client(&w.token(true)).await;
    let run = id_of(&ok(&call(&owner, "start_run", w.start_args("Fix it")).await));
    let waiting = waiting_attempt(&settled(&owner, &run).await);

    // It may run, but the person hasn't let it give feedback.
    for tool in FEEDBACK {
        let refusal = refused(&call(&owner, tool, feedback_args(tool, &run)).await);
        assert!(
            refusal.contains("may not give feedback"),
            "{tool}: {refusal}"
        );
        assert!(refusal.contains("Keel's settings"), "{tool}: {refusal}");
    }
    let reader = w.client(&w.token(false)).await;
    for tool in FEEDBACK {
        let refusal = refused(&call(&reader, tool, feedback_args(tool, &run)).await);
        assert!(refusal.contains("may only read"), "{tool}: {refusal}");
    }
    // Allowed to give feedback, but on a run another client started, or the person did.
    let other = w.client(&w.steering_token("Other")).await;
    let person = w.person_run("By hand");
    settled(&other, &person).await;
    for run in [&run, &person] {
        for tool in FEEDBACK {
            let refusal = refused(&call(&other, tool, feedback_args(tool, run)).await);
            assert!(refusal.contains("didn't start"), "{tool}: {refusal}");
        }
    }
    let refusal = refused(&call(&other, "close_run", json!({ "run": "nope" })).await);
    assert!(refusal.contains("no run"), "{refusal}");

    assert_eq!(
        w.store.attempt(&waiting).unwrap().state,
        AttemptState::WaitingApproval.as_str(),
        "nothing changed"
    );
    assert!(w.store.run(&person).unwrap().finished_at.is_none());
    let calls = w.store.mcp_calls(100).unwrap();
    assert!(
        calls
            .iter()
            .filter(|c| FEEDBACK.contains(&c.tool.as_str()))
            .all(|c| !c.ok),
        "each refusal is on record"
    );
    for client in [owner, reader, other] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_sends_its_run_back_from_the_approval_and_the_builder_hears_who_it_was() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let hold = w.hold();
    let run = id_of(&ok(
        &call(&client, "start_run", w.start_args("Fix it")).await
    ));
    let refusal = refused(&call(&client, "send_back", feedback_args("send_back", &run)).await);
    assert!(
        refusal.contains("isn't waiting at an approval"),
        "{refusal}"
    );
    std::fs::remove_file(&hold).unwrap();
    let view = settled(&client, &run).await;
    assert_eq!(view["waitingFor"]["state"], "waitingApproval", "{view}");

    for empty in ["", "  \n "] {
        let args = json!({ "run": run, "instructions": empty });
        let refusal = refused(&call(&client, "send_back", args).await);
        assert!(refusal.contains("`instructions`"), "{refusal}");
    }
    let args = json!({ "run": run, "instructions": "x".repeat(16 * 1024 + 1) });
    let refusal = refused(&call(&client, "send_back", args).await);
    assert!(refusal.contains("16 KB"), "{refusal}");

    let approval = waiting_attempt(&view);
    let args = json!({ "run": run, "instructions": "Return 0 instead" });
    let sent = ok(&call(&client, "send_back", args).await);
    assert_eq!(sent["state"], "working", "{sent}");
    let returned = w.store.attempt(&approval).unwrap();
    assert_eq!(returned.state, "returned");
    assert_eq!(
        returned.output.unwrap(),
        json!({ "instructions": "Return 0 instead", "by": "Claude Code" })
    );
    let event = w
        .store
        .events(&approval, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "returned")
        .unwrap();
    assert_eq!(event.data["by"], "Claude Code");

    let view = settled(&client, &run).await;
    assert_eq!(view["waitingFor"]["step"], "human", "{view}");
    assert_eq!(view["round"], 1);
    let builder = view["steps"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|step| step["step"] == "builder")
        .unwrap();
    let prompt = w
        .store
        .attempt(builder["attemptId"].as_str().unwrap())
        .unwrap()
        .prompt;
    assert!(
        prompt.contains("Claude Code, an assistant the person allowed, sent this work back."),
        "{prompt}"
    );
    assert!(prompt.contains("Return 0 instead"), "{prompt}");

    // Two more, and then it's the person's.
    for n in 2..=3 {
        let args = json!({ "run": run, "instructions": format!("Round {n}") });
        ok(&call(&client, "send_back", args).await);
        let view = settled(&client, &run).await;
        assert_eq!(view["waitingFor"]["step"], "human", "{view}");
    }
    assert_eq!(
        view_of(&client, &run).await["waitingFor"]["youCan"],
        json!(["close_run"])
    );
    let args = json!({ "run": run, "instructions": "Once more" });
    let refusal = refused(&call(&client, "send_back", args).await);
    assert!(refusal.contains("3 times"), "{refusal}");
    assert!(refusal.contains("Bring it to the person"), "{refusal}");
    assert_eq!(w.store.run(&run).unwrap().round, 3);
    client.cancel().await.unwrap();
}

/// Words up to about `bytes`.
fn at_length(words: &str, bytes: usize) -> String {
    let mut text = String::new();
    while text.len() < bytes {
        text.push_str(words);
        text.push(' ');
    }
    text
}

/// Makes what a run's agents wrote as long as a real run's: KBs of spec, reviews, evidence and
/// answers, and a change to 150 files. A structured answer is also the attempt's text, as Claude
/// Code returns it. Returns how many bytes that is.
fn write_at_length(store: &Store, run: &str) -> usize {
    let files: Vec<String> = (0..150)
        .map(|n| format!("src/part_{n}/file_{n}.rs"))
        .collect();
    let files = json!(files).to_string();
    let attempts = store.attempts_of_run(run).unwrap();
    let mut written = 0;
    for attempt in &attempts {
        let mut fields = Vec::new();
        if let Some(mut output) = attempt.output.clone() {
            output["summary"] = json!(at_length("Every part of it holds up.", 6 * 1024));
            let text = output.to_string();
            written += 2 * text.len();
            fields.push(("output", Value::Text(text.clone())));
            fields.push(("result_text", Value::Text(text)));
        } else if attempt.kind == "agent" {
            let text = at_length("It's fixed and tested.", 2 * 1024);
            written += text.len() + files.len();
            fields.push(("result_text", Value::Text(text)));
            fields.push(("files_changed", Value::Text(files.clone())));
        }
        if !fields.is_empty() {
            store.patch_attempt(&attempt.id, &fields).unwrap();
        }
    }
    let last = &attempts.last().unwrap().id;
    store
        .patch_attempt(last, &[("run_files", Value::Text(files))])
        .unwrap();
    written
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_went_round_again_reads_in_one_go_and_says_each_thing_once() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w.saved("Everything", reporting_steps());
    let client = w.client(&w.steering_token("Claude Code")).await;
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&workflow, "Fix the bug"),
    )
    .await));

    // While the spec asks, its questions come whole. The rest of what it wrote is in get_step, once.
    let spec = waiting_attempt(&settled(&client, &run).await);
    write_at_length(&w.store, &run);
    let asking = view_of(&client, &run).await;
    assert_eq!(
        asking["waitingFor"]["questions"],
        json!([{
            "id": "Q1",
            "text": "What then?",
            "options": [{ "label": "Yes", "recommended": true }, { "label": "No", "recommended": false }],
        }])
    );
    assert!(!asking.to_string().contains("holds up"), "{asking}");
    let step = ok(&call(&client, "get_step", json!({ "attempt": spec })).await);
    let summary = step["output"]["summary"].as_str().unwrap();
    assert!(summary.contains("holds up"), "{step}");
    assert!(step.get("result").is_none(), "{step}");

    // The person answers, the client sends the work back once, and it's at the approval again.
    let yes = Answer {
        question: "Q1".into(),
        choice: AnswerChoice::Option {
            label: "Yes".into(),
        },
        remember: false,
    };
    w.engine.answer_questions(&spec, &[yes]).unwrap();
    let approval = settled(&client, &run).await;
    assert_eq!(approval["waitingFor"]["step"], "human", "{approval}");
    let args = json!({ "run": run, "instructions": "Return 0 instead" });
    ok(&call(&client, "send_back", args).await);
    let settled_again = settled(&client, &run).await;
    assert_eq!(settled_again["round"], 1, "{settled_again}");

    let written = write_at_length(&w.store, &run);
    let attempts = w.store.attempts_of_run(&run).unwrap();
    let view = view_of(&client, &run).await;
    // A real run of 22 attempts that wrote less than this one came to 72 KB when each attempt
    // carried what it wrote, more than Claude Code shows of a tool's answer. Now a run's view is
    // what it waits for and its report, whole, and a few hundred bytes per attempt, so 20 KB leaves
    // room for a longer answer, more rounds and longer paths.
    assert!(written > 80 * 1024, "{written}");
    let size = view.to_string().len();
    assert!(size < 20 * 1024, "{size} bytes: {view}");
    // wait_for_run returns it the same way, without the request.
    let waited = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 1 })).await);
    let mut without_request = view.clone();
    without_request.as_object_mut().unwrap().remove("request");
    assert_eq!(waited, without_request);

    // What it waits for comes whole.
    let human = attempts.last().unwrap();
    assert_eq!(human.step_id, "human");
    let waiting = &view["waitingFor"];
    assert_eq!(waiting["attemptId"], json!(human.id), "{waiting}");
    assert_eq!(waiting["state"], "waitingApproval");
    assert_eq!(waiting["canSendBack"], true);
    assert_eq!(waiting["youCan"], json!(["send_back", "close_run"]));
    let who = waiting["who"].as_str().unwrap();
    assert!(who.contains("Only the person approves"), "{who}");

    // And so does the report, with the first 100 files and how many more there are.
    let report = &view["report"];
    assert_eq!(
        report["answer"],
        json!(at_length("It's fixed and tested.", 2 * 1024))
    );
    assert_eq!(report["filesChanged"].as_array().unwrap().len(), 100);
    assert_eq!(report["filesChanged"][0], "src/part_0/file_0.rs");
    assert_eq!(report["moreFilesChanged"], 50);
    assert_eq!(
        report["reviews"][0]["findings"],
        json!(["Line 3 divides by zero"])
    );
    assert_eq!(report["reviews"][1]["decision"], "approve");

    // Every attempt is there, in order, without what it wrote. One that a later attempt of the same
    // step replaced is shorter.
    let steps = view["steps"].as_array().unwrap();
    assert_eq!(steps.len(), attempts.len(), "{view}");
    assert!(view.get("earlierAttemptsLeftOut").is_none(), "{view}");
    let mut superseded = 0;
    for (step, attempt) in steps.iter().zip(&attempts) {
        assert_eq!(step["attemptId"], json!(attempt.id));
        for in_get_step in ["result", "output", "filesChanged"] {
            assert!(step.get(in_get_step).is_none(), "{step}");
        }
        let latest = attempts
            .iter()
            .rev()
            .find(|a| a.step_id == attempt.step_id)
            .unwrap();
        let size = step.to_string().len();
        if latest.id == attempt.id {
            assert!(step.get("superseded").is_none(), "{step}");
            assert_eq!(step["state"], json!(attempt.state));
            assert_eq!(step["modelRequested"], json!(attempt.model_requested));
            assert!(size < 500, "{size} bytes: {step}");
        } else {
            superseded += 1;
            let mut keys: Vec<&str> = step
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "attemptId",
                    "number",
                    "reason",
                    "round",
                    "state",
                    "step",
                    "superseded"
                ]
            );
            assert_eq!(step["superseded"], true);
            assert!(size < 250, "{size} bytes: {step}");
        }
    }
    // The first round's builder, check, review, verifier, approver and approval.
    assert!(superseded >= 6, "{view}");
    let builder = steps.iter().rev().find(|s| s["step"] == "builder").unwrap();
    assert_eq!(builder["round"], 1);

    // get_step has each attempt in full, and what it answered once.
    let latest = |id: &str| {
        let step = steps.iter().rev().find(|s| s["step"] == id).unwrap();
        json!({ "attempt": step["attemptId"] })
    };
    let review = ok(&call(&client, "get_step", latest("security")).await);
    assert_eq!(review["output"]["verdict"], "changes", "{review}");
    assert!(review.get("result").is_none(), "{review}");
    let built = ok(&call(&client, "get_step", latest("builder")).await);
    assert_eq!(built["result"], report["answer"], "{built}");
    assert!(built.get("output").is_none(), "{built}");
    // The first 100 files, and how many more there are.
    assert_eq!(built["filesChanged"].as_array().unwrap().len(), 100);
    assert_eq!(
        built["filesChanged"][0],
        json!({ "path": "src/part_0/file_0.rs" })
    );
    assert_eq!(built["moreFilesChanged"], 50);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn following_a_run_does_not_repeat_its_request_or_a_reviewer_s_prompt() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let workflow = w.saved(
        "Build and review",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Parallel {
                id: "reviews".into(),
                steps: vec![agent("security", ToolProfile::ReadOnly, StepRole::Reviewer)],
            },
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let client = w.client(&w.token(true)).await;
    // A request as long as a real one, which every agent's prompt repeats.
    let request = at_length("Fix the parser so names with spaces work.", 8 * 1024)
        .trim()
        .to_owned();
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&workflow, &request),
    )
    .await));

    // Waiting on it doesn't bring the request back each time. get_run has it.
    let waited = settled(&client, &run).await;
    assert_eq!(waited["waitingFor"]["step"], "human", "{waited}");
    assert!(waited.get("request").is_none(), "{waited}");
    assert!(!waited.to_string().contains(&request), "{waited}");
    let mut got = view_of(&client, &run).await;
    assert_eq!(got["request"], json!(request));
    got.as_object_mut().unwrap().remove("request");
    assert_eq!(waited, got);
    // Nor does waiting on several.
    let args = json!({ "runs": [run], "seconds": 1 });
    let many = ok(&call(&client, "wait_for_run", args).await);
    assert_eq!(many["runs"][0], waited, "{many}");

    // A reviewer's step is what it answered, without the prompt that repeats the request. It says
    // how long the prompt is, and `part` reads it whole.
    let steps = waited["steps"].as_array().unwrap();
    let review = steps.iter().find(|s| s["step"] == "security").unwrap();
    let review = review["attemptId"].as_str().unwrap().to_owned();
    let prompt = keel_core::mask_secrets(&w.store.attempt(&review).unwrap().prompt);
    assert!(prompt.len() > 8 * 1024, "{}", prompt.len());
    let step = ok(&call(&client, "get_step", json!({ "attempt": review })).await);
    assert_eq!(step["output"]["verdict"], "changes", "{step}");
    assert!(step.get("prompt").is_none(), "{step}");
    let size = step.to_string().len();
    assert!(size < 2 * 1024, "{size} bytes: {step}");
    assert_eq!(step["leftOut"], json!({ "prompt": prompt.len() }), "{step}");
    let (read, _) = read_whole(&client, json!({ "attempt": review, "part": "prompt" })).await;
    assert!(read == prompt, "the pieces differ from the prompt");
    client.cancel().await.unwrap();
}

/// A diff as git writes it: a file moved, one removed, a binary one, one with a space in its name,
/// one too long for a piece with what JSON escapes and characters of several bytes, then `small`
/// short new files. Returns it, and each file's path and diff in order.
fn big_diff(small: usize) -> (String, Vec<(String, String)>) {
    let new_file = |path: &str, lines: Vec<String>| {
        // Git puts a tab after a name with a space.
        let tab = if path.contains(' ') { "\t" } else { "" };
        let mut text = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..1111111\n\
             --- /dev/null\n+++ b/{path}{tab}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for line in lines {
            text.push_str(&format!("+{line}\n"));
        }
        text
    };
    let mut files = vec![
        (
            "src/b.rs".to_owned(),
            "diff --git a/src/a.rs b/src/b.rs\nsimilarity index 100%\nrename from src/a.rs\n\
             rename to src/b.rs\n"
                .to_owned(),
        ),
        (
            "old.txt".to_owned(),
            "diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex 1111111..0000000\n\
             --- a/old.txt\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-one\n-two\n-three\n"
                .to_owned(),
        ),
        (
            "logo.png".to_owned(),
            "diff --git a/logo.png b/logo.png\nnew file mode 100644\nindex 0000000..1111111\n\
             Binary files /dev/null and b/logo.png differ\n"
                .to_owned(),
        ),
    ];
    let lines = vec!["# Read me".to_owned(), "Ñandú 🦀".to_owned()];
    files.push(("docs/read me.md".into(), new_file("docs/read me.md", lines)));
    let lines = (0..1500)
        .map(|n| format!("\tlet s{n} = \"ñandú \\\\ 🦀 {n}\";"))
        .collect();
    files.push(("src/big.rs".into(), new_file("src/big.rs", lines)));
    for n in 0..small {
        let path = format!("src/part_{n}/file_{n}.rs");
        let lines = (0..10)
            .map(|l| format!("pub fn f{l}() -> u32 {{ {l} }}"))
            .collect();
        files.push((path.clone(), new_file(&path, lines)));
    }
    let diff = files.iter().map(|(_, text)| text.as_str()).collect();
    (diff, files)
}

/// Reads one part of an attempt piece by piece, from the start, as a client would, following each
/// piece's `next`. Each piece reads in one go and says how much is left. Returns the part and how
/// many pieces it came in.
async fn read_whole(client: &Client, args: Json) -> (String, usize) {
    let mut args = args;
    let mut text = String::new();
    let mut pieces = 0;
    loop {
        let piece = ok(&call(client, "get_step", args).await);
        pieces += 1;
        let size = piece.to_string().len();
        assert!(size < 20 * 1024, "{size} bytes");
        assert_eq!(piece["from"], json!(text.len()), "{piece}");
        text.push_str(piece["text"].as_str().unwrap());
        let bytes = piece["bytes"].as_u64().unwrap() as usize;
        assert_eq!(piece["left"], json!(bytes - text.len()), "{piece}");
        match piece.get("next") {
            Some(next) => args = next.clone(),
            None => {
                assert_eq!(text.len(), bytes);
                return (text, pieces);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_big_change_comes_in_short_and_every_part_of_it_reads_whole_in_pieces() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;

    // A small attempt comes whole in one call, as before, but for its prompt, which repeats the
    // request.
    let run = w.person_run("Fix the bug");
    let view = settled(&client, &run).await;
    let small = view["steps"][0]["attemptId"].as_str().unwrap().to_owned();
    let step = ok(&call(&client, "get_step", json!({ "attempt": small })).await);
    assert!(step.get("prompt").is_none(), "{step}");
    let prompt = w.store.attempt(&small).unwrap().prompt;
    assert_eq!(step["leftOut"], json!({ "prompt": prompt.len() }), "{step}");
    let to_read = step["toRead"].as_str().unwrap();
    assert!(to_read.contains("`part`"), "{to_read}");
    assert_eq!(
        step["diff"],
        json!(w.engine.diff(&small).unwrap()),
        "{step}"
    );
    assert_eq!(step["result"], "Fixed it.");
    assert_eq!(
        step["filesChanged"],
        json!([{ "path": "fixed.txt", "added": 1, "removed": 0 }])
    );

    // A builder that got a long prompt, changed 124 files in about 100 KB, and answered at length.
    // Put together here, so no secret scanner trips on this file.
    let secret = format!("{}{}", "ghp_", "x".repeat(36));
    let request = format!(
        "{}\nUse API_KEY={secret} to test.\n{}",
        at_length("Fix the parser so \"quoted\" names and ñ work.", 30 * 1024),
        at_length("Then say what changed.", 10 * 1024)
    );
    let run = w.person_run(&request);
    let view = settled(&client, &run).await;
    let builder = view["steps"][0]["attemptId"].as_str().unwrap().to_owned();
    let (diff, files) = big_diff(119);
    assert!(diff.len() > 64 * 1024, "{}", diff.len());
    let diff_path = w._dir.path().join("big.diff");
    std::fs::write(&diff_path, &diff).unwrap();
    let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let answer = at_length("It's done and tested, ñandú.", 20 * 1024);
    let fields = [
        (
            "diff_path",
            Value::Text(diff_path.to_string_lossy().into_owned()),
        ),
        ("files_changed", Value::Text(json!(paths).to_string())),
        ("result_text", Value::Text(answer.clone())),
    ];
    w.store.patch_attempt(&builder, &fields).unwrap();
    let prompt = keel_core::mask_secrets(&w.store.attempt(&builder).unwrap().prompt);
    assert!(prompt.len() > 40 * 1024, "{}", prompt.len());
    // Every file it changed, as `git diff --numstat` lists them.
    let numstat: String = paths
        .iter()
        .map(|path| {
            let (added, removed) = match *path {
                "src/b.rs" | "logo.png" => (0, 0),
                "old.txt" => (0, 3),
                "docs/read me.md" => (2, 0),
                "src/big.rs" => (1500, 0),
                _ => (10, 0),
            };
            format!("{added}\t{removed}\t{path}\n")
        })
        .collect();

    // It comes in short, and says what it left out and how to read it.
    let step = ok(&call(&client, "get_step", json!({ "attempt": builder })).await);
    let size = step.to_string().len();
    assert!(size < 20 * 1024, "{size} bytes: {step}");
    assert_eq!(
        step["leftOut"],
        json!({
            "diff": diff.len(),
            "files": numstat.len(),
            "prompt": prompt.len(),
            "answer": answer.len(),
        })
    );
    let to_read = step["toRead"].as_str().unwrap();
    assert!(
        to_read.contains("`part`") && to_read.contains("`next`"),
        "{to_read}"
    );
    assert!(step.get("diff").is_none(), "{step}");
    assert!(step.get("prompt").is_none(), "{step}");
    assert!(!step.to_string().contains(&secret), "{step}");
    // What it answered, from the start, saying where it's cut.
    let result = step["result"].as_str().unwrap();
    let (shown, cut) = result.rsplit_once("\n… [").unwrap();
    assert!(
        shown.len() > 4 * 1024 && answer.starts_with(shown),
        "{result}"
    );
    assert!(cut.contains("part answer"), "{cut}");
    // The files it changed, with what each adds and removes, and how many more there are.
    let listed = step["filesChanged"].as_array().unwrap();
    let more = step["moreFilesChanged"].as_u64().unwrap() as usize;
    assert_eq!(listed.len() + more, files.len(), "{step}");
    assert_eq!(listed.len(), 100, "{step}");
    assert_eq!(
        listed[..5],
        [
            json!({ "path": "src/b.rs", "added": 0, "removed": 0 }),
            json!({ "path": "old.txt", "added": 0, "removed": 3 }),
            json!({ "path": "logo.png", "added": 0, "removed": 0 }),
            json!({ "path": "docs/read me.md", "added": 2, "removed": 0 }),
            json!({ "path": "src/big.rs", "added": 1500, "removed": 0 }),
        ]
    );
    assert_eq!(
        listed[99],
        json!({ "path": "src/part_94/file_94.rs", "added": 10, "removed": 0 })
    );

    // The diff, piece by piece, is all of it, byte for byte.
    let (read, pieces) = read_whole(&client, json!({ "attempt": builder, "part": "diff" })).await;
    assert!(read == diff, "the pieces differ from the diff");
    assert!(pieces > diff.len() / (20 * 1024), "{pieces}");
    // And so is one file's, which also takes more than one piece.
    let (path, big) = &files[4];
    let (read, pieces) = read_whole(&client, json!({ "attempt": builder, "file": path })).await;
    assert!(read == *big, "the pieces differ from {path}");
    assert!(pieces > 1, "{pieces}");
    let (path, spaced) = &files[3];
    let args = json!({ "attempt": builder, "part": "diff", "file": path });
    assert_eq!(read_whole(&client, args).await, (spaced.clone(), 1));
    let (path, removed) = &files[1];
    let args = json!({ "attempt": builder, "file": path });
    assert_eq!(read_whole(&client, args).await, (removed.clone(), 1));
    // A piece can start anywhere.
    let args = json!({ "attempt": builder, "part": "diff", "from": 100 });
    let piece = ok(&call(&client, "get_step", args).await);
    assert_eq!(piece["from"], 100);
    assert!(diff[100..].starts_with(piece["text"].as_str().unwrap()));

    // Every file it changed, past the 100 listed.
    let (read, _) = read_whole(&client, json!({ "attempt": builder, "part": "files" })).await;
    assert_eq!(read, numstat);

    // The prompt, masked as it always is, and the answer.
    let (read, _) = read_whole(&client, json!({ "attempt": builder, "part": "prompt" })).await;
    assert!(read == prompt, "the pieces differ from the prompt");
    assert!(read.contains(&format!("API_KEY={MASK} to test")));
    assert!(!read.contains(&secret));
    let (read, _) = read_whole(&client, json!({ "attempt": builder, "part": "answer" })).await;
    assert!(read == answer, "the pieces differ from the answer");

    // What it can't do says why.
    for (args, says) in [
        (
            json!({ "part": "diff", "from": diff.len() + 1 }),
            "`from` goes up to",
        ),
        (json!({ "file": "nope.rs" }), "no file \"nope.rs\""),
        (
            json!({ "part": "prompt", "file": "src/big.rs" }),
            "only goes with part diff",
        ),
        (json!({ "from": 10 }), "goes with `part`"),
        (json!({ "part": "stdout" }), "setup or check"),
    ] {
        let mut args = args;
        args["attempt"] = json!(builder);
        let refusal = refused(&call(&client, "get_step", args).await);
        assert!(refusal.contains(says), "{refusal}");
    }
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_of_a_run_whose_copy_keel_freed_still_reads_whole() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let run = w.person_run("Fix the bug");
    let view = settled(&client, &run).await;
    let builder = view["steps"][0]["attemptId"].as_str().unwrap().to_owned();
    let before = ok(&call(&client, "get_step", json!({ "attempt": builder })).await);
    assert!(before["diff"].as_str().unwrap().contains("fixed.txt"));

    // The person rejected it long ago, and Keel freed the clone it worked in.
    w.engine
        .answer_approval(&waiting_attempt(&view), false, "Not this way")
        .unwrap();
    let long_ago = Value::Text("2026-01-01T00:00:00.000Z".into());
    w.store
        .patch_run(&run, &[("finished_at", long_ago)])
        .unwrap();
    let workspace = PathBuf::from(w.store.attempt(&builder).unwrap().workspace);
    // What its build left goes through the same trash, right when it ends.
    for _ in 0..500 {
        if !workspace.with_file_name("trash").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pass = w.engine.free_old_copies();
    assert_eq!((pass.freed, pass.failed), (vec![run.clone()], vec![]));
    assert!(!workspace.exists());

    let after = ok(&call(&client, "get_step", json!({ "attempt": builder })).await);
    assert_eq!(after["diff"], before["diff"]);
    assert_eq!(after["filesChanged"], before["filesChanged"]);
    assert_eq!(after["result"], before["result"]);
    let part = json!({ "attempt": builder, "part": "prompt" });
    let (prompt, _) = read_whole(&client, part).await;
    assert!(prompt.contains("Fix the bug"), "{prompt}");
    ok(&call(&client, "get_run", json!({ "run": run })).await);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_printed_a_lot_shows_how_it_ended_and_reads_whole_in_pieces() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let loud = w.saved(
        "Loud",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Check {
                id: "tests".into(),
                run: "i=0; while [ $i -lt 4000 ]; do echo \"test $i of the suite: ok\"; \
                      i=$((i+1)); done; echo 'error: 3 failed' >&2; exit 1"
                    .into(),
                only_if_changed: false,
            },
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&loud, "Fix it"),
    )
    .await));
    let check = waiting_attempt(&settled(&client, &run).await);
    let recorded = w
        .store
        .events(&check, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "checkResult")
        .unwrap()
        .data["stdout"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(recorded.len() > 32 * 1024, "{}", recorded.len());

    // The end of what it printed, from a whole line, which is where it says what went wrong.
    let step = ok(&call(&client, "get_step", json!({ "attempt": check })).await);
    let size = step.to_string().len();
    assert!(size < 20 * 1024, "{size} bytes: {step}");
    let output = &step["commandOutput"];
    assert_eq!(output["exitCode"], 1, "{step}");
    assert_eq!(output["stderr"], "error: 3 failed\n");
    let stdout = output["stdout"].as_str().unwrap();
    let (cut, tail) = stdout.split_once('\n').unwrap();
    assert!(cut.contains("part stdout"), "{cut}");
    assert!(
        tail.starts_with("test ") && recorded.ends_with(tail),
        "{tail}"
    );
    assert!(tail.ends_with("test 3999 of the suite: ok\n"), "{tail}");
    assert_eq!(step["leftOut"], json!({ "stdout": recorded.len() }));

    // All of it, in pieces.
    let (read, pieces) = read_whole(&client, json!({ "attempt": check, "part": "stdout" })).await;
    assert!(read == recorded, "the pieces differ from what it printed");
    assert!(pieces > 1, "{pieces}");
    let args = json!({ "attempt": check, "part": "stderr" });
    assert_eq!(
        read_whole(&client, args).await,
        ("error: 3 failed\n".to_owned(), 1)
    );
    client.cancel().await.unwrap();
}

async fn view_of(client: &Client, run: &str) -> Json {
    ok(&call(client, "get_run", json!({ "run": run })).await)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_retries_only_a_step_that_failed_was_interrupted_or_stopped() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let checked = w.failing_check();
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&checked, "Fix it"),
    )
    .await));
    let failed = waiting_attempt(&settled(&client, &run).await);

    let retried = ok(&call(&client, "retry_step", json!({ "run": run })).await);
    assert_ne!(waiting_attempt(&settled(&client, &run).await), failed);
    let event = w
        .store
        .events(&failed, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "retryRequested")
        .unwrap();
    assert_eq!(event.data, json!({ "by": "Claude Code", "resume": false }));
    assert_eq!(retried["id"], json!(run));

    // What's blocked or stalled is the person's to judge.
    let view = view_of(&client, &run).await;
    let latest = waiting_attempt(&view);
    let reason = keel_core::Reason::ReworkLimit { rounds: 1 };
    w.store
        .set_state(&latest, AttemptState::Blocked, Some(&reason))
        .unwrap();
    let refusal = refused(&call(&client, "retry_step", json!({ "run": run })).await);
    assert!(refusal.contains("person's to decide"), "{refusal}");
    assert_eq!(
        view_of(&client, &run).await["waitingFor"]["youCan"],
        json!(["close_run"])
    );

    // As if the app had gone down while it ran.
    w.store
        .set_state(&latest, AttemptState::Interrupted, None)
        .unwrap();
    ok(&call(&client, "retry_step", json!({ "run": run })).await);
    settled(&client, &run).await;
    ok(&call(&client, "retry_step", json!({ "run": run, "resume": true })).await);
    settled(&client, &run).await;
    let refusal = refused(&call(&client, "retry_step", json!({ "run": run })).await);
    assert!(refusal.contains("3 times"), "{refusal}");

    // A stopped step, on a run that then waits at an approval, where there's nothing to retry.
    let hold = w.hold();
    let other = id_of(&ok(
        &call(&client, "start_run", w.start_args("Fix that")).await
    ));
    for _ in 0..100 {
        if view_of(&client, &other).await["steps"][0]["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ok(&call(&client, "stop_run", json!({ "run": other })).await);
    let stopped = settled(&client, &other).await;
    assert_eq!(stopped["waitingFor"]["state"], "stopped", "{stopped}");
    assert_eq!(
        stopped["waitingFor"]["youCan"],
        json!(["retry_step", "close_run"])
    );
    std::fs::remove_file(&hold).unwrap();
    ok(&call(&client, "retry_step", json!({ "run": other })).await);
    let view = settled(&client, &other).await;
    assert_eq!(view["waitingFor"]["state"], "waitingApproval", "{view}");
    let refusal = refused(&call(&client, "retry_step", json!({ "run": other })).await);
    assert!(refusal.contains("nothing to retry"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_retries_with_the_project_s_current_commands_only_when_it_asks() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let set_setup = |setup: &str| {
        let settings = keel_store::ProjectSettings {
            setup_command: Some(setup.into()),
            check_command: Some("make test".into()),
            ..keel_store::ProjectSettings::default()
        };
        w.engine.update_project(&w.project_id, &settings).unwrap();
    };
    set_setup("true");
    // Keel puts the project's setup first. The check is the workflow's own, whatever the
    // project's is.
    let checked = w.saved(
        "Checked",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Check {
                id: "check".into(),
                run: "test -f installed.txt".into(),
                only_if_changed: false,
            },
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&checked, "Fix it"),
    )
    .await));
    let failed = settled(&client, &run).await;
    assert_eq!(failed["waitingFor"]["step"], "check", "{failed}");
    assert_eq!(failed["waitingFor"]["state"], "failed", "{failed}");
    assert!(failed.get("commandsChanged").is_none(), "{failed}");

    set_setup("touch installed.txt");
    assert_eq!(
        view_of(&client, &run).await["commandsChanged"],
        json!([{ "step": "setup", "kind": "setup", "before": "true", "now": "touch installed.txt" }])
    );
    // Without asking, it runs what the run started with.
    ok(&call(&client, "retry_step", json!({ "run": run })).await);
    let again = settled(&client, &run).await;
    assert_eq!(again["waitingFor"]["state"], "failed", "{again}");

    // Asking, the new setup runs, and then the check again.
    let args = json!({ "run": run, "useCurrentCommands": true });
    ok(&call(&client, "retry_step", args).await);
    let view = settled(&client, &run).await;
    assert_eq!(view["waitingFor"]["state"], "waitingApproval", "{view}");
    assert!(view.get("commandsChanged").is_none(), "{view}");
    let attempts = w.store.attempts_of_run(&run).unwrap();
    let ran = |step: &str| -> Vec<(i64, String, String)> {
        attempts
            .iter()
            .filter(|a| a.step_id == step)
            .map(|a| (a.number, a.prompt.clone(), a.state.clone()))
            .collect()
    };
    let at = |number: i64, command: &str, state: &str| (number, command.into(), state.into());
    assert_eq!(
        ran("setup"),
        vec![
            at(1, "true", "succeeded"),
            at(2, "touch installed.txt", "succeeded")
        ]
    );
    // What ran keeps how it ended. Keel had sent the work back to the builder twice by itself
    // before the check waited. The check put off for the setup says so, and came round again.
    assert_eq!(
        ran("check"),
        vec![
            at(1, "test -f installed.txt", "returned"),
            at(2, "test -f installed.txt", "returned"),
            at(3, "test -f installed.txt", "failed"),
            at(4, "test -f installed.txt", "failed"),
            at(5, "test -f installed.txt", "postponed"),
            at(6, "test -f installed.txt", "succeeded"),
        ]
    );
    let second = attempts
        .iter()
        .find(|a| a.step_id == "check" && a.number == 4)
        .unwrap();
    assert_eq!(second.reason.as_ref().unwrap()["code"], "checkFailed");
    assert_eq!(
        ran("builder").len(),
        3,
        "the agent didn't run again for either retry"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_asking_for_new_commands_when_there_are_none_is_told_so() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let set_commands = |setup: &str, check: &str| {
        let settings = keel_store::ProjectSettings {
            setup_command: Some(setup.into()),
            check_command: Some(check.into()),
            ..keel_store::ProjectSettings::default()
        };
        w.engine.update_project(&w.project_id, &settings).unwrap();
    };
    // Keel puts the project's setup first. The check is the workflow's own.
    set_commands("exit 4", "make test");
    let checked = w.saved(
        "Checked",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Check {
                id: "check".into(),
                run: "exit 3".into(),
                only_if_changed: false,
            },
            WorkflowStep::Approval { id: "human".into() },
        ],
    );
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&checked, "Fix it"),
    )
    .await));
    let failed = settled(&client, &run).await;
    assert_eq!(failed["waitingFor"]["step"], "setup", "{failed}");
    let asked = json!({ "run": run, "useCurrentCommands": true });
    let mut refusals = 0;

    // The project's setup is the run's.
    let refusal = refused(&call(&client, "retry_step", asked.clone()).await);
    refusals += 1;
    assert!(refusal.contains("nothing new to run"), "{refusal}");
    assert!(
        refusal.contains("the same as the project's now"),
        "{refusal}"
    );
    let view = view_of(&client, &run).await;
    assert_eq!(
        view["waitingFor"]["attemptId"],
        failed["waitingFor"]["attemptId"]
    );

    // Once it's fixed, the new setup runs, and the run goes on to the workflow's check.
    set_commands("true", "make test");
    ok(&call(&client, "retry_step", asked.clone()).await);
    let failed = settled(&client, &run).await;
    assert_eq!(failed["waitingFor"]["step"], "check", "{failed}");
    assert_eq!(failed["waitingFor"]["state"], "failed", "{failed}");

    // The project's commands are the run's, and this check is the workflow's own. Changing the
    // project's check doesn't change that: it never follows it.
    for check in ["make test", "echo fixed"] {
        set_commands("true", check);
        let refusal = refused(&call(&client, "retry_step", asked.clone()).await);
        refusals += 1;
        assert!(refusal.contains("nothing new to run"), "{refusal}");
        assert!(
            refusal.contains("a check of the saved workflow \"Checked\""),
            "{refusal}"
        );
    }
    let view = view_of(&client, &run).await;
    assert_eq!(
        view["waitingFor"]["attemptId"],
        failed["waitingFor"]["attemptId"]
    );

    // None of the refusals used up one of the client's 3 retries: one went on the setup.
    assert_eq!(refusals, 3);
    for _ in 0..2 {
        ok(&call(&client, "retry_step", json!({ "run": run })).await);
        let again = settled(&client, &run).await;
        assert_eq!(again["waitingFor"]["state"], "failed", "{again}");
    }
    refused(&call(&client, "retry_step", json!({ "run": run })).await);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_takes_new_commands_only_for_a_setup_or_a_check() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let hold = w.hold();
    let run = id_of(&ok(
        &call(&client, "start_run", w.start_args("Fix that")).await
    ));
    for _ in 0..100 {
        if view_of(&client, &run).await["steps"][0]["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ok(&call(&client, "stop_run", json!({ "run": run })).await);
    let stopped = settled(&client, &run).await;
    assert_eq!(stopped["waitingFor"]["step"], "builder", "{stopped}");
    std::fs::remove_file(&hold).unwrap();

    let args = json!({ "run": run, "useCurrentCommands": true, "resume": true });
    let refusal = refused(&call(&client, "retry_step", args).await);
    assert!(refusal.contains("setup or a check"), "{refusal}");
    assert_eq!(
        settled(&client, &run).await["waitingFor"]["state"],
        "stopped"
    );
    ok(&call(&client, "retry_step", json!({ "run": run, "resume": true })).await);
    let view = settled(&client, &run).await;
    assert_eq!(view["waitingFor"]["state"], "waitingApproval", "{view}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_closes_its_waiting_run_with_nothing_delivered_and_never_one_that_works() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.steering_token("Claude Code")).await;
    let hold = w.hold();
    let run = id_of(&ok(&call(
        &client,
        "start_run",
        w.start_args("Fix the bug\n\nAnd its test."),
    )
    .await));
    let refusal = refused(&call(&client, "close_run", json!({ "run": run })).await);
    assert!(refusal.contains("stop_run first"), "{refusal}");
    std::fs::remove_file(&hold).unwrap();
    let view = settled(&client, &run).await;
    let approval = waiting_attempt(&view);
    assert_eq!(
        view["waitingFor"]["youCan"],
        json!(["send_back", "close_run"])
    );

    let closed = ok(&call(&client, "close_run", json!({ "run": run })).await);
    assert_eq!(closed["state"], "finished", "{closed}");
    assert_eq!(closed["waitingFor"], Json::Null);
    assert_eq!(closed["prUrl"], Json::Null);
    assert_eq!(closed["report"]["prUrl"], Json::Null);
    assert_eq!(w.store.attempt(&approval).unwrap().state, "abandoned");
    let event = w
        .store
        .events(&approval, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "abandoned")
        .unwrap();
    assert_eq!(event.data["by"], "Claude Code");

    // The card goes back for the person, with what it said.
    let card = w.card_in(&run, BoardColumn::Backlog).await;
    assert_eq!(card.title, "Fix the bug");

    let refusal = refused(&call(&client, "close_run", json!({ "run": run })).await);
    assert!(refusal.contains("has finished"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_closes_a_failed_run_of_its_own_and_that_makes_room_for_another() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.engine
        .update_project(
            &w.project_id,
            &keel_store::ProjectSettings {
                setup_command: Some("echo 'error: no space left' >&2; exit 3".into()),
                ..keel_store::ProjectSettings::default()
            },
        )
        .unwrap();
    // It may start runs, and the person hasn't let it give feedback.
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    assert!(!row.can_steer);
    let client = w.client(&token).await;
    let mut runs = Vec::new();
    for n in 0..MAX_OPEN_RUNS {
        let started = ok(&call(&client, "start_run", w.start_args(&format!("Fix {n}"))).await);
        let view = settled(&client, &id_of(&started)).await;
        assert_eq!(view["waitingFor"]["step"], "setup", "{view}");
        assert_eq!(view["waitingFor"]["state"], "failed", "{view}");
        // Retrying is feedback. Closing comes with starting runs.
        assert_eq!(view["waitingFor"]["youCan"], json!(["close_run"]));
        runs.push(id_of(&started));
    }
    let refusal = refused(&call(&client, "start_run", w.start_args("One more")).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    assert!(refusal.contains("close_run"), "{refusal}");
    assert!(refusal.contains("`mine`"), "{refusal}");
    assert_eq!(
        w.store.open_runs_of_mcp_client(&row.id).unwrap(),
        MAX_OPEN_RUNS
    );

    let run = &runs[0];
    let setup = waiting_attempt(&view_of(&client, run).await);
    let closed = ok(&call(&client, "close_run", json!({ "run": run })).await);
    assert_eq!(closed["state"], "finished", "{closed}");
    assert_eq!(closed["waitingFor"], Json::Null);
    assert_eq!(closed["prUrl"], Json::Null);
    // As when the person leaves it in the app, and the run says which client it was.
    assert_eq!(w.store.attempt(&setup).unwrap().state, "abandoned");
    let event = w
        .store
        .events(&setup, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "abandoned")
        .unwrap();
    assert_eq!(event.data, json!({ "by": "Claude Code" }));
    let on_record = w.store.mcp_calls(1).unwrap().remove(0);
    assert_eq!(on_record.tool, "close_run");
    assert!(on_record.ok);
    assert_eq!(on_record.run_id.as_deref(), Some(run.as_str()));
    let card = w.card_in(run, BoardColumn::Backlog).await;
    assert_eq!(card.title, "Fix 0");
    // The others are still there for it, or for the person.
    for other in &runs[1..] {
        assert!(w.store.run(other).unwrap().finished_at.is_none());
    }

    // Its place is free again, and only that one.
    assert_eq!(
        w.store.open_runs_of_mcp_client(&row.id).unwrap(),
        MAX_OPEN_RUNS - 1
    );
    ok(&call(&client, "start_run", w.start_args("One more")).await);
    let refusal = refused(&call(&client, "start_run", w.start_args("And another")).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_closes_only_the_runs_it_started_and_only_while_it_may_run() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    let owner = w.client(&token).await;
    let run = id_of(&ok(&call(&owner, "start_run", w.start_args("Fix it")).await));
    let approval = waiting_attempt(&settled(&owner, &run).await);
    let person = w.person_run("By hand");
    settled(&owner, &person).await;

    // The person started this one.
    let refusal = refused(&call(&owner, "close_run", json!({ "run": person })).await);
    assert!(refusal.contains("didn't start"), "{refusal}");
    assert_eq!(
        view_of(&owner, &person).await["waitingFor"]["youCan"],
        json!([])
    );
    // Another client didn't start either, even with the same name and allowed to give feedback,
    // as Claude Code is once connected again.
    let other = w.client(&w.steering_token("Claude Code")).await;
    for run in [&run, &person] {
        let refusal = refused(&call(&other, "close_run", json!({ "run": run })).await);
        assert!(refusal.contains("didn't start"), "{refusal}");
        assert_eq!(
            view_of(&other, run).await["waitingFor"]["youCan"],
            json!([])
        );
    }
    // One that may only read closes nothing, not even what it started before the person said so.
    let reader = w.client(&w.token(false)).await;
    let refusal = refused(&call(&reader, "close_run", json!({ "run": run })).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    w.store.set_mcp_client_can_run(&row.id, false).unwrap();
    let refusal = refused(&call(&owner, "close_run", json!({ "run": run })).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    assert_eq!(
        view_of(&owner, &run).await["waitingFor"]["youCan"],
        json!([])
    );

    assert_eq!(
        w.store.attempt(&approval).unwrap().state,
        AttemptState::WaitingApproval.as_str(),
        "nothing changed"
    );
    assert!(w.store.run(&person).unwrap().finished_at.is_none());
    let closes: Vec<_> = w
        .store
        .mcp_calls(100)
        .unwrap()
        .into_iter()
        .filter(|c| c.tool == "close_run")
        .collect();
    assert_eq!(closes.len(), 5);
    assert!(closes.iter().all(|c| !c.ok), "each refusal is on record");

    // Once it may run again, it closes its own while it waits for the person's approval. That
    // withdraws what it asked for and decides nothing: nothing is delivered.
    w.store.set_mcp_client_can_run(&row.id, true).unwrap();
    assert_eq!(
        view_of(&owner, &run).await["waitingFor"]["youCan"],
        json!(["close_run"])
    );
    let closed = ok(&call(&owner, "close_run", json!({ "run": run })).await);
    assert_eq!(closed["state"], "finished", "{closed}");
    assert_eq!(closed["prUrl"], Json::Null);
    assert_eq!(w.store.attempt(&approval).unwrap().state, "abandoned");
    for client in [owner, other, reader] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_cannot_close_a_run_that_went_through() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    // The person approved it, and it finished.
    let run = id_of(&ok(
        &call(&client, "start_run", w.start_args("Fix it")).await
    ));
    let approval = waiting_attempt(&settled(&client, &run).await);
    w.engine.answer_approval(&approval, true, "").unwrap();
    let mut view = json!({});
    for _ in 0..20 {
        view = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 5 })).await);
        if view["state"] == "finished" {
            break;
        }
    }
    assert_eq!(view["state"], "finished", "{view}");
    // A task that finished on its own.
    let task = id_of(&ok(&call(
        &client,
        "start_task",
        json!({ "project": "app", "prompt": "What does it do?" }),
    )
    .await));
    assert_eq!(settled(&client, &task).await["state"], "finished");

    for run in [&run, &task] {
        let before = w.store.attempts_of_run(run).unwrap();
        let refusal = refused(&call(&client, "close_run", json!({ "run": run })).await);
        assert!(refusal.contains("has finished"), "{refusal}");
        let after = w.store.attempts_of_run(run).unwrap();
        assert_eq!(
            before.iter().map(|a| &a.state).collect::<Vec<_>>(),
            after.iter().map(|a| &a.state).collect::<Vec<_>>(),
            "nothing changed"
        );
        for attempt in &after {
            let events = w.store.events(&attempt.id, 0).unwrap();
            assert!(events.iter().all(|e| e.kind != "abandoned"), "{events:?}");
        }
    }
    // Keel's own path refuses it too, should the run finish between a client looking and acting.
    assert!(w.engine.abandon_by(&approval, "Claude Code").is_err());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_is_one_agent_that_only_reads_and_its_finding_is_the_report_s_answer() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    // A project that sets up before building. A task builds nothing, so it doesn't.
    w.engine
        .update_project(
            &w.project_id,
            &keel_store::ProjectSettings {
                setup_command: Some("echo installing".into()),
                ..keel_store::ProjectSettings::default()
            },
        )
        .unwrap();
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    let client = w.client(&token).await;
    let args = json!({
        "project": "app",
        "prompt": "What does the login do?\n\nSay where it checks the password.",
        // A title is one line: what breaks it into more is a space.
        "title": "How the login\n  works",
        "effort": "low",
    });
    let started = ok(&call(&client, "start_task", args).await);
    let run = id_of(&started);
    assert_eq!(started["startedByYou"], true);
    assert_eq!(started["startedBy"], "Claude Code");
    assert_eq!(started["title"], "How the login works");
    let card = w.store.card_of_run(&run).unwrap().unwrap();
    assert_eq!(w.store.card(&card).unwrap().title, "How the login works");
    assert_eq!(started["workflowId"], Json::Null);

    let steps: Vec<WorkflowStep> =
        serde_json::from_value(w.store.run(&run).unwrap().workflow).unwrap();
    let [WorkflowStep::Agent(task)] = steps.as_slice() else {
        panic!("{steps:?}")
    };
    assert_eq!(task.id, "task");
    assert_eq!(task.tools, ToolProfile::ReadOnly);
    assert_eq!(task.role, StepRole::Worker);
    assert_eq!(task.model, "sonnet");
    assert_eq!(task.effort.as_deref(), Some("low"));

    let done = settled(&client, &run).await;
    assert_eq!(done["state"], "finished", "{done}");
    assert_eq!(
        done["report"]["answer"],
        "The login checks the password twice."
    );
    assert_eq!(done["report"]["filesChanged"], json!([]));
    assert_eq!(done["report"]["check"], Json::Null);
    assert_eq!(done["prUrl"], Json::Null);
    assert_eq!(done["steps"].as_array().unwrap().len(), 1, "no setup ran");
    assert!(w.store.run_started_by_mcp_client(&run, &row.id).unwrap());
    let calls = w.store.mcp_calls(100).unwrap();
    let started = calls.iter().find(|c| c.tool == "start_task").unwrap();
    assert_eq!(started.run_id.as_deref(), Some(run.as_str()));

    // It's on the board under its title, and ends there with nothing delivered.
    let card = w.card_in(&run, BoardColumn::Done).await;
    assert_eq!(card.title, "How the login works");
    let listed = ok(&call(&client, "list_runs", json!({ "mine": true })).await);
    assert_eq!(listed["runs"][0]["id"], json!(run));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_task_can_ask_for_nothing_more_than_reading() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let reader = w.client(&w.token(false)).await;
    let args = json!({ "project": "app", "prompt": "What does it do?" });
    let refusal = refused(&call(&reader, "start_task", args).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    reader.cancel().await.unwrap();

    let client = w.client(&w.token(true)).await;
    let with = |extra: Json| {
        let mut args = json!({ "project": "app", "prompt": "What does it do?" });
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        args
    };
    let cases = [
        (
            with(json!({ "model": "--dangerously-skip-permissions" })),
            "model's name",
        ),
        (with(json!({ "model": "opus model" })), "model's name"),
        (with(json!({ "maxTurns": 0 })), "above zero"),
        (with(json!({ "maxBudgetUsd": 0.0 })), "above zero"),
        (with(json!({ "prompt": "  " })), "`prompt`"),
        (
            with(json!({ "prompt": "x".repeat(16 * 1024 + 1) })),
            "`prompt`",
        ),
        (with(json!({ "title": "x".repeat(81) })), "80 characters"),
        (with(json!({ "language": "fr" })), "`en` or `es`"),
        (with(json!({ "project": "nope" })), "no project"),
        // What a task can't have is refused, not ignored.
        (with(json!({ "tools": "build" })), "tools"),
        (
            with(json!({ "allowedDomains": ["evil.example"] })),
            "allowedDomains",
        ),
    ];
    for (args, says) in cases {
        let refusal = refused(&call(&client, "start_task", args.clone()).await);
        assert!(refusal.contains(says), "{args}: {refusal}");
    }
    assert!(w.store.runs(10).unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_counts_toward_the_runs_a_client_may_have_open() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_max_open_runs(&row.id, 1).unwrap();
    let client = w.client(&token).await;
    let hold = w.hold();
    let task = json!({ "project": "app", "prompt": "What does it do?" });
    let run = id_of(&ok(&call(&client, "start_task", task.clone()).await));
    let refusal = refused(&call(&client, "start_task", task.clone()).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    let refusal = refused(&call(&client, "start_run", w.start_args("Fix it")).await);
    assert!(refusal.contains("runs open"), "{refusal}");

    std::fs::remove_file(&hold).unwrap();
    assert_eq!(settled(&client, &run).await["state"], "finished");
    ok(&call(&client, "start_task", task).await);
    client.cancel().await.unwrap();
}

// What a client sends at once, from several sessions, goes through as if it came one at a time.

impl World {
    /// As many sessions of one client, to send its calls at once.
    async fn sessions(&self, token: &str, count: usize) -> Vec<Client> {
        let mut sessions = Vec::new();
        for _ in 0..count {
            sessions.push(self.client(token).await);
        }
        sessions
    }
}

/// Each call from its own session, all sent at once, and what each got back, in the same order.
async fn at_once(sessions: &[Client], calls: &[(&str, Json)]) -> Vec<CallToolResult> {
    assert!(calls.len() <= sessions.len());
    let mut sent = tokio::task::JoinSet::new();
    for (index, (session, (tool, args))) in sessions.iter().zip(calls).enumerate() {
        let peer = session.peer().clone();
        let Json::Object(arguments) = args.clone() else {
            panic!("arguments are an object")
        };
        let request = CallToolRequestParams::new((*tool).to_owned()).with_arguments(arguments);
        sent.spawn(async move { (index, peer.call_tool(request).await.unwrap()) });
    }
    let mut results = sent.join_all().await;
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

fn went_through(results: &[CallToolResult]) -> usize {
    results.iter().filter(|r| r.is_error == Some(false)).count()
}

/// Stops the run's latest step once it runs, and waits until it has stopped.
async fn stop_once_running(client: &Client, run: &str) {
    for _ in 0..100 {
        let view = view_of(client, run).await;
        if view["steps"].as_array().unwrap().last().unwrap()["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ok(&call(client, "stop_run", json!({ "run": run })).await);
    let stopped = settled(client, run).await;
    assert_eq!(stopped["waitingFor"]["state"], "stopped", "{stopped}");
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_sent_at_once_start_the_step_once_and_never_more_often_than_allowed() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let sessions = w.sessions(&w.steering_token("Claude Code"), 8).await;
    let client = &sessions[0];
    let _hold = w.hold();
    let run = id_of(&ok(&call(client, "start_run", w.start_args("Fix it")).await));
    let retries = vec![("retry_step", json!({ "run": run })); sessions.len()];
    for round in 1..=4 {
        stop_once_running(client, &run).await;
        let results = at_once(&sessions, &retries).await;
        let numbers: Vec<i64> = w
            .store
            .attempts_of_run(&run)
            .unwrap()
            .iter()
            .filter(|a| a.step_id == "builder")
            .map(|a| a.number)
            .collect();
        if round <= 3 {
            assert_eq!(went_through(&results), 1, "round {round}: {results:?}");
            assert_eq!(numbers, (1..=round + 1).collect::<Vec<_>>());
        } else {
            assert_eq!(went_through(&results), 0, "{results:?}");
            for result in &results {
                assert!(refused(result).contains("3 times"), "{result:?}");
            }
            assert_eq!(numbers, [1, 2, 3, 4]);
        }
    }
    let calls = w.store.mcp_calls(100).unwrap();
    let retried = calls.iter().filter(|c| c.tool == "retry_step" && c.ok);
    assert_eq!(retried.count(), 3);
    for session in sessions {
        session.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_and_a_close_sent_at_once_never_leave_a_step_working_in_a_run_that_ended() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_can_steer(&row.id, true).unwrap();
    let sessions = w.sessions(&token, 2).await;
    let client = &sessions[0];
    let _hold = w.hold();
    for trial in 0..4 {
        let run = id_of(&ok(&call(
            client,
            "start_run",
            w.start_args(&format!("Fix {trial}")),
        )
        .await));
        stop_once_running(client, &run).await;
        let retry = ("retry_step", json!({ "run": run }));
        let close = ("close_run", json!({ "run": run }));
        // Sent the other way round every other time.
        let (retried, closed) = if trial % 2 == 0 {
            let results = at_once(&sessions, &[retry, close]).await;
            (results[0].clone(), results[1].clone())
        } else {
            let results = at_once(&sessions, &[close, retry]).await;
            (results[1].clone(), results[0].clone())
        };
        let (retried, closed) = (
            retried.is_error == Some(false),
            closed.is_error == Some(false),
        );
        assert!(retried != closed, "trial {trial}: one of them, never both");
        let finished = w.store.run(&run).unwrap().finished_at.is_some();
        let working = w
            .store
            .attempts_of_run(&run)
            .unwrap()
            .iter()
            .any(|a| AttemptState::parse(&a.state).unwrap().is_active());
        assert_eq!(finished, closed, "trial {trial}");
        assert_eq!(working, retried, "trial {trial}");
        // What the retry started holds the project's place: it ends before the next trial.
        if retried {
            stop_once_running(client, &run).await;
            ok(&call(client, "close_run", json!({ "run": run })).await);
        }
    }
    for session in sessions {
        session.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_started_at_once_keep_to_the_number_the_person_set_and_waiting_holds_none_up() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_max_open_runs(&row.id, 1).unwrap();
    let sessions = w.sessions(&token, 8).await;
    let _hold = w.hold();
    let task = json!({ "project": "app", "prompt": "What does it do?" });
    let starts: Vec<(&str, Json)> = (0..sessions.len())
        .map(|n| match n % 2 {
            0 => ("start_task", task.clone()),
            _ => ("start_run", w.start_args(&format!("Fix {n}"))),
        })
        .collect();
    let results = at_once(&sessions, &starts).await;
    assert_eq!(went_through(&results), 1, "{results:?}");
    for result in results.iter().filter(|r| r.is_error == Some(true)) {
        assert!(refused(result).contains("runs open"), "{result:?}");
    }
    assert_eq!(w.store.runs(10).unwrap().len(), 1);
    let run = w.store.runs(10).unwrap()[0].id.clone();

    // A long wait for that run doesn't keep the same client from being answered meanwhile.
    let peer = sessions[1].peer().clone();
    let Json::Object(arguments) = json!({ "run": run, "seconds": 5 }) else {
        unreachable!()
    };
    let waiting = tokio::spawn(async move {
        let request = CallToolRequestParams::new("wait_for_run").with_arguments(arguments);
        peer.call_tool(request).await.unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let began = std::time::Instant::now();
    let refusal = refused(&call(&sessions[0], "start_task", task).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "it waited for the wait: {:?}",
        began.elapsed()
    );
    assert_eq!(ok(&waiting.await.unwrap())["state"], "working");
    for session in sessions {
        session.cancel().await.unwrap();
    }
}

#[tokio::test]
async fn a_long_list_of_runs_to_wait_for_is_refused_at_once() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let client = w.client(&w.token(false)).await;
    // About 800 KB, under what a request may carry. Going through it would take a while.
    let runs: Vec<String> = (0..100_000).map(|n| n.to_string()).collect();
    let began = std::time::Instant::now();
    let args = json!({ "runs": runs, "seconds": 1 });
    let refusal = refused(&call(&client, "wait_for_run", args).await);
    assert!(refusal.contains("up to 10"), "{refusal}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    // Counted as given, repeats too.
    let args = json!({ "runs": vec!["same"; 11], "seconds": 1 });
    let refusal = refused(&call(&client, "wait_for_run", args).await);
    assert!(refusal.contains("up to 10"), "{refusal}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_across_projects_starts_its_agreement_in_the_first_one_and_waits_for_the_person() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let lib = w.other_project("lib");
    let docs = w.other_project("docs");
    let client = w.client(&w.token(true)).await;

    let args = json!({
        "project": "app",
        "alsoTouches": ["lib", docs],
        "request": "Log in through lib",
    });
    let started = ok(&call(&client, "propose_change", args).await);
    let run = id_of(&started);
    assert_eq!(started["startedBy"], "Claude Code");
    assert_eq!(started["startedByYou"], true);
    assert_eq!(started["jointChange"]["role"], "agreement");

    let view = settled(&client, &run).await;
    assert_eq!(view["state"], "waitingForPerson", "{view}");
    assert_eq!(view["waitingFor"]["state"], "waitingApproval");
    assert_eq!(view["projectId"], json!(w.project_id));

    // The card is in app, started by the client, and the change is agreed on it.
    let card = w.store.card(view["cardId"].as_str().unwrap()).unwrap();
    assert_eq!(card.project_id, w.project_id);
    assert_eq!(card.joint_change_id.as_deref(), Some(card.id.as_str()));
    assert_eq!(
        w.store.joint_change(&card.id).unwrap().projects,
        [lib.clone(), docs.clone()]
    );
    let board = w.engine.board(&w.project_id).unwrap();
    assert_eq!(board.cards[0].also_touches, [lib, docs]);
    let row = w.store.runs(10).unwrap().remove(0);
    assert_eq!(row.started_by.as_deref(), Some("Claude Code"));

    // It only reads, and nothing but the person's approval opens a part.
    assert_eq!(w.store.attempts_of_run(&run).unwrap()[0].tools, "readOnly");
    assert_eq!(w.cards_everywhere(), 1);
    assert!(view["jointChange"]["parts"].as_array().unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_across_projects_is_refused_when_it_should_be_and_creates_nothing() {
    keel_harness::needs!(sockets);
    let w = world().await;
    let lib = w.other_project("lib");
    let reader = w.client(&w.token(false)).await;
    let args = json!({ "project": "app", "alsoTouches": ["lib"], "request": "Do it" });
    let start_run_refusal = refused(&call(&reader, "start_run", w.start_args("Do it")).await);
    let refusal = refused(&call(&reader, "propose_change", args.clone()).await);
    assert_eq!(refusal, start_run_refusal);
    reader.cancel().await.unwrap();

    let client = w.client(&w.token(true)).await;
    let refusal = refused(
        &call(
            &client,
            "propose_change",
            json!({ "project": "app", "alsoTouches": ["lib", "billing"], "request": "Do it" }),
        )
        .await,
    );
    assert!(refusal.contains("billing"), "{refusal}");
    // Only its own project, by name or id, or none at all, isn't a change across projects.
    for also in [
        json!(["app"]),
        json!([w.project_id]),
        json!([]),
        json!(["app", "app"]),
    ] {
        let refusal = refused(
            &call(
                &client,
                "propose_change",
                json!({ "project": "app", "alsoTouches": also, "request": "Do it" }),
            )
            .await,
        );
        assert!(refusal.contains("alsoTouches"), "{refusal}");
    }
    let refusal = refused(
        &call(
            &client,
            "propose_change",
            json!({ "project": "app", "alsoTouches": ["lib"] }),
        )
        .await,
    );
    assert!(refusal.contains("request"), "{refusal}");
    let refusal = refused(
        &call(
            &client,
            "propose_change",
            json!({ "project": "nope", "alsoTouches": ["lib"], "request": "x" }),
        )
        .await,
    );
    assert!(refusal.contains("nope"), "{refusal}");
    assert_eq!(w.cards_everywhere(), 0);
    assert!(w.store.runs(10).unwrap().is_empty());

    // The limit on open runs applies to the agreement.
    let (limited, token) = w.server.create_client("Limited", true).unwrap();
    w.store
        .set_mcp_client_max_open_runs(&limited.id, 1)
        .unwrap();
    let limited = w.client(&token).await;
    ok(&call(&limited, "propose_change", args.clone()).await);
    let refusal = refused(&call(&limited, "propose_change", args).await);
    assert!(refusal.contains("as many runs open"), "{refusal}");
    assert_eq!(w.cards_everywhere(), 1);
    // Duplicates and the project's own are ignored, as on the board.
    let (_, token) = w.server.create_client("Other", true).unwrap();
    let other = w.client(&token).await;
    let started = ok(&call(
        &other,
        "propose_change",
        json!({ "project": "app", "alsoTouches": ["app", "lib", lib, "lib"], "request": "Again" }),
    )
    .await);
    let card = w.store.card(started["cardId"].as_str().unwrap()).unwrap();
    assert_eq!(w.store.joint_change(&card.id).unwrap().projects, [lib]);
    for client in [limited, other, client] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn once_the_person_agrees_the_parts_open_and_are_the_client_s_to_follow_and_send_back() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    let lib = w.other_project("lib");
    w.use_by_default(&w.project_id, &w.workflow_id);
    w.engine.start_board();
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_can_steer(&row.id, true).unwrap();
    // As few open runs as it can be allowed: the parts open all the same.
    w.store.set_mcp_client_max_open_runs(&row.id, 1).unwrap();
    let steering = w.client(&token).await;
    let started = ok(&call(
        &steering,
        "propose_change",
        json!({ "project": "app", "alsoTouches": ["lib"], "request": "Log in through lib" }),
    )
    .await);
    let agreement = id_of(&started);
    let view = settled(&steering, &agreement).await;
    let approval = view["waitingFor"]["attemptId"].as_str().unwrap().to_owned();
    // The client can't answer it, and nothing in its view says how to.
    assert!(!view["waitingFor"]["youCan"].to_string().contains("approve"));

    w.engine
        .answer_approval(&approval, true, "Go ahead")
        .unwrap();

    // The first part, in lib, starts on its own, and the one in app waits for it.
    let mut parts = json!([]);
    for _ in 0..200 {
        let view = ok(&call(&steering, "get_run", json!({ "run": agreement })).await);
        parts = view["jointChange"]["parts"].clone();
        if parts.as_array().is_some_and(|p| p.len() == 2) && parts[0]["runId"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(parts[0]["position"], 1, "{parts}");
    assert_eq!(parts[0]["project"], "lib");
    assert_eq!(parts[0]["projectId"], json!(lib));
    assert_eq!(parts[1]["position"], 2);
    assert_eq!(parts[1]["project"], "app");
    assert_eq!(
        parts[1]["runId"],
        Json::Null,
        "it waits for the part before it"
    );
    let part_run = parts[0]["runId"].as_str().unwrap().to_owned();

    let part = settled(&steering, &part_run).await;
    assert_eq!(part["state"], "waitingForPerson", "{part}");
    assert_eq!(part["startedByYou"], true);
    assert_eq!(part["jointChange"]["role"], "part");
    assert_eq!(part["jointChange"]["part"]["position"], 1);
    assert_eq!(part["jointChange"]["part"]["of"], 2);
    assert_eq!(part["jointChange"]["project"], "lib");
    assert_eq!(part["jointChange"]["part"]["waitsFor"], Json::Null);
    assert!(
        part["waitingFor"]["youCan"]
            .to_string()
            .contains("send_back")
    );

    let mine = ok(&call(&steering, "list_runs", json!({ "mine": true })).await);
    assert!(mine.to_string().contains(&part_run), "{mine}");

    // Sent back like its other runs, and only that many times.
    let back = ok(&call(
        &steering,
        "send_back",
        json!({ "run": part_run, "instructions": "Name it better" }),
    )
    .await);
    assert_eq!(back["id"], json!(part_run));
    assert!(w.store.attempts_of_run(&part_run).unwrap().len() > 1);

    // A client that wasn't the one who proposed it can't, even one allowed to give feedback.
    let stranger = w.client(&w.steering_token("Someone else")).await;
    let refusal = refused(
        &call(
            &stranger,
            "send_back",
            json!({ "run": part_run, "instructions": "x" }),
        )
        .await,
    );
    assert!(refusal.contains("didn't start"), "{refusal}");
    let view = ok(&call(&stranger, "get_run", json!({ "run": part_run })).await);
    assert_eq!(view["startedByYou"], false);
    for client in [steering, stranger] {
        client.cancel().await.unwrap();
    }
}

fn note_args(card: &str, text: &str) -> Json {
    json!({ "card": card, "text": text })
}

/// The run a card has now, once it has one other than `before`.
async fn next_run_of(w: &World, card: &str, before: Option<&str>) -> String {
    for _ in 0..200 {
        let now = w.store.card(card).unwrap().run_id;
        if let Some(run) = now.filter(|run| Some(run.as_str()) != before) {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("card {card} started no run");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_adds_a_note_and_puts_back_in_todo_only_a_waiting_card_of_its_own() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.use_by_default(&w.project_id, &w.workflow_id);
    let (row, token) = w.server.create_client("Claude Code", true).unwrap();
    w.store.set_mcp_client_can_steer(&row.id, true).unwrap();
    w.store.set_mcp_client_max_open_runs(&row.id, 1).unwrap();
    let client = w.client(&token).await;
    // Not the project's default: running the card again keeps it.
    let workflow = w.saved(
        "Also builds",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Approval { id: "human".into() },
        ],
    );

    let first = id_of(&ok(&call(
        &client,
        "start_run",
        w.args_for(&workflow, "Fix the bug\n\nAnd its test."),
    )
    .await));
    let view = settled(&client, &first).await;
    let card = view["cardId"].as_str().unwrap().to_owned();
    // While its run is open, the card is Keel's, and nothing says otherwise.
    assert_eq!(
        view["waitingFor"]["youCan"],
        json!(["send_back", "close_run"])
    );
    let refusal = refused(
        &call(
            &client,
            "append_to_card",
            note_args(&card, "Also the docs."),
        )
        .await,
    );
    assert!(refusal.contains("underway"), "{refusal}");
    let refusal = refused(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
    assert!(refusal.contains("close_run"), "{refusal}");

    ok(&call(&client, "close_run", json!({ "run": first })).await);
    w.card_in(&first, BoardColumn::Backlog).await;
    let second = id_of(&ok(&call(
        &client,
        "start_run",
        w.start_args("Something else"),
    )
    .await));
    settled(&client, &second).await;

    // Closed, the card waits in the Backlog: a note goes at its end, saying who added it.
    let noted = ok(&call(
        &client,
        "append_to_card",
        note_args(&card, "Also the docs."),
    )
    .await);
    assert_eq!(noted["cardId"], json!(card));
    assert_eq!(noted["column"], "backlog");
    let body = w.store.card(&card).unwrap().body;
    let (person, notes) = split_assistant_notes(&body);
    assert_eq!(person, "Fix the bug\n\nAnd its test.");
    assert_eq!(notes.len(), 1, "{body}");
    assert!(
        notes[0].starts_with("⟦Note from Claude Code, an assistant, on "),
        "{body}"
    );
    assert!(
        notes[0].ends_with("⟧\nAlso the docs.\n⟦End of the note⟧"),
        "{body}"
    );
    assert_eq!(noted["body"], json!(body));
    // It can't go back to Todo while the client has as many runs open as it may.
    assert_eq!(noted["youCan"], json!(["append_to_card"]));
    let refusal = refused(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
    assert!(refusal.contains("runs open"), "{refusal}");

    // Nobody else's to touch, and each needs its own permission.
    let stranger = w.client(&w.steering_token("Someone else")).await;
    let refusal = refused(&call(&stranger, "append_to_card", note_args(&card, "Mine.")).await);
    assert!(refusal.contains("didn't start"), "{refusal}");
    let refusal = refused(&call(&stranger, "move_card_to_todo", json!({ "card": card })).await);
    assert!(refusal.contains("didn't start"), "{refusal}");
    let reader = w.client(&w.token(false)).await;
    let refusal = refused(&call(&reader, "move_card_to_todo", json!({ "card": card })).await);
    assert!(refusal.contains("may only read"), "{refusal}");
    let no_feedback = w.client(&w.token(true)).await;
    let refusal = refused(&call(&no_feedback, "append_to_card", note_args(&card, "Mine.")).await);
    assert!(refusal.contains("may not give feedback"), "{refusal}");
    let refusal = refused(&call(&client, "append_to_card", note_args("nope", "x")).await);
    assert!(refusal.contains("no card"), "{refusal}");
    let refusal = refused(&call(&client, "append_to_card", note_args(&card, "  ")).await);
    assert!(refusal.contains("`text`"), "{refusal}");
    assert_eq!(w.store.card(&card).unwrap().body, body, "nothing changed");

    // With room again, it goes back to Todo and counts as a run the client has open.
    ok(&call(&client, "close_run", json!({ "run": second })).await);
    let moved = ok(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
    assert_eq!(moved["column"], "todo");
    assert_eq!(moved["youCan"], json!(["append_to_card"]));
    assert_eq!(w.store.open_runs_of_mcp_client(&row.id).unwrap(), 1);
    let refusal = refused(&call(&client, "start_run", w.start_args("One more")).await);
    assert!(refusal.contains("runs open"), "{refusal}");
    let refusal = refused(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
    assert!(refusal.contains("already in Todo"), "{refusal}");
    ok(&call(
        &client,
        "append_to_card",
        note_args(&card, "And the changelog."),
    )
    .await);

    // The run it starts is the client's to follow, says who asked for it, runs the same workflow,
    // and keeps the notes apart from what the person wrote.
    w.engine.start_board();
    let again = next_run_of(&w, &card, None).await;
    let view = settled(&client, &again).await;
    assert_eq!(view["startedByYou"], true, "{view}");
    assert_eq!(view["startedBy"], "Claude Code");
    assert_eq!(view["workflowId"], json!(workflow));
    assert_eq!(
        view["waitingFor"]["youCan"],
        json!(["send_back", "close_run"])
    );
    let request = ok(&call(&client, "get_run", json!({ "run": again })).await)["request"].clone();
    let (person, notes) = split_assistant_notes(request.as_str().unwrap());
    assert!(!person.contains("docs"), "{request}");
    assert_eq!(notes.len(), 2, "{request}");
    assert!(notes[1].contains("\nAnd the changelog.\n"), "{request}");
    let mine = ok(&call(&client, "list_runs", json!({ "mine": true })).await);
    assert!(mine.to_string().contains(&again), "{mine}");

    let calls = w.store.mcp_calls(100).unwrap();
    for tool in ["append_to_card", "move_card_to_todo"] {
        assert!(calls.iter().any(|c| c.tool == tool && c.ok), "{tool}");
        assert!(calls.iter().any(|c| c.tool == tool && !c.ok), "{tool}");
    }
    for client in [client, stranger, reader, no_feedback] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_run_of_a_card_a_client_keeps_putting_back_in_todo_is_its_own() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.use_by_default(&w.project_id, &w.workflow_id);
    let client = w.client(&w.token(true)).await;
    let first = id_of(&ok(&call(
        &client,
        "start_run",
        w.start_args("Fix the bug"),
    )
    .await));
    let card = settled(&client, &first).await["cardId"]
        .as_str()
        .unwrap()
        .to_owned();
    w.engine.start_board();
    let mut runs = vec![first];
    // Closed and put back in Todo twice: the card only points at the last run.
    for _ in 0..2 {
        let last = runs.last().unwrap().clone();
        ok(&call(&client, "close_run", json!({ "run": last })).await);
        w.card_in(&last, BoardColumn::Backlog).await;
        ok(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
        let again = next_run_of(&w, &card, Some(&last)).await;
        settled(&client, &again).await;
        runs.push(again);
    }
    let mine = ok(&call(&client, "list_runs", json!({ "mine": true })).await);
    for run in &runs {
        assert!(mine.to_string().contains(run.as_str()), "{run}: {mine}");
        let view = ok(&call(&client, "get_run", json!({ "run": run })).await);
        assert_eq!(view["startedByYou"], true, "{run}: {view}");
    }
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_waiting_its_turn_takes_a_note_and_one_closed_goes_back_to_todo() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.other_project("lib");
    w.use_by_default(&w.project_id, &w.workflow_id);
    w.engine.start_board();
    let client = w.client(&w.steering_token("Claude Code")).await;
    let agreement = id_of(&ok(&call(
        &client,
        "propose_change",
        json!({ "project": "app", "alsoTouches": ["lib"], "request": "Log in through lib" }),
    )
    .await));
    let view = settled(&client, &agreement).await;
    w.engine
        .answer_approval(&waiting_attempt(&view), true, "Go ahead")
        .unwrap();
    let mut parts = json!([]);
    for _ in 0..200 {
        let view = ok(&call(&client, "get_run", json!({ "run": agreement })).await);
        parts = view["jointChange"]["parts"].clone();
        if parts.as_array().is_some_and(|p| p.len() == 2) && parts[0]["runId"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let part_run = parts[0]["runId"].as_str().unwrap().to_owned();
    let first = parts[0]["cardId"].as_str().unwrap().to_owned();
    let second = parts[1]["cardId"].as_str().unwrap().to_owned();

    // The second part waits for its turn: the client that proposed the change may add to it, and
    // the part it follows says so.
    let part = settled(&client, &part_run).await;
    assert_eq!(
        part["waitingFor"]["youCan"],
        json!(["send_back", "close_run", "append_to_card"])
    );
    assert_eq!(part["jointChange"]["parts"][0]["youCan"], json!([]));
    assert_eq!(
        part["jointChange"]["parts"][1]["youCan"],
        json!(["append_to_card"])
    );
    let noted = ok(&call(
        &client,
        "append_to_card",
        note_args(&second, "Send `expiresAt` too."),
    )
    .await);
    assert_eq!(noted["column"], "todo");
    let (agreed, notes) = split_assistant_notes(&w.store.card(&second).unwrap().body);
    assert!(!agreed.contains("expiresAt"), "{agreed}");
    assert!(
        notes[0].starts_with("⟦Note from Claude Code, an assistant, on "),
        "{notes:?}"
    );
    assert!(notes[0].contains("\nSend `expiresAt` too.\n"), "{notes:?}");
    // Not on the part that's underway, and not for another client.
    let refusal = refused(&call(&client, "append_to_card", note_args(&first, "x")).await);
    assert!(refusal.contains("underway"), "{refusal}");
    let stranger = w.client(&w.steering_token("Someone else")).await;
    let refusal = refused(&call(&stranger, "append_to_card", note_args(&second, "x")).await);
    assert!(refusal.contains("didn't start"), "{refusal}");
    let seen = ok(&call(&stranger, "get_run", json!({ "run": part_run })).await);
    assert_eq!(seen["jointChange"]["parts"][1]["youCan"], json!([]));
    assert_eq!(seen["waitingFor"]["youCan"], json!([]));

    // The person holds the second part back in the Backlog: it's theirs from now on.
    w.engine
        .card_move(&second, CardLane::Backlog, 0, "en")
        .unwrap();
    for (tool, args) in [
        ("append_to_card", note_args(&second, "x")),
        ("move_card_to_todo", json!({ "card": second })),
    ] {
        let refusal = refused(&call(&client, tool, args).await);
        assert!(refusal.contains("the person"), "{tool}: {refusal}");
    }
    // Nor does putting it back in Todo give it back: the person put it there.
    w.engine
        .card_move(&second, CardLane::Todo, 0, "en")
        .unwrap();
    let refusal = refused(&call(&client, "append_to_card", note_args(&second, "x")).await);
    assert!(
        refusal.contains("The person put this card in Todo"),
        "{refusal}"
    );
    let part = ok(&call(&client, "get_run", json!({ "run": part_run })).await);
    assert_eq!(part["jointChange"]["parts"][1]["youCan"], json!([]));
    assert_eq!(
        part["waitingFor"]["youCan"],
        json!(["send_back", "close_run"])
    );

    // A part it closes goes back to Todo with a note, and its new run is the client's.
    ok(&call(&client, "close_run", json!({ "run": part_run })).await);
    let mut parts = json!([]);
    for _ in 0..200 {
        let view = ok(&call(&client, "get_run", json!({ "run": agreement })).await);
        parts = view["jointChange"]["parts"].clone();
        if parts[0]["column"] == "backlog" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        parts[0]["youCan"],
        json!(["append_to_card", "move_card_to_todo"]),
        "{parts}"
    );
    ok(&call(
        &client,
        "append_to_card",
        note_args(&first, "Name it `token`."),
    )
    .await);
    ok(&call(&client, "move_card_to_todo", json!({ "card": first })).await);
    let again = next_run_of(&w, &first, Some(&part_run)).await;
    let view = settled(&client, &again).await;
    assert_eq!(view["startedByYou"], true, "{view}");
    assert_eq!(view["startedBy"], "Claude Code");
    // The note stays out of what was agreed for the part, which wins over the request.
    let request = ok(&call(&client, "get_run", json!({ "run": again })).await)["request"].clone();
    let (agreed, notes) = split_assistant_notes(request.as_str().unwrap());
    assert!(agreed.contains("In this project: "), "{request}");
    assert!(!agreed.contains("Name it"), "{request}");
    assert_eq!(notes.len(), 1, "{request}");
    assert!(notes[0].contains("\nName it `token`.\n"), "{request}");
    // The part's first run is still the client's, now that its card points at the next one.
    let first_run = ok(&call(&client, "get_run", json!({ "run": part_run })).await);
    assert_eq!(first_run["startedByYou"], true, "{first_run}");
    let mine = ok(&call(&client, "list_runs", json!({ "mine": true })).await);
    for run in [&part_run, &again] {
        assert!(mine.to_string().contains(run.as_str()), "{run}: {mine}");
    }
    for client in [client, stranger] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_the_person_put_back_in_todo_runs_as_theirs() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.other_project("lib");
    w.use_by_default(&w.project_id, &w.workflow_id);
    let client = w.client(&w.steering_token("Claude Code")).await;
    let agreement = id_of(&ok(&call(
        &client,
        "propose_change",
        json!({ "project": "app", "alsoTouches": ["lib"], "request": "Log in through lib" }),
    )
    .await));
    let view = settled(&client, &agreement).await;
    w.engine
        .answer_approval(&waiting_attempt(&view), true, "Go ahead")
        .unwrap();
    let mut first = None;
    for _ in 0..200 {
        let view = ok(&call(&client, "get_run", json!({ "run": agreement })).await);
        first = view["jointChange"]["parts"][0]["cardId"]
            .as_str()
            .map(str::to_owned);
        if first.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = first.expect("the parts never opened");

    // Before it starts, the person holds the first part back and then puts it in Todo themselves.
    w.engine
        .card_move(&first, CardLane::Backlog, 0, "en")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    w.engine.card_move(&first, CardLane::Todo, 0, "en").unwrap();
    w.engine.start_board();
    let theirs = next_run_of(&w, &first, None).await;
    let view = settled(&client, &theirs).await;
    assert_eq!(view["startedByYou"], false, "{view}");
    // Only the next part, still as the change opened it, takes the client's note.
    assert_eq!(
        view["waitingFor"]["youCan"],
        json!(["append_to_card"]),
        "{view}"
    );
    let mine = ok(&call(&client, "list_runs", json!({ "mine": true })).await);
    assert!(!mine.to_string().contains(theirs.as_str()), "{mine}");

    // So the client can't close it, add to it, or put it back in Todo in its place.
    let refusal = refused(&call(&client, "close_run", json!({ "run": theirs })).await);
    assert!(refusal.contains("didn't start"), "{refusal}");
    let refusal = refused(&call(&client, "append_to_card", note_args(&first, "x")).await);
    assert!(refusal.contains("underway"), "{refusal}");
    w.engine.abandon(&waiting_attempt(&view)).unwrap();
    // It's in lib, not on app's board.
    for _ in 0..100 {
        if w.store.card(&first).unwrap().column() == BoardColumn::Backlog {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(w.store.card(&first).unwrap().column(), BoardColumn::Backlog);
    for (tool, args) in [
        ("append_to_card", note_args(&first, "x")),
        ("move_card_to_todo", json!({ "card": first })),
    ] {
        let refusal = refused(&call(&client, tool, args).await);
        assert!(refusal.contains("The person ended"), "{tool}: {refusal}");
    }
    assert_eq!(w.store.card(&first).unwrap().run_id, Some(theirs));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_cannot_add_to_or_run_again_what_the_person_ended() {
    keel_harness::needs!(sandbox, sockets);
    let w = world().await;
    w.other_project("lib");
    w.use_by_default(&w.project_id, &w.workflow_id);
    let client = w.client(&w.steering_token("Claude Code")).await;
    let started = |request: &'static str| {
        let args = w.start_args(request);
        let client = &client;
        async move {
            let run = id_of(&ok(&call(client, "start_run", args).await));
            let approval = waiting_attempt(&settled(client, &run).await);
            (run, approval)
        }
    };
    let refuses = async |run: &str, says: &str| {
        let card = w.store.card_of_run(run).unwrap().unwrap();
        let note = refused(&call(&client, "append_to_card", note_args(&card, "x")).await);
        assert!(note.contains(says), "{note}");
        let moved = refused(&call(&client, "move_card_to_todo", json!({ "card": card })).await);
        assert!(moved.contains(says), "{moved}");
    };

    // The person rejected it.
    let (rejected, approval) = started("Rejected").await;
    w.engine
        .answer_approval(&approval, false, "Not like this")
        .unwrap();
    w.card_in(&rejected, BoardColumn::Backlog).await;
    refuses(&rejected, "rejected").await;

    // The client closed it, then the person tried it again and closed it themselves.
    let (closed, approval) = started("Closed").await;
    ok(&call(&client, "close_run", json!({ "run": closed })).await);
    w.engine.retry(&approval, false).unwrap();
    let approval = waiting_attempt(&settled(&client, &closed).await);
    w.engine.abandon(&approval).unwrap();
    w.card_in(&closed, BoardColumn::Backlog).await;
    refuses(&closed, "The person ended").await;

    // The person tried it again after the client closed it, and the client closed it once more:
    // trying it again was the person's say, so it stays theirs.
    let (reopened, approval) = started("Reopened").await;
    ok(&call(&client, "close_run", json!({ "run": reopened })).await);
    w.engine.retry(&approval, false).unwrap();
    settled(&client, &reopened).await;
    ok(&call(&client, "close_run", json!({ "run": reopened })).await);
    w.card_in(&reopened, BoardColumn::Backlog).await;
    refuses(&reopened, "tried it again").await;

    // The person stopped it, though it was the client that closed it after.
    let hold = w.hold();
    let stopped = id_of(&ok(
        &call(&client, "start_run", w.start_args("Stopped")).await
    ));
    let builder = loop {
        let attempts = w.store.attempts_of_run(&stopped).unwrap();
        if let Some(running) = attempts.iter().find(|a| a.state == "running") {
            break running.id.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    w.engine.stop(&builder).unwrap();
    std::fs::remove_file(&hold).unwrap();
    settled(&client, &stopped).await;
    ok(&call(&client, "close_run", json!({ "run": stopped })).await);
    w.card_in(&stopped, BoardColumn::Backlog).await;
    refuses(&stopped, "The person ended").await;

    // Where a change across projects is agreed starts with propose_change, even closed by the client.
    let agreement = id_of(&ok(&call(
        &client,
        "propose_change",
        json!({ "project": "app", "alsoTouches": ["lib"], "request": "Log in through lib" }),
    )
    .await));
    settled(&client, &agreement).await;
    ok(&call(&client, "close_run", json!({ "run": agreement })).await);
    w.card_in(&agreement, BoardColumn::Backlog).await;
    refuses(&agreement, "propose_change").await;

    // One it stopped and closed itself is its own to run again.
    let hold = w.hold();
    let own = id_of(&ok(
        &call(&client, "start_run", w.start_args("Its own")).await
    ));
    stop_once_running(&client, &own).await;
    std::fs::remove_file(&hold).unwrap();
    ok(&call(&client, "close_run", json!({ "run": own })).await);
    let card = w.card_in(&own, BoardColumn::Backlog).await;
    ok(&call(&client, "append_to_card", note_args(&card.id, "Try again.")).await);
    let moved = ok(&call(&client, "move_card_to_todo", json!({ "card": card.id })).await);
    assert_eq!(moved["column"], "todo");
    client.cancel().await.unwrap();
}
