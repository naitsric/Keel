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
const MAX_QUESTIONS: usize = 3;
const MAX_QUESTION_TEXT: usize = 140;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_OPTION_LABEL: usize = 60;
const MAX_PARTS: usize = 5;
const MAX_TOGETHER: usize = 5;
/// After this many, the engine takes the recommended options rather than asking a third time. A
/// question with none recommended can only be answered by a person, so it gets one round more.
pub const MAX_QUESTION_ROUNDS: usize = 2;

/// `size` and `risks` are a prediction, for a workflow with steps that depend on the change. The schema
/// says what small means as well as the prompt, since any specifier gets asked for it, whatever its
/// prompt says. Its words keep clear of those the test fakes tell schemas apart by.
pub const SPEC_SCHEMA: &str = r#"{"type":"object","properties":{"size":{"type":"string","enum":["small","normal"],"description":"small only if it is understood with nothing left to ask, its risk is contained, and someone could pick the work up from just the request and the change. How much code it touches does not decide this: one changed line in permissions is not small. If in doubt, normal."},"risks":{"type":"array","items":{"type":"string","enum":["security","money","personalData","irreversibleData","sharedContracts","concurrency","environment","noRegressionTest"]}},"summary":{"type":"string"},"criteria":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"when":{"type":"string"},"then":{"type":"string"}},"required":["id","when","then"],"additionalProperties":false}},"changes":{"type":"array","items":{"type":"object","properties":{"kind":{"type":"string","enum":["adds","changes","removes"]},"text":{"type":"string"}},"required":["kind","text"],"additionalProperties":false}},"limits":{"type":"array","items":{"type":"string"}},"decided":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"quote":{"type":"string","description":"Only if the request itself settles it in so many words: those words, copied exactly."}},"required":["text"],"additionalProperties":false}},"questions":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"recommended":{"type":"boolean","description":"true for one option at most, and only when the code or the request backs it up. When only the person can know, such as an amount, a date or a business rule, none is: never make up a value."}},"required":["label","recommended"],"additionalProperties":false}}},"required":["id","text","options"],"additionalProperties":false}}},"required":["size","risks","summary","criteria","changes","limits","decided","questions"],"additionalProperties":false}"#;

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
    if let Some(required) = schema["required"].as_array_mut() {
        required.extend([json!("parts"), json!("together")]);
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
    /// The person wrote it in the request: its `quote` is there, word for word.
    Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decided {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<Taken>,
    /// The words of the request that settle it, as the specifier quoted them. Kept only if the
    /// request has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
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
            if chars(text) > MAX_LISTED_TEXT {
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
        for risk in &mut self.risks {
            tidy(risk);
        }
        for decided in &mut self.decided {
            tidy(&mut decided.text);
            // Only the engine says who settled something.
            decided.taken = None;
            decided.quote = decided
                .quote
                .as_deref()
                .map(one_line)
                .filter(|quote| !quote.is_empty());
        }
        for question in &mut self.questions {
            tidy(&mut question.id);
            tidy(&mut question.text);
            for option in &mut question.options {
                tidy(&mut option.label);
            }
        }
    }

    fn problems(&self) -> Vec<SpecProblem> {
        let mut problems = Vec::new();
        if self.summary.is_empty() {
            problems.push(SpecProblem::NoSummary);
        } else if chars(&self.summary) > MAX_SUMMARY {
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
            if length > MAX_CRITERION {
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

        if self.decided.len() > MAX_DECIDED {
            problems.push(SpecProblem::TooManyDecided {
                count: self.decided.len(),
                limit: MAX_DECIDED,
            });
        }
        for (index, decided) in self.decided.iter().enumerate() {
            if chars(&decided.text) > MAX_DECIDED_TEXT {
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
        if chars(&question.text) > MAX_QUESTION_TEXT {
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
            if chars(&option.label) > MAX_OPTION_LABEL {
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

    /// What the specifier quoted from the request is the person's, if the request says it word for
    /// word, however it's spaced or broken into lines. A quote it doesn't have goes, and what it
    /// quoted for stays the specification's own.
    fn take_from_request(&mut self, request: &str) {
        let request = one_line(request);
        for decided in &mut self.decided {
            match &decided.quote {
                Some(quote) if request.contains(quote.as_str()) => {
                    decided.taken = Some(Taken::Request);
                }
                _ => decided.quote = None,
            }
        }
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
                quote: None,
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
        section(
            say(
                "What was decided along the way",
                "Lo que se decidió sobre la marcha",
            ),
            self.decided
                .iter()
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
        text
    }
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
fn validate_spec_for(
    output: &Value,
    projects: Option<&[String]>,
) -> Result<Spec, Vec<SpecProblem>> {
    let mut spec: Spec =
        serde_json::from_value(output.clone()).map_err(|_| vec![SpecProblem::Unreadable])?;
    spec.normalize();
    let mut problems = spec.problems();
    match projects {
        Some(projects) => problems.extend(spec.part_problems(projects)),
        None => {
            spec.parts.clear();
            spec.together.clear();
        }
    }
    if problems.is_empty() {
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

/// `after_correction` is whether this result was itself the fix for a broken limit, and
/// `rounds_answered` how many rounds of questions a person has already answered. They're separate
/// on purpose: fixing a limit doesn't use up the person's chance to be asked, and being asked doesn't
/// use up the fix a later result gets.
///
/// Once the rounds run out, Keel only answers for the person when every question recommends an
/// option. A question with none goes to the person once more, and asking past that stops the step,
/// so it can't go on asking forever and Keel never makes up an answer.
///
/// `request` is the run's, which is what a decision can quote to count as the person's.
pub fn judge(
    output: Option<&Value>,
    request: &str,
    after_correction: bool,
    rounds_answered: usize,
) -> Verdict {
    judge_for(output, request, after_correction, rounds_answered, None)
}

/// [`judge`] where a change across these projects is agreed. Its parts are checked before anything
/// is asked: once a person answers with every recommended option, nothing judges it again.
pub fn judge_agreement(
    output: Option<&Value>,
    request: &str,
    after_correction: bool,
    rounds_answered: usize,
    projects: &[String],
) -> Verdict {
    judge_for(
        output,
        request,
        after_correction,
        rounds_answered,
        Some(projects),
    )
}

fn judge_for(
    output: Option<&Value>,
    request: &str,
    after_correction: bool,
    rounds_answered: usize,
    projects: Option<&[String]>,
) -> Verdict {
    let mut spec = match output.map(|output| validate_spec_for(output, projects)) {
        Some(Ok(spec)) => spec,
        Some(Err(problems)) if after_correction => return Verdict::Stop(problems),
        Some(Err(problems)) => return Verdict::Correct(problems),
        None if after_correction => return Verdict::Stop(vec![SpecProblem::Unreadable]),
        None => return Verdict::Correct(vec![SpecProblem::Unreadable]),
    };
    spec.take_from_request(request);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_spec(output: &Value) -> Result<Spec, Vec<SpecProblem>> {
        validate_spec_for(output, None)
    }

    /// [`super::judge`] for a request with nothing in it a spec could quote.
    fn judge(output: Option<&Value>, after_correction: bool, rounds_answered: usize) -> Verdict {
        super::judge(output, "", after_correction, rounds_answered)
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
            problems(with("summary", json!("ñ".repeat(MAX_SUMMARY + 1)))),
            [SpecProblem::SummaryTooLong {
                chars: 201,
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
            problems(with("criteria", json!([criterion(1, 150, 51)]))),
            [SpecProblem::CriterionTooLong {
                id: "C1".into(),
                chars: 201,
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
            problems(with("decided", json!([one("x".repeat(141))]))),
            [SpecProblem::DecidedTooLong {
                position: 1,
                chars: 141,
                limit: 140
            }]
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

        // Written past the 120 it was asked for, a decision still fits. Past the 140 Keel takes,
        // it goes back once, saying which one and by how much.
        let decided = |chars: usize| with("decided", json!([{ "text": "x".repeat(chars) }]));
        assert!(matches!(
            judge(Some(&decided(140)), false, 0),
            Verdict::Ready(_)
        ));
        let Verdict::Correct(problems) = judge(Some(&decided(141)), false, 0) else {
            panic!()
        };
        assert_eq!(
            problems,
            [SpecProblem::DecidedTooLong {
                position: 1,
                chars: 141,
                limit: 140
            }]
        );
        assert!(
            correction_prompt(&problems, "en")
                .contains("Item 1 of `decided` has 141 characters, and the limit is 140.")
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
            of(json!([question("Q1", &"x".repeat(141), &good)])),
            [SpecProblem::QuestionTooLong {
                id: "Q1".into(),
                chars: 141,
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
        let long = "x".repeat(61);
        assert_eq!(
            of(json!([question(
                "Q1",
                "a?",
                &[(long.as_str(), true), ("No", false)]
            )])),
            [SpecProblem::OptionTooLong {
                id: "Q1".into(),
                chars: 61,
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

    const ASKED: &str = "Count the threads that got fixed.\n\nDecisions already made (don't ask them):\n6. If the author resolves their own\n   critical thread, it counts for now.";

    fn quoting(decided: Value) -> Spec {
        let Verdict::Ready(kept) = super::judge(Some(&with("decided", decided)), ASKED, false, 0)
        else {
            panic!()
        };
        serde_json::from_value(kept).unwrap()
    }

    #[test]
    fn what_the_request_already_decided_is_the_persons_when_quoted_word_for_word() {
        let spec = quoting(json!([
            // Spaced and broken into lines otherwise than in the request, they're still its words.
            { "text": "A critical thread its author resolves counts", "quote": "If the author resolves their own critical thread,\n it counts  for now." },
            { "text": "Keeps the rounding as it is" },
        ]));
        assert_eq!(spec.decided[0].taken, Some(Taken::Request));
        assert_eq!(
            spec.decided[0].quote.as_deref(),
            Some("If the author resolves their own critical thread, it counts for now.")
        );
        assert_eq!(spec.decided[1].taken, None, "the spec's own");
        let kept = json!(spec);
        assert_eq!(kept["decided"][0]["taken"], "request");
        assert_eq!(kept["decided"][1].get("quote"), None);
    }

    #[test]
    fn a_quote_that_is_not_in_the_request_word_for_word_is_dropped() {
        let spec = quoting(json!([
            // Close, but not what the request says.
            { "text": "a", "quote": "If the author resolves their own critical thread, it always counts." },
            { "text": "b", "quote": "if the author resolves their own critical thread" },
            // Nothing quoted is in every request.
            { "text": "c", "quote": " \n " },
            // And an agent can't say so without quoting it.
            { "text": "d", "taken": "request" },
        ]));
        for decided in &spec.decided {
            assert_eq!(
                (decided.taken, decided.quote.as_deref()),
                (None, None),
                "{}",
                decided.text
            );
        }
    }

    #[test]
    fn a_spec_kept_before_quotes_reads_and_is_kept_as_it_was() {
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
        let broken = with("summary", json!("x".repeat(201)));

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
        // A quote of the request is only there when it decides something.
        let decided = &schema["properties"]["decided"]["items"];
        assert_eq!(decided["required"], json!(["text"]));
        assert_eq!(decided["properties"]["quote"]["type"], "string");
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
        judge_agreement(Some(output), "", false, 0, &projects)
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

    #[test]
    fn anywhere_else_parts_are_dropped_like_anything_not_asked_for() {
        let output = agreed(json!([part("valora", "x")]), json!(["y"]));
        let Verdict::Ready(kept) = judge(Some(&output), false, 0) else {
            panic!();
        };
        assert!(kept.get("parts").is_none(), "{kept}");
        assert!(kept.get("together").is_none(), "{kept}");
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
        ] {
            assert!(
                correction_prompt(std::slice::from_ref(&problem), "en").contains("`"),
                "{problem:?}"
            );
        }
    }
}
