//! Builds the exact `claude -p` invocation for a step, and the environment it runs in.

use std::path::{Path, PathBuf};
use std::process::Command;

use keel_core::StepSpec;

/// Variables an agent inherits. Everything else, including every token, is dropped.
const ENV_ALLOW: [&str; 8] = [
    "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "SHELL", "TERM",
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
}

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
}
