//! The MCP server in the app: starting it with the app when the person left it on, the settings, the
//! clients, and connecting Claude Code with one click. The tools themselves are in `keel-mcp`.

use std::process::Command;

use keel_core::{Log, Masked};
use keel_mcp::{McpServer, McpSettings, ServerStatus};
use keel_store::{McpCall, McpClient, McpProjectChange};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::{AppState, CommandError, CommandResult, launch};

/// What Claude Code lists the server as, and the name of the client that connecting creates.
const SERVER_NAME: &str = launch::mcp_server_name(launch::BRANCH_BUILD);
const CLAUDE_CLIENT: &str = "Claude Code";
const CALLS_SHOWN: i64 = 30;
/// Apart from the calls, so that later ones don't push a change to a project out of sight.
const PROJECT_CHANGES_SHOWN: i64 = 100;
/// For the status bar. A server that starts or stops says nothing else that the UI would hear.
const MCP_EVENT: &str = "keel://mcp-changed";

pub struct McpState(pub McpServer);

/// Starts the server if the person left it on. A failure is in its status, for the UI to show.
pub fn start_if_enabled(server: McpServer, app: AppHandle, log: &Log) {
    match server.settings() {
        Ok(settings) if settings.enabled => {
            tauri::async_runtime::spawn(async move {
                let _ = server.start(settings.port).await;
                let _ = app.emit(MCP_EVENT, ());
            });
        }
        Ok(_) => {}
        Err(e) => log.write(&format!("reading whether the MCP server is on: {e}")),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOverview {
    settings: McpSettings,
    status: ServerStatus,
    clients: Vec<McpClient>,
    /// A call's arguments and error can hold whatever a client sent or a step wrote.
    calls: Masked<Vec<McpCall>>,
    /// Not masked, as a project's commands aren't in its settings: the person has to see all of
    /// what a client set to run on their Mac.
    project_changes: Vec<McpProjectChange>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewClient {
    client: McpClient,
    /// Shown once. Only its hash is kept.
    token: String,
}

fn overview(state: &AppState, mcp: &McpState) -> CommandResult<McpOverview> {
    Ok(McpOverview {
        settings: mcp.0.settings()?,
        status: mcp.0.status(),
        clients: state.store.mcp_clients()?,
        calls: Masked(state.store.mcp_calls(CALLS_SHOWN)?),
        project_changes: state.store.mcp_project_changes(PROJECT_CHANGES_SHOWN)?,
    })
}

fn failed(code: &'static str, detail: Option<String>) -> CommandError {
    CommandError {
        code,
        reason: None,
        detail,
    }
}

#[tauri::command(async)]
pub fn mcp_overview(
    state: State<'_, AppState>,
    mcp: State<'_, McpState>,
) -> CommandResult<McpOverview> {
    overview(&state, &mcp)
}

/// Turned on, it listens on the saved port, restarting if it's on another. Turned off, it stops.
/// Runs it started keep going: they belong to Keel.
#[tauri::command]
pub async fn mcp_settings_save(
    settings: McpSettings,
    app: AppHandle,
    state: State<'_, AppState>,
    mcp: State<'_, McpState>,
) -> CommandResult<McpOverview> {
    mcp.0.save_settings(&settings)?;
    let status = mcp.0.status();
    if !settings.enabled {
        mcp.0.stop().await;
    } else if !status.running || status.port != Some(settings.port) {
        // A failure is in the status, which the overview carries.
        let _ = mcp.0.start(settings.port).await;
    }
    let _ = app.emit(MCP_EVENT, ());
    overview(&state, &mcp)
}

#[tauri::command(async)]
pub fn mcp_client_create(
    name: String,
    can_run: bool,
    mcp: State<'_, McpState>,
) -> CommandResult<NewClient> {
    if name.trim().is_empty() {
        return Err(failed("invalidState", None));
    }
    let (client, token) = mcp.0.create_client(&name, can_run)?;
    Ok(NewClient { client, token })
}

#[tauri::command(async)]
pub fn mcp_client_set_can_run(
    id: String,
    can_run: bool,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state.engine.set_mcp_client_can_run(&id, can_run)?;
    // Feedback only works with this on. It goes off with it, so it can't come back on unasked.
    if !can_run {
        state.store.set_mcp_client_can_steer(&id, false)?;
    }
    Ok(())
}

/// Lets it send work back and retry on the runs it started. It also needs `can_run`.
#[tauri::command(async)]
pub fn mcp_client_set_can_steer(
    id: String,
    can_steer: bool,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state.store.set_mcp_client_can_steer(&id, can_steer)?;
    Ok(())
}

/// Lets it add projects and change their GitHub account, commands and allowed domains. It also
/// needs `can_run`, without which the store keeps it off.
#[tauri::command(async)]
pub fn mcp_client_set_can_manage_projects(
    id: String,
    can_manage_projects: bool,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state
        .store
        .set_mcp_client_can_manage_projects(&id, can_manage_projects)?;
    Ok(())
}

/// How many of its runs can be open at once. The store keeps it within bounds.
#[tauri::command(async)]
pub fn mcp_client_set_max_open_runs(
    id: String,
    max: i64,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state.store.set_mcp_client_max_open_runs(&id, max)?;
    Ok(())
}

#[tauri::command(async)]
pub fn mcp_client_revoke(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let client = state.engine.revoke_mcp_client(&id)?;
    // So Claude Code stops trying a token that no longer works.
    if client.name == CLAUDE_CLIENT {
        let _ = claude_mcp(&state, &["remove", SERVER_NAME, "--scope", "user"]);
    }
    Ok(())
}

/// Creates a client for Claude Code and registers it there, for this user, without the person
/// pasting anything. It replaces the one from before, whose token nobody has anymore.
#[tauri::command(async)]
pub fn mcp_connect_claude(
    can_run: bool,
    state: State<'_, AppState>,
    mcp: State<'_, McpState>,
) -> CommandResult<McpOverview> {
    state.ready.wait();
    let Some(url) = mcp.0.status().url else {
        return Err(failed("mcpNotRunning", None));
    };
    let (client, token) = mcp.0.create_client(CLAUDE_CLIENT, can_run)?;
    let _ = claude_mcp(&state, &["remove", SERVER_NAME, "--scope", "user"]);
    let header = format!("Authorization: Bearer {token}");
    let added = claude_mcp(
        &state,
        &[
            "add",
            "--scope",
            "user",
            "--transport",
            "http",
            SERVER_NAME,
            &url,
            "--header",
            &header,
        ],
    );
    if let Err(detail) = added {
        state.engine.revoke_mcp_client(&client.id)?;
        return Err(failed("mcpConnectFailed", Some(detail)));
    }
    for previous in state.store.mcp_clients()? {
        if previous.name == CLAUDE_CLIENT
            && previous.id != client.id
            && previous.revoked_at.is_none()
        {
            state.engine.revoke_mcp_client(&previous.id)?;
        }
    }
    overview(&state, &mcp)
}

/// `claude mcp <args>`, with what it says when it fails. It never echoes the token back: `args` is
/// what it was given, and only its own output comes back here.
fn claude_mcp(state: &AppState, args: &[&str]) -> Result<(), String> {
    let tools = state
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let claude = tools
        .claude
        .ok_or_else(|| "Claude Code wasn't found".to_owned())?;
    let output = Command::new(claude)
        .arg("mcp")
        .args(args)
        .env("PATH", &tools.path_env)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}
