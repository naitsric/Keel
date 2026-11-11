//! Where the program Claude Code runs around a builder's Bash commands lives (`crates/keel-hook`).
//! The app carries it inside itself and writes it out at startup, named after its content: a
//! settings file then names one build, and a step can never run an older one under that name.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

/// A build goes once nothing has used it for this long. Every Keel that installs it or names it in
/// a step's settings marks it as used, so another Keel running a different build leaves it alone.
const KEEP_OLD: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Marks a build as in use, so another Keel's `install` doesn't take it for an old one. `false`
/// when it's gone.
pub fn touch(path: &Path) -> bool {
    std::fs::File::open(path)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .is_ok()
}

/// Writes `program` into `dir`, unless it's already there, and returns its path. Other builds
/// unused for a week go.
pub fn install(dir: &Path, program: &[u8]) -> io::Result<PathBuf> {
    let digest = Sha256::digest(program);
    let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("keel-hook-{name}"));
    // Keel's own caution, not a limit anyone has seen: the docs take an absolute path with spaces
    // in exec form, but no hook has been tried from one, and `~/.keel` has none.
    if path.to_string_lossy().contains(char::is_whitespace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the hook's path has spaces",
        ));
    }
    prune(dir, &path);
    if std::fs::read(&path).is_ok_and(|there| there == program) {
        // Something may have taken away its right to run since it was written.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        touch(&path);
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    // Written whole, then moved into place, so a step never finds half a program.
    let partial = dir.join(format!(".keel-hook-{name}.{}", std::process::id()));
    std::fs::write(&partial, program)?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// Removes other builds, and anything half written, once unused for a week. Only names this
/// module writes.
fn prune(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let ours = name.to_string_lossy().starts_with("keel-hook-")
            || name.to_string_lossy().starts_with(".keel-hook-");
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_OLD);
        if ours && old && path != current {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Runs the hook once with nothing to do. macOS looks a new program over the first time it runs,
/// which took from seconds to minutes on a busy machine, far longer than a step gives the hook.
/// Better here, off the main thread, than in a step. Once looked over, it starts in milliseconds.
pub fn warm_up(path: &Path) {
    let _ = std::process::Command::new(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_build_gets_its_own_path_and_an_existing_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let first = install(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(first.starts_with(&bin));
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("keel-hook-")
        );
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(install(&bin, b"#!/bin/sh\nexit 0\n").unwrap(), first);
        // Kept, and able to run again if something took that away.
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(install(&bin, b"#!/bin/sh\nexit 0\n").unwrap(), first);
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let second = install(&bin, b"#!/bin/sh\nexit 1\n").unwrap();
        assert_ne!(second, first);
        assert!(first.exists(), "a step still running may use the old one");
        let names: Vec<_> = std::fs::read_dir(&bin).unwrap().collect();
        assert_eq!(names.len(), 2, "nothing half written is left behind");
    }

    #[test]
    fn warming_up_runs_the_hook_once_with_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("ran");
        let hook = install(
            &dir.path().join("bin"),
            format!("#!/bin/sh\n[ $# -eq 0 ] && touch {}\n", mark.display()).as_bytes(),
        )
        .unwrap();
        warm_up(&hook);
        assert!(mark.exists());
        // One that can't start is no reason to stop.
        warm_up(&dir.path().join("missing"));
    }

    fn age(path: &Path, days: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60))
            .unwrap();
    }

    fn days_unused(path: &Path) -> u64 {
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs()
            / (24 * 60 * 60)
    }

    #[test]
    fn builds_unused_for_a_week_go_and_the_one_installed_is_marked_used() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let current = install(&bin, b"current").unwrap();
        let recent = install(&bin, b"recent").unwrap();
        let old = install(&bin, b"old").unwrap();
        let half = bin.join(".keel-hook-0011.123");
        let theirs = bin.join("someone-else");
        std::fs::write(&half, b"half").unwrap();
        std::fs::write(&theirs, b"theirs").unwrap();
        for path in [&current, &old, &half, &theirs] {
            age(path, 30);
        }
        age(&recent, 2);
        assert_eq!(install(&bin, b"current").unwrap(), current);
        assert_eq!(
            days_unused(&current),
            0,
            "installing it again marks it as used"
        );
        assert!(recent.exists(), "another Keel may still be using it");
        assert!(!old.exists() && !half.exists());
        assert!(theirs.exists(), "only the hook's own files go");
    }

    #[test]
    fn a_build_a_step_names_is_marked_used_and_one_that_is_gone_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let hook = install(&bin, b"in use").unwrap();
        age(&hook, 30);
        // Read-only, as a program that's running may as well be: marking it needs no writing.
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(touch(&hook));
        assert_eq!(days_unused(&hook), 0);
        // Another Keel's `install` now leaves it alone.
        install(&bin, b"another build").unwrap();
        assert!(hook.exists());
        assert!(!touch(&bin.join("keel-hook-gone")));
    }

    #[test]
    fn a_path_with_spaces_gets_no_hook() {
        let dir = tempfile::tempdir().unwrap();
        assert!(install(&dir.path().join("Application Support"), b"x").is_err());
    }
}
