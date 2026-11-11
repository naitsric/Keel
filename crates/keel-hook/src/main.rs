//! What Claude Code runs around each Bash command of a Keel step that can edit. Builders changed
//! source files from Bash even when told to use Edit and Write, and a replacement from Bash that
//! matches nothing changes nothing and says nothing.
//!
//! Before the command it only watches: it notes what `git status` lists and says nothing, so the
//! command goes on exactly as it would have. After it, it looks again, and the agent gets a short
//! note when project files changed, or when a command that looks like an edit changed none.
//!
//! It has no dependencies, so the app's build script compiles it with plain `rustc` and the app
//! installs it apart from itself (see `src-tauri/build.rs`). Whatever goes wrong, it says nothing
//! and exits 0.

mod git;
mod json;
mod note;
mod snapshot;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use git::Git;
use snapshot::Limits;

/// The most of the event read from stdin. A Bash result can be long; past this, no note.
const MOST_INPUT: u64 = 8 << 20;
/// A snapshot whose command never came back, like one that was stopped, goes after this.
const STALE: Duration = Duration::from_secs(6 * 60 * 60);

/// What Keel put in the step's settings: its git, the project, a folder of the run's that the
/// sandbox can't write to, for the snapshots, and the sandbox profile for git.
struct Paths {
    git: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    profile: PathBuf,
}

fn main() {
    // No message and no crash report: the agent carries on either way.
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // Without them there's nothing to do. The app runs it that way once, to have macOS look a new
    // program over before a step waits on it.
    let [git, workspace, state, profile] = args.as_slice() else {
        return;
    };
    let paths = Paths {
        git: git.into(),
        workspace: workspace.into(),
        state: state.into(),
        profile: profile.into(),
    };
    let answer = std::panic::catch_unwind(move || {
        let mut input = String::new();
        std::io::stdin()
            .take(MOST_INPUT)
            .read_to_string(&mut input)
            .ok()?;
        answer(&input, &paths, &Limits::DEFAULT)
    });
    if let Ok(Some(output)) = answer {
        let _ = std::io::stdout().write_all(output.as_bytes());
    }
}

/// The JSON for Claude Code, given the event it sent, or `None` for nothing at all.
fn answer(input: &str, paths: &Paths, limits: &Limits) -> Option<String> {
    let deadline = Instant::now() + limits.time;
    let event = json::parse(input)?;
    if event
        .get("tool_name")
        .and_then(json::Value::as_str)
        .is_some_and(|tool| tool != "Bash")
    {
        return None;
    }
    let name = event.get("hook_event_name")?.as_str()?;
    let id = key(event.get("tool_use_id")?.as_str()?)?;
    let file = paths.state.join(format!("{id}.snap"));
    // A command sent to the background has only started when the call returns.
    let input = event.get("tool_input");
    if input
        .and_then(|input| input.get("run_in_background"))
        .and_then(json::Value::as_bool)
        == Some(true)
    {
        return None;
    }
    let git = Git {
        bin: &paths.git,
        workspace: &paths.workspace,
        profile: &paths.profile,
    };
    match name {
        "PreToolUse" => {
            // Only watching: no output at all, so permissions go as they would have.
            prune(&paths.state);
            let snapshot = snapshot::take(&git, limits, deadline)?;
            store(&paths.state, &file, &snapshot::encode(&snapshot));
            None
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let before = std::fs::read(&file).ok();
            let _ = std::fs::remove_file(&file);
            let before = snapshot::decode(&before?)?;
            let after = snapshot::take(&git, limits, deadline)?;
            let changes = snapshot::compare(&before, &after, &git, limits, deadline);
            let command = input
                .and_then(|input| input.get("command"))
                .and_then(json::Value::as_str)
                .unwrap_or("");
            // A short commit id is enough for the agent to type, and plenty to be unique.
            let base = match &before.head {
                Some(head) if changes.head_moved => head.chars().take(12).collect(),
                _ => "HEAD".to_owned(),
            };
            let text = note::note(&changes, note::looks_like_edit(command), &base)?;
            Some(format!(
                r#"{{"hookSpecificOutput":{{"hookEventName":{},"additionalContext":{}}}}}"#,
                json::quote(name),
                json::quote(&text),
            ))
        }
        _ => None,
    }
}

/// Claude Code's id for the tool call, as a file name, or `None` when it has anything else in it.
fn key(id: &str) -> Option<&str> {
    let plain = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (plain && !id.is_empty() && id.len() <= 128).then_some(id)
}

fn store(state: &Path, file: &Path, bytes: &[u8]) {
    if std::fs::create_dir_all(state).is_err() {
        return;
    }
    // Written whole, then moved into place, so the call after never reads half a snapshot.
    let partial = file.with_extension(format!("snap.{}", std::process::id()));
    if std::fs::write(&partial, bytes).is_ok() {
        let _ = std::fs::rename(&partial, file);
    }
}

fn prune(state: &Path) {
    let Ok(entries) = std::fs::read_dir(state) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE);
        if stale && entry.file_name().to_string_lossy().contains(".snap") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// An absolute git, the way Keel passes it.
    fn git_bin() -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|dir| dir.join("git"))
            .find(|git| git.is_absolute() && git.is_file())
            .expect("git on PATH")
    }

    struct Repo {
        dir: tempfile::TempDir,
        run: tempfile::TempDir,
        git: PathBuf,
    }

    impl Repo {
        fn new(files: &[(&str, &str)]) -> Self {
            let repo = Self {
                dir: tempfile::tempdir().unwrap(),
                run: tempfile::tempdir().unwrap(),
                git: git_bin(),
            };
            repo.git_cmd(&["init", "-q", "-b", "main"]);
            for (path, text) in files {
                repo.write(path, text);
            }
            repo.git_cmd(&["add", "-A"]);
            repo.git_cmd(&["commit", "-qm", "start"]);
            repo.sandbox("(version 1)\n(allow default)\n(deny network*)\n");
            repo
        }

        fn sandbox(&self, profile: &str) {
            std::fs::write(self.run.path().join("git.sb"), profile).unwrap();
        }

        fn ws(&self) -> &Path {
            self.dir.path()
        }

        fn git_cmd(&self, args: &[&str]) {
            let ok = Command::new(&self.git)
                .current_dir(self.ws())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        }

        fn head(&self) -> String {
            let output = Command::new(&self.git)
                .current_dir(self.ws())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn write(&self, path: &str, text: &str) {
            let full = self.ws().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }

        fn paths(&self) -> Paths {
            Paths {
                git: self.git.clone(),
                workspace: self.ws().to_path_buf(),
                state: self.run.path().join("hook"),
                profile: self.run.path().join("git.sb"),
            }
        }

        fn send(&self, event: &str, id: &str, command: &str, limits: &Limits) -> Option<String> {
            let input = format!(
                r#"{{"session_id":"s","hook_event_name":{},"tool_name":"Bash","tool_use_id":{},"tool_input":{{"command":{},"description":"x"}},"tool_response":{{"stdout":"","stderr":""}},"cwd":"/somewhere/else"}}"#,
                json::quote(event),
                json::quote(id),
                json::quote(command),
            );
            answer(&input, &self.paths(), limits)
        }

        /// The note for `command`, which `change` stands in for.
        fn around(&self, command: &str, change: impl FnOnce(&Repo)) -> Option<String> {
            self.around_with(command, "PostToolUse", &Limits::DEFAULT, change)
        }

        fn around_with(
            &self,
            command: &str,
            after: &str,
            limits: &Limits,
            change: impl FnOnce(&Repo),
        ) -> Option<String> {
            assert_eq!(self.send("PreToolUse", "toolu_1", command, limits), None);
            change(self);
            let output = self.send(after, "toolu_1", command, limits)?;
            let value = json::parse(&output).expect("valid JSON");
            let specific = value.get("hookSpecificOutput").unwrap();
            assert_eq!(
                specific.get("hookEventName").and_then(json::Value::as_str),
                Some(after)
            );
            Some(
                specific
                    .get("additionalContext")
                    .and_then(json::Value::as_str)
                    .unwrap()
                    .to_owned(),
            )
        }

        /// What the `git diff` the note suggests prints, run by a shell from `folder`, the way the
        /// agent would run it after a `cd`.
        fn suggested_diff(&self, note: &str, folder: &str) -> String {
            let start = note.find("`git diff ").expect("a git diff to run") + 1;
            let length = note[start..].find('`').unwrap();
            let command = &note[start..start + length];
            let git_dir = self.git.parent().unwrap().display().to_string();
            let out = Command::new("/bin/sh")
                .args(["-c", command])
                .current_dir(self.ws().join(folder))
                .env("PATH", format!("{git_dir}:/usr/bin:/bin"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_PAGER", "cat")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    }

    fn start() -> Repo {
        Repo::new(&[
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("src/lib.rs", "fn a() {}\n"),
            (".gitignore", "target/\n"),
        ])
    }

    const HINT: &str = "Edit and Write fail out loud when a replacement matches nothing; an edit \
                        from Bash says nothing.";

    const NONE_WATCHED: &str = "This Bash command changed none of the files git watches here; \
                                ignored files, submodules and nested repositories aren't \
                                watched.";

    #[test]
    fn modified_new_deleted_and_renamed_files_are_named() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around("sed -i '' 's/a/x/' a.txt", |repo| {
                repo.write("a.txt", "x\n");
                repo.write("new.txt", "n\n");
                std::fs::remove_file(repo.ws().join("src/lib.rs")).unwrap();
                std::fs::rename(repo.ws().join("b.txt"), repo.ws().join("c.txt")).unwrap();
            })
            .expect("a note");
        assert_eq!(
            note,
            format!(
                "While this Bash command ran, files in the project changed (modified: 'a.txt'; \
                 new: 'c.txt', 'new.txt'; deleted: 'b.txt', 'src/lib.rs'). Check them with \
                 `git diff HEAD -- ':(top,literal)a.txt' ':(top,literal)b.txt' \
                 ':(top,literal)src/lib.rs'` and `git status --short`. {HINT}"
            )
        );
    }

    #[test]
    fn only_what_changed_during_the_command_is_named() {
        keel_harness::needs!(sandbox);
        let repo = start();
        // Both already differ from the commit, and only one changes again.
        repo.write("a.txt", "a2\n");
        repo.write("b.txt", "b2\n");
        repo.write("untracked.txt", "u\n");
        let note = repo
            .around("python3 fix.py", |repo| {
                repo.write("a.txt", "a3\n");
                repo.write("untracked.txt", "u2\n");
            })
            .expect("a note");
        assert!(
            note.contains("(modified: 'a.txt', 'untracked.txt')"),
            "{note}"
        );
        assert!(
            note.contains("`git diff HEAD -- ':(top,literal)a.txt'` and `git status --short`"),
            "{note}"
        );
        assert!(!note.contains("b.txt"), "{note}");
    }

    #[test]
    fn the_suggested_diff_shows_the_change_from_any_folder_staged_or_not() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around("sed -i '' 's/a/x/' a.txt", |repo| {
                repo.write("a.txt", "x\n")
            })
            .expect("a note");
        assert!(repo.suggested_diff(&note, "src").contains("+x"), "{note}");
        let note = repo
            .around("sed -i '' 's/b/y/' b.txt && git add b.txt", |repo| {
                repo.write("b.txt", "y\n");
                repo.git_cmd(&["add", "b.txt"]);
            })
            .expect("a note");
        assert!(repo.suggested_diff(&note, "src").contains("+y"), "{note}");
    }

    #[test]
    fn an_edit_committed_in_the_same_command_is_named() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around("sed -i '' 's/a/x/' a.txt && git commit -qam fix", |repo| {
                repo.write("a.txt", "x\n");
                repo.git_cmd(&["commit", "-qam", "fix"]);
            })
            .expect("a note");
        assert!(note.contains("(modified: 'a.txt')"), "{note}");
        // Against the commit from before, since `HEAD` now has the change.
        assert!(!note.contains("git diff HEAD"), "{note}");
        let diff = repo.suggested_diff(&note, "src");
        assert!(diff.contains("-a") && diff.contains("+x"), "{diff}");
    }

    #[test]
    fn a_checkout_names_what_it_changed() {
        keel_harness::needs!(sandbox);
        let repo = start();
        repo.git_cmd(&["checkout", "-qb", "other"]);
        repo.write("b.txt", "other\n");
        repo.git_cmd(&["commit", "-qam", "other"]);
        repo.git_cmd(&["checkout", "-q", "main"]);
        let note = repo
            .around("sed -i '' 's/q/w/' a.txt; git checkout -q other", |repo| {
                repo.git_cmd(&["checkout", "-q", "other"])
            })
            .expect("a note");
        assert!(note.contains("(modified: 'b.txt')"), "{note}");
        assert!(
            repo.suggested_diff(&note, "src").contains("+other"),
            "{note}"
        );
    }

    #[test]
    fn a_moved_commit_never_claims_that_nothing_changed() {
        keel_harness::needs!(sandbox);
        let repo = start();
        repo.write("a.txt", "a2\n");
        // An edit that matched nothing, then a commit of what was already there.
        assert_eq!(
            repo.around("sed -i '' 's/zzz/y/' a.txt && git commit -qam x", |repo| {
                repo.git_cmd(&["commit", "-qam", "x"])
            }),
            None
        );
    }

    #[test]
    fn a_commit_or_a_command_that_writes_nothing_gets_no_note() {
        keel_harness::needs!(sandbox);
        let repo = start();
        repo.write("a.txt", "a2\n");
        assert_eq!(
            repo.around("git commit -am done", |repo| repo
                .git_cmd(&["commit", "-qam", "done"])),
            None
        );
        assert_eq!(repo.around("ls -la", |_| {}), None);
        assert_eq!(repo.around("cargo test", |_| {}), None);
        assert_eq!(
            repo.around("python3 -m pytest tests/test_x.py", |_| {}),
            None
        );
    }

    #[test]
    fn an_edit_that_changed_nothing_says_so_and_only_what_was_watched() {
        keel_harness::needs!(sandbox);
        let repo = start();
        assert_eq!(
            repo.around("sed -i '' 's/zzz/yyy/' a.txt", |_| {}),
            Some(format!("{NONE_WATCHED} {HINT}"))
        );
        assert!(
            repo.around("python3 - <<'EOF'\nopen('a.txt', 'w')\nEOF", |_| {})
                .is_some()
        );
    }

    #[test]
    fn files_git_ignores_are_never_named() {
        keel_harness::needs!(sandbox);
        let repo = start();
        repo.write("target/old.log", "x\n");
        assert_eq!(
            repo.around("make", |repo| repo.write("target/old.log", "y\n")),
            None
        );
        // Ignored, so not watched, and the note says only that.
        assert_eq!(
            repo.around("cargo build > target/build.log", |repo| {
                repo.write("target/build.log", "z\n")
            }),
            Some(format!("{NONE_WATCHED} {HINT}"))
        );
    }

    #[test]
    fn a_link_is_known_by_where_it_points_and_never_followed() {
        keel_harness::needs!(sandbox);
        let repo = Repo::new(&[("a.txt", "a\n"), ("d/f.txt", "inside\n")]);
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "one\n").unwrap();
        std::os::unix::fs::symlink(&secret, repo.ws().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.ws().join("dirlink")).unwrap();
        assert_eq!(
            repo.around("ls", |_| std::fs::write(&secret, "two\n").unwrap()),
            None
        );
        let note = repo
            .around("ln -sf /etc/hosts link", |repo| {
                std::fs::remove_file(repo.ws().join("link")).unwrap();
                std::os::unix::fs::symlink("/etc/hosts", repo.ws().join("link")).unwrap();
            })
            .expect("the link changed");
        assert!(note.contains("modified: 'link'"), "{note}");
        // A folder swapped for a link to another `f.txt` leads nowhere: that file is gone.
        std::fs::write(outside.path().join("f.txt"), "outside\n").unwrap();
        let note = repo
            .around("ls", |repo| {
                std::fs::remove_dir_all(repo.ws().join("d")).unwrap();
                std::os::unix::fs::symlink(outside.path(), repo.ws().join("d")).unwrap();
            })
            .expect("the folder became a link");
        assert!(note.contains("deleted: 'd/f.txt'"), "{note}");
        assert!(!note.contains("modified: 'd/f.txt'"), "{note}");
    }

    #[test]
    fn paths_are_quoted_for_the_shell_and_the_diff_still_finds_them() {
        keel_harness::needs!(sandbox);
        let repo = Repo::new(&[("app/[id]/page.tsx", "a\n"), ("app/it's.txt", "a\n")]);
        let note = repo
            .around("node gen.js", |repo| {
                repo.write("app/[id]/page.tsx", "b\n");
                repo.write("app/it's.txt", "c\n");
                repo.write("a b.txt", "n\n");
            })
            .expect("a note");
        assert!(
            note.contains(
                r"`git diff HEAD -- ':(top,literal)app/[id]/page.tsx' ':(top,literal)app/it'\''s.txt'`"
            ),
            "{note}"
        );
        assert!(note.contains("new: 'a b.txt'"), "{note}");
        let diff = repo.suggested_diff(&note, "app/[id]");
        assert!(diff.contains("+b") && diff.contains("+c"), "{diff}");
    }

    #[test]
    fn a_file_git_stops_tracking_is_named_once() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around("git rm -q --cached a.txt", |repo| {
                repo.git_cmd(&["rm", "-q", "--cached", "a.txt"])
            })
            .expect("a note");
        assert!(note.contains("(no longer tracked: 'a.txt')"), "{note}");
        assert!(
            !note.contains("new:") && !note.contains("deleted:"),
            "{note}"
        );
    }

    #[test]
    fn a_failed_command_gets_its_note_as_a_failure() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around_with(
                "sed -i 's/a/b/' a.txt && cargo test",
                "PostToolUseFailure",
                &Limits::DEFAULT,
                |repo| repo.write("a.txt", "b\n"),
            )
            .expect("a note");
        assert!(note.contains("modified: 'a.txt'"), "{note}");
    }

    #[test]
    fn a_command_sent_to_the_background_is_left_alone() {
        let repo = start();
        let input = |event: &str| {
            format!(
                r#"{{"hook_event_name":"{event}","tool_name":"Bash","tool_use_id":"toolu_bg","tool_input":{{"command":"sed -i '' s/a/b/ a.txt","run_in_background":true}}}}"#
            )
        };
        assert_eq!(
            answer(&input("PreToolUse"), &repo.paths(), &Limits::DEFAULT),
            None
        );
        assert!(!repo.run.path().join("hook/toolu_bg.snap").exists());
        assert_eq!(
            answer(&input("PostToolUse"), &repo.paths(), &Limits::DEFAULT),
            None
        );
    }

    #[test]
    fn without_a_snapshot_from_before_there_is_nothing_to_say() {
        let repo = start();
        repo.write("a.txt", "changed\n");
        assert_eq!(
            repo.send(
                "PostToolUse",
                "toolu_9",
                "sed -i '' 's/a/b/' a.txt",
                &Limits::DEFAULT
            ),
            None
        );
    }

    #[test]
    fn no_repository_no_git_or_no_sandbox_means_silence() {
        keel_harness::needs!(sandbox);
        let plain = tempfile::tempdir().unwrap();
        let repo = start();
        let profile = repo.paths().profile;
        let run = tempfile::tempdir().unwrap();
        for (git, workspace, profile) in [
            (git_bin(), plain.path().to_path_buf(), profile.clone()),
            (
                PathBuf::from("/nowhere/git"),
                repo.ws().to_path_buf(),
                profile.clone(),
            ),
            (PathBuf::from("git"), repo.ws().to_path_buf(), profile),
            (
                git_bin(),
                repo.ws().to_path_buf(),
                run.path().join("none.sb"),
            ),
        ] {
            let paths = Paths {
                git,
                workspace,
                state: run.path().join("hook"),
                profile,
            };
            for event in ["PreToolUse", "PostToolUse"] {
                let input = format!(
                    r#"{{"hook_event_name":"{event}","tool_use_id":"toolu_1","tool_input":{{"command":"sed -i x a"}}}}"#
                );
                assert_eq!(answer(&input, &paths, &Limits::DEFAULT), None);
            }
        }
    }

    /// A filter for `a.txt` that runs `script`, with `a.txt` rewritten as it was: git then has to
    /// run the filter to tell. Keel keeps the agent away from `.git/config`, but an agent that
    /// swapped `.git` for one of its own could name such a filter.
    fn plant_filter(repo: &Repo, script: &str) {
        repo.write(".gitattributes", "a.txt filter=evil\n");
        repo.git_cmd(&["config", "filter.evil.clean", &format!("{script}; cat")]);
        repo.write("a.txt", "a\n");
    }

    #[test]
    fn whatever_the_project_makes_git_run_stays_in_the_sandbox() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let mark = outside.join("ran");
        plant_filter(&repo, &format!("touch '{}'", mark.display()));
        // Without a sandbox, git runs it.
        let status = Command::new(&repo.git)
            .current_dir(repo.ws())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(mark.exists(), "the filter should run outside a sandbox");
        std::fs::remove_file(&mark).unwrap();
        repo.sandbox(&format!(
            "(version 1)\n(allow default)\n(deny file-write* (subpath \"{}\"))\n",
            outside.display()
        ));
        repo.write("a.txt", "a\n");
        repo.around("ls", |repo| repo.write("a.txt", "a\n"));
        assert!(!mark.exists(), "the filter ran outside the sandbox");
    }

    #[test]
    fn nothing_git_starts_outlives_the_call() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let pid_file = repo.run.path().join("pid");
        // It keeps git waiting for the filter's output, so git overruns and is stopped.
        plant_filter(
            &repo,
            &format!("sleep 30 & echo $! > '{}'", pid_file.display()),
        );
        let limits = Limits {
            time: Duration::from_millis(1500),
            ..Limits::DEFAULT
        };
        let started = Instant::now();
        assert_eq!(repo.send("PreToolUse", "toolu_s", "ls", &limits), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let pid = std::fs::read_to_string(&pid_file).expect("the filter ran");
        std::thread::sleep(Duration::from_millis(200));
        let alive = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap()
            .status
            .success();
        assert!(!alive, "sleep {} outlived the hook", pid.trim());
    }

    #[test]
    fn a_huge_status_is_given_up_on_inside_the_budget() {
        keel_harness::needs!(sandbox);
        let repo = start();
        for n in 0..40 {
            repo.write(&format!("many/{n}.txt"), "x\n");
        }
        let limits = Limits {
            entries: 20,
            ..Limits::DEFAULT
        };
        let started = Instant::now();
        assert_eq!(
            repo.around_with("sed -i '' x a.txt", "PostToolUse", &limits, |repo| repo
                .write("a.txt", "y\n")),
            None
        );
        assert!(started.elapsed() < limits.time, "{:?}", started.elapsed());
        // Under the limit, the same change is named: it was the limit that kept quiet.
        let note = repo.around("sed -i '' x a.txt", |repo| repo.write("a.txt", "z\n"));
        assert!(note.is_some_and(|note| note.contains("'a.txt'")));
    }

    #[test]
    fn a_snapshot_lives_outside_the_project_under_a_plain_name() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let limits = Limits::DEFAULT;
        assert_eq!(repo.send("PreToolUse", "toolu_A-1", "ls", &limits), None);
        assert!(repo.run.path().join("hook/toolu_A-1.snap").is_file());
        assert_eq!(repo.send("PostToolUse", "toolu_A-1", "ls", &limits), None);
        assert!(!repo.run.path().join("hook/toolu_A-1.snap").exists());
        for odd in ["../escape", "a/b", "", "x y"] {
            assert_eq!(repo.send("PreToolUse", odd, "ls", &limits), None);
        }
        let names: Vec<_> = std::fs::read_dir(repo.run.path().join("hook"))
            .unwrap()
            .collect();
        assert!(names.is_empty(), "{names:?}");
        assert!(!repo.run.path().join("escape.snap").exists());
    }

    #[test]
    fn a_snapshot_whose_command_never_came_back_is_cleared_by_the_next_one() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let state = repo.run.path().join("hook");
        std::fs::create_dir_all(&state).unwrap();
        let old = state.join("toolu_old.snap");
        std::fs::write(&old, "keel-snapshot-1\0-\0").unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(7 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        repo.send("PreToolUse", "toolu_new", "ls", &Limits::DEFAULT);
        assert!(!old.exists());
        assert!(state.join("toolu_new.snap").exists());
    }

    #[test]
    fn many_files_are_summed_up() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let note = repo
            .around("make", |repo| {
                for n in 0..8 {
                    repo.write(&format!("gen/{n}.rs"), "x\n");
                }
            })
            .expect("a note");
        assert!(note.contains("'gen/4.rs', and 3 more"), "{note}");
        assert!(!note.contains("Edit and Write"), "{note}");
    }

    #[test]
    fn a_file_too_large_to_compare_is_never_called_modified() {
        keel_harness::needs!(sandbox);
        let repo = start();
        let limits = Limits {
            file_bytes: 4,
            ..Limits::DEFAULT
        };
        repo.write("big.txt", "0123456789\n");
        let later = |path: &Path| {
            let then = std::fs::metadata(path).unwrap().modified().unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(then + Duration::from_secs(5))
                .unwrap();
        };
        // Written again at the same size: it may not have changed at all.
        let note = repo
            .around_with(
                "sed -i '' 's/z/y/' big.txt",
                "PostToolUse",
                &limits,
                |repo| {
                    repo.write("big.txt", "0123456789\n");
                    later(&repo.ws().join("big.txt"));
                },
            )
            .expect("a note");
        assert_eq!(
            note,
            format!(
                "While this Bash command ran, files in the project were written (rewritten, too \
                 large to compare: 'big.txt'). Check them with `git status --short`. {HINT}"
            )
        );
        // A new size, though, is a change.
        let note = repo
            .around_with(
                "sed -i '' 's/0/00/' big.txt",
                "PostToolUse",
                &limits,
                |repo| repo.write("big.txt", "00123456789\n"),
            )
            .expect("a note");
        assert!(note.contains("(modified: 'big.txt')"), "{note}");
    }

    #[test]
    fn past_the_cap_files_are_not_told_one_by_one() {
        keel_harness::needs!(sandbox);
        let files: Vec<(String, String)> = (0..510)
            .map(|n| (format!("many/{n}.txt"), format!("{n}\n")))
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let repo = Repo::new(&files);
        let old: String = repo.head().chars().take(12).collect();
        let note = repo
            .around("git checkout -q --orphan other", |repo| {
                repo.git_cmd(&["checkout", "-q", "--orphan", "other"])
            })
            .expect("a note");
        assert_eq!(
            note,
            format!(
                "While this Bash command ran, files in the project changed state; `git diff \
                 --stat {old}` and `git status --short` show them."
            )
        );
    }

    fn many(count: usize, text: &str) -> Vec<(String, String)> {
        (0..count)
            .map(|n| (format!("many/{n}.txt"), format!("{n} {text}\n")))
            .collect()
    }

    fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect()
    }

    #[test]
    fn committing_many_files_edited_before_the_command_changes_nothing() {
        keel_harness::needs!(sandbox);
        let files = many(600, "a");
        let repo = Repo::new(&borrowed(&files));
        for (path, _) in many(600, "b") {
            let n = path.trim_start_matches("many/").trim_end_matches(".txt");
            repo.write(&path, &format!("{n} b\n"));
        }
        // Listed before as edited and not after, with the same content: the commit moved, and
        // nothing in the files changed.
        let note = repo.around("git commit -qam x", |repo| {
            repo.git_cmd(&["commit", "-qam", "x"])
        });
        assert_eq!(note, None);
    }

    #[test]
    fn a_checkout_across_many_files_points_at_the_commits() {
        keel_harness::needs!(sandbox);
        let files = many(600, "a");
        let repo = Repo::new(&borrowed(&files));
        let old: String = repo.head().chars().take(12).collect();
        repo.git_cmd(&["checkout", "-q", "-b", "other"]);
        for (path, text) in many(600, "b") {
            repo.write(&path, &text);
        }
        repo.git_cmd(&["commit", "-qam", "other"]);
        repo.git_cmd(&["checkout", "-q", "-"]);
        assert_eq!(repo.head().chars().take(12).collect::<String>(), old);
        let note = repo
            .around("git checkout -q other", |repo| {
                repo.git_cmd(&["checkout", "-q", "other"])
            })
            .expect("a note");
        // The tree is clean after a checkout, so `git status` alone would show nothing.
        assert_eq!(
            note,
            format!(
                "While this Bash command ran, files in the project changed state; `git diff \
                 --stat {old}` and `git status --short` show them."
            )
        );
    }

    #[test]
    fn edits_are_told_from_other_commands_by_their_look() {
        for command in [
            "sed -i '' 's/a/b/' src/lib.rs",
            "sed -Ei 's/a/b/' x",
            "timeout 30 sed -i '' s/a/b/ x",
            "LC_ALL=C sed -i '' s/a/b/ x",
            "perl -pi -e 's/a/b/' x",
            "perl -0pi -e 's/a/b/' x",
            "ruby -i -pe 'x' f",
            "gawk -i inplace '{print}' f",
            // What the builder in run 40c0fe47 did.
            "python3 - <<'EOF'\np = 'crates/keel-engine/src/brain.rs'\ns = open(p).read()\nopen(p, 'w').write(s.replace('a', 'b'))\nEOF",
            "python3 $TMPDIR/fix.py",
            "FOO=1 \\\n  python3 $TMPDIR/fix.py",
            "python3 /tmp/fix.py",
            "cat <<'EOF' | python3\nopen('a.txt', 'w').write('x')\nEOF",
            "python3 -c \"open('a.txt', 'w').write('x')\"",
            "python3 -Bc \"import shutil; shutil.copy('a', 'b')\"",
            "python3 <<< \"open('a', 'w')\"",
            "node -e \"require('fs').writeFileSync('a', 'x')\"",
            "uv run python -c \"p.write_text('x')\"",
            "cat > src/a.rs <<'EOF'\nfn a() {}\nEOF",
            "echo x >> notes.md",
            "echo x>notes.md",
            "echo x | tee src/a.rs",
            "find . -name '*.rs' -exec sed -i '' 's/a/b/' {} +",
            "bash -c \"sed -i '' 's/a/b/' f\"",
            // A `<<` that isn't a here-document hides nothing after it.
            "git commit -m \"x << y\"\nsed -i '' s/a/b/ f",
            "echo $(( 1 << 2 ))\nsed -i '' s/a/b/ f",
            // A delimiter with a dash ends its here-document, and what follows still counts.
            "cat <<'END-X' | wc -l\nx > y\nEND-X\nsed -i '' s/a/b/ b.txt",
        ] {
            assert!(note::looks_like_edit(command), "{command}");
        }
        for command in [
            "cargo test",
            "cargo test > $TMPDIR/test.log 2>&1",
            "cargo build > /dev/null 2>&1",
            "echo done >&2",
            "sed -n '1,5p' a",
            "sed -n 1p a.txt | grep -vi x",
            "perl -Ilib -ne 'print' f",
            // Test runners and the project's own scripts.
            "python3 -m pytest tests/test_x.py",
            "python3 -W error -m pytest",
            "python3 \\\n  -m pytest",
            "python3 -V",
            "python manage.py test",
            "python3 scripts/check.py",
            "node scripts/x.mjs",
            "node --test a.test.js",
            "node \\\n  --test",
            // Inline code that writes nothing.
            "curl -s https://example.com/x | python3 -c 'import json, sys; print(json.load(sys.stdin))'",
            "node -e \"console.log(1)\"",
            "python3 <<'EOF'\nprint(1)\nEOF",
            "cat <<'EOF' | python3\nprint(1)\nEOF",
            // `>` that isn't a redirection.
            "awk '$1 > 5' a.txt",
            "git commit -m \"a > b\"",
            "git log --format='%h > %s'",
            "git log --pretty=format:%h->%s",
            "# write it all > out.txt\nls",
            "echo $(( 3 > 2 ))",
            "(( 3 > 2 )) && echo yes",
            "[[ $a > $b ]] && echo yes",
            "git commit -m \"$(cat <<'EOF'\nfix > and -> stuff\nEOF\n)\"",
            "cat <<'END-X' | wc -l\nx > y\nEND-X\necho done",
            "ls | tee /dev/stderr",
            // Saving a script to a temporary folder changes nothing in the project yet.
            "cat > $TMPDIR/fix.py <<'EOF'\np = 'x'\nopen(p, 'w').write('')\nEOF",
        ] {
            assert!(!note::looks_like_edit(command), "{command}");
        }
    }
}
