//! Git as Keel runs it. The agent works in a private clone. The app only ever touches a repo
//! the agent can write to with everything that could execute code turned off, and pushes will
//! come from the mirror, which no agent can reach (see docs/m0-report.md).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{0}")]
    Failed(String),
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Git {
    pub bin: PathBuf,
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

    /// The repo's root, or an error if this isn't a repo with at least one commit.
    pub fn toplevel(&self, path: &Path) -> Result<PathBuf, GitError> {
        let root = self.run(path, &["rev-parse", "--show-toplevel"])?;
        self.run(path, &["rev-parse", "--verify", "HEAD"])?;
        Ok(PathBuf::from(root))
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

    /// Brings the project's current commit into the mirror, reading the user's repo and changing
    /// nothing in it. Returns the branch name it was on.
    pub fn update_mirror(&self, source: &Path, mirror: &Path) -> Result<String, GitError> {
        if !mirror.join("HEAD").exists() {
            std::fs::create_dir_all(mirror)?;
            self.run(mirror, &["init", "-q", "--bare", "-b", "keel-base"])?;
        }
        let source_text = source.to_string_lossy();
        self.run(
            mirror,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &source_text,
                "+HEAD:refs/heads/keel-base",
            ],
        )?;
        let branch = Command::new(&self.bin)
            .current_dir(source)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()?;
        Ok(String::from_utf8_lossy(&branch.stdout).trim().to_owned())
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
                "--branch",
                "keel-base",
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
}
