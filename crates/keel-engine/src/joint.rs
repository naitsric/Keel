//! A change that touches several projects. A run is still one repo. One card is where the change is
//! agreed: a run that only reads, in that card's project, works out what changes in each project
//! and in what order, and a person approves it. Then each project that changes gets a card of its
//! own, and each waits for the one before it to have a pull request (see `Store::open_joint_parts`).

use keel_core::{AttemptState, Reason, StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_store::{Attempt, Card, JointChange, NewPart};
use serde_json::json;

use crate::{Inner, Result, new_id, prompts, specs, workflows};

/// The specifier's usual id, so that whatever reads a run's spec finds this one too.
const AGREE_STEP: &str = "spec";

/// Whether this card is where a change across projects is agreed, rather than one of its parts or
/// an ordinary card.
pub(crate) fn agrees(card: &Card) -> bool {
    card.joint_change_id.as_deref() == Some(card.id.as_str())
}

/// The run where a change across projects is agreed: work out what changes where, and a person
/// approves it. Nothing is built or delivered, so there's nothing to learn from either. It's no
/// saved workflow, so it never counts in how workflows compare.
pub(crate) fn plan_workflow(language: &str) -> Vec<WorkflowStep> {
    let human = if language.starts_with("es") {
        "humano"
    } else {
        "human"
    };
    vec![
        WorkflowStep::Agent(StepSpec {
            id: AGREE_STEP.into(),
            model: "opus".into(),
            effort: None,
            tools: ToolProfile::ReadOnly,
            prompt: prompts::agreement_prompt(language),
            max_turns: None,
            max_budget_usd: None,
            paths: Vec::new(),
            when: None,
            role: StepRole::Specifier,
        }),
        WorkflowStep::Approval { id: human.into() },
    ]
}

/// A project a change touches, by the name its agent knows it by.
#[derive(Debug, Clone)]
pub(crate) struct Named {
    pub id: String,
    pub name: String,
}

/// A run where a change across projects is being agreed.
pub(crate) struct Agreement {
    pub card: Card,
    pub change: JointChange,
    /// The card's own project first, then the others as the person chose them. One removed from
    /// Keel since isn't here.
    pub projects: Vec<Named>,
}

impl Agreement {
    pub fn names(&self) -> Vec<String> {
        self.projects.iter().map(|p| p.name.clone()).collect()
    }

    /// The list the agent gets, saying which one it's in.
    pub fn projects_text(&self, language: &str) -> String {
        let here = if language.starts_with("es") {
            " (en el que estás)"
        } else {
            " (the one you're in)"
        };
        self.projects
            .iter()
            .map(|project| {
                let mark = if project.id == self.card.project_id {
                    here
                } else {
                    ""
                };
                format!("- {}{mark}", project.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Inner {
    /// The agreement a run is for, if it's for one: the run of the card where a change is agreed.
    /// The card can't become one, or stop being one, while its run is open.
    pub(crate) fn agreement(&self, run_id: &str) -> Result<Option<Agreement>> {
        let Some(card_id) = self.store.card_of_run(run_id)? else {
            return Ok(None);
        };
        let card = self.store.card(&card_id)?;
        if !agrees(&card) {
            return Ok(None);
        }
        let change = self.store.joint_change(&card.id)?;
        let mut found = Vec::new();
        for id in std::iter::once(&card.project_id).chain(&change.projects) {
            if self.store.project_is_removed(id)? {
                continue;
            }
            let project = self.store.project(id)?;
            found.push((project.id, project.name, project.path));
        }
        // The agent writes back the name, so two projects called the same go by their folder too.
        let projects = found
            .iter()
            .map(|(id, name, path)| {
                let shared = found.iter().filter(|(_, other, _)| other == name).count() > 1;
                Named {
                    id: id.clone(),
                    name: if shared {
                        format!("{name} ({path})")
                    } else {
                        name.clone()
                    },
                }
            })
            .collect();
        Ok(Some(Agreement {
            card,
            change,
            projects,
        }))
    }

    /// Puts the parts of an approved agreement on the board, once the rest of its run is done.
    /// False if they couldn't go there: the approval is blocked then, saying why, so the person can
    /// try again. Going through here again after the app stopped halfway opens nothing twice.
    pub(crate) fn open_parts(
        &self,
        run_id: &str,
        steps: &[WorkflowStep],
        attempts: &[Attempt],
    ) -> Result<bool> {
        let Some(agreement) = self.agreement(run_id)? else {
            return Ok(true);
        };
        let approval = steps.iter().rev().find_map(|step| match step {
            WorkflowStep::Approval { id } => attempts.iter().rev().find(|a| a.step_id == *id),
            _ => None,
        });
        let Some(approval) = approval else {
            return Ok(true);
        };
        if approval.state != AttemptState::Succeeded.as_str() {
            return Ok(true);
        }
        match self.put_parts_on_board(&agreement, attempts) {
            Ok(cards) => {
                self.store
                    .append_event(&approval.id, "partsOpened", &json!({ "cards": cards }))?;
                Ok(true)
            }
            Err(detail) => {
                let reason = Reason::PartsNotOpened { detail };
                self.store
                    .set_state(&approval.id, AttemptState::Blocked, Some(&reason))?;
                self.store.append_event(
                    &approval.id,
                    "partsNotOpened",
                    &json!({ "reason": reason }),
                )?;
                self.changed(run_id);
                Ok(false)
            }
        }
    }

    fn put_parts_on_board(
        &self,
        agreement: &Agreement,
        attempts: &[Attempt],
    ) -> Result<Vec<String>, String> {
        let spec = workflows::specifying_attempt(attempts)
            .and_then(specs::of_attempt)
            .ok_or_else(|| "there's no agreement to open".to_owned())?;
        let mut parts = Vec::new();
        for part in &spec.parts {
            let project = agreement
                .projects
                .iter()
                .find(|project| project.name == part.project)
                .ok_or_else(|| format!("{} is no longer in Keel", part.project))?;
            parts.push(NewPart {
                id: new_id(),
                project_id: project.id.clone(),
                title: agreement.card.title.clone(),
                body: part.text.clone(),
            });
        }
        self.store
            .open_joint_parts(&agreement.change.id, &json!(spec), &parts, &spec.together)
            .map_err(|error| error.to_string())
    }
}
