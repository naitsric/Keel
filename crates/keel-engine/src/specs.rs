//! A specifier's answer: what a request will change, in words for the person who asked, and what
//! only they can decide. The limits are enforced here, because a prompt alone doesn't hold them and
//! the schema's own limits aren't reliable. Everything counts characters, since Spanish matters.

use std::collections::HashSet;
use std::sync::LazyLock;

use keel_core::{AttemptState, Reason, SpecProblem};
use keel_store::Attempt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prompts;

const MAX_SUMMARY: usize = 200;
const MAX_CRITERIA: usize = 7;
const MAX_CRITERION: usize = 200;
const MAX_LISTED: usize = 10;
const MAX_LISTED_TEXT: usize = 200;
const MAX_DECIDED: usize = 5;
const MAX_DECIDED_TEXT: usize = 140;
/// What the request decided doesn't count with what the specifier decided alone: a request can
/// settle many things at once. Past this many, the rest stay in the request, for agents to read.
const MAX_FROM_REQUEST: usize = 10;
/// Each goes into every prompt after the spec, so a longer one stays only in the request.
const MAX_REQUEST_ITEM: usize = 300;
/// What a list of what the person already decided can be called, as a heading.
const DECIDED_HEADINGS: [&str; 4] = [
    "decisiones ya tomadas",
    "decisiones tomadas",
    "decisions already made",
    "decided",
];
/// What a note after such a heading can say, each part of it between commas, for what's under it
/// to count: that the decisions are taken, or only not to ask about them. Any other note holds them
/// back, since nobody can list every way of saying they aren't settled.
const AFFIRMING: [&str; 34] = [
    "no las preguntes",
    "no preguntes",
    "no preguntar",
    "no las cuestiones",
    "sin preguntas",
    "definitivas",
    "definitivos",
    "definitiva",
    "definitivo",
    "por mí",
    "por mi",
    "ya tomadas",
    "tomadas",
    "ya decididas",
    "decididas",
    "don't ask",
    "dont ask",
    "do not ask",
    "don't ask them",
    "do not ask them",
    "don't ask about them",
    "do not ask about them",
    "don't question them",
    "do not question them",
    "no questions",
    "final",
    "by me",
    "already made",
    "already decided",
    "decided",
    "made",
    "settled",
    "taken",
    "done",
];
/// What a note can say that holds what's under it back, word for word, even next to one of those.
const HEDGES: [&str; 50] = [
    "tentative",
    "tentatively",
    "provisional",
    "provisionally",
    "not yet",
    "maybe",
    "draft",
    "drafts",
    "to confirm",
    "to be confirmed",
    "unconfirmed",
    "not confirmed",
    "not final",
    "to be decided",
    "pending",
    "proposed",
    "proposal",
    "preliminary",
    "wip",
    "tbd",
    "tbc",
    "so far",
    "todavía no",
    "todavia no",
    "aún no",
    "aun no",
    "por confirmar",
    "sin confirmar",
    "por definir",
    "por revisar",
    "pendiente",
    "pendientes",
    "propuesta",
    "propuestas",
    "no definitivas",
    "no definitivos",
    "no definitiva",
    "no definitivo",
    "tentativo",
    "tentativa",
    "tentativos",
    "tentativas",
    "provisionales",
    "quizá",
    "quizás",
    "quiza",
    "quizas",
    "tal vez",
    "borrador",
    "borradores",
];
const MAX_QUESTIONS: usize = 3;
const MAX_QUESTION_TEXT: usize = 140;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_OPTION_LABEL: usize = 60;
const MAX_PARTS: usize = 5;
const MAX_TOGETHER: usize = 5;
const MAX_CONTRACT: usize = 7;
/// After this many, the engine takes the recommended options rather than asking a third time. A
/// question with none recommended can only be answered by a person, so it gets one round more.
pub const MAX_QUESTION_ROUNDS: usize = 2;

/// `size` and `risks` are a prediction, for a workflow with steps that depend on the change. The schema
/// says what small means as well as the prompt, since any specifier gets asked for it, whatever its
/// prompt says. Its words keep clear of those the test fakes tell schemas apart by.
pub const SPEC_SCHEMA: &str = r#"{"type":"object","properties":{"size":{"type":"string","enum":["small","normal"],"description":"small only if it is understood with nothing left to ask, its risk is contained, and someone could pick the work up from just the request and the change. How much code it touches does not decide this: one changed line in permissions is not small. If in doubt, normal."},"risks":{"type":"array","items":{"type":"string","enum":["security","money","personalData","irreversibleData","sharedContracts","concurrency","environment","noRegressionTest"]}},"summary":{"type":"string"},"criteria":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"when":{"type":"string"},"then":{"type":"string"}},"required":["id","when","then"],"additionalProperties":false}},"changes":{"type":"array","items":{"type":"object","properties":{"kind":{"type":"string","enum":["adds","changes","removes"]},"text":{"type":"string"}},"required":["kind","text"],"additionalProperties":false}},"limits":{"type":"array","items":{"type":"string"}},"decided":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}},"questions":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"recommended":{"type":"boolean","description":"true for one option at most, and only when the code or the request backs it up. When only the person can know, such as an amount, a date or a business rule, none is: never make up a value."}},"required":["label","recommended"],"additionalProperties":false}}},"required":["id","text","options"],"additionalProperties":false}}},"required":["size","risks","summary","criteria","changes","limits","decided","questions"],"additionalProperties":false}"#;

/// How big a specifier expects a change to be. Only ever a claim: it can stop a run from counting as
/// small, and never makes one so by itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Size {
    Small,
    /// Also what a spec from before this was asked for reads as.
    #[default]
    Normal,
}

/// What the specifier of a change across projects answers: the same, plus what changes in each
/// project and what only works with all of them in place.
static PLAN_SCHEMA: LazyLock<String> = LazyLock::new(|| {
    let mut schema: Value = serde_json::from_str(SPEC_SCHEMA).unwrap_or_default();
    schema["properties"]["parts"] = json!({"type":"array","items":{"type":"object","properties":{"project":{"type":"string"},"text":{"type":"string"}},"required":["project","text"],"additionalProperties":false}});
    schema["properties"]["together"] = json!({"type":"array","items":{"type":"string"}});
    // No `id`: Keel numbers them.
    schema["properties"]["contract"] = json!({"type":"array","items":{"type":"object","properties":{"when":{"type":"string"},"then":{"type":"string"},"projects":{"type":"array","items":{"type":"string"}}},"required":["when","then","projects"],"additionalProperties":false}});
    if let Some(required) = schema["required"].as_array_mut() {
        required.extend([json!("parts"), json!("together"), json!("contract")]);
    }
    schema.to_string()
});

pub fn plan_schema() -> &'static str {
    &PLAN_SCHEMA
}

/// What a specifier answers, once it has passed [`judge`]. New fields need `default`: runs
/// already in the database don't have them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    #[serde(default)]
    pub size: Size,
    /// Plain text rather than a closed list on this side: one Keel doesn't know is still a risk, and
    /// refusing it here would only ask the agent to take it out.
    #[serde(default)]
    pub risks: Vec<String>,
    pub summary: String,
    pub criteria: Vec<Criterion>,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default)]
    pub limits: Vec<String>,
    #[serde(default)]
    pub decided: Vec<Decided>,
    #[serde(default)]
    pub questions: Vec<Question>,
    /// Only where a change across projects is agreed: what changes in each project, in the order
    /// it's done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
    /// Only there too: what only works with every part in place, for a person to try.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub together: Vec<String>,
    /// Only there too: what the parts have to agree on exactly. Each part that a point names gets it
    /// as a criterion of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract: Vec<ContractPoint>,
}

/// A route, a field, a type, a response code or a change of state that more than one part relies on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractPoint {
    /// K1, K2 and so on, in order. Only the engine writes it.
    #[serde(default)]
    pub id: String,
    pub when: String,
    pub then: String,
    /// The projects whose part has to keep it, by the names the agent was given.
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// The project's name, as the agent was given it.
    pub project: String,
    /// What changes there.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub when: String,
    pub then: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeKind {
    Adds,
    Changes,
    Removes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub text: String,
}

/// Who settled something that was once a question, or that the request settled. Only the engine
/// writes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Taken {
    /// A person went with the recommended option.
    Person,
    /// It would have asked a third time, so Keel took the recommended option.
    RoundsLimit,
    /// The person wrote it in the request, in a list of what they already decided.
    Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decided {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<Taken>,
    /// Where the request decided it: the assistant that wrote it for the person, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
    /// Or the project's GitHub account, for an issue written with it.
    #[serde(
        default,
        rename = "githubAccount",
        skip_serializing_if = "Option::is_none"
    )]
    pub github_account: Option<String>,
}

/// A piece of a run's request that the person wrote, or an assistant they allowed to start it, or
/// an issue written with the project's GitHub account, as Keel knew when the run started. Only these
/// can settle something for the person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Written {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
    /// An issue written with the project's GitHub account is that account's: an assistant may have
    /// written it through `gh`, and anyone who can edit it may have changed it since.
    #[serde(
        default,
        rename = "githubAccount",
        skip_serializing_if = "Option::is_none"
    )]
    pub github_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub recommended: bool,
}

/// How a person answers one question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AnswerChoice {
    /// One of the options on offer.
    Option { label: String },
    /// Their own words.
    Own { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// The question's id.
    pub question: String,
    pub choice: AnswerChoice,
    /// Whether it becomes a decision for later runs. Otherwise it stays in this run.
    #[serde(default)]
    pub remember: bool,
}

/// An answer that checked out against its question, as it goes on the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GivenAnswer {
    pub question: String,
    /// What the question said.
    pub asked: String,
    pub answer: String,
    pub recommended: bool,
    pub remember: bool,
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// How far past its limit a text may go and still be taken as it is, in hundredths of the limit.
const NEAR_MISS: usize = 15;

/// Whether a text of `length` characters is too long for `limit`. Models count characters loosely,
/// and one a few over reads the same to the person, so a near miss is taken as it is rather than
/// spending another turn of the agent on it. Only lengths: how many items a list has is exact.
fn too_long(length: usize, limit: usize) -> bool {
    length > limit + limit * NEAR_MISS / 100
}

/// These are all shown in a line or read as one, so a line break can't fake a heading anywhere.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Spec {
    fn normalize(&mut self) {
        let tidy = |text: &mut String| *text = one_line(text);
        tidy(&mut self.summary);
        for criterion in &mut self.criteria {
            tidy(&mut criterion.id);
            tidy(&mut criterion.when);
            tidy(&mut criterion.then);
        }
        // Nothing depends on how long these are, so a long one is cut rather than sent back.
        let cut = |text: &mut String| {
            tidy(text);
            if too_long(chars(text), MAX_LISTED_TEXT) {
                let kept: String = text.chars().take(MAX_LISTED_TEXT - 1).collect();
                *text = format!("{}…", kept.trim_end());
            }
        };
        for change in &mut self.changes {
            cut(&mut change.text);
        }
        for limit in &mut self.limits {
            cut(limit);
        }
        for part in &mut self.parts {
            tidy(&mut part.project);
            cut(&mut part.text);
        }
        for together in &mut self.together {
            cut(together);
        }
        for (index, point) in self.contract.iter_mut().enumerate() {
            point.id = format!("K{}", index + 1);
            tidy(&mut point.when);
            tidy(&mut point.then);
            for project in &mut point.projects {
                tidy(project);
            }
        }
        for risk in &mut self.risks {
            tidy(risk);
        }
        for decided in &mut self.decided {
            tidy(&mut decided.text);
            // Only the engine says who settled something.
            decided.taken = None;
            decided.assistant = None;
            decided.github_account = None;
        }
        for question in &mut self.questions {
            tidy(&mut question.id);
            tidy(&mut question.text);
            for option in &mut question.options {
                tidy(&mut option.label);
            }
        }
    }

    /// What breaks a limit. What the request decided, in `from_request`, has none of these.
    fn problems(&self, from_request: &RequestDecided) -> Vec<SpecProblem> {
        let mut problems = Vec::new();
        if self.summary.is_empty() {
            problems.push(SpecProblem::NoSummary);
        } else if too_long(chars(&self.summary), MAX_SUMMARY) {
            problems.push(SpecProblem::SummaryTooLong {
                chars: chars(&self.summary),
                limit: MAX_SUMMARY,
            });
        }

        if self.criteria.is_empty() {
            problems.push(SpecProblem::NoCriteria);
        } else if self.criteria.len() > MAX_CRITERIA {
            problems.push(SpecProblem::TooManyCriteria {
                count: self.criteria.len(),
                limit: MAX_CRITERIA,
            });
        }
        let mut seen = HashSet::new();
        for (index, criterion) in self.criteria.iter().enumerate() {
            let length = chars(&criterion.when) + chars(&criterion.then);
            if criterion.when.is_empty() || criterion.then.is_empty() {
                problems.push(SpecProblem::CriterionNeedsText {
                    id: criterion.id.clone(),
                });
            }
            if criterion.id.is_empty() {
                problems.push(SpecProblem::CriterionNeedsId {
                    position: index + 1,
                });
            } else if !seen.insert(criterion.id.as_str()) {
                problems.push(SpecProblem::RepeatedCriterion {
                    id: criterion.id.clone(),
                });
            }
            if too_long(length, MAX_CRITERION) {
                problems.push(SpecProblem::CriterionTooLong {
                    id: criterion.id.clone(),
                    chars: length,
                    limit: MAX_CRITERION,
                });
            }
        }

        if self.changes.len() > MAX_LISTED {
            problems.push(SpecProblem::TooManyChanges {
                count: self.changes.len(),
                limit: MAX_LISTED,
            });
        }
        if self.limits.len() > MAX_LISTED {
            problems.push(SpecProblem::TooManyLimits {
                count: self.limits.len(),
                limit: MAX_LISTED,
            });
        }
        if self.parts.len() > MAX_PARTS {
            problems.push(SpecProblem::TooManyParts {
                count: self.parts.len(),
                limit: MAX_PARTS,
            });
        }
        if self.together.len() > MAX_TOGETHER {
            problems.push(SpecProblem::TooManyTogether {
                count: self.together.len(),
                limit: MAX_TOGETHER,
            });
        }

        // A copy of what the request decided isn't the spec's own, nor one of what Keel left in the
        // request, but the agent counts it in its numbers, so that's what it's told.
        let own: Vec<(usize, &Decided)> = self
            .decided
            .iter()
            .enumerate()
            .filter(|(_, decided)| !from_request.has(&decided.text))
            .collect();
        if own.len() > MAX_DECIDED {
            problems.push(SpecProblem::TooManyDecided {
                count: own.len(),
                limit: MAX_DECIDED,
            });
        }
        for (index, decided) in own {
            if too_long(chars(&decided.text), MAX_DECIDED_TEXT) {
                problems.push(SpecProblem::DecidedTooLong {
                    position: index + 1,
                    chars: chars(&decided.text),
                    limit: MAX_DECIDED_TEXT,
                });
            }
        }

        if self.questions.len() > MAX_QUESTIONS {
            problems.push(SpecProblem::TooManyQuestions {
                count: self.questions.len(),
                limit: MAX_QUESTIONS,
            });
        }
        let mut seen = HashSet::new();
        for (index, question) in self.questions.iter().enumerate() {
            problems.extend(Self::question_problems(question, index, &mut seen));
        }

        if self.leaves_something_unresolved() {
            problems.push(SpecProblem::LeftUnresolved);
        }
        problems
    }

    /// What an agreement across these projects gets wrong about its parts: it needs one at least,
    /// each in one of them, and never two in the same one.
    fn part_problems(&self, projects: &[String]) -> Vec<SpecProblem> {
        let mut problems = Vec::new();
        if self.parts.is_empty() {
            problems.push(SpecProblem::NoParts);
        }
        let mut seen = HashSet::new();
        for part in &self.parts {
            if !projects.contains(&part.project) {
                problems.push(SpecProblem::UnknownProject {
                    project: part.project.clone(),
                });
            } else if !seen.insert(part.project.as_str()) {
                problems.push(SpecProblem::RepeatedPart {
                    project: part.project.clone(),
                });
            }
        }
        problems
    }

    /// What its contract gets wrong: like a criterion, each point says when and then within the
    /// same limit, and it names only projects it was given, one with a part at least. Each problem
    /// says where the point is, since its id is Keel's and the agent never wrote it.
    fn contract_problems(&self, projects: &[String]) -> Vec<SpecProblem> {
        let mut problems = Vec::new();
        if self.contract.len() > MAX_CONTRACT {
            problems.push(SpecProblem::TooManyContract {
                count: self.contract.len(),
                limit: MAX_CONTRACT,
            });
        }
        for (index, point) in self.contract.iter().enumerate() {
            let id = || point.id.clone();
            let position = index + 1;
            if point.when.is_empty() || point.then.is_empty() {
                problems.push(SpecProblem::ContractNeedsText { id: id(), position });
            }
            let length = chars(&point.when) + chars(&point.then);
            if too_long(length, MAX_CRITERION) {
                problems.push(SpecProblem::ContractTooLong {
                    id: id(),
                    position,
                    chars: length,
                    limit: MAX_CRITERION,
                });
            }
            let unknown: Vec<&String> = point
                .projects
                .iter()
                .filter(|project| !projects.contains(project))
                .collect();
            for project in &unknown {
                problems.push(SpecProblem::ContractUnknownProject {
                    id: id(),
                    position,
                    project: (*project).clone(),
                });
            }
            let has_a_part = self
                .parts
                .iter()
                .any(|part| point.projects.contains(&part.project));
            if unknown.is_empty() && !has_a_part {
                problems.push(SpecProblem::ContractWithoutPart { id: id(), position });
            }
        }
        problems
    }

    /// The contract as a person corrected it once it was agreed: `points` replace it, in their
    /// order and with their ids already given, tidied as an agent's are and checked by the same
    /// rules. A point may name only the `known` projects, which were among those the agent was
    /// given, and one of them with a part.
    pub fn corrected_contract(
        &self,
        points: Vec<ContractPoint>,
        known: &[String],
    ) -> Result<Vec<ContractPoint>, Vec<SpecProblem>> {
        let mut spec = self.clone();
        spec.contract = points
            .into_iter()
            .map(|point| {
                let mut projects: Vec<String> = Vec::new();
                for project in &point.projects {
                    let project = one_line(project);
                    if !projects.contains(&project) {
                        projects.push(project);
                    }
                }
                ContractPoint {
                    id: point.id,
                    when: one_line(&point.when),
                    then: one_line(&point.then),
                    projects,
                }
            })
            .collect();
        let problems = spec.contract_problems(known);
        if problems.is_empty() {
            Ok(spec.contract)
        } else {
            Err(problems)
        }
    }

    /// The points of the contract a project's part has to keep, as its criteria.
    pub fn contract_of(&self, project: &str) -> Vec<Criterion> {
        self.contract
            .iter()
            .filter(|point| point.projects.iter().any(|named| named == project))
            .map(|point| Criterion {
                id: point.id.clone(),
                when: point.when.clone(),
                then: point.then.clone(),
            })
            .collect()
    }

    fn question_problems<'a>(
        question: &'a Question,
        index: usize,
        seen: &mut HashSet<&'a str>,
    ) -> Vec<SpecProblem> {
        let mut problems = Vec::new();
        let id = || question.id.clone();
        if question.id.is_empty() {
            problems.push(SpecProblem::QuestionNeedsId {
                position: index + 1,
            });
        } else if !seen.insert(question.id.as_str()) {
            problems.push(SpecProblem::RepeatedQuestion { id: id() });
        }
        if question.text.is_empty() {
            problems.push(SpecProblem::QuestionNeedsText { id: id() });
        }
        if too_long(chars(&question.text), MAX_QUESTION_TEXT) {
            problems.push(SpecProblem::QuestionTooLong {
                id: id(),
                chars: chars(&question.text),
                limit: MAX_QUESTION_TEXT,
            });
        }
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&question.options.len()) {
            problems.push(SpecProblem::WrongNumberOfOptions {
                id: id(),
                count: question.options.len(),
                min: MIN_OPTIONS,
                max: MAX_OPTIONS,
            });
        }
        let mut labels = HashSet::new();
        for option in &question.options {
            if too_long(chars(&option.label), MAX_OPTION_LABEL) {
                problems.push(SpecProblem::OptionTooLong {
                    id: id(),
                    chars: chars(&option.label),
                    limit: MAX_OPTION_LABEL,
                });
            }
            // An answer names its option, so two with one name can't be told apart.
            if !labels.insert(option.label.as_str()) || option.label.is_empty() {
                problems.push(SpecProblem::RepeatedOption { id: id() });
            }
        }
        let recommended = question.options.iter().filter(|o| o.recommended).count();
        if recommended > 1 {
            problems.push(SpecProblem::TooManyRecommended {
                id: id(),
                count: recommended,
            });
        }
        problems
    }

    fn leaves_something_unresolved(&self) -> bool {
        fn any_string(value: &Value) -> bool {
            match value {
                Value::String(text) => text.to_uppercase().contains("[TBC]"),
                Value::Array(items) => items.iter().any(any_string),
                Value::Object(fields) => fields.values().any(any_string),
                _ => false,
            }
        }
        serde_json::to_value(self).is_ok_and(|value| any_string(&value))
    }

    /// Whether every question recommends an option, so that Keel could take them all.
    fn all_recommend(&self) -> bool {
        self.questions
            .iter()
            .all(|question| question.options.iter().any(|o| o.recommended))
    }

    /// Moves the questions into what's decided, going with the recommended options. Each one keeps
    /// its criteria: those were written as if the person picks that option. Only for questions that
    /// all recommend one: one with none would be dropped.
    pub fn take_recommended(&mut self, taken: Taken) -> Vec<Value> {
        let mut settled = Vec::new();
        for question in std::mem::take(&mut self.questions) {
            let Some(option) = question.options.iter().find(|o| o.recommended) else {
                continue;
            };
            self.decided.push(Decided {
                text: decided_line(&question.text, &option.label),
                taken: Some(taken),
                assistant: None,
                github_account: None,
            });
            settled.push(json!({
                "question": question.id,
                "asked": question.text,
                "answer": option.label,
            }));
        }
        settled
    }

    /// For someone who reads it outside Keel, in the run's language.
    pub fn readable(&self, language: &str) -> String {
        let es = language.starts_with("es");
        let say = |en: &'static str, spanish: &'static str| if es { spanish } else { en };
        let mut text = self.summary.clone();
        let mut section = |title: &str, lines: Vec<String>| {
            if lines.is_empty() {
                return;
            }
            text.push_str(&format!("\n\n**{title}**\n"));
            for line in lines {
                text.push_str(&format!("\n- {line}"));
            }
        };
        section(
            say("What has to be true", "Lo que tiene que cumplirse"),
            self.criteria
                .iter()
                .map(|c| format!("{}: {} → {}", c.id, c.when, c.then))
                .collect(),
        );
        section(
            say("What changes", "Qué cambia"),
            self.changes
                .iter()
                .map(|change| {
                    let kind = match change.kind {
                        ChangeKind::Adds => say("Adds", "Agrega"),
                        ChangeKind::Changes => say("Changes", "Cambia"),
                        ChangeKind::Removes => say("Removes", "Quita"),
                    };
                    format!("{kind}: {}", change.text)
                })
                .collect(),
        );
        section(
            say("What stays as it is", "Qué se queda igual"),
            self.limits.clone(),
        );
        let from_request = |decided: &&Decided| decided.taken == Some(Taken::Request);
        section(
            say("Decided in the request", "Lo que se decidió en el pedido"),
            self.decided
                .iter()
                .filter(from_request)
                .map(
                    |decided| match (&decided.assistant, &decided.github_account) {
                        (Some(assistant), _) if es => {
                            format!("{} (lo escribió {assistant})", decided.text)
                        }
                        (Some(assistant), _) => format!("{} ({assistant} wrote it)", decided.text),
                        (None, Some(_)) if es => format!(
                            "{} (lo escribió la cuenta del proyecto en GitHub)",
                            decided.text
                        ),
                        (None, Some(_)) => format!(
                            "{} (written with the project's GitHub account)",
                            decided.text
                        ),
                        (None, None) => decided.text.clone(),
                    },
                )
                .collect(),
        );
        section(
            say(
                "What was decided along the way",
                "Lo que se decidió sobre la marcha",
            ),
            self.decided
                .iter()
                .filter(|decided| !from_request(decided))
                .map(|decided| match decided.taken {
                    Some(Taken::RoundsLimit) => format!(
                        "{} {}",
                        decided.text,
                        say(
                            "(Keel took the recommended option when the rounds of questions ran out)",
                            "(Keel tomó la opción recomendada al acabarse las rondas de preguntas)",
                        )
                    ),
                    _ => decided.text.clone(),
                })
                .collect(),
        );
        section(
            say(
                "What changes in each project",
                "Qué cambia en cada proyecto",
            ),
            self.parts
                .iter()
                .enumerate()
                .map(|(index, part)| format!("{}. {}: {}", index + 1, part.project, part.text))
                .collect(),
        );
        section(
            say("To try together", "Para probar juntos"),
            self.together.clone(),
        );
        section(
            say(
                "What the parts agree on exactly",
                "Lo que las partes acuerdan entre sí",
            ),
            self.contract
                .iter()
                .map(|point| {
                    format!(
                        "{} ({}): {} → {}",
                        point.id,
                        point.projects.join(", "),
                        point.when,
                        point.then
                    )
                })
                .collect(),
        );
        text
    }
}

/// A heading's words, without bold or italics around them or a colon, and each note in brackets
/// after them.
fn heading_name(heading: &str) -> (&str, Vec<&str>) {
    let mut name = heading.trim().trim_start_matches(['*', '_']);
    let mut notes = Vec::new();
    loop {
        let before = name;
        name = name.trim_end().trim_end_matches([':', '*', '_']).trim_end();
        if name.ends_with(')')
            && let Some(open) = name.rfind('(')
        {
            notes.push(&name[open + 1..name.len() - 1]);
            name = &name[..open];
        }
        if name == before {
            return (name, notes);
        }
    }
}

/// Whether a note leaves what's under the heading counting: each part of it says the decisions are
/// taken, or only not to ask about them, as "(no las preguntes)" does, and none of it hedges.
fn affirms(note: &str) -> bool {
    let note = note.to_lowercase().replace(['’', '‘'], "'");
    let words: Vec<&str> = note
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|word| !word.is_empty())
        .collect();
    let hedged = HEDGES.iter().any(|hedge| {
        let hedge: Vec<&str> = hedge.split(' ').collect();
        words
            .windows(hedge.len())
            .any(|window| window == hedge.as_slice())
    });
    let parts: Vec<String> = note
        .split([',', ';'])
        .map(|part| one_line(part.trim().trim_end_matches(['.', '!'])))
        .collect();
    !hedged && parts.iter().all(|part| AFFIRMING.contains(&part.as_str()))
}

/// Whether a heading names a list of what was already decided. One with a note that doesn't say
/// they're taken, such as "Decided (tentative)", doesn't.
fn names_what_was_decided(heading: &str) -> bool {
    let (name, notes) = heading_name(heading);
    DECIDED_HEADINGS
        .iter()
        .any(|heading| name.eq_ignore_ascii_case(heading))
        && notes.into_iter().all(affirms)
}

/// What a heading with `#` says, if the line is one. As in Markdown, that's one to six `#` and then
/// a space or nothing, so `#131` is a word. Any `#` at its end, after a space, only close it.
fn atx_heading(trimmed: &str) -> Option<&str> {
    let rest = trimmed.trim_start_matches('#');
    let marks = trimmed.len() - rest.len();
    if !(1..=6).contains(&marks) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let rest = rest.trim();
    let open = rest.trim_end_matches('#');
    Some(if open.is_empty() || open.ends_with([' ', '\t']) {
        open.trim_end()
    } else {
        rest
    })
}

/// A line of only `=`, or only `-`, which makes the text right above it a heading.
fn underline(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && (trimmed.bytes().all(|c| c == b'=') || trimmed.bytes().all(|c| c == b'-'))
}

/// How a list item is marked.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    /// `-`, `*` or `+`.
    Bullet,
    /// A number and `.` or `)`.
    Numbered { from_one: bool },
}

/// A list item a line starts, if it starts one.
struct Marked<'a> {
    said: &'a str,
    mark: Mark,
    /// How far a line has to be indented to be part of it, as Markdown counts it: to where its
    /// words start, or one past its mark when there are none, or they're code.
    content: usize,
}

/// What a list item says, if the line is one, how it's marked, and where its words start.
fn list_item(line: &str) -> Option<Marked<'_>> {
    let mark_at = indent(line);
    let line = line.trim_start();
    let (rest, mark) = match line.strip_prefix(['-', '*', '+']) {
        Some(rest) => (rest, Mark::Bullet),
        None => {
            let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if !(1..=9).contains(&digits) {
                return None;
            }
            let from_one = line[..digits].parse::<u32>() == Ok(1);
            (
                line[digits..].strip_prefix(['.', ')'])?,
                Mark::Numbered { from_one },
            )
        }
    };
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let after_mark = mark_at + (line.len() - rest.len());
    let words_at = columns(after_mark, rest);
    let content = if rest.trim().is_empty() || words_at - after_mark > 4 {
        after_mark + 1
    } else {
        words_at
    };
    Some(Marked {
        said: rest.trim(),
        mark,
        content,
    })
}

/// Whether a line that's marked as a list item starts one, as Markdown reads it. Right under a
/// line of text, only one that says something can, and only a bullet or a number 1: any other goes
/// on with that line, as a sentence wrapped before a number does. Right under an item of its kind,
/// it's the next one, and at the start of a block it starts a list.
fn starts_item(said: &str, mark: Mark, starts_block: bool, item: Option<&Item>) -> bool {
    let numbered = matches!(mark, Mark::Numbered { .. });
    starts_block
        || item.is_some_and(|item| item.numbered == numbered)
        || !said.is_empty() && mark != Mark::Numbered { from_one: false }
}

/// How far a line is indented, with tabs stopping every four columns, as in Markdown.
fn indent(line: &str) -> usize {
    columns(0, line)
}

/// The column the space at the start of a text reaches, starting from this one.
fn columns(from: usize, text: &str) -> usize {
    text.chars()
        .take_while(|c| c.is_whitespace())
        .fold(
            from,
            |at, c| if c == '\t' { at + 4 - at % 4 } else { at + 1 },
        )
}

/// The fence a line opens a code block with, if it opens one: its character and length. However
/// indented, since one under a list item is indented with it. As in Markdown, backticks with more
/// backticks after them on the line are code within it, and open nothing.
fn fence(line: &str) -> Option<(char, usize)> {
    let line = line.trim_start();
    let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = line.chars().take_while(|c| *c == mark).count();
    let words = &line[length..];
    (length >= 3 && !(mark == '`' && words.contains('`'))).then_some((mark, length))
}

/// Whether a line ends a code block opened with this fence: only the same character, as many times
/// or more, and spaces, indented at most three past where the fence's block starts.
fn closes(line: &str, Fence { mark, length, at }: Fence) -> bool {
    let trimmed = line.trim();
    indent(line) <= at + 3
        && trimmed.chars().all(|c| c == mark)
        && trimmed.chars().count() >= length
}

/// A code block's fence: its character, how many, and where the block it's in starts, from which
/// it can be indented at most three, as in Markdown. Indented more, it's a line of code.
#[derive(Clone, Copy)]
struct Fence {
    mark: char,
    length: usize,
    at: usize,
}

/// A line of only `-`, `*` or `_`, three or more, which breaks one part of a text from the next.
fn thematic_break(trimmed: &str) -> bool {
    let mut marks = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = marks.next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    let rest: Vec<char> = marks.collect();
    rest.len() >= 2 && rest.iter().all(|c| *c == first)
}

/// Whether an item's words may not be what it says: a comment in them can hide part of it, or Keel
/// can misread where one ends, and struck-through words may have been taken back.
fn unsure(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["<!--", "~~", "<s>", "<del>", "<strike>"]
        .iter()
        .any(|mark| lower.contains(mark))
}

/// What a task that's done says, without its box: `[x]` and a space.
fn done_task(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("[x]")
        .or_else(|| text.strip_prefix("[X]"))?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// A list item being read.
struct Item {
    text: String,
    /// A task still open, which isn't decided.
    open_task: bool,
    /// Marked with a number rather than a bullet.
    numbered: bool,
    /// How far a line has to be indented to be part of it.
    content: usize,
}

/// A list of what was decided, as it's read item by item.
#[derive(Default)]
struct DecidedList {
    decided: Vec<String>,
    /// Under a heading that says what follows was decided, and not past the list yet.
    open: bool,
    item: Option<Item>,
    /// A blank line came after the item. It's whole, unless what comes next is still part of it.
    after_blank: bool,
    /// A comment on lines of its own came after the item, not indented as its words. It says
    /// nothing, but Markdown ends the item there, so nothing after it goes on with it.
    after_comment: bool,
}

impl DecidedList {
    /// Whether a line this indented is part of the item being read.
    fn within(&self, depth: usize) -> bool {
        self.item.as_ref().is_some_and(|item| depth >= item.content)
    }

    /// Keeps the item being read, which is whole.
    fn end_item(&mut self) {
        (self.after_blank, self.after_comment) = (false, false);
        if let Some(item) = self.item.take() {
            let text = one_line(&item.text);
            let text = done_task(&text).unwrap_or(&text);
            // Markup with no words, such as `[x]` or `**`, says nothing, as an empty template doesn't.
            let says = text.chars().any(char::is_alphanumeric);
            let kept = !item.open_task && says && !unsure(text);
            if kept && !self.decided.iter().any(|known| known == text) {
                self.decided.push(text.to_owned());
            }
        }
    }

    /// Leaves out the item being read: something cut it short, or may have, and part of a
    /// decision can say the opposite of the whole.
    fn drop_item(&mut self) {
        (self.item, self.after_blank, self.after_comment) = (None, false, false);
    }

    fn end(&mut self) {
        self.end_item();
        self.open = false;
    }
}

/// What a person settled in so many words in a text: each item of a list under a heading such as
/// "Decided" or "Decisiones ya tomadas", on one line, without the box of a task that's done. It's
/// read as Markdown reads it, where that leaves no doubt: a line indented as an item's words, an
/// item under it among them, goes on with it, and one less indented is the next. The list ends at
/// a blank line, a heading or a break. Keel shows a request as plain text, so whatever might be the
/// rest of an item without being indented as its words, or might cut it short, drops the item: a
/// line that isn't, which also ends the list, as it may be a label over what follows; code, a
/// quote or a tag right under it or in it; more of it after a blank line; or a comment in it, or
/// words after one right under it. A comment on lines of its own between items, as templates
/// have, says nothing. A task still open isn't decided, nor are struck-through words, nor is
/// anything in code, a quote or a comment: someone else may be speaking there.
pub(crate) fn decided_in(text: &str) -> Vec<String> {
    let mut list = DecidedList::default();
    let mut code: Option<Fence> = None;
    let mut comment = false;
    // Whether this line starts a block, as after a blank line or a heading, rather than going on
    // with a paragraph. A line of text names a list only there.
    let mut block_start = true;
    // Whether the line before was text outside a list's items, which a line of `=` or `-` makes a
    // heading.
    let mut paragraph = false;
    for line in text.lines() {
        if let Some(opened) = code {
            if closes(line, opened) {
                (code, block_start) = (None, true);
            }
            continue;
        }
        if comment {
            if let Some(end) = line.find("-->") {
                comment = false;
                // Words after it, on its line, may be the rest of the item above.
                if !line[end + 3..].trim().is_empty() {
                    list.drop_item();
                    list.end();
                }
            }
            continue;
        }
        let trimmed = line.trim();
        let depth = indent(line);
        if let Some(opened) = trimmed.strip_prefix("<!--") {
            let after = opened.find("-->").map(|end| opened[end + 3..].trim());
            comment = after.is_none();
            if list.within(depth) {
                // In an item, it may hide part of what it says. After a blank line, it's more of
                // the item, which drops it too.
                if list.after_blank {
                    list.drop_item();
                    list.end();
                } else if let Some(item) = &mut list.item {
                    item.text.push(' ');
                    item.text.push_str(trimmed);
                }
            } else if after == Some("") || after.is_none() {
                // On lines of its own, between items or before the list, it says nothing, unless
                // words follow where it ends.
                list.after_comment |= list.item.is_some();
            } else {
                // Words after it may be the rest of the item above, or a label over what follows.
                list.drop_item();
                list.end();
            }
            continue;
        }
        let under_text = std::mem::take(&mut paragraph);
        if trimmed.is_empty() {
            // Between a heading and its list it's fine. After an item, the list is over.
            list.after_blank |= list.item.is_some();
            block_start = true;
            continue;
        }
        if std::mem::take(&mut list.after_blank) {
            if list.within(depth) {
                list.drop_item();
            }
            list.end();
        }
        // After a comment Markdown ends the item at, more of it would be read as something else.
        if std::mem::take(&mut list.after_comment) && list.within(depth) {
            list.drop_item();
        }
        let within = list.within(depth);
        // Code, a quote or a tag on its own line, such as one that closes an issue, is never part
        // of an item. Right under one, it may be what the item goes on to say.
        let tag = trimmed.starts_with('<') && trimmed.ends_with('>');
        let block_at = match &list.item {
            Some(item) if within => item.content,
            _ => 0,
        };
        if let Some((mark, length)) = fence(line).filter(|_| depth <= block_at + 3) {
            list.drop_item();
            list.end();
            code = Some(Fence {
                mark,
                length,
                at: block_at,
            });
            continue;
        }
        if trimmed.starts_with('>') || tag {
            list.drop_item();
            list.end();
            block_start = false;
            continue;
        }
        // The text above is a heading. If it named the list, the list is still open, and any other
        // text has ended it already.
        if under_text && depth < 4 && underline(trimmed) {
            block_start = true;
            continue;
        }
        let starts_block = std::mem::replace(&mut block_start, false);
        let heading = atx_heading(trimmed);
        if thematic_break(trimmed) || within && heading.is_some() {
            // In an item it cuts it short. After one, the item was whole.
            if within {
                list.drop_item();
            }
            list.end();
            block_start = true;
        } else if within {
            if let Some(item) = &mut list.item {
                item.text.push(' ');
                item.text.push_str(trimmed);
            }
        } else if let Some(marked) = list_item(line).filter(|marked| {
            depth < 4 && starts_item(marked.said, marked.mark, starts_block, list.item.as_ref())
        }) {
            // An item whose words start a comment that doesn't end on its line opens one, which
            // goes on over the lines after it.
            comment = marked
                .said
                .strip_prefix("<!--")
                .is_some_and(|opened| !opened.contains("-->"));
            if list.open {
                list.end_item();
                list.item = Some(Item {
                    text: marked.said.to_owned(),
                    open_task: marked.said.starts_with("[ ]"),
                    numbered: matches!(marked.mark, Mark::Numbered { .. }),
                    content: marked.content,
                });
            }
        } else if let Some(heading) = heading.filter(|_| depth < 4) {
            list.end();
            (list.open, block_start) = (names_what_was_decided(heading), true);
        } else if list.item.is_some() || depth >= 4 {
            // Under an item, a line not indented as its words may be the rest of it, as Markdown
            // reads it, or a label such as "Pending:" over what follows. Anywhere else, this deep
            // it's code.
            list.drop_item();
            list.end();
        } else if starts_block && names_what_was_decided(trimmed) {
            list.end();
            (list.open, paragraph) = (true, true);
        } else {
            list.end();
            paragraph = true;
        }
    }
    list.end();
    list.decided
}

/// What the lists of what was already decided in a request say: what Keel takes into `decided`,
/// and what it leaves in the request, past its limits, for agents to read there.
#[derive(Debug, Default)]
pub(crate) struct RequestDecided {
    pub(crate) taken: Vec<Decided>,
    pub(crate) left: Vec<String>,
}

impl RequestDecided {
    /// Whether a line of `decided` says one of them, word for word, so it's the request's.
    fn has(&self, text: &str) -> bool {
        self.taken.iter().any(|item| item.text == text) || self.left.iter().any(|item| item == text)
    }
}

/// What the request decided, as lines of `decided` that say so and who wrote them. Where two
/// pieces say the same, it's the first one's. An item too long to carry into every prompt, or one
/// past the first ten, stays only in the request.
pub(crate) fn from_request(written: &[Written]) -> RequestDecided {
    let mut found = RequestDecided::default();
    for piece in written {
        for text in decided_in(&piece.text) {
            if found.has(&text) {
                continue;
            }
            if chars(&text) > MAX_REQUEST_ITEM || found.taken.len() == MAX_FROM_REQUEST {
                found.left.push(text);
            } else {
                found.taken.push(Decided {
                    text,
                    taken: Some(Taken::Request),
                    assistant: piece.assistant.clone(),
                    github_account: piece.github_account.clone(),
                });
            }
        }
    }
    found
}

/// A settled question, as a line of what's decided: "If whole is 0, what should happen? Show an error".
fn decided_line(question: &str, answer: &str) -> String {
    let question = question.trim_end();
    if question.ends_with(['?', ':']) {
        format!("{question} {answer}")
    } else {
        format!("{question}: {answer}")
    }
}

/// Reads a specifier's structured answer and checks it against the limits. Anything an agent added
/// beyond the structure is dropped here, so it can't pass as something the engine wrote.
///
/// `projects` is where a change across projects is agreed, by the names the agent was given. Only
/// then does a spec have parts: anywhere else they're dropped, like anything else not asked for.
/// `written` is what the person wrote in the request, where Keel finds what they already decided.
fn validate_spec_for(
    output: &Value,
    projects: Option<&[String]>,
    written: &[Written],
) -> Result<Spec, Vec<SpecProblem>> {
    let mut spec: Spec =
        serde_json::from_value(output.clone()).map_err(|_| vec![SpecProblem::Unreadable])?;
    spec.normalize();
    // What the request decided, Keel adds after the limits, which are for what the spec decided.
    // What the spec copied of it is the request's, and not its own.
    let from_request = from_request(written);
    let mut problems = spec.problems(&from_request);
    match projects {
        Some(projects) => {
            problems.extend(spec.part_problems(projects));
            problems.extend(spec.contract_problems(projects));
        }
        None => {
            spec.parts.clear();
            spec.together.clear();
            spec.contract.clear();
        }
    }
    if problems.is_empty() {
        spec.decided.retain(|own| !from_request.has(&own.text));
        spec.decided.splice(0..0, from_request.taken);
        Ok(spec)
    } else {
        Err(problems)
    }
}

/// The spec a specifier's attempt holds, if it holds one. An old `spec` step that's a plain worker
/// doesn't, and is read from what it wrote instead.
pub fn of_attempt(attempt: &Attempt) -> Option<Spec> {
    if attempt.role != "specifier" {
        return None;
    }
    serde_json::from_value(attempt.output.clone()?).ok()
}

/// What a person answered on this attempt, if it asked anything.
pub fn answered_on(attempt: &Attempt) -> Vec<GivenAnswer> {
    // The engine only writes this on a specifier that succeeded. What any other agent answered is
    // kept as it came, and could have a key with this name: that's nobody's answer.
    if attempt.role != "specifier" || attempt.state != AttemptState::Succeeded.as_str() {
        return Vec::new();
    }
    attempt
        .output
        .as_ref()
        .and_then(|output| output.get("answered"))
        .and_then(|answered| serde_json::from_value(answered.clone()).ok())
        .unwrap_or_default()
}

/// Everything a person answered in this run, oldest first.
pub fn answered_in_run(attempts: &[Attempt]) -> Vec<GivenAnswer> {
    attempts.iter().flat_map(answered_on).collect()
}

/// What to do with a specifier's result.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Nothing to ask. This is what to keep.
    Ready(Value),
    /// A person has to answer these.
    Ask {
        output: Value,
        questions: Vec<Question>,
    },
    /// It would have asked again after `rounds`. The engine took the recommended options.
    TookRecommended {
        output: Value,
        settled: Vec<Value>,
        rounds: usize,
    },
    /// It broke a limit, and can have one try at fixing it.
    Correct(Vec<SpecProblem>),
    /// It broke a limit on the try it had at fixing one.
    Stop(Vec<SpecProblem>),
    /// It still asked something with no recommended option after `rounds`, the extra one included.
    /// The output is kept, so a person can read what it wanted to know.
    KeptAsking { output: Value, rounds: usize },
}

/// Whether a criterion as an agent wrote it has the id of one of these points.
fn is_one_of(criterion: &Value, contract: &[Criterion]) -> bool {
    let id = criterion.get("id").and_then(Value::as_str).map(one_line);
    contract
        .iter()
        .any(|point| id.as_deref() == Some(point.id.as_str()))
}

/// The points of the contract a part's specifier also wrote, in words of its own, which give way to
/// what was agreed. One it copied as it was agreed, but for spacing, said nothing different.
pub fn replaced_by_contract(output: Option<&Value>, contract: &[Criterion]) -> Vec<String> {
    let mut replaced: Vec<String> = Vec::new();
    let written = output
        .and_then(|output| output.get("criteria"))
        .and_then(Value::as_array);
    for criterion in written.into_iter().flatten() {
        let text = |field: &str| criterion.get(field).and_then(Value::as_str).map(one_line);
        let Some(point) = text("id").and_then(|id| contract.iter().find(|point| point.id == id))
        else {
            continue;
        };
        let same = text("when").as_deref() == Some(point.when.as_str())
            && text("then").as_deref() == Some(point.then.as_str());
        if !same && !replaced.contains(&point.id) {
            replaced.push(point.id.clone());
        }
    }
    replaced
}

/// [`judge`] for one part of a change across projects: the points of the contract it has to keep
/// join its criteria, in the agreement's words and with their ids, so they're verified like any
/// other. The engine adds them, since an agent could leave one out. One the specifier wrote with
/// the same id gives way before its limits are counted: they're only for what it wrote itself, and
/// it needs none of its own if the points say it all.
pub fn judge_part(
    output: Option<&Value>,
    written: &[Written],
    after_correction: bool,
    rounds_answered: usize,
    contract: &[Criterion],
) -> Verdict {
    if contract.is_empty() {
        return judge(output, written, after_correction, rounds_answered);
    }
    let points = || contract.iter().map(|point| json!(point));
    let mut own = output.cloned();
    // With none of its own left, the points stand in for them and are judged with the rest.
    // Otherwise they join once it's judged, outside its limits.
    let only_points = match own.as_mut().and_then(criteria_mut) {
        Some(criteria) => {
            criteria.retain(|criterion| !is_one_of(criterion, contract));
            let none_left = criteria.is_empty();
            if none_left {
                criteria.extend(points());
            }
            none_left
        }
        None => false,
    };
    let mut verdict = judge(own.as_ref(), written, after_correction, rounds_answered);
    if !only_points && let Some(criteria) = verdict.output_mut().and_then(criteria_mut) {
        criteria.extend(points());
    }
    verdict
}

fn criteria_mut(output: &mut Value) -> Option<&mut Vec<Value>> {
    output.get_mut("criteria").and_then(Value::as_array_mut)
}

impl Verdict {
    /// The answer as it's kept. None when it isn't: it has to be fixed, or it stops the step.
    pub fn output(&self) -> Option<&Value> {
        match self {
            Self::Ready(output)
            | Self::Ask { output, .. }
            | Self::TookRecommended { output, .. }
            | Self::KeptAsking { output, .. } => Some(output),
            Self::Correct(_) | Self::Stop(_) => None,
        }
    }

    fn output_mut(&mut self) -> Option<&mut Value> {
        match self {
            Self::Ready(output)
            | Self::Ask { output, .. }
            | Self::TookRecommended { output, .. }
            | Self::KeptAsking { output, .. } => Some(output),
            Self::Correct(_) | Self::Stop(_) => None,
        }
    }
}

/// `after_correction` is whether this result was itself the fix for a broken limit, and
/// `rounds_answered` how many rounds of questions a person has already answered. They're separate
/// on purpose: fixing a limit doesn't use up the person's chance to be asked, and being asked doesn't
/// use up the fix a later result gets.
///
/// Once the rounds run out, Keel only answers for the person when every question recommends an
/// option. A question with none goes to the person once more, and asking past that stops the step,
/// so it can't go on asking forever and Keel never makes up an answer.
///
/// `written` is the run's request as Keel knows who wrote each piece: the person, or an assistant
/// they allowed. Each item of a list of what a piece already decided goes first in `decided`, as
/// the request's and in its words, and a copy the spec made of one is dropped. Nothing else can be
/// the request's.
pub fn judge(
    output: Option<&Value>,
    written: &[Written],
    after_correction: bool,
    rounds_answered: usize,
) -> Verdict {
    judge_for(output, written, after_correction, rounds_answered, None)
}

/// [`judge`] where a change across these projects is agreed. Its parts are checked before anything
/// is asked: once a person answers with every recommended option, nothing judges it again.
pub fn judge_agreement(
    output: Option<&Value>,
    written: &[Written],
    after_correction: bool,
    rounds_answered: usize,
    projects: &[String],
) -> Verdict {
    judge_for(
        output,
        written,
        after_correction,
        rounds_answered,
        Some(projects),
    )
}

fn judge_for(
    output: Option<&Value>,
    written: &[Written],
    after_correction: bool,
    rounds_answered: usize,
    projects: Option<&[String]>,
) -> Verdict {
    let mut spec = match output.map(|output| validate_spec_for(output, projects, written)) {
        Some(Ok(spec)) => spec,
        Some(Err(problems)) if after_correction => return Verdict::Stop(problems),
        Some(Err(problems)) => return Verdict::Correct(problems),
        None if after_correction => return Verdict::Stop(vec![SpecProblem::Unreadable]),
        None => return Verdict::Correct(vec![SpecProblem::Unreadable]),
    };
    if spec.questions.is_empty() {
        return Verdict::Ready(json!(spec));
    }
    let only_a_person = !spec.all_recommend();
    if rounds_answered < MAX_QUESTION_ROUNDS
        || only_a_person && rounds_answered == MAX_QUESTION_ROUNDS
    {
        return Verdict::Ask {
            output: json!(spec),
            questions: spec.questions,
        };
    }
    if only_a_person {
        return Verdict::KeptAsking {
            output: json!(spec),
            rounds: rounds_answered,
        };
    }
    let settled = spec.take_recommended(Taken::RoundsLimit);
    Verdict::TookRecommended {
        output: json!(spec),
        settled,
        rounds: rounds_answered,
    }
}

fn invalid(problem: &str, question: &str) -> Reason {
    Reason::InvalidAnswer {
        problem: problem.to_owned(),
        question: question.to_owned(),
    }
}

/// Checks answers against what was asked, and refuses rather than guess: every question answered
/// once, with an option that's on offer or with something written.
pub fn check_answers(
    questions: &[Question],
    answers: &[Answer],
) -> Result<Vec<GivenAnswer>, Reason> {
    let mut seen = HashSet::new();
    for answer in answers {
        if !questions.iter().any(|q| q.id == answer.question) {
            return Err(invalid("unknownQuestion", &answer.question));
        }
        if !seen.insert(answer.question.as_str()) {
            return Err(invalid("repeatedQuestion", &answer.question));
        }
    }
    questions
        .iter()
        .map(|question| {
            let answer = answers
                .iter()
                .find(|a| a.question == question.id)
                .ok_or_else(|| invalid("unanswered", &question.id))?;
            let (text, recommended) = match &answer.choice {
                AnswerChoice::Option { label } => {
                    let option = question
                        .options
                        .iter()
                        .find(|o| o.label == *label)
                        .ok_or_else(|| invalid("optionNotOffered", &question.id))?;
                    (option.label.clone(), option.recommended)
                }
                AnswerChoice::Own { text } if text.trim().is_empty() => {
                    return Err(invalid("emptyText", &question.id));
                }
                AnswerChoice::Own { text } => (text.trim().to_owned(), false),
            };
            Ok(GivenAnswer {
                question: question.id.clone(),
                asked: question.text.clone(),
                answer: text,
                recommended,
                remember: answer.remember,
            })
        })
        .collect()
}

/// One decision for later runs: a line for the Brain, whatever was typed.
pub fn decision_parts(given: &GivenAnswer) -> (String, String) {
    (one_line(&given.asked), one_line(&given.answer))
}

/// The answers, one per line, as plain text.
pub fn answer_lines(given: &[GivenAnswer]) -> String {
    given
        .iter()
        .map(|a| format!("- {}", decided_line(&a.asked, &a.answer)))
        .collect::<Vec<_>>()
        .join("\n")
}

const ALREADY_DECIDED_EN: [&str; 3] = [
    "Keel already adds these decisions from the request, so don't repeat them in `decided`:\n{{decided}}\n",
    "Keel leaves these in the request, where agents read them: they're past what it takes from its lists of what was already decided. Don't copy them into `decided`:\n{{left}}\n",
    "Anything else the request treats as decided, copy into `decided` as usual.",
];
const ALREADY_DECIDED_ES: [&str; 3] = [
    "Keel ya agrega estas decisiones del pedido; no las repitas en `decided`:\n{{decided}}\n",
    "Keel deja estas en el pedido, donde los agentes las leen: pasan de lo que toma de sus listas de lo ya decidido. No las copies en `decided`:\n{{left}}\n",
    "Cualquier otra cosa que el pedido dé por decidida, cópiala en `decided` como siempre.",
];

/// What a specifier is told Keel adds from the request itself, word for word, so it repeats none
/// of it and copies the rest as ever, and what Keel leaves in the request, so it doesn't copy that
/// either: those would count against its own limits. Nothing when the request's lists say nothing.
/// What a person typed reaches the agent as text, and expands nothing.
pub(crate) fn already_decided_prompt(decided: &RequestDecided, language: &str) -> Option<String> {
    if decided.taken.is_empty() && decided.left.is_empty() {
        return None;
    }
    let [taken, left, rest] = if language.starts_with("es") {
        ALREADY_DECIDED_ES
    } else {
        ALREADY_DECIDED_EN
    };
    let lines = |items: Vec<&str>| {
        items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let taken_lines = lines(decided.taken.iter().map(|d| d.text.as_str()).collect());
    let left_lines = lines(decided.left.iter().map(String::as_str).collect());
    let template = [
        (!decided.taken.is_empty()).then_some(taken),
        (!decided.left.is_empty()).then_some(left),
        Some(rest),
    ]
    .into_iter()
    .flatten()
    .collect::<String>();
    Some(prompts::render(&template, |name| match name {
        "decided" => Some(taken_lines.clone()),
        "left" => Some(left_lines.clone()),
        _ => None,
    }))
}

const ANSWERED_EN: &str = "The person answered your questions:\n{{answers}}\n\nAnswer again in the same structure, with `criteria` reflecting what they said. You've already read the code, so read more only if an answer needs something new. Take what they answered out of `questions`, and ask something new only if their answer raises a decision that still meets the rules for asking.";
const ANSWERED_ES: &str = "La persona respondió tus preguntas:\n{{answers}}\n\nResponde de nuevo con la misma estructura, con `criteria` reflejando lo que dijo. Ya leíste el código, así que lee más solo si una respuesta necesita algo nuevo. Saca de `questions` lo que respondió, y pregunta algo nuevo solo si su respuesta abre una decisión que todavía cumpla las reglas para preguntar. Responde en español.";

/// What a resumed specifier is told. Rendered once like any prompt, so nothing a person typed gets
/// expanded, and it reaches the agent as text.
pub fn answered_prompt(given: &[GivenAnswer], language: &str) -> String {
    let template = if language.starts_with("es") {
        ANSWERED_ES
    } else {
        ANSWERED_EN
    };
    let lines = answer_lines(given);
    prompts::render(template, |name| (name == "answers").then(|| lines.clone()))
}

// As in the specifier's prompt, a text that's too long is asked for below its limit: a fix that
// misses it again by a character would leave the step stopped.
const CORRECTION_EN: &str = "Your answer doesn't fit what Keel can show the person. Fix exactly this and answer again in the same structure, without reading the repository again:\n{{problems}}\n\nShorten a text that's too long to about 15% under its limit, since characters are easy to miscount. Keep everything else as it was.";
const CORRECTION_ES: &str = "Tu respuesta no cabe en lo que Keel le puede mostrar a la persona. Corrige exactamente esto y responde de nuevo con la misma estructura, sin volver a leer el repositorio:\n{{problems}}\n\nAcorta un texto demasiado largo hasta un 15 % por debajo de su límite, porque es fácil contar mal los caracteres. Deja todo lo demás como estaba. Responde en español.";

/// What a specifier is told when it broke a limit, saying exactly what to fix.
pub fn correction_prompt(problems: &[SpecProblem], language: &str) -> String {
    let es = language.starts_with("es");
    let template = if es { CORRECTION_ES } else { CORRECTION_EN };
    let lines = problems
        .iter()
        .map(|problem| format!("- {}", problem_line(problem, es)))
        .collect::<Vec<_>>()
        .join("\n");
    prompts::render(template, |name| (name == "problems").then(|| lines.clone()))
}

fn problem_line(problem: &SpecProblem, es: bool) -> String {
    use SpecProblem::*;
    match problem {
        Unreadable if es => "No viene en la estructura pedida.".to_owned(),
        Unreadable => "It isn't in the structure you were asked for.".to_owned(),
        NoSummary if es => "`summary` está vacío.".to_owned(),
        NoSummary => "`summary` is empty.".to_owned(),
        SummaryTooLong { chars, limit } if es => {
            format!("`summary` tiene {chars} caracteres y el máximo es {limit}.")
        }
        SummaryTooLong { chars, limit } => {
            format!("`summary` has {chars} characters, and the limit is {limit}.")
        }
        CriterionNeedsText { id } if es => {
            format!("A `{id}` le falta `when` o `then`.")
        }
        CriterionNeedsText { id } => format!("`{id}` is missing its `when` or `then`."),
        QuestionNeedsText { id } if es => format!("`{id}` no tiene texto."),
        QuestionNeedsText { id } => format!("`{id}` has no text."),
        TooManyChanges { count, limit } if es => {
            format!("`changes` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyChanges { count, limit } => {
            format!("`changes` has {count} items, and the limit is {limit}.")
        }
        TooManyLimits { count, limit } if es => {
            format!("`limits` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyLimits { count, limit } => {
            format!("`limits` has {count} items, and the limit is {limit}.")
        }
        NoCriteria if es => "`criteria` está vacío.".to_owned(),
        NoCriteria => "`criteria` is empty.".to_owned(),
        TooManyCriteria { count, limit } if es => {
            format!("`criteria` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyCriteria { count, limit } => {
            format!("`criteria` has {count} items, and the limit is {limit}.")
        }
        CriterionTooLong { id, chars, limit } if es => {
            format!("En {id}, `when` y `then` suman {chars} caracteres y el máximo es {limit}.")
        }
        CriterionTooLong { id, chars, limit } => {
            format!(
                "In {id}, `when` and `then` add up to {chars} characters, and the limit is {limit}."
            )
        }
        CriterionNeedsId { position } if es => {
            format!("El elemento {position} de `criteria` no tiene `id`.")
        }
        CriterionNeedsId { position } => format!("Item {position} of `criteria` has no `id`."),
        RepeatedCriterion { id } if es => format!("El `id` {id} se repite en `criteria`."),
        RepeatedCriterion { id } => format!("The `id` {id} is repeated in `criteria`."),
        TooManyDecided { count, limit } if es => {
            format!("`decided` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyDecided { count, limit } => {
            format!("`decided` has {count} items, and the limit is {limit}.")
        }
        DecidedTooLong {
            position,
            chars,
            limit,
        } if es => {
            format!(
                "El elemento {position} de `decided` tiene {chars} caracteres y el máximo es {limit}."
            )
        }
        DecidedTooLong {
            position,
            chars,
            limit,
        } => {
            format!(
                "Item {position} of `decided` has {chars} characters, and the limit is {limit}."
            )
        }
        TooManyQuestions { count, limit } if es => {
            format!("`questions` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyQuestions { count, limit } => {
            format!("`questions` has {count} items, and the limit is {limit}.")
        }
        QuestionTooLong { id, chars, limit } if es => {
            format!("La pregunta {id} tiene {chars} caracteres y el máximo es {limit}.")
        }
        QuestionTooLong { id, chars, limit } => {
            format!("Question {id} has {chars} characters, and the limit is {limit}.")
        }
        QuestionNeedsId { position } if es => {
            format!("El elemento {position} de `questions` no tiene `id`.")
        }
        QuestionNeedsId { position } => format!("Item {position} of `questions` has no `id`."),
        RepeatedQuestion { id } if es => format!("El `id` {id} se repite en `questions`."),
        RepeatedQuestion { id } => format!("The `id` {id} is repeated in `questions`."),
        WrongNumberOfOptions {
            id,
            count,
            min,
            max,
        } if es => {
            format!("La pregunta {id} tiene {count} opciones, y deben ser de {min} a {max}.")
        }
        WrongNumberOfOptions {
            id,
            count,
            min,
            max,
        } => {
            format!("Question {id} has {count} options, and it needs {min} to {max}.")
        }
        OptionTooLong { id, chars, limit } if es => {
            format!(
                "Una opción de la pregunta {id} tiene {chars} caracteres y el máximo es {limit}."
            )
        }
        OptionTooLong { id, chars, limit } => {
            format!("An option of question {id} has {chars} characters, and the limit is {limit}.")
        }
        RepeatedOption { id } if es => {
            format!("La pregunta {id} tiene opciones vacías o repetidas.")
        }
        RepeatedOption { id } => format!("Question {id} has empty or repeated options."),
        TooManyRecommended { id, count } if es => {
            format!(
                "La pregunta {id} tiene {count} opciones recomendadas, y puede tener una como máximo."
            )
        }
        TooManyRecommended { id, count } => {
            format!("Question {id} has {count} recommended options, and it can have one at most.")
        }
        LeftUnresolved if es => {
            "Dejaste algo marcado como [TBC]. Decídelo o pregúntalo.".to_owned()
        }
        LeftUnresolved => "You left something marked [TBC]. Decide it or ask.".to_owned(),
        NoParts if es => {
            "`parts` está vacío: di qué cambia en al menos uno de los proyectos.".to_owned()
        }
        NoParts => "`parts` is empty: say what changes in one of the projects at least.".to_owned(),
        TooManyParts { count, limit } if es => {
            format!("`parts` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyParts { count, limit } => {
            format!("`parts` has {count} items, and the limit is {limit}.")
        }
        UnknownProject { project } if es => format!(
            "`parts` nombra {project}, que no es ninguno de los proyectos de la lista. Usa el nombre exacto."
        ),
        UnknownProject { project } => format!(
            "`parts` names {project}, which isn't one of the listed projects. Use the exact name."
        ),
        RepeatedPart { project } if es => {
            format!("{project} aparece dos veces en `parts`: junta lo que cambia ahí en una.")
        }
        RepeatedPart { project } => {
            format!("{project} is in `parts` twice: put what changes there in one.")
        }
        TooManyTogether { count, limit } if es => {
            format!("`together` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyTogether { count, limit } => {
            format!("`together` has {count} items, and the limit is {limit}.")
        }
        TooManyContract { count, limit } if es => {
            format!("`contract` tiene {count} elementos y el máximo es {limit}.")
        }
        TooManyContract { count, limit } => {
            format!("`contract` has {count} items, and the limit is {limit}.")
        }
        ContractNeedsText { id, position } if es => {
            format!("Al elemento {position} de `contract` ({id}) le falta `when` o `then`.")
        }
        ContractNeedsText { id, position } => {
            format!("Item {position} of `contract` ({id}) is missing its `when` or `then`.")
        }
        ContractTooLong {
            id,
            position,
            chars,
            limit,
        } if es => format!(
            "En el elemento {position} de `contract` ({id}), `when` y `then` suman {chars} caracteres y el máximo es {limit}."
        ),
        ContractTooLong {
            id,
            position,
            chars,
            limit,
        } => format!(
            "In item {position} of `contract` ({id}), `when` and `then` add up to {chars} characters, and the limit is {limit}."
        ),
        ContractUnknownProject {
            id,
            position,
            project,
        } if es => format!(
            "El elemento {position} de `contract` ({id}) nombra {project}, que no es ninguno de los proyectos de la lista. Usa el nombre exacto."
        ),
        ContractUnknownProject {
            id,
            position,
            project,
        } => format!(
            "Item {position} of `contract` ({id}) names {project}, which isn't one of the listed projects. Use the exact name."
        ),
        ContractWithoutPart { id, position } if es => format!(
            "El elemento {position} de `contract` ({id}) no nombra ningún proyecto de `parts`: pon en `projects` cada uno cuya parte tiene que cumplirlo."
        ),
        ContractWithoutPart { id, position } => format!(
            "Item {position} of `contract` ({id}) names no project in `parts`: put in `projects` each one whose part has to keep it."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_spec(output: &Value) -> Result<Spec, Vec<SpecProblem>> {
        validate_spec_for(output, None, &[])
    }

    /// [`super::judge`] for a request that decided nothing, so Keel adds nothing from it.
    fn judge(output: Option<&Value>, after_correction: bool, rounds_answered: usize) -> Verdict {
        super::judge(output, &[], after_correction, rounds_answered)
    }

    /// [`super::judge_part`] for a request that decided nothing, so Keel adds nothing from it.
    fn judge_part(
        output: Option<&Value>,
        after_correction: bool,
        rounds_answered: usize,
        contract: &[Criterion],
    ) -> Verdict {
        super::judge_part(output, &[], after_correction, rounds_answered, contract)
    }

    /// A request a person wrote.
    fn typed(text: &str) -> Vec<Written> {
        vec![Written {
            text: text.into(),
            assistant: None,
            github_account: None,
        }]
    }

    fn valid() -> Value {
        json!({
            "summary": "add returns the sum",
            "criteria": [{ "id": "C1", "when": "add(1, 2)", "then": "it returns 3" }],
            "changes": [{ "kind": "changes", "text": "add stops subtracting" }],
            "limits": ["Its signature stays the same"],
            "decided": [{ "text": "Keeps the rounding as it is" }],
            "questions": [],
        })
    }

    fn with(field: &str, value: Value) -> Value {
        let mut output = valid();
        output[field] = value;
        output
    }

    #[test]
    fn empty_text_is_sent_back_and_long_lists_too() {
        let problems = |output: Value| validate_spec(&output).unwrap_err();
        let criteria = json!([{ "id": "C1", "when": "  ", "then": "t" }]);
        assert_eq!(
            problems(with("criteria", criteria)),
            [SpecProblem::CriterionNeedsText { id: "C1".into() }]
        );
        let questions = json!([question("Q1", " ", &[("a", true), ("b", false)])]);
        assert_eq!(
            problems(with("questions", questions)),
            [SpecProblem::QuestionNeedsText { id: "Q1".into() }]
        );
        let changes: Vec<Value> = (0..11)
            .map(|_| json!({ "kind": "adds", "text": "x" }))
            .collect();
        assert_eq!(
            problems(with("changes", json!(changes))),
            [SpecProblem::TooManyChanges {
                count: 11,
                limit: 10
            }]
        );
        assert_eq!(
            problems(with("limits", json!(vec!["x"; 11]))),
            [SpecProblem::TooManyLimits {
                count: 11,
                limit: 10
            }]
        );
    }

    #[test]
    fn a_long_line_in_those_lists_is_cut_rather_than_sent_back() {
        let spec = validate_spec(&with("limits", json!(["é".repeat(300)]))).unwrap();
        assert_eq!(spec.limits[0].chars().count(), MAX_LISTED_TEXT);
        assert!(spec.limits[0].ends_with('…'));
        // One a little over reads whole, with nothing cut off its end.
        let spec = validate_spec(&with("limits", json!(["é".repeat(211)]))).unwrap();
        assert_eq!(spec.limits[0], "é".repeat(211));
    }

    fn question(id: &str, text: &str, options: &[(&str, bool)]) -> Value {
        json!({
            "id": id,
            "text": text,
            "options": options.iter().map(|(label, recommended)| json!({ "label": label, "recommended": recommended })).collect::<Vec<_>>(),
        })
    }

    fn problems(output: Value) -> Vec<SpecProblem> {
        validate_spec(&output).unwrap_err()
    }

    #[test]
    fn a_good_answer_passes_and_loses_nothing() {
        let spec = validate_spec(&valid()).unwrap();
        assert_eq!(spec.summary, "add returns the sum");
        assert_eq!(spec.criteria[0].then, "it returns 3");
        assert_eq!(spec.changes[0].kind, ChangeKind::Changes);
        assert_eq!(spec.limits[0], "Its signature stays the same");
        assert_eq!(spec.decided[0].text, "Keeps the rounding as it is");
    }

    #[test]
    fn limits_count_characters_and_not_bytes() {
        let at_the_limit = with("summary", json!("ñ".repeat(MAX_SUMMARY)));
        assert!(validate_spec(&at_the_limit).is_ok(), "twice as many bytes");
        assert_eq!(
            problems(with("summary", json!("ñ".repeat(231)))),
            [SpecProblem::SummaryTooLong {
                chars: 231,
                limit: 200
            }]
        );
    }

    #[test]
    fn criteria_have_a_limit_of_their_own_and_when_and_then_count_together() {
        let criterion = |id: usize, when: usize, then: usize| json!({ "id": format!("C{id}"), "when": "w".repeat(when), "then": "t".repeat(then) });
        let seven: Vec<Value> = (1..=7).map(|i| criterion(i, 100, 100)).collect();
        assert!(validate_spec(&with("criteria", json!(seven))).is_ok());
        let eight: Vec<Value> = (1..=8).map(|i| criterion(i, 1, 1)).collect();
        assert_eq!(
            problems(with("criteria", json!(eight))),
            [SpecProblem::TooManyCriteria { count: 8, limit: 7 }]
        );
        assert_eq!(
            problems(with("criteria", json!([criterion(1, 150, 81)]))),
            [SpecProblem::CriterionTooLong {
                id: "C1".into(),
                chars: 231,
                limit: 200
            }]
        );
        assert_eq!(
            problems(with("criteria", json!([]))),
            [SpecProblem::NoCriteria]
        );
    }

    #[test]
    fn what_it_decided_alone_has_its_limits() {
        let one = |text: String| json!({ "text": text });
        let five: Vec<Value> = (0..5).map(|_| one("x".repeat(140))).collect();
        assert!(validate_spec(&with("decided", json!(five))).is_ok());
        let six: Vec<Value> = (0..6).map(|_| one("x".into())).collect();
        assert_eq!(
            problems(with("decided", json!(six))),
            [SpecProblem::TooManyDecided { count: 6, limit: 5 }]
        );
        assert_eq!(
            problems(with("decided", json!([one("x".repeat(162))]))),
            [SpecProblem::DecidedTooLong {
                position: 1,
                chars: 162,
                limit: 140
            }]
        );
    }

    /// Real specs went back for 141 of 140, 211 of 200 and 216 of 200, each at the cost of a second
    /// turn of the agent. A text up to 15 % over reads the same to the person, so it's taken as it is.
    #[test]
    fn a_text_a_little_over_its_limit_is_taken_as_it_is_and_one_far_over_goes_back() {
        let good = [("Yes", true), ("No", false)];
        let criterion = |then: usize| json!([{ "id": "C1", "when": "w".repeat(100), "then": "t".repeat(then) }]);
        let point = |then: usize| json!([{ "when": "w".repeat(100), "then": "t".repeat(then), "projects": ["valora"] }]);
        // Each text at the most that still fits, and one character more.
        for (field, of, most) in [
            (
                "summary",
                &(|n| json!("ñ".repeat(n))) as &dyn Fn(usize) -> Value,
                230,
            ),
            ("criteria", &|n| criterion(n - 100), 230),
            ("decided", &|n| json!([{ "text": "x".repeat(n) }]), 161),
            (
                "questions",
                &|n| json!([question("Q1", &"x".repeat(n), &good)]),
                161,
            ),
            (
                "questions",
                &|n| {
                    json!([question(
                        "Q1",
                        "a?",
                        &[(&"x".repeat(n), true), ("No", false)]
                    )])
                },
                69,
            ),
        ] {
            let spec = validate_spec(&with(field, of(most)));
            assert!(spec.is_ok(), "{field} of {most}: {spec:?}");
            assert_eq!(problems(with(field, of(most + 1))).len(), 1, "{field}");
        }
        // The ones a real run went back for.
        for (field, value) in [
            ("decided", json!([{ "text": "x".repeat(141) }])),
            ("summary", json!("x".repeat(211))),
            ("criteria", criterion(116)),
        ] {
            let Verdict::Ready(output) = judge(Some(&with(field, value.clone())), false, 0) else {
                panic!("{field} went back")
            };
            // As it was written, with nothing cut.
            if field == "summary" {
                assert_eq!(output[field], value);
            }
        }
        // A point of the contract goes by the same limit as a criterion.
        let contract = |then: usize| {
            let mut output = agreed(json!([part("valora", "x")]), json!([]));
            output["contract"] = point(then);
            judged(&output)
        };
        assert!(matches!(contract(130), Verdict::Ready(_)));
        assert!(matches!(contract(131), Verdict::Correct(_)));
        // How many items a list has is exact, as it was.
        let six: Vec<Value> = (0..6).map(|_| json!({ "text": "x" })).collect();
        assert_eq!(
            problems(with("decided", json!(six))),
            [SpecProblem::TooManyDecided { count: 6, limit: 5 }]
        );
    }

    /// Models count characters loosely: asked for 140, one wrote a decision of 141. So the prompt
    /// asks for less than Keel takes, and a near miss still fits.
    #[test]
    fn the_prompt_asks_for_less_than_keel_takes() {
        let asked = [
            (
                "what will change, up to",
                "qué va a cambiar, hasta",
                170,
                MAX_SUMMARY,
            ),
            (
                "together take up to",
                "juntos llegan hasta",
                170,
                MAX_CRITERION,
            ),
            ("each up to", "cada uno hasta", 120, MAX_DECIDED_TEXT),
            (
                "one line of up to",
                "una línea de hasta",
                120,
                MAX_QUESTION_TEXT,
            ),
            (
                "options of up to",
                "opciones de hasta",
                50,
                MAX_OPTION_LABEL,
            ),
        ];
        let en = prompts::spec_prompt_with_size("en");
        let es = prompts::spec_prompt_with_size("es");
        for (in_en, in_es, number, limit) in asked {
            assert!(
                en.contains(&format!("{in_en} {number} characters")),
                "{in_en}"
            );
            assert!(
                es.contains(&format!("{in_es} {number} caracteres")),
                "{in_es}"
            );
            assert!(
                number < limit && number * 100 >= limit * 80,
                "a margin: {number} of {limit}"
            );
        }

        // Written past the 120 it was asked for, a decision still fits. Far past the 140 Keel
        // takes, it goes back once, saying which one and by how much.
        let decided = |chars: usize| with("decided", json!([{ "text": "x".repeat(chars) }]));
        assert!(matches!(
            judge(Some(&decided(140)), false, 0),
            Verdict::Ready(_)
        ));
        let Verdict::Correct(problems) = judge(Some(&decided(162)), false, 0) else {
            panic!()
        };
        assert_eq!(
            problems,
            [SpecProblem::DecidedTooLong {
                position: 1,
                chars: 162,
                limit: 140
            }]
        );
        assert!(
            correction_prompt(&problems, "en")
                .contains("Item 1 of `decided` has 162 characters, and the limit is 140.")
        );
        // The fix is asked for below the limit too, or it may miss it again by one and stop.
        assert!(
            correction_prompt(&problems, "en").contains("to about 15% under its limit"),
            "en"
        );
        assert!(
            correction_prompt(&problems, "es").contains("hasta un 15 % por debajo de su límite"),
            "es"
        );
    }

    #[test]
    fn questions_have_their_limits_and_at_most_one_recommended_option() {
        let good = [("Yes", true), ("No", false)];
        let of = |questions: Value| problems(with("questions", questions));
        assert!(validate_spec(&with("questions", json!([question("Q1", "Why?", &good)]))).is_ok());
        assert_eq!(
            of(json!([
                question("Q1", "a?", &good),
                question("Q2", "b?", &good),
                question("Q3", "c?", &good),
                question("Q4", "d?", &good),
            ])),
            [SpecProblem::TooManyQuestions { count: 4, limit: 3 }]
        );
        assert_eq!(
            of(json!([question("Q1", &"x".repeat(162), &good)])),
            [SpecProblem::QuestionTooLong {
                id: "Q1".into(),
                chars: 162,
                limit: 140
            }]
        );
        assert_eq!(
            of(json!([question("Q1", "a?", &[("Only", true)])])),
            [SpecProblem::WrongNumberOfOptions {
                id: "Q1".into(),
                count: 1,
                min: 2,
                max: 4
            }]
        );
        let five = [
            ("a", true),
            ("b", false),
            ("c", false),
            ("d", false),
            ("e", false),
        ];
        assert!(matches!(
            of(json!([question("Q1", "a?", &five)]))[0],
            SpecProblem::WrongNumberOfOptions { count: 5, .. }
        ));
        let long = "x".repeat(70);
        assert_eq!(
            of(json!([question(
                "Q1",
                "a?",
                &[(long.as_str(), true), ("No", false)]
            )])),
            [SpecProblem::OptionTooLong {
                id: "Q1".into(),
                chars: 70,
                limit: 60
            }]
        );
        // When nothing backs one up, none is recommended, rather than a made-up one.
        assert!(
            validate_spec(&with(
                "questions",
                json!([question("Q1", "a?", &[("a", false), ("b", false)])])
            ))
            .is_ok()
        );
        assert_eq!(
            of(json!([question("Q1", "a?", &[("a", true), ("b", true)])])),
            [SpecProblem::TooManyRecommended {
                id: "Q1".into(),
                count: 2
            }]
        );
    }

    #[test]
    fn ids_and_options_that_could_not_be_told_apart_are_refused() {
        let good = [("Yes", true), ("No", false)];
        assert_eq!(
            problems(with(
                "questions",
                json!([question("Q1", "a?", &good), question("Q1", "b?", &good)])
            )),
            [SpecProblem::RepeatedQuestion { id: "Q1".into() }]
        );
        assert_eq!(
            problems(with(
                "questions",
                json!([question("Q1", "a?", &[("Same", true), ("Same", false)])])
            )),
            [SpecProblem::RepeatedOption { id: "Q1".into() }]
        );
        let repeated = json!([
            { "id": "C1", "when": "a", "then": "b" },
            { "id": "C1", "when": "c", "then": "d" },
        ]);
        assert_eq!(
            problems(with("criteria", repeated)),
            [SpecProblem::RepeatedCriterion { id: "C1".into() }]
        );
    }

    #[test]
    fn nothing_can_stay_marked_as_still_to_be_decided_anywhere() {
        for output in [
            with("summary", json!("Do it [TBC]")),
            with("limits", json!(["[tbc] who knows"])),
            with(
                "questions",
                json!([question("Q1", "a?", &[("[TBC]", true), ("b", false)])]),
            ),
        ] {
            assert!(
                problems(output).contains(&SpecProblem::LeftUnresolved),
                "anywhere"
            );
        }
    }

    #[test]
    fn something_that_is_not_the_structure_says_so() {
        assert_eq!(
            problems(json!({ "verdict": "ok" })),
            [SpecProblem::Unreadable]
        );
        assert_eq!(problems(json!("just text")), [SpecProblem::Unreadable]);
    }

    #[test]
    fn line_breaks_go_so_nothing_can_fake_a_heading_later() {
        let spec = validate_spec(&with("summary", json!("  a\n\n# Heading\tb "))).unwrap();
        assert_eq!(spec.summary, "a # Heading b");
    }

    #[test]
    fn an_agent_cannot_write_what_only_the_engine_writes() {
        let mut output = valid();
        output["decided"] = json!([{ "text": "x", "taken": "person" }]);
        output["answered"] = json!([{ "question": "Q1", "asked": "x", "answer": "y", "recommended": false, "remember": true }]);
        let Verdict::Ready(kept) = judge(Some(&output), false, 0) else {
            panic!()
        };
        assert_eq!(kept["decided"][0].get("taken"), None);
        assert_eq!(kept.get("answered"), None);
    }

    #[test]
    fn a_list_under_a_decisions_heading_is_what_the_request_decided_word_for_word() {
        // Its items, and one broken over two lines is still one, with its "no" in it.
        let asked = "Count the threads that got fixed.\n\nDecisiones ya tomadas (no las preguntes):\n- No se cuentan los hilos viejos.\n- Si el autor resuelve su propio hilo critical, por ahora no\n   cuenta como arreglado.\n\nMore context.\n- Not a decision.";
        assert_eq!(
            decided_in(asked),
            [
                "No se cuentan los hilos viejos.",
                "Si el autor resuelve su propio hilo critical, por ahora no cuenta como arreglado.",
            ]
        );
        // Numbered too, and with a blank line before the list, as Markdown often has.
        let numbered = "## Decided\n\n1. Old threads stay.\n2) Critical ones count.";
        assert_eq!(
            decided_in(numbered),
            ["Old threads stay.", "Critical ones count."]
        );
        for heading in [
            "**Decisions already made:**",
            "Decisiones tomadas",
            "### decided:",
            "DECIDED (don't ask)",
        ] {
            assert_eq!(
                decided_in(&format!("{heading}\n* It stays.")),
                ["It stays."],
                "{heading}"
            );
        }
        // A sentence that only starts like one isn't a heading, and another heading ends the list.
        assert!(decided_in("Decided to keep it:\n- It stays.").is_empty());
        assert_eq!(
            decided_in("Decided:\n- It stays.\n## Questions\n- Should it?"),
            ["It stays."]
        );
    }

    #[test]
    fn a_line_indented_as_an_item_s_words_goes_on_with_it() {
        assert_eq!(
            decided_in("Decided:\n- Old threads are deleted\n  only when their author asks."),
            ["Old threads are deleted only when their author asks."]
        );
        assert_eq!(
            decided_in("## Decided\n10. Old threads are deleted\n    only when their author asks."),
            ["Old threads are deleted only when their author asks."]
        );
    }

    #[test]
    fn a_line_under_an_item_not_indented_as_its_words_ends_the_list_and_drops_that_item() {
        // The person sees it on a line of its own: it may be the rest of the item, or a label over
        // what follows. Either way, the item may be cut short, and what follows isn't the list's.
        assert!(
            decided_in("Decided:\n- Old threads are deleted\nonly when their author asks.")
                .is_empty()
        );
        assert!(
            decided_in(
                "Decisiones ya tomadas:\n- No se cuentan los hilos viejos.\nPor decidir:\n- Borrar la tabla vieja."
            )
            .is_empty()
        );
        // The items before it stay.
        assert_eq!(
            decided_in(
                "Decided:\n- Old threads stay.\n- Critical ones count.\nPending:\n- Drop the table."
            ),
            ["Old threads stay."]
        );
        assert_eq!(
            decided_in(
                "## Decided\n1. Old threads stay.\n2. Critical ones count.\n**Pending:**\n1. Should old threads count?"
            ),
            ["Old threads stay."]
        );
        // Less indented than the item's words is not enough.
        assert!(decided_in("Decided:\n- Old threads are deleted\n only when asked.").is_empty());
        assert!(decided_in("Decided:\n1. Old threads are deleted\n  only when asked.").is_empty());
        // A tag right under an item may be part of it. After a blank line, the item was whole.
        assert!(decided_in("Decided:\n- It stays.\n</issue>").is_empty());
        assert_eq!(
            decided_in("Decided:\n- It stays.\n\n</issue>"),
            ["It stays."]
        );
    }

    #[test]
    fn an_item_something_cuts_short_is_dropped_whole() {
        // More of it after a blank line, indented as its words are.
        assert!(
            decided_in(
                "Decided:\n1. Old threads are deleted.\n\n   Except critical ones, which stay."
            )
            .is_empty()
        );
        assert!(
            decided_in("Decided:\n- Old threads stay.\n\n\n  - Unless they are critical.")
                .is_empty()
        );
        // Code, a quote, a tag, a heading or a break inside it.
        for inside in [
            "  ```\n  migrate down\n  ```",
            "  > migrate down",
            "  <b>migrate down</b>",
            "  ## migrate down",
            "  ***",
        ] {
            let text =
                format!("Decided:\n- Old threads stay.\n- Never run\n{inside}\n  in production.");
            assert_eq!(decided_in(&text), ["Old threads stay."], "{inside}");
        }
        // Right under it, however indented: they may be what it goes on to say.
        for under in [
            "```\nmigrate down\n```",
            "> migrate down",
            "<code>migrate down</code>",
        ] {
            let text = format!("Decided:\n- Never run\n{under}\nin production.");
            assert!(decided_in(&text).is_empty(), "{under}");
        }
        // After a blank line, with what follows not indented as its words, it was whole.
        assert_eq!(
            decided_in("Decided:\n1. Old threads stay.\n\n  Some context.\n- Not this."),
            ["Old threads stay."]
        );
        assert_eq!(
            decided_in("Decided:\n- Old threads stay.\n\n```\nmigrate down\n```"),
            ["Old threads stay."]
        );
        assert_eq!(
            decided_in("Decided:\n- Old threads stay.\n\n"),
            ["Old threads stay."]
        );
    }

    #[test]
    fn an_item_less_indented_than_another_s_words_is_its_sibling_not_part_of_it() {
        assert_eq!(
            decided_in("Decided:\n- Old threads stay.\n - Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        assert_eq!(
            decided_in("Decided:\n1. Old threads stay.\n  2. Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        assert_eq!(
            decided_in("Decided:\n-   Old threads stay.\n  - Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        // As indented as its words, it's part of it.
        assert_eq!(
            decided_in("Decided:\n1. Use the new API for:\n   - reads\n2. Old threads stay."),
            ["Use the new API for: - reads", "Old threads stay."]
        );
        assert_eq!(
            decided_in("Decided:\n-   Use the new API for:\n    - reads"),
            ["Use the new API for: - reads"]
        );
    }

    #[test]
    fn a_heading_s_note_opens_the_list_only_when_it_says_the_decisions_are_taken() {
        for heading in [
            "Decided (tentative):",
            "## Decided (not yet, ask me)",
            "**Decisions already made (TBD):**",
            "Decided (maybe)",
            "Decided (draft)",
            "Decided (to confirm)",
            "Decided (provisional)",
            "## Decided (pending)",
            "## Decided (proposed)",
            "## Decisions already made (preliminary)",
            "## Decided (not final)",
            "## Decided (pending confirmation)",
            "## Decided (to be decided)",
            "## Decided (WIP)",
            "## Decided (final, maybe)",
            "## Decided (don't ask, pending)",
            "## Decided (not confirmed)",
            "## Decided (so far)",
            "**Decided** (pending):",
            "Decisiones tomadas (todavía no):",
            "Decisiones ya tomadas (aún no, pregúntame)",
            "Decisiones tomadas (por confirmar)",
            "Decisiones tomadas (pendiente)",
            "Decisiones tomadas (tentativo)",
            "Decisiones tomadas (provisional)",
            "Decisiones tomadas (quizá)",
            "Decisiones tomadas (tal vez)",
            "Decisiones tomadas (borrador)",
            "## Decisiones tomadas (propuesta)",
            "## Decisiones tomadas (por revisar)",
            "## Decisiones tomadas (no definitivas)",
            "## Decisiones ya tomadas (por definir)",
            "## Decisiones tomadas (pendiente de confirmar)",
        ] {
            assert!(
                decided_in(&format!("{heading}\n- Old threads stay.")).is_empty(),
                "{heading}"
            );
        }
        // A note that says they're taken, or only not to ask them, opens it, and so does none.
        for heading in [
            "Decisiones ya tomadas (no las preguntes):",
            "Decisiones ya tomadas (No las preguntes.)",
            "Decided (don't ask):",
            "Decided (don’t ask)",
            "Decided (do not ask)",
            "## Decided (by me)",
            "## Decided (final)",
            "## Decided (final, don't ask)",
            "## Decisiones tomadas (definitivas)",
            "## Decisiones tomadas (por mí)",
            "## Decisions already made (already made)",
            "## Decided",
        ] {
            assert_eq!(
                decided_in(&format!("{heading}\n- Old threads stay.")),
                ["Old threads stay."],
                "{heading}"
            );
        }
    }

    #[test]
    fn only_the_list_s_own_items_count_up_to_a_blank_line_and_not_open_tasks() {
        // What comes after a blank line is something else, such as steps to follow.
        assert_eq!(
            decided_in(
                "Decisiones ya tomadas:\n- La tabla vieja se queda.\n\n1. Agrega la columna.\n2. Borra la tabla vieja."
            ),
            ["La tabla vieja se queda."]
        );
        // An item under another one is part of it.
        assert_eq!(
            decided_in(
                "Decided:\n- Use the new API for:\n  - reads\n  - writes\n- Pendiente, no decidido:\n    - ¿Cuenta el hilo critical?"
            ),
            [
                "Use the new API for: - reads - writes",
                "Pendiente, no decidido: - ¿Cuenta el hilo critical?"
            ]
        );
        // A task still open isn't decided, nor anything under it.
        assert_eq!(
            decided_in("Decided:\n- [ ] Open thing\n  that goes on.\n- [x] Old threads stay."),
            ["Old threads stay."]
        );
    }

    #[test]
    fn a_task_that_is_done_is_decided_without_its_box() {
        assert_eq!(
            decided_in("Decided:\n- [x] Old threads stay.\n- [X]  Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        // Only a box with a space after it is one.
        assert_eq!(
            decided_in("Decided:\n- [x]Old threads stay."),
            ["[x]Old threads stay."]
        );
        // So a spec that copies it is matched, and it's there once, as the request's.
        let spec = kept(
            json!([{ "text": "Old threads stay." }]),
            &typed("Decided:\n- [x] Old threads stay."),
        );
        assert_eq!(
            json!(spec)["decided"],
            json!([{ "text": "Old threads stay.", "taken": "request" }])
        );
    }

    #[test]
    fn a_line_that_starts_with_a_hash_is_a_heading_only_as_markdown_reads_one() {
        // `#131` goes on with the sentence: a heading has one to six `#`, then a space.
        assert_eq!(
            decided_in("Decided:\n- Old threads are deleted only after\n  #131 lands"),
            ["Old threads are deleted only after #131 lands"]
        );
        assert_eq!(
            decided_in("Decided:\n- It stays\n  ####### as it is."),
            ["It stays ####### as it is."]
        );
        // Not indented as the item's words, it may be the rest of it, so it's never kept cut short.
        assert!(
            decided_in("Decided:\n- Old threads are deleted only after\n#131 lands").is_empty()
        );
        assert!(decided_in("Decided:\n- It stays\n####### as it is.").is_empty());
        // With a space, or nothing after them, they are one, and the list is over.
        assert_eq!(
            decided_in("Decided:\n- It stays.\n# Next\n- Not this."),
            ["It stays."]
        );
        assert_eq!(
            decided_in("Decided:\n- It stays.\n##\n- Not this."),
            ["It stays."]
        );
        // Nor is `#Decided` a heading for what was decided.
        assert!(decided_in("#Decided\n- Drop the table.").is_empty());
    }

    #[test]
    fn a_wrapped_line_that_starts_with_a_number_goes_on_with_the_sentence() {
        // Only a numbered item that starts at 1 breaks into a sentence, as in Markdown. Any other,
        // not indented as the item's words, may be the rest of it, so it's never kept cut short.
        assert_eq!(
            decided_in("Decided:\n- Old threads are deleted only after\n  10. lands"),
            ["Old threads are deleted only after 10. lands"]
        );
        assert!(decided_in("Decided:\n- Old threads are deleted only after\n10. lands").is_empty());
        assert_eq!(
            decided_in("Decided:\n- Old threads stay.\n1. Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        // A numbered list goes on with any number.
        assert_eq!(
            decided_in("## Decided\n9. Old threads stay.\n10. Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        // Right under a line of text that names the list, one from 2 is part of that line, so
        // there's no list. After a blank line, there is.
        assert!(decided_in("Decided:\n2. Old threads stay.\n3. Critical ones count.").is_empty());
        assert_eq!(
            decided_in("Decided:\n\n2. Old threads stay."),
            ["Old threads stay."]
        );
    }

    #[test]
    fn code_in_a_line_is_not_a_code_block() {
        // Backticks with more backticks after them are code within the line, as in Markdown.
        assert_eq!(
            decided_in("Decided:\n- Check it with\n  ```pnpm test``` first.\n- Old threads stay."),
            ["Check it with ```pnpm test``` first.", "Old threads stay."]
        );
        assert_eq!(
            decided_in("```pnpm test```\n\nDecided:\n- Old threads stay."),
            ["Old threads stay."]
        );
        // After tildes, backticks are still the words of a fence.
        assert_eq!(
            decided_in("~~~ `md`\nDecided:\n- Drop the table.\n~~~\nDecided:\n- It stays."),
            ["It stays."]
        );
        // And only a line of the same marks, as many or more, ends one.
        assert_eq!(
            decided_in(
                "```\nDecided:\n- Drop the table.\n``` still code\n```\nDecided:\n- It stays."
            ),
            ["It stays."]
        );
    }

    #[test]
    fn plus_bullets_closed_headings_and_underlined_ones_are_read_as_markdown_does() {
        assert_eq!(
            decided_in("Decided:\n+ Old threads stay.\n+ Critical ones count."),
            ["Old threads stay.", "Critical ones count."]
        );
        for heading in [
            "## Decided ##",
            "# Decisiones ya tomadas #####",
            "### Decided #  ",
        ] {
            assert_eq!(
                decided_in(&format!("{heading}\n- It stays.")),
                ["It stays."],
                "{heading}"
            );
        }
        // The closing `#` needs a space before it.
        assert!(decided_in("## Decided#\n- Drop the table.").is_empty());
        for underline in ["---", "===", "  ======  "] {
            assert_eq!(
                decided_in(&format!("Decided\n{underline}\n- It stays.")),
                ["It stays."],
                "{underline}"
            );
        }
        assert_eq!(decided_in("Decided:\n---\n\n- It stays."), ["It stays."]);
        // Under an item, it's a break and the list is over. Under more than the heading's words,
        // it's another heading.
        assert_eq!(
            decided_in("Decided\n---\n- It stays.\n---\n- Not this."),
            ["It stays."]
        );
        assert!(decided_in("Some context.\nDecided\n---\n- Not this.").is_empty());
    }

    #[test]
    fn a_heading_starts_a_block_and_is_never_the_end_of_a_sentence() {
        assert!(
            decided_in(
                "Count the threads. The team has not\ndecided:\n- whether old threads count"
            )
            .is_empty()
        );
        // One with `#` starts a block wherever it is.
        assert_eq!(
            decided_in("Some context.\n## Decided\n- It stays."),
            ["It stays."]
        );
    }

    #[test]
    fn markup_and_template_text_is_not_a_decision() {
        assert_eq!(
            decided_in(
                "Decided:\n- [x]\n- -\n- **\n- <!-- list what is already decided -->\n- It stays."
            ),
            ["It stays."]
        );
        // Struck through, it may have been taken back.
        for struck in [
            "~~Use Redis for sessions~~",
            "<s>Use Redis</s> for sessions",
            "<del>Use Redis</del>",
        ] {
            assert_eq!(
                decided_in(&format!("## Decided\n- Use Postgres\n- {struck}")),
                ["Use Postgres"],
                "{struck}"
            );
        }
        // A line of only `*`, `-` or `_` is a break that ends the list.
        assert_eq!(
            decided_in("Decided:\n- It stays.\n* * *\n- Not this."),
            ["It stays."]
        );
    }

    #[test]
    fn code_or_a_quote_under_an_item_is_never_part_of_it_and_a_comment_is_nothing() {
        // Code or a quote right under an item may be what it goes on to say: it's dropped, and the
        // list is over.
        let fenced =
            "Decided:\n- It stays.\n  ```\n  drop table threads;\n  ```\n- Not after code.";
        assert!(decided_in(fenced).is_empty());
        let quoted = "Decided:\n- It stays.\n  > a customer said to drop it\n- Not after a quote.";
        assert!(decided_in(quoted).is_empty());
    }

    #[test]
    fn an_item_with_a_comment_in_it_is_dropped_and_one_between_items_is_nothing() {
        // A comment can hide part of what an item says, or Keel can misread where it ends.
        for text in [
            "## Decided\n- Delete the old threads\n  <!-- note --> only those older than a year",
            "## Decided\n- Delete the old threads\n  <!-- note\n  --> only those older than a year",
            "## Decided\n- Delete the old threads\n<!-- note --> only the ones from 2019",
            "## Decided\n- Delete the old threads\n<!-- note\n--> only the ones from 2019",
            "## Decided\n- Retention: 7 days\n  <!-- legal asked --> but only for EU users",
            "## Decided\n- Lines starting with `<!--` are skipped until `-->`, and the rest is kept",
            "## Decided\n- Delete generated files, except those with a `<!--` header",
            "## Decided\n- Escape `<!--` in user content before saving it",
            "## Decided\n- It stays. <!-- why: the export -->",
            "## Decided\n- It stays\n  <!-- drop it later -->\n  as it is.",
            "## Decided\n- It stays.\n  <!-- drop it later -->",
            "## Decided\n- It stays\n<!-- drop it later -->\n  as it is.",
        ] {
            assert!(
                decided_in(text).is_empty(),
                "{text:?}: {:?}",
                decided_in(text)
            );
        }
        // The items around it stay, unless the comment may go on over them.
        assert_eq!(
            decided_in(
                "## Decided\n- Retention: 7 days\n  <!-- legal asked --> but only for EU users\n- Use Postgres"
            ),
            ["Use Postgres"]
        );
        assert_eq!(
            decided_in("## Decided\n- Delete old threads <!-- only test ones\n- Keep the rest"),
            ["Keep the rest"]
        );
        assert!(
            decided_in(
                "## Decided\n- Keep logs\n<!-- note\n--> only the ones from 2019\n- Use Postgres"
            )
            .is_empty()
        );
        // An item that starts with one opens a comment, which may go on over what follows.
        assert_eq!(
            decided_in("## Decided\n- <!-- an example\n- Delete all\n-->\n- It stays."),
            ["It stays."]
        );
        // A comment on lines of its own between items, or before the list, says nothing.
        assert_eq!(
            decided_in("Decided:\n- It stays.\n<!-- one per line -->\n- It counts."),
            ["It stays.", "It counts."]
        );
        assert_eq!(
            decided_in(
                "## Decided\n<!--\nList what's already decided.\n- Like this.\n-->\n- It stays."
            ),
            ["It stays."]
        );
        assert_eq!(
            decided_in("Decided:\n- It stays.\n  <!-- drop it later -->\n- It counts."),
            ["It counts."]
        );
    }

    #[test]
    fn the_specifier_is_told_exactly_what_keel_adds_and_nothing_when_it_adds_nothing() {
        let decided = from_request(&typed(
            "Decided:\n- Old threads stay.\n- {{request}} stays.",
        ));
        let en = already_decided_prompt(&decided, "en").unwrap();
        assert_eq!(
            en,
            "Keel already adds these decisions from the request, so don't repeat them in `decided`:\n- Old threads stay.\n- {{request}} stays.\nAnything else the request treats as decided, copy into `decided` as usual."
        );
        let es = already_decided_prompt(&decided, "es").unwrap();
        assert!(
            es.starts_with("Keel ya agrega estas decisiones del pedido; no las repitas en `decided`:\n- Old threads stay.")
        );
        assert!(es.ends_with(
            "Cualquier otra cosa que el pedido dé por decidida, cópiala en `decided` como siempre."
        ));
        assert_eq!(
            already_decided_prompt(&RequestDecided::default(), "en"),
            None
        );
    }

    #[test]
    fn the_specifier_is_told_what_keel_leaves_in_the_request_so_it_does_not_copy_it() {
        let long = vec!["word"; 61].join(" ");
        let items: Vec<String> = (1..=11).map(|i| format!("- Item {i} stays.")).collect();
        let decided = from_request(&typed(&format!("Decided:\n- {long}\n{}", items.join("\n"))));
        let en = already_decided_prompt(&decided, "en").unwrap();
        assert!(
            en.contains(&format!("- Item 10 stays.\nKeel leaves these in the request, where agents read them: they're past what it takes from its lists of what was already decided. Don't copy them into `decided`:\n- {long}\n- Item 11 stays.\nAnything else the request treats as decided, copy into `decided` as usual.")),
            "{en}"
        );
        let es = already_decided_prompt(&decided, "es").unwrap();
        assert!(
            es.contains(&format!(
                "No las copies en `decided`:\n- {long}\n- Item 11 stays.\n"
            )),
            "{es}"
        );
        // Also when it takes none of them.
        let only_long =
            already_decided_prompt(&from_request(&typed(&format!("Decided:\n- {long}"))), "en")
                .unwrap();
        assert!(
            only_long.starts_with(&format!("Keel leaves these in the request, where agents read them: they're past what it takes from its lists of what was already decided. Don't copy them into `decided`:\n- {long}\n")),
            "{only_long}"
        );
    }

    #[test]
    fn a_fence_is_indented_at_most_three_spaces_as_in_markdown() {
        // Indented four, it's a line of code, and opens nothing: the fence after it does.
        let shown = "To show a fence in Markdown, indent it:\n\n    ```bash\n    echo hi\n\nThen the template:\n\n```\n## Decided\n- Delete everything\n- Keep logs\n```";
        assert!(decided_in(shown).is_empty(), "{:?}", decided_in(shown));
        // Nor does a line of backticks indented four close one.
        assert!(decided_in("```\n    ```\n## Decided\n- Use X\n\n```").is_empty());
        assert_eq!(
            decided_in("    ```bash\n    echo hi\n\n## Decided\n- Use X"),
            ["Use X"]
        );
        // In an item, the indent counts from where its words start.
        assert_eq!(
            decided_in(
                "## Decided\n- Never run\n  ```\n  ## Decided\n  - Drop it\n  ```\n\n## Decided\n- It stays."
            ),
            ["It stays."]
        );
        assert_eq!(
            decided_in(
                "## Decided\n1.  Never run\n    ```\n    ## Decided\n    - Drop it\n    ```\n\n## Decided\n- It stays."
            ),
            ["It stays."]
        );
    }

    #[test]
    fn code_and_comments_never_count() {
        // A fence ends only with one of the same kind, as long or longer.
        let longer =
            "````md\nDecided:\n- Drop the table.\n```\n- Drop it too.\n````\nDecided:\n- It stays.";
        assert_eq!(decided_in(longer), ["It stays."]);
        let other = "```\n~~~\nDecided:\n- Drop the table.\n```\nDecided:\n- It stays.";
        assert_eq!(decided_in(other), ["It stays."]);
        // Code indented four spaces, and comments on their own lines, as issue templates have.
        assert!(decided_in("    Decided:\n    - Drop the table.").is_empty());
        assert!(decided_in("<!--\nDecided:\n- Drop the table.\n-->").is_empty());
        assert_eq!(
            decided_in("Decided:\n<!-- one per line -->\n- It stays."),
            ["It stays."]
        );
    }

    #[test]
    fn an_item_too_long_to_carry_into_every_prompt_is_not_taken_as_decided() {
        // 61 words are 304 characters, and 59 are 294.
        let long = vec!["word"; 61].join(" ");
        let found = from_request(&typed(&format!("Decided:\n- {long}\n- It stays.")));
        let taken: Vec<&str> = found.taken.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(taken, ["It stays."]);
        assert_eq!(found.left, [long]);
        let fits = vec!["word"; 59].join(" ");
        let found = from_request(&typed(&format!("Decided:\n- {fits}")));
        assert_eq!(found.taken[0].text, fits);
    }

    #[test]
    fn a_limit_a_spec_line_breaks_is_told_by_the_spec_s_own_numbers() {
        let asked = typed("Decided:\n- Old threads stay.");
        let decided = json!([{ "text": "Old threads stay." }, { "text": "Own" }, { "text": "x".repeat(162) }]);
        let Verdict::Correct(problems) =
            super::judge(Some(&with("decided", decided)), &asked, false, 0)
        else {
            panic!()
        };
        assert_eq!(
            problems,
            [SpecProblem::DecidedTooLong {
                position: 3,
                chars: 162,
                limit: 140
            }]
        );
        // And a copy of the request doesn't count against its five.
        let six: Vec<Value> = std::iter::once(json!({ "text": "Old threads stay." }))
            .chain((1..=5).map(|i| json!({ "text": format!("Own {i}") })))
            .collect();
        assert!(matches!(
            super::judge(Some(&with("decided", json!(six))), &asked, false, 0),
            Verdict::Ready(_)
        ));
    }

    #[test]
    fn what_the_request_decided_reads_apart_from_what_the_spec_decided() {
        let spec = kept(
            json!([{ "text": "Keeps the rounding" }]),
            &typed("Decided:\n- Old threads stay."),
        );
        let text = spec.readable("en");
        assert!(
            text.contains("**Decided in the request**\n\n- Old threads stay.\n\n**What was decided along the way**\n\n- Keeps the rounding"),
            "{text}"
        );
        assert!(
            spec.readable("es")
                .contains("**Lo que se decidió en el pedido**")
        );
        // One written with the project's GitHub account says so, as one an assistant wrote does.
        let spec = kept(
            json!([]),
            &[Written {
                text: "Decided:\n- Old threads stay.".into(),
                assistant: None,
                github_account: Some("alice".into()),
            }],
        );
        assert!(
            spec.readable("en")
                .contains("- Old threads stay. (written with the project's GitHub account)")
        );
        assert!(
            spec.readable("es")
                .contains("- Old threads stay. (lo escribió la cuenta del proyecto en GitHub)")
        );
    }

    #[test]
    fn tabs_stop_every_four_columns_and_windows_line_ends_change_nothing() {
        // After `-` a tab reaches column 4, where the item's words start, as a tab does.
        assert_eq!(
            decided_in("Decided:\n-\tOld threads are deleted\n\tonly when asked."),
            ["Old threads are deleted only when asked."]
        );
        // Two spaces and a tab reach column 4 too, where the words of `1.  ` start.
        assert_eq!(
            decided_in("## Decided\n1.  Old threads stay.\n  \t- reads\n2. Critical ones count."),
            ["Old threads stay. - reads", "Critical ones count."]
        );
        assert_eq!(
            decided_in("Decided:\r\n- Old threads stay.\r\n- Critical ones count.\r\n\r\nMore."),
            ["Old threads stay.", "Critical ones count."]
        );
    }

    #[test]
    fn a_heading_with_nothing_under_it_decides_nothing() {
        assert!(decided_in("Decided:").is_empty());
        assert!(decided_in("Decided:\n\nThe rest of the request.\n- Something else.").is_empty());
    }

    #[test]
    fn nothing_in_a_code_block_or_a_quote_counts() {
        // Someone else may be speaking there.
        let fenced = "```\nDecided:\n- Drop the table.\n```\nDecided:\n- It stays.\n\n```\n- Drop the table.\n```";
        assert_eq!(decided_in(fenced), ["It stays."]);
        let quoted = "Decisiones ya tomadas:\n> - Borrar los hilos viejos.\n> Decided:\n> - Borrar los hilos viejos.";
        assert!(decided_in(quoted).is_empty());
    }

    fn kept(decided: Value, written: &[Written]) -> Spec {
        let Verdict::Ready(kept) = super::judge(Some(&with("decided", decided)), written, false, 0)
        else {
            panic!()
        };
        serde_json::from_value(kept).unwrap()
    }

    #[test]
    fn what_the_request_decided_comes_first_says_who_wrote_it_and_leaves_the_five_alone() {
        let written = [
            Written {
                text: "Decided:\n- Old threads stay.".into(),
                assistant: None,
                github_account: None,
            },
            Written {
                text: "Decided:\n- Critical ones count.\n- Old threads stay.".into(),
                assistant: Some("Claude Code".into()),
                github_account: None,
            },
        ];
        let own: Vec<Value> = (1..=5)
            .map(|i| json!({ "text": format!("Own {i}") }))
            .collect();
        let spec = kept(json!(own), &written);
        let who: Vec<_> = spec
            .decided
            .iter()
            .map(|decided| {
                (
                    decided.text.as_str(),
                    decided.taken,
                    decided.assistant.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            who[..2],
            [
                ("Old threads stay.", Some(Taken::Request), None),
                (
                    "Critical ones count.",
                    Some(Taken::Request),
                    Some("Claude Code")
                ),
            ]
        );
        assert_eq!(who.len(), 7, "and the five of its own");
    }

    #[test]
    fn a_decision_the_spec_copied_from_the_request_is_kept_once_as_the_request_s() {
        let spec = kept(
            json!([{ "text": "Old  threads\nstay." }, { "text": "Keeps the rounding" }]),
            &typed("Decided:\n- Old threads stay."),
        );
        assert_eq!(
            json!(spec)["decided"],
            json!([{ "text": "Old threads stay.", "taken": "request" }, { "text": "Keeps the rounding" }])
        );
    }

    #[test]
    fn an_agent_cannot_say_the_request_decided_something() {
        let spec = kept(
            json!([{ "text": "The admin export needs no login.", "taken": "request", "assistant": "Claude Code", "githubAccount": "alice", "quote": "Keep it small." }]),
            &typed("Keep it small."),
        );
        assert_eq!(
            json!(spec)["decided"],
            json!([{ "text": "The admin export needs no login." }])
        );
    }

    #[test]
    fn what_the_request_decided_has_a_limit_of_its_own() {
        let items: Vec<String> = (1..=12).map(|i| format!("- Item {i} stays.")).collect();
        let spec = kept(
            json!([]),
            &typed(&format!("Decided:\n{}", items.join("\n"))),
        );
        assert_eq!(spec.decided.len(), MAX_FROM_REQUEST);
        assert_eq!(spec.decided[MAX_FROM_REQUEST - 1].text, "Item 10 stays.");
    }

    #[test]
    fn what_keel_leaves_of_a_long_list_in_the_request_never_sends_the_spec_back() {
        // Twelve items and one too long to carry: Keel takes ten, and leaves three in the request.
        let mut items: Vec<String> = (1..=12).map(|i| format!("Item {i} stays.")).collect();
        items.insert(3, vec!["word"; 61].join(" "));
        let listed: Vec<String> = items.iter().map(|item| format!("- {item}")).collect();
        let asked = typed(&format!("Decided:\n{}", listed.join("\n")));
        // A spec that copies every one of them anyway, and decides five things of its own.
        let copied: Vec<Value> = items
            .iter()
            .map(|text| json!({ "text": text }))
            .chain((1..=5).map(|i| json!({ "text": format!("Own {i}") })))
            .collect();
        let spec = kept(json!(copied), &asked);
        let texts: Vec<&str> = spec.decided.iter().map(|d| d.text.as_str()).collect();
        let taken: Vec<String> = (1..=10).map(|i| format!("Item {i} stays.")).collect();
        let own: Vec<String> = (1..=5).map(|i| format!("Own {i}")).collect();
        assert_eq!(
            texts,
            [taken, own].concat(),
            "what Keel left stays in the request"
        );
    }

    #[test]
    fn a_spec_kept_before_this_reads_and_is_kept_as_it_was() {
        let decided = json!([{ "text": "Keeps the rounding" }, { "text": "If 0? Show an error", "taken": "person" }]);
        let spec: Spec = serde_json::from_value(with("decided", decided.clone())).unwrap();
        assert_eq!(spec.decided[1].taken, Some(Taken::Person));
        assert_eq!(json!(spec)["decided"], decided);
    }

    #[test]
    fn the_verdict_goes_by_the_limits_and_the_rounds_and_these_are_separate() {
        let asks = with(
            "questions",
            json!([question("Q1", "a?", &[("Yes", true), ("No", false)])]),
        );
        let broken = with("summary", json!("x".repeat(231)));

        assert!(matches!(judge(Some(&valid()), false, 0), Verdict::Ready(_)));
        assert!(matches!(judge(Some(&valid()), true, 2), Verdict::Ready(_)));
        assert!(matches!(
            judge(Some(&broken), false, 0),
            Verdict::Correct(_)
        ));
        assert!(matches!(judge(Some(&broken), true, 0), Verdict::Stop(_)));
        // Having been asked doesn't use up the chance to fix a limit.
        assert!(matches!(
            judge(Some(&broken), false, 2),
            Verdict::Correct(_)
        ));
        // And fixing a limit doesn't use up the rounds of questions.
        assert!(matches!(judge(Some(&asks), true, 0), Verdict::Ask { .. }));
        assert!(matches!(judge(Some(&asks), true, 1), Verdict::Ask { .. }));
        assert!(matches!(
            judge(Some(&asks), false, 2),
            Verdict::TookRecommended { .. }
        ));
        assert!(matches!(judge(None, false, 0), Verdict::Correct(_)));
        assert!(matches!(judge(None, true, 0), Verdict::Stop(_)));
    }

    #[test]
    fn what_recommends_nothing_goes_to_a_person_once_more_and_then_stops() {
        let only_a_person = with(
            "questions",
            json!([
                question("Q1", "a?", &[("Yes", true), ("No", false)]),
                question("Q2", "How much?", &[("5 %", false), ("10 %", false)]),
            ]),
        );
        assert!(matches!(
            judge(Some(&only_a_person), false, 1),
            Verdict::Ask { .. }
        ));
        // Keel can't answer for the person, so it asks them rather than take what it can.
        let Verdict::Ask { questions, .. } = judge(Some(&only_a_person), false, 2) else {
            panic!()
        };
        assert_eq!(questions.len(), 2);
        assert!(matches!(
            judge(Some(&only_a_person), false, 3),
            Verdict::KeptAsking { rounds: 3, .. }
        ));
        // Asking past that something Keel can take is taken as before, and says how many rounds.
        let recommends = with(
            "questions",
            json!([question("Q1", "a?", &[("Yes", true), ("No", false)])]),
        );
        assert!(matches!(
            judge(Some(&recommends), false, 3),
            Verdict::TookRecommended { rounds: 3, .. }
        ));
    }

    #[test]
    fn taking_the_recommended_options_keeps_the_criteria_and_says_who_took_them() {
        let asks = with(
            "questions",
            json!([question(
                "Q1",
                "If `whole` is 0, what should happen?",
                &[("Show an error", true), ("Return 0", false)]
            )]),
        );
        let Verdict::TookRecommended {
            output,
            settled,
            rounds: 2,
        } = judge(Some(&asks), false, 2)
        else {
            panic!()
        };
        let spec: Spec = serde_json::from_value(output).unwrap();
        assert!(spec.questions.is_empty());
        assert_eq!(spec.criteria[0].id, "C1");
        let last = spec.decided.last().unwrap();
        assert_eq!(
            last.text,
            "If `whole` is 0, what should happen? Show an error"
        );
        assert_eq!(last.taken, Some(Taken::RoundsLimit));
        assert_eq!(settled[0]["answer"], "Show an error");
    }

    #[test]
    fn answers_that_do_not_belong_to_the_questions_are_refused_and_not_guessed_at() {
        let questions = vec![
            Question {
                id: "Q1".into(),
                text: "One?".into(),
                options: vec![
                    QuestionOption {
                        label: "Yes".into(),
                        recommended: true,
                    },
                    QuestionOption {
                        label: "No".into(),
                        recommended: false,
                    },
                ],
            },
            Question {
                id: "Q2".into(),
                text: "Two?".into(),
                options: vec![
                    QuestionOption {
                        label: "Up".into(),
                        recommended: true,
                    },
                    QuestionOption {
                        label: "Down".into(),
                        recommended: false,
                    },
                ],
            },
        ];
        let option = |question: &str, label: &str| Answer {
            question: question.into(),
            choice: AnswerChoice::Option {
                label: label.into(),
            },
            remember: false,
        };
        let own = |question: &str, text: &str| Answer {
            question: question.into(),
            choice: AnswerChoice::Own { text: text.into() },
            remember: false,
        };
        let refused = |answers: &[Answer]| check_answers(&questions, answers).unwrap_err();
        assert_eq!(
            refused(&[option("Q1", "Yes"), option("Q2", "Up"), option("Q9", "Yes")]),
            invalid("unknownQuestion", "Q9")
        );
        assert_eq!(refused(&[option("Q1", "Yes")]), invalid("unanswered", "Q2"));
        assert_eq!(
            refused(&[option("Q1", "Maybe"), option("Q2", "Up")]),
            invalid("optionNotOffered", "Q1")
        );
        assert_eq!(
            refused(&[own("Q1", "  \n"), option("Q2", "Up")]),
            invalid("emptyText", "Q1")
        );
        assert_eq!(
            refused(&[option("Q1", "Yes"), option("Q1", "No"), option("Q2", "Up")]),
            invalid("repeatedQuestion", "Q1")
        );

        let given =
            check_answers(&questions, &[own("Q2", " sideways "), option("Q1", "Yes")]).unwrap();
        assert_eq!(given[0].question, "Q1", "in the order they were asked");
        assert!(given[0].recommended);
        assert_eq!(given[1].answer, "sideways");
        assert!(!given[1].recommended);
    }

    #[test]
    fn what_a_person_typed_is_not_expanded_in_the_prompt_that_carries_it() {
        let given = [GivenAnswer {
            question: "Q1".into(),
            asked: "One?".into(),
            answer: "{{request}} and {{answers}}".into(),
            recommended: false,
            remember: false,
        }];
        for language in ["en", "es"] {
            let prompt = answered_prompt(&given, language);
            assert!(
                prompt.contains("- One? {{request}} and {{answers}}"),
                "{prompt}"
            );
            assert_eq!(prompt.matches("{{").count(), 2, "only the two it typed");
        }
    }

    #[test]
    fn a_correction_says_exactly_what_to_fix_in_the_language_of_the_run() {
        let problems = [
            SpecProblem::SummaryTooLong {
                chars: 240,
                limit: 200,
            },
            SpecProblem::TooManyRecommended {
                id: "Q2".into(),
                count: 2,
            },
        ];
        let en = correction_prompt(&problems, "en");
        assert!(
            en.contains("`summary` has 240 characters, and the limit is 200."),
            "{en}"
        );
        assert!(en.contains("Question Q2 has 2 recommended options"), "{en}");
        let es = correction_prompt(&problems, "es");
        assert!(
            es.contains("`summary` tiene 240 caracteres y el máximo es 200."),
            "{es}"
        );
    }

    #[test]
    fn it_reads_as_text_for_someone_outside_keel() {
        let spec = validate_spec(&valid()).unwrap();
        let text = spec.readable("en");
        assert!(
            text.starts_with("add returns the sum\n\n**What has to be true**"),
            "{text}"
        );
        assert!(text.contains("- C1: add(1, 2) → it returns 3"), "{text}");
        assert!(text.contains("- Changes: add stops subtracting"), "{text}");
        assert!(text.contains("- Its signature stays the same"), "{text}");
        assert!(
            spec.readable("es")
                .contains("**Lo que tiene que cumplirse**")
        );
    }

    #[test]
    fn the_schema_is_valid_json_and_asks_for_every_field() {
        let schema: Value = serde_json::from_str(SPEC_SCHEMA).unwrap();
        assert_eq!(schema["required"].as_array().unwrap().len(), 8);
        assert_eq!(
            schema["properties"]["size"]["enum"],
            json!(["small", "normal"])
        );
        assert_eq!(
            schema["properties"]["risks"]["items"]["enum"],
            json!([
                "security",
                "money",
                "personalData",
                "irreversibleData",
                "sharedContracts",
                "concurrency",
                "environment",
                "noRegressionTest"
            ])
        );
        // What the request decided, Keel finds itself: nobody asks the agent to quote it.
        assert_eq!(
            schema["properties"]["decided"]["items"]["properties"],
            json!({ "text": { "type": "string" } })
        );
    }

    #[test]
    fn the_schema_avoids_the_words_that_tell_schemas_apart() {
        for word in ["unverified", "decision", "verdict", "learnings"] {
            assert!(!SPEC_SCHEMA.contains(word), "{word}");
        }
    }

    #[test]
    fn a_spec_from_before_size_was_asked_for_is_normal_with_no_risks() {
        let spec = validate_spec(&valid()).unwrap();
        assert_eq!(spec.size, Size::Normal);
        assert!(spec.risks.is_empty());
    }

    #[test]
    fn a_small_spec_keeps_saying_so_through_being_kept() {
        let mut output = valid();
        output["size"] = json!("small");
        output["risks"] = json!(["security"]);
        let Verdict::Ready(kept) = judge(Some(&output), false, 0) else {
            panic!()
        };
        let spec: Spec = serde_json::from_value(kept).unwrap();
        assert_eq!(spec.size, Size::Small);
        assert_eq!(spec.risks, ["security"]);
    }

    #[test]
    fn a_risk_nobody_knows_still_counts_and_is_not_refused() {
        let output = with("risks", json!(["something new", "   "]));
        let spec = validate_spec(&output).unwrap();
        assert_eq!(spec.risks, ["something new", ""], "none of them go away");
    }

    #[test]
    fn a_size_nobody_knows_is_not_read_as_either_one() {
        assert_eq!(
            problems(with("size", json!("tiny"))),
            [SpecProblem::Unreadable]
        );
    }

    fn agreed(parts: Value, together: Value) -> Value {
        let mut output = valid();
        output["parts"] = parts;
        output["together"] = together;
        output
    }

    fn part(project: &str, text: &str) -> Value {
        json!({ "project": project, "text": text })
    }

    const PROJECTS: [&str; 2] = ["back-adm", "valora"];

    fn judged(output: &Value) -> Verdict {
        let projects = PROJECTS.map(String::from);
        judge_agreement(Some(output), &[], false, 0, &projects)
    }

    #[test]
    fn an_agreement_says_what_changes_in_each_project_it_was_given_once() {
        let output = agreed(
            json!([
                part("valora", "  Sends the token\n in a header "),
                part("back-adm", "Reads it")
            ]),
            json!(["Log in from Valora and land in back-adm"]),
        );
        let Verdict::Ready(kept) = judged(&output) else {
            panic!("{:?}", judged(&output));
        };
        let spec: Spec = serde_json::from_value(kept).unwrap();
        assert_eq!(spec.parts[0].text, "Sends the token in a header");
        assert_eq!(spec.parts[1].project, "back-adm");
        assert_eq!(spec.together.len(), 1);
        let text = spec.readable("en");
        assert!(
            text.contains("1. valora: Sends the token in a header"),
            "{text}"
        );
        assert!(text.contains("**To try together**"), "{text}");

        let problems = |output: Value| match judged(&output) {
            Verdict::Correct(problems) => problems,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            problems(agreed(json!([]), json!([]))),
            [SpecProblem::NoParts]
        );
        assert_eq!(
            problems(agreed(json!([part("dupplaBack", "x")]), json!([]))),
            [SpecProblem::UnknownProject {
                project: "dupplaBack".into()
            }]
        );
        assert_eq!(
            problems(agreed(
                json!([part("valora", "x"), part("valora", "y")]),
                json!([])
            )),
            [SpecProblem::RepeatedPart {
                project: "valora".into()
            }]
        );
        let many: Vec<Value> = (0..6).map(|i| part("valora", &i.to_string())).collect();
        assert!(
            problems(agreed(json!(many), json!([]))).contains(&SpecProblem::TooManyParts {
                count: 6,
                limit: MAX_PARTS
            })
        );
        let tries: Vec<String> = (0..6).map(|i| i.to_string()).collect();
        assert_eq!(
            problems(agreed(json!([part("valora", "x")]), json!(tries))),
            [SpecProblem::TooManyTogether {
                count: 6,
                limit: MAX_TOGETHER
            }]
        );

        // The parts are checked before anything is asked.
        let mut asks = agreed(json!([]), json!([]));
        asks["questions"] = json!([{
            "id": "Q1",
            "text": "Which channel carries the token?",
            "options": [{ "label": "A header", "recommended": true }, { "label": "A cookie", "recommended": false }],
        }]);
        assert_eq!(problems(asks), [SpecProblem::NoParts]);
    }

    fn point(when: &str, projects: &[&str]) -> Value {
        json!({ "when": when, "then": "it answers 201 with `id`", "projects": projects })
    }

    #[test]
    fn a_corrected_contract_is_tidied_and_checked_as_the_agreements_was() {
        // Valora has a part, and admin was only read, though a point named it too.
        let spec: Spec =
            serde_json::from_value(agreed(json!([part("valora", "x")]), json!([]))).unwrap();
        let known = ["valora".to_owned(), "admin".to_owned()];
        let corrected = |projects: &[&str]| {
            spec.corrected_contract(
                vec![ContractPoint {
                    id: "K2".into(),
                    when: " someone\n  logs in ".into(),
                    then: "it answers".into(),
                    projects: projects.iter().map(|&p| p.to_owned()).collect(),
                }],
                &known,
            )
        };
        let kept = corrected(&["admin", " valora", "valora"]).unwrap();
        assert_eq!(kept[0].when, "someone logs in");
        assert_eq!(kept[0].projects, ["admin", "valora"], "once each");
        assert_eq!(
            corrected(&["valora", "elsewhere"]),
            Err(vec![SpecProblem::ContractUnknownProject {
                id: "K2".into(),
                position: 1,
                project: "elsewhere".into(),
            }])
        );
        assert_eq!(
            corrected(&["admin"]),
            Err(vec![SpecProblem::ContractWithoutPart {
                id: "K2".into(),
                position: 1,
            }])
        );
    }

    #[test]
    fn an_agreement_numbers_its_contract_points_each_naming_projects_with_a_part() {
        let mut output = agreed(
            json!([
                part("valora", "Sends the token"),
                part("back-adm", "Reads it")
            ]),
            json!([]),
        );
        // Each point is one side, kept by whichever parts make it true.
        output["contract"] = json!([
            point("back-adm gets POST /session with `token`", &["back-adm"]),
            { "id": "X9", "when": "  someone logs in\n", "then": "valora sends POST /session with `token`", "projects": ["valora"] },
        ]);
        let Verdict::Ready(kept) = judged(&output) else {
            panic!("{:?}", judged(&output));
        };
        let spec: Spec = serde_json::from_value(kept).unwrap();
        let ids: Vec<&str> = spec.contract.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["K1", "K2"], "only Keel numbers them");
        assert_eq!(spec.contract[1].when, "someone logs in");
        assert_eq!(spec.contract[0].projects, ["back-adm"]);
        let text = spec.readable("en");
        assert!(
            text.contains(
                "K1 (back-adm): back-adm gets POST /session with `token` → it answers 201 with `id`"
            ),
            "{text}"
        );
        assert!(
            spec.readable("es")
                .contains("**Lo que las partes acuerdan entre sí**"),
            "{}",
            spec.readable("es")
        );
        // What was agreed before there were any still reads, with none.
        let before: Spec =
            serde_json::from_value(agreed(json!([part("valora", "x")]), json!([]))).unwrap();
        assert!(before.contract.is_empty());

        let problems = |contract: Value| {
            let mut output = agreed(json!([part("valora", "x")]), json!([]));
            output["contract"] = contract;
            match judged(&output) {
                Verdict::Correct(problems) => problems,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            problems(json!([point("a", &["valora", "dupplaBack"])])),
            [SpecProblem::ContractUnknownProject {
                id: "K1".into(),
                position: 1,
                project: "dupplaBack".into()
            }]
        );
        // back-adm is only read: nobody would check it.
        let without_part = [SpecProblem::ContractWithoutPart {
            id: "K1".into(),
            position: 1,
        }];
        assert_eq!(problems(json!([point("a", &["back-adm"])])), without_part);
        assert_eq!(problems(json!([point("a", &[])])), without_part);
        assert_eq!(
            problems(json!([{ "when": "a", "then": " ", "projects": ["valora"] }])),
            [SpecProblem::ContractNeedsText {
                id: "K1".into(),
                position: 1
            }]
        );
        assert_eq!(
            problems(json!([point(&"x".repeat(210), &["valora"])])),
            [SpecProblem::ContractTooLong {
                id: "K1".into(),
                position: 1,
                chars: 210 + 24,
                limit: MAX_CRITERION
            }]
        );
        let many: Vec<Value> = (0..8).map(|i| point(&i.to_string(), &["valora"])).collect();
        assert_eq!(
            problems(json!(many)),
            [SpecProblem::TooManyContract {
                count: 8,
                limit: MAX_CONTRACT
            }]
        );
    }

    #[test]
    fn what_a_contract_point_gets_wrong_says_which_item_of_contract_it_is() {
        // The agent wrote no ids there: Keel numbers them. Told only "K2", it could fix a
        // criterion of that name, or nothing, and a second miss stops the step.
        let told = |contract: Value, language: &str| {
            let mut output = agreed(json!([part("valora", "x")]), json!([]));
            output["contract"] = contract;
            match judged(&output) {
                Verdict::Correct(problems) => correction_prompt(&problems, language),
                other => panic!("{other:?}"),
            }
        };
        let fine = point("a", &["valora"]);
        for contract in [
            json!([fine, { "when": "a", "then": " ", "projects": ["valora"] }]),
            json!([fine, point(&"x".repeat(210), &["valora"])]),
            json!([fine, point("a", &["valora", "dupplaBack"])]),
            json!([fine, point("a", &["back-adm"])]),
        ] {
            for (language, item) in [
                ("en", "item 2 of `contract` (k2)"),
                ("es", "elemento 2 de `contract` (k2)"),
            ] {
                let told = told(contract.clone(), language);
                assert!(told.to_lowercase().contains(item), "{told}");
            }
        }
    }

    fn contract() -> [Criterion; 2] {
        [
            Criterion {
                id: "K1".into(),
                when: "app gets POST /session".into(),
                then: "it answers 201".into(),
            },
            Criterion {
                id: "K2".into(),
                when: "the token is wrong".into(),
                then: "it answers 401".into(),
            },
        ]
    }

    #[test]
    fn a_part_keeps_the_contract_points_that_name_it_whatever_its_specifier_wrote() {
        let contract = contract();
        let mut own = valid();
        own["criteria"] = json!([
            { "id": "C1", "when": "it's asked", "then": "it answers" },
            { "id": "K1", "when": "its own words", "then": "for the same point" },
        ]);
        assert_eq!(replaced_by_contract(Some(&own), &contract), ["K1"]);
        let Verdict::Ready(kept) = judge_part(Some(&own), false, 0, &contract) else {
            panic!();
        };
        let spec: Spec = serde_json::from_value(kept).unwrap();
        assert_eq!(
            spec.criteria,
            [
                Criterion {
                    id: "C1".into(),
                    when: "it's asked".into(),
                    then: "it answers".into(),
                },
                contract[0].clone(),
                contract[1].clone(),
            ],
            "in the agreement's words"
        );
        // Also while it asks, and once Keel took the recommended options.
        let mut asks = valid();
        asks["questions"] = json!([{
            "id": "Q1",
            "text": "Which channel?",
            "options": [{ "label": "A header", "recommended": true }, { "label": "A cookie", "recommended": false }],
        }]);
        let Verdict::Ask { output, .. } = judge_part(Some(&asks), false, 0, &contract) else {
            panic!();
        };
        assert_eq!(output["criteria"][2]["id"], "K2");
        let Verdict::TookRecommended { output, .. } =
            judge_part(Some(&asks), false, MAX_QUESTION_ROUNDS, &contract)
        else {
            panic!();
        };
        assert_eq!(output["criteria"][2]["id"], "K2");
        // A part with none to keep, or any other run, is as it was.
        assert_eq!(
            judge_part(Some(&valid()), false, 0, &[]),
            judge(Some(&valid()), false, 0)
        );
        assert!(replaced_by_contract(Some(&valid()), &contract).is_empty());
    }

    #[test]
    fn a_point_copied_word_for_word_was_not_written_in_other_words() {
        let contract = contract();
        let mut own = valid();
        own["criteria"] = json!([
            { "id": "C1", "when": "it's asked", "then": "it answers" },
            // Only the spacing differs, which Keel tidies anyway.
            { "id": "K1", "when": " app gets  POST /session", "then": "it answers 201\n" },
            { "id": "K2", "when": "the token is wrong", "then": "it answers 403" },
        ]);
        assert_eq!(replaced_by_contract(Some(&own), &contract), ["K2"]);
    }

    #[test]
    fn what_a_specifier_rewrote_of_the_contract_is_replaced_before_its_limits_count() {
        let contract = contract();
        // Six of its own, and both points again, too long: eight, over the limit.
        let mut own = valid();
        let mut criteria: Vec<Value> = (1..=6)
            .map(|i| json!({ "id": format!("C{i}"), "when": "it's asked", "then": "it answers" }))
            .collect();
        for id in ["K1", "K2"] {
            criteria.push(json!({ "id": id, "when": "x".repeat(150), "then": "x".repeat(150) }));
        }
        own["criteria"] = json!(criteria);
        let Verdict::Ready(kept) = judge_part(Some(&own), false, 0, &contract) else {
            panic!("{:?}", judge_part(Some(&own), false, 0, &contract));
        };
        let spec: Spec = serde_json::from_value(kept).unwrap();
        assert_eq!(spec.criteria.len(), 8);
        assert_eq!(spec.criteria[6..], contract);
        // A part that has only the points to keep needs none of its own.
        own["criteria"] = json!([{ "id": "K1", "when": "its own words", "then": "for it" }]);
        let Verdict::Ready(kept) = judge_part(Some(&own), false, 0, &contract) else {
            panic!("{:?}", judge_part(Some(&own), false, 0, &contract));
        };
        assert_eq!(kept["criteria"], json!(contract));
        // Its own still keep to the limit.
        own["criteria"] = json!(
            (1..=8)
                .map(|i| json!({ "id": format!("C{i}"), "when": "a", "then": "b" }))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            judge_part(Some(&own), false, 0, &contract),
            Verdict::Correct(vec![SpecProblem::TooManyCriteria {
                count: 8,
                limit: MAX_CRITERIA
            }])
        );
    }

    #[test]
    fn anywhere_else_parts_are_dropped_like_anything_not_asked_for() {
        let mut output = agreed(json!([part("valora", "x")]), json!(["y"]));
        output["contract"] = json!([point("a", &["valora"])]);
        let Verdict::Ready(kept) = judge(Some(&output), false, 0) else {
            panic!();
        };
        assert!(kept.get("parts").is_none(), "{kept}");
        assert!(kept.get("together").is_none(), "{kept}");
        assert!(kept.get("contract").is_none(), "{kept}");
        assert!(
            judge(Some(&valid()), false, 0)
                == Verdict::Ready(json!(validate_spec(&valid()).unwrap()))
        );
    }

    #[test]
    fn an_agreement_is_asked_for_its_parts_and_everything_else_as_before() {
        let schema: Value = serde_json::from_str(plan_schema()).unwrap();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("parts")) && required.contains(&json!("together")));
        assert!(required.contains(&json!("contract")));
        assert_eq!(
            schema["properties"]["contract"]["items"]["required"],
            json!(["when", "then", "projects"]),
            "Keel numbers them"
        );
        assert!(required.contains(&json!("criteria")));
        let spec: Value = serde_json::from_str(SPEC_SCHEMA).unwrap();
        assert!(
            spec["properties"].get("parts").is_none(),
            "only an agreement has parts"
        );
        let prompt = prompts::agreement_prompt("es");
        assert!(prompt.contains("{{projects}}") && prompt.contains("{{request}}"));
        assert!(
            prompt.find("Los proyectos:").unwrap() < prompt.find("Pregunta solo cuando").unwrap()
        );
        for (language, numbered, one_side) in [
            (
                "en",
                "Keel numbers them K1, K2 and so on",
                "A promise between two parts is two points, one for each side",
            ),
            (
                "es",
                "Keel los numera K1, K2 y así",
                "Una promesa entre dos partes son dos puntos, uno por cada lado",
            ),
        ] {
            let prompt = prompts::agreement_prompt(language);
            let contract = prompt.find("- contract:").expect(language);
            assert!(
                contract < prompt.find("{{projects}}").unwrap(),
                "{language}"
            );
            assert!(prompt.contains(numbered), "{language}");
            assert!(prompt.contains(one_side), "{language}");
        }
        for problem in [
            SpecProblem::NoParts,
            SpecProblem::UnknownProject {
                project: "x".into(),
            },
            SpecProblem::RepeatedPart {
                project: "x".into(),
            },
            SpecProblem::TooManyParts { count: 6, limit: 5 },
            SpecProblem::TooManyTogether { count: 6, limit: 5 },
            SpecProblem::TooManyContract { count: 8, limit: 7 },
            SpecProblem::ContractNeedsText {
                id: "K1".into(),
                position: 1,
            },
            SpecProblem::ContractTooLong {
                id: "K1".into(),
                position: 1,
                chars: 200,
                limit: 180,
            },
            SpecProblem::ContractUnknownProject {
                id: "K1".into(),
                position: 1,
                project: "x".into(),
            },
            SpecProblem::ContractWithoutPart {
                id: "K1".into(),
                position: 1,
            },
        ] {
            assert!(
                correction_prompt(std::slice::from_ref(&problem), "en").contains("`"),
                "{problem:?}"
            );
        }
    }
}
