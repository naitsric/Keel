//! The server over HTTP, end to end: rmcp's own client and plain HTTP against a server on a free
//! port, with the real engine and a fake `claude`.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{
    AttemptState, BoardColumn, CardLane, MASK, StepRole, StepSpec, ToolProfile, WorkflowStep,
};
use keel_engine::{
    AddOutcome, Answer, AnswerChoice, Config, Engine, MAX_PROJECT_PROPOSALS, StartRun, Workflow,
};
use keel_harness::Tools;
use keel_mcp::{MAX_DRAFTS, MAX_OPEN_RUNS, MCP_PATH, McpServer, StartError};
use keel_store::Store;
use keel_store::rusqlite_value::Value;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value as Json, json};

/// Everything a client can do. A new tool has to be added here on purpose: nothing that answers an
/// approval, merges, changes settings or saved workflows, or lets anything into the Brain belongs
/// here. draft_workflow is here because what it leaves can't run until the person saves it, and
/// propose_project because what it leaves isn't a project until the person adds it. send_back is
/// here because the run comes back to the same approval, so the person still decides, and
/// retry_step because it only retries what failed or was stopped, never what waits on the person's
/// judgment. close_run is here because it delivers nothing, and the card keeps its text for the
/// person. All three are only for a client the person lets give feedback, on runs it started, and
/// the first two only a few times per run. start_task is here because its one agent only reads, and
/// has nothing to deliver.
const TOOLS: [&str; 16] = [
    "close_run",
    "draft_workflow",
    "get_brain",
    "get_run",
    "get_step",
    "list_issues",
    "list_projects",
    "list_runs",
    "list_workflows",
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
  *verdict*) out='{"verdict":"changes","findings":["Line 3 divides by zero"],"summary":"One problem"}';;
  *decision*) out='{"decision":"approve","risk":"low","reasons":"It does what was asked","instructions":"Only a note"}';;
  *unverified*) out='{"criteria":[{"id":"C1","result":"met","evidence":""}],"summary":"It works"}';;
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
    std::fs::write(&claude, fake_claude()).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

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
    let w = world().await;
    let client = w.client(&w.token(true)).await;
    let started = ok(&call(&client, "start_run", w.start_args("Fix the bug")).await);
    let run = started["id"].as_str().unwrap().to_owned();
    assert_eq!(started["workflowId"], json!(w.workflow_id));
    assert_eq!(started["workflowName"], "Build and ask");
    assert_eq!(started["prState"], Json::Null, "no pull request yet");

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
    ] {
        assert!(schema.contains(word), "{word}: {schema}");
    }
    assert!(!schema.contains("setup"), "{schema}");
    assert!(!schema.contains("mapper"), "{schema}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_draft_can_ask_for_a_step_to_depend_on_the_change_and_it_is_kept_and_shown() {
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
async fn a_draft_waits_for_the_person_and_nothing_can_run_it_until_they_save_it() {
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

#[tokio::test]
async fn it_listens_on_this_mac_only() {
    let w = world().await;
    assert!(w.addr.ip().is_loopback());
    let status = w.server.status();
    assert!(status.running);
    assert_eq!(status.port, Some(w.addr.port()));
}

#[tokio::test]
async fn a_request_needs_a_token_and_a_revoked_one_stops_at_once() {
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
    let w = world().await;
    let token = w.token(true);
    let from_a_page = w.post(Some(&token)).header(ORIGIN, "https://evil.example");
    assert_eq!(from_a_page.send().await.unwrap().status(), 403);
    let rebound = w.post(Some(&token)).header(HOST, "evil.example");
    assert_eq!(rebound.send().await.unwrap().status(), 403);
}

#[tokio::test]
async fn a_removed_project_is_not_there_for_a_client() {
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
    assert_eq!(step["filesChanged"], json!(["fixed.txt"]), "{step}");
    assert!(step["prompt"].as_str().unwrap().contains("Fix the bug"));
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
    assert!(step["prompt"].as_str().unwrap().contains(MASK), "{step}");

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
            }],
            "reviews": [
                { "step": "security", "verdict": "changes", "findings": ["Line 3 divides by zero"] },
                // What an approver that approved only noted isn't something to do.
                { "step": "approver", "decision": "approve", "risk": "low", "reasons": "It does what was asked", "instructions": null },
            ],
            // A run an assistant started never counts as small.
            "skipped": [],
            "prUrl": null,
            "prState": null,
        })
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_names_what_was_skipped_for_being_small() {
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
    let w = world().await;
    let workflow = w.saved(
        "Checked",
        vec![
            WorkflowStep::Agent(agent("builder", ToolProfile::Build, StepRole::Worker)),
            WorkflowStep::Check {
                id: "tests".into(),
                run: "echo '3 failed' >&2; exit 1".into(),
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
    assert_eq!(seen["waitingFor"]["youCan"], json!([]));
    for client in [owner, other, reader] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_the_person_started_is_not_a_client_s_to_act_on() {
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

const FEEDBACK: [&str; 3] = ["send_back", "retry_step", "close_run"];

fn feedback_args(tool: &str, run: &str) -> Json {
    match tool {
        "send_back" => json!({ "run": run, "instructions": "Return 0 instead" }),
        _ => json!({ "run": run }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_needs_the_person_s_permission_and_a_run_of_the_client_s_own() {
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
    // wait_for_run returns it the same way.
    let waited = ok(&call(&client, "wait_for_run", json!({ "run": run, "seconds": 1 })).await);
    assert_eq!(waited, view);

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
    assert_eq!(built["filesChanged"].as_array().unwrap().len(), 150);
    client.cancel().await.unwrap();
}

async fn view_of(client: &Client, run: &str) -> Json {
    ok(&call(client, "get_run", json!({ "run": run })).await)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_retries_only_a_step_that_failed_was_interrupted_or_stopped() {
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
async fn a_client_closes_its_waiting_run_with_nothing_delivered_and_never_one_that_works() {
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
async fn a_task_is_one_agent_that_only_reads_and_its_finding_is_the_report_s_answer() {
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
        "title": "How the login works",
        "effort": "low",
    });
    let started = ok(&call(&client, "start_task", args).await);
    let run = id_of(&started);
    assert_eq!(started["startedByYou"], true);
    assert_eq!(started["startedBy"], "Claude Code");
    assert_eq!(started["title"], "How the login works");
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
