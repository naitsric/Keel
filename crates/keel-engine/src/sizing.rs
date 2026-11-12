//! Whether a change is small enough to skip what a workflow says only a bigger one needs. The engine
//! works this out from what it can see for itself, and everything here can only stop a run from
//! counting as small. What an agent says can stop it, and never makes it so. Text in a run can come
//! from someone else's issue, which could call anything trivial to dodge the reviews. A person's
//! approval, the sandbox and the check aren't in question: only agents can have a condition.

use std::collections::HashSet;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use keel_core::{AttemptState, Reason, StepRole, StepSpec, WhyNotSmall, WorkflowStep};
use keel_store::Attempt;
use serde_json::{Value, json};

use crate::git::ChangeSize;
use crate::specs::{self, Size};
use crate::verification::{self, CheckFact};
use crate::workflows;

/// This doesn't classify anything: it catches a prediction that turned out wrong.
pub const MAX_CHANGED_LINES: usize = 400;
/// How many files a reason names. The diff has the rest.
const SHOWN_FILES: usize = 5;

/// What's always too sensitive for a change to count as small. A project can add to this and can't
/// take anything away. Matched without regard to case, since a Mac's disk doesn't tell `Auth` from
/// `auth`, and `*` also crosses folders, as in a step's `paths`.
pub const ALWAYS_SENSITIVE: &[&str] = &[
    // What runs by itself once it's pushed, with the repo's secrets.
    "**/.github/**",
    "**/.gitlab-ci.yml",
    "**/.circleci/**",
    // What changes how git treats the other files, or brings in code from elsewhere.
    "**/.gitattributes",
    "**/.gitmodules",
    // What the check goes through, and what brings in code. A small diff here can make a check
    // pass without proving anything, and with the reviews gone nothing else would notice.
    "**/package.json",
    "**/Makefile",
    "**/GNUmakefile",
    "**/*.mk",
    "**/justfile",
    "**/Taskfile.y*ml",
    "**/Rakefile",
    "**/Cargo.toml",
    "**/.cargo/**",
    "**/pyproject.toml",
    "**/setup.py",
    "**/setup.cfg",
    "**/requirements*.txt",
    "**/tox.ini",
    "**/noxfile.py",
    "**/pytest.ini",
    "**/conftest.py",
    "**/vitest.config.*",
    "**/jest.config.*",
    "**/go.mod",
    "**/Gemfile",
    "**/pom.xml",
    "**/build.gradle*",
    "**/settings.gradle*",
    "**/Dockerfile*",
    "**/docker-compose*",
    "**/compose.y*ml",
    "**/build.rs",
    "**/vite.config.*",
    "**/webpack.config.*",
    "**/rollup.config.*",
    "**/.pre-commit-config.yaml",
    "**/.husky/**",
    "**/lefthook.y*ml",
    // Migrations and the shape of the data. Not easily undone once they've run.
    "**/migrations/**",
    "**/migrate/**",
    "**/alembic/**",
    "**/*migration*",
    "**/*.sql",
    "**/*.ddl",
    "**/*.prisma",
    "**/schema.rb",
    // Who can do what. Not `*auth*`, which would catch every author.
    "**/auth/**",
    "**/*auth.*",
    "**/auth[-_.]*",
    "**/*[-_]auth[-_.]*",
    "**/*oauth*",
    "**/*authn*",
    "**/*authz*",
    "**/*authenticat*",
    "**/*authoriz*",
    "**/*permission*",
    "**/*rbac*",
    "**/*login*",
    "**/*passw*",
    // Secrets, and the files they tend to end up in.
    "**/*secret*",
    "**/*credential*",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/*.jks",
    "**/*.keystore",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/.npmrc",
    "**/.pypirc",
    "**/.netrc",
    // Environment files.
    "**/.env",
    "**/.env.*",
    "**/*.env",
    "**/.envrc",
    // Lock files: a long diff nobody reads, and a good place to hide a dependency.
    "**/*.lock",
    "**/*.lockfile",
    "**/*.lockb",
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/pnpm-lock.yaml",
    "**/packages.lock.json",
    "**/Package.resolved",
    "**/go.sum",
];

/// What a project's own pattern means. People write `billing/` or `*.proto`, so a folder is one with
/// that name anywhere, and a name also covers a folder called that.
fn expanded(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim();
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let pattern = pattern.strip_prefix("**/").unwrap_or(pattern);
    if pattern.is_empty() {
        Vec::new()
    } else if pattern.ends_with('/') {
        vec![format!("**/{pattern}**")]
    } else {
        vec![format!("**/{pattern}"), format!("**/{pattern}/**")]
    }
}

fn add_to(set: &mut GlobSetBuilder, pattern: &str) -> Result<(), globset::Error> {
    set.add(GlobBuilder::new(pattern).case_insensitive(true).build()?);
    Ok(())
}

/// Everything sensitive for this project. The pattern it couldn't read, if there is one: that isn't
/// something to quietly leave out, since then nothing would ever match it.
fn sensitive_set(own: &[String]) -> Result<GlobSet, String> {
    let mut set = GlobSetBuilder::new();
    for pattern in ALWAYS_SENSITIVE {
        add_to(&mut set, pattern).map_err(|_| (*pattern).to_owned())?;
    }
    for pattern in own {
        for glob in expanded(pattern) {
            add_to(&mut set, &glob).map_err(|_| pattern.clone())?;
        }
    }
    set.build().map_err(|_| String::new())
}

/// Refuses a project's patterns if one can't be read, for when a person saves them.
pub fn check_patterns(own: &[String]) -> Result<(), Reason> {
    sensitive_set(own)
        .map(|_| ())
        .map_err(|value| Reason::InvalidSetting {
            field: "sensitivePaths".to_owned(),
            value,
        })
}

/// The files named in a reason, and how many there were.
fn named(files: Vec<String>) -> (Vec<String>, usize) {
    let total = files.len();
    (files.into_iter().take(SHOWN_FILES).collect(), total)
}

/// What the engine came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Small {
        lines: usize,
        files: usize,
    },
    /// Each failed condition, for the person and the results.
    NotSmall(Vec<WhyNotSmall>),
    /// A step that depends on this has already really run in this run. It stays that way in every
    /// later round, and it was already said why.
    StaysUp,
}

impl Decision {
    /// What goes on the record. Nothing for `StaysUp`, which isn't a new decision.
    pub fn on_record(&self, round: i64) -> Option<Value> {
        match self {
            Self::Small { lines, files } => {
                Some(json!({ "small": true, "lines": lines, "files": files, "round": round }))
            }
            Self::NotSmall(why) => Some(json!({ "small": false, "because": why, "round": round })),
            Self::StaysUp => None,
        }
    }
}

/// What a decision goes on. All of it is read, and none of it is taken from what an agent says it did.
pub struct Facts<'a> {
    pub steps: &'a [WorkflowStep],
    pub attempts: &'a [Attempt],
    /// The run's round. Attempts say which one they belong to, and it counts rounds a person asked
    /// for too, so nothing here does arithmetic on it.
    pub round: i64,
    pub issue: Option<&'a Value>,
    pub account: Option<&'a str>,
    /// The assistant that started the run over MCP, if one did.
    pub started_by: Option<&'a str>,
    pub sensitive: &'a [String],
    /// What the project runs to set up and to check.
    pub commands: &'a [String],
    /// `None` if it couldn't be read.
    pub change: Option<ChangeSize>,
    /// Whether the run does one part of a change across projects.
    pub part_of_joint_change: bool,
}

fn skipped_for(attempt: &Attempt, code: &str) -> bool {
    attempt.state == AttemptState::Skipped.as_str()
        && attempt.reason.as_ref().and_then(|r| r.get("code")) == Some(&json!(code))
}

pub fn skipped_as_small(attempt: &Attempt) -> bool {
    skipped_for(attempt, "treatedAsSmall")
}

/// Left out because what was asked names none of the risks it looks at. That's settled before the
/// size, so it says nothing about it either way.
pub fn skipped_for_risks(attempt: &Attempt) -> bool {
    skipped_for(attempt, "noMatchingRisks")
}

/// Whether a person asked for what was skipped to run, at any point in this run.
pub fn person_asked(attempts: &[Attempt]) -> bool {
    attempts
        .iter()
        .any(|attempt| attempt.state == AttemptState::Postponed.as_str())
}

/// An approval a person put off comes round again, and what they asked for runs before it. Asking
/// keeps the run from counting as small, so nothing gets skipped this way after that, and the attempts
/// alone say what's due: a restart loses nothing.
pub fn due_again(attempt: &Attempt, attempts: &[Attempt]) -> bool {
    attempt.state == AttemptState::Postponed.as_str()
        || (skipped_as_small(attempt) && person_asked(attempts))
}

/// The steps that stand skipped for being a small change: the ones a person can ask for. Once they've
/// asked, all of it is on its way.
pub fn skipped_steps(attempts: &[Attempt]) -> Vec<String> {
    if person_asked(attempts) {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut found: Vec<String> = attempts
        .iter()
        .rev()
        .filter(|attempt| seen.insert(attempt.step_id.as_str()))
        .filter(|attempt| skipped_as_small(attempt))
        .map(|attempt| attempt.step_id.clone())
        .collect();
    found.reverse();
    found
}

fn conditional_step_ids(steps: &[WorkflowStep]) -> HashSet<&str> {
    steps
        .iter()
        .flat_map(|step| match step {
            WorkflowStep::Agent(spec) => vec![spec],
            WorkflowStep::Parallel { steps, .. } => steps.iter().collect(),
            _ => Vec::new(),
        })
        .filter(|spec: &&StepSpec| spec.when.is_some())
        .map(|spec| spec.id.as_str())
        .collect()
}

/// Whether a step that only runs for a change that isn't small has been past the point of being
/// skipped for it. That's what keeps a run from going back down in a later round, across a person's
/// send-back or a restart. A step passed over because its paths didn't match counts: those are only
/// looked at once the change has been found not to be small. One its risks left out doesn't: that
/// comes first, small or not.
pub fn already_up(steps: &[WorkflowStep], attempts: &[Attempt]) -> bool {
    let conditional = conditional_step_ids(steps);
    attempts.iter().any(|a| {
        conditional.contains(a.step_id.as_str()) && !skipped_as_small(a) && !skipped_for_risks(a)
    })
}

pub(crate) fn why_from_spec(attempts: &[Attempt]) -> Vec<WhyNotSmall> {
    // A spec from before this was asked for reads as normal, and so does a step that wrote free text.
    let Some(spec) = workflows::specifying_attempt(attempts).and_then(specs::of_attempt) else {
        return vec![WhyNotSmall::SpecNotSmall];
    };
    let mut why = Vec::new();
    if spec.size != Size::Small {
        why.push(WhyNotSmall::SpecNotSmall);
    }
    // Small means understood with nothing left to ask, and this was left to ask.
    if spec
        .decided
        .iter()
        .any(|decided| decided.taken == Some(specs::Taken::RoundsLimit))
    {
        why.push(WhyNotSmall::KeelChoseAnAnswer);
    }
    // Any value at all counts: one Keel doesn't know must never read as no risk.
    if !spec.risks.is_empty() {
        why.push(WhyNotSmall::SpecNamesRisks { risks: spec.risks });
    }
    why
}

fn why_from_check(steps: &[WorkflowStep], attempts: &[Attempt], round: i64) -> Option<WhyNotSmall> {
    let wanted = steps
        .iter()
        .filter(|step| matches!(step, WorkflowStep::Check { .. }))
        .count();
    why_from_fact(
        &verification::checks_in_round(steps, attempts, round),
        wanted,
    )
}

/// What the round's checks say against it being small, when the workflow has `wanted` of them.
fn why_from_fact(fact: &CheckFact, wanted: usize) -> Option<WhyNotSmall> {
    match fact {
        CheckFact::NoCheckInWorkflow => Some(WhyNotSmall::NoCheck),
        // A check further along still has its say, and it can't be counted before it's had it.
        CheckFact::Ran { checks } if fact.passed() && checks.len() >= wanted => {
            // What a skipped test would have proved, nothing did. When the tools don't say, the
            // check counts as it always has.
            match fact.skipped() {
                0 => None,
                count => Some(WhyNotSmall::CheckSkippedTests { count }),
            }
        }
        _ => Some(WhyNotSmall::CheckNotPassed),
    }
}

/// A run a person typed has no issue. Anyone can write one, and an unknown author could be anyone.
fn why_from_issue(issue: Option<&Value>, account: Option<&str>) -> Option<WhyNotSmall> {
    let issue = issue?;
    let author = issue
        .get("author")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|author| !author.is_empty());
    let account = account.map(str::trim).filter(|account| !account.is_empty());
    match (author, account) {
        (None, _) => Some(WhyNotSmall::IssueFromUnknownAuthor),
        (Some(_), None) => Some(WhyNotSmall::IssueWithNoAccount),
        // GitHub logins don't go by case.
        (Some(author), Some(account)) if !author.eq_ignore_ascii_case(account) => {
            Some(WhyNotSmall::IssueFromSomeoneElse {
                author: author.to_owned(),
            })
        }
        _ => None,
    }
}

/// What runs the file it's given, rather than only reading it.
const INTERPRETERS: [&str; 14] = [
    "sh", "bash", "zsh", "dash", "python", "python3", "node", "deno", "bun", "ruby", "perl", "php",
    "uv", "tsx",
];

/// A file a test command takes as an argument is usually the test itself, which changes with the code.
fn looks_like_a_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|part| matches!(part, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// The files a project's commands run, as written in them. With `./scripts/test.sh` as the check, one
/// line in that script can make it pass. Nothing here understands a shell: it can only add to the list.
fn run_by_commands(commands: &[String]) -> HashSet<String> {
    let mut found = HashSet::new();
    for command in commands {
        for part in command.split(['&', '|', ';', '\n']) {
            let words: Vec<&str> = part
                .split_whitespace()
                .map(|word| word.trim_matches(['"', '\'']))
                .map(|word| word.strip_prefix("./").unwrap_or(word))
                .filter(|word| !word.contains('='))
                .collect();
            let Some((first, rest)) = words.split_first() else {
                continue;
            };
            found.insert((*first).to_owned());
            // What `grep` or `test -f` is given is only data. What `node` is given is what runs.
            if INTERPRETERS.contains(first) {
                let scripts = rest.iter().filter(|word| !looks_like_a_test(word));
                found.extend(scripts.map(|word| (*word).to_owned()));
            }
        }
    }
    found
}

fn why_from_change(
    change: Option<&ChangeSize>,
    own: &[String],
    commands: &[String],
) -> Vec<WhyNotSmall> {
    let Some(change) = change else {
        return vec![WhyNotSmall::DiffUnreadable];
    };
    let mut why = Vec::new();
    let run_by = run_by_commands(commands);
    match sensitive_set(own) {
        Ok(set) => {
            let touched: Vec<String> = change
                .files
                .iter()
                .filter(|file| set.is_match(file) || run_by.contains(file.as_str()))
                .cloned()
                .collect();
            if !touched.is_empty() {
                let (files, total) = named(touched);
                why.push(WhyNotSmall::SensitiveFiles { files, total });
            }
        }
        Err(pattern) => why.push(WhyNotSmall::BadSensitivePattern { pattern }),
    }
    if change.lines >= MAX_CHANGED_LINES {
        why.push(WhyNotSmall::TooManyLines {
            lines: change.lines,
            limit: MAX_CHANGED_LINES,
        });
    }
    if !change.without_lines.is_empty() {
        let (files, total) = named(change.without_lines.clone());
        why.push(WhyNotSmall::FilesWithoutLines { files, total });
    }
    why
}

/// With the reviews gone, nothing else reacts to what a verifier found, so this has to come after it
/// has looked at the code as it is. A saved workflow can put one anywhere, and one that hasn't run
/// yet says nothing. An empty list means that, and not that it's fine.
fn why_from_verifier(steps: &[WorkflowStep], attempts: &[Attempt], round: i64) -> Vec<WhyNotSmall> {
    let verifiers: Vec<&str> = steps
        .iter()
        .filter_map(|step| match step {
            WorkflowStep::Agent(spec) if spec.role == StepRole::Verifier => Some(spec.id.as_str()),
            _ => None,
        })
        .collect();
    if verifiers.is_empty() {
        return Vec::new();
    }
    let mut why = Vec::new();
    let unmet = verification::in_round(attempts, round)
        .iter()
        .map(|verification| verification.counts().unmet)
        .max()
        .unwrap_or(0);
    if unmet > 0 {
        why.push(WhyNotSmall::VerifierFoundUnmet { count: unmet });
    }
    let in_round: Vec<&Attempt> = attempts.iter().filter(|a| a.round == round).collect();
    let code_settled = verification::after_last_build(&in_round);
    let looked = |id: &str| {
        in_round[code_settled..]
            .iter()
            .rev()
            .find(|a| a.step_id == id)
            .is_some_and(|a| verification::of_attempt(a).is_some())
    };
    if !verifiers.iter().all(|id| looked(id)) {
        why.push(WhyNotSmall::VerifierHasNotRun);
    }
    why
}

/// A run is small only if every one of these holds.
pub fn decide(facts: &Facts) -> Decision {
    let mut why = why_from_spec(facts.attempts);
    why.extend(why_from_check(facts.steps, facts.attempts, facts.round));
    why.extend(why_from_issue(facts.issue, facts.account));
    why.extend(
        facts
            .started_by
            .map(|client| WhyNotSmall::StartedByAssistant {
                client: client.to_owned(),
            }),
    );
    why.extend(why_from_change(
        facts.change.as_ref(),
        facts.sensitive,
        facts.commands,
    ));
    why.extend(why_from_verifier(facts.steps, facts.attempts, facts.round));
    // However small it looks here, it only works with the other parts, and nothing here proves that.
    if facts.part_of_joint_change {
        why.push(WhyNotSmall::PartOfJointChange);
    }
    match (&facts.change, why.is_empty()) {
        (Some(change), true) => Decision::Small {
            lines: change.lines,
            files: change.files.len(),
        },
        _ => Decision::NotSmall(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::ToolProfile;

    fn change(files: &[&str], lines: usize) -> ChangeSize {
        ChangeSize {
            files: files.iter().map(|file| (*file).to_owned()).collect(),
            lines,
            without_lines: Vec::new(),
        }
    }

    fn sensitive(file: &str, own: &[&str]) -> bool {
        let own: Vec<String> = own.iter().map(|pattern| (*pattern).to_owned()).collect();
        !why_from_change(Some(&change(&[file], 1)), &own, &[]).is_empty()
    }

    #[test]
    fn what_is_always_sensitive_is_all_readable_and_covers_what_it_says() {
        assert_eq!(check_patterns(&[]), Ok(()));
        for file in [
            ".github/workflows/ci.yml",
            "packages/app/.github/CODEOWNERS",
            ".gitlab-ci.yml",
            ".gitattributes",
            ".gitmodules",
            "db/migrations/0012_add.sql",
            "db/migrate/20260101_add.rb",
            "alembic/versions/a.py",
            "crates/store/src/migration.rs",
            "schema.sql",
            "db/schema.rb",
            "prisma/schema.prisma",
            "app/auth/views.py",
            "src/auth.ts",
            "src/lib/oauth.rs",
            "src/useAuth.ts",
            "auth_helpers.py",
            "auth-service/index.ts",
            "pkg/basic_auth.go",
            "src/AuthorizationPolicy.java",
            "src/Authentication.kt",
            "config/permissions.yml",
            "app/rbac.py",
            "src/LoginPage.tsx",
            "src/password_reset.py",
            "config/secrets.yml",
            "deploy/aws-credentials.json",
            "certs/server.pem",
            "certs/server.key",
            "id_rsa",
            ".npmrc",
            ".env",
            ".env.production",
            "config/prod.env",
            ".envrc",
            "Cargo.lock",
            "yarn.lock",
            "poetry.lock",
            "app/Gemfile.lock",
            "bun.lockb",
            "package-lock.json",
            "pnpm-lock.yaml",
            "go.sum",
            "ios/Package.resolved",
        ] {
            assert!(sensitive(file, &[]), "{file}");
        }
    }

    #[test]
    fn it_is_not_so_broad_that_nothing_is_ever_small() {
        for file in [
            "README.md",
            "docs/authoring.md",
            "src/lib/author.ts",
            "src/views/Home.tsx",
            "src/styles/tokens.css",
            "src/plugin/index.ts",
            "crates/keel-engine/src/lib.rs",
            "tests/test_calc.py",
            "calc.py",
            "src/makefile_parser.ts",
            "docs/package.md",
            "src/build.rs.md",
            "src/monkey.ts",
            "src/lib/session.test.ts",
        ] {
            assert!(!sensitive(file, &[]), "{file}");
        }
    }

    #[test]
    fn what_a_check_goes_through_is_sensitive_so_a_small_diff_cannot_quietly_weaken_it() {
        for file in [
            "package.json",
            "web/package.json",
            "Makefile",
            "Cargo.toml",
            "crates/keel-core/Cargo.toml",
            ".cargo/config.toml",
            "pyproject.toml",
            "requirements-dev.txt",
            "tests/conftest.py",
            "vitest.config.ts",
            "go.mod",
            "Dockerfile",
            "Dockerfile.dev",
            "docker-compose.yml",
        ] {
            assert!(sensitive(file, &[]), "{file}");
        }
    }

    #[test]
    fn what_a_project_s_own_commands_run_is_sensitive_but_not_the_test_a_command_names() {
        let touches = |file: &str, command: &str| {
            !why_from_change(Some(&change(&[file], 1)), &[], &[command.to_owned()]).is_empty()
        };
        assert!(touches("scripts/test.sh", "./scripts/test.sh"));
        assert!(touches("scripts/test.sh", "bash scripts/test.sh"));
        assert!(touches(
            "scripts/check.mjs",
            "pnpm i && node scripts/check.mjs"
        ));
        assert!(touches("run_test.sh", "CI=1 ./run_test.sh"));
        assert!(touches("check.py", "uv run 'check.py' | tee out"));
        // Tests change along with the code. If this counted, nothing on such a project would be small.
        assert!(!touches("test_calc.py", "python3 test_calc.py"));
        assert!(!touches("tests/calc.py", "python3 tests/calc.py"));
        assert!(!touches("src/calc.test.ts", "node src/calc.test.ts"));
        assert!(!touches("calc.py", "python3 test_calc.py"));
        // Only what's run, and not what a command happens to read.
        assert!(!touches("built.txt", "test -f built.txt"));
        assert!(!touches("notes.md", "grep ok notes.md"));
    }

    #[test]
    fn case_does_not_get_a_file_past_the_list() {
        for file in [".GitHub/workflows/ci.yml", "Migrations/1.py", "AUTH/x.py"] {
            assert!(sensitive(file, &[]), "{file}");
        }
    }

    #[test]
    fn a_projects_own_patterns_read_the_way_people_write_them() {
        let own = |pattern: &str, file: &str| sensitive(file, &[pattern]);
        assert!(!sensitive("billing/charge.py", &[]));
        assert!(own("billing/", "billing/charge.py"));
        assert!(own("billing/", "apps/billing/charge.py"), "anywhere");
        assert!(
            own("billing", "billing/charge.py"),
            "a name covers a folder"
        );
        assert!(own("./billing/", "billing/charge.py"));
        assert!(own("/billing/", "billing/charge.py"));
        assert!(own("*.proto", "api/v1/user.proto"));
        assert!(own("**/*.proto", "api/v1/user.proto"));
        assert!(own("src/pay.py", "src/pay.py"));
        assert!(!own("billing/", "billings.md"));
        assert!(!own("  ", "billing/charge.py"), "a blank line is nothing");
    }

    #[test]
    fn a_pattern_that_cannot_be_read_is_refused_and_never_quietly_left_out() {
        let bad = vec!["a[".to_owned()];
        assert_eq!(
            check_patterns(&bad),
            Err(Reason::InvalidSetting {
                field: "sensitivePaths".into(),
                value: "a[".into(),
            })
        );
        assert_eq!(check_patterns(&["billing/".to_owned()]), Ok(()));
        assert_eq!(
            why_from_change(Some(&change(&["calc.py"], 1)), &bad, &[]),
            [WhyNotSmall::BadSensitivePattern {
                pattern: "a[".into()
            }]
        );
    }

    #[test]
    fn a_reason_names_a_few_files_and_says_how_many_there_were() {
        let many: Vec<String> = (0..8).map(|n| format!(".github/{n}.yml")).collect();
        let files: Vec<&str> = many.iter().map(String::as_str).collect();
        let why = why_from_change(Some(&change(&files, 8)), &[], &[]);
        let [WhyNotSmall::SensitiveFiles { files, total }] = &why[..] else {
            panic!("{why:?}")
        };
        assert_eq!(files.len(), SHOWN_FILES);
        assert_eq!(*total, 8);
    }

    #[test]
    fn the_line_limit_is_a_limit_and_what_git_will_not_count_is_not_a_small_number() {
        assert!(
            why_from_change(Some(&change(&["a.py"], MAX_CHANGED_LINES - 1)), &[], &[]).is_empty()
        );
        assert_eq!(
            why_from_change(Some(&change(&["a.py"], MAX_CHANGED_LINES)), &[], &[]),
            [WhyNotSmall::TooManyLines {
                lines: 400,
                limit: 400
            }]
        );
        let counted_nothing = ChangeSize {
            files: vec!["big.py".into()],
            lines: 0,
            without_lines: vec!["big.py".into()],
        };
        assert_eq!(
            why_from_change(Some(&counted_nothing), &[], &[]),
            [WhyNotSmall::FilesWithoutLines {
                files: vec!["big.py".into()],
                total: 1
            }]
        );
        assert_eq!(
            why_from_change(None, &[], &[]),
            [WhyNotSmall::DiffUnreadable]
        );
    }

    #[test]
    fn an_issue_counts_only_when_it_is_known_to_be_from_the_projects_own_account() {
        let from = |author: &str| json!({ "number": 7, "author": author });
        assert_eq!(why_from_issue(None, None), None, "a person typed it");
        assert_eq!(why_from_issue(None, Some("alice")), None);
        assert_eq!(why_from_issue(Some(&from("alice")), Some("alice")), None);
        assert_eq!(why_from_issue(Some(&from("Alice")), Some("alice")), None);
        assert_eq!(
            why_from_issue(Some(&from("mallory")), Some("alice")),
            Some(WhyNotSmall::IssueFromSomeoneElse {
                author: "mallory".into()
            })
        );
        assert_eq!(
            why_from_issue(Some(&from("")), Some("alice")),
            Some(WhyNotSmall::IssueFromUnknownAuthor)
        );
        assert_eq!(
            why_from_issue(Some(&json!({ "number": 7 })), Some("alice")),
            Some(WhyNotSmall::IssueFromUnknownAuthor)
        );
        assert_eq!(
            why_from_issue(Some(&from("alice")), None),
            Some(WhyNotSmall::IssueWithNoAccount)
        );
        assert_eq!(
            why_from_issue(Some(&from("alice")), Some(" ")),
            Some(WhyNotSmall::IssueWithNoAccount)
        );
    }

    fn agent(id: &str) -> StepSpec {
        StepSpec {
            id: id.into(),
            model: "fake".into(),
            effort: None,
            tools: ToolProfile::ReadOnly,
            prompt: "p".into(),
            max_turns: None,
            max_budget_usd: None,
            paths: vec![],
            role: StepRole::Reviewer,
            when: Some(keel_core::StepCondition::NotSmall),
            risks: vec![],
            model_when_risky: None,
        }
    }

    #[test]
    fn only_steps_with_a_condition_count_and_members_of_a_group_do_too() {
        let mut plain = agent("plain");
        plain.when = None;
        let steps = [
            WorkflowStep::Agent(agent("alone")),
            WorkflowStep::Agent(plain.clone()),
            WorkflowStep::Parallel {
                id: "group".into(),
                steps: vec![agent("member"), plain],
            },
        ];
        let ids = conditional_step_ids(&steps);
        assert!(ids.contains("alone") && ids.contains("member"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn a_check_that_passed_skipping_tests_is_not_enough_for_a_small_change() {
        let ran = |checks: &[(bool, Option<u32>)]| CheckFact::Ran {
            checks: checks
                .iter()
                .map(|&(passed, skipped)| verification::RanCheck {
                    command: "cargo test".into(),
                    passed,
                    skipped,
                })
                .collect(),
        };
        assert_eq!(
            why_from_fact(&ran(&[(true, Some(385))]), 1),
            Some(WhyNotSmall::CheckSkippedTests { count: 385 })
        );
        // All of them count, whichever check skipped them.
        assert_eq!(
            why_from_fact(&ran(&[(true, Some(0)), (true, Some(2))]), 2),
            Some(WhyNotSmall::CheckSkippedTests { count: 2 })
        );
        // Not passing comes first: it proves nothing at all.
        assert_eq!(
            why_from_fact(&ran(&[(false, Some(3))]), 1),
            Some(WhyNotSmall::CheckNotPassed)
        );
        // None skipped is enough. So is not knowing, as it always was: a project's own tools
        // decide what its check says.
        assert_eq!(why_from_fact(&ran(&[(true, Some(0))]), 1), None);
        assert_eq!(why_from_fact(&ran(&[(true, None)]), 1), None);
        let on_record = Decision::NotSmall(vec![WhyNotSmall::CheckSkippedTests { count: 385 }])
            .on_record(0)
            .unwrap();
        assert_eq!(
            on_record["because"][0],
            serde_json::json!({ "code": "checkSkippedTests", "count": 385 })
        );
    }

    #[test]
    fn the_record_says_what_was_decided_and_nothing_for_what_was_already_said() {
        let small = Decision::Small { lines: 3, files: 1 }.on_record(2).unwrap();
        assert_eq!(small["small"], true);
        assert_eq!(small["lines"], 3);
        assert_eq!(small["round"], 2);
        let up = Decision::NotSmall(vec![WhyNotSmall::NoCheck])
            .on_record(0)
            .unwrap();
        assert_eq!(up["small"], false);
        assert_eq!(up["because"][0]["code"], "noCheck");
        assert_eq!(Decision::StaysUp.on_record(1), None);
    }

    #[test]
    fn a_part_of_a_change_across_projects_is_never_small() {
        let why = |part| {
            let facts = Facts {
                steps: &[],
                attempts: &[],
                round: 0,
                issue: None,
                account: None,
                started_by: None,
                sensitive: &[],
                commands: &[],
                change: Some(change(&["src/visita.mts"], 3)),
                part_of_joint_change: part,
            };
            match decide(&facts) {
                Decision::NotSmall(why) => why,
                other => panic!("{other:?}"),
            }
        };
        assert!(why(true).contains(&WhyNotSmall::PartOfJointChange));
        assert!(!why(false).contains(&WhyNotSmall::PartOfJointChange));
    }
}
