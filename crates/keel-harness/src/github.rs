//! GitHub through the user's own `gh`. Keel has no login of its own and stores no token: it asks
//! `gh` for one when it's about to use it and hands it only to that one process.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use keel_core::{CiState, PrState};
use serde::{Deserialize, Serialize};

use crate::command::clean_env;
use crate::process::{SIGKILL, signal_group};

/// How long a look at a pull request gets. It's asked again later anyway.
const LOOK_LIMIT: Duration = Duration::from_secs(30);
/// How a `gh` that went past its limit starts saying so.
const TOOK_TOO_LONG: &str = "gh took longer than";
/// How long `gh` gets to say which accounts it has. It checks each one with GitHub, and whoever
/// asked is waiting.
pub const ACCOUNTS_LIMIT: Duration = LOOK_LIMIT;
/// What Keel reads of a pull request. Its newest commits come oldest first, each with how many
/// parents it has, so that Keel can tell what was added after it delivered. They're asked under a
/// name of their own, since the checks ask for the last commit alone.
const PR_FIELDS: &str = "state isDraft mergedAt closedAt updatedAt headRefOid newest:commits(last:100){nodes{commit{oid parents(first:1){totalCount}}}}";
/// How many of a pull request's newest commits one answer brings.
pub const MOST_COMMITS_READ: usize = 100;
/// Its head commit's CI: GitHub's own word on it, and its checks, each with the workflow and the
/// event it ran for. The word is asked apart: asked next to the checks, the same field also counts
/// runs of a check that later ones replaced, and says FAILURE where GitHub's pull request says it
/// passed.
const CI_FIELDS: &str = "commits(last:1){nodes{commit{verdict:statusCheckRollup{state} checks:statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{name status conclusion startedAt completedAt checkSuite{workflowRun{event workflow{name}}}} ... on StatusContext{context state createdAt}}}}}}}";

/// What a pull request asks for while Keel follows it: whether it conflicts, its review threads,
/// and the latest word of each reviewer who decided something.
const FEEDBACK_FIELDS: &str = "state headRefOid baseRefName mergeable reviewThreads(last:50){nodes{isResolved isOutdated path line comments(last:20){nodes{id body createdAt authorAssociation author{login __typename}}}}} latestOpinionatedReviews(last:30){nodes{id state body submittedAt authorAssociation author{login __typename}}}";
/// And the checks of its head commit, asked apart, since reading them takes more of a token.
const FEEDBACK_CHECKS: &str = "commits(last:1){nodes{commit{oid checks:statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{databaseId name status conclusion startedAt completedAt checkSuite{app{slug} workflowRun{event workflow{name}}}} ... on StatusContext{context state createdAt}}}}}}}";
/// Who GitHub says has to do with the repo. Not all of them can write to it, which `can_write`
/// asks, and nobody else is even asked about: anyone can comment on a public pull request.
const OF_THE_REPO: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// A token that can't end up in a log by accident.
pub struct Token(String);

impl Token {
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Removes the token from text that's about to be stored or shown.
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.0, "<token>")
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<hidden>)")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub login: String,
    pub active: bool,
    pub ok: bool,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub default_branch: String,
    pub permission: String,
    pub private: bool,
    pub default_branch_protected: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub url: String,
    pub body: String,
}

/// What a look at a repo's issues found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IssueLook {
    /// Open with the label, newest first, at most `MOST_ISSUES`.
    pub open: Vec<Issue>,
    /// Whether the repo has a label by that name at all.
    pub label_exists: bool,
    /// Of the issues asked about, the ones GitHub says are closed. One it couldn't say about isn't.
    pub closed: Vec<i64>,
}

/// How many open issues with the label a look brings, and how many others it asks about.
pub const MOST_ISSUES: usize = 100;

/// Where a pull request stands. `at` is when it was merged or closed, or when it last changed if
/// neither has happened.
#[derive(Debug, Clone, PartialEq)]
pub struct PrStatus {
    pub state: PrState,
    pub at: Option<String>,
    /// The commit its branch is at. Someone can push to it after Keel opened it.
    pub head: Option<String>,
    /// What its CI says about that commit. None when the answer didn't say.
    pub ci: Option<PrChecks>,
    /// Its newest commits. None when the answer didn't say.
    pub commits: Option<PrCommits>,
}

/// The newest commits of a pull request, oldest first, at most `MOST_COMMITS_READ` of them.
#[derive(Debug, Clone, PartialEq)]
pub struct PrCommits(pub Vec<PrCommit>);

#[derive(Debug, Clone, PartialEq)]
pub struct PrCommit {
    pub sha: String,
    /// It has more than one parent, as a merge of the base branch into it does, and GitHub's
    /// "Update branch". It brings in work from elsewhere, and isn't a fix to this.
    pub merge: bool,
}

impl PrCommits {
    /// How many commits were added after `delivered`, not counting merges, on a branch that is at
    /// `head`. None when `delivered` isn't among them, so nothing says where to count from: more
    /// were added than one answer brings, or the branch was rewritten and that commit is gone.
    /// None too when they don't end at `head`: GitHub lists only so many commits of a pull
    /// request, and past that the newest are missing, so the count would come out low.
    pub fn added_after(&self, delivered: &str, head: &str) -> Option<i64> {
        if self.0.last()?.sha != head {
            return None;
        }
        let at = self.0.iter().rposition(|commit| commit.sha == delivered)?;
        Some(self.0[at + 1..].iter().filter(|c| !c.merge).count() as i64)
    }
}

/// A repo decides how many checks fail at once, and a person reads their names.
pub const MOST_FAILED_NAMED: usize = 10;

/// What a pull request's CI says, and the checks that failed, by name, when some did.
#[derive(Debug, Clone, PartialEq)]
pub struct PrChecks {
    pub state: CiState,
    pub failed: Vec<String>,
}

/// What a pull request asks for, as it is right now.
#[derive(Debug, Clone, PartialEq)]
pub struct PrFeedback {
    /// The commit its branch is at, which the checks are about.
    pub head: String,
    /// The branch it would be merged into.
    pub base: String,
    pub conflicts: bool,
    /// Something of its CI hasn't finished, so what failed may not be all of it.
    pub checks_pending: bool,
    pub failed: Vec<FailedCheck>,
    /// The review threads nobody has settled and the reviews that ask for changes, by people who
    /// have to do with the repo. Whether each can write to it is asked apart.
    pub threads: Vec<PrThread>,
}

/// An open review thread, or a review that asks for changes: what each person said in it.
#[derive(Debug, Clone, PartialEq)]
pub struct PrThread {
    pub file: Option<String>,
    pub line: Option<u64>,
    pub said: Vec<PrSaid>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrSaid {
    pub id: String,
    pub author: String,
    pub text: String,
    /// When it was said, as GitHub writes it.
    pub at: String,
}

impl PrFeedback {
    /// Everyone who said something in it, each once.
    pub fn authors(&self) -> Vec<&str> {
        let mut authors: Vec<&str> = Vec::new();
        for said in self.threads.iter().flat_map(|thread| &thread.said) {
            if !authors.contains(&said.author.as_str()) {
                authors.push(&said.author);
            }
        }
        authors
    }

    /// What those who `writes` is true for asked for, oldest first. Nobody else's words are in it.
    pub fn comments(&self, writes: impl Fn(&str) -> bool) -> Vec<PrComment> {
        let mut comments: Vec<PrComment> = self
            .threads
            .iter()
            .filter_map(|thread| {
                let said: Vec<&PrSaid> = thread.said.iter().filter(|s| writes(&s.author)).collect();
                let mut authors: Vec<String> = Vec::new();
                for one in &said {
                    if !authors.contains(&one.author) {
                        authors.push(one.author.clone());
                    }
                }
                let text = said
                    .iter()
                    .map(|one| match authors.len() {
                        1 => one.text.clone(),
                        _ => format!("{}: {}", one.author, one.text),
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Some(PrComment {
                    at: said.last()?.at.clone(),
                    ids: said.iter().map(|one| one.id.clone()).collect(),
                    file: thread.file.clone(),
                    line: thread.line,
                    authors,
                    text,
                })
            })
            .collect();
        comments.sort_by(|a, b| a.at.cmp(&b.at));
        comments
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedCheck {
    pub name: String,
    /// The job it ran as, when GitHub Actions ran it. Only those have a log Keel can read.
    pub job: Option<u64>,
}

/// What people who can write to the repo asked for in one thread or review.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrComment {
    /// Of everything said in it. A thread someone adds to is something else to answer, and one
    /// someone takes a word out of isn't.
    pub ids: Vec<String>,
    /// Who wrote what's in `text`, each once.
    pub authors: Vec<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub text: String,
    /// When its newest word was said.
    pub at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GhError {
    pub kind: &'static str,
    pub detail: String,
}

/// GitHub says this when a token asks too much too fast, often with a 403 like a missing permission.
const RATE_LIMITED: [&str; 3] = ["rate limit", "http 429", "abuse detection"];

fn rate_limited(lower: &str) -> bool {
    RATE_LIMITED.iter().any(|words| lower.contains(words))
}

/// What a failed push or `gh` call means, so the person gets something they can act on. Too many
/// questions is none of these, and GitHub's own words say it best.
pub fn classify_error(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if rate_limited(&lower) {
        "other"
    } else if has(&[
        "refusing to allow",
        "without `workflow` scope",
        "workflow scope",
    ]) {
        "workflowScope"
    } else if has(&["saml", "sso"]) {
        "sso"
    } else if has(&["must be signed", "commit signature", "signed commits"]) {
        "signedCommits"
    } else if has(&[
        "gh006",
        "gh013",
        "protected branch",
        "rule violation",
        "repository rule",
    ]) {
        "repoRules"
    } else if has(&[
        "authentication failed",
        "could not read username",
        "gh auth login",
        "http 401",
        "bad credentials",
        "not logged",
    ]) {
        "notSignedIn"
    } else if has(&[
        "permission to",
        "permission denied",
        "http 403",
        "must have",
        "write access",
        "repository not found",
        "could not resolve to a repository",
        "http 404",
    ]) {
        "noPermission"
    } else {
        "other"
    }
}

/// `owner/name`, if this remote is on GitHub.
pub fn github_repo(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty() && parts.next().is_none())
        .then(|| format!("{owner}/{name}"))
}

/// `owner/name` and the number of a pull request, from its URL. It's the URL that says which repo,
/// since a project's folder can point at another one by now, where that number means something else.
pub fn pull_request_of(url: &str) -> Option<(String, i64)> {
    numbered(url, "pull")
}

/// `owner/name` and the number of an issue, from its URL. A card keeps an issue as it was copied,
/// with no other record of the repo it came from.
pub fn issue_in(url: &str) -> Option<(String, i64)> {
    numbered(url, "issues")
}

fn numbered(url: &str, kind: &str) -> Option<(String, i64)> {
    let path = url.trim().strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let (owner, name, found, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if owner.is_empty() || name.is_empty() || found != kind || parts.next().is_some() {
        return None;
    }
    let number = number.parse().ok().filter(|number| *number > 0)?;
    Some((format!("{owner}/{name}"), number))
}

#[derive(Debug, Clone)]
pub struct Gh {
    pub bin: PathBuf,
    pub path_env: String,
}

impl Gh {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .env_clear()
            .envs(clean_env(&self.path_env))
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        command
    }

    fn run(&self, token: Option<&Token>, args: &[&str]) -> Result<String, GhError> {
        let mut command = self.command();
        command.args(args);
        if let Some(token) = token {
            command.env("GH_TOKEN", token.expose());
        }
        let output = command.output().map_err(|e| GhError {
            kind: "other",
            detail: e.to_string(),
        })?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned());
        }
        let raw = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = token.map_or_else(|| raw.clone(), |t| t.redact(&raw));
        Err(GhError {
            kind: classify_error(&detail),
            detail,
        })
    }

    /// The accounts `gh` knows, giving up at the limit. Reads no token.
    pub fn accounts(&self, limit: Duration) -> Result<Vec<Account>, GhError> {
        // `gh auth status` exits non-zero if any account is in trouble, and still prints all of them.
        let (_, out, _) =
            self.run_within_answer(None, &["auth", "status", "--json", "hosts"], limit)?;
        let value: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        let accounts = value
            .pointer("/hosts/github.com")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(accounts
            .iter()
            .filter_map(|account| {
                Some(Account {
                    login: account.get("login")?.as_str()?.to_owned(),
                    active: account
                        .get("active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    ok: account.get("state").and_then(|v| v.as_str()) == Some("success"),
                    scopes: account
                        .get("scopes")
                        .and_then(|v| v.as_str())
                        .map(|s| {
                            s.split(',')
                                .map(|x| x.trim().to_owned())
                                .filter(|x| !x.is_empty())
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Asks `gh` for an account's token, without changing which account is active.
    pub fn token(&self, login: &str) -> Result<Token, GhError> {
        self.run(None, &["auth", "token", "--user", login])
            .map(|t| Token(t.trim().to_owned()))
    }

    pub fn repo_info(&self, token: &Token, repo: &str) -> Result<RepoInfo, GhError> {
        let text = self.run(
            Some(token),
            &[
                "repo",
                "view",
                repo,
                "--json",
                "defaultBranchRef,viewerPermission,isPrivate",
            ],
        )?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| GhError {
            kind: "other",
            detail: e.to_string(),
        })?;
        let default_branch = value
            .pointer("/defaultBranchRef/name")
            .and_then(|v| v.as_str())
            .unwrap_or("main")
            .to_owned();
        let endpoint = format!("repos/{repo}/branches/{default_branch}");
        let protected = self
            .run(Some(token), &["api", &endpoint, "--jq", ".protected"])
            .ok()
            .map(|t| t.trim() == "true");
        Ok(RepoInfo {
            default_branch,
            permission: value
                .get("viewerPermission")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            private: value
                .get("isPrivate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            default_branch_protected: protected,
        })
    }

    pub fn issues(&self, token: &Token, repo: &str) -> Result<Vec<Issue>, GhError> {
        let text = self.run(
            Some(token),
            &[
                "issue",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--limit",
                "50",
                "--json",
                "number,title,author,url,body",
            ],
        )?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| GhError {
            kind: "other",
            detail: e.to_string(),
        })?;
        Ok(value
            .as_array()
            .map(|issues| issues.iter().filter_map(parse_issue).collect())
            .unwrap_or_default())
    }

    /// The open issues with `label`, whether the repo has that label, and which of `numbers` were
    /// closed, in one GraphQL question that only reads. A number GitHub can't find, such as one
    /// deleted, spoils only its own part of the answer.
    pub fn look_at_issues(
        &self,
        token: &Token,
        repo: &str,
        label: &str,
        numbers: &[i64],
    ) -> Result<IssueLook, GhError> {
        let unexpected = |detail: String| GhError {
            kind: "other",
            detail,
        };
        let (owner, name) = owner_and_name(repo)?;
        let asked: Vec<i64> = numbers
            .iter()
            .copied()
            .filter(|n| *n > 0)
            .take(MOST_ISSUES)
            .collect();
        let states: String = asked
            .iter()
            .map(|n| format!(" i{n}:issue(number:{n}){{state}}"))
            .collect();
        let query = format!(
            "query=query($owner:String!,$name:String!,$label:String!){{repository(owner:$owner,name:$name){{label(name:$label){{name}} open:issues(labels:[$label],states:OPEN,first:{MOST_ISSUES},orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{number title url body author{{login}}}}}}{states}}}}}"
        );
        let (_, out, err) = self.run_within_answer(
            Some(token),
            &[
                "api",
                "graphql",
                "-f",
                &query,
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-f",
                &format!("label={label}"),
            ],
            LOOK_LIMIT,
        )?;
        // GitHub answers what it could next to what it couldn't, and `gh` then fails.
        let repository = serde_json::from_str::<serde_json::Value>(&out)
            .ok()
            .and_then(|value| value.pointer("/data/repository").cloned())
            .filter(|repository| repository.is_object());
        let Some(repository) = repository else {
            let detail = token.redact(&err);
            return Err(if detail.is_empty() {
                unexpected(format!("unexpected answer about the issues of {repo}"))
            } else {
                GhError {
                    kind: classify_error(&detail),
                    detail,
                }
            });
        };
        let open = repository
            .pointer("/open/nodes")
            .and_then(|nodes| nodes.as_array())
            .map(|nodes| nodes.iter().filter_map(parse_issue).collect())
            .unwrap_or_default();
        let closed = asked
            .into_iter()
            .filter(|n| {
                repository
                    .pointer(&format!("/i{n}/state"))
                    .and_then(|state| state.as_str())
                    == Some("CLOSED")
            })
            .collect();
        Ok(IssueLook {
            open,
            label_exists: repository.get("label").is_some_and(|l| l.is_object()),
            closed,
        })
    }

    /// Writes a comment on an issue. Keel's only words on GitHub besides its pull requests.
    pub fn comment_on_issue(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
        body: &str,
    ) -> Result<(), GhError> {
        let (owner, name) = owner_and_name(repo)?;
        self.run_within(
            token,
            &[
                "api",
                &format!("repos/{owner}/{name}/issues/{number}/comments"),
                "--method",
                "POST",
                "-f",
                &format!("body={body}"),
            ],
            LOOK_LIMIT,
        )?;
        Ok(())
    }

    /// `run`, giving up at the limit along with anything it started. For what runs on its own in the
    /// background, where nobody would notice it hanging and everything after it would wait.
    fn run_within(&self, token: &Token, args: &[&str], limit: Duration) -> Result<String, GhError> {
        let (succeeded, out, err) = self.run_within_answer(Some(token), args, limit)?;
        if succeeded {
            return Ok(out);
        }
        let detail = token.redact(&err);
        Err(GhError {
            kind: classify_error(&detail),
            detail,
        })
    }

    /// Like `run_within`, but a `gh` that ran to the end gives what it printed either way: whether
    /// it succeeded, its output, and what it said on stderr, without the token.
    fn run_within_answer(
        &self,
        token: Option<&Token>,
        args: &[&str],
        limit: Duration,
    ) -> Result<(bool, String, String), GhError> {
        let mut command = self.command();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(token) = token {
            command.env("GH_TOKEN", token.expose());
        }
        let failed = |detail: String| GhError {
            kind: "other",
            detail,
        };
        let mut child = command.spawn().map_err(|e| failed(e.to_string()))?;
        let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                bytes
            })
        };
        let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
        let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
        let deadline = Instant::now() + limit;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    signal_group(child.id() as i32, SIGKILL);
                    let _ = child.wait();
                    return Err(failed(format!(
                        "{TOOK_TOO_LONG} {} seconds",
                        limit.as_secs()
                    )));
                }
                Err(e) => return Err(failed(e.to_string())),
            }
        };
        let text = |reader: std::thread::JoinHandle<Vec<u8>>| {
            String::from_utf8_lossy(&reader.join().unwrap_or_default())
                .trim()
                .to_owned()
        };
        let (out, err) = (text(stdout), text(stderr));
        let err = token.map_or_else(|| err.clone(), |t| t.redact(&err));
        Ok((status.success(), out, err))
    }

    /// What became of a pull request, and what its CI says, in one question. Only reads: after
    /// opening one, Keel writes nothing to it.
    pub fn pr_status(&self, token: &Token, repo: &str, number: i64) -> Result<PrStatus, GhError> {
        self.pr_status_within(token, repo, number, LOOK_LIMIT)
    }

    /// Reading the checks takes more of a token than the rest. One that can't still says what
    /// became of the pull request, with its CI left unknown. One that hangs would only hang again.
    fn pr_status_within(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
        limit: Duration,
    ) -> Result<PrStatus, GhError> {
        match self.ask_about_pr(token, repo, number, limit, true) {
            Err(error) if about_the_checks(&error) => {
                self.ask_about_pr(token, repo, number, limit, false)
            }
            answer => answer,
        }
    }

    /// One GraphQL question, which only reads. `gh pr view` doesn't say what GitHub makes of the
    /// checks, only what each says.
    fn ask_about_pr(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
        limit: Duration,
        with_checks: bool,
    ) -> Result<PrStatus, GhError> {
        let unexpected = |detail: String| GhError {
            kind: "other",
            detail,
        };
        let (owner, name) = owner_and_name(repo)?;
        let fields = if with_checks {
            format!("{PR_FIELDS} {CI_FIELDS}")
        } else {
            PR_FIELDS.to_owned()
        };
        let query = format!(
            "query=query($owner:String!,$name:String!,$number:Int!){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{fields}}}}}}}"
        );
        let text = self.run_within(
            token,
            &[
                "api",
                "graphql",
                "-f",
                &query,
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-F",
                &format!("number={number}"),
            ],
            limit,
        )?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| unexpected(e.to_string()))?;
        value
            .pointer("/data/repository/pullRequest")
            .and_then(parse_pr_status)
            .ok_or_else(|| unexpected(format!("unexpected answer about pull request {number}")))
    }

    /// What a pull request asks for while Keel follows it. Only reads. A token that can't read the
    /// checks still says the rest, with nothing known of its CI.
    pub fn pr_feedback(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
    ) -> Result<PrFeedback, GhError> {
        match self.ask_for_feedback(token, repo, number, true) {
            Err(error) if about_the_checks(&error) => {
                self.ask_for_feedback(token, repo, number, false)
            }
            answer => answer,
        }
    }

    fn ask_for_feedback(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
        with_checks: bool,
    ) -> Result<PrFeedback, GhError> {
        let (owner, name) = owner_and_name(repo)?;
        let fields = if with_checks {
            format!("{FEEDBACK_FIELDS} {FEEDBACK_CHECKS}")
        } else {
            FEEDBACK_FIELDS.to_owned()
        };
        let query = format!(
            "query=query($owner:String!,$name:String!,$number:Int!){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{fields}}}}}}}"
        );
        let text = self.run_within(
            token,
            &[
                "api",
                "graphql",
                "-f",
                &query,
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-F",
                &format!("number={number}"),
            ],
            LOOK_LIMIT,
        )?;
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .as_ref()
            .and_then(|value| value.pointer("/data/repository/pullRequest"))
            .and_then(parse_feedback)
            .ok_or_else(|| GhError {
                kind: "other",
                detail: format!("unexpected answer about pull request {number}"),
            })
    }

    /// Whether someone can write to the repo. A member of its organization, or a collaborator on
    /// it, may only be able to read. Only a token that can write may ask, and anything but a yes
    /// is a no.
    pub fn can_write(&self, token: &Token, repo: &str, login: &str) -> bool {
        let plain = !login.is_empty()
            && login
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let Ok((owner, name)) = owner_and_name(repo) else {
            return false;
        };
        plain
            && self
                .run_within(
                    token,
                    &[
                        "api",
                        &format!("repos/{owner}/{name}/collaborators/{login}/permission"),
                        "--jq",
                        ".permission",
                    ],
                    LOOK_LIMIT,
                )
                .is_ok_and(|permission| matches!(permission.trim(), "admin" | "write"))
    }

    /// What a GitHub Actions job printed. A job that never ran, such as one its account couldn't
    /// pay for, has none, and that's an error.
    pub fn job_log(&self, token: &Token, repo: &str, job: u64) -> Result<String, GhError> {
        let (owner, name) = owner_and_name(repo)?;
        self.run_within(
            token,
            &[
                "api",
                &format!("repos/{owner}/{name}/actions/jobs/{job}/logs"),
            ],
            LOOK_LIMIT,
        )
    }

    /// Opens a draft and returns its URL. Keel doesn't merge, and this module has no way to.
    pub fn create_draft_pr(
        &self,
        token: &Token,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body_file: &Path,
    ) -> Result<String, GhError> {
        let body_file = body_file.to_string_lossy();
        self.run(
            Some(token),
            &[
                "pr",
                "create",
                "--repo",
                repo,
                "--draft",
                "--head",
                head,
                "--base",
                base,
                "--title",
                title,
                "--body-file",
                &body_file,
            ],
        )
        .map(|out| {
            out.lines()
                .rfind(|l| l.starts_with("http"))
                .unwrap_or(out.as_str())
                .trim()
                .to_owned()
        })
    }
}

/// `owner` and `name` of `owner/name`, or an error for anything else.
fn owner_and_name(repo: &str) -> Result<(&str, &str), GhError> {
    repo.split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| GhError {
            kind: "other",
            detail: format!("not a repo on GitHub: {repo}"),
        })
}

fn parse_issue(value: &serde_json::Value) -> Option<Issue> {
    Some(Issue {
        number: value.get("number")?.as_i64()?,
        title: value.get("title")?.as_str()?.to_owned(),
        author: value
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        url: value
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        body: value
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
    })
}

fn parse_pr_status(value: &serde_json::Value) -> Option<PrStatus> {
    let is_draft = value.get("isDraft").and_then(|v| v.as_bool());
    let (state, when) = match value.get("state")?.as_str()? {
        "MERGED" => (PrState::Merged, "mergedAt"),
        "CLOSED" => (PrState::Closed, "closedAt"),
        "OPEN" if is_draft == Some(true) => (PrState::Draft, "updatedAt"),
        "OPEN" => (PrState::Open, "updatedAt"),
        _ => return None,
    };
    let at = value
        .get(when)
        .and_then(|v| v.as_str())
        .filter(|at| !at.is_empty())
        .map(str::to_owned);
    let head = value
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .filter(|head| !head.is_empty())
        .map(str::to_owned);
    Some(PrStatus {
        state,
        at,
        head,
        ci: value.pointer("/commits/nodes/0/commit").map(parse_checks),
        commits: value.pointer("/newest/nodes").and_then(parse_commits),
    })
}

/// None if it doesn't say where the branch is: then nothing it says can be told apart from what
/// was already dealt with.
fn parse_feedback(value: &serde_json::Value) -> Option<PrFeedback> {
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let head = text(value, "headRefOid")?;
    let commit = value.pointer("/commits/nodes/0/commit");
    // An answer about the checks of another commit says nothing of this one.
    let checks = commit
        .filter(|commit| text(commit, "oid").as_deref() == Some(head.as_str()))
        .map(latest_checks)
        .unwrap_or_default();
    let mut failed: Vec<FailedCheck> = Vec::new();
    let mut checks_pending = false;
    for check in &checks {
        match outcome_of(check) {
            Outcome::Passed => {}
            Outcome::Pending => checks_pending = true,
            // One that was cancelled, as the rest of a matrix is when one fails, or that waits for
            // a person, says nothing of the work.
            Outcome::Failed
                if matches!(
                    check_text(check, "conclusion").as_str(),
                    "CANCELLED" | "ACTION_REQUIRED"
                ) => {}
            Outcome::Failed => failed.push(FailedCheck {
                name: check_name(check),
                job: check
                    .get("databaseId")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|_| {
                        check
                            .pointer("/checkSuite/app/slug")
                            .and_then(|s| s.as_str())
                            == Some("github-actions")
                    }),
            }),
        }
    }
    let nodes = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let said_by = |comment: &serde_json::Value| {
        let writes = text(comment, "authorAssociation")
            .is_some_and(|association| OF_THE_REPO.contains(&association.as_str()));
        let person = comment
            .pointer("/author/__typename")
            .and_then(|v| v.as_str())
            == Some("User");
        let login = comment
            .get("author")
            .and_then(|author| text(author, "login"));
        login.filter(|_| writes && person)
    };
    let mut threads: Vec<PrThread> = Vec::new();
    for thread in nodes("/reviewThreads/nodes") {
        if thread.get("isResolved").and_then(|v| v.as_bool()) != Some(false) {
            continue;
        }
        let said: Vec<PrSaid> = thread
            .pointer("/comments/nodes")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|comment| {
                Some(PrSaid {
                    id: text(comment, "id")?,
                    author: said_by(comment)?,
                    text: text(comment, "body")?,
                    at: text(comment, "createdAt")?,
                })
            })
            .collect();
        if !said.is_empty() {
            threads.push(PrThread {
                file: text(&thread, "path"),
                line: thread.get("line").and_then(serde_json::Value::as_u64),
                said,
            });
        }
    }
    for review in nodes("/latestOpinionatedReviews/nodes") {
        if text(&review, "state").as_deref() != Some("CHANGES_REQUESTED") {
            continue;
        }
        // With no words of its own, what it asks for is in its threads.
        if let (Some(id), Some(author), Some(body), Some(at)) = (
            text(&review, "id"),
            said_by(&review),
            text(&review, "body"),
            text(&review, "submittedAt"),
        ) {
            threads.push(PrThread {
                file: None,
                line: None,
                said: vec![PrSaid {
                    id,
                    author,
                    text: body,
                    at,
                }],
            });
        }
    }
    Some(PrFeedback {
        head,
        base: text(value, "baseRefName").unwrap_or_default(),
        conflicts: text(value, "mergeable").as_deref() == Some("CONFLICTING"),
        checks_pending,
        failed,
        threads,
    })
}

/// None unless every commit says which it is and how many parents it has: a count from half a
/// list would be a wrong number.
fn parse_commits(nodes: &serde_json::Value) -> Option<PrCommits> {
    nodes
        .as_array()?
        .iter()
        .map(|node| {
            let sha = node.pointer("/commit/oid")?.as_str()?;
            let parents = node.pointer("/commit/parents/totalCount")?.as_u64()?;
            Some(PrCommit {
                sha: sha.to_owned(),
                merge: parents > 1,
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(PrCommits)
}

/// Whether a failed question can be about the checks alone: GitHub saying the token can't reach
/// something. Not a token that's signed out, a pull request that isn't there, too many questions, a
/// GitHub that can't be reached or a `gh` that hangs, which asking again without them won't fix.
fn about_the_checks(error: &GhError) -> bool {
    let detail = error.detail.to_ascii_lowercase();
    let says = |words: &[&str]| words.iter().any(|word| detail.contains(word));
    !rate_limited(&detail)
        && (says(&[
            "statuscheckrollup",
            "checksuite",
            "checkrun",
            "not accessible",
        ]) || (error.kind == "noPermission"
            && !says(&["not found", "404", "could not resolve"])))
}

/// When a check started, or ended. GitHub's zero time, for one that hasn't, is none.
fn check_time(
    check: &serde_json::Value,
    key: &str,
) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    use chrono::Datelike;
    let text = check.get(key)?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .filter(|time| time.year() > 1)
}

/// Of the runs of one check, the newest, as `gh pr checks` counts them, however soon after the
/// others it started: a push's and a pull request's runs are two checks, since their events differ.
/// One that hasn't started is newer than any that has.
fn newest_run(all: &[serde_json::Value], runs: &[usize]) -> Vec<usize> {
    let waiting: Vec<usize> = runs
        .iter()
        .copied()
        .filter(|&at| check_time(&all[at], "startedAt").is_none())
        .collect();
    if !waiting.is_empty() {
        return waiting;
    }
    runs.iter()
        .copied()
        .max_by_key(|&at| check_time(&all[at], "startedAt"))
        .into_iter()
        .collect()
}

/// A check with the name of its workflow, the event it ran for, and when it started, next to the
/// rest. A commit status starts when it's set.
fn flat(check: &serde_json::Value) -> serde_json::Value {
    let run = check.pointer("/checkSuite/workflowRun");
    let of_run = |pointer: &str| {
        run.and_then(|run| run.pointer(pointer))
            .cloned()
            .unwrap_or_default()
    };
    let mut flat = check.clone();
    if let Some(fields) = flat.as_object_mut() {
        fields.insert("workflowName".into(), of_run("/workflow/name"));
        fields.insert("event".into(), of_run("/event"));
        if let (false, Some(created)) = (fields.contains_key("startedAt"), check.get("createdAt")) {
            fields.insert("startedAt".into(), created.clone());
        }
    }
    flat
}

/// GitHub's own word on a commit's CI is what it says: SUCCESS passed, PENDING or EXPECTED is
/// pending, FAILURE or ERROR failed, and a commit with no check at all has no CI. Each check is a
/// check run, with a status and, once it's done, a conclusion, or a commit status, with a state, and
/// they only name what failed, the way `gh pr checks` counts them: cancelled is a failure, skipped
/// and neutral are not. Of the runs of one check, only the one that says where it stands counts.
/// Only when GitHub's word is missing do the checks decide.
fn parse_checks(commit: &serde_json::Value) -> PrChecks {
    let verdict = commit
        .pointer("/verdict/state")
        .and_then(serde_json::Value::as_str);
    let checks = latest_checks(commit);
    let mut failed: Vec<String> = Vec::new();
    let mut pending = false;
    for check in &checks {
        match outcome_of(check) {
            Outcome::Passed => {}
            Outcome::Failed => {
                let named = check_name(check);
                if failed.len() < MOST_FAILED_NAMED && !failed.contains(&named) {
                    failed.push(named);
                }
            }
            Outcome::Pending => pending = true,
        }
    }
    let state = match verdict {
        Some("SUCCESS") => CiState::Passed,
        Some("PENDING" | "EXPECTED") => CiState::Pending,
        Some("FAILURE" | "ERROR") => CiState::Failed,
        _ if !failed.is_empty() => CiState::Failed,
        _ if pending => CiState::Pending,
        _ if checks.is_empty() => CiState::NoCi,
        _ => CiState::Passed,
    };
    if state != CiState::Failed {
        failed.clear();
    }
    PrChecks { state, failed }
}

fn check_text(check: &serde_json::Value, key: &str) -> String {
    check
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn check_name(check: &serde_json::Value) -> String {
    Some(check_text(check, "name"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| check_text(check, "context"))
}

/// A commit's checks, each flat, with only the run of each one that says where it stands.
fn latest_checks(commit: &serde_json::Value) -> Vec<serde_json::Value> {
    let all: Vec<serde_json::Value> = commit
        .pointer("/checks/contexts/nodes")
        .and_then(serde_json::Value::as_array)
        .map(|nodes| nodes.iter().map(flat).collect())
        .unwrap_or_default();
    // The same job in another workflow, or for another event, is another check, and so is a commit
    // status with its name.
    let key = |check: &serde_json::Value| {
        (
            check_text(check, "__typename"),
            check_text(check, "workflowName"),
            check_text(check, "event"),
            check_name(check),
        )
    };
    let mut runs: HashMap<(String, String, String, String), Vec<usize>> = HashMap::new();
    for (at, check) in all.iter().enumerate() {
        runs.entry(key(check)).or_default().push(at);
    }
    let kept: HashSet<usize> = runs
        .values()
        .flat_map(|runs| newest_run(&all, runs))
        .collect();
    all.into_iter()
        .enumerate()
        .filter(|(at, _)| kept.contains(at))
        .map(|(_, check)| check)
        .collect()
}

enum Outcome {
    Passed,
    Failed,
    Pending,
}

fn outcome_of(check: &serde_json::Value) -> Outcome {
    let status = check_text(check, "status");
    let outcome = if !status.is_empty() && status != "COMPLETED" {
        status
    } else if check.get("conclusion").is_some() {
        check_text(check, "conclusion")
    } else {
        check_text(check, "state")
    };
    match outcome.as_str() {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => Outcome::Passed,
        "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            Outcome::Failed
        }
        _ => Outcome::Pending,
    }
}

/// The arguments that make `git` authenticate through `gh`, with nothing written to any config.
/// The token itself travels only in the process's `GH_TOKEN`.
pub fn git_credential_args(gh: &Path) -> Vec<String> {
    let path = gh.to_string_lossy().replace('\'', r"'\''");
    vec![
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        format!("credential.helper=!'{path}' auth git-credential"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_github_remotes_and_only_those() {
        for remote in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app.git",
            "https://github.com/acme/app",
            "ssh://git@github.com/acme/app.git\n",
        ] {
            assert_eq!(github_repo(remote).as_deref(), Some("acme/app"), "{remote}");
        }
        for remote in [
            "git@gitlab.com:acme/app.git",
            "https://github.com/acme",
            "/tmp/repo",
            "https://github.com.evil.com/a/b",
        ] {
            assert_eq!(github_repo(remote), None, "{remote}");
        }
    }

    #[test]
    fn only_a_yes_from_github_means_someone_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"case "$2" in *ana/*) echo write;; *root/*) echo admin;; *luis/*) echo read;; *) exit 1;; esac"#,
        );
        let token = Token("tok".into());
        let can = |login: &str| gh.can_write(&token, "acme/app", login);
        assert!(can("ana") && can("root"));
        assert!(!can("luis"), "a collaborator can be one who only reads");
        assert!(!can("stranger"), "an error isn't a yes");
        assert!(!can("../../x") && !can(""));
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(!calls.contains(".."), "{calls}");
        assert!(!gh.can_write(&token, "not a repo", "ana"));
    }

    #[test]
    fn a_token_that_cannot_read_the_checks_still_hears_what_people_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"case "$*" in
*statusCheckRollup*) echo 'Resource not accessible by personal access token (statusCheckRollup)' >&2; exit 1;;
*) echo '{"data":{"repository":{"pullRequest":{"state":"OPEN","headRefOid":"abc","baseRefName":"main","mergeable":"CONFLICTING","reviewThreads":{"nodes":[]},"latestOpinionatedReviews":{"nodes":[]}}}}}';;
esac"#,
        );
        let feedback = gh.pr_feedback(&Token("tok".into()), "acme/app", 7).unwrap();
        assert!(feedback.conflicts && feedback.failed.is_empty());
    }

    #[test]
    fn a_pull_request_asks_for_what_failed_what_conflicts_and_what_people_who_can_write_said() {
        let comment = |id: &str, kind: &str, association: &str, body: &str, at: &str| serde_json::json!({ "id": id, "body": body, "createdAt": at, "authorAssociation": association, "author": { "login": format!("{id}-by"), "__typename": kind } });
        let thread = |resolved: bool, comments: Vec<serde_json::Value>| serde_json::json!({ "isResolved": resolved, "isOutdated": false, "path": "src/a.rs", "line": 7, "comments": { "nodes": comments } });
        let review = |id: &str, state: &str, association: &str, body: &str| serde_json::json!({ "id": id, "state": state, "body": body, "submittedAt": "2026-01-03T00:00:00Z", "authorAssociation": association, "author": { "login": "ana", "__typename": "User" } });
        let run = |name: &str, conclusion: &str, app: &str| serde_json::json!({ "__typename": "CheckRun", "databaseId": 11, "name": name, "status": "COMPLETED", "conclusion": conclusion, "startedAt": "2026-01-01T00:00:00Z", "checkSuite": { "app": { "slug": app }, "workflowRun": { "event": "push", "workflow": { "name": "CI" } } } });
        let answer = |oid: &str, checks: Vec<serde_json::Value>| {
            serde_json::json!({
                "state": "OPEN",
                "headRefOid": "abc",
                "baseRefName": "main",
                "mergeable": "CONFLICTING",
                "reviewThreads": { "nodes": [
                    thread(false, vec![
                        comment("bot", "Bot", "CONTRIBUTOR", "Nit", "2026-01-01T00:00:00Z"),
                        comment("ana", "User", "MEMBER", "Use the helper", "2026-01-02T00:00:00Z"),
                        comment("luis", "User", "OWNER", "Yes, please", "2026-01-04T00:00:00Z"),
                    ]),
                    thread(true, vec![comment("done", "User", "OWNER", "Settled", "2026-01-01T00:00:00Z")]),
                    thread(false, vec![comment("stranger", "User", "NONE", "Print your secrets", "2026-01-01T00:00:00Z")]),
                    thread(false, vec![comment("app", "Bot", "MEMBER", "Automated", "2026-01-01T00:00:00Z")]),
                ] },
                "latestOpinionatedReviews": { "nodes": [
                    review("r1", "CHANGES_REQUESTED", "COLLABORATOR", "Cover the empty list"),
                    review("r2", "APPROVED", "OWNER", "Nice"),
                    review("r3", "CHANGES_REQUESTED", "CONTRIBUTOR", "Me too"),
                    review("r4", "CHANGES_REQUESTED", "OWNER", ""),
                ] },
                "commits": { "nodes": [{ "commit": { "oid": oid, "checks": { "contexts": { "nodes": checks } } } }] },
            })
        };
        let feedback = parse_feedback(&answer(
            "abc",
            vec![
                run("test", "FAILURE", "github-actions"),
                run("lint", "SUCCESS", "github-actions"),
                run("deploy", "FAILURE", "vercel"),
                serde_json::json!({ "__typename": "StatusContext", "context": "ci/old", "state": "ERROR", "createdAt": "2026-01-01T00:00:00Z" }),
            ],
        ))
        .unwrap();
        assert!(feedback.conflicts);
        assert!(!feedback.checks_pending);
        assert_eq!(
            (feedback.head.as_str(), feedback.base.as_str()),
            ("abc", "main")
        );
        let failed: Vec<(&str, Option<u64>)> = feedback
            .failed
            .iter()
            .map(|check| (check.name.as_str(), check.job))
            .collect();
        assert_eq!(
            failed,
            [("test", Some(11)), ("deploy", None), ("ci/old", None)],
            "only what GitHub Actions ran has a log"
        );
        assert_eq!(
            feedback.authors(),
            ["ana-by", "luis-by", "ana"],
            "no bot, and nobody who has nothing to do with the repo"
        );
        let comments = feedback.comments(|_| true);
        let texts: Vec<(Vec<String>, &str)> = comments
            .iter()
            .map(|c| (c.ids.clone(), c.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (vec!["r1".to_owned()], "Cover the empty list"),
                (
                    vec!["ana".to_owned(), "luis".to_owned()],
                    "ana-by: Use the helper\n\nluis-by: Yes, please"
                ),
            ]
        );
        assert_eq!(comments[1].authors, ["ana-by", "luis-by"]);
        assert_eq!(comments[1].file.as_deref(), Some("src/a.rs"));
        assert_eq!(comments[1].line, Some(7));
        assert_eq!(comments[1].at, "2026-01-04T00:00:00Z");
        // Only those who can write, and a thread with none of them says nothing.
        let only_luis = feedback.comments(|login| login == "luis-by");
        assert_eq!(only_luis.len(), 1);
        assert_eq!(only_luis[0].text, "Yes, please");
        assert!(feedback.comments(|_| false).is_empty());

        // What was cancelled when another failed, or waits for a person, isn't a failure.
        let cut = parse_feedback(&answer(
            "abc",
            vec![
                run("a", "CANCELLED", "github-actions"),
                run("b", "ACTION_REQUIRED", "github-actions"),
                run("c", "TIMED_OUT", "github-actions"),
            ],
        ))
        .unwrap();
        let names: Vec<&str> = cut.failed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c"]);
        // An answer with no checks in it says nothing of them.
        let mut without = answer("abc", vec![]);
        without.as_object_mut().unwrap().remove("commits");
        let without = parse_feedback(&without).unwrap();
        assert!(without.failed.is_empty() && !without.checks_pending);

        // Something still running, and checks of another commit.
        let mut waiting = run("test", "FAILURE", "github-actions");
        waiting["status"] = "IN_PROGRESS".into();
        assert!(
            parse_feedback(&answer("abc", vec![waiting]))
                .unwrap()
                .checks_pending
        );
        let other = parse_feedback(&answer(
            "old",
            vec![run("test", "FAILURE", "github-actions")],
        ));
        assert!(other.unwrap().failed.is_empty());
        let mut nowhere = answer("abc", vec![]);
        nowhere["headRefOid"] = serde_json::Value::Null;
        assert_eq!(parse_feedback(&nowhere), None);
    }

    #[test]
    fn explains_what_went_wrong() {
        let cases = [
            (
                "refusing to allow an OAuth App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope",
                "workflowScope",
            ),
            ("Resource protected by organization SAML enforcement", "sso"),
            (
                "remote: error: GH006: Protected branch update failed",
                "repoRules",
            ),
            (
                "Commits must have verified signatures. Commit must be signed",
                "signedCommits",
            ),
            (
                "fatal: Authentication failed for 'https://github.com/a/b.git/'",
                "notSignedIn",
            ),
            ("HTTP 401: Bad credentials", "notSignedIn"),
            (
                "remote: Permission to a/b.git denied to someone.",
                "noPermission",
            ),
            (
                "GraphQL: Could not resolve to a Repository with the name 'a/b'. (repository)",
                "noPermission",
            ),
            // Too many questions isn't a missing permission, though GitHub says 403 for both.
            (
                "HTTP 403: API rate limit exceeded for user ID 1. (https://api.github.com/graphql)",
                "other",
            ),
            (
                "HTTP 403: You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
                "other",
            ),
            ("something else entirely", "other"),
        ];
        for (text, kind) in cases {
            assert_eq!(classify_error(text), kind, "{text}");
        }
    }

    #[test]
    fn a_token_never_shows_in_debug_output_and_can_be_redacted() {
        let token = Token("gho_secret".into());
        assert_eq!(format!("{token:?}"), "Token(<hidden>)");
        assert_eq!(
            token.redact("failed with gho_secret here"),
            "failed with <token> here"
        );
    }

    #[test]
    fn credentials_are_never_in_git_arguments() {
        let args = git_credential_args(Path::new("/opt/homebrew/bin/gh"));
        assert_eq!(args[1], "credential.helper=");
        assert!(args[3].contains("auth git-credential"));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("gho_") || a.contains("token"))
        );
    }

    #[test]
    fn a_pull_requests_repo_and_number_come_from_its_url() {
        assert_eq!(
            pull_request_of("https://github.com/acme/app/pull/12\n"),
            Some(("acme/app".to_owned(), 12))
        );
        for url in [
            "https://github.com/acme/app/issues/12",
            "https://github.com/acme/app/pull/12/files",
            "https://github.com/acme/app/pull/-1",
            "https://github.com/acme/app/pull/twelve",
            "https://github.com/acme/app/pull",
            "https://github.com//app/pull/12",
            "https://github.com.evil.com/acme/app/pull/12",
            "http://github.com/acme/app/pull/12",
            "",
        ] {
            assert_eq!(pull_request_of(url), None, "{url}");
        }
    }

    #[test]
    fn an_issues_repo_and_number_come_from_its_url() {
        assert_eq!(
            issue_in("https://github.com/acme/back/issues/434"),
            Some(("acme/back".to_owned(), 434))
        );
        for url in [
            "https://github.com/acme/back/pull/434",
            "https://github.com/acme/back/issues/434#issuecomment-1",
            "https://github.com/acme/back/issues/0",
            "https://example.com/acme/back/issues/434",
            "",
        ] {
            assert_eq!(issue_in(url), None, "{url}");
        }
    }

    #[test]
    fn a_gh_that_hangs_is_given_up_on_along_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        let log = dir.path().join("calls.log");
        let script = format!(
            "#!/bin/sh\necho called >> '{}'\nsleep 30 &\nwait\n",
            log.display()
        );
        // Run once first, so the time limit measures the script and not macOS looking at it.
        crate::testing::write_script(&bin, &script).unwrap();
        let gh = Gh {
            bin,
            path_env: std::env::var("PATH").unwrap_or_default(),
        };
        let started = Instant::now();
        let error = gh
            .pr_status_within(&Token("t".into()), "a/b", 1, Duration::from_millis(100))
            .unwrap_err();
        assert!(error.detail.contains("longer than"), "{}", error.detail);
        assert!(started.elapsed() < Duration::from_secs(10));
        // One that hangs isn't asked again without the checks: it would only hang again.
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
    }

    #[test]
    fn a_gh_that_never_says_its_accounts_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        crate::testing::write_script(&bin, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
        let gh = Gh {
            bin,
            path_env: "/usr/bin:/bin".to_owned(),
        };
        let started = Instant::now();
        let error = gh.accounts(Duration::from_millis(100)).unwrap_err();
        assert!(error.detail.contains("longer than"), "{}", error.detail);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_accounts_are_read_without_a_token_even_when_gh_fails_over_one() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        let script = r#"#!/bin/sh
test -z "$GH_TOKEN" || exit 1
echo '{"hosts":{"github.com":[{"login":"alice","active":true,"state":"error","scopes":"repo"}]}}'
exit 1
"#;
        crate::testing::write_script(&bin, script).unwrap();
        let gh = Gh {
            bin,
            path_env: "/usr/bin:/bin".to_owned(),
        };
        let accounts = gh.accounts(ACCOUNTS_LIMIT).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].login, "alice");
        assert!(!accounts[0].ok);
    }

    #[test]
    fn reads_where_a_pull_request_stands() {
        let status = |value: serde_json::Value| parse_pr_status(&value);
        let at = |state, at: &str| {
            Some(PrStatus {
                state,
                at: Some(at.to_owned()),
                head: None,
                ci: None,
                commits: None,
            })
        };
        assert_eq!(
            status(
                serde_json::json!({ "state": "OPEN", "isDraft": true, "mergedAt": null, "closedAt": null, "updatedAt": "t1" })
            ),
            at(PrState::Draft, "t1")
        );
        assert_eq!(
            status(
                serde_json::json!({ "state": "OPEN", "isDraft": false, "mergedAt": null, "closedAt": null, "updatedAt": "t2" })
            ),
            at(PrState::Open, "t2")
        );
        // A merged pull request is closed too, and GitHub says so. Merged is what matters.
        assert_eq!(
            status(
                serde_json::json!({ "state": "MERGED", "isDraft": false, "mergedAt": "t3", "closedAt": "t4", "updatedAt": "t5" })
            ),
            at(PrState::Merged, "t3")
        );
        assert_eq!(
            status(
                serde_json::json!({ "state": "CLOSED", "isDraft": true, "mergedAt": null, "closedAt": "t6", "updatedAt": "t7" })
            ),
            at(PrState::Closed, "t6")
        );
        assert_eq!(
            status(serde_json::json!({ "state": "MERGED", "mergedAt": null })),
            Some(PrStatus {
                state: PrState::Merged,
                at: None,
                head: None,
                ci: None,
                commits: None,
            })
        );
        assert_eq!(
            status(serde_json::json!({ "state": "OPEN", "isDraft": true, "updatedAt": "t8", "headRefOid": "abc123" }))
                .and_then(|status| status.head),
            Some("abc123".to_owned())
        );
        assert_eq!(
            status(serde_json::json!({ "state": "SOMETHING_NEW" })),
            None
        );
        assert_eq!(status(serde_json::json!({})), None);
    }

    /// A pull request with these commits, oldest first, as the query asks for them. One named
    /// `merge-…` has two parents.
    fn with_commits(shas: &[&str]) -> serde_json::Value {
        let nodes: Vec<serde_json::Value> = shas
            .iter()
            .map(|sha| {
                let parents = if sha.starts_with("merge") { 2 } else { 1 };
                serde_json::json!({ "commit": { "oid": sha, "parents": { "totalCount": parents } } })
            })
            .collect();
        serde_json::json!({ "state": "MERGED", "newest": { "nodes": nodes } })
    }

    /// What was added after `delivered` to a branch that is at the last of these commits.
    fn added_after(delivered: &str, shas: &[&str]) -> Option<i64> {
        let head = shas.last().copied().unwrap_or_default();
        parse_pr_status(&with_commits(shas))
            .and_then(|status| status.commits)
            .and_then(|commits| commits.added_after(delivered, head))
    }

    #[test]
    fn commits_added_after_the_one_delivered_are_counted_and_merges_are_not() {
        assert_eq!(added_after("b", &["a", "b"]), Some(0), "as delivered");
        assert_eq!(added_after("b", &["a", "b", "c", "d"]), Some(2));
        // Bringing the base branch up to date fixes nothing.
        assert_eq!(added_after("b", &["a", "b", "merge-main"]), Some(0));
        assert_eq!(
            added_after("b", &["a", "b", "c", "merge-main", "d", "merge-again"]),
            Some(2)
        );
        // What came before it was Keel's own.
        assert_eq!(added_after("c", &["a", "merge-main", "c"]), Some(0));
    }

    #[test]
    fn with_the_delivered_commit_out_of_sight_the_count_is_not_known_rather_than_wrong() {
        // The branch was rewritten, so that commit is gone.
        assert_eq!(added_after("b", &["a", "x", "y"]), None);
        assert_eq!(added_after("b", &[]), None);
        // More were added than one answer brings: it has the newest, and the one delivered is
        // further back.
        let newest: Vec<String> = (0..MOST_COMMITS_READ).map(|n| format!("c{n}")).collect();
        let newest: Vec<&str> = newest.iter().map(String::as_str).collect();
        assert_eq!(added_after("b", &newest), None);
        assert_eq!(added_after("c0", &newest), Some(99));
        // GitHub lists only the first 250 commits of a pull request, so on a longer one the list
        // stops short of where the branch is, and what's missing is the newest.
        let listed = parse_pr_status(&with_commits(&["a", "b", "c"]))
            .and_then(|status| status.commits)
            .unwrap();
        assert_eq!(listed.added_after("b", "c"), Some(1));
        assert_eq!(listed.added_after("b", "newer"), None);
        // An answer that says nothing of its commits, or leaves one of them half said.
        let silent = serde_json::json!({ "state": "MERGED" });
        assert_eq!(parse_pr_status(&silent).unwrap().commits, None);
        let half = serde_json::json!({ "state": "MERGED", "newest": { "nodes": [
            { "commit": { "oid": "b", "parents": { "totalCount": 1 } } },
            { "commit": { "oid": "c" } },
        ]}});
        assert_eq!(parse_pr_status(&half).unwrap().commits, None);
    }

    /// A pull request whose head commit's CI GitHub sums up as `verdict`, with these checks, as the
    /// query asks for them.
    fn with_ci(verdict: &str, checks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "state": "OPEN", "commits": { "nodes": [{ "commit": {
            "verdict": { "state": verdict },
            "checks": { "contexts": { "nodes": checks } },
        }}]}})
    }

    fn ci_of(verdict: &str, checks: serde_json::Value) -> Option<(CiState, Vec<String>)> {
        parse_pr_status(&with_ci(verdict, checks))
            .and_then(|status| status.ci)
            .map(|ci| (ci.state, ci.failed))
    }

    /// A job of a GitHub Actions workflow named `CI`, run for a pull request.
    fn run(name: &str, status: &str, conclusion: &str) -> serde_json::Value {
        serde_json::json!({ "__typename": "CheckRun", "name": name, "status": status, "conclusion": conclusion,
            "checkSuite": { "workflowRun": { "event": "pull_request", "workflow": { "name": "CI" } } } })
    }

    fn context(name: &str, state: &str) -> serde_json::Value {
        serde_json::json!({ "__typename": "StatusContext", "context": name, "state": state })
    }

    fn at(mut check: serde_json::Value, started: &str, completed: &str) -> serde_json::Value {
        check["startedAt"] = started.into();
        check["completedAt"] = completed.into();
        check
    }

    fn names(failed: &[&str]) -> Vec<String> {
        failed.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn what_github_says_of_the_ci_is_the_verdict() {
        use CiState::{Failed, NoCi, Passed, Pending};
        // No check and no status at all.
        let none = serde_json::json!({ "state": "OPEN", "commits": { "nodes": [{ "commit": { "verdict": null, "checks": null } }] } });
        assert_eq!(
            parse_pr_status(&none).and_then(|status| status.ci),
            Some(PrChecks {
                state: NoCi,
                failed: Vec::new()
            })
        );
        let all_good = serde_json::json!([
            run("test", "COMPLETED", "SUCCESS"),
            run("docs", "COMPLETED", "SKIPPED"),
            context("ci/circleci", "SUCCESS"),
        ]);
        assert_eq!(ci_of("SUCCESS", all_good), Some((Passed, vec![])));
        // GitHub can count as passed what a run of a check that another replaced says failed:
        // psf/black#5677 ran `changelog / check` four times in three seconds and only the first
        // failed. Keel never contradicts GitHub.
        let replaced = serde_json::json!([
            at(
                run("check", "COMPLETED", "FAILURE"),
                "2026-11-09T06:51:03Z",
                "2026-11-09T06:51:07Z"
            ),
            at(
                run("check", "COMPLETED", "SUCCESS"),
                "2026-11-09T06:51:01Z",
                "2026-11-09T06:51:05Z"
            ),
        ]);
        assert_eq!(ci_of("SUCCESS", replaced), Some((Passed, vec![])));
        for waiting in ["PENDING", "EXPECTED"] {
            assert_eq!(
                ci_of(
                    waiting,
                    serde_json::json!([
                        run("test", "IN_PROGRESS", ""),
                        run("lint", "COMPLETED", "FAILURE")
                    ])
                ),
                Some((Pending, vec![])),
                "{waiting}"
            );
        }
        // A failure names its checks, each once.
        let broken = serde_json::json!([
            run("lint", "COMPLETED", "FAILURE"),
            run("test", "QUEUED", ""),
            run("e2e", "COMPLETED", "TIMED_OUT"),
            context("ci/circleci", "ERROR"),
            context("ci/circleci", "ERROR"),
        ]);
        assert_eq!(
            ci_of("FAILURE", broken.clone()),
            Some((Failed, names(&["lint", "e2e", "ci/circleci"])))
        );
        assert_eq!(
            ci_of("ERROR", broken),
            Some((Failed, names(&["lint", "e2e", "ci/circleci"])))
        );
        // GitHub says it failed even when none of the checks Keel got does, like one past the
        // first hundred.
        assert_eq!(
            ci_of(
                "FAILURE",
                serde_json::json!([run("test", "COMPLETED", "SUCCESS")])
            ),
            Some((Failed, vec![]))
        );
        // An answer that doesn't say leaves what Keel knew.
        assert_eq!(
            parse_pr_status(&serde_json::json!({ "state": "OPEN" })).and_then(|s| s.ci),
            None
        );
        // A repo decides how many there are.
        let many: Vec<serde_json::Value> = (0..40)
            .map(|n| run(&format!("job {n}"), "COMPLETED", "FAILURE"))
            .collect();
        let (_, failed) = ci_of("FAILURE", serde_json::json!(many)).unwrap();
        assert_eq!(failed.len(), MOST_FAILED_NAMED);
        assert_eq!(failed[0], "job 0");
    }

    /// A `gh` that writes down how it was called, then answers with what's in `reply`.
    fn fake_gh(dir: &Path, reply: &str) -> Gh {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("gh");
        let script = format!(
            "#!/bin/bash\necho \"$* [token:$GH_TOKEN]\" >> '{log}'\n{reply}\n",
            log = dir.join("calls.log").display(),
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh {
            bin,
            path_env: "/usr/bin:/bin".to_owned(),
        }
    }

    /// How GitHub's GraphQL answers about pull request 12, as `gh api graphql` prints it.
    fn answer(pull_request: &str) -> String {
        format!("echo '{{\"data\":{{\"repository\":{{\"pullRequest\":{pull_request}}}}}}}'")
    }

    #[test]
    fn asking_about_a_pull_request_only_reads_and_the_token_stays_out_of_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            &answer(
                r#"{"state":"MERGED","isDraft":false,"mergedAt":"2026-11-06T10:00:00Z","closedAt":"2026-11-06T10:00:00Z","updatedAt":"2026-11-06T10:00:00Z","commits":{"nodes":[{"commit":{"verdict":{"state":"FAILURE"},"checks":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"}]}}}}]}}"#,
            ),
        );
        let token = Token("gho_secret".into());
        let status = gh.pr_status(&token, "acme/app", 12).unwrap();
        assert_eq!(status.state, PrState::Merged);
        assert_eq!(status.at.as_deref(), Some("2026-11-06T10:00:00Z"));
        assert_eq!(
            status.ci,
            Some(PrChecks {
                state: CiState::Failed,
                failed: vec!["lint".to_owned()]
            })
        );
        // One question, which only reads, with GitHub's verdict asked on its own: asked next to
        // the checks, the same field counts runs that later ones replaced.
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            calls[0].starts_with("api graphql -f query=query("),
            "{}",
            calls[0]
        );
        assert!(!calls[0].contains("mutation"), "{}", calls[0]);
        assert!(
            calls[0].contains("verdict:statusCheckRollup{state}"),
            "{}",
            calls[0]
        );
        assert!(calls[0].contains("event"), "{}", calls[0]);
        // Its commits come in the same question, not in one more per pull request.
        assert!(
            calls[0].contains(
                "newest:commits(last:100){nodes{commit{oid parents(first:1){totalCount}}}}"
            ),
            "{}",
            calls[0]
        );
        assert!(
            calls[0].ends_with(" -f owner=acme -f name=app -F number=12 [token:gho_secret]"),
            "{}",
            calls[0]
        );
    }

    #[test]
    fn a_look_at_issues_reads_those_open_with_the_label_and_which_of_the_others_closed() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"echo '{"data":{"repository":{"label":{"name":"keel"},"open":{"nodes":[{"number":9,"title":"Nine","url":"https://github.com/acme/app/issues/9","body":"b9","author":{"login":"bob"}},{"number":8,"title":"Eight","url":"https://github.com/acme/app/issues/8","body":"","author":null}]},"i3":{"state":"CLOSED"},"i4":{"state":"OPEN"}}}}'"#,
        );
        let token = Token("gho_secret".into());
        let look = gh
            .look_at_issues(&token, "acme/app", "keel, ready", &[3, 4])
            .unwrap();
        assert!(look.label_exists);
        assert_eq!(
            look.open.iter().map(|i| i.number).collect::<Vec<_>>(),
            [9, 8]
        );
        assert_eq!(look.open[0].author, "bob");
        assert_eq!(look.open[0].body, "b9");
        assert_eq!(look.open[1].author, "");
        assert_eq!(look.closed, [3]);
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
        assert!(calls.starts_with("api graphql -f query=query("), "{calls}");
        assert!(!calls.contains("mutation"), "{calls}");
        assert!(calls.contains("i3:issue(number:3){state}"), "{calls}");
        // The label goes as a variable, whatever it says, and the token never as an argument.
        assert!(
            calls
                .trim_end()
                .ends_with(" -f owner=acme -f name=app -f label=keel, ready [token:gho_secret]"),
            "{calls}"
        );
    }

    #[test]
    fn an_issue_github_cannot_find_spoils_only_its_own_part_of_the_look() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"echo '{"data":{"repository":{"label":null,"open":{"nodes":[]},"i3":null,"i5":{"state":"CLOSED"}}},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to an Issue with the number of 3."}]}'
echo 'gh: Could not resolve to an Issue with the number of 3.' >&2; exit 1"#,
        );
        let look = gh
            .look_at_issues(&Token("t".into()), "acme/app", "keel", &[3, 5])
            .unwrap();
        assert!(!look.label_exists);
        assert!(look.open.is_empty());
        assert_eq!(look.closed, [5]);
    }

    #[test]
    fn a_look_at_issues_that_gets_no_answer_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            "echo 'HTTP 401: Bad credentials (https://api.github.com/graphql) gho_secret' >&2; exit 1",
        );
        let error = gh
            .look_at_issues(&Token("gho_secret".into()), "acme/app", "keel", &[])
            .unwrap_err();
        assert!(error.detail.contains("Bad credentials"), "{error:?}");
        assert!(!error.detail.contains("gho_secret"), "{error:?}");
        assert!(
            gh.look_at_issues(&Token("t".into()), "not-a-repo", "keel", &[])
                .is_err()
        );
    }

    #[test]
    fn a_comment_on_an_issue_is_one_post_with_the_text_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo '{}'");
        gh.comment_on_issue(
            &Token("gho_secret".into()),
            "acme/app",
            7,
            "@team Keel started.\nSecond line",
        )
        .unwrap();
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert_eq!(
            calls.trim_end(),
            "api repos/acme/app/issues/7/comments --method POST -f body=@team Keel started.\nSecond line [token:gho_secret]"
        );
    }

    #[test]
    fn a_token_that_cannot_read_the_checks_still_reads_the_pull_request() {
        let dir = tempfile::tempdir().unwrap();
        let reply = format!(
            "case \"$*\" in *statusCheckRollup*) echo 'gh: Resource not accessible by personal access token' >&2; exit 1;; esac\n{}",
            answer(r#"{"state":"MERGED","isDraft":false,"mergedAt":"2026-11-06T10:00:00Z"}"#)
        );
        let gh = fake_gh(dir.path(), &reply);
        let status = gh
            .pr_status(&Token("gho_secret".into()), "acme/app", 12)
            .unwrap();
        assert_eq!(status.state, PrState::Merged);
        assert_eq!(status.ci, None, "what its CI says stays unknown");
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].contains("statusCheckRollup"));
        assert!(!calls[1].contains("statusCheckRollup"), "{}", calls[1]);
        assert!(calls[1].contains("state isDraft"), "{}", calls[1]);
        assert!(calls[1].contains("newest:commits("), "{}", calls[1]);
    }

    #[test]
    fn of_the_runs_of_a_check_for_one_event_the_newest_says_where_it_stands() {
        let event = |mut check: serde_json::Value, event: &str| {
            check["checkSuite"]["workflowRun"]["event"] = event.into();
            check
        };
        let broken = |checks: serde_json::Value| ci_of("FAILURE", checks).map(|(_, failed)| failed);
        // Started again for the same event, however soon: the newest is where it stands, as
        // `gh pr checks` says.
        let replaced = [
            at(
                run("lint", "COMPLETED", "FAILURE"),
                "2026-11-06T10:00:00Z",
                "2026-11-06T10:00:30Z",
            ),
            at(
                run("lint", "COMPLETED", "SUCCESS"),
                "2026-11-06T10:00:02Z",
                "2026-11-06T10:00:40Z",
            ),
            at(
                run("test", "COMPLETED", "FAILURE"),
                "2026-11-06T10:00:00Z",
                "2026-11-06T10:01:00Z",
            ),
        ];
        assert_eq!(broken(serde_json::json!(replaced)), Some(names(&["test"])));
        let mut reversed = replaced.to_vec();
        reversed.reverse();
        assert_eq!(broken(serde_json::json!(reversed)), Some(names(&["test"])));
        let broken_again = [
            at(
                run("lint", "COMPLETED", "SUCCESS"),
                "2026-11-06T10:00:00Z",
                "2026-11-06T10:01:00Z",
            ),
            at(
                run("lint", "COMPLETED", "FAILURE"),
                "2026-11-06T10:10:00Z",
                "2026-11-06T10:11:00Z",
            ),
        ];
        assert_eq!(
            broken(serde_json::json!(broken_again)),
            Some(names(&["lint"]))
        );
        // A push's and a pull request's runs are two checks, like psf/black#5629's.
        let siblings = serde_json::json!([
            at(
                event(run("lint", "COMPLETED", "FAILURE"), "push"),
                "2026-11-06T10:00:00Z",
                "2026-11-06T10:00:30Z"
            ),
            at(
                run("lint", "COMPLETED", "SUCCESS"),
                "2026-11-06T10:00:02Z",
                "2026-11-06T10:00:40Z"
            ),
        ]);
        assert_eq!(broken(siblings), Some(names(&["lint"])));
        // A run that hasn't started yet, with no time or the zero one, is the newest.
        for unset in [
            serde_json::json!("0001-01-01T00:00:00Z"),
            serde_json::Value::Null,
        ] {
            let mut queued = run("lint", "QUEUED", "");
            queued["startedAt"] = unset;
            let checks = serde_json::json!([
                at(
                    run("lint", "COMPLETED", "FAILURE"),
                    "2026-11-06T10:00:00Z",
                    "2026-11-06T10:01:00Z"
                ),
                queued,
                run("e2e", "COMPLETED", "FAILURE"),
            ]);
            assert_eq!(broken(checks), Some(names(&["e2e"])));
        }
        // The same job in another workflow is another check.
        let mut nightly = at(
            run("test", "COMPLETED", "FAILURE"),
            "2026-11-06T10:00:00Z",
            "2026-11-06T10:01:00Z",
        );
        nightly["checkSuite"]["workflowRun"]["workflow"]["name"] = "Nightly".into();
        let two_workflows = serde_json::json!([
            at(
                run("test", "COMPLETED", "SUCCESS"),
                "2026-11-06T10:10:00Z",
                "2026-11-06T10:11:00Z"
            ),
            nightly,
        ]);
        assert_eq!(broken(two_workflows), Some(names(&["test"])));
    }

    #[test]
    fn checks_that_say_no_event_also_count_their_newest_run() {
        let broken = |checks: serde_json::Value| ci_of("FAILURE", checks).map(|(_, failed)| failed);
        let status = |state: &str, created: &str| serde_json::json!({ "__typename": "StatusContext", "context": "deploy", "state": state, "createdAt": created });
        // Set again seconds later or minutes later, the newest is where it stands, as gh's
        // `eliminateDuplicates` keeps it.
        for later in ["2026-11-06T10:00:02Z", "2026-11-06T10:05:00Z"] {
            assert_eq!(
                broken(serde_json::json!([
                    status("FAILURE", "2026-11-06T10:00:00Z"),
                    status("SUCCESS", later),
                ])),
                Some(vec![]),
                "{later}"
            );
        }
        assert_eq!(
            broken(serde_json::json!([
                status("SUCCESS", "2026-11-06T10:00:00Z"),
                status("FAILURE", "2026-11-06T10:00:02Z"),
            ])),
            Some(names(&["deploy"]))
        );
        // A check run of an app that isn't GitHub Actions has no workflow run.
        let app = |conclusion: &str, started: &str| {
            serde_json::json!({ "__typename": "CheckRun", "name": "CodSpeed", "status": "COMPLETED", "conclusion": conclusion,
                "startedAt": started, "completedAt": started, "checkSuite": { "workflowRun": null } })
        };
        assert_eq!(
            broken(serde_json::json!([
                app("FAILURE", "2026-11-06T10:00:00Z"),
                app("SUCCESS", "2026-11-06T10:00:03Z"),
            ])),
            Some(vec![])
        );
        // A commit status and a check run with the same name are two checks.
        assert_eq!(
            broken(serde_json::json!([
                { "__typename": "StatusContext", "context": "build", "state": "FAILURE", "createdAt": "2026-11-06T10:00:00Z" },
                at(run("build", "COMPLETED", "SUCCESS"), "2026-11-06T10:10:00Z", "2026-11-06T10:11:00Z"),
            ])),
            Some(names(&["build"]))
        );
    }

    #[test]
    fn only_an_answer_about_the_checks_is_asked_again_without_them() {
        for said in [
            "HTTP 401: Bad credentials (https://api.github.com/graphql)",
            "GraphQL: Could not resolve to a PullRequest with the number of 12. (repository.pullRequest)",
            "HTTP 404: Not Found (https://api.github.com/graphql)",
            "GraphQL: API rate limit exceeded for user ID 1.",
            // GitHub answers a token past its limit with a 403, like one that can't read something.
            "HTTP 403: API rate limit exceeded for user ID 1. (https://api.github.com/graphql)",
            "HTTP 403: You have exceeded a secondary rate limit. Please wait a few minutes before you try again. (https://api.github.com/graphql)",
            "HTTP 403: You have exceeded a secondary rate limit (repository.pullRequest.statusCheckRollup)",
            "HTTP 429: Too Many Requests (https://api.github.com/graphql)",
            "error connecting to api.github.com",
            "HTTP 502: Bad Gateway (https://api.github.com/graphql)",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let gh = fake_gh(dir.path(), &format!("echo '{said}' >&2; exit 1"));
            // A token no word has in it, since it's taken out of what gh says.
            gh.pr_status(&Token("gho_secret".into()), "acme/app", 12)
                .unwrap_err();
            let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
            assert_eq!(calls.lines().count(), 1, "{said}: {calls}");
        }
    }

    #[test]
    fn a_failed_question_about_a_pull_request_never_repeats_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            "echo 'HTTP 401: Bad credentials for gho_secret' >&2; exit 1",
        );
        let error = gh
            .pr_status(&Token("gho_secret".into()), "acme/app", 12)
            .unwrap_err();
        assert_eq!(error.kind, "notSignedIn");
        assert_eq!(error.detail, "HTTP 401: Bad credentials for <token>");

        for said in ["echo 'not what it should say'".to_owned(), answer("null")] {
            let dir = tempfile::tempdir().unwrap();
            let gh = fake_gh(dir.path(), &said);
            let error = gh
                .pr_status(&Token("gho_secret".into()), "acme/app", 12)
                .unwrap_err();
            assert_eq!(error.kind, "other", "{said}");
        }
        // A repo Keel couldn't name to GitHub isn't asked about.
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "exit 1");
        assert!(
            gh.pr_status(&Token("gho_secret".into()), "acme", 12)
                .is_err()
        );
        assert!(!dir.path().join("calls.log").exists());
    }

    #[test]
    fn this_module_has_no_merge_or_force_push() {
        let source = include_str!("github.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for word in ["\"merge\"", "--force", "--admin", "\"switch\""] {
            assert!(!code.contains(word), "{word}");
        }
    }
}
