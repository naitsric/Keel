//! The sandbox policy for a step. M0 showed that denying credential paths one by one leaves
//! Keychain readable, so the whole home folder is denied and only what's listed comes back.

use std::path::Path;

use serde_json::{Value, json};

/// Claude Code's own install, which a Bash step may need to read.
const TOOL_DIRS: [&str; 2] = ["~/.local", "~/.claude/local"];

pub fn step_settings(workspace: &Path, allowed_domains: &[String], extra_read: &[String]) -> Value {
    let mut allow_read = vec![workspace.to_string_lossy().into_owned()];
    allow_read.extend(TOOL_DIRS.iter().map(|d| (*d).to_owned()));
    allow_read.extend(extra_read.iter().cloned());
    json!({
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "allowUnsandboxedCommands": false,
            "network": { "allowedDomains": allowed_domains },
            "filesystem": { "denyRead": ["~"], "allowRead": allow_read },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_home_and_never_allows_unsandboxed_commands() {
        let settings = step_settings(
            Path::new("/data/runs/r1/workspace"),
            &["pypi.org".into()],
            &[],
        );
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(settings["sandbox"]["filesystem"]["denyRead"], json!(["~"]));
        let allow = settings["sandbox"]["filesystem"]["allowRead"]
            .as_array()
            .unwrap();
        assert!(allow.contains(&json!("/data/runs/r1/workspace")));
        assert!(!allow.iter().any(|p| p == "~" || p == "~/.ssh"));
        assert_eq!(
            settings["sandbox"]["network"]["allowedDomains"],
            json!(["pypi.org"])
        );
    }
}
