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

/// The variables that put a command in `home`: the home folder itself, where tools keep their
/// settings, caches, data and state, git's settings, and the toolchains in `real_home`.
pub fn env(home: &Path, real_home: &Path) -> Vec<(String, String)> {
    let inside = |folder: &str| home.join(folder).to_string_lossy().into_owned();
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
    env.insert(0, ("HOME".to_owned(), home.to_string_lossy().into_owned()));
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

/// `env` as a script, one `export` a line, for a process that has to keep the real home folder and
/// runs it ahead of each command it starts: an agent's `claude`, whose login and sessions are there.
pub fn script(home: &Path, real_home: &Path) -> String {
    env(home, real_home)
        .iter()
        .map(|(name, value)| format!("export {name}='{}'\n", value.replace('\'', r"'\''")))
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
