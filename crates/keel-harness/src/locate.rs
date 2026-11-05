//! Finds `claude`, `gh` and `git`. An app opened from Finder doesn't inherit the terminal's
//! PATH, so a plain lookup would miss them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START: &str = "__KEEL_PATH_START__";
const END: &str = "__KEEL_PATH_END__";

#[derive(Debug, Clone, Default)]
pub struct Tools {
    pub claude: Option<PathBuf>,
    pub gh: Option<PathBuf>,
    pub git: Option<PathBuf>,
    /// The PATH agents and commands run with.
    pub path_env: String,
}

impl Tools {
    pub fn detect() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let mut dirs: Vec<PathBuf> = login_shell_path()
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        // Where these tools usually live, in case the shell gave us nothing.
        for extra in [".local/bin", ".claude/local", ".cargo/bin"] {
            dirs.push(home.join(extra));
        }
        for extra in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ] {
            dirs.push(PathBuf::from(extra));
        }
        let mut seen = Vec::new();
        dirs.retain(|d| {
            let new = !seen.contains(d);
            seen.push(d.clone());
            new
        });
        Self::from_dirs(&dirs)
    }

    pub fn from_dirs(dirs: &[PathBuf]) -> Self {
        let find = |name: &str| dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p));
        Self {
            claude: find("claude"),
            gh: find("gh"),
            git: find("git"),
            path_env: std::env::join_paths(dirs)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Asks the user's login shell for its PATH. Interactive too, since many people set PATH in
/// `.zshrc`. A shell that hangs or prints noise must not break the app, so this has a deadline
/// and looks for markers.
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '{START}%s{END}' \"$PATH\"");
    let mut child = Command::new(shell)
        .args(["-lic", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(30)),
        }
    }
    let output = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let start = text.find(START)? + START.len();
    let end = text[start..].find(END)? + start;
    Some(text[start..end].to_owned())
}

use std::os::unix::process::CommandExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_executables() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::write(&claude, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("gh"), "not executable").unwrap();
        let tools = Tools::from_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(tools.claude.as_deref(), Some(claude.as_path()));
        assert!(tools.gh.is_none());
    }
}
