//! A run keeps a copy of the project's setup and check from when it started. When the person
//! fixes one of them, a retry can take the new one.

use keel_core::WorkflowStep;
use serde::Serialize;

/// A run's step with one of the project's commands, as the run has it and as the project has it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandChange {
    pub step_id: String,
    pub before: String,
    pub now: String,
}

/// The ids Keel gives the project's setup when it puts it first in a run. See `start_run`.
const SETUP_IDS: [&str; 2] = ["setup", "project-setup"];
/// The id of the project's check in the built-in workflows. See `prompts::full_workflow`.
const CHECK_ID: &str = "check";

/// The run's setup and check whose command the project has changed since the run started. Any
/// other step that runs a command is the workflow's own, and keeps it. A command the project no
/// longer has changes nothing, since a step can't run nothing.
pub fn changed(
    steps: &[WorkflowStep],
    setup: Option<&str>,
    check: Option<&str>,
) -> Vec<CommandChange> {
    fn current(command: Option<&str>) -> Option<&str> {
        command.map(str::trim).filter(|c| !c.is_empty())
    }
    let (setup, check) = (current(setup), current(check));
    steps
        .iter()
        .filter_map(|step| {
            let (id, run, now) = match step {
                WorkflowStep::Setup { id, run } if SETUP_IDS.contains(&id.as_str()) => {
                    (id, run, setup?)
                }
                WorkflowStep::Check { id, run } if id == CHECK_ID => (id, run, check?),
                _ => return None,
            };
            (run.trim() != now).then(|| CommandChange {
                step_id: id.clone(),
                before: run.clone(),
                now: now.to_owned(),
            })
        })
        .collect()
}

/// The steps with those commands in place of the ones they had.
pub fn with_changes(steps: &[WorkflowStep], changes: &[CommandChange]) -> Vec<WorkflowStep> {
    steps
        .iter()
        .map(|step| match step {
            WorkflowStep::Setup { id, .. } | WorkflowStep::Check { id, .. } => {
                let Some(change) = changes.iter().find(|c| &c.step_id == id) else {
                    return step.clone();
                };
                let run = change.now.clone();
                match step {
                    WorkflowStep::Setup { .. } => WorkflowStep::Setup {
                        id: id.clone(),
                        run,
                    },
                    _ => WorkflowStep::Check {
                        id: id.clone(),
                        run,
                    },
                }
            }
            _ => step.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn change(step: &str, before: &str, now: &str) -> CommandChange {
        CommandChange {
            step_id: step.into(),
            before: before.into(),
            now: now.into(),
        }
    }

    #[test]
    fn only_the_setup_and_check_keel_took_from_the_project_follow_it() {
        let steps = vec![
            setup("setup", "uv sync"),
            check("check", "uv run pytest"),
            // A workflow's own check runs what the workflow says.
            check("lint", "uv run ruff check"),
            WorkflowStep::Approval {
                id: "approval".into(),
            },
        ];
        assert_eq!(
            changed(
                &steps,
                Some("uv sync --group test"),
                Some("uv run --group test pytest")
            ),
            vec![
                change("setup", "uv sync", "uv sync --group test"),
                change("check", "uv run pytest", "uv run --group test pytest"),
            ]
        );
        // Named so when the workflow has a step called `setup` of its own.
        assert_eq!(
            changed(&[setup("project-setup", "npm ci")], Some("pnpm i"), None),
            vec![change("project-setup", "npm ci", "pnpm i")]
        );
    }

    #[test]
    fn the_same_command_or_none_at_all_changes_nothing() {
        let steps = vec![setup("setup", "uv sync"), check("check", "make test")];
        assert_eq!(
            changed(&steps, Some("  uv sync\n"), Some("make test")),
            vec![]
        );
        // A step can't run nothing, so one the project no longer has stays.
        assert_eq!(changed(&steps, None, Some("  ")), vec![]);
        assert_eq!(changed(&[], Some("uv sync"), Some("make test")), vec![]);
    }

    #[test]
    fn the_new_commands_replace_only_those_steps() {
        let steps = vec![
            setup("setup", "uv sync"),
            check("check", "make test"),
            check("lint", "make lint"),
        ];
        let changes = changed(&steps, Some("uv sync --group test"), Some("make check"));
        assert_eq!(
            with_changes(&steps, &changes),
            vec![
                setup("setup", "uv sync --group test"),
                check("check", "make check"),
                check("lint", "make lint"),
            ]
        );
        assert_eq!(with_changes(&steps, &[]), steps);
    }
}
