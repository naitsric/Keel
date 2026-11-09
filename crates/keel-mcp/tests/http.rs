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
use keel_engine::{AddOutcome, Config, Engine, MAX_PROJECT_PROPOSALS, Workflow};
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
/// propose_project because what it leaves isn't a project until the person adds it.
const TOOLS: [&str; 12] = [
    "draft_workflow",
    "get_brain",
    "get_run",
    "get_step",
    "list_issues",
    "list_projects",
    "list_runs",
    "list_workflows",
    "propose_project",
    "start_run",
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
  echo '{"type":"result","subtype":"success","is_error":false,"result":"","num_turns":1,"structured_output":{"summary":"It gets fixed","criteria":[{"id":"C1","when":"it runs","then":"it works"}],"changes":[],"limits":[],"decided":[],"questions":[{"id":"Q1","text":"What then?","options":[{"label":"Yes","recommended":true},{"label":"No","recommended":false}]}]}}'
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
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("can't approve")
    );

    for tool in [
        "answer_approval",
        "answer_questions",
        "send_back",
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
    assert_eq!(view["steps"][0]["result"], "Fixed it.");

    let attempt = view["steps"][0]["attemptId"].as_str().unwrap();
    let step = ok(&call(&client, "get_step", json!({ "attempt": attempt })).await);
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
