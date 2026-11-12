//! The sandbox policy for a step. M0 showed that denying credential paths one by one leaves
//! Keychain readable, so the whole home folder is denied and only what's listed comes back.

use std::path::Path;

use serde_json::{Value, json};

/// Claude Code's own install, which a Bash step may need to read. Every project gets these and
/// nothing else in the home folder: the toolchains a project uses are found for it, in
/// `toolchains`, and a person lets them be read.
pub const TOOL_DIRS: [&str; 2] = ["~/.local", "~/.claude/local"];

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

/// Lets an agent read these folders and change nothing in them. `--add-dir` has to name them too,
/// or the file tools can't reach them, and it opens them to writes as well: the `Edit` rule is what
/// closes that, for the file tools and, through the sandbox, for Bash.
pub fn with_read_only(mut settings: Value, folders: &[String]) -> Value {
    for folder in folders {
        let folder = folder.trim_end_matches('/');
        if let Some(allow) = settings["sandbox"]["filesystem"]["allowRead"].as_array_mut() {
            allow.push(json!(folder));
        }
        if let Some(deny) = settings["permissions"]["deny"].as_array_mut() {
            deny.push(json!(format!("Edit(/{folder}/**)")));
        }
    }
    settings
}

/// The paths the Bash hook is given: the program (see `hook`), Keel's git, the project, a folder
/// of the run's outside the project for what it notes before each command, and the sandbox
/// profile its git runs in.
pub struct BashHook<'a> {
    pub program: &'a Path,
    pub git: &'a Path,
    pub workspace: &'a Path,
    pub state: &'a Path,
    pub profile: &'a Path,
}

/// For a step that can edit: Claude Code runs the hook around each Bash command. Before it, the
/// hook only notes what git lists and answers nothing, so permissions go as they would have.
/// After it, whether it worked or not, the agent may get a note about the files that changed, or
/// that a command that looks like an edit changed none. It never stops a command. Hooks run
/// outside the sandbox, so nothing here has to let the program in.
pub fn with_bash_hook(mut settings: Value, hook: &BashHook) -> Value {
    let group = json!([{
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            // With `args` there's no shell in between, so no path needs quoting.
            "command": hook.program.to_string_lossy(),
            "args": [
                hook.git.to_string_lossy(),
                hook.workspace.to_string_lossy(),
                hook.state.to_string_lossy(),
                hook.profile.to_string_lossy(),
            ],
            // It answers in milliseconds. Past this the command goes on, without a note.
            "timeout": 5,
        }],
    }]);
    settings["hooks"] = json!({
        "PreToolUse": group,
        "PostToolUse": group,
        "PostToolUseFailure": group,
    });
    settings
}

/// Whether these settings let an agent read this folder and change nothing in it. Only then may
/// `--add-dir` name it.
pub fn is_read_only(settings: &Value, folder: &str) -> bool {
    let folder = folder.trim_end_matches('/');
    let listed = |path: &str, value: Value| {
        settings
            .pointer(path)
            .and_then(Value::as_array)
            .is_some_and(|list| list.contains(&value))
    };
    listed("/sandbox/filesystem/allowRead", json!(folder))
        && listed("/permissions/deny", json!(format!("Edit(/{folder}/**)")))
}

/// The same policy for a `check` step, which Keel runs itself. Tests and Makefiles can be edited
/// by an agent, so they get no more trust than the agent does. A setup that runs after an agent
/// that can write gets this too, for the same reason. A Seatbelt profile for `sandbox-exec`: no
/// home folder except what's listed, no network, and writes only here. An agent's Bash may reach
/// the project's allowed domains, but this can't be held to a list of domains, so it gets none.
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
/// Only for a setup before any agent of its run that can write, which runs what's published. An
/// agent that only reads has no way to change a file. Once one that can write has had the run's
/// copy, what a setup runs may be its doing, and it gets `check_profile`.
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
    // when one is refused. This says only that these folders exist, not what's in them. A setup
    // step's cache sits apart from its workspace, so the way to it needs the same.
    let to_cache = setup_cache
        .into_iter()
        .flat_map(|cache| cache.ancestors().skip(1));
    let mut folders: Vec<&Path> = Vec::new();
    for folder in workspace.ancestors().skip(1).chain(to_cache) {
        if folder.starts_with(home) && folder != home && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    let way_in: String = folders
        .iter()
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
    fn a_folder_only_read_is_readable_and_closed_to_changes() {
        let settings = with_read_only(
            step_settings(Path::new("/w"), &[], &[], &[]),
            &["/data/runs/r1/context/p2/".into()],
        );
        let allow = settings["sandbox"]["filesystem"]["allowRead"]
            .as_array()
            .unwrap();
        assert!(allow.contains(&json!("/data/runs/r1/context/p2")));
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(deny.contains(&json!("Edit(//data/runs/r1/context/p2/**)")));
        assert!(
            deny.contains(&json!("Edit(//w/.git/config)")),
            "what was denied stays denied"
        );
        assert!(is_read_only(&settings, "/data/runs/r1/context/p2"));
        assert!(!is_read_only(&settings, "/data/runs/r1/context/p3"));
        assert!(
            !is_read_only(
                &step_settings(
                    Path::new("/w"),
                    &[],
                    &["/data/runs/r1/context/p2".into()],
                    &[]
                ),
                "/data/runs/r1/context/p2"
            ),
            "readable isn't enough"
        );
    }

    #[test]
    fn the_bash_hook_watches_before_each_bash_command_and_notes_after_it() {
        let plain = step_settings(Path::new("/r/workspace"), &[], &[], &[]);
        assert!(plain.get("hooks").is_none());
        let settings = with_bash_hook(
            plain.clone(),
            &BashHook {
                program: Path::new("/h/.keel/bin/keel-hook-0123456789abcdef"),
                git: Path::new("/usr/bin/git"),
                workspace: Path::new("/r/workspace"),
                state: Path::new("/r/hook"),
                profile: Path::new("/r/hook/git.sb"),
            },
        );
        // Before, to note what git lists; after, whether the command worked or not, since one
        // that failed may have changed files first.
        let events: Vec<&String> = settings["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(events, ["PostToolUse", "PostToolUseFailure", "PreToolUse"]);
        for event in events {
            let group = &settings["hooks"][event][0];
            assert_eq!(group["matcher"], "Bash", "{event}");
            // Nothing that could block or decide: no `if`, no `onFailure`.
            assert_eq!(
                group["hooks"][0],
                json!({
                    "type": "command",
                    "command": "/h/.keel/bin/keel-hook-0123456789abcdef",
                    "args": ["/usr/bin/git", "/r/workspace", "/r/hook", "/r/hook/git.sb"],
                    "timeout": 5,
                }),
                "{event}"
            );
        }
        assert_eq!(settings["sandbox"], plain["sandbox"]);
        assert_eq!(settings["permissions"], plain["permissions"]);
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

    /// The folders a command may know exist. `with_own_output` adds a line of its own later on.
    fn way_in(profile: &str) -> &str {
        profile
            .lines()
            .find(|line| line.starts_with("(allow file-read-metadata"))
            .unwrap()
    }

    #[test]
    fn setup_gets_the_way_to_its_cache_and_a_check_does_not() {
        let home = Path::new("/h");
        let workspace = Path::new("/h/data/runs/r1/workspace");
        // Each folder once, though both paths go through `/h/data`, and never the cache itself:
        // that already has a rule of its own, with more in it.
        assert_eq!(
            way_in(&setup_profile(
                workspace,
                home,
                &[],
                &[],
                Path::new("/h/data/projects/p1/cache")
            )),
            "(allow file-read-metadata (literal \"/h\") (literal \"/h/data/runs/r1\") \
             (literal \"/h/data/runs\") (literal \"/h/data\") \
             (literal \"/h/data/projects/p1\") (literal \"/h/data/projects\"))"
        );
        assert_eq!(
            way_in(&check_profile(workspace, home, &[], &[])),
            "(allow file-read-metadata (literal \"/h\") (literal \"/h/data/runs/r1\") \
             (literal \"/h/data/runs\") (literal \"/h/data\"))"
        );
    }

    #[test]
    fn a_cache_outside_the_home_folder_adds_no_way_in() {
        let home = Path::new("/users/me");
        let workspace = Path::new("/users/me/w/workspace");
        let without = check_profile(workspace, home, &[], &[]);
        // `/users/me2` starts with the same letters, and isn't inside the home folder.
        for cache in ["/opt/keel/cache", "/users/me2/keel/cache"] {
            let with = setup_profile(workspace, home, &[], &[], Path::new(cache));
            assert_eq!(way_in(&with), way_in(&without), "{cache}");
        }
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
