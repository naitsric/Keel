//! Git as Keel runs it. The agent works in a private clone. Credentials only ever touch the
//! mirror, a bare repo no agent can reach. Anything that reads that clone runs in the sandbox,
//! because a `.git/config` can name programs for git to run, and a step could have written one.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use keel_harness::github::{Token, classify_error, git_credential_args};
use keel_harness::process::{SIGKILL, signal_group};
use keel_harness::settings::check_profile;

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
/// Where a run's tools keep their caches. Inside the private copy, since the real ones live in the
/// home folder, which sandboxed commands can't reach.
pub const RUN_CACHE: &str = ".keel-cache";

/// How long git gets in a workspace. What a step left there could make it hang, and a run must
/// never sit there saying nothing.
const WORKSPACE_LIMIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct Git {
    bin: PathBuf,
    limit: Duration,
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
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            limit: WORKSPACE_LIMIT,
        }
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.bin);
        Self::harden(&mut command, cwd);
        command
    }

    fn harden(command: &mut Command, cwd: &Path) {
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
                "-c",
                "gc.auto=0",
                "-c",
                "color.ui=false",
            ]);
    }

    /// Where this git's own files are, if they happen to be in the home folder.
    fn install_dirs(&self, home: &Path) -> Vec<String> {
        let real = self.bin.canonicalize().unwrap_or_else(|_| self.bin.clone());
        let Some(dir) = real.parent() else {
            return Vec::new();
        };
        let libexec = dir.parent().map(|prefix| prefix.join("libexec/git-core"));
        [Some(dir.to_path_buf()), libexec]
            .into_iter()
            .flatten()
            // A git sitting right in the home folder must not open up the whole folder.
            .filter(|path| !home.starts_with(path))
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    /// Git on a run's workspace, in the same sandbox as a `check` step. Whatever the workspace
    /// makes git run gets no more than that step did.
    fn run_in_workspace(&self, workspace: &Path, args: &[&str]) -> Result<String, GitError> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| GitError::Failed("HOME is not set".to_owned()))?;
        let profile = check_profile(workspace, &home, &self.install_dirs(&home), &[]);
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.args(["-p", &profile]).arg(&self.bin);
        Self::harden(&mut command, workspace);
        command.args(args);
        let (ok, stdout, stderr) = bounded_output(command, self.limit)?;
        if ok {
            Ok(String::from_utf8_lossy(&stdout).trim_end().to_owned())
        } else {
            Err(GitError::Failed(format!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&stderr).trim()
            )))
        }
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
        // The run's own cache must never end up in a commit.
        let exclude = workspace.join(".git/info/exclude");
        if let Some(parent) = exclude.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut excluded = std::fs::read_to_string(&exclude).unwrap_or_default();
        excluded.push_str(&format!("\n/{RUN_CACHE}/\n"));
        std::fs::write(&exclude, excluded)?;
        self.head(workspace)
    }

    pub fn head(&self, workspace: &Path) -> Result<String, GitError> {
        self.run_in_workspace(workspace, &["rev-parse", "HEAD"])
    }

    pub fn current_branch(&self, workspace: &Path) -> Result<String, GitError> {
        self.run_in_workspace(workspace, &["symbolic-ref", "--short", "HEAD"])
    }

    /// How many commits the run's branch has beyond what it started from.
    pub fn commits_ahead(&self, mirror: &Path, workspace: &Path) -> Result<usize, GitError> {
        let head = self.head(workspace)?;
        let base = self.run(mirror, &["rev-parse", &format!("refs/heads/{BASE_REF}")])?;
        let count = self.run_in_workspace(
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
            .run_in_workspace(
                workspace,
                &["log", "--reverse", "--format=%s", &format!("{base}..HEAD")],
            )
            .ok()?;
        log.lines().next().map(str::to_owned)
    }

    /// Copies the run's branch into the mirror. This is the one command outside the sandbox that
    /// touches the workspace: it has to write to the mirror, and the other end, `upload-pack`, is
    /// the part of git made to be safe on a repo nobody trusts.
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
        if !self
            .run_in_workspace(workspace, &["status", "--porcelain"])?
            .is_empty()
        {
            self.run_in_workspace(workspace, &["add", "-A"])?;
            let message = format!("keel: changes left uncommitted by {step_id}");
            self.run_in_workspace(workspace, &["commit", "-q", "--no-verify", "-m", &message])?;
        }
        let sha = self.head(workspace)?;
        let range = format!("{before}..{sha}");
        let files = self
            .run_in_workspace(workspace, &["diff", "--no-ext-diff", "--name-only", &range])?
            .lines()
            .map(str::to_owned)
            .collect();
        let diff = self.diff_range(workspace, &range)?;
        Ok(Snapshot { sha, files, diff })
    }

    /// The change itself. Between two commits no filter applies, and these flags keep what the
    /// workspace's config could name from rewriting what a person is about to approve.
    fn diff_range(&self, workspace: &Path, range: &str) -> Result<String, GitError> {
        self.run_in_workspace(
            workspace,
            &["diff", "--no-ext-diff", "--no-textconv", range],
        )
    }

    /// The files the run has changed since `base`, as the pull request will list them.
    pub fn files_since(&self, workspace: &Path, base: &str) -> Result<Vec<String>, GitError> {
        let range = format!("{base}..HEAD");
        Ok(self
            .run_in_workspace(workspace, &["diff", "--no-ext-diff", "--name-only", &range])?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Everything the run has committed since `base`, as the pull request will show it.
    pub fn diff_since(&self, workspace: &Path, base: &str) -> Result<String, GitError> {
        self.diff_range(workspace, &format!("{base}..HEAD"))
    }

    /// Whether a command left the workspace different, which a check shouldn't.
    pub fn is_dirty(&self, workspace: &Path) -> bool {
        self.run_in_workspace(workspace, &["status", "--porcelain"])
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

/// Runs a command and gives up on it at the limit, along with anything it started.
fn bounded_output(
    mut command: Command,
    limit: Duration,
) -> Result<(bool, Vec<u8>, Vec<u8>), GitError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let (sender, receiver) = mpsc::channel();
    let drain = |is_stdout: bool, pipe: Option<Box<dyn std::io::Read + Send>>| {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sender.send((is_stdout, bytes));
        });
    };
    drain(true, child.stdout.take().map(|p| Box::new(p) as _));
    drain(false, child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            signal_group(child.id() as i32, SIGKILL);
            let _ = child.wait();
            return Err(GitError::Failed(format!(
                "git took longer than {} seconds",
                limit.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    // Something it left behind could hold the pipes open, and that's no reason to keep waiting.
    for _ in 0..2 {
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok((true, bytes)) => stdout = bytes,
            Ok((false, bytes)) => stderr = bytes,
            Err(_) => break,
        }
    }
    Ok((status.success(), stdout, stderr))
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

    /// A run's workspace after a step that had write access to `.git/config`, set up as in a real
    /// install: under the home folder, with a canary a program outside the sandbox could touch.
    struct Poisoned {
        _dir: tempfile::TempDir,
        git: Git,
        mirror: PathBuf,
        workspace: PathBuf,
        canary: PathBuf,
        base: String,
    }

    fn sh(cwd: &Path, script: &str) {
        let output = Command::new("/bin/sh")
            .current_dir(cwd)
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn poisoned(config: &str) -> Poisoned {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let dir = tempfile::Builder::new()
            .prefix(".keel-git-test")
            .tempdir_in(&home)
            .unwrap();
        let root = dir.path().to_path_buf();
        let git = Git::new(PathBuf::from("/usr/bin/git"));
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        sh(
            &source,
            "git init -q -b main && git config user.name T && git config user.email t@e.c && echo one > a.txt && git add -A && git commit -qm init",
        );
        let mirror = root.join("mirror.git");
        git.update_mirror_from_local(&source, &mirror).unwrap();
        let workspace = root.join("workspace");
        let identity = ("T".to_owned(), "t@e.c".to_owned());
        let base = git
            .create_workspace(&mirror, &workspace, "keel/test", &identity)
            .unwrap();
        let canary = root.join("canary");
        let config = config
            .replace("CANARY", &canary.to_string_lossy())
            .replace("MARKER", &workspace.join(".git/marker").to_string_lossy());
        let mut existing = std::fs::read_to_string(workspace.join(".git/config")).unwrap();
        existing.push_str(&config);
        std::fs::write(workspace.join(".git/config"), existing).unwrap();
        std::fs::write(
            workspace.join(".gitattributes"),
            "* filter=evil diff=evil\n",
        )
        .unwrap();
        std::fs::write(workspace.join("a.txt"), "two\n").unwrap();
        Poisoned {
            _dir: dir,
            git,
            mirror,
            workspace,
            canary,
            base,
        }
    }

    const FILTER: &str = "[filter \"evil\"]\n\tclean = \"touch CANARY; touch MARKER; cat\"\n";

    #[test]
    fn a_filter_a_step_planted_stays_in_the_sandbox() {
        let p = poisoned(FILTER);
        let snapshot = p.git.snapshot(&p.workspace, &p.base, "builder").unwrap();
        assert!(
            p.workspace.join(".git/marker").exists(),
            "the filter did run, so this proves something"
        );
        assert!(!p.canary.exists(), "it could not reach the home folder");
        assert_eq!(snapshot.files, [".gitattributes", "a.txt"]);
        assert!(!snapshot.sha.is_empty());
    }

    #[test]
    fn a_planted_diff_program_can_neither_run_nor_lie() {
        let script = "#!/bin/sh\ntouch CANARY\necho FAKE-DIFF\n";
        let p = poisoned("");
        let path = p.workspace.join(".git/evil.sh");
        std::fs::write(&path, script.replace("CANARY", &p.canary.to_string_lossy())).unwrap();
        sh(&p.workspace, "chmod +x .git/evil.sh");
        let config = format!(
            "[diff]\n\texternal = {0}\n[diff \"evil\"]\n\ttextconv = {0}\n\tcommand = {0}\n",
            path.display()
        );
        let mut existing = std::fs::read_to_string(p.workspace.join(".git/config")).unwrap();
        existing.push_str(&config);
        std::fs::write(p.workspace.join(".git/config"), existing).unwrap();

        let snapshot = p.git.snapshot(&p.workspace, &p.base, "builder").unwrap();
        assert!(!p.canary.exists());
        assert!(!snapshot.diff.contains("FAKE-DIFF"), "{}", snapshot.diff);
        assert!(snapshot.diff.contains("+two"), "{}", snapshot.diff);
        let run_diff = p.git.diff_since(&p.workspace, &p.base).unwrap();
        assert!(
            !run_diff.contains("FAKE-DIFF") && run_diff.contains("+two"),
            "{run_diff}"
        );
    }

    #[test]
    fn delivering_a_poisoned_workspace_runs_nothing_either() {
        let p = poisoned(FILTER);
        p.git.snapshot(&p.workspace, &p.base, "builder").unwrap();
        std::fs::write(p.workspace.join("a.txt"), "three\n").unwrap();
        assert!(p.git.is_dirty(&p.workspace));
        assert_eq!(p.git.current_branch(&p.workspace).unwrap(), "keel/test");
        assert_eq!(p.git.commits_ahead(&p.mirror, &p.workspace).unwrap(), 1);
        assert!(p.git.first_subject(&p.mirror, &p.workspace).is_some());
        p.git
            .fetch_run_branch(&p.mirror, &p.workspace, "keel/test")
            .unwrap();
        assert!(!p.canary.exists());
        let fetched = p
            .git
            .run(&p.mirror, &["rev-parse", "refs/heads/keel/test"])
            .unwrap();
        assert_eq!(fetched, p.git.head(&p.workspace).unwrap());
    }

    #[test]
    fn git_that_hangs_is_given_up_on_along_with_what_it_started() {
        let mut p = poisoned(
            "[filter \"evil\"]\n\tclean = \"touch MARKER; sleep 300\"\n\trequired = true\n",
        );
        p.git.limit = Duration::from_millis(600);
        let started = Instant::now();
        let error = p
            .git
            .snapshot(&p.workspace, &p.base, "builder")
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(error.to_string().contains("longer than"), "{error}");
    }

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
