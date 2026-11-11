//! A run keeps a copy of the project's setup and check from when it started. When the person
//! fixes one of them, a retry can take the new one.

use keel_core::WorkflowStep;
use serde::Serialize;
use serde_json::Value;

/// Which of the project's commands a step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandKind {
    Setup,
    Check,
}

/// A run's step with one of the project's commands, as the run has it and as the project has it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandChange {
    pub step_id: String,
    pub kind: CommandKind,
    pub before: String,
    pub now: String,
}

/// On a step in the run's copy whose command Keel took from the project. Whatever reads the steps
/// leaves it aside.
const MARK: &str = "fromProject";
/// The ids Keel gives the project's setup when it puts it first in a run. See `start_run`.
const SETUP_IDS: [&str; 2] = ["setup", "project-setup"];
/// The id of the project's check in the built-in workflows. See `prompts::full_workflow`.
const CHECK_ID: &str = "check";

/// Whether the steps came from one of the workflows Keel ships, which take the project's check.
fn is_builtin(workflow_id: Option<&str>) -> bool {
    workflow_id.is_some_and(|id| id.starts_with("builtin:"))
}

fn kind_of(step: &Value) -> Option<CommandKind> {
    match step.get("type")?.as_str()? {
        "setup" => Some(CommandKind::Setup),
        "check" => Some(CommandKind::Check),
        _ => None,
    }
}

/// The run's copy of its steps, with a mark on each one whose command Keel took from the project:
/// the setup it put first, if it did, and a built-in workflow's check. `workflow_id` is the saved
/// workflow the steps came from, if any.
pub fn marked(steps: &[WorkflowStep], setup: Option<&str>, workflow_id: Option<&str>) -> Value {
    let mut workflow = serde_json::to_value(steps).unwrap_or_else(|_| Value::Array(Vec::new()));
    let builtin = is_builtin(workflow_id);
    for step in workflow.as_array_mut().into_iter().flatten() {
        let id = step.get("id").and_then(Value::as_str);
        let from_project = match kind_of(step) {
            Some(CommandKind::Setup) => setup.is_some() && id == setup,
            Some(CommandKind::Check) => builtin && id == Some(CHECK_ID),
            None => false,
        };
        if from_project {
            step[MARK] = Value::Bool(true);
        }
    }
    workflow
}

/// The run's setup and check whose command the project has changed since the run started. A run
/// from before Keel marked them goes by their names, and only if it used a built-in workflow: a
/// saved one's steps are its own. A command the project no longer has changes nothing, since a
/// step can't run nothing.
pub fn changed(
    workflow: &Value,
    workflow_id: Option<&str>,
    setup: Option<&str>,
    check: Option<&str>,
) -> Vec<CommandChange> {
    fn current(command: Option<&str>) -> Option<&str> {
        command.map(str::trim).filter(|c| !c.is_empty())
    }
    let steps = workflow.as_array().map(Vec::as_slice).unwrap_or_default();
    let any_marked = steps.iter().any(|step| step.get(MARK).is_some());
    let builtin = is_builtin(workflow_id);
    steps
        .iter()
        .filter_map(|step| {
            let kind = kind_of(step)?;
            let id = step.get("id")?.as_str()?;
            let before = step.get("run")?.as_str()?;
            let from_project = if any_marked {
                step.get(MARK) == Some(&Value::Bool(true))
            } else {
                builtin
                    && match kind {
                        CommandKind::Setup => SETUP_IDS.contains(&id),
                        CommandKind::Check => id == CHECK_ID,
                    }
            };
            let now = match kind {
                CommandKind::Setup => current(setup),
                CommandKind::Check => current(check),
            }?;
            (from_project && before.trim() != now).then(|| CommandChange {
                step_id: id.to_owned(),
                kind,
                before: before.to_owned(),
                now: now.to_owned(),
            })
        })
        .collect()
}

/// The run's copy with those commands in place of the ones they had, and its marks as they were.
pub fn with_changes(workflow: &Value, changes: &[CommandChange]) -> Value {
    let mut workflow = workflow.clone();
    for step in workflow.as_array_mut().into_iter().flatten() {
        let Some(kind) = kind_of(step) else {
            continue;
        };
        let id = step.get("id").and_then(Value::as_str);
        if let Some(change) = changes
            .iter()
            .find(|change| change.kind == kind && id == Some(change.step_id.as_str()))
        {
            step["run"] = Value::String(change.now.clone());
        }
    }
    workflow
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::AttemptState;
    use serde_json::json;

    fn setup(id: &str, run: &str) -> WorkflowStep {
        WorkflowStep::Setup {
            id: id.into(),
            run: run.into(),
        }
    }

    fn check(id: &str, run: &str) -> WorkflowStep {
        WorkflowStep::Check {
            id: id.into(),
            run: run.into(),
        }
    }

    fn change(step: &str, kind: CommandKind, before: &str, now: &str) -> CommandChange {
        CommandChange {
            step_id: step.into(),
            kind,
            before: before.into(),
            now: now.into(),
        }
    }

    fn steps_of(workflow: &Value) -> Vec<WorkflowStep> {
        serde_json::from_value(workflow.clone()).unwrap()
    }

    /// Keel's setup, a built-in's check, and a check the workflow has of its own.
    fn steps() -> Vec<WorkflowStep> {
        vec![
            setup("setup", "uv sync"),
            check("check", "uv run pytest"),
            check("lint", "uv run ruff check"),
            WorkflowStep::Approval {
                id: "approval".into(),
            },
        ]
    }

    const BUILT_IN: Option<&str> = Some("builtin:full");
    const SAVED: Option<&str> = Some("mine");

    #[test]
    fn the_run_marks_only_the_steps_keel_took_from_the_project() {
        let workflow = marked(&steps(), Some("setup"), BUILT_IN);
        assert_eq!(workflow[0]["fromProject"], json!(true));
        assert_eq!(workflow[1]["fromProject"], json!(true));
        assert_eq!(workflow[2].get("fromProject"), None);
        // Whatever reads the run's steps reads them as they were.
        assert_eq!(steps_of(&workflow), steps());
        // A saved workflow's steps are its own, whatever they're called.
        for saved in [SAVED, None] {
            let workflow = marked(&steps(), None, saved);
            assert_eq!(workflow, serde_json::to_value(steps()).unwrap());
        }
    }

    #[test]
    fn only_those_steps_follow_the_project() {
        let workflow = marked(&steps(), Some("setup"), BUILT_IN);
        assert_eq!(
            changed(
                &workflow,
                BUILT_IN,
                Some("uv sync --group test"),
                Some("uv run --group test pytest")
            ),
            vec![
                change(
                    "setup",
                    CommandKind::Setup,
                    "uv sync",
                    "uv sync --group test"
                ),
                change(
                    "check",
                    CommandKind::Check,
                    "uv run pytest",
                    "uv run --group test pytest"
                ),
            ]
        );
        // A saved workflow's own check called `check`, beside the setup Keel put first.
        let saved = marked(&steps(), Some("setup"), SAVED);
        assert_eq!(
            changed(&saved, SAVED, Some("uv sync"), Some("make test")),
            vec![]
        );
        assert_eq!(
            changed(
                &saved,
                SAVED,
                Some("uv sync --group test"),
                Some("make test")
            ),
            vec![change(
                "setup",
                CommandKind::Setup,
                "uv sync",
                "uv sync --group test"
            )]
        );
    }

    #[test]
    fn a_run_from_before_the_marks_goes_by_name_only_in_a_built_in_workflow() {
        let old = serde_json::to_value(vec![
            setup("project-setup", "npm ci"),
            check("check", "npm test"),
        ])
        .unwrap();
        assert_eq!(
            changed(&old, BUILT_IN, Some("pnpm i"), Some("pnpm test")),
            vec![
                change("project-setup", CommandKind::Setup, "npm ci", "pnpm i"),
                change("check", CommandKind::Check, "npm test", "pnpm test"),
            ]
        );
        for saved in [SAVED, None] {
            assert_eq!(
                changed(&old, saved, Some("pnpm i"), Some("pnpm test")),
                vec![]
            );
        }
    }

    #[test]
    fn the_same_command_or_none_at_all_changes_nothing() {
        let workflow = marked(&steps(), Some("setup"), BUILT_IN);
        assert_eq!(
            changed(
                &workflow,
                BUILT_IN,
                Some("  uv sync\n"),
                Some("uv run pytest")
            ),
            vec![]
        );
        // A step can't run nothing, so one the project no longer has stays.
        assert_eq!(changed(&workflow, BUILT_IN, None, Some("  ")), vec![]);
        assert_eq!(changed(&json!([]), BUILT_IN, Some("uv sync"), None), vec![]);
    }

    /// Attempts as the store reads them back: step, kind and state.
    fn recorded(rows: &[(&str, &str, AttemptState)]) -> Vec<keel_store::Attempt> {
        let dir = tempfile::tempdir().unwrap();
        let store = keel_store::Store::open(&dir.path().join("keel.db")).unwrap();
        for (index, (step, kind, state)) in rows.iter().enumerate() {
            let id = format!("a{index}");
            store
                .add_attempt(&keel_store::NewAttempt {
                    id: id.clone(),
                    run_id: "r".into(),
                    step_id: (*step).into(),
                    kind: (*kind).into(),
                    role: "worker".into(),
                    round: 0,
                    number: index as i64 + 1,
                    state: *state,
                    model_requested: String::new(),
                    effort: None,
                    tools: (*kind).into(),
                    max_turns: None,
                    max_budget_usd: None,
                    language: "en".into(),
                    prompt: String::new(),
                    system_prompt_sources: "[]".into(),
                    settings: "{}".into(),
                    session_id: id.clone(),
                    resumed_from: None,
                    workspace: String::new(),
                    raw_path: String::new(),
                    started_at: keel_store::now(),
                })
                .unwrap();
        }
        store.attempts_of_run("r").unwrap()
    }

    #[test]
    fn a_step_put_off_for_a_new_setup_comes_round_again_and_is_not_a_person_asking_for_more() {
        use AttemptState::{Postponed, Running, Succeeded};
        let put_off = recorded(&[
            ("setup", "setup", Succeeded),
            ("check", "check", Postponed),
            ("setup", "setup", Running),
        ]);
        assert!(crate::sizing::due_again(&put_off[1], &put_off));
        // So nothing skipped for being small runs because of it, and the run isn't counted as one
        // where a person asked.
        assert!(!crate::sizing::person_asked(&put_off));
        let asked = recorded(&[("approval", "approval", Postponed)]);
        assert!(crate::sizing::person_asked(&asked));
    }

    #[test]
    fn the_new_commands_replace_only_those_steps_and_keep_their_marks() {
        let workflow = marked(&steps(), Some("setup"), BUILT_IN);
        let changes = changed(
            &workflow,
            BUILT_IN,
            Some("uv sync --group test"),
            Some("make check"),
        );
        let updated = with_changes(&workflow, &changes);
        assert_eq!(
            steps_of(&updated),
            vec![
                setup("setup", "uv sync --group test"),
                check("check", "make check"),
                check("lint", "uv run ruff check"),
                WorkflowStep::Approval {
                    id: "approval".into()
                },
            ]
        );
        assert_eq!(updated[1]["fromProject"], json!(true));
        assert_eq!(
            changed(
                &updated,
                BUILT_IN,
                Some("uv sync --group test"),
                Some("make check")
            ),
            vec![]
        );
        assert_eq!(with_changes(&workflow, &[]), workflow);
    }
}
