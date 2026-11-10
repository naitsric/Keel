//! GitHub through the user's own `gh`. Keel has no login of its own and stores no token: it asks
//! `gh` for one when it's about to use it and hands it only to that one process.

use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use keel_core::PrState;
use serde::{Deserialize, Serialize};

use crate::command::clean_env;
use crate::process::{SIGKILL, signal_group};

/// How long a look at a pull request gets. It's asked again later anyway.
const LOOK_LIMIT: Duration = Duration::from_secs(30);

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

/// Where a pull request stands. `at` is when it was merged or closed, or when it last changed if
/// neither has happened.
#[derive(Debug, Clone, PartialEq)]
pub struct PrStatus {
    pub state: PrState,
    pub at: Option<String>,
    /// The commit its branch is at. Someone can push to it after Keel opened it.
    pub head: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GhError {
    pub kind: &'static str,
    pub detail: String,
}

/// What a failed push or `gh` call means, so the person gets something they can act on.
pub fn classify_error(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&[
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

    /// The accounts `gh` knows. Reads no token.
    pub fn accounts(&self) -> Result<Vec<Account>, GhError> {
        // `gh auth status` exits non-zero if any account is in trouble, and still prints all of them.
        let output = self
            .command()
            .args(["auth", "status", "--json", "hosts"])
            .output()
            .map_err(|e| GhError {
                kind: "other",
                detail: e.to_string(),
            })?;
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
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

    /// `run`, giving up at the limit along with anything it started. For what runs on its own in the
    /// background, where nobody would notice it hanging and everything after it would wait.
    fn run_within(&self, token: &Token, args: &[&str], limit: Duration) -> Result<String, GhError> {
        let mut command = self.command();
        command
            .args(args)
            .env("GH_TOKEN", token.expose())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
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
                        "gh took longer than {} seconds",
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
        if status.success() {
            return Ok(out);
        }
        let detail = token.redact(&err);
        Err(GhError {
            kind: classify_error(&detail),
            detail,
        })
    }

    /// What became of a pull request. Only reads: after opening one, Keel writes nothing to it.
    pub fn pr_status(&self, token: &Token, repo: &str, number: i64) -> Result<PrStatus, GhError> {
        self.pr_status_within(token, repo, number, LOOK_LIMIT)
    }

    fn pr_status_within(
        &self,
        token: &Token,
        repo: &str,
        number: i64,
        limit: Duration,
    ) -> Result<PrStatus, GhError> {
        let number = number.to_string();
        let text = self.run_within(
            token,
            &[
                "pr",
                "view",
                &number,
                "--repo",
                repo,
                "--json",
                "state,isDraft,mergedAt,closedAt,updatedAt,headRefOid",
            ],
            limit,
        )?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| GhError {
            kind: "other",
            detail: e.to_string(),
        })?;
        parse_pr_status(&value).ok_or_else(|| GhError {
            kind: "other",
            detail: format!("unexpected answer about pull request {number}"),
        })
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
    Some(PrStatus { state, at, head })
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
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    }

    #[test]
    fn reads_where_a_pull_request_stands() {
        let status = |value: serde_json::Value| parse_pr_status(&value);
        let at = |state, at: &str| {
            Some(PrStatus {
                state,
                at: Some(at.to_owned()),
                head: None,
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

    #[test]
    fn asking_about_a_pull_request_only_reads_and_the_token_stays_out_of_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"echo '{"state":"MERGED","isDraft":false,"mergedAt":"2026-11-06T10:00:00Z","closedAt":"2026-11-06T10:00:00Z","updatedAt":"2026-11-06T10:00:00Z"}'"#,
        );
        let token = Token("gho_secret".into());
        let status = gh.pr_status(&token, "acme/app", 12).unwrap();
        assert_eq!(status.state, PrState::Merged);
        assert_eq!(status.at.as_deref(), Some("2026-11-06T10:00:00Z"));
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert_eq!(
            calls.trim(),
            "pr view 12 --repo acme/app --json state,isDraft,mergedAt,closedAt,updatedAt,headRefOid [token:gho_secret]"
        );
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

        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo 'not what it should say'");
        let error = gh
            .pr_status(&Token("gho_secret".into()), "acme/app", 12)
            .unwrap_err();
        assert_eq!(error.kind, "other");
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
