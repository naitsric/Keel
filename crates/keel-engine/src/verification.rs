//! A verifier's answer: for each thing the spec says has to be true, whether a test really proves it.
//! The verifier only reads, so what it says is a claim. The engine corrects that claim when it
//! arrives, and can only make it stricter. What it first said stays on the record, in the `result`
//! event. Nothing here runs anything.

use keel_core::{AttemptState, WorkflowStep};
use keel_store::Attempt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::specs::{Spec, one_line};

const MAX_EVIDENCE: usize = 140;
const MAX_SUMMARY: usize = 200;

pub const VERIFY_SCHEMA: &str = r#"{"type":"object","properties":{"criteria":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"result":{"type":"string","enum":["met","unmet","unverified"]},"evidence":{"type":"string"},"why":{"type":"string","enum":["","noTest","notRun"],"description":"Only for unverified: notRun if a test for it exists but didn't run here, because the command Keel ran doesn't run it or skipped it, and noTest if no test proves it. Empty for met and unmet."}},"required":["id","result","evidence","why"],"additionalProperties":false}},"summary":{"type":"string"}},"required":["criteria","summary"],"additionalProperties":false}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Met,
    Unmet,
    /// Nothing proves it either way. A normal answer, not a failure.
    Unverified,
}

impl Outcome {
    fn strictness(self) -> u8 {
        match self {
            Self::Met => 0,
            Self::Unverified => 1,
            Self::Unmet => 2,
        }
    }
}

/// Why the engine wouldn't take the verifier's word. Only the engine writes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Correction {
    /// It said this was met, without saying what proves it.
    NoEvidence,
    /// It said this was met, but no check passed on the code as it is in this round.
    NoCheckPassed,
    /// It said nothing about this.
    LeftOut,
}

/// Why nothing proves something, as the verifier says it. Only for what it left unverified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Unproven {
    /// No test proves it.
    NoTest,
    /// A test for it exists, but didn't run here: the command Keel ran doesn't run it, or skipped it.
    NotRun,
}

/// Why something wasn't proven, as a person reads it: what Keel corrected, or else what the
/// verifier said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WhyNotProven {
    NotRun,
    NoTest,
    NoCheckPassed,
    NoEvidence,
    LeftOut,
    /// It pointed at something, but didn't say why that doesn't prove it. Only answers from before
    /// it said why, or that left it empty.
    NotSaid,
}

/// One thing that has to be true, in the spec's words, and what became of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checked {
    pub id: String,
    pub when: String,
    pub then: String,
    pub result: Outcome,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected: Option<Correction>,
    /// Why nothing proves it, if the verifier said so. Answers from before it did don't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<Unproven>,
}

/// What a verifier's attempt holds once the engine has corrected it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub criteria: Vec<Checked>,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub met: usize,
    pub unmet: usize,
    pub unverified: usize,
}

/// The structure the verifier was asked for. Anything else it added is dropped here, so it can't
/// pass as something the engine wrote.
#[derive(Deserialize)]
struct Answer {
    criteria: Vec<AnswerItem>,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct AnswerItem {
    id: String,
    result: Outcome,
    #[serde(default)]
    evidence: String,
    #[serde(default, deserialize_with = "known_unproven")]
    why: Option<Unproven>,
}

/// A why Keel knows. Anything else, such as an empty one, says nothing, and doesn't make the whole
/// answer unreadable.
fn known_unproven<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Unproven>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(deserializer)?).ok())
}

/// What the engine did to an answer.
pub struct Corrected {
    pub verification: Verification,
    /// Ids it answered about that the spec doesn't have.
    pub dropped: Vec<String>,
}

/// Up to `limit` characters, on one line, saying it was cut.
fn fit(text: &str, limit: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= limit {
        return text;
    }
    let kept: String = text.chars().take(limit - 1).collect();
    format!("{}…", kept.trim_end())
}

/// What an answer becomes, whatever it said. The list comes from the spec and not from the verifier,
/// so nothing can be added to it or left off it. `None` if it isn't in the structure at all: that has
/// to reach a person rather than pass.
pub fn correct(output: &Value, spec: &Spec, check_passed: bool) -> Option<Corrected> {
    let answer: Answer = serde_json::from_value(output.clone()).ok()?;
    let items: Vec<AnswerItem> = answer
        .criteria
        .into_iter()
        .map(|item| AnswerItem {
            id: one_line(&item.id),
            result: item.result,
            evidence: fit(&item.evidence, MAX_EVIDENCE),
            why: item.why,
        })
        .collect();
    let criteria = spec
        .criteria
        .iter()
        .map(|criterion| {
            // Said twice, the stricter one stands.
            let said = items
                .iter()
                .filter(|item| item.id == criterion.id)
                .max_by_key(|item| item.result.strictness());
            let (result, corrected) = match said.map(|item| (item.result, item.evidence.is_empty()))
            {
                None => (Outcome::Unverified, Some(Correction::LeftOut)),
                Some((Outcome::Met, true)) => (Outcome::Unverified, Some(Correction::NoEvidence)),
                Some((Outcome::Met, false)) if !check_passed => {
                    (Outcome::Unverified, Some(Correction::NoCheckPassed))
                }
                Some((result, _)) => (result, None),
            };
            // Only for what it left unverified itself: where Keel didn't take its word, that's why.
            let why = said
                .filter(|item| item.result == Outcome::Unverified)
                .and_then(|item| item.why);
            Checked {
                id: criterion.id.clone(),
                when: criterion.when.clone(),
                then: criterion.then.clone(),
                result,
                evidence: said.map(|item| item.evidence.clone()).unwrap_or_default(),
                corrected,
                why,
            }
        })
        .collect();
    let dropped = items
        .iter()
        .filter(|item| !spec.criteria.iter().any(|c| c.id == item.id))
        .map(|item| item.id.clone())
        .collect();
    Some(Corrected {
        verification: Verification {
            criteria,
            summary: fit(&answer.summary, MAX_SUMMARY),
        },
        dropped,
    })
}

/// What a verifier's attempt says. Only one that succeeded has been corrected: an attempt that was
/// stopped can still hold what the agent first answered.
pub fn of_attempt(attempt: &Attempt) -> Option<Verification> {
    if attempt.role != "verifier" || attempt.state != AttemptState::Succeeded.as_str() {
        return None;
    }
    serde_json::from_value(attempt.output.clone()?).ok()
}

/// What verifiers said in this round, oldest first. A verifier runs again each round, and what it said
/// about code that has changed since says nothing.
pub fn in_round(attempts: &[Attempt], round: i64) -> Vec<Verification> {
    attempts
        .iter()
        .filter(|attempt| attempt.round == round)
        .filter_map(of_attempt)
        .collect()
}

impl Verification {
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for checked in &self.criteria {
            match checked.result {
                Outcome::Met => counts.met += 1,
                Outcome::Unmet => counts.unmet += 1,
                Outcome::Unverified => counts.unverified += 1,
            }
        }
        counts
    }

    /// "3 of 4 met, 1 not verified". Keel's own words, so they're the same in the app and the PR.
    pub fn line(&self, spanish: bool) -> String {
        let counts = self.counts();
        let total = self.criteria.len();
        if counts.met == 0 && counts.unmet == 0 {
            return match (spanish, total) {
                (false, 1) => "It couldn't be verified".to_owned(),
                (false, _) => format!("None of the {total} could be verified"),
                (true, 1) => "No se pudo verificar".to_owned(),
                (true, _) => format!("No se pudo verificar ninguno de los {total}"),
            };
        }
        let mut parts = vec![if spanish {
            let verb = if counts.met == 1 {
                "se cumple"
            } else {
                "se cumplen"
            };
            format!("{} de {total} {verb}", counts.met)
        } else {
            format!("{} of {total} met", counts.met)
        }];
        if counts.unmet > 0 {
            parts.push(if spanish {
                let verb = if counts.unmet == 1 {
                    "no se cumple"
                } else {
                    "no se cumplen"
                };
                format!("{} {verb}", counts.unmet)
            } else {
                format!("{} not met", counts.unmet)
            });
        }
        if counts.unverified > 0 {
            parts.push(if spanish {
                format!("{} sin verificar", counts.unverified)
            } else {
                format!("{} not verified", counts.unverified)
            });
        }
        parts.join(", ")
    }

    /// The pull request's section: that line, and then one line for each thing that had to be true.
    pub fn pr_section(&self, spanish: bool) -> String {
        let say = |en: &'static str, es: &'static str| if spanish { es } else { en };
        let mut text = format!(
            "\n### {}\n\n{}\n\n",
            say("What had to be true", "Lo que tenía que cumplirse"),
            self.line(spanish)
        );
        for checked in &self.criteria {
            let outcome = match checked.result {
                Outcome::Met => say("Met", "Se cumple"),
                Outcome::Unmet => say("Not met", "No se cumple"),
                Outcome::Unverified => say("Not verified", "Sin verificar"),
            };
            text.push_str(&format!("- {outcome}: {} → {}", checked.when, checked.then));
            let note = checked.note(spanish);
            if !note.is_empty() {
                text.push_str(&format!(" ({note})"));
            }
            text.push('\n');
        }
        text
    }
}

impl Checked {
    /// Why nothing proves it. `None` if it was met or found not met, which isn't unproven.
    pub fn why_not_proven(&self) -> Option<WhyNotProven> {
        if self.result != Outcome::Unverified {
            return None;
        }
        Some(match (self.corrected, self.why) {
            (Some(Correction::NoEvidence), _) => WhyNotProven::NoEvidence,
            (Some(Correction::NoCheckPassed), _) => WhyNotProven::NoCheckPassed,
            (Some(Correction::LeftOut), _) => WhyNotProven::LeftOut,
            (None, Some(Unproven::NotRun)) => WhyNotProven::NotRun,
            (None, Some(Unproven::NoTest)) => WhyNotProven::NoTest,
            // Pointing at nothing, it found nothing that proves it.
            (None, None) if self.evidence.is_empty() => WhyNotProven::NoTest,
            (None, None) => WhyNotProven::NotSaid,
        })
    }

    /// What goes with its outcome: why the engine changed it, and what the verifier pointed at.
    fn note(&self, spanish: bool) -> String {
        let say = |en: &'static str, es: &'static str| if spanish { es } else { en };
        let why = match self.why_not_proven() {
            Some(WhyNotProven::NoEvidence) => Some(say(
                "it said this was met, but not what proves it",
                "dijo que se cumple, pero no qué lo demuestra",
            )),
            Some(WhyNotProven::NoCheckPassed) => Some(say(
                "it said this was met, but no check passed in this round",
                "dijo que se cumple, pero ninguna verificación pasó en esta ronda",
            )),
            Some(WhyNotProven::LeftOut) => {
                Some(say("it said nothing about this", "no dijo nada sobre esto"))
            }
            Some(WhyNotProven::NotRun) => Some(say(
                "its test exists, but didn't run here",
                "su prueba existe, pero no corrió aquí",
            )),
            Some(WhyNotProven::NoTest) => Some(say(
                "it found nothing that proves it",
                "no encontró nada que lo demuestre",
            )),
            Some(WhyNotProven::NotSaid) | None => None,
        };
        match why {
            Some(why) if self.evidence.is_empty() => why.to_owned(),
            Some(why) => format!("{why}: {}", self.evidence),
            None => self.evidence.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RanCheck {
    pub command: String,
    pub passed: bool,
    /// How many tests its test tools said they skipped. `None` if Keel didn't recognise how they
    /// say it, which isn't the same as none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<u32>,
}

/// What Keel itself ran in a round. It comes from the attempts of the steps that really are checks,
/// so an agent can't write it, whatever it calls itself or says it did.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CheckFact {
    NoCheckInWorkflow,
    /// The workflow has one, but it hasn't run on the code as it is in this round.
    NotRun,
    Ran {
        checks: Vec<RanCheck>,
    },
}

impl CheckFact {
    pub fn passed(&self) -> bool {
        matches!(self, Self::Ran { checks } if checks.iter().all(|check| check.passed))
    }

    /// How many tests the checks that ran said they skipped, of those Keel could count.
    pub fn skipped(&self) -> u32 {
        match self {
            Self::Ran { checks } => checks
                .iter()
                .filter_map(|check| check.skipped)
                .fold(0, u32::saturating_add),
            _ => 0,
        }
    }
}

/// Whether the round's code was checked. A later attempt at a check replaces an earlier one, and a
/// check before the round's last change to the code says nothing about that change.
pub fn checks_in_round(steps: &[WorkflowStep], attempts: &[Attempt], round: i64) -> CheckFact {
    let in_round: Vec<&Attempt> = attempts.iter().filter(|a| a.round == round).collect();
    let after_last_build = after_last_build(&in_round);
    let mut latest: Vec<&Attempt> = Vec::new();
    for attempt in in_round[after_last_build..]
        .iter()
        .filter(|a| a.kind == "check")
    {
        latest.retain(|earlier| earlier.step_id != attempt.step_id);
        latest.push(attempt);
    }
    if !latest.is_empty() {
        return CheckFact::Ran {
            checks: latest
                .iter()
                .map(|attempt| RanCheck {
                    command: one_line(&attempt.prompt),
                    passed: attempt.state == AttemptState::Succeeded.as_str(),
                    skipped: attempt
                        .skipped_tests
                        .and_then(|count| u32::try_from(count).ok()),
                })
                .collect(),
        };
    }
    let has_a_check = steps
        .iter()
        .any(|s| matches!(s, WorkflowStep::Check { .. }));
    if has_a_check {
        CheckFact::NotRun
    } else {
        CheckFact::NoCheckInWorkflow
    }
}

/// Where the round's code stopped changing. A step that was skipped changed nothing.
pub fn after_last_build(in_round: &[&Attempt]) -> usize {
    in_round
        .iter()
        .rposition(|a| {
            a.kind == "agent"
                && a.tools == keel_core::ToolProfile::Build.as_str()
                && a.state != AttemptState::Skipped.as_str()
        })
        .map_or(0, |index| index + 1)
}

/// `{{check}}`: the fact, for a prompt, in the run's language.
pub fn check_text(fact: &CheckFact, language: &str) -> String {
    let spanish = language.starts_with("es");
    match fact {
        CheckFact::NoCheckInWorkflow if spanish => {
            "Este workflow no tiene paso de verificación, así que Keel no corrió ningún comando."
                .to_owned()
        }
        CheckFact::NoCheckInWorkflow => {
            "This workflow has no check, so Keel ran no command.".to_owned()
        }
        CheckFact::NotRun if spanish => {
            "Keel no ha corrido ninguna verificación sobre el código como está en esta ronda."
                .to_owned()
        }
        CheckFact::NotRun => {
            "Keel hasn't run a check on the code as it is in this round.".to_owned()
        }
        CheckFact::Ran { checks } => checks
            .iter()
            .map(|check| {
                let RanCheck {
                    command,
                    passed,
                    skipped,
                } = check;
                // A check that didn't pass proves nothing anyway.
                let skipped = skipped.filter(|count| *passed && *count > 0);
                match (spanish, passed, skipped) {
                    (false, true, Some(count)) => format!(
                        "Keel ran `{command}` in this round, and it passed, but its test tools said {}. A skipped test didn't run, so it proves nothing. If the only test that proves an item might be one of them, for example because it skips itself when something it needs is missing, the item is unverified.",
                        if count == 1 {
                            "1 test was skipped".to_owned()
                        } else {
                            format!("{count} tests were skipped")
                        }
                    ),
                    (false, true, None) => {
                        format!("Keel ran `{command}` in this round, and it passed.")
                    }
                    (false, false, _) => {
                        format!("Keel ran `{command}` in this round, and it didn't pass.")
                    }
                    (true, true, Some(count)) => format!(
                        "Keel corrió `{command}` en esta ronda, y pasó, pero sus herramientas de pruebas dijeron que {}. Una prueba saltada no corrió, así que no demuestra nada. Si la única prueba que demuestra un elemento podría ser una de ellas, por ejemplo porque se salta sola cuando le falta algo que necesita, el elemento es unverified.",
                        if count == 1 {
                            "se saltó 1 prueba".to_owned()
                        } else {
                            format!("se saltaron {count} pruebas")
                        }
                    ),
                    (true, true, None) => format!("Keel corrió `{command}` en esta ronda, y pasó."),
                    (true, false, _) => {
                        format!("Keel corrió `{command}` en esta ronda, y no pasó.")
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specs::Criterion;
    use serde_json::json;

    fn spec(ids: &[&str]) -> Spec {
        Spec {
            size: Default::default(),
            risks: vec![],
            summary: "x".into(),
            criteria: ids
                .iter()
                .map(|id| Criterion {
                    id: (*id).into(),
                    when: format!("when {id}"),
                    then: format!("then {id}"),
                })
                .collect(),
            changes: vec![],
            limits: vec![],
            decided: vec![],
            questions: vec![],
            parts: vec![],
            together: vec![],
        }
    }

    fn answer(items: Value) -> Value {
        json!({ "criteria": items, "summary": "Looked at it" })
    }

    fn met(id: &str) -> Value {
        json!({ "id": id, "result": "met", "evidence": "tests/calc.rs:12" })
    }

    fn results(corrected: &Corrected) -> Vec<(Outcome, Option<Correction>)> {
        corrected
            .verification
            .criteria
            .iter()
            .map(|checked| (checked.result, checked.corrected))
            .collect()
    }

    #[test]
    fn met_with_something_that_proves_it_and_a_check_that_passed_stays_met() {
        let corrected = correct(&answer(json!([met("C1")])), &spec(&["C1"]), true).unwrap();
        assert_eq!(results(&corrected), [(Outcome::Met, None)]);
        let checked = &corrected.verification.criteria[0];
        assert_eq!(checked.evidence, "tests/calc.rs:12");
        assert_eq!(corrected.verification.summary, "Looked at it");
    }

    #[test]
    fn met_with_empty_evidence_is_not_verified() {
        for evidence in ["", "  \n\t "] {
            let items = json!([{ "id": "C1", "result": "met", "evidence": evidence }]);
            let corrected = correct(&answer(items), &spec(&["C1"]), true).unwrap();
            assert_eq!(
                results(&corrected),
                [(Outcome::Unverified, Some(Correction::NoEvidence))]
            );
        }
    }

    #[test]
    fn met_with_no_check_that_passed_is_not_verified_and_keeps_what_it_pointed_at() {
        let corrected = correct(&answer(json!([met("C1")])), &spec(&["C1"]), false).unwrap();
        assert_eq!(
            results(&corrected),
            [(Outcome::Unverified, Some(Correction::NoCheckPassed))]
        );
        assert_eq!(
            corrected.verification.criteria[0].evidence,
            "tests/calc.rs:12"
        );
    }

    #[test]
    fn what_it_left_out_is_not_verified_and_what_the_spec_does_not_have_is_dropped() {
        let items = json!([met("C1"), met("C9")]);
        let corrected = correct(&answer(items), &spec(&["C1", "C2"]), true).unwrap();
        assert_eq!(
            results(&corrected),
            [
                (Outcome::Met, None),
                (Outcome::Unverified, Some(Correction::LeftOut))
            ]
        );
        let ids: Vec<&str> = corrected
            .verification
            .criteria
            .iter()
            .map(|checked| checked.id.as_str())
            .collect();
        assert_eq!(ids, ["C1", "C2"], "the list is the spec's");
        assert_eq!(corrected.dropped, ["C9"]);
    }

    #[test]
    fn the_words_come_from_the_spec_and_not_from_the_verifier() {
        let items = json!([{ "id": "C1", "when": "it said", "then": "anything", "result": "unmet", "evidence": "a.py:1" }]);
        let corrected = correct(&answer(items), &spec(&["C1"]), true).unwrap();
        assert_eq!(corrected.verification.criteria[0].when, "when C1");
        assert_eq!(corrected.verification.criteria[0].then, "then C1");
    }

    #[test]
    fn it_can_only_get_stricter() {
        // Unmet and unverified aren't claims of proof, so nothing needs to back them.
        let items = json!([
            { "id": "C1", "result": "unmet", "evidence": "" },
            { "id": "C2", "result": "unverified", "evidence": "" },
        ]);
        let corrected = correct(&answer(items), &spec(&["C1", "C2"]), false).unwrap();
        assert_eq!(
            results(&corrected),
            [(Outcome::Unmet, None), (Outcome::Unverified, None)]
        );
        // Said twice, it can't pick the one it likes.
        for items in [
            json!([met("C1"), { "id": "C1", "result": "unmet", "evidence": "a.py:1" }]),
            json!([{ "id": "C1", "result": "unmet", "evidence": "a.py:1" }, met("C1")]),
        ] {
            let corrected = correct(&answer(items), &spec(&["C1"]), true).unwrap();
            assert_eq!(results(&corrected), [(Outcome::Unmet, None)]);
        }
    }

    #[test]
    fn an_agent_cannot_write_what_only_the_engine_writes() {
        let items = json!([{ "id": "C1", "result": "unverified", "evidence": "", "corrected": "noCheckPassed" }]);
        let mut output = answer(items);
        output["passed"] = json!(true);
        let corrected = correct(&output, &spec(&["C1"]), true).unwrap();
        assert_eq!(corrected.verification.criteria[0].corrected, None);
        let kept = json!(corrected.verification);
        assert_eq!(kept.get("passed"), None);
    }

    #[test]
    fn evidence_is_one_line_up_to_its_limit_counting_characters() {
        let at_the_limit = "ñ".repeat(MAX_EVIDENCE);
        let items = json!([
            { "id": "C1", "result": "unmet", "evidence": at_the_limit },
            { "id": "C2", "result": "unmet", "evidence": "ñ".repeat(MAX_EVIDENCE + 1) },
            { "id": "C3", "result": "unmet", "evidence": " a.py:1\n\n# Heading\tb " },
        ]);
        let corrected = correct(&answer(items), &spec(&["C1", "C2", "C3"]), true).unwrap();
        let evidence = |index: usize| corrected.verification.criteria[index].evidence.clone();
        assert_eq!(evidence(0).chars().count(), MAX_EVIDENCE, "twice the bytes");
        assert!(!evidence(0).ends_with('…'));
        assert_eq!(evidence(1).chars().count(), MAX_EVIDENCE);
        assert!(evidence(1).ends_with('…'), "and it says it was cut");
        assert_eq!(evidence(2), "a.py:1 # Heading b");
    }

    #[test]
    fn an_answer_in_some_other_shape_is_not_passed_off_as_one() {
        for output in [
            json!({ "summary": "It gets fixed", "verdict": "approved" }),
            json!("all good"),
            json!({ "criteria": [{ "id": "C1", "result": "passed", "evidence": "x" }] }),
        ] {
            assert!(correct(&output, &spec(&["C1"]), true).is_none(), "{output}");
        }
    }

    #[test]
    fn it_says_why_nothing_proves_something_and_only_for_what_it_left_unverified() {
        let items = json!([
            { "id": "C1", "result": "unverified", "evidence": "tests/db.rs:12, needs Postgres", "why": "notRun" },
            { "id": "C2", "result": "unverified", "evidence": "", "why": "noTest" },
            { "id": "C3", "result": "met", "evidence": "tests/calc.rs:1", "why": "notRun" },
            { "id": "C4", "result": "unmet", "evidence": "src/a.rs:9", "why": "noTest" },
            // Anything else says nothing: empty, unknown, or not even a word.
            { "id": "C5", "result": "unverified", "evidence": "", "why": "" },
            { "id": "C6", "result": "unverified", "evidence": "", "why": "itsComplicated" },
            { "id": "C7", "result": "unverified", "evidence": "", "why": 3 },
            { "id": "C8", "result": "unverified", "evidence": "", "why": null },
        ]);
        let ids = ["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"];
        let corrected = correct(&answer(items), &spec(&ids), true).unwrap();
        let whys: Vec<Option<Unproven>> = corrected
            .verification
            .criteria
            .iter()
            .map(|checked| checked.why)
            .collect();
        assert_eq!(
            whys,
            [
                Some(Unproven::NotRun),
                Some(Unproven::NoTest),
                None,
                None,
                None,
                None,
                None,
                None
            ]
        );
        assert_eq!(corrected.verification.criteria[0].result, Unverified);

        // When Keel didn't take its word, what Keel corrected says why.
        let items =
            json!([{ "id": "C1", "result": "met", "evidence": "tests/a.rs:1", "why": "notRun" }]);
        let corrected = correct(&answer(items), &spec(&["C1"]), false).unwrap();
        let checked = &corrected.verification.criteria[0];
        assert_eq!(checked.corrected, Some(Correction::NoCheckPassed));
        assert_eq!(checked.why, None);
    }

    #[test]
    fn an_answer_from_before_it_said_why_still_reads_and_keeps_its_shape() {
        // A verifier from before answered without `why`, and Keel kept it without one.
        let items = json!([{ "id": "C1", "result": "unverified", "evidence": "" }]);
        let corrected = correct(&answer(items), &spec(&["C1"]), true).unwrap();
        assert_eq!(corrected.verification.criteria[0].why, None);
        let kept = json!(corrected.verification);
        assert_eq!(kept["criteria"][0].get("why"), None, "{kept}");

        let mut attempts = recorded(&[("verify", "agent", "readOnly", 0, Succeeded, "")]);
        attempts[0].output = Some(json!({
            "criteria": [{ "id": "C1", "when": "w", "then": "t", "result": "unverified", "evidence": "" }],
            "summary": "From before",
        }));
        let read = of_attempt(&attempts[0]).unwrap();
        assert_eq!(read.criteria[0].why, None);
        assert_eq!(
            read.criteria[0].why_not_proven(),
            Some(WhyNotProven::NoTest)
        );

        // And one that said why reads back as it was kept.
        attempts[0].output = Some(json!(verification_saying(&[(
            Unverified,
            "tests/db.rs:12",
            None,
            Some(Unproven::NotRun)
        )])));
        let read = of_attempt(&attempts[0]).unwrap();
        assert_eq!(read.criteria[0].why, Some(Unproven::NotRun));
    }

    #[test]
    fn why_something_was_not_proven_is_what_keel_corrected_or_else_what_the_verifier_said() {
        let why = |result: Outcome,
                   evidence: &str,
                   corrected: Option<Correction>,
                   said: Option<Unproven>| {
            verification_saying(&[(result, evidence, corrected, said)]).criteria[0].why_not_proven()
        };
        use WhyNotProven as W;
        assert_eq!(
            why(Unverified, "tests/db.rs:12", None, Some(Unproven::NotRun)),
            Some(W::NotRun)
        );
        assert_eq!(
            why(Unverified, "", None, Some(Unproven::NoTest)),
            Some(W::NoTest)
        );
        for (correction, expected) in [
            (Correction::NoEvidence, W::NoEvidence),
            (Correction::NoCheckPassed, W::NoCheckPassed),
            (Correction::LeftOut, W::LeftOut),
        ] {
            assert_eq!(
                why(Unverified, "a", Some(correction), Some(Unproven::NotRun)),
                Some(expected)
            );
        }
        // An answer that didn't say: nothing it pointed at means it found nothing that proves it.
        assert_eq!(why(Unverified, "", None, None), Some(W::NoTest));
        assert_eq!(
            why(Unverified, "tests/a.rs:1", None, None),
            Some(W::NotSaid)
        );
        // What was proven either way isn't unproven.
        assert_eq!(why(Met, "a", None, None), None);
        assert_eq!(why(Unmet, "b", None, Some(Unproven::NoTest)), None);
        assert_eq!(json!(W::NotRun), json!("notRun"));
        assert_eq!(json!(W::NotSaid), json!("notSaid"));
    }

    #[test]
    fn an_answer_about_nothing_leaves_everything_not_verified() {
        let corrected = correct(&answer(json!([])), &spec(&["C1", "C2"]), true).unwrap();
        assert_eq!(corrected.verification.counts().unverified, 2);
    }

    /// Attempts as the store reads them back: step, kind, tools, round, state and command.
    fn recorded(rows: &[(&str, &str, &str, i64, AttemptState, &str)]) -> Vec<Attempt> {
        let dir = tempfile::tempdir().unwrap();
        let store = keel_store::Store::open(&dir.path().join("keel.db")).unwrap();
        for (index, (step, kind, tools, round, state, prompt)) in rows.iter().enumerate() {
            let id = format!("a{index}");
            store
                .add_attempt(&keel_store::NewAttempt {
                    id: id.clone(),
                    run_id: "r".into(),
                    step_id: (*step).into(),
                    kind: (*kind).into(),
                    role: if *step == "verify" {
                        "verifier".into()
                    } else {
                        "worker".into()
                    },
                    round: *round,
                    number: index as i64 + 1,
                    state: *state,
                    model_requested: "fake".into(),
                    effort: None,
                    tools: (*tools).into(),
                    max_turns: None,
                    max_budget_usd: None,
                    language: "en".into(),
                    prompt: (*prompt).into(),
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

    fn check_step() -> WorkflowStep {
        WorkflowStep::Check {
            id: "check".into(),
            run: "make test".into(),
        }
    }

    use AttemptState::{Failed, Running, Skipped, Succeeded};

    fn ran(command: &str, passed: bool) -> CheckFact {
        ran_skipping(command, passed, None)
    }

    fn ran_skipping(command: &str, passed: bool, skipped: Option<u32>) -> CheckFact {
        CheckFact::Ran {
            checks: vec![RanCheck {
                command: command.into(),
                passed,
                skipped,
            }],
        }
    }

    #[test]
    fn a_check_only_counts_in_its_own_round() {
        let attempts = recorded(&[
            ("builder", "agent", "build", 0, Succeeded, "b"),
            ("check", "check", "check", 0, Succeeded, "make test"),
            ("builder", "agent", "build", 1, Succeeded, "b"),
        ]);
        let steps = [check_step()];
        assert_eq!(
            checks_in_round(&steps, &attempts, 0),
            ran("make test", true)
        );
        assert!(checks_in_round(&steps, &attempts, 0).passed());
        let later = checks_in_round(&steps, &attempts, 1);
        assert_eq!(later, CheckFact::NotRun, "it passed, but on other code");
        assert!(!later.passed());
    }

    #[test]
    fn a_check_says_nothing_about_a_change_made_after_it() {
        let attempts = recorded(&[
            ("builder", "agent", "build", 0, Succeeded, "b"),
            ("check", "check", "check", 0, Succeeded, "make test"),
            ("polish", "agent", "build", 0, Succeeded, "p"),
        ]);
        assert_eq!(
            checks_in_round(&[check_step()], &attempts, 0),
            CheckFact::NotRun
        );
        // What only reads changes nothing.
        let attempts = recorded(&[
            ("builder", "agent", "build", 0, Succeeded, "b"),
            ("check", "check", "check", 0, Succeeded, "make test"),
            ("code", "agent", "readOnly", 0, Succeeded, "r"),
        ]);
        assert!(checks_in_round(&[check_step()], &attempts, 0).passed());
        // Nor does what could have built, and was skipped.
        let attempts = recorded(&[
            ("builder", "agent", "build", 0, Succeeded, "b"),
            ("check", "check", "check", 0, Succeeded, "make test"),
            ("polish", "agent", "build", 0, Skipped, "p"),
        ]);
        assert!(checks_in_round(&[check_step()], &attempts, 0).passed());
    }

    #[test]
    fn the_latest_attempt_at_a_check_is_the_one_that_counts_and_every_check_has_to_pass() {
        let attempts = recorded(&[
            ("check", "check", "check", 0, Failed, "make test"),
            ("check", "check", "check", 0, Succeeded, "make test"),
        ]);
        assert_eq!(
            checks_in_round(&[check_step()], &attempts, 0),
            ran("make test", true)
        );
        let attempts = recorded(&[
            ("check", "check", "check", 0, Succeeded, "make test"),
            ("check", "check", "check", 0, Failed, "make test"),
        ]);
        assert_eq!(
            checks_in_round(&[check_step()], &attempts, 0),
            ran("make test", false)
        );
        assert!(!checks_in_round(&[check_step()], &attempts, 0).passed());

        let attempts = recorded(&[
            ("unit", "check", "check", 0, Succeeded, "make unit"),
            ("lint", "check", "check", 0, Failed, "make lint"),
        ]);
        assert!(!checks_in_round(&[check_step()], &attempts, 0).passed());
        let attempts = recorded(&[("check", "check", "check", 0, Running, "make test")]);
        assert!(!checks_in_round(&[check_step()], &attempts, 0).passed());
    }

    #[test]
    fn an_agent_called_check_is_not_a_check_and_a_workflow_without_one_says_so() {
        let attempts = recorded(&[("check", "agent", "readOnly", 0, Succeeded, "I passed")]);
        let fact = checks_in_round(&[], &attempts, 0);
        assert_eq!(fact, CheckFact::NoCheckInWorkflow);
        assert!(!fact.passed());
    }

    #[test]
    fn the_fact_reads_as_what_happened_in_the_language_of_the_run() {
        let cases = [
            (CheckFact::NoCheckInWorkflow, "en", "has no check"),
            (CheckFact::NoCheckInWorkflow, "es", "no tiene paso"),
            (CheckFact::NotRun, "en", "hasn't run a check"),
            (CheckFact::NotRun, "es", "no ha corrido"),
            (
                ran("make test", true),
                "en",
                "Keel ran `make test` in this round, and it passed.",
            ),
            (
                ran("make test", false),
                "en",
                "Keel ran `make test` in this round, and it didn't pass.",
            ),
            (
                ran("make test", true),
                "es",
                "Keel corrió `make test` en esta ronda, y pasó.",
            ),
            (
                ran("make test", false),
                "es",
                "Keel corrió `make test` en esta ronda, y no pasó.",
            ),
        ];
        for (fact, language, expected) in cases {
            let text = check_text(&fact, language);
            assert!(text.contains(expected), "{text}");
        }
    }

    #[test]
    fn a_check_that_passed_says_how_many_tests_it_skipped_and_that_they_prove_nothing() {
        let cases = [
            (
                ran_skipping("cargo test", true, Some(385)),
                "en",
                "Keel ran `cargo test` in this round, and it passed, but its test tools said 385 tests were skipped.",
            ),
            (
                ran_skipping("cargo test", true, Some(1)),
                "en",
                "Keel ran `cargo test` in this round, and it passed, but its test tools said 1 test was skipped.",
            ),
            (
                ran_skipping("cargo test", true, Some(385)),
                "es",
                "Keel corrió `cargo test` en esta ronda, y pasó, pero sus herramientas de pruebas dijeron que se saltaron 385 pruebas.",
            ),
            (
                ran_skipping("cargo test", true, Some(1)),
                "es",
                "Keel corrió `cargo test` en esta ronda, y pasó, pero sus herramientas de pruebas dijeron que se saltó 1 prueba.",
            ),
        ];
        for (fact, language, expected) in cases {
            let text = check_text(&fact, language);
            assert!(text.starts_with(expected), "{text}");
            let rule = if language == "es" {
                "Una prueba saltada no corrió, así que no demuestra nada"
            } else {
                "A skipped test didn't run, so it proves nothing"
            };
            assert!(text.contains(rule), "{text}");
            assert!(text.contains("unverified"), "{text}");
        }
        // None skipped, or not known, reads as before. So does a check that didn't pass, which
        // proves nothing either way.
        for (skipped, passed, expected) in [
            (
                Some(0),
                true,
                "Keel ran `make test` in this round, and it passed.",
            ),
            (
                None,
                true,
                "Keel ran `make test` in this round, and it passed.",
            ),
            (
                Some(3),
                false,
                "Keel ran `make test` in this round, and it didn't pass.",
            ),
        ] {
            let text = check_text(&ran_skipping("make test", passed, skipped), "en");
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn what_keel_counted_comes_from_the_check_on_the_record() {
        let mut attempts = recorded(&[
            ("builder", "agent", "build", 0, Succeeded, "b"),
            ("check", "check", "check", 0, Succeeded, "cargo test"),
        ]);
        attempts[1].skipped_tests = Some(385);
        assert_eq!(
            checks_in_round(&[check_step()], &attempts, 0),
            ran_skipping("cargo test", true, Some(385))
        );
        assert_eq!(
            checks_in_round(&[check_step()], &attempts, 0).skipped(),
            385
        );
        attempts[1].skipped_tests = None;
        assert_eq!(checks_in_round(&[check_step()], &attempts, 0).skipped(), 0);
    }

    fn verification(results: &[(Outcome, &str, Option<Correction>)]) -> Verification {
        let said: Vec<_> = results
            .iter()
            .map(|(result, evidence, corrected)| (*result, *evidence, *corrected, None))
            .collect();
        verification_saying(&said)
    }

    /// As `verification`, with why the verifier said nothing proves each.
    fn verification_saying(
        results: &[(Outcome, &str, Option<Correction>, Option<Unproven>)],
    ) -> Verification {
        Verification {
            criteria: results
                .iter()
                .enumerate()
                .map(|(index, (result, evidence, corrected, why))| Checked {
                    id: format!("C{}", index + 1),
                    when: format!("when {}", index + 1),
                    then: format!("then {}", index + 1),
                    result: *result,
                    evidence: (*evidence).into(),
                    corrected: *corrected,
                    why: *why,
                })
                .collect(),
            summary: "x".into(),
        }
    }

    use Outcome::{Met, Unmet, Unverified};

    #[test]
    fn the_line_says_how_it_went_in_either_language() {
        let line = |results: &[(Outcome, &str, Option<Correction>)]| {
            let verification = verification(results);
            (verification.line(false), verification.line(true))
        };
        let met = (Met, "a", None);
        let open = (Unverified, "", None);
        let bad = (Unmet, "b", None);
        assert_eq!(
            line(&[met, met, met, met]),
            ("4 of 4 met".to_owned(), "4 de 4 se cumplen".to_owned())
        );
        assert_eq!(
            line(&[met, met, met, open]),
            (
                "3 of 4 met, 1 not verified".to_owned(),
                "3 de 4 se cumplen, 1 sin verificar".to_owned()
            )
        );
        assert_eq!(
            line(&[met, bad, open, open]),
            (
                "1 of 4 met, 1 not met, 2 not verified".to_owned(),
                "1 de 4 se cumple, 1 no se cumple, 2 sin verificar".to_owned()
            )
        );
        assert_eq!(
            line(&[open, open]),
            (
                "None of the 2 could be verified".to_owned(),
                "No se pudo verificar ninguno de los 2".to_owned()
            )
        );
        assert_eq!(
            line(&[open]),
            (
                "It couldn't be verified".to_owned(),
                "No se pudo verificar".to_owned()
            )
        );
        assert_eq!(
            line(&[bad, bad]),
            (
                "0 of 2 met, 2 not met".to_owned(),
                "0 de 2 se cumplen, 2 no se cumplen".to_owned()
            )
        );
    }

    #[test]
    fn the_pull_request_section_has_a_line_for_each_with_its_words_and_why() {
        let section = verification(&[
            (Met, "tests/calc.rs:12", None),
            (Unverified, "", None),
            (Unverified, "tests/a.rs:1", Some(Correction::NoCheckPassed)),
            (Unverified, "", Some(Correction::NoEvidence)),
            (Unverified, "", Some(Correction::LeftOut)),
            (Unmet, "src/a.rs:9", None),
        ])
        .pr_section(false);
        assert!(
            section.starts_with(
                "\n### What had to be true\n\n1 of 6 met, 1 not met, 4 not verified\n\n"
            ),
            "{section}"
        );
        for line in [
            "- Met: when 1 → then 1 (tests/calc.rs:12)\n",
            "- Not verified: when 2 → then 2 (it found nothing that proves it)\n",
            "- Not verified: when 3 → then 3 (it said this was met, but no check passed in this round: tests/a.rs:1)\n",
            "- Not verified: when 4 → then 4 (it said this was met, but not what proves it)\n",
            "- Not verified: when 5 → then 5 (it said nothing about this)\n",
            "- Not met: when 6 → then 6 (src/a.rs:9)\n",
        ] {
            assert!(section.contains(line), "{line}\n{section}");
        }
        assert!(!section.contains("C1"), "no bare ids: {section}");
    }

    #[test]
    fn the_pull_request_section_says_when_a_test_exists_but_did_not_run_here() {
        let saying = verification_saying(&[
            (Unverified, "tests/db.rs:12", None, Some(Unproven::NotRun)),
            (Unverified, "", None, Some(Unproven::NoTest)),
        ]);
        let section = saying.pr_section(false);
        for line in [
            "- Not verified: when 1 → then 1 (its test exists, but didn't run here: tests/db.rs:12)\n",
            "- Not verified: when 2 → then 2 (it found nothing that proves it)\n",
        ] {
            assert!(section.contains(line), "{line}\n{section}");
        }
        let section = saying.pr_section(true);
        assert!(
            section.contains(
                "- Sin verificar: when 1 → then 1 (su prueba existe, pero no corrió aquí: tests/db.rs:12)\n"
            ),
            "{section}"
        );
    }

    #[test]
    fn the_pull_request_section_is_in_the_language_of_the_run() {
        let section = verification(&[(Met, "a", None), (Unverified, "", None)]).pr_section(true);
        assert!(section.contains("### Lo que tenía que cumplirse"));
        assert!(section.contains("1 de 2 se cumple, 1 sin verificar"));
        assert!(section.contains("- Se cumple: when 1 → then 1 (a)\n"));
        assert!(section.contains("- Sin verificar: when 2 → then 2 (no encontró"));
    }

    #[test]
    fn nothing_read_back_counts_unless_it_succeeded_and_only_in_its_round() {
        let mut attempts = recorded(&[
            ("verify", "agent", "readOnly", 0, Succeeded, ""),
            ("verify", "agent", "readOnly", 1, Failed, ""),
            ("verify", "agent", "readOnly", 2, Succeeded, ""),
        ]);
        let said = |result: Outcome| json!(verification(&[(result, "a", None)]));
        attempts[0].output = Some(said(Unmet));
        attempts[1].output = Some(said(Unmet));
        attempts[2].output = Some(said(Met));
        let unmet_in = |round: i64| {
            in_round(&attempts, round)
                .iter()
                .any(|verification| verification.counts().unmet > 0)
        };
        assert!(unmet_in(0));
        assert!(!unmet_in(1), "it didn't succeed, so it wasn't corrected");
        assert_eq!(in_round(&attempts, 1), []);
        assert!(!unmet_in(2), "an earlier round's doesn't carry over");
        assert_eq!(in_round(&attempts, 2).len(), 1);
        assert_eq!(in_round(&attempts, 3), []);

        attempts[2].role = "worker".into();
        assert_eq!(in_round(&attempts, 2), [], "only a verifier says this");
    }

    #[test]
    fn the_schema_is_valid_json_and_asks_for_every_field() {
        let schema: Value = serde_json::from_str(VERIFY_SCHEMA).unwrap();
        assert_eq!(schema["required"], json!(["criteria", "summary"]));
        assert_eq!(
            schema["properties"]["criteria"]["items"]["required"],
            json!(["id", "result", "evidence", "why"])
        );
        // Saved workflows keep an older copy of the prompt, so the schema says what `why` is.
        let why = &schema["properties"]["criteria"]["items"]["properties"]["why"];
        assert_eq!(why["enum"], json!(["", "noTest", "notRun"]));
        let description = why["description"].as_str().unwrap();
        for part in [
            "unverified",
            "didn't run here",
            "skipped",
            "Empty for met and unmet",
        ] {
            assert!(description.contains(part), "{part}: {description}");
        }
    }
}
