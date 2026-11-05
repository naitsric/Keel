//! The sandbox policy for a step. M0 showed that denying credential paths one by one leaves
//! Keychain readable, so the whole home folder is denied and only what's listed comes back.

use std::path::Path;

use serde_json::{Value, json};

/// Claude Code's own install, which a Bash step may need to read, and where pnpm and Rust's
/// installers put themselves. Only programs: Cargo's credentials are in `~/.cargo`, not in its `bin`.
const TOOL_DIRS: [&str; 5] = [
    "~/.local",
    "~/.claude/local",
    "~/Library/pnpm",
    "~/.cargo/bin",
    "~/.rustup",
];

/// `sockets` are the folders of the project's service sockets. Only these open up, and they're the
/// only way a sandboxed command reaches a local server: TCP on localhost would expose all of them.
pub fn step_settings(
    workspace: &Path,
    allowed_domains: &[String],
    extra_read: &[String],
    sockets: &[String],
) -> Value {
    let mut allow_read = vec![workspace.to_string_lossy().into_owned()];
    allow_read.extend(TOOL_DIRS.iter().map(|d| (*d).to_owned()));
    allow_read.extend(extra_read.iter().cloned());
    // The sandbox covers Bash. This covers Claude Code's own file tools. Not all of `.git`: a rule
    // here also reaches the sandbox, and the agent has to be able to commit.
    let root = workspace.to_string_lossy();
    let git_files: Vec<String> = ["config", "config.worktree", "commondir", "hooks/**"]
        .iter()
        .map(|file| format!("Edit(/{root}/.git/{file})"))
        .collect();
    let mut network = json!({ "allowedDomains": allowed_domains });
    if !sockets.is_empty() {
        network["allowUnixSockets"] = json!(sockets);
    }
    json!({
        "permissions": { "deny": git_files },
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "allowUnsandboxedCommands": false,
            "network": network,
            "filesystem": { "denyRead": ["~"], "allowRead": allow_read },
        },
    })
}

/// The same policy for a `check` step, which Keel runs itself. Tests and Makefiles can be edited
/// by an agent, so they get no more trust than the agent does. A Seatbelt profile for
/// `sandbox-exec`: no home folder except what's listed, no network, and writes only here.
pub fn check_profile(
    workspace: &Path,
    home: &Path,
    extra_read: &[String],
    sockets: &[String],
) -> String {
    profile(workspace, home, extra_read, sockets, None)
}

/// A `setup` step installs dependencies, so it gets the internet and a cache. Still no home folder
/// and no credentials, and nothing on this machine: not local ports, and only the listed sockets.
pub fn setup_profile(
    workspace: &Path,
    home: &Path,
    extra_read: &[String],
    sockets: &[String],
    cache: &Path,
) -> String {
    profile(workspace, home, extra_read, sockets, Some(cache))
}

fn profile(
    workspace: &Path,
    home: &Path,
    extra_read: &[String],
    sockets: &[String],
    setup_cache: Option<&Path>,
) -> String {
    let quote = |path: &str| path.replace('\\', "\\\\").replace('"', "\\\"");
    let expand = |path: &str| match path.strip_prefix("~/") {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => path.to_owned(),
    };
    // Node and uv look up each folder on the way to a file as they work out its real path, and stop
    // when one is refused. This says only that these folders exist, not what's in them.
    let way_in: String = workspace
        .ancestors()
        .skip(1)
        .filter(|folder| folder.starts_with(home) && *folder != home)
        .map(|folder| format!(" (literal \"{}\")", quote(&folder.to_string_lossy())))
        .collect();
    let workspace = quote(&workspace.to_string_lossy());
    // Git would run whatever these name, so nothing here gets to change them. Moving `.git` away
    // and making another would get around that, so `.git` itself stays put as well.
    let git_rules = format!(
        "(literal \"{workspace}\") (literal \"{workspace}/.git\") (literal \"{workspace}/.git/config\") \
         (literal \"{workspace}/.git/config.worktree\") (literal \"{workspace}/.git/commondir\") \
         (subpath \"{workspace}/.git/hooks\")"
    );
    let home_text = quote(&home.to_string_lossy());
    let mut read_rules = format!("(subpath \"{workspace}\")");
    for dir in TOOL_DIRS
        .iter()
        .map(|d| (*d).to_owned())
        .chain(extra_read.iter().cloned())
    {
        read_rules.push_str(&format!(" (subpath \"{}\")", quote(&expand(&dir))));
    }
    let mut write_rules = format!("(subpath \"{workspace}\")");
    if let Some(cache) = setup_cache {
        let cache = quote(&cache.to_string_lossy());
        read_rules.push_str(&format!(" (subpath \"{cache}\")"));
        write_rules.push_str(&format!(" (subpath \"{cache}\")"));
    }
    let network = if setup_cache.is_some() {
        "(allow network*)\n\
         (deny network* (remote ip \"localhost:*\"))\n\
         (deny network-outbound (remote unix-socket))\n\
         (allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n"
    } else {
        "(deny network*)\n"
    };
    let socket_rules: String = sockets
        .iter()
        .map(|dir| format!("(allow network-outbound (subpath \"{}\"))\n", quote(dir)))
        .collect();
    format!(
        "(version 1)\n\
         (allow default)\n\
         {network}\
         {socket_rules}\
         (deny file-read* (subpath \"{home_text}\"))\n\
         (allow file-read* {read_rules})\n\
         (allow file-read-metadata (literal \"{home_text}\"){way_in})\n\
         (deny file-write*)\n\
         (allow file-write* {write_rules} (subpath \"/private/tmp\") (subpath \"/private/var/folders\") (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\") (literal \"/dev/tty\") (regex #\"^/dev/fd/\"))\n\
         (deny file-write* {git_rules})\n\
         (deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd\") (global-name \"com.apple.secd\") (global-name \"com.apple.securityd.xpc\"))\n"
    )
}

/// A command's stdin, stdout and stderr are files beside its workspace. Node asks about them as it
/// starts and aborts if that's refused, so a check that uses it dies before running anything.
pub fn with_own_output(profile: String, files: &[&Path]) -> String {
    let rules: String = files
        .iter()
        .map(|file| {
            let path = file
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            format!(" (literal \"{path}\")")
        })
        .collect();
    format!("{profile}(allow file-read-metadata{rules})\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_git_would_run_is_off_limits_and_the_rest_of_git_is_not() {
        let settings = step_settings(Path::new("/w"), &[], &[], &[]);
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(deny.contains(&json!("Edit(//w/.git/config)")));
        assert!(deny.contains(&json!("Edit(//w/.git/hooks/**)")));
        // Denying all of `.git` stops the agent from committing, as a real run showed.
        assert!(
            !deny
                .iter()
                .any(|rule| rule == "Edit(//w/.git/**)" || rule == "Edit(//w/.git)")
        );
    }

    #[test]
    fn denies_home_and_never_allows_unsandboxed_commands() {
        let settings = step_settings(
            Path::new("/data/runs/r1/workspace"),
            &["pypi.org".into()],
            &[],
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

    #[test]
    fn sockets_open_up_only_when_a_project_has_them() {
        let without = step_settings(Path::new("/w"), &[], &[], &[]);
        assert!(
            without["sandbox"]["network"]
                .get("allowUnixSockets")
                .is_none()
        );
        let with = step_settings(Path::new("/w"), &[], &[], &["/s/db".into()]);
        assert_eq!(
            with["sandbox"]["network"]["allowUnixSockets"],
            json!(["/s/db"])
        );
        assert!(
            with["sandbox"]["network"]
                .get("allowLocalBinding")
                .is_none(),
            "that would open every local port"
        );
    }
}
