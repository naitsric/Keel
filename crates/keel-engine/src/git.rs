//! Git as Keel runs it. The agent works in a private clone. Credentials only ever touch the
//! mirror, a bare repo no agent can reach, and everything in git that could execute code is off.

use std::path::{Path, PathBuf};
use std::process::Command;

use keel_harness::github::{Token, classify_error, git_credential_args};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{0}")]
    Failed(String),
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("Keel only pushes its own branches, not {0}")]
    UnsafeBranch(String),
}

impl GitError {
    /// What a person has to fix, if this came from talking to GitHub.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Failed(text) => classify_error(text),
            _ => "other",
        }
    }
}

/// The branch prefix Keel owns. A push to anything else is refused here, whatever the caller says.
pub const BRANCH_PREFIX: &str = "keel/";
const BASE_REF: &str = "keel-base";

#[derive(Debug, Clone)]
pub struct Git {
    pub bin: PathBuf,
}

/// How to reach GitHub for one operation. Nothing here is written anywhere.
pub struct RemoteAuth<'a> {
    pub gh: &'a Path,
    pub token: &'a Token,
    pub path_env: &'a str,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub sha: String,
    pub files: Vec<String>,
    pub diff: String,
}

impl Git {
    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .current_dir(cwd)
            .env_clear()
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "commit.gpgsign=false",
            ]);
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        let output = self.command(cwd).args(args).output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned())
        } else {
            Err(GitError::Failed(format!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn run_remote(&self, cwd: &Path, auth: &RemoteAuth, args: &[&str]) -> Result<String, GitError> {
        let mut command = self.command(cwd);
        command
            .args(git_credential_args(auth.gh))
            .args(args)
            .env("GH_TOKEN", auth.token.expose())
            .env("PATH", auth.path_env);
        let output = command.output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned())
        } else {
            let detail = String::from_utf8_lossy(&output.stderr);
            Err(GitError::Failed(auth.token.redact(detail.trim())))
        }
    }

    /// The repo's root, or an error if this isn't a repo with at least one commit.
    pub fn toplevel(&self, path: &Path) -> Result<PathBuf, GitError> {
        let root = self.run(path, &["rev-parse", "--show-toplevel"])?;
        self.run(path, &["rev-parse", "--verify", "HEAD"])?;
        Ok(PathBuf::from(root))
    }

    /// The `origin` remote of the user's repo, if it has one.
    pub fn origin_url(&self, source: &Path) -> Option<String> {
        self.run(source, &["remote", "get-url", "origin"]).ok()
    }

    /// Reads the identity from the user's repo with their real config, so commits look like theirs.
    pub fn identity(&self, source: &Path) -> (String, String) {
        let read = |key: &str| -> Option<String> {
            let output = Command::new(&self.bin)
                .current_dir(source)
                .args(["config", key])
                .output()
                .ok()?;
            let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (output.status.success() && !value.is_empty()).then_some(value)
        };
        (
            read("user.name").unwrap_or_else(|| "Keel".into()),
            read("user.email").unwrap_or_else(|| "keel@localhost".into()),
        )
    }

    fn ensure_mirror(&self, mirror: &Path) -> Result<(), GitError> {
        if !mirror.join("HEAD").exists() {
            std::fs::create_dir_all(mirror)?;
            self.run(mirror, &["init", "-q", "--bare", "-b", BASE_REF])?;
        }
        Ok(())
    }

    /// Brings the project's current local commit into the mirror, for projects with no GitHub
    /// account set. Reads the user's repo and changes nothing in it. Returns its branch name.
    pub fn update_mirror_from_local(
        &self,
        source: &Path,
        mirror: &Path,
    ) -> Result<String, GitError> {
        self.ensure_mirror(mirror)?;
        let source_text = source.to_string_lossy();
        let refspec = format!("+HEAD:refs/heads/{BASE_REF}");
        self.run(
            mirror,
            &["fetch", "-q", "--no-tags", &source_text, &refspec],
        )?;
        let branch = Command::new(&self.bin)
            .current_dir(source)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()?;
        Ok(String::from_utf8_lossy(&branch.stdout).trim().to_owned())
    }

    /// Brings the default branch from GitHub into the mirror, so a run starts from what's
    /// published and not from whatever the user has checked out.
    pub fn update_mirror_from_remote(
        &self,
        mirror: &Path,
        url: &str,
        branch: &str,
        auth: &RemoteAuth,
    ) -> Result<(), GitError> {
        self.ensure_mirror(mirror)?;
        let refspec = format!("+refs/heads/{branch}:refs/heads/{BASE_REF}");
        self.run_remote(mirror, auth, &["fetch", "-q", "--no-tags", url, &refspec])?;
        Ok(())
    }

    /// A private clone for one run. It has no remote, so `git push` has nowhere to go.
    pub fn create_workspace(
        &self,
        mirror: &Path,
        workspace: &Path,
        branch: &str,
        identity: &(String, String),
    ) -> Result<String, GitError> {
        let parent = workspace.parent().unwrap_or(workspace);
        std::fs::create_dir_all(parent)?;
        self.run(
            parent,
            &[
                "clone",
                "-q",
                "--no-hardlinks",
                "--single-branch",
                "--no-tags",
                "--branch",
                BASE_REF,
                &mirror.to_string_lossy(),
                &workspace.to_string_lossy(),
            ],
        )?;
        self.run(workspace, &["remote", "remove", "origin"])?;
        self.run(workspace, &["checkout", "-q", "-b", branch])?;
        self.run(workspace, &["config", "user.name", &identity.0])?;
        self.run(workspace, &["config", "user.email", &identity.1])?;
        // Git looks for these in the home folder, which the sandbox hides. Without this, every
        // command an agent runs warns about it.
        self.run(workspace, &["config", "core.excludesFile", "/dev/null"])?;
        self.run(workspace, &["config", "core.attributesFile", "/dev/null"])?;
        self.head(workspace)
    }

    pub fn head(&self, repo: &Path) -> Result<String, GitError> {
        self.run(repo, &["rev-parse", "HEAD"])
    }

    pub fn current_branch(&self, repo: &Path) -> Result<String, GitError> {
        self.run(repo, &["symbolic-ref", "--short", "HEAD"])
    }

    /// How many commits the run's branch has beyond what it started from.
    pub fn commits_ahead(&self, mirror: &Path, workspace: &Path) -> Result<usize, GitError> {
        let head = self.head(workspace)?;
        let base = self.run(mirror, &["rev-parse", &format!("refs/heads/{BASE_REF}")])?;
        let count = self.run(
            workspace,
            &["rev-list", "--count", &format!("{base}..{head}")],
        )?;
        Ok(count.trim().parse().unwrap_or(0))
    }

    /// The subject of the run's first commit, which usually says what the change is.
    pub fn first_subject(&self, mirror: &Path, workspace: &Path) -> Option<String> {
        let base = self
            .run(mirror, &["rev-parse", &format!("refs/heads/{BASE_REF}")])
            .ok()?;
        let log = self
            .run(
                workspace,
                &["log", "--reverse", "--format=%s", &format!("{base}..HEAD")],
            )
            .ok()?;
        log.lines().next().map(str::to_owned)
    }

    /// Copies the run's branch into the mirror. The workspace is only read.
    pub fn fetch_run_branch(
        &self,
        mirror: &Path,
        workspace: &Path,
        branch: &str,
    ) -> Result<(), GitError> {
        require_own_branch(branch)?;
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        self.run(
            mirror,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &workspace.to_string_lossy(),
                &refspec,
            ],
        )?;
        Ok(())
    }

    /// Pushes the run's branch from the mirror. Never forced, and only to Keel's own branches.
    pub fn push_branch(
        &self,
        mirror: &Path,
        url: &str,
        branch: &str,
        auth: &RemoteAuth,
    ) -> Result<(), GitError> {
        require_own_branch(branch)?;
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        self.run_remote(mirror, auth, &["push", "-q", "--no-verify", url, &refspec])?;
        Ok(())
    }

    /// Commits whatever the step left uncommitted, then reports what changed since `before`.
    pub fn snapshot(
        &self,
        workspace: &Path,
        before: &str,
        step_id: &str,
    ) -> Result<Snapshot, GitError> {
        if !self.run(workspace, &["status", "--porcelain"])?.is_empty() {
            self.run(workspace, &["add", "-A"])?;
            let message = format!("keel: changes left uncommitted by {step_id}");
            self.run(workspace, &["commit", "-q", "--no-verify", "-m", &message])?;
        }
        let sha = self.head(workspace)?;
        let range = format!("{before}..{sha}");
        let files = self
            .run(workspace, &["diff", "--name-only", &range])?
            .lines()
            .map(str::to_owned)
            .collect();
        let diff = self.run(workspace, &["diff", &range])?;
        Ok(Snapshot { sha, files, diff })
    }

    /// Whether a command left the workspace different, which a check shouldn't.
    pub fn is_dirty(&self, workspace: &Path) -> bool {
        self.run(workspace, &["status", "--porcelain"])
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

fn require_own_branch(branch: &str) -> Result<(), GitError> {
    let safe = branch.starts_with(BRANCH_PREFIX)
        && branch.len() > BRANCH_PREFIX.len()
        && !branch.contains("..")
        && !branch.contains(':')
        && !branch.starts_with('+');
    if safe {
        Ok(())
    } else {
        Err(GitError::UnsafeBranch(branch.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_keel_branches_can_be_pushed() {
        for branch in ["keel/abc", "keel/feature-1"] {
            assert!(require_own_branch(branch).is_ok(), "{branch}");
        }
        for branch in [
            "main",
            "master",
            "keel/",
            "+keel/x",
            "keel/x:main",
            "refs/heads/main",
            "keel/../main",
            "develop",
        ] {
            assert!(require_own_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn no_push_can_be_forced() {
        let source = include_str!("git.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let push = &code
            [code.find("pub fn push_branch").unwrap()..code.find("/// Commits whatever").unwrap()];
        assert!(
            !push.contains("--force") && !push.contains('+'),
            "pushes are never forced"
        );
    }
}
