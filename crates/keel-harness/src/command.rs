//! Builds the exact `claude -p` invocation for a step, and the environment it runs in.

use std::path::{Path, PathBuf};
use std::process::Command;

use keel_core::StepSpec;

/// Variables an agent inherits. Everything else, including every token, is dropped.
const ENV_ALLOW: [&str; 7] = [
    "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "SHELL",
];

/// Nobody is there to answer a step's commands, so nothing they run may wait for someone: an editor
/// returns at once with the text as it was, a pager just prints, and git fails rather than ask for
/// a password. `TERM` is the same however Keel was opened: from a terminal it was that terminal's,
/// which no step has, and tools took it as someone to show an editor or a full-screen page to.
const NOBODY_ANSWERS: [(&str, &str); 7] = [
    ("TERM", "dumb"),
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// Variables that, if set in the user's environment, would change how Claude Code bills or
/// authenticates. Keel removes them and says so.
pub const IGNORED_AUTH_ENV: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

pub fn ignored_auth_env() -> Vec<&'static str> {
    IGNORED_AUTH_ENV
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect()
}

/// The environment for any process that belongs to a step, with nothing inherited by accident.
pub fn clean_env(path_env: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ENV_ALLOW
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v)))
        .collect();
    env.push(("PATH".into(), path_env.to_owned()));
    // The sandbox hides the home folder, and git refuses to start if it can't read a config it
    // knows is there. This way it doesn't look. A run's identity is in its own clone's config.
    env.push(("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()));
    env.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    env.extend(
        NOBODY_ANSWERS
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    // Removes the ability to leave a test suite running behind the agent's back, which is what
    // jammed the Archon pilot. Long commands then need a generous foreground timeout.
    env.push(("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS".into(), "1".into()));
    env.push(("BASH_DEFAULT_TIMEOUT_MS".into(), "1800000".into()));
    env.push(("BASH_MAX_TIMEOUT_MS".into(), "3600000".into()));
    env.push((
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
        "1".into(),
    ));
    env
}

#[derive(Debug, Clone)]
pub struct StepCommand {
    pub claude: PathBuf,
    pub path_env: String,
    pub workspace: PathBuf,
    pub settings_file: PathBuf,
    pub system_prompt_file: PathBuf,
    pub session_id: String,
    /// Continue this session instead of starting `session_id`.
    pub resume: Option<String>,
    /// Makes the step return JSON matching this schema, in `structured_output`.
    pub json_schema: Option<String>,
    /// Folders outside the workspace the file tools may reach. They're writable too unless the
    /// settings deny it, which `settings::with_read_only` does.
    pub add_dirs: Vec<PathBuf>,
    /// A script `claude` runs ahead of each Bash command, in the sandbox, such as `home::script`.
    /// Only its commands get those variables: `claude` itself keeps its own.
    pub env_file: Option<PathBuf>,
}

/// Where `claude` looks for `env_file`.
const ENV_FILE: &str = "CLAUDE_ENV_FILE";

impl StepCommand {
    /// The arguments, kept apart from process setup so tests can read them.
    pub fn args(&self, step: &StepSpec) -> Vec<String> {
        let tools = step.tools.tools().join(",");
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--restricted",
            "--strict-mcp-config",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        // It takes any number of folders, up to the next flag.
        if !self.add_dirs.is_empty() {
            args.push("--add-dir".to_owned());
            args.extend(
                self.add_dirs
                    .iter()
                    .map(|dir| dir.to_string_lossy().into_owned()),
            );
        }
        let mut pair = |flag: &str, value: String| {
            args.push(flag.to_owned());
            args.push(value);
        };
        pair("--tools", tools.clone());
        pair("--allowedTools", tools);
        pair("--permission-mode", "dontAsk".to_owned());
        pair("--model", step.model.clone());
        pair(
            "--settings",
            self.settings_file.to_string_lossy().into_owned(),
        );
        pair(
            "--append-system-prompt-file",
            self.system_prompt_file.to_string_lossy().into_owned(),
        );
        match &self.resume {
            Some(session) => pair("--resume", session.clone()),
            None => pair("--session-id", self.session_id.clone()),
        }
        if let Some(schema) = &self.json_schema {
            pair("--json-schema", schema.clone());
        }
        if let Some(effort) = &step.effort {
            pair("--effort", effort.clone());
        }
        if let Some(turns) = step.max_turns {
            pair("--max-turns", turns.to_string());
        }
        if let Some(budget) = step.max_budget_usd {
            pair("--max-budget-usd", budget.to_string());
        }
        args
    }

    pub fn command(&self, step: &StepSpec) -> Command {
        let mut command = Command::new(&self.claude);
        command
            .args(self.args(step))
            .current_dir(&self.workspace)
            .env_clear()
            .envs(clean_env(&self.path_env));
        if let Some(file) = &self.env_file {
            command.env(ENV_FILE, file);
        }
        command
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::ToolProfile;

    fn step(tools: ToolProfile) -> StepSpec {
        StepSpec {
            id: "spec".into(),
            model: "opus".into(),
            effort: Some("high".into()),
            tools,
            prompt: "x".into(),
            max_turns: Some(5),
            max_budget_usd: None,
            paths: vec![],
            when: None,
            risks: vec![],
            model_when_risky: None,
            role: Default::default(),
        }
    }

    fn command() -> StepCommand {
        StepCommand {
            claude: "/bin/claude".into(),
            path_env: "/bin".into(),
            workspace: "/w".into(),
            settings_file: "/s.json".into(),
            system_prompt_file: "/p.md".into(),
            session_id: "sid".into(),
            resume: None,
            json_schema: None,
            add_dirs: Vec::new(),
            env_file: None,
        }
    }

    fn value_after(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .map(|i| args[i + 1].clone())
    }

    #[test]
    fn each_step_gets_its_own_model_and_a_hardened_launch() {
        let args = command().args(&step(ToolProfile::Build));
        assert_eq!(value_after(&args, "--model").as_deref(), Some("opus"));
        assert_eq!(value_after(&args, "--effort").as_deref(), Some("high"));
        assert_eq!(
            value_after(&args, "--permission-mode").as_deref(),
            Some("dontAsk")
        );
        assert!(args.iter().any(|a| a == "--restricted"));
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert_eq!(value_after(&args, "--session-id").as_deref(), Some("sid"));
    }

    #[test]
    fn read_only_steps_have_no_way_to_run_commands() {
        let args = command().args(&step(ToolProfile::ReadOnly));
        let tools = value_after(&args, "--tools").unwrap();
        assert_eq!(tools, "Read,Grep,Glob");
        assert!(!tools.contains("Bash") && !tools.contains("Edit") && !tools.contains("Agent"));
    }

    #[test]
    fn build_steps_cannot_spawn_agents_or_monitors() {
        let tools = value_after(&command().args(&step(ToolProfile::Build)), "--tools").unwrap();
        assert!(!tools.contains("Agent") && !tools.contains("Task") && !tools.contains("Monitor"));
    }

    #[test]
    fn a_reviewer_is_asked_for_a_structured_answer() {
        let mut c = command();
        assert!(value_after(&c.args(&step(ToolProfile::ReadOnly)), "--json-schema").is_none());
        c.json_schema = Some("{}".into());
        assert_eq!(
            value_after(&c.args(&step(ToolProfile::ReadOnly)), "--json-schema").as_deref(),
            Some("{}")
        );
    }

    #[test]
    fn folders_outside_the_workspace_are_named_only_when_there_are_some() {
        let mut c = command();
        assert!(
            !c.args(&step(ToolProfile::ReadOnly))
                .contains(&"--add-dir".to_owned())
        );
        c.add_dirs = vec!["/r/context/a".into(), "/r/context/b".into()];
        let args = c.args(&step(ToolProfile::ReadOnly));
        let at = args.iter().position(|a| a == "--add-dir").unwrap();
        assert_eq!(args[at + 1..at + 3], ["/r/context/a", "/r/context/b"]);
        assert!(
            args[at + 3].starts_with("--"),
            "the next flag ends the list"
        );
    }

    #[test]
    fn resume_replaces_session_id() {
        let mut c = command();
        c.resume = Some("old".into());
        let args = c.args(&step(ToolProfile::Build));
        assert_eq!(value_after(&args, "--resume").as_deref(), Some("old"));
        assert!(value_after(&args, "--session-id").is_none());
    }

    #[test]
    fn a_script_for_bash_leaves_claudes_own_home_alone() {
        let var = |command: &Command, name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        let step = step(ToolProfile::Build);
        let mut c = command();
        assert_eq!(var(&c.command(&step), "CLAUDE_ENV_FILE"), None);
        c.env_file = Some("/r/builder-1.env".into());
        let with = c.command(&step);
        assert_eq!(
            var(&with, "CLAUDE_ENV_FILE").as_deref(),
            Some("/r/builder-1.env")
        );
        assert_eq!(var(&with, "HOME"), std::env::var("HOME").ok());
        assert_eq!(
            var(&with, "GIT_CONFIG_GLOBAL").as_deref(),
            Some("/dev/null")
        );
    }

    #[test]
    fn environment_carries_no_credentials_and_disables_background_tasks() {
        let env = clean_env("/bin");
        for (name, _) in &env {
            assert!(
                !IGNORED_AUTH_ENV.contains(&name.as_str()),
                "{name} must not reach an agent"
            );
            assert!(
                !["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"].contains(&name.as_str()),
                "{name} must not reach an agent"
            );
        }
        assert!(
            env.iter()
                .any(|(k, v)| k == "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS" && v == "1")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "GIT_CONFIG_GLOBAL" && v == "/dev/null"),
            "git must not look for a config the sandbox hides"
        );
        assert!(env.iter().any(|(k, v)| k == "PATH" && v == "/bin"));
    }

    /// A builder once ran `git checkout -p` and its step sat there for 26 minutes.
    #[test]
    fn nothing_a_step_runs_can_wait_for_an_editor_a_pager_or_a_password() {
        let env = clean_env("/bin");
        let var = |name: &str| {
            let found: Vec<&str> = env
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect();
            assert!(found.len() <= 1, "{name} is set once: {found:?}");
            found.first().copied()
        };
        // Never the terminal Keel was opened from: no step has one.
        assert_eq!(var("TERM"), Some("dumb"));
        for editor in ["GIT_EDITOR", "EDITOR", "VISUAL"] {
            assert_eq!(var(editor), Some("true"), "{editor}");
        }
        for pager in ["GIT_PAGER", "PAGER"] {
            assert_eq!(var(pager), Some("cat"), "{pager}");
        }
        assert_eq!(var("GIT_TERMINAL_PROMPT"), Some("0"));
    }

    /// With the real git: a commit with no message would open an editor and wait for it.
    #[test]
    fn git_gives_up_at_once_where_it_would_open_an_editor() {
        let Some(git) = [
            "/usr/bin/git",
            "/opt/homebrew/bin/git",
            "/usr/local/bin/git",
        ]
        .iter()
        .find(|path| Path::new(path).exists()) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let mut command = Command::new(git);
            command
                .args(args)
                .current_dir(dir.path())
                .env_clear()
                .envs(clean_env("/usr/bin:/bin"));
            crate::process::stdout_within(command, std::time::Duration::from_secs(30)).unwrap()
        };
        assert!(run(&["init", "-q"]).is_some());
        let commit = [
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--allow-empty",
        ];
        assert!(run(&commit).is_some(), "it waited for an editor");
        // Nothing was committed, since nobody wrote a message.
        let log = run(&["log", "--oneline"]).unwrap();
        assert!(log.is_empty(), "{}", String::from_utf8_lossy(&log));
    }
}
