//! The server over HTTP, end to end: rmcp's own client and plain HTTP against a server on a free
//! port, with the real engine and a fake `claude`.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{AttemptState, BoardColumn, CardLane, MASK, StepSpec, ToolProfile, WorkflowStep};
use keel_engine::{Config, Engine, Workflow};
use keel_harness::Tools;
use keel_mcp::{MAX_OPEN_RUNS, MCP_PATH, McpServer, StartError};
use keel_store::Store;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value as Json, json};

/// Everything a client can do. A new tool has to be added here on purpose: nothing that answers an
/// approval, merges, changes settings or workflows, or lets anything into the Brain belongs here.
const TOOLS: [&str; 10] = [
    "get_brain",
    "get_run",
    "get_step",
    "list_issues",
    "list_projects",
    "list_runs",
    "list_workflows",
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
tools=""; prev=""
for a in "$@"; do [ "$prev" = "--tools" ] && tools="$a"; prev="$a"; done
list=$(printf '%s' "$tools" | sed 's/,/","/g')
echo "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fake\",\"tools\":[\"$list\"],\"apiKeySource\":\"none\"}"
while [ -f "$(dirname "$0")/hold" ]; do sleep 0.02; done
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
        "approve",
        "reject",
        "merge",
        "update_project",
        "save_workflow",
        "accept_note",
    ] {
        let params = CallToolRequestParams::new(tool.to_owned());
        assert!(client.call_tool(params).await.is_err(), "{tool}");
    }
    client.cancel().await.unwrap();
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
