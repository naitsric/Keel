//! The desktop shell. Commands are thin: the rules live in `keel-engine`.

mod menu;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use keel_core::{Reason, StepSpec, ToolProfile};
use keel_engine::{Config, Engine, Error as EngineError, Preflight, RawCheck, StartRun};
use keel_harness::Tools;
use keel_store::{Attempt, Event, Project, RunRow, Store, Totals};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, RunEvent, State, WindowEvent};

const LANGUAGE_KEY: &str = "language";
const CHANGED_EVENT: &str = "keel://changed";
const QUIT_EVENT: &str = "keel://quit-requested";

/// Finding the tools can take seconds, so it happens off the main thread and commands that
/// need them wait here.
#[derive(Default)]
struct Ready {
    done: Mutex<bool>,
    signal: Condvar,
}

impl Ready {
    fn set(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signal.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.signal.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct AppState {
    store: Arc<Store>,
    engine: Engine,
    ready: Arc<Ready>,
    tools: Arc<Mutex<Tools>>,
    data_dir: PathBuf,
    quit_allowed: AtomicBool,
}

/// Every command's error: a code the UI translates, with the reason when there is one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandError {
    code: &'static str,
    reason: Option<Reason>,
    detail: Option<String>,
}

impl From<EngineError> for CommandError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Blocked(reason) => Self {
                code: "blocked",
                reason: Some(reason),
                detail: None,
            },
            EngineError::NotARepo(path) => Self {
                code: "notARepo",
                reason: None,
                detail: Some(path),
            },
            EngineError::InvalidState => Self {
                code: "invalidState",
                reason: None,
                detail: None,
            },
            other => Self {
                code: "internal",
                reason: None,
                detail: Some(other.to_string()),
            },
        }
    }
}

impl From<keel_store::Error> for CommandError {
    fn from(error: keel_store::Error) -> Self {
        Self {
            code: "internal",
            reason: None,
            detail: Some(error.to_string()),
        }
    }
}

type CommandResult<T> = Result<T, CommandError>;

#[tauri::command(async)]
fn preflight(state: State<'_, AppState>) -> Preflight {
    state.ready.wait();
    state.engine.preflight()
}

/// Looks for the tools again, for after the user installs something or signs in.
#[tauri::command(async)]
fn refresh_session(state: State<'_, AppState>) -> Preflight {
    state.ready.wait();
    let tools = Tools::detect();
    *state.tools.lock().unwrap_or_else(|e| e.into_inner()) = tools.clone();
    state.engine.set_tools(tools);
    state.engine.preflight()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Overview {
    runs: Vec<RunRow>,
    totals: Totals,
    projects: Vec<Project>,
}

#[tauri::command(async)]
fn overview(state: State<'_, AppState>) -> CommandResult<Overview> {
    Ok(Overview {
        runs: state.store.runs(200)?,
        totals: state.store.totals()?,
        projects: state.store.projects()?,
    })
}

#[tauri::command(async)]
fn project_add(state: State<'_, AppState>, path: String) -> CommandResult<Project> {
    state.ready.wait();
    Ok(state.engine.add_project(Path::new(&path))?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    project_id: String,
    step_id: String,
    prompt: String,
    model: String,
    effort: Option<String>,
    tools: ToolProfile,
    max_turns: Option<u32>,
    language: String,
}

#[tauri::command(async)]
fn run_start(state: State<'_, AppState>, request: RunRequest) -> CommandResult<String> {
    state.ready.wait();
    Ok(state.engine.start_run(StartRun {
        project_id: request.project_id,
        step: StepSpec {
            id: request.step_id,
            model: request.model,
            effort: request.effort,
            tools: request.tools,
            prompt: request.prompt,
            max_turns: request.max_turns,
            max_budget_usd: None,
        },
        language: request.language,
        allowed_domains: vec![],
    })?)
}

#[tauri::command(async)]
fn run_attempts(state: State<'_, AppState>, run_id: String) -> CommandResult<Vec<Attempt>> {
    Ok(state.store.attempts_of_run(&run_id)?)
}

#[tauri::command(async)]
fn attempt_events(
    state: State<'_, AppState>,
    attempt_id: String,
    after_seq: i64,
) -> CommandResult<Vec<Event>> {
    Ok(state.store.events(&attempt_id, after_seq)?)
}

#[tauri::command(async)]
fn attempt_diff(state: State<'_, AppState>, attempt_id: String) -> CommandResult<String> {
    Ok(state.engine.diff(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_raw_check(state: State<'_, AppState>, attempt_id: String) -> CommandResult<RawCheck> {
    Ok(state.engine.verify_raw(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_stop(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.stop(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_wait(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.wait_longer(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_kill_command(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.kill_command(&attempt_id)?)
}

#[tauri::command(async)]
fn attempt_retry(
    state: State<'_, AppState>,
    attempt_id: String,
    resume: bool,
) -> CommandResult<String> {
    Ok(state.engine.retry(&attempt_id, resume)?)
}

#[tauri::command(async)]
fn attempt_abandon(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    Ok(state.engine.abandon(&attempt_id)?)
}

#[tauri::command(async)]
fn language_get(state: State<'_, AppState>) -> CommandResult<Option<String>> {
    Ok(state.store.setting(LANGUAGE_KEY)?)
}

#[tauri::command(async)]
fn language_set(state: State<'_, AppState>, language: String) -> CommandResult<()> {
    Ok(state.store.set_setting(LANGUAGE_KEY, &language)?)
}

/// Opens Terminal running `claude auth login`. The login happens in the user's own CLI: Keel
/// never sees it. A `.command` file avoids asking for permission to control Terminal.
#[tauri::command(async)]
fn open_claude_login(state: State<'_, AppState>) -> CommandResult<()> {
    let claude = state
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .claude
        .clone();
    let Some(claude) = claude else {
        return Err(CommandError {
            code: "blocked",
            reason: Some(Reason::ClaudeMissing),
            detail: None,
        });
    };
    let script = state.data_dir.join("claude-login.command");
    let quoted = claude.to_string_lossy().replace('\'', r"'\''");
    let write = std::fs::write(&script, format!("#!/bin/sh\nexec '{quoted}' auth login\n"))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        });
    let opened = write.and_then(|()| {
        std::process::Command::new("/usr/bin/open")
            .arg(&script)
            .status()
    });
    opened.map(|_| ()).map_err(|e| CommandError {
        code: "internal",
        reason: None,
        detail: Some(e.to_string()),
    })
}

/// Shows a file of this attempt in Finder. Only paths the store recorded, never one from the UI.
#[tauri::command(async)]
fn reveal_raw(state: State<'_, AppState>, attempt_id: String) -> CommandResult<()> {
    let attempt = state.store.attempt(&attempt_id)?;
    std::process::Command::new("/usr/bin/open")
        .args(["-R", &attempt.raw_path])
        .status()
        .map(|_| ())
        .map_err(|e| CommandError {
            code: "internal",
            reason: None,
            detail: Some(e.to_string()),
        })
}

#[tauri::command(async)]
fn active_count(state: State<'_, AppState>) -> CommandResult<usize> {
    Ok(state.engine.active_count()?)
}

/// Ends the app. Agents keep running unless `stop_runs` is set; Keel reconnects on reopening.
#[tauri::command(async)]
fn app_quit(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    stop_runs: bool,
) -> CommandResult<()> {
    if stop_runs {
        for attempt in state.store.active_attempts()? {
            let _ = state.engine.stop(&attempt.id);
        }
        // Gives the agents a moment to record how they ended.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        while state.engine.active_count()? > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    state.quit_allowed.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

fn wants_confirmation(app: &tauri::AppHandle) -> bool {
    let state = app.state::<AppState>();
    !state.quit_allowed.load(Ordering::SeqCst) && state.engine.active_count().unwrap_or(0) > 0
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .on_menu_event(menu::on_menu_event)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && wants_confirmation(window.app_handle())
            {
                api.prevent_close();
                let _ = window.emit(QUIT_EVENT, ());
            }
        })
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Arc::new(Store::open(&data_dir.join("keel.db"))?);
            let handle = app.handle().clone();
            let engine = Engine::new(
                store.clone(),
                Config::new(data_dir.clone()),
                Tools::default(),
                move |run_id| {
                    let _ = handle.emit(CHANGED_EVENT, run_id);
                },
            );
            let ready = Arc::new(Ready::default());
            let tools_shared = Arc::new(Mutex::new(Tools::default()));
            {
                let engine = engine.clone();
                let ready = ready.clone();
                let tools_shared = tools_shared.clone();
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let tools = Tools::detect();
                    *tools_shared.lock().unwrap_or_else(|e| e.into_inner()) = tools.clone();
                    engine.set_tools(tools);
                    // Reconnects to steps that outlived the last session.
                    let _ = engine.recover();
                    ready.set();
                    let _ = handle.emit(CHANGED_EVENT, "");
                });
            }
            app.manage(AppState {
                store,
                engine,
                ready,
                tools: tools_shared,
                data_dir,
                quit_allowed: AtomicBool::new(false),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            preflight,
            refresh_session,
            overview,
            project_add,
            run_start,
            run_attempts,
            attempt_events,
            attempt_diff,
            attempt_raw_check,
            attempt_stop,
            attempt_wait,
            attempt_kill_command,
            attempt_retry,
            attempt_abandon,
            language_get,
            language_set,
            open_claude_login,
            reveal_raw,
            active_count,
            app_quit,
            menu::menu_set,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { api, .. } = event
            && wants_confirmation(handle)
        {
            api.prevent_exit();
            let _ = handle.emit(QUIT_EVENT, ());
        }
    });
}
