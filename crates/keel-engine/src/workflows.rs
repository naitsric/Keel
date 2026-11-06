//! Workflows: the rules they must follow, the built-in templates, and how a project keeps its own.
//! They're YAML files in the app's data, and each run keeps an exact copy of what it used.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use globset::{Glob, GlobSetBuilder};
use keel_core::{AttemptState, Reason, StepRole, StepSpec, ToolProfile, WorkflowStep};
use keel_store::Attempt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{brain, prompts, specs};

/// How much of a diff goes into a prompt. A reviewer can still read any file it wants.
const DIFF_LIMIT: usize = 200 * 1024;
const MAX_ROUNDS: u32 = 5;

pub const VERDICT_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["approved","changes"]},"findings":{"type":"array","items":{"type":"string"}},"summary":{"type":"string"}},"required":["verdict","findings","summary"],"additionalProperties":false}"#;
pub const DECISION_SCHEMA: &str = r#"{"type":"object","properties":{"decision":{"type":"string","enum":["approve","rework"]},"risk":{"type":"string","enum":["low","medium","high"]},"reasons":{"type":"string"},"instructions":{"type":"string"}},"required":["decision","risk","reasons","instructions"],"additionalProperties":false}"#;

pub fn schema_for(role: &StepRole) -> Option<&'static str> {
    match role {
        StepRole::Worker => None,
        StepRole::Reviewer => Some(VERDICT_SCHEMA),
        StepRole::Approver { .. } => Some(DECISION_SCHEMA),
        StepRole::Mapper => Some(brain::MAP_SCHEMA),
        StepRole::Learner => Some(brain::LEARNINGS_SCHEMA),
        StepRole::Specifier => Some(specs::SPEC_SCHEMA),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    /// `builtin:` for the ones that ship with Keel, otherwise the file's name.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    #[serde(default)]
    pub builtin: bool,
}

impl Workflow {
    pub fn reference(&self) -> crate::WorkflowRef {
        crate::WorkflowRef {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

/// A workflow an assistant proposed, waiting for a person. It's its own type in its own folder so
/// that nothing that runs can reach it: it only becomes a [`Workflow`] when the person saves it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDraft {
    /// The file's name, which Keel picks, so a client can't aim at one that's already there.
    #[serde(default)]
    pub id: String,
    /// The folder it's in.
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    pub proposed_by: String,
    pub created_at: String,
}

fn problem(problem: &str, step: &str) -> Reason {
    Reason::InvalidWorkflow {
        problem: problem.to_owned(),
        step: step.to_owned(),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every workflow has to pass this, whether it came from the editor or a hand-edited file.
pub fn validate(steps: &[WorkflowStep]) -> Result<(), Reason> {
    if steps.is_empty() {
        return Err(problem("empty", ""));
    }
    let mut seen = HashSet::new();
    let mut remember = |id: &str| -> Result<(), Reason> {
        if !valid_id(id) {
            return Err(problem("badId", id));
        }
        if !seen.insert(id.to_owned()) {
            return Err(problem("duplicateId", id));
        }
        Ok(())
    };
    let check_agent = |spec: &StepSpec| -> Result<(), Reason> {
        if spec.model.trim().is_empty() {
            return Err(problem("noModel", &spec.id));
        }
        if spec.prompt.trim().is_empty() {
            return Err(problem("noPrompt", &spec.id));
        }
        for pattern in &spec.paths {
            Glob::new(pattern).map_err(|_| problem("badPath", &spec.id))?;
        }
        // A step that decides or judges can't also change what it's judging.
        if spec.role != StepRole::Worker && spec.tools != ToolProfile::ReadOnly {
            return Err(problem("reviewerMustReadOnly", &spec.id));
        }
        Ok(())
    };
    let mut approval_seen = false;
    let mut earlier: Vec<&str> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        remember(step.id())?;
        match step {
            WorkflowStep::Agent(spec) => {
                check_agent(spec)?;
                // It's about what the run did, so it has to come once the run has done it.
                if spec.role == StepRole::Learner && index + 1 != steps.len() {
                    return Err(problem("learnerNotLast", &spec.id));
                }
                if let StepRole::Approver {
                    rework_to,
                    max_rounds,
                } = &spec.role
                {
                    if !earlier.contains(&rework_to.as_str()) {
                        return Err(problem("reworkTarget", &spec.id));
                    }
                    if *max_rounds > MAX_ROUNDS {
                        return Err(problem("tooManyRounds", &spec.id));
                    }
                }
            }
            WorkflowStep::Parallel { steps: group, id } => {
                if group.is_empty() {
                    return Err(problem("emptyGroup", id));
                }
                for spec in group {
                    remember(&spec.id)?;
                    check_agent(spec)?;
                    // The rest act on what the whole run has done, so they can't run alongside it.
                    if !matches!(spec.role, StepRole::Worker | StepRole::Reviewer) {
                        return Err(problem("groupRole", &spec.id));
                    }
                    // They share one workspace, so only read-only agents can run at once.
                    if spec.tools != ToolProfile::ReadOnly {
                        return Err(problem("parallelMustReadOnly", &spec.id));
                    }
                }
            }
            WorkflowStep::Setup { run, id } | WorkflowStep::Check { run, id } => {
                if run.trim().is_empty() {
                    return Err(problem("noCommand", id));
                }
            }
            WorkflowStep::Approval { .. } => approval_seen = true,
            WorkflowStep::Deliver { id } => {
                // The one rule that can't be configured away: a person approves first.
                if !approval_seen {
                    return Err(problem("deliverNeedsApproval", id));
                }
            }
        }
        earlier.push(step.id());
    }
    Ok(())
}

/// A step by id, looking inside parallel groups too.
pub fn find_step(steps: &[WorkflowStep], id: &str) -> Option<WorkflowStep> {
    steps.iter().find_map(|step| match step {
        WorkflowStep::Parallel { steps: group, .. } => group
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .map(WorkflowStep::Agent),
        other if other.id() == id => Some(other.clone()),
        _ => None,
    })
}

/// Where rework starts: everything from here on runs again each round. An approver says where, and
/// without one it's the last worker that can change files. One after the last approval doesn't
/// count, since nobody is left to send work back to it.
pub fn rework_index(steps: &[WorkflowStep]) -> Option<usize> {
    steps
        .iter()
        .find_map(|step| match step {
            WorkflowStep::Agent(StepSpec {
                role: StepRole::Approver { rework_to, .. },
                ..
            }) => steps.iter().position(|s| s.id() == rework_to),
            _ => None,
        })
        .or_else(|| {
            let end = steps
                .iter()
                .rposition(|step| matches!(step, WorkflowStep::Approval { .. }))
                .unwrap_or(steps.len());
            steps[..end].iter().rposition(|step| {
                matches!(step, WorkflowStep::Agent(spec)
                    if spec.role == StepRole::Worker && spec.tools == ToolProfile::Build)
            })
        })
}

/// Whether a person at this approval can send the work back: what would do it again has to come
/// before them, or they'd only be sent straight back to the same question.
pub fn can_send_back(steps: &[WorkflowStep], approval_id: &str) -> bool {
    let at = steps.iter().position(|step| step.id() == approval_id);
    matches!((rework_index(steps), at), (Some(from), Some(at)) if from < at)
}

/// Rounds that happened because a person asked for them. They don't count against the approver.
pub fn rounds_asked_by_a_person(attempts: &[Attempt]) -> i64 {
    attempts
        .iter()
        .filter(|a| a.state == AttemptState::Returned.as_str())
        .count() as i64
}

pub fn next_number(attempts: &[Attempt], step_id: &str) -> i64 {
    attempts
        .iter()
        .filter(|a| a.step_id == step_id)
        .map(|a| a.number)
        .max()
        .unwrap_or(0)
        + 1
}

/// What the run has changed so far, going by the last step that looked.
pub fn files_changed(attempts: &[Attempt]) -> Vec<String> {
    attempts
        .iter()
        .rev()
        .find_map(|a| a.run_files.clone())
        .and_then(|files| serde_json::from_value(files).ok())
        .unwrap_or_default()
}

/// Whether a pull request is what comes right after a step. The step that proposes notes runs after
/// everything else and pushes nothing, so it doesn't count.
pub fn delivery_is_next(steps: &[WorkflowStep], step_id: &str) -> bool {
    let next = steps
        .iter()
        .skip_while(|step| step.id() != step_id)
        .skip(1)
        .find(|step| !matches!(step, WorkflowStep::Agent(spec) if spec.role == StepRole::Learner));
    matches!(next, Some(WorkflowStep::Deliver { .. }))
}

/// Whether a step limited to some paths has anything to look at.
pub fn paths_match(paths: &[String], changed: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in paths {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return true;
    };
    changed.iter().any(|file| set.is_match(file))
}

/// What a step produced, for the next one's prompt. From its latest attempt.
pub fn output_of(attempts: &[Attempt], step_id: &str) -> String {
    let Some(attempt) = attempts.iter().rev().find(|a| a.step_id == step_id) else {
        return String::new();
    };
    if attempt.state == AttemptState::Skipped.as_str() {
        return "(skipped: nothing it covers changed)".to_owned();
    }
    match &attempt.output {
        Some(output) => output.to_string(),
        None => attempt.result_text.clone().unwrap_or_default(),
    }
}

/// The attempt that says what was asked: the latest that worked of a step with the specifier's role,
/// or else of a step called `spec`, which is how workflows saved before that role say it.
pub fn specifying_attempt(attempts: &[Attempt]) -> Option<&Attempt> {
    let latest = |applies: fn(&Attempt) -> bool| {
        attempts.iter().rev().find(|a| {
            a.kind == "agent" && a.state == AttemptState::Succeeded.as_str() && applies(a)
        })
    };
    latest(|a| a.role == "specifier").or_else(|| latest(|a| a.step_id == "spec"))
}

/// What that attempt says, in words a person can read: its structured answer as text, or else what
/// it wrote.
pub fn asked_text(attempt: &Attempt) -> Option<String> {
    if let Some(spec) = specs::of_attempt(attempt) {
        return Some(spec.readable(&attempt.language));
    }
    attempt
        .result_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub fn wants_rework(attempt: &Attempt) -> bool {
    attempt
        .output
        .as_ref()
        .and_then(|o| o.get("decision"))
        .and_then(Value::as_str)
        == Some("rework")
}

fn instructions_of(attempt: &Attempt) -> Option<String> {
    attempt
        .output
        .as_ref()?
        .get("instructions")?
        .as_str()
        .map(str::to_owned)
}

/// What was asked for last time, by the approver or by a person, for whoever works on or reviews
/// this round. Reviewers need it too: a real run went in circles when one called out of scope what
/// the approver had just asked for.
pub fn rework_text(attempts: &[Attempt], round: i64, language: &str) -> String {
    if round == 0 {
        return String::new();
    }
    let es = language.starts_with("es");
    let last_round = || attempts.iter().rev().filter(|a| a.round == round - 1);
    // An approver that approved may still have written something here, and a person sent it back
    // for their own reason.
    let from_approver = last_round()
        .filter(|a| a.role == "approver" && wants_rework(a))
        .find_map(instructions_of)
        .map(|text| {
            let intro = if es {
                "El aprobador devolvió este trabajo. Esto es lo único que debe cambiar en esta ronda, y cuenta como parte del alcance:"
            } else {
                "The approver sent this work back. This is the only thing to change in this round, and it counts as part of the scope:"
            };
            format!("{intro}\n{text}")
        });
    let from_person = last_round()
        .filter(|a| a.state == AttemptState::Returned.as_str())
        .find_map(instructions_of)
        .map(|text| {
            let intro = if es {
                "Una persona devolvió este trabajo. Esto es lo único que debe cambiar en esta ronda, y cuenta como parte del alcance:"
            } else {
                "A person sent this work back. This is the only thing to change in this round, and it counts as part of the scope:"
            };
            format!("{intro}\n{text}")
        });
    [from_person, from_approver]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Whether a person sent the work back in the round before this one.
pub fn returned_last_round(attempts: &[Attempt], round: i64) -> bool {
    attempts
        .iter()
        .any(|a| a.round == round - 1 && a.state == AttemptState::Returned.as_str())
}

/// What a person answered in this run, for `{{answers}}`. Empty if nothing.
pub fn answers_text(attempts: &[Attempt], language: &str) -> String {
    let given = specs::answered_in_run(attempts);
    if given.is_empty() {
        return String::new();
    }
    let intro = if language.starts_with("es") {
        "La persona ya respondió esto:"
    } else {
        "The person has already answered this:"
    };
    format!("{intro}\n{}", specs::answer_lines(&given))
}

pub fn cap_diff(diff: String) -> String {
    if diff.len() <= DIFF_LIMIT {
        return diff;
    }
    let mut end = DIFF_LIMIT;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[The diff was cut here. Read the files for the rest.]",
        &diff[..end]
    )
}

fn agent(
    id: &str,
    model: &str,
    tools: ToolProfile,
    role: StepRole,
    paths: &[&str],
    prompt: &str,
) -> StepSpec {
    StepSpec {
        id: id.into(),
        model: model.into(),
        effort: None,
        tools,
        prompt: prompt.into(),
        max_turns: None,
        max_budget_usd: None,
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        role,
    }
}

const SCREEN_FILES: [&str; 8] = [
    "**/*.tsx",
    "**/*.jsx",
    "**/*.vue",
    "**/*.svelte",
    "**/*.html",
    "**/*.css",
    "**/*.scss",
    "**/*.dart",
];

/// The workflows that ship with Keel, in the app's language. The full one is the user's diagram.
pub fn builtin(language: &str, check: Option<&str>) -> Vec<Workflow> {
    let es = language.starts_with("es");
    let text = |en: &'static str, spanish: &'static str| if es { spanish } else { en };

    let review = |focus_en: &'static str, focus_es: &'static str| {
        format!(
            "{}\n\n{}\n{{{{steps.spec.output}}}}\n\n{{{{rework}}}}\n\n{}\n{{{{diff}}}}",
            if es {
                format!(
                    "Revisas un cambio que no escribiste, y no editas nada. {focus_es} Reporta solo problemas reales de este cambio, cada uno con archivo y línea. Si no hay, el veredicto es approved. Responde en español."
                )
            } else {
                format!(
                    "You're reviewing a change you didn't write, and you edit nothing. {focus_en} Report only real problems in this change, each with its file and line. If there are none, the verdict is approved."
                )
            },
            text("Specification:", "Especificación:"),
            text("Change:", "Cambio:"),
        )
    };
    let (security, code, approver, human) = if es {
        ("seguridad", "codigo", "aprobador", "humano")
    } else {
        ("security", "code", "approver", "human")
    };
    let approver_prompt = format!(
        "{}\n\n{}\n{{{{steps.spec.output}}}}\n\n{security}: {{{{steps.{security}.output}}}}\nux: {{{{steps.ux.output}}}}\nui: {{{{steps.ui.output}}}}\n{code}: {{{{steps.{code}.output}}}}\n\n{{{{rework}}}}",
        text(
            "You decide whether this change is ready for a person to review, using the reviews below, and you edit nothing. Check each finding in the code before believing it, and discard false ones. decision = rework if a real finding remains, approve if not. risk = high if it touches authorization, personal data, money, contractual deadlines or the database schema; low if it's contained and easy to revert; medium otherwise. In instructions, say exactly what to fix.",
            "Decides si este cambio está listo para que lo revise una persona, usando las revisiones de abajo, y no editas nada. Comprueba cada hallazgo en el código antes de creerle y descarta los falsos. decision = rework si queda un hallazgo real, approve si no. risk = high si toca autorización, datos personales, dinero, plazos contractuales o el esquema de base; low si es acotado y fácil de revertir; medium en otro caso. En instructions, di exactamente qué corregir. Responde en español.",
        ),
        text("Specification:", "Especificación:"),
    );

    // The simple one is the base, up to where a person gets involved.
    let simple = prompts::full_workflow(language, "opus", "sonnet", check);
    let mut full: Vec<WorkflowStep> = simple
        .iter()
        .filter(|step| {
            !matches!(
                step,
                WorkflowStep::Approval { .. } | WorkflowStep::Deliver { .. }
            )
        })
        .cloned()
        .collect();
    full.push(WorkflowStep::Parallel {
        id: "reviews".into(),
        steps: vec![
            agent(security, "sonnet", ToolProfile::ReadOnly, StepRole::Reviewer, &[], &review(
                "Focus on SECURITY: authorization per resource, IDOR, personal data in logs, secrets, injection, role permissions and input validation.",
                "Enfócate en SEGURIDAD: autorización por recurso, IDOR, datos personales en logs, secretos, inyección, permisos por rol y validación de entradas.",
            )),
            agent("ux", "sonnet", ToolProfile::ReadOnly, StepRole::Reviewer, &SCREEN_FILES, &review(
                "Focus on UX: that the flow makes sense, and that errors, empty states, loading and text tell the person what happened and what to do. You only see the code, not the screen: say so if something needs to be seen.",
                "Enfócate en UX: que el flujo se entienda, y que los errores, estados vacíos, cargas y textos le digan a la persona qué pasó y qué hacer. Solo ves el código, no la pantalla: dilo si algo requiere verse.",
            )),
            agent("ui", "sonnet", ToolProfile::ReadOnly, StepRole::Reviewer, &SCREEN_FILES, &review(
                "Focus on UI: consistency with the repo's design system, existing components, responsive behavior and accessibility. You only see the code, not the screen: say so if something needs to be seen.",
                "Enfócate en UI: consistencia con el sistema de diseño del repo, componentes existentes, responsive y accesibilidad. Solo ves el código, no la pantalla: dilo si algo requiere verse.",
            )),
            agent(code, "sonnet", ToolProfile::ReadOnly, StepRole::Reviewer, &[], &review(
                "Focus on the CODE: that it meets each criterion of the specification, stays within its limits, logic errors, edge cases, and tests that really prove the behavior.",
                "Enfócate en el CÓDIGO: que cumpla cada criterio de la especificación, que no se salga de sus límites, errores de lógica, casos borde y pruebas que de verdad demuestren el comportamiento.",
            )),
        ],
    });
    full.push(WorkflowStep::Agent(agent(
        approver,
        "opus",
        ToolProfile::ReadOnly,
        StepRole::Approver {
            rework_to: "builder".into(),
            max_rounds: 2,
        },
        &[],
        &approver_prompt,
    )));
    full.push(WorkflowStep::Approval { id: human.into() });
    full.push(WorkflowStep::Deliver {
        id: "pull-request".into(),
    });

    let learnings = if es { "aprendizajes" } else { "learnings" };
    let learner = |reviewers: &[&str]| {
        WorkflowStep::Agent(brain::learner_step(
            learnings, language, "sonnet", reviewers,
        ))
    };
    full.push(learner(&[security, "ux", "ui", code]));
    let mut simple = simple;
    simple.push(learner(&[]));

    vec![
        Workflow {
            id: "builtin:full".into(),
            name: text("Full, with reviews", "Completo, con revisiones").into(),
            steps: full,
            builtin: true,
        },
        Workflow {
            id: "builtin:simple".into(),
            name: text("Simple, without reviews", "Simple, sin revisiones").into(),
            steps: simple,
            builtin: true,
        },
    ]
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "workflow".to_owned()
    } else {
        trimmed.chars().take(40).collect()
    }
}

/// Keeps a file name inside the folder, whatever id comes in.
fn safe_file(dir: &Path, id: &str) -> io::Result<std::path::PathBuf> {
    if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") || id.starts_with("builtin:")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a workflow file",
        ));
    }
    Ok(dir.join(format!("{id}.yaml")))
}

pub fn load_all(dir: &Path) -> io::Result<Vec<Workflow>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // A hand-edited file that no longer parses shouldn't take the others down with it.
        if let Ok(mut workflow) = serde_yaml_ng::from_str::<Workflow>(&text) {
            workflow.id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            workflow.builtin = false;
            found.push(workflow);
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

pub fn save(dir: &Path, workflow: &Workflow) -> io::Result<Workflow> {
    std::fs::create_dir_all(dir)?;
    let mut saved = workflow.clone();
    saved.builtin = false;
    // Saving a built-in keeps it intact and makes the project its own copy.
    if saved.id.is_empty() || saved.id.starts_with("builtin:") {
        let base = slug(&saved.name);
        let mut id = base.clone();
        let mut counter = 2;
        while dir.join(format!("{id}.yaml")).exists() {
            id = format!("{base}-{counter}");
            counter += 1;
        }
        saved.id = id;
    }
    let path = safe_file(dir, &saved.id)?;
    let text = serde_yaml_ng::to_string(&saved).map_err(io::Error::other)?;
    std::fs::write(path, text)?;
    Ok(saved)
}

pub fn delete(dir: &Path, id: &str) -> io::Result<()> {
    std::fs::remove_file(safe_file(dir, id)?)
}

/// A project's drafts, newest first. One that no longer parses is left out, as in [`load_all`].
pub fn load_drafts(dir: &Path, project_id: &str) -> io::Result<Vec<WorkflowDraft>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(mut draft) = serde_yaml_ng::from_str::<WorkflowDraft>(&text) {
            draft.id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            draft.project_id = project_id.to_owned();
            found.push(draft);
        }
    }
    found.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(found)
}

pub fn save_draft(dir: &Path, draft: &WorkflowDraft) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_yaml_ng::to_string(draft).map_err(io::Error::other)?;
    std::fs::write(safe_file(dir, &draft.id)?, text)
}

/// Whether this draft is still waiting.
pub fn draft_exists(dir: &Path, id: &str) -> io::Result<bool> {
    safe_file(dir, id)?.try_exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Row<'a> = (&'a str, &'a str, &'a str, AttemptState, &'a str);

    /// Attempts as the store reads them back: step, kind, role, state and what it wrote.
    fn recorded(rows: &[Row]) -> Vec<Attempt> {
        use keel_store::rusqlite_value::Value;
        let dir = tempfile::tempdir().unwrap();
        let store = keel_store::Store::open(&dir.path().join("keel.db")).unwrap();
        for (index, (step, kind, role, state, text)) in rows.iter().enumerate() {
            let id = format!("a{index}");
            store
                .add_attempt(&keel_store::NewAttempt {
                    id: id.clone(),
                    run_id: "r".into(),
                    step_id: (*step).into(),
                    kind: (*kind).into(),
                    role: (*role).into(),
                    round: 0,
                    number: index as i64 + 1,
                    state: *state,
                    model_requested: "fake".into(),
                    effort: None,
                    tools: "readOnly".into(),
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
            store
                .patch_attempt(&id, &[("result_text", Value::Text((*text).into()))])
                .unwrap();
        }
        store.attempts_of_run("r").unwrap()
    }

    fn asked(rows: &[Row]) -> Option<String> {
        let attempts = recorded(rows);
        specifying_attempt(&attempts).and_then(asked_text)
    }

    use AttemptState::{Failed, Succeeded};

    #[test]
    fn a_workflow_saved_before_the_role_says_what_was_asked_in_a_step_called_spec() {
        assert_eq!(
            asked(&[
                (
                    "spec",
                    "agent",
                    "worker",
                    Succeeded,
                    "  add returns the sum\n"
                ),
                ("builder", "agent", "worker", Succeeded, "Fixed it."),
            ])
            .as_deref(),
            Some("add returns the sum")
        );
    }

    #[test]
    fn the_specifiers_role_counts_for_more_than_a_steps_name() {
        assert_eq!(
            asked(&[
                ("plan", "agent", "specifier", Succeeded, "From the role"),
                ("spec", "agent", "worker", Succeeded, "From the name"),
            ])
            .as_deref(),
            Some("From the role")
        );
    }

    #[test]
    fn what_was_asked_is_the_latest_attempt_that_worked() {
        assert_eq!(
            asked(&[
                ("spec", "agent", "worker", Succeeded, "First try"),
                ("spec", "agent", "worker", Succeeded, "Second try"),
                ("spec", "agent", "worker", Failed, "Third try"),
            ])
            .as_deref(),
            Some("Second try")
        );
    }

    #[test]
    fn nothing_says_what_was_asked_without_an_agent_that_wrote_it() {
        assert_eq!(
            asked(&[("builder", "agent", "worker", Succeeded, "Fixed it.")]),
            None,
            "no such step"
        );
        assert_eq!(
            asked(&[("spec", "check", "worker", Succeeded, "make test")]),
            None,
            "a check that happens to have that name"
        );
        assert_eq!(
            asked(&[("spec", "agent", "worker", Succeeded, "  \n")]),
            None,
            "it wrote nothing"
        );
    }

    fn worker(id: &str, tools: ToolProfile) -> StepSpec {
        agent(id, "opus", tools, StepRole::Worker, &[], "do it")
    }

    #[test]
    fn delivery_is_next_only_right_after_the_step_not_later_or_never() {
        let human = || WorkflowStep::Approval { id: "h".into() };
        let pr = || WorkflowStep::Deliver { id: "pr".into() };
        let builder = || WorkflowStep::Agent(worker("b", ToolProfile::Build));
        let learner = || {
            WorkflowStep::Agent(agent(
                "l",
                "opus",
                ToolProfile::ReadOnly,
                StepRole::Learner,
                &[],
                "learn",
            ))
        };
        assert!(delivery_is_next(&[builder(), human(), pr()], "h"));
        assert!(delivery_is_next(&[human(), pr(), learner()], "h"));
        assert!(!delivery_is_next(&[human(), builder(), pr()], "h"));
        assert!(!delivery_is_next(&[builder(), human()], "h"));
        assert!(!delivery_is_next(&[builder(), human(), learner()], "h"));
        assert!(!delivery_is_next(&[human(), pr()], "missing"));
    }

    #[test]
    fn the_built_in_workflows_are_valid_in_both_languages_with_and_without_a_check() {
        for language in ["en", "es"] {
            for check in [None, Some("make test")] {
                for workflow in builtin(language, check) {
                    assert_eq!(validate(&workflow.steps), Ok(()), "{}", workflow.id);
                }
            }
        }
    }

    #[test]
    fn the_full_workflow_follows_the_diagram_with_a_model_per_step() {
        let full = builtin("es", Some("make test")).remove(0);
        let ids: Vec<&str> = full.steps.iter().map(WorkflowStep::id).collect();
        assert_eq!(
            ids,
            [
                "spec",
                "builder",
                "check",
                "reviews",
                "aprobador",
                "humano",
                "pull-request",
                "aprendizajes"
            ]
        );
        let WorkflowStep::Parallel { steps: group, .. } = &full.steps[3] else {
            panic!()
        };
        assert_eq!(
            group.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["seguridad", "ux", "ui", "codigo"]
        );
        assert!(
            group
                .iter()
                .all(|s| s.tools == ToolProfile::ReadOnly && s.role == StepRole::Reviewer)
        );
        assert!(group[0].paths.is_empty(), "security always reviews");
        assert!(!group[1].paths.is_empty(), "UX only with screens");
        let WorkflowStep::Agent(spec) = &full.steps[0] else {
            panic!()
        };
        assert_eq!(spec.model, "opus");
        // It says what happens and asks what only a person can decide, in the run's language.
        assert_eq!(spec.role, StepRole::Specifier);
        assert_eq!(spec.tools, ToolProfile::ReadOnly);
        assert!(spec.prompt.contains("¿qué debe pasar?"));
    }

    #[test]
    fn both_built_in_workflows_ask_with_the_specifier_and_can_take_work_back() {
        for language in ["en", "es"] {
            for workflow in builtin(language, Some("make test")) {
                let spec = find_step(&workflow.steps, "spec").unwrap();
                let WorkflowStep::Agent(spec) = spec else {
                    panic!()
                };
                assert_eq!(spec.role, StepRole::Specifier, "{}", workflow.id);
                let WorkflowStep::Agent(builder) = find_step(&workflow.steps, "builder").unwrap()
                else {
                    panic!()
                };
                for token in ["{{rework}}", "{{answers}}", "{{steps.spec.output}}"] {
                    assert!(builder.prompt.contains(token), "{} {token}", workflow.id);
                }
            }
        }
    }

    fn approver(rework_to: &str) -> StepSpec {
        agent(
            "approver",
            "opus",
            ToolProfile::ReadOnly,
            StepRole::Approver {
                rework_to: rework_to.into(),
                max_rounds: 2,
            },
            &[],
            "decide",
        )
    }

    fn step_of(spec: StepSpec) -> WorkflowStep {
        WorkflowStep::Agent(spec)
    }

    #[test]
    fn rework_starts_where_the_approver_says_or_else_at_the_last_worker_that_builds() {
        let reader = || step_of(worker("reader", ToolProfile::ReadOnly));
        let first = || step_of(worker("first", ToolProfile::Build));
        let second = || step_of(worker("second", ToolProfile::Build));
        let human = || WorkflowStep::Approval { id: "h".into() };
        let check = || WorkflowStep::Check {
            id: "c".into(),
            run: "x".into(),
        };
        assert_eq!(
            rework_index(&[first(), second(), step_of(approver("first")), human()]),
            Some(0),
            "an approver decides"
        );
        assert_eq!(
            rework_index(&[reader(), first(), second(), check(), human()]),
            Some(2),
            "the last that builds"
        );
        assert_eq!(rework_index(&[reader(), human()]), None, "nothing builds");
        assert_eq!(
            rework_index(&[first(), human(), second()]),
            Some(0),
            "nobody can send work back to what comes after the last approval"
        );
        let mut judge = worker("judge", ToolProfile::Build);
        judge.role = StepRole::Reviewer;
        assert_eq!(
            rework_index(&[step_of(judge), human()]),
            None,
            "only a worker does the work again"
        );
    }

    #[test]
    fn a_person_can_only_send_back_what_comes_before_them() {
        let builder = || step_of(worker("b", ToolProfile::Build));
        let human = || WorkflowStep::Approval { id: "h".into() };
        assert!(can_send_back(&[builder(), human()], "h"));
        assert!(!can_send_back(&[human(), builder()], "h"));
        assert!(!can_send_back(
            &[step_of(worker("r", ToolProfile::ReadOnly)), human()],
            "h"
        ));
        assert!(!can_send_back(&[builder(), human()], "missing"));
        // An approver's target after the person asking takes nothing back either.
        assert!(!can_send_back(
            &[human(), builder(), step_of(approver("b"))],
            "h"
        ));
    }

    fn with_output(attempts: &mut [Attempt], index: usize, state: &str, output: Value) {
        attempts[index].state = state.into();
        attempts[index].output = Some(output);
    }

    #[test]
    fn what_comes_back_says_who_asked_and_never_what_an_approver_only_noted() {
        let mut attempts = recorded(&[
            ("approver", "agent", "approver", Succeeded, ""),
            ("human", "approval", "worker", Succeeded, ""),
        ]);
        with_output(
            &mut attempts,
            0,
            "succeeded",
            serde_json::json!({ "decision": "approve", "instructions": "Nothing to do" }),
        );
        with_output(
            &mut attempts,
            1,
            "returned",
            serde_json::json!({ "instructions": "Return 0 instead" }),
        );
        let text = rework_text(&attempts, 1, "en");
        assert!(text.starts_with("A person sent this work back."), "{text}");
        assert!(text.ends_with("Return 0 instead"), "{text}");
        assert!(!text.contains("Nothing to do"), "{text}");
        assert!(rework_text(&attempts, 1, "es").starts_with("Una persona devolvió"));
        assert_eq!(rework_text(&attempts, 0, "en"), "", "the first round");
        assert_eq!(rework_text(&attempts, 2, "en"), "", "only the last round");
        assert!(returned_last_round(&attempts, 1));
        assert!(!returned_last_round(&attempts, 2));
        assert_eq!(rounds_asked_by_a_person(&attempts), 1);

        with_output(
            &mut attempts,
            0,
            "succeeded",
            serde_json::json!({ "decision": "rework", "instructions": "Fix it" }),
        );
        with_output(&mut attempts, 1, "succeeded", serde_json::json!({}));
        let text = rework_text(&attempts, 1, "en");
        assert!(
            text.starts_with("The approver sent this work back."),
            "{text}"
        );
        assert!(text.ends_with("Fix it"), "{text}");
    }

    #[test]
    fn a_person_answers_reach_a_prompt_as_plain_text_and_no_answers_as_nothing() {
        let mut attempts = recorded(&[("spec", "agent", "specifier", Succeeded, "")]);
        assert_eq!(answers_text(&attempts, "en"), "");
        attempts[0].output = Some(serde_json::json!({
            "answered": [{ "question": "Q1", "asked": "If it is 0?", "answer": "Return 0", "recommended": false, "remember": false }],
        }));
        assert_eq!(
            answers_text(&attempts, "en"),
            "The person has already answered this:\n- If it is 0? Return 0"
        );
    }

    #[test]
    fn what_a_structured_answer_asked_reads_as_text_and_a_broken_one_as_what_it_wrote() {
        let mut attempts = recorded(&[("spec", "agent", "specifier", Succeeded, "Plain words")]);
        assert_eq!(asked_text(&attempts[0]).as_deref(), Some("Plain words"));
        attempts[0].output = Some(serde_json::json!({
            "summary": "add returns the sum",
            "criteria": [{ "id": "C1", "when": "add(1, 2)", "then": "3" }],
        }));
        let text = asked_text(&attempts[0]).unwrap();
        assert!(text.starts_with("add returns the sum"), "{text}");
        assert!(text.contains("C1: add(1, 2) → 3"), "{text}");
        // A plain worker's output says nothing about what was asked.
        attempts[0].role = "worker".into();
        assert_eq!(asked_text(&attempts[0]).as_deref(), Some("Plain words"));
    }

    #[test]
    fn a_specifier_has_to_stay_read_only_and_alone() {
        let mut spec = worker("spec", ToolProfile::Build);
        spec.role = StepRole::Specifier;
        assert_eq!(
            validate(&[WorkflowStep::Agent(spec.clone())]),
            Err(problem("reviewerMustReadOnly", "spec"))
        );
        spec.tools = ToolProfile::ReadOnly;
        assert_eq!(
            validate(&[WorkflowStep::Parallel {
                id: "g".into(),
                steps: vec![spec],
            }]),
            Err(problem("groupRole", "spec"))
        );
    }

    #[test]
    fn the_editor_and_the_engine_read_a_workflow_the_same_way() {
        // Exactly what `src/views/WorkflowsView.tsx` sends when it saves.
        let sent = serde_json::json!({
            "id": "", "name": "Mine", "builtin": false,
            "steps": [
                { "type": "agent", "id": "builder", "model": "sonnet", "effort": "high", "tools": "build", "prompt": "Do it",
                  "maxTurns": null, "maxBudgetUsd": null, "paths": [], "role": { "kind": "worker" } },
                { "type": "parallel", "id": "reviews", "steps": [
                    { "id": "ux", "model": "claude-haiku-5-5", "effort": null, "tools": "readOnly", "prompt": "Look",
                      "maxTurns": null, "maxBudgetUsd": null, "paths": ["**/*.tsx"], "role": { "kind": "reviewer" } },
                ] },
                { "type": "agent", "id": "approver", "model": "opus", "effort": null, "tools": "readOnly", "prompt": "Decide",
                  "maxTurns": null, "maxBudgetUsd": null, "paths": [], "role": { "kind": "approver", "reworkTo": "builder", "maxRounds": 2 } },
                { "type": "check", "id": "check", "run": "make test" },
                { "type": "approval", "id": "human" },
                { "type": "deliver", "id": "pr" },
            ],
        });
        let workflow: Workflow = serde_json::from_value(sent).unwrap();
        assert_eq!(validate(&workflow.steps), Ok(()));
        let WorkflowStep::Agent(builder) = &workflow.steps[0] else {
            panic!()
        };
        assert_eq!(
            (builder.model.as_str(), builder.effort.as_deref()),
            ("sonnet", Some("high"))
        );
        let WorkflowStep::Parallel { steps: group, .. } = &workflow.steps[1] else {
            panic!()
        };
        assert_eq!(group[0].paths, ["**/*.tsx"]);
        assert_eq!(group[0].role, StepRole::Reviewer);
        let WorkflowStep::Agent(approver) = &workflow.steps[2] else {
            panic!()
        };
        assert_eq!(
            approver.role,
            StepRole::Approver {
                rework_to: "builder".into(),
                max_rounds: 2
            }
        );

        // And what comes back has the shape the screens read.
        let back = serde_json::to_value(&workflow).unwrap();
        assert_eq!(back["steps"][2]["role"]["reworkTo"], "builder");
        assert_eq!(back["steps"][2]["role"]["maxRounds"], 2);
        assert_eq!(back["steps"][1]["steps"][0]["tools"], "readOnly");
    }

    #[test]
    fn a_workflow_can_never_deliver_without_a_person_approving() {
        let steps = vec![
            WorkflowStep::Agent(worker("a", ToolProfile::Build)),
            WorkflowStep::Deliver { id: "pr".into() },
        ];
        assert_eq!(validate(&steps), Err(problem("deliverNeedsApproval", "pr")));
        let steps = vec![
            WorkflowStep::Deliver { id: "pr".into() },
            WorkflowStep::Approval { id: "human".into() },
        ];
        assert!(validate(&steps).is_err(), "approving after is too late");
    }

    #[test]
    fn only_read_only_agents_can_run_in_parallel_or_judge() {
        let steps = vec![WorkflowStep::Parallel {
            id: "g".into(),
            steps: vec![worker("a", ToolProfile::Build)],
        }];
        assert_eq!(validate(&steps), Err(problem("parallelMustReadOnly", "a")));
        let mut judge = worker("a", ToolProfile::Build);
        judge.role = StepRole::Reviewer;
        assert_eq!(
            validate(&[WorkflowStep::Agent(judge)]),
            Err(problem("reviewerMustReadOnly", "a"))
        );
    }

    #[test]
    fn steps_that_look_at_the_whole_run_cannot_run_in_a_group_or_too_early() {
        let mut approver = worker("a", ToolProfile::ReadOnly);
        approver.role = StepRole::Approver {
            rework_to: "x".into(),
            max_rounds: 1,
        };
        let mut learner = worker("b", ToolProfile::ReadOnly);
        learner.role = StepRole::Learner;
        for spec in [approver, learner.clone()] {
            let id = spec.id.clone();
            let steps = vec![WorkflowStep::Parallel {
                id: "g".into(),
                steps: vec![spec],
            }];
            assert_eq!(validate(&steps), Err(problem("groupRole", &id)));
        }
        let early = vec![
            WorkflowStep::Agent(learner.clone()),
            WorkflowStep::Approval { id: "h".into() },
        ];
        assert_eq!(validate(&early), Err(problem("learnerNotLast", "b")));
        let last = vec![
            WorkflowStep::Approval { id: "h".into() },
            WorkflowStep::Agent(learner),
        ];
        assert_eq!(validate(&last), Ok(()));
    }

    #[test]
    fn bad_workflows_say_what_is_wrong() {
        assert_eq!(validate(&[]), Err(problem("empty", "")));
        let twice = vec![
            WorkflowStep::Approval { id: "a".into() },
            WorkflowStep::Approval { id: "a".into() },
        ];
        assert_eq!(validate(&twice), Err(problem("duplicateId", "a")));
        assert_eq!(
            validate(&[WorkflowStep::Approval {
                id: "Bad Id".into()
            }]),
            Err(problem("badId", "Bad Id"))
        );
        assert_eq!(
            validate(&[WorkflowStep::Check {
                id: "c".into(),
                run: " ".into()
            }]),
            Err(problem("noCommand", "c"))
        );
        let mut no_model = worker("a", ToolProfile::Build);
        no_model.model = String::new();
        assert_eq!(
            validate(&[WorkflowStep::Agent(no_model)]),
            Err(problem("noModel", "a"))
        );
        let mut approver = worker("a", ToolProfile::ReadOnly);
        approver.role = StepRole::Approver {
            rework_to: "later".into(),
            max_rounds: 1,
        };
        assert_eq!(
            validate(&[WorkflowStep::Agent(approver.clone())]),
            Err(problem("reworkTarget", "a"))
        );
        approver.role = StepRole::Approver {
            rework_to: "b".into(),
            max_rounds: 99,
        };
        assert_eq!(
            validate(&[
                WorkflowStep::Agent(worker("b", ToolProfile::Build)),
                WorkflowStep::Agent(approver)
            ]),
            Err(problem("tooManyRounds", "a"))
        );
        assert_eq!(
            validate(&[WorkflowStep::Parallel {
                id: "g".into(),
                steps: vec![]
            }]),
            Err(problem("emptyGroup", "g"))
        );
    }

    #[test]
    fn paths_decide_whether_a_step_has_anything_to_look_at() {
        let screens = SCREEN_FILES.map(str::to_owned);
        assert!(paths_match(&[], &[]), "no limit means always");
        assert!(paths_match(&screens, &["src/views/Home.tsx".to_owned()]));
        assert!(paths_match(&screens, &["a.css".to_owned()]));
        assert!(!paths_match(
            &screens,
            &["app/store.py".to_owned(), "README.md".to_owned()]
        ));
        assert!(!paths_match(&screens, &[]));
    }

    #[test]
    fn workflows_survive_a_trip_through_a_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = builtin("en", Some("make test")).remove(0);
        let saved = save(dir.path(), &original).unwrap();
        assert_eq!(
            saved.id, "full-with-reviews",
            "a built-in becomes the project's own copy"
        );
        assert!(!saved.builtin);
        let text = std::fs::read_to_string(dir.path().join("full-with-reviews.yaml")).unwrap();
        assert!(
            text.contains("type: parallel") && text.contains("model: opus"),
            "readable by a person:\n{text}"
        );
        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].steps, original.steps);

        let again = save(dir.path(), &original).unwrap();
        assert_eq!(
            again.id, "full-with-reviews-2",
            "a second copy doesn't overwrite the first"
        );
        delete(dir.path(), &again.id).unwrap();
        assert_eq!(load_all(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_broken_file_does_not_take_the_others_down() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &builtin("en", None).remove(1)).unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "not: [valid").unwrap();
        assert_eq!(load_all(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn drafts_come_back_newest_first_and_a_broken_file_takes_none_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let draft = |id: &str, created_at: &str| WorkflowDraft {
            id: id.into(),
            project_id: "ignored".into(),
            name: id.into(),
            steps: vec![WorkflowStep::Approval { id: "h".into() }],
            proposed_by: "Claude Code".into(),
            created_at: created_at.into(),
        };
        save_draft(dir.path(), &draft("old", "2026-11-05T10:00:00.000Z")).unwrap();
        save_draft(dir.path(), &draft("new", "2026-11-06T10:00:00.000Z")).unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "not: [a draft").unwrap();

        let found = load_drafts(dir.path(), "p1").unwrap();
        let ids: Vec<&str> = found.iter().map(|draft| draft.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
        assert!(found.iter().all(|draft| draft.project_id == "p1"));

        assert!(draft_exists(dir.path(), "new").unwrap());
        delete(dir.path(), "new").unwrap();
        assert!(!draft_exists(dir.path(), "new").unwrap());
        assert_eq!(load_drafts(&dir.path().join("none"), "p1").unwrap(), []);
    }

    #[test]
    fn a_draft_cannot_reach_outside_its_folder() {
        let root = tempfile::tempdir().unwrap();
        let drafts = root.path().join("workflow-drafts");
        std::fs::create_dir(&drafts).unwrap();
        let mine = root.path().join("workflows");
        std::fs::create_dir(&mine).unwrap();
        std::fs::write(mine.join("mine.yaml"), "name: Mine\nsteps: []\n").unwrap();

        assert!(delete(&drafts, "../workflows/mine").is_err());
        assert!(draft_exists(&drafts, "../workflows/mine").is_err());
        assert!(mine.join("mine.yaml").exists());
    }

    #[test]
    fn ids_cannot_escape_the_workflows_folder() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["../x", "a/b", "..", "builtin:full", ""] {
            assert!(delete(dir.path(), id).is_err(), "{id}");
        }
        let mut workflow = builtin("en", None).remove(1);
        workflow.id = "../../evil".into();
        assert!(save(dir.path(), &workflow).is_err());
    }

    #[test]
    fn long_diffs_are_cut_and_say_so() {
        let cut = cap_diff("é".repeat(DIFF_LIMIT));
        assert!(cut.len() < DIFF_LIMIT + 100);
        assert!(cut.ends_with("Read the files for the rest.]"));
        assert_eq!(cap_diff("short".to_owned()), "short");
    }
}
