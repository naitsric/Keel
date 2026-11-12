//! A change that touches several projects. A run is still one repo. One card is where the change is
//! agreed: a run that only reads, in that card's project, works out what changes in each project
//! and in what order, and a person approves it. Then each project that changes gets a card of its
//! own, and each waits for the one before it to have a pull request (see `Store::open_joint_parts`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use keel_core::{AttemptState, PrState, Reason, StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_harness::github;
use keel_store::rusqlite_value::Value as Column;
use keel_store::{Attempt, Card, JointChange, NewPart, Project, Run};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::deliver::repo_url;
use crate::git::{BASE_REF, BRANCH_PREFIX, Git, RemoteAuth};
use crate::specs::{ContractPoint, Criterion, GivenAnswer, Spec};
use crate::verification::Outcome;
use crate::{Error, Inner, Result, locked, new_id, prompts, specs, verification, workflows};

/// The specifier's usual id, so that whatever reads a run's spec finds this one too.
const AGREE_STEP: &str = "spec";

/// Where what was agreed keeps the request it was agreed from, which each part reads in full.
const ASKED: &str = "request";

/// And who wrote each piece of it, as Keel knew when the run that agreed it started.
const ASKED_WRITTEN: &str = "requestWritten";

/// Where a project's mirror keeps what Keel last read of its default branch for a run in another
/// project. Apart from `keel-base`, which a run of this project starts from.
const READ_BRANCH: &str = "keel-read";

/// What a part's specifier was given of the contract, recorded on its attempt once what it wrote is
/// kept: the points, as they were then.
pub(crate) const CONTRACT_GIVEN: &str = "contractGiven";

/// Where it keeps what it last read of an earlier part's branch.
fn part_read_branch(branch: &str) -> String {
    let short = branch.strip_prefix(BRANCH_PREFIX).unwrap_or(branch);
    format!("{READ_BRANCH}-{}", short.replace('/', "-"))
}

/// Whether an agent in this role reads the change's other projects and is told about the change. A
/// map is of its own project, and what a run taught comes from that run.
pub(crate) fn reads_others(role: &StepRole) -> bool {
    !matches!(role, StepRole::Mapper | StepRole::Learner)
}

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
            risks: Vec::new(),
            model_when_risky: None,
            role: StepRole::Specifier,
        }),
        WorkflowStep::Approval { id: human.into() },
    ]
}

/// A project a change touches, by the name its agent knows it by.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Named {
    pub id: String,
    pub name: String,
}

/// A change across several projects, as a person reads it in one place.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JointChangeView {
    pub id: String,
    /// The card where it's agreed, while it's still on the board.
    pub agreed_in: Option<JointCard>,
    /// That card's project first, then the others as the person chose them.
    pub projects: Vec<Named>,
    /// What a person approved, with what they answered on the way. None until then.
    pub agreed: Option<Value>,
    /// First to last. A part that was deleted isn't here.
    pub parts: Vec<JointCard>,
    /// The projects it touches that no part changes.
    pub read_only: Vec<Named>,
    /// What only works with every part in place, which only a person can try.
    pub checks: Vec<keel_store::JointCheck>,
    /// What the parts agree on exactly, point by point, with what each part that keeps it verified:
    /// as a person corrected it since it was agreed, if they did. Empty until it's agreed, and for
    /// an agreement without any.
    pub contract: Vec<ContractView>,
    /// The number the next new point of the contract takes, so a person sees the id it'll have.
    /// None until it's agreed.
    pub contract_next: Option<i64>,
}

/// A point of what the parts agree on exactly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractView {
    pub id: String,
    pub when: String,
    pub then: String,
    /// The projects it names, by the names they go by in what was agreed.
    pub projects: Vec<String>,
    /// The parts that keep it, first to last.
    pub parts: Vec<ContractPart>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractPart {
    pub card_id: String,
    pub position: i64,
    pub project: String,
    /// What its verifier said of it in its run's latest round. None until it said something, and
    /// while the part was specified with the point as it no longer is.
    pub result: Option<Outcome>,
    /// False once its run shows a workflow with no verifier, which never will. Until it has a run,
    /// nothing says.
    pub verifies: bool,
    /// Whether its part was specified with this point as it was before a person corrected it, or
    /// before it was added. Starting the part again gives it the point as it is.
    pub earlier: bool,
}

/// A point of the contract as a person writes it, correcting it once it's agreed. One already
/// there keeps its id, and a new one has none until Keel gives it one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractEdit {
    #[serde(default)]
    pub id: Option<String>,
    pub when: String,
    pub then: String,
    /// The projects whose part has to keep it, by the names they go by in what was agreed.
    pub projects: Vec<String>,
}

/// Whether a workflow has a step that checks what its spec says has to be true. A verifier can't
/// be in a group (see `workflows::validate`).
fn has_verifier(steps: &[WorkflowStep]) -> bool {
    steps
        .iter()
        .any(|step| matches!(step, WorkflowStep::Agent(spec) if spec.role == StepRole::Verifier))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JointCard {
    pub card_id: String,
    pub project_id: String,
    pub project: String,
    pub title: String,
    pub column: keel_core::BoardColumn,
    pub run_id: Option<String>,
    pub run_state: Option<AttemptState>,
    pub pr_url: Option<String>,
    pub pr_state: Option<PrState>,
    /// None on the card where it's agreed.
    pub part: Option<PartOf>,
}

/// What a run's page says about the change across projects it's in.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunJoint {
    pub change_id: String,
    /// Whether this run is where it's agreed, rather than one of its parts.
    pub agrees: bool,
    pub part: Option<PartOf>,
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

/// Another project of the change, which a run reads and can't change.
#[derive(Debug, Clone)]
pub(crate) struct ReadCopy {
    pub project_id: String,
    pub name: String,
    pub path: PathBuf,
    /// The earlier part in that project, if it has a pull request: then its branch is what's read.
    pub part: Option<EarlierPart>,
}

#[derive(Debug, Clone)]
pub(crate) struct EarlierPart {
    pub position: i64,
    pub run_id: String,
    pub branch: String,
}

/// A copy that's there to read, and the commit it's at.
#[derive(Debug, Clone)]
pub(crate) struct ReadyCopy {
    pub copy: ReadCopy,
    pub commit: String,
}

/// Where a card stands as one part of a change across projects.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartOf {
    pub change_id: String,
    /// Which part it is, from 1.
    pub position: i64,
    /// How many parts were agreed. One deleted since still counts, so that the others keep their
    /// numbers.
    pub total: i64,
    /// The part it starts after, while it's still waiting for that one's pull request.
    pub waits_for: Option<Neighbour>,
    /// What changed in the parts before it since it read them.
    pub warnings: Vec<PartWarning>,
    /// The points of the contract it was specified with as they no longer are, by id: corrected,
    /// added or removed since. Empty until it's specified. Starting it again gives it the contract
    /// as it is.
    pub contract_changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Neighbour {
    pub card_id: String,
    pub position: Option<i64>,
    pub project: String,
}

/// Something about a part before this one that a person should know before approving this one.
/// Worked out each time, never stored: Keel only finds out by looking at GitHub.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PartWarning {
    /// Its pull request was closed without being merged.
    Closed { position: i64, project: String },
    /// Its pull request has commits this part didn't read.
    Changed { position: i64, project: String },
}

/// How a pull request in `repo` names the issue a run came from. A bare number would point at
/// that repo's own issue of the same number. None if the issue doesn't say which repo it's in.
pub(crate) fn issue_ref(issue: &Value, repo: &str) -> Option<String> {
    let (from, number) = issue
        .get("url")
        .and_then(|url| url.as_str())
        .and_then(github::issue_in)?;
    Some(if from.eq_ignore_ascii_case(repo) {
        format!("Refs #{number}")
    } else {
        format!("Refs {from}#{number}")
    })
}

/// What was agreed, as a spec. None until it was.
fn agreed_spec(change: &JointChange) -> Option<Spec> {
    serde_json::from_value(change.agreed.clone()?).ok()
}

/// What was agreed, as a spec, with its contract as a person corrected it since. None until it was
/// agreed, and if the correction can't be read.
fn current_spec(change: &JointChange) -> Option<Spec> {
    let mut spec = agreed_spec(change)?;
    if let Some(contract) = &change.contract {
        spec.contract = serde_json::from_value(contract.clone()).ok()?;
    }
    Some(spec)
}

/// The number in a point's id: 3 for K3.
fn point_number(id: &str) -> Option<i64> {
    id.strip_prefix('K')?.parse().ok()
}

/// The number the next new point of a change's contract takes: past every one it ever gave, so a
/// point removed never comes back meaning something else.
fn next_point(change: &JointChange) -> i64 {
    change.contract_next.unwrap_or_else(|| {
        let agreed = agreed_spec(change).map(|spec| spec.contract);
        let highest = agreed
            .iter()
            .flatten()
            .filter_map(|point| point_number(&point.id))
            .max();
        highest.unwrap_or(0) + 1
    })
}

/// What a part's run was given of the contract with its spec.
enum Given {
    /// Its spec isn't kept yet, or a step that isn't a specifier wrote it, which gets no points.
    Nothing,
    /// What was recorded as its answer was kept: its own points, and every point then.
    Recorded {
        points: Vec<Criterion>,
        contract: Option<Vec<ContractPoint>>,
    },
    /// Kept before Keel recorded it: what was agreed, since nothing could correct it then.
    Agreed,
}

/// The points a part was `given` as they no longer are, by id: each of the points it has `now` that
/// it wasn't given word for word, then each it was given that it no longer has.
fn changed_points(given: &[Criterion], now: &[Criterion]) -> Vec<String> {
    let mut changed: Vec<String> = now
        .iter()
        .filter(|point| !given.contains(point))
        .map(|point| point.id.clone())
        .collect();
    for point in given {
        if !now.iter().any(|kept| kept.id == point.id) && !changed.contains(&point.id) {
            changed.push(point.id.clone());
        }
    }
    changed
}

/// The name a part's project goes by in what was agreed.
fn part_name(spec: &Spec, position: i64) -> Option<&str> {
    let index = usize::try_from(position - 1).ok()?;
    spec.parts.get(index).map(|part| part.project.as_str())
}

/// How many parts were agreed.
fn total(change: &JointChange) -> i64 {
    let agreed = change
        .agreed
        .as_ref()
        .and_then(|agreed| agreed.get("parts"))
        .and_then(|parts| parts.as_array())
        .map(Vec::len);
    i64::try_from(agreed.unwrap_or(change.parts.len())).unwrap_or(i64::MAX)
}

fn copy_failed(copy: &ReadCopy, detail: impl Into<String>) -> Error {
    Error::Blocked(Reason::ReadCopyFailed {
        project: copy.name.clone(),
        detail: detail.into(),
    })
}

impl crate::Engine {
    /// A change across several projects, with where each of its parts stands.
    pub fn joint_change(&self, id: &str) -> Result<JointChangeView> {
        let inner = &self.inner;
        let change = inner.store.joint_change(id)?;
        let agreeing = match inner.store.card(id) {
            Ok(card) => Some(card),
            Err(keel_store::Error::NotFound(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let ids: Vec<String> = agreeing
            .iter()
            .map(|card| card.project_id.clone())
            .chain(change.projects.iter().cloned())
            .collect();
        let projects = inner.named(&ids)?;
        let mut parts = Vec::new();
        for card_id in &change.parts {
            parts.push(inner.joint_card_view(inner.store.card(card_id)?)?);
        }
        let read_only = projects
            .iter()
            .filter(|project| !parts.iter().any(|part| part.project_id == project.id))
            .cloned()
            .collect();
        let contract = inner.contract_view(&change, &parts)?;
        let contract_next = change.agreed.is_some().then(|| next_point(&change));
        Ok(JointChangeView {
            id: change.id.clone(),
            agreed_in: agreeing
                .map(|card| inner.joint_card_view(card))
                .transpose()?,
            projects,
            // Until it's agreed, every project is only read, and that says nothing yet.
            read_only: if change.agreed.is_some() {
                read_only
            } else {
                Vec::new()
            },
            agreed: change.agreed,
            parts,
            checks: change.checks,
            contract,
            contract_next,
        })
    }

    /// The change across projects a run is in, if it's in one.
    pub fn run_joint(&self, run_id: &str) -> Result<Option<RunJoint>> {
        let Some((card, change)) = self.inner.joint_card(run_id)? else {
            return Ok(None);
        };
        Ok(Some(RunJoint {
            change_id: change.id,
            agrees: agrees(&card),
            part: self.inner.part_of(&card)?,
        }))
    }

    /// A person says something that only works with every part in place works, or takes it back.
    /// Nothing else can: no agent, and no assistant.
    pub fn joint_check_set(&self, change_id: &str, seq: i64, works: bool) -> Result<()> {
        if !self
            .inner
            .store
            .confirm_joint_check(change_id, seq, works)?
        {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// A person corrects what the parts of an agreed change agree on exactly: `points` replace the
    /// contract, in their order. One already there keeps its id wherever it goes, and a new one
    /// takes the next, never one given before. It's checked as the agreement's was, and what was
    /// approved stays as it was. Nothing else can: no agent, and no assistant. A part takes the
    /// contract as it is when its specifier's answer is kept, so one specified before says so,
    /// and starting it again gives it this one. `InvalidState` before it's agreed, for an id that
    /// isn't there or comes twice, and when another correction came first.
    pub fn joint_contract_set(&self, change_id: &str, points: &[ContractEdit]) -> Result<()> {
        let store = &self.inner.store;
        let change = store.joint_change(change_id)?;
        let Some(spec) = current_spec(&change) else {
            return Err(Error::InvalidState);
        };
        let mut next = next_point(&change);
        let mut kept: Vec<&str> = Vec::new();
        let mut numbered = Vec::new();
        for point in points {
            let id = match &point.id {
                Some(id) => {
                    let there = spec.contract.iter().any(|known| known.id == *id);
                    if !there || kept.contains(&id.as_str()) {
                        return Err(Error::InvalidState);
                    }
                    kept.push(id);
                    id.clone()
                }
                None => {
                    next += 1;
                    format!("K{}", next - 1)
                }
            };
            numbered.push(ContractPoint {
                id,
                when: point.when.clone(),
                then: point.then.clone(),
                projects: point.projects.clone(),
            });
        }
        // A point may name each project with a part, and any other one the contract named, as it
        // was agreed or since: all were among those the agent was given.
        let mut known: Vec<String> = spec.parts.iter().map(|p| p.project.clone()).collect();
        let agreed = agreed_spec(&change).map(|agreed| agreed.contract);
        for project in agreed
            .iter()
            .flatten()
            .chain(&spec.contract)
            .flat_map(|point| &point.projects)
        {
            if !known.contains(project) {
                known.push(project.clone());
            }
        }
        let contract = spec
            .corrected_contract(numbered, &known)
            .map_err(|problems| Error::Blocked(Reason::ContractNotValid { problems }))?;
        // Saving it as it is changes nothing.
        if contract == spec.contract && next == next_point(&change) {
            return Ok(());
        }
        if !store.correct_joint_contract(
            change_id,
            change.contract.as_ref(),
            change.contract_next,
            &json!(contract),
            next,
        )? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }

    /// Lets a part start without waiting for the one before it to have a pull request: that one
    /// was abandoned, say, because its project needed nothing after all.
    pub fn card_stop_waiting(&self, card_id: &str) -> Result<()> {
        if !self.inner.store.stop_waiting(card_id)? {
            return Err(Error::InvalidState);
        }
        self.inner.changed("");
        Ok(())
    }
}

impl Inner {
    fn joint_card_view(&self, card: Card) -> Result<JointCard> {
        let part = self.part_of(&card)?;
        Ok(JointCard {
            project: self.store.project(&card.project_id)?.name,
            column: card.column(),
            card_id: card.id,
            project_id: card.project_id,
            title: card.title,
            run_id: card.run_id,
            run_state: card.run_state,
            pr_url: card.pr_url,
            pr_state: card.pr_state,
            part,
        })
    }

    /// The projects of a change, as its agents know them: the card where it was agreed first, then
    /// the others as the person chose them. One removed from Keel since isn't here.
    fn named(&self, ids: &[String]) -> Result<Vec<Named>> {
        let mut found: Vec<(String, String, String)> = Vec::new();
        for id in ids {
            if found.iter().any(|(seen, _, _)| seen == id) || self.store.project_is_removed(id)? {
                continue;
            }
            let project = self.store.project(id)?;
            found.push((project.id, project.name, project.path));
        }
        // The agent writes back the name, so two projects called the same go by their folder too.
        Ok(found
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
            .collect())
    }

    /// The card of a run that's part of a change across projects, as the card where it's agreed or
    /// as one of its parts, with the change.
    fn joint_card(&self, run_id: &str) -> Result<Option<(Card, JointChange)>> {
        let Some(card_id) = self.store.card_of_run(run_id)? else {
            return Ok(None);
        };
        let card = self.store.card(&card_id)?;
        let Some(change_id) = card.joint_change_id.clone() else {
            return Ok(None);
        };
        match self.store.joint_change(&change_id) {
            Ok(change) => Ok(Some((card, change))),
            Err(keel_store::Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// What was agreed for the change a part belongs to. None for a card of no change, or one
    /// that's gone.
    fn agreed_of(&self, part: &Card) -> Result<Option<Value>> {
        let Some(change_id) = &part.joint_change_id else {
            return Ok(None);
        };
        match self.store.joint_change(change_id) {
            Ok(change) => Ok(change.agreed),
            Err(keel_store::Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The request a part's change was agreed from, word for word. None for a part opened before
    /// Keel kept it.
    pub(crate) fn asked_of(&self, part: &Card) -> Result<Option<String>> {
        Ok(self
            .agreed_of(part)?
            .as_ref()
            .and_then(|agreed| agreed.get(ASKED))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// For a run that does one part of a change across projects, the contract as it is now: the
    /// points its part has to keep, as criteria, and every point. As a person corrected them since
    /// it was agreed, if they did. None for any other run. Empty for an agreement without any.
    pub(crate) fn part_contract(
        &self,
        run_id: &str,
    ) -> Result<Option<(Vec<Criterion>, Vec<ContractPoint>)>> {
        let Some((card, change)) = self.joint_card(run_id)? else {
            return Ok(None);
        };
        let Some(here) = card.joint_position else {
            return Ok(None);
        };
        let Some(spec) = current_spec(&change) else {
            return Ok(Some((Vec::new(), Vec::new())));
        };
        let own = part_name(&spec, here)
            .map(|name| spec.contract_of(name))
            .unwrap_or_default();
        Ok(Some((own, spec.contract)))
    }

    /// What a part's run was given of the contract with its spec, by its latest kept specifier
    /// answer.
    fn given(&self, run_id: &str) -> Result<Given> {
        let attempts = self.store.attempts_of_run(run_id)?;
        // A step that isn't a specifier is never given any points.
        let Some(specified) =
            workflows::specifying_attempt(&attempts).filter(|a| a.role == "specifier")
        else {
            return Ok(Given::Nothing);
        };
        let recorded = self
            .store
            .run_events(run_id, CONTRACT_GIVEN)?
            .into_iter()
            .rev()
            .find(|event| event.attempt_id == specified.id);
        Ok(match recorded {
            Some(event) => Given::Recorded {
                points: serde_json::from_value(event.data["points"].clone()).unwrap_or_default(),
                contract: serde_json::from_value(event.data["contract"].clone()).ok(),
            },
            None => Given::Agreed,
        })
    }

    /// The points of the contract a part's card was specified with as they no longer are: see
    /// [`PartOf::contract_changed`]. Nothing can differ while nobody has corrected it.
    fn contract_changed(&self, change: &JointChange, card: &Card) -> Result<Vec<String>> {
        let (Some(here), Some(run_id)) = (card.joint_position, &card.run_id) else {
            return Ok(Vec::new());
        };
        if change.contract.is_none() {
            return Ok(Vec::new());
        }
        let of = |spec: Option<Spec>| {
            spec.and_then(|spec| part_name(&spec, here).map(|name| spec.contract_of(name)))
                .unwrap_or_default()
        };
        let given = match self.given(run_id)? {
            Given::Nothing => return Ok(Vec::new()),
            Given::Recorded { points, .. } => points,
            Given::Agreed => of(agreed_spec(change)),
        };
        Ok(changed_points(&given, &of(current_spec(change))))
    }

    /// Each point of the contract, with what the verifier of each part that keeps it said of it in
    /// that part's latest round, the same round its pull request reports.
    fn contract_view(
        &self,
        change: &JointChange,
        parts: &[JointCard],
    ) -> Result<Vec<ContractView>> {
        let Some(spec) = current_spec(change).filter(|spec| !spec.contract.is_empty()) else {
            return Ok(Vec::new());
        };
        let mut verified = Vec::new();
        for card in parts {
            let found = match &card.run_id {
                Some(run_id) => {
                    let run = self.store.run(run_id)?;
                    let attempts = self.store.attempts_of_run(run_id)?;
                    let steps: Vec<WorkflowStep> =
                        serde_json::from_value(run.workflow).unwrap_or_default();
                    (
                        verification::in_round(&attempts, run.round).pop(),
                        has_verifier(&steps),
                    )
                }
                None => (None, true),
            };
            verified.push(found);
        }
        Ok(spec
            .contract
            .iter()
            .map(|point| ContractView {
                id: point.id.clone(),
                when: point.when.clone(),
                then: point.then.clone(),
                projects: point.projects.clone(),
                parts: parts
                    .iter()
                    .zip(&verified)
                    .filter_map(|(card, (latest, verifies))| {
                        let part = card.part.as_ref()?;
                        let name = part_name(&spec, part.position)?;
                        // What its verifier found is of the point as it was then.
                        let earlier = part.contract_changed.contains(&point.id);
                        point
                            .projects
                            .iter()
                            .any(|named| named == name)
                            .then(|| ContractPart {
                                card_id: card.card_id.clone(),
                                position: part.position,
                                project: card.project.clone(),
                                result: latest.as_ref().filter(|_| !earlier).and_then(|latest| {
                                    latest
                                        .criteria
                                        .iter()
                                        .find(|checked| checked.id == point.id)
                                        .map(|checked| checked.result)
                                }),
                                verifies: *verifies,
                                earlier,
                            })
                    })
                    .collect(),
            })
            .collect())
    }

    /// Who wrote each piece of the request a part's change was agreed from, as Keel knew when the
    /// run that agreed it started. Nothing for a change agreed before Keel kept it.
    pub(crate) fn asked_written_of(&self, part: &Card) -> Result<Vec<specs::Written>> {
        Ok(self
            .agreed_of(part)?
            .and_then(|agreed| serde_json::from_value(agreed.get(ASKED_WRITTEN)?.clone()).ok())
            .unwrap_or_default())
    }

    /// Whether a run does one part of a change across projects.
    pub(crate) fn is_part(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .joint_card(run_id)?
            .is_some_and(|(card, _)| card.joint_position.is_some()))
    }

    /// The agreement a run is for, if it's for one: the run of the card where a change is agreed.
    /// The card can't become one, or stop being one, while its run is open.
    pub(crate) fn agreement(&self, run_id: &str) -> Result<Option<Agreement>> {
        let Some((card, change)) = self.joint_card(run_id)? else {
            return Ok(None);
        };
        if !agrees(&card) {
            return Ok(None);
        }
        let ids: Vec<String> = std::iter::once(card.project_id.clone())
            .chain(change.projects.iter().cloned())
            .collect();
        let projects = self.named(&ids)?;
        Ok(Some(Agreement {
            card,
            change,
            projects,
        }))
    }

    /// The other projects a run reads, if it's part of a change across projects. Only what the
    /// store says: nothing is fetched here. Where the change is agreed, each project as it is now.
    /// For a part, also each project whose part came before and has a pull request: then its branch.
    pub(crate) fn read_copies(&self, run_id: &str) -> Result<Vec<ReadCopy>> {
        let Some((card, change)) = self.joint_card(run_id)? else {
            return Ok(Vec::new());
        };
        // The card where it was agreed may have been deleted since, and its project only goes with
        // it if it isn't a part too.
        let agreed_in = if agrees(&card) {
            Some(card.project_id.clone())
        } else {
            match self.store.card(&change.id) {
                Ok(agreeing) => Some(agreeing.project_id),
                Err(keel_store::Error::NotFound(_)) => None,
                Err(error) => return Err(error.into()),
            }
        };
        let ids: Vec<String> = agreed_in
            .into_iter()
            .chain(change.projects.iter().cloned())
            .collect();
        let mut earlier = Vec::new();
        for (part, run) in self.earlier_parts(&card, &change)? {
            if let (Some(_), Some(branch)) = (run.pr_url, run.branch) {
                earlier.push((
                    part.project_id,
                    EarlierPart {
                        position: part.joint_position.unwrap_or_default(),
                        run_id: run.id,
                        branch,
                    },
                ));
            }
        }
        let folder = self.run_dir(run_id).join("context");
        Ok(self
            .named(&ids)?
            .into_iter()
            .filter(|project| project.id != card.project_id)
            .map(|project| ReadCopy {
                path: folder.join(&project.id),
                part: earlier
                    .iter()
                    .find(|(id, _)| *id == project.id)
                    .map(|(_, part)| part.clone()),
                project_id: project.id,
                name: project.name,
            })
            .collect())
    }

    /// The parts before this card's that have started, first to last, with their runs.
    fn earlier_parts(&self, card: &Card, change: &JointChange) -> Result<Vec<(Card, Run)>> {
        let Some(here) = card.joint_position else {
            return Ok(Vec::new());
        };
        let mut earlier = Vec::new();
        for id in &change.parts {
            let part = self.store.card(id)?;
            if !part.joint_position.is_some_and(|at| at < here) {
                continue;
            }
            if let Some(run_id) = &part.run_id {
                let run = self.store.run(run_id)?;
                earlier.push((part, run));
            }
        }
        Ok(earlier)
    }

    /// The runs of the parts before this run's, if it does one part of a change across projects.
    pub(crate) fn earlier_part_runs(&self, run_id: &str) -> Result<Vec<Run>> {
        let Some((card, change)) = self.joint_card(run_id)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .earlier_parts(&card, &change)?
            .into_iter()
            .map(|(_, run)| run)
            .collect())
    }

    /// Where a card stands as one part of a change across projects. None for any other card.
    pub(crate) fn part_of(&self, card: &Card) -> Result<Option<PartOf>> {
        let (Some(change_id), Some(here)) = (&card.joint_change_id, card.joint_position) else {
            return Ok(None);
        };
        let change = match self.store.joint_change(change_id) {
            Ok(change) => change,
            Err(keel_store::Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut warnings = Vec::new();
        for (part, run) in self.earlier_parts(card, &change)? {
            let position = part.joint_position.unwrap_or_default();
            let project = self.store.project(&part.project_id)?.name;
            // Not knowing which commit either side is at says nothing.
            let read = card.basis.get(&run.id);
            if run.pr_state == Some(PrState::Closed) {
                warnings.push(PartWarning::Closed { position, project });
            } else if let (Some(read), Some(head)) = (read, &run.pr_head)
                && read != head
            {
                warnings.push(PartWarning::Changed { position, project });
            }
        }
        let waits_for = match (&card.waits_for_card, card.waiting) {
            (Some(id), true) => match self.store.card(id) {
                Ok(before) => Some(Neighbour {
                    card_id: before.id,
                    position: before.joint_position,
                    project: self.store.project(&before.project_id)?.name,
                }),
                Err(keel_store::Error::NotFound(_)) => None,
                Err(error) => return Err(error.into()),
            },
            _ => None,
        };
        Ok(Some(PartOf {
            change_id: change.id.clone(),
            position: here,
            total: total(&change),
            waits_for,
            warnings,
            contract_changed: self.contract_changed(&change, card)?,
        }))
    }

    /// What a part's pull request says about the change it's part of, for whoever reviews it
    /// without having seen Keel: which part it is, the others in the order they go in, what was
    /// only read, and what only works with every part in place. The issue goes here too, named so
    /// that it points at the right repo. None for any other run.
    pub(crate) fn pr_section(
        &self,
        run: &Run,
        repo: &str,
        spanish: bool,
    ) -> Result<Option<String>> {
        let Some((card, change)) = self.joint_card(&run.id)? else {
            return Ok(None);
        };
        let Some(here) = card.joint_position else {
            return Ok(None);
        };
        let say = |en: &'static str, es: &'static str| if spanish { es } else { en };
        let total = total(&change);
        let mut text = if spanish {
            format!("### Parte {here} de {total} de un cambio en varios proyectos\n\n")
        } else {
            format!("### Part {here} of {total} of a change across several projects\n\n")
        };
        if let Some(line) = run
            .source_issue
            .as_ref()
            .and_then(|issue| issue_ref(issue, repo))
        {
            text.push_str(&format!("{line}\n\n"));
        }
        text.push_str(say(
            "It only works with every part in place. Merge and deploy them in this order:\n\n",
            "Solo funciona con todas las partes en su lugar. Mergéalas y despliégalas en este orden:\n\n",
        ));
        let mut changed = Vec::new();
        for id in &change.parts {
            let part = self.store.card(id)?;
            let Some(position) = part.joint_position else {
                continue;
            };
            changed.push(part.project_id.clone());
            let name = self.store.project(&part.project_id)?.name;
            let pull_request = match &part.run_id {
                Some(run_id) => self.store.run(run_id)?.pr_url,
                None => None,
            };
            let what = match pull_request.as_deref().and_then(github::pull_request_of) {
                _ if part.id == card.id => say("this pull request", "este pull request").to_owned(),
                Some((repo, number)) => format!("{repo}#{number}"),
                None if position > here => {
                    say("opens after this one", "se abre después de este").to_owned()
                }
                None => say("no pull request yet", "todavía sin pull request").to_owned(),
            };
            text.push_str(&format!(
                "- {} {position}, **{name}**: {what}\n",
                say("Part", "Parte")
            ));
        }
        let agreed_in = match self.store.card(&change.id) {
            Ok(agreeing) => Some(agreeing.project_id),
            Err(keel_store::Error::NotFound(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let read: Vec<String> = agreed_in
            .into_iter()
            .chain(change.projects.iter().cloned())
            .filter(|id| !changed.contains(id))
            .collect();
        let read = self.named(&read)?;
        if !read.is_empty() {
            let names = read
                .iter()
                .map(|project| format!("**{}**", project.name))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!(
                "\n{} {names}.\n",
                say("Read, and not changed:", "Se leyeron, sin cambios:")
            ));
        }
        if !change.checks.is_empty() {
            text.push_str(&format!(
                "\n**{}** {}\n\n",
                say("To try together.", "Para probar juntos."),
                say(
                    "No single pull request proves these. A person tries them with every part in place, and marks them in Keel.",
                    "Ningún pull request por sí solo lo prueba. Una persona lo prueba con todas las partes en su lugar, y lo marca en Keel.",
                )
            ));
            for check in &change.checks {
                let mark = if check.confirmed_at.is_some() {
                    "x"
                } else {
                    " "
                };
                text.push_str(&format!("- [{mark}] {}\n", check.text));
            }
        }
        text.push('\n');
        Ok(Some(text))
    }

    fn read_lock(&self, key: String) -> Arc<Mutex<()>> {
        locked(&self.read_locks).entry(key).or_default().clone()
    }

    /// Makes sure each copy is there, fetching the ones that aren't. A run makes each once, and
    /// every step after reads the same. A copy that can't be had from anywhere blocks the step,
    /// naming the project, and nothing else: one project's account being signed out mustn't hold up
    /// the board. Never under `creating_workspace`, which every run waits for, since fetching has
    /// no time limit.
    pub(crate) fn prepare_read_copies(
        &self,
        attempt: &Attempt,
        copies: &[ReadCopy],
    ) -> Result<Vec<ReadyCopy>> {
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        let git = Git::new(
            locked(&self.tools)
                .git
                .clone()
                .ok_or(Error::Blocked(Reason::GitMissing))?,
        );
        // Steps of a group start together, and must not make the same copy twice.
        let run_lock = self.read_lock(format!("run:{}", attempt.run_id));
        let _run = locked(&run_lock);
        let card_id = self.store.card_of_run(&attempt.run_id)?;
        let mut ready = Vec::new();
        let mut made = Vec::new();
        for copy in copies {
            let commit = if copy.path.join(".git").exists() {
                git.head(&copy.path)
                    .map_err(|error| copy_failed(copy, error.to_string()))?
            } else {
                let (commit, record) = self.make_read_copy(&git, copy)?;
                if let (Some(card_id), Some(part)) = (&card_id, &copy.part) {
                    self.store.add_card_basis(card_id, &part.run_id, &commit)?;
                }
                made.push(record);
                commit
            };
            ready.push(ReadyCopy {
                copy: copy.clone(),
                commit,
            });
        }
        if !made.is_empty() {
            self.store
                .append_event(&attempt.id, "readCopies", &json!({ "copies": made }))?;
        }
        Ok(ready)
    }

    /// One copy, from what the project has now. If that fails, from what Keel read of it before,
    /// and the record says so.
    fn make_read_copy(&self, git: &Git, copy: &ReadCopy) -> Result<(String, Value)> {
        // Two runs reading the same project fetch one at a time.
        let project_lock = self.read_lock(format!("project:{}", copy.project_id));
        let _project = locked(&project_lock);
        let project = self.store.project(&copy.project_id)?;
        let mirror = self.mirror(&project.id);
        let (wanted, earlier) = match &copy.part {
            Some(part) => {
                let wanted = part_read_branch(&part.branch);
                // What Keel delivered is in the mirror, and is the part as it was then.
                (wanted.clone(), vec![wanted, part.branch.clone()])
            }
            None => (
                READ_BRANCH.to_owned(),
                vec![READ_BRANCH.to_owned(), BASE_REF.to_owned()],
            ),
        };
        let (branch, from, failed) =
            match self.fetch_for_reading(git, &project, &mirror, copy.part.as_ref(), &wanted) {
                Ok(from) => (wanted, from, None),
                Err(detail) => {
                    let Some(branch) = earlier
                        .into_iter()
                        .find(|branch| git.mirror_has(&mirror, branch))
                    else {
                        return Err(copy_failed(copy, detail));
                    };
                    (branch, "earlier", Some(detail))
                }
            };
        let commit = git
            .create_read_copy(&mirror, &branch, &copy.path)
            .map_err(|error| copy_failed(copy, error.to_string()))?;
        // That's where the earlier part's pull request is now, which Keel otherwise only learns by
        // looking. Until it did, the two would look like someone had pushed to it.
        if let (Some(part), "github") = (&copy.part, from) {
            self.store
                .patch_run(&part.run_id, &[("pr_head", Column::Text(commit.clone()))])?;
        }
        let record = json!({
            "project": project.id,
            "name": copy.name,
            "path": copy.path,
            "from": from,
            "branch": branch,
            "commit": commit,
            "part": copy.part.as_ref().map(|part| json!({
                "position": part.position,
                "runId": part.run_id,
                "branch": part.branch,
            })),
            "failed": failed,
        });
        Ok((commit, record))
    }

    /// Brings what a copy reads into the project's mirror, from where the project lives: GitHub if
    /// it has an account set, its folder if not. Where it came from, or what went wrong.
    fn fetch_for_reading(
        &self,
        git: &Git,
        project: &Project,
        mirror: &Path,
        part: Option<&EarlierPart>,
        into: &str,
    ) -> std::result::Result<&'static str, String> {
        let has_gh = locked(&self.tools).gh.is_some();
        match (&project.github_account, &project.github_repo, has_gh) {
            (Some(account), Some(repo), true) => {
                let gh = self.gh().map_err(|error| error.to_string())?;
                let token = gh.token(account).map_err(|error| error.detail)?;
                // Commits someone added to the pull request since are part of it now.
                let branch = match part {
                    Some(part) => part.branch.clone(),
                    None => {
                        gh.repo_info(&token, repo)
                            .map_err(|error| error.detail)?
                            .default_branch
                    }
                };
                let auth = RemoteAuth {
                    gh: &gh.bin,
                    token: &token,
                    path_env: &gh.path_env,
                };
                git.fetch_for_reading(
                    mirror,
                    &repo_url(&self.config.github_git_base, repo),
                    &branch,
                    into,
                    &auth,
                )
                .map_err(|error| error.to_string())?;
                Ok("github")
            }
            // A part's branch is only on GitHub, and in the mirror once Keel delivered it.
            _ if part.is_some() => {
                Err("its project has no GitHub account set, so Keel can't look for newer commits on its pull request".to_owned())
            }
            _ => {
                git.fetch_local_for_reading(Path::new(&project.path), mirror, into)
                    .map_err(|error| error.to_string())?;
                Ok("folder")
            }
        }
    }

    /// What an agent of a run in a change across projects is told, along with what Keel knows of
    /// its own project: which part it does and what was agreed, and where the other projects are.
    /// Empty for any other run. `role` is the agent's.
    pub(crate) fn joint_text(
        &self,
        run_id: &str,
        copies: &[ReadyCopy],
        role: &StepRole,
    ) -> Result<String> {
        let Some((card, change)) = self.joint_card(run_id)? else {
            return Ok(String::new());
        };
        let language = self.store.run(run_id)?.language;
        let mut text = String::new();
        let agreed = change.agreed.clone().unwrap_or_default();
        let spec = match (card.joint_position, current_spec(&change)) {
            // A run keeps the contract its part was specified with, which is what its criteria
            // say, until its part is specified again. Only its specifier reads it as it is now.
            (Some(_), Some(mut spec)) if *role != StepRole::Specifier => {
                match self.given(run_id)? {
                    Given::Recorded {
                        contract: Some(contract),
                        ..
                    } => spec.contract = contract,
                    Given::Agreed => {
                        spec.contract =
                            agreed_spec(&change).map(|s| s.contract).unwrap_or_default();
                    }
                    Given::Recorded { contract: None, .. } | Given::Nothing => {}
                }
                Some(spec)
            }
            (_, spec) => spec,
        };
        let corrected = spec.as_ref().is_some_and(|spec| {
            agreed_spec(&change).is_some_and(|agreed| agreed.contract != spec.contract)
        });
        if let (Some(here), Some(spec)) = (card.joint_position, spec) {
            let answered: Vec<GivenAnswer> = agreed
                .get("answered")
                .and_then(|answered| serde_json::from_value(answered.clone()).ok())
                .unwrap_or_default();
            let index = usize::try_from(here - 1).unwrap_or_default();
            let name = spec
                .parts
                .get(index)
                .map_or("this project", |part| part.project.as_str());
            text.push_str(&format!(
                "\n# A change across several projects\n\n\
                 This run is part {here} of {total} of a change that touches several projects. A person approved all of it at once, as it reads below. Each part is done in its own project, in order, and this one is for {name}. Do only this part.\n\n\
                 ## What was agreed\n\n{agreed}\n",
                total = spec.parts.len(),
                agreed = spec.readable(&language),
            ));
            if corrected {
                text.push_str(
                    "\nA person corrected what the parts agree on exactly after approving it. It reads above as it applies to this run, and that's what counts.\n",
                );
            }
            if !answered.is_empty() {
                text.push_str(&format!(
                    "\n## What the person answered\n\n{}\n",
                    specs::answer_lines(&answered)
                ));
            }
            let contract = spec.contract_of(name);
            if !contract.is_empty() {
                let ids: Vec<&str> = contract.iter().map(|point| point.id.as_str()).collect();
                text.push_str(&format!(
                    "\nKeel adds the points of the contract that name {name} to this part's criteria, with their ids ({}), and they're verified like the rest.",
                    ids.join(", ")
                ));
                // Only the specifier writes criteria.
                if *role == StepRole::Specifier {
                    text.push_str(" Don't write them again as criteria of your own.");
                }
                text.push('\n');
            }
            if spec.contract.len() > contract.len() {
                let which = if contract.is_empty() {
                    "\nThe points of the contract"
                } else {
                    "The other points"
                };
                text.push_str(&format!(
                    "{which} are what the other parts promise: rely on them, but they aren't verified in this part.\n"
                ));
            }
            if !spec.together.is_empty() {
                text.push_str(
                    "\nWhat only works with every part in place can't be proven from one project. Don't say any of it is tested or works: a person tries it once every part is done.\n",
                );
            }
        }
        if !copies.is_empty() {
            text.push_str("\n# The other projects\n\n");
            if agrees(&card) {
                text.push_str(
                    "This request touches other projects too. Here's a copy of each, to read, as it was when this run started. You can't change them: what changes in each is what you're working out.\n",
                );
            } else {
                text.push_str(
                    "Here's a copy of each, to read, as it was when this run started. You can't change them: each part is done in its own project.\n",
                );
            }
            for ready in copies {
                let what = match &ready.copy.part {
                    Some(part) => format!("the branch of part {}", part.position),
                    None => "its default branch".to_owned(),
                };
                let short: String = ready.commit.chars().take(7).collect();
                text.push_str(&format!(
                    "- {}, in `{}`: {what}, at commit {short}.\n",
                    ready.copy.name,
                    ready.copy.path.display(),
                ));
            }
        }
        Ok(text)
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
        match self.put_parts_on_board(run_id, &agreement, attempts) {
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
        run_id: &str,
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
        // What a person answered reaches every part, as part of what was agreed.
        let mut agreed = json!(spec);
        let answered = specs::answered_in_run(attempts);
        if !answered.is_empty() {
            agreed["answered"] = json!(answered);
        }
        // So does the request, word for word as it was agreed from: what was agreed only sums it
        // up. It's kept here, since the card where it was agreed may be deleted before a part starts.
        let request = self
            .store
            .run(run_id)
            .map_err(|error| error.to_string())?
            .request;
        agreed[ASKED] = json!(request);
        agreed[ASKED_WRITTEN] = json!(self.written_in_request(run_id));
        self.store
            .open_joint_parts(&agreement.change.id, &agreed, &parts, &spec.together)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: &str, then: &str) -> Criterion {
        Criterion {
            id: id.into(),
            when: "someone logs in".into(),
            then: then.into(),
        }
    }

    #[test]
    fn a_part_was_given_a_point_as_it_no_longer_is_when_it_changed_came_or_went() {
        let given = [point("K1", "201"), point("K2", "401"), point("K3", "409")];
        assert!(changed_points(&given, &given).is_empty());
        // K1 says something else now, K4 is new, and K2 is gone. K3 stays as it was.
        let now = [
            point("K1", "201 with `id`"),
            point("K3", "409"),
            point("K4", "429"),
        ];
        assert_eq!(changed_points(&given, &now), ["K1", "K4", "K2"]);
        // Given none, every point it has now is new to it.
        assert_eq!(changed_points(&[], &now), ["K1", "K3", "K4"]);
    }

    fn change(ids: &[&str], next: Option<i64>) -> JointChange {
        let contract: Vec<Value> = ids
            .iter()
            .map(|id| json!({ "id": id, "when": "a", "then": "b", "projects": ["valora"] }))
            .collect();
        JointChange {
            id: "c".into(),
            projects: vec![],
            agreed: Some(json!({ "summary": "s", "criteria": [], "contract": contract })),
            contract: None,
            contract_next: next,
            created_at: String::new(),
            parts: vec![],
            checks: vec![],
        }
    }

    #[test]
    fn a_new_point_takes_a_number_past_every_one_given_before() {
        assert_eq!(next_point(&change(&["K1", "K2", "K3"], None)), 4);
        assert_eq!(next_point(&change(&[], None)), 1);
        // Once a person corrected it, the count is what's kept, past points removed since.
        assert_eq!(next_point(&change(&["K1"], Some(7))), 7);
        assert_eq!(point_number("C1"), None);
        assert_eq!(point_number("K"), None);
    }
}
