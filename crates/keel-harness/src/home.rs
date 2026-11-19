//! A home folder of the project's own, for the commands Keel runs and those an agent runs in Bash.
//! Tools keep what they download in the home folder, and read their settings there: `~/.m2`,
//! `~/.gradle`, `~/.cache`, `~/.config/git/ignore`. The sandbox hides the real one, so setup fills one
//! in the project's cache, with the network, and each run gets a copy, where its steps work. What a
//! step writes there reaches neither the project's home nor the next run. Toolchains stay in the real
//! home folder, only to be read, and a few variables say where.

use std::io;
use std::path::Path;

/// The home's folder, in the project's cache and in the run's.
pub const FOLDER: &str = "home";

/// Where version managers keep the toolchains they installed, which they'd look for in the home
/// folder. They stay in the real one, where the sandbox at most lets them be read.
const TOOLCHAINS: [(&str, &str); 8] = [
    ("RUSTUP_HOME", ".rustup"),
    ("UV_PYTHON_INSTALL_DIR", ".local/share/uv/python"),
    ("MISE_DATA_DIR", ".local/share/mise"),
    ("ASDF_DATA_DIR", ".asdf"),
    ("NVM_DIR", ".nvm"),
    ("PYENV_ROOT", ".pyenv"),
    ("RBENV_ROOT", ".rbenv"),
    ("VOLTA_HOME", ".volta"),
];

/// Git's own settings in every home. Test suites make repos and commit to them, and git has to know
/// who from. The run's clone says who in its own settings, which win over these.
const GIT_SETTINGS: &str = "[user]\n\tname = Keel\n\temail = keel@localhost\n";

/// `path` as one of Java's options reads it: split at spaces, with quotes and no escapes. So it goes
/// in double quotes, and each double quote in it in single ones.
fn java_quoted(path: &str) -> String {
    format!("\"{}\"", path.replace('"', r#""'"'""#))
}

/// The variables that put a command in `home`: the home folder itself, where tools keep their
/// settings, caches, data and state, git's settings, and the toolchains in `real_home`. Two
/// runtimes ask the system for the home folder and not `HOME`, and each is told on its own: Java,
/// for Maven, Gradle and every other tool on it, and Foundation, for Swift's.
pub fn env(home: &Path, real_home: &Path) -> Vec<(String, String)> {
    let inside = |folder: &str| home.join(folder).to_string_lossy().into_owned();
    let home_text = home.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_STATE_HOME", ".local/state"),
        ("GIT_CONFIG_GLOBAL", ".gitconfig"),
    ]
    .iter()
    .map(|(name, folder)| ((*name).to_owned(), inside(folder)))
    .collect();
    env.insert(0, ("HOME".to_owned(), home_text.clone()));
    env.push((
        "JAVA_TOOL_OPTIONS".to_owned(),
        format!("-Duser.home={}", java_quoted(&home_text)),
    ));
    env.push(("CFFIXED_USER_HOME".to_owned(), home_text));
    if real_home.is_absolute() {
        env.extend(TOOLCHAINS.iter().map(|(name, folder)| {
            (
                (*name).to_owned(),
                real_home.join(folder).to_string_lossy().into_owned(),
            )
        }));
    }
    env
}

/// Claude Code gives the shell of each Bash command a pipe to read from that nobody writes to, and
/// points the command at `/dev/null` instead only when it has no `<` or `<<` of its own. One that
/// has, and reads what it wasn't given, such as a `head` with no file or `git checkout -p`, would
/// wait on that pipe for as long as its time limit. The script runs in that shell ahead of the
/// command, so this closes it for all of them.
const NOTHING_TO_READ: &str = "exec </dev/null\n";

/// `env` as a script, one `export` a line, for a process that has to keep the real home folder and
/// runs it ahead of each command it starts: an agent's `claude`, whose login and sessions are there.
/// It ends by leaving the command nothing to read: see `NOTHING_TO_READ`.
pub fn script(home: &Path, real_home: &Path) -> String {
    env(home, real_home)
        .iter()
        .map(|(name, value)| format!("export {name}='{}'\n", value.replace('\'', r"'\''")))
        .chain(std::iter::once(NOTHING_TO_READ.to_owned()))
        .collect()
}

/// Makes a home, with git's settings in it, unless it's there. Settings a command changed stay.
pub fn create(home: &Path) -> io::Result<()> {
    std::fs::create_dir_all(home)?;
    let git = home.join(".gitconfig");
    if git.symlink_metadata().is_err() {
        std::fs::write(git, GIT_SETTINGS)?;
    }
    Ok(())
}

/// Gives a run its own copy of the project's home, once: another step of the same run keeps what the
/// run has. It's a clone on APFS, which costs almost nothing, and a copy where that's not possible.
/// Never a link, so nothing a step writes can reach the project's home. A project that has none yet
/// gives the run an empty one.
pub fn copy(project_home: &Path, run_home: &Path) -> io::Result<()> {
    if run_home.exists() {
        return Ok(());
    }
    if project_home.is_dir() {
        if let Some(parent) = run_home.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let copy = |flags: &str| {
            std::process::Command::new("/bin/cp")
                .arg(flags)
                .arg(project_home)
                .arg(run_home)
                .status()
                .is_ok_and(|status| status.success())
        };
        if !copy("-cR") {
            let _ = std::fs::remove_dir_all(run_home);
            if !copy("-R") {
                let _ = std::fs::remove_dir_all(run_home);
                return Err(io::Error::other(format!(
                    "could not copy {}",
                    project_home.display()
                )));
            }
        }
    }
    create(run_home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn what_tools_keep_goes_in_the_home_and_toolchains_stay_where_they_are() {
        let env = env(Path::new("/r/w/.keel-cache/home"), Path::new("/Users/me"));
        assert_eq!(var(&env, "HOME"), Some("/r/w/.keel-cache/home"));
        for (name, folder) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_STATE_HOME", ".local/state"),
            ("GIT_CONFIG_GLOBAL", ".gitconfig"),
        ] {
            assert_eq!(
                var(&env, name),
                Some(format!("/r/w/.keel-cache/home/{folder}").as_str()),
                "{name}"
            );
        }
        // rustup's `cargo` finds its toolchains through this, whatever the home folder is.
        assert_eq!(var(&env, "RUSTUP_HOME"), Some("/Users/me/.rustup"));
        // Nothing else points at the real home folder, so nothing writes there.
        for (name, value) in &env {
            if value.starts_with("/Users/me") {
                assert!(
                    TOOLCHAINS.iter().any(|(toolchain, _)| toolchain == name),
                    "{name}"
                );
            }
        }
        // Without a real home folder there are no toolchains in it.
        let none = super::env(Path::new("/h"), Path::new(""));
        assert_eq!(var(&none, "RUSTUP_HOME"), None);
    }

    #[test]
    fn the_script_sets_what_env_does_in_any_shell_whatever_the_path() {
        // As old runs have it, and with what would end a quote.
        let home = Path::new("/Users/me/Library/Application Support/it's $HOME/.keel-cache/home");
        let real_home = Path::new("/Users/me");
        let env = env(home, real_home);
        let print: String = env
            .iter()
            .map(|(name, _)| format!("printf '%s\\n' \"${name}\"\n"))
            .collect();
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            let output = std::process::Command::new(shell)
                .arg("-c")
                .arg(format!("{}{print}", script(home, real_home)))
                .env_clear()
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}");
            let said = String::from_utf8(output.stdout).unwrap();
            let values: Vec<&str> = env.iter().map(|(_, value)| value.as_str()).collect();
            assert_eq!(said.lines().collect::<Vec<_>>(), values, "{shell}");
        }
    }

    /// As Claude Code starts a Bash command: the shell reads from a pipe that stays open, with
    /// nothing ever written to it. A builder's `head` with no file waited there until a person
    /// stepped in.
    #[test]
    fn after_the_script_a_command_that_reads_what_nobody_writes_ends_at_once() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let script = script(Path::new("/r/w/.keel-cache/home"), Path::new("/Users/me"));
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            let mut child = Command::new(shell)
                .arg("-c")
                .arg(format!("{script}/usr/bin/head -1; echo \"read=$?\""))
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            // Held open, as Claude Code holds it.
            let pipe = child.stdin.take();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("{shell}: the command waited for something to read");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(pipe);
            let said = child.wait_with_output().unwrap().stdout;
            assert_eq!(String::from_utf8_lossy(&said), "read=0\n", "{shell}");
        }
    }

    /// A home folder like a project's, with what would end a quote in each of the ways it's written.
    fn awkward_home(dir: &Path) -> std::path::PathBuf {
        let home = dir.join("Application Support/it's \"here\"/home");
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// Java takes its home folder from the system and not from `HOME` (JDK-8280357), so Maven and
    /// Gradle would keep what they download in the real one, which the sandbox refuses. So would
    /// Swift and anything else built on Foundation.
    #[test]
    fn java_and_foundation_are_told_where_the_home_is() {
        let home = Path::new("/h/Application Support/it's \"here\"/home");
        let env = env(home, Path::new("/Users/me"));
        assert_eq!(var(&env, "CFFIXED_USER_HOME"), Some(home.to_str().unwrap()));
        // Java splits the variable at spaces, and reads quotes, with no way to escape one.
        assert_eq!(
            var(&env, "JAVA_TOOL_OPTIONS"),
            Some(r#"-Duser.home="/h/Application Support/it's "'"'"here"'"'"/home""#)
        );
    }

    /// Where a JDK is, if this Mac has one: `/usr/bin/java` is only a stub without one.
    fn java() -> Option<std::path::PathBuf> {
        let registered = std::process::Command::new("/usr/libexec/java_home")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        registered
            .into_iter()
            .map(|jdk| Path::new(&jdk).join("bin/java"))
            .chain(
                [
                    "/opt/homebrew/opt/openjdk/bin/java",
                    "/usr/local/opt/openjdk/bin/java",
                ]
                .map(std::path::PathBuf::from),
            )
            .find(|java| java.is_file())
    }

    #[test]
    fn java_finds_its_home_folder_in_the_home_whatever_the_path() {
        let Some(java) = java() else {
            crate::testing::skipped("there's no Java on this Mac");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let home = awkward_home(dir.path());
        let output = std::process::Command::new(java)
            .args(["-XshowSettings:properties", "-version"])
            .env_clear()
            .envs(env(&home, Path::new("/Users/me")))
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{said}");
        let user_home = said
            .lines()
            .find_map(|line| line.trim().strip_prefix("user.home = "))
            .expect(&said);
        assert_eq!(user_home, home.to_str().unwrap(), "{said}");
    }

    #[test]
    fn foundation_finds_its_home_folder_in_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = awkward_home(dir.path());
        let output = std::process::Command::new("/usr/bin/osascript")
            .args([
                "-l",
                "JavaScript",
                "-e",
                "ObjC.import('Foundation'); $.NSHomeDirectory().js",
            ])
            .env_clear()
            .envs(env(&home, Path::new("/Users/me")))
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(said.trim(), home.to_str().unwrap());
    }

    #[test]
    fn a_home_has_git_settings_and_keeps_those_a_command_changed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        create(&home).unwrap();
        let git = home.join(".gitconfig");
        assert_eq!(std::fs::read_to_string(&git).unwrap(), GIT_SETTINGS);
        std::fs::write(&git, "[user]\n\tname = Theirs\n").unwrap();
        create(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(&git).unwrap(),
            "[user]\n\tname = Theirs\n"
        );
    }

    #[test]
    fn a_run_copies_the_home_once_and_never_writes_to_the_projects() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("cache/home");
        create(&project).unwrap();
        std::fs::create_dir_all(project.join(".cargo/registry")).unwrap();
        std::fs::write(project.join(".cargo/registry/dep"), "x").unwrap();
        let run = dir.path().join("workspace/.keel-cache/home");
        copy(&project, &run).unwrap();
        assert!(run.join(".cargo/registry/dep").exists());
        assert!(run.join(".gitconfig").exists());
        std::fs::write(run.join(".cargo/registry/mine"), "y").unwrap();
        std::fs::write(run.join(".cargo/registry/dep"), "changed").unwrap();
        // Another step of the same run keeps what the run has.
        copy(&project, &run).unwrap();
        assert!(run.join(".cargo/registry/mine").exists());
        assert!(!project.join(".cargo/registry/mine").exists());
        assert_eq!(
            std::fs::read_to_string(project.join(".cargo/registry/dep")).unwrap(),
            "x"
        );
    }

    #[test]
    fn a_run_of_a_project_without_setup_starts_with_an_empty_home() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("workspace/.keel-cache/home");
        copy(&dir.path().join("cache/home"), &run).unwrap();
        assert!(run.join(".gitconfig").exists());
        assert!(!dir.path().join("cache").exists());
    }
}
