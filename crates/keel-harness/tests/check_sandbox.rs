//! The sandbox for `check` steps, for real. Tests and Makefiles can be edited by an agent, so
//! what runs here gets no more trust than the agent. Each test starts a sandbox, which macOS
//! refuses inside another one, as when Keel checks a change to itself. There they say so and stop.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use keel_harness::clean_env;
use keel_harness::home;
use keel_harness::settings::{check_profile, setup_profile, with_localhost, with_own_output};
use keel_harness::toolchains;

struct Sandbox {
    canary_dir: tempfile::TempDir,
    canary: PathBuf,
    workspace: tempfile::TempDir,
    home: PathBuf,
}

fn sandbox() -> Sandbox {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    // A canary in the real home folder, and a workspace beside it, as in a real install.
    let canary_dir = tempfile::Builder::new()
        .prefix(".keel-sandbox-test")
        .tempdir_in(&home)
        .unwrap();
    let canary = canary_dir.path().join("secret.txt");
    std::fs::write(&canary, "secret").unwrap();
    let workspace = tempfile::Builder::new()
        .prefix(".keel-sandbox-workspace")
        .tempdir_in(&home)
        .unwrap();
    std::fs::write(workspace.path().join("ok.txt"), "fine").unwrap();
    Sandbox {
        canary_dir,
        canary,
        workspace,
        home,
    }
}

impl Sandbox {
    fn profile(&self) -> String {
        check_profile(self.workspace.path(), &self.home, &[], &[])
    }

    fn run(&self, script: &str) -> std::process::Output {
        self.run_in(&self.profile(), script)
    }

    fn run_in(&self, profile: &str, script: &str) -> std::process::Output {
        // The environment a real check gets.
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", script])
            .current_dir(self.workspace.path())
            .env_clear()
            .envs(clean_env(&std::env::var("PATH").unwrap()))
            .output()
            .unwrap()
    }

    fn allowed(&self, script: &str) -> bool {
        self.run(script).status.success()
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy())
}

#[test]
fn exit_codes_come_through() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    assert_eq!(s.run("exit 0").status.code(), Some(0));
    assert_eq!(s.run("exit 7").status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&s.run("echo hello").stdout).trim(),
        "hello"
    );
}

#[test]
fn the_workspace_is_readable_and_writable() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    assert!(s.allowed("cat ok.txt"));
    assert!(s.allowed("echo x > new.txt && cat new.txt && mkdir -p a/b && rm new.txt"));
}

#[test]
fn the_home_folder_is_not() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    assert!(!s.allowed(&format!("cat {}", quoted(&s.canary))));
    assert!(!s.allowed(&format!("cat {}", quoted(&s.home.join(".ssh/known_hosts")))));
    assert!(!s.allowed(&format!(
        "cat {}",
        quoted(&s.home.join(".config/gh/hosts.yml"))
    )));
    assert!(!s.allowed(&format!(
        "touch {}",
        quoted(&s.home.join(".keel-sandbox-write"))
    )));
    assert!(!s.allowed(&format!("rm {}", quoted(&s.canary))));
    assert!(s.canary.exists());
}

#[test]
fn a_check_cannot_change_what_git_will_run() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let git = |script: &str| {
        Command::new("/bin/sh")
            .current_dir(s.workspace.path())
            .args(["-c", script])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(git(
        "git init -q -b main && git config user.name T && git config user.email t@e.c"
    ));
    let config = std::fs::read_to_string(s.workspace.path().join(".git/config")).unwrap();

    for attack in [
        "echo '[core]' >> .git/config",
        "rm .git/config",
        "mv .git/config .git/old",
        "echo x > .git/config.worktree",
        "echo /tmp > .git/commondir",
        "echo x > .git/hooks/pre-commit",
        "mv .git .git-old",
        "git config diff.external /tmp/x",
    ] {
        assert!(!s.allowed(attack), "{attack}");
    }
    assert_eq!(
        std::fs::read_to_string(s.workspace.path().join(".git/config")).unwrap(),
        config
    );
    assert!(s.workspace.path().join(".git/HEAD").exists());

    // Ordinary work still happens, and that's why the rest of `.git` stays open.
    assert!(s.allowed("git add -A && git commit -qm work && git branch other && git status"));
}

#[test]
fn git_works_when_the_home_folder_has_a_git_config() {
    keel_harness::needs!(sandbox);
    // Most Macs have one. The sandbox hides it, and git gives up when it can't read it.
    let s = sandbox();
    std::fs::write(
        s.canary_dir.path().join(".gitconfig"),
        "[user]\n\tname = X\n",
    )
    .unwrap();
    // Keel makes the clone, outside the sandbox. A check can't create a `.git` of its own.
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(s.workspace.path())
        .status()
        .unwrap();
    assert!(init.success());
    let profile = check_profile(s.workspace.path(), &s.home, &[], &[]);
    let path = std::env::var("PATH").unwrap();
    let git_status = |env: Vec<(String, String)>| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", "git status"])
            .current_dir(s.workspace.path())
            .env_clear()
            .envs(env)
            .env("HOME", s.canary_dir.path())
            .output()
            .unwrap()
    };
    let without = git_status(vec![("PATH".to_owned(), path.clone())]);
    assert!(
        !without.status.success(),
        "if this passes, the test proves nothing"
    );
    assert!(
        String::from_utf8_lossy(&without.stderr).contains(".gitconfig"),
        "{}",
        String::from_utf8_lossy(&without.stderr)
    );
    let with = git_status(clean_env(&path));
    assert!(
        with.status.success(),
        "{}",
        String::from_utf8_lossy(&with.stderr)
    );
}

/// Where a real install keeps a run: folders down inside the home folder, with its log beside the
/// workspace. Node and uv stopped dead there, and a workspace right in the home folder hid it.
struct DeepRun {
    root: tempfile::TempDir,
    runs: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
}

fn deep_run() -> DeepRun {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let root = tempfile::Builder::new()
        .prefix(".keel-sandbox-deep")
        .tempdir_in(&home)
        .unwrap();
    let runs = root.path().join("Application Support/runs/r1");
    let workspace = runs.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    DeepRun {
        root,
        runs,
        workspace,
        home,
    }
}

#[test]
fn a_command_can_find_its_way_to_a_workspace_deep_in_the_home_folder() {
    keel_harness::needs!(sandbox);
    let d = deep_run();
    let other = d.runs.with_file_name("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(d.runs.join("secret.txt"), "secret").unwrap();
    let profile = check_profile(&d.workspace, &d.home, &[], &[]);
    let allowed = |script: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh", "-c", script])
            .current_dir(&d.workspace)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(allowed(&format!("ls -ld {}", quoted(&d.runs))));
    assert!(allowed(&format!("ls -ld {}", quoted(d.root.path()))));
    assert!(allowed(
        "/usr/bin/perl -MCwd -e 'exit(defined(Cwd::realpath(\".\")) ? 0 : 1)'"
    ));

    // That they exist, and nothing else.
    assert!(!allowed(&format!("ls {}", quoted(&d.runs))));
    assert!(!allowed(&format!(
        "cat {}",
        quoted(&d.runs.join("secret.txt"))
    )));
    assert!(!allowed(&format!("ls -ld {}", quoted(&other))));
    assert!(!allowed(&format!(
        "ls -ld {}",
        quoted(&d.runs.join("secret.txt"))
    )));
    assert!(!allowed(&format!(
        "touch {}",
        quoted(&d.runs.join("new.txt"))
    )));
}

#[test]
fn a_command_can_ask_about_its_own_output_files() {
    keel_harness::needs!(sandbox);
    let d = deep_run();
    let log = d.runs.join("step.log");
    let asks = |profile: &str| {
        let output = std::fs::File::create(&log).unwrap();
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", "/usr/bin/stat -f %z <&1"])
            .current_dir(&d.workspace)
            .stdout(output)
            .status()
            .unwrap()
            .success()
    };
    let without = check_profile(&d.workspace, &d.home, &[], &[]);
    assert!(!asks(&without), "if this passes, the test proves nothing");
    let with = with_own_output(without, &[&log]);
    assert!(asks(&with));
    // Only the file itself: what's written to it still gets there, and reading it doesn't work.
    assert!(
        !Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &with, "/bin/cat", &log.to_string_lossy()])
            .current_dir(&d.workspace)
            .status()
            .unwrap()
            .success()
    );
}

/// #120: every project gets Claude Code's own folders and no toolchain. Those are found for each
/// project, and a person lets them be read.
#[test]
fn only_claude_codes_own_folders_are_readable_in_every_project() {
    let home = Path::new("/Users/me");
    let profile = check_profile(Path::new("/Users/me/w"), home, &[], &[]);
    for dir in [".local", ".claude/local"] {
        assert!(
            profile.contains(&format!("(subpath \"{}\")", home.join(dir).display())),
            "{dir}"
        );
    }
    for dir in ["Library/pnpm", ".cargo/bin", ".rustup", ".cargo"] {
        assert!(
            !profile.contains(&format!("(subpath \"{}\")", home.join(dir).display())),
            "{dir}"
        );
    }
}

/// A check in a workspace outside the home folder, as Keel runs one, with `path_env` for its PATH,
/// `home` as the home folder and `read` as what the project lets it read.
fn check_in(
    home: &Path,
    workspace: &Path,
    read: &[String],
    path_env: &str,
    script: &str,
) -> std::process::Output {
    let own = workspace.join(".keel-cache").join(home::FOLDER);
    home::create(&own).unwrap();
    Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            &check_profile(workspace, home, read, &[]),
            "/bin/sh",
            "-c",
            script,
        ])
        .current_dir(workspace)
        .env_clear()
        .envs(clean_env(path_env))
        .envs(home::env(&own, home))
        .output()
        .unwrap()
}

/// #120: `node` from nvm or from asdf, as each lays it out in a home folder of the test's own: a
/// version's `bin` on the PATH, or a shim that has asdf run the version the repo names. `node` is a
/// script that reads what was installed beside it. Keel finds the folder from the PATH alone, the
/// check runs with it and nothing added by hand, and without it the sandbox stops it.
#[test]
fn a_check_runs_node_from_nvm_or_asdf_with_the_folder_keel_proposes() {
    keel_harness::needs!(sandbox);
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let fake_home = root.path().canonicalize().unwrap().join("Users/me");
    let (workspace, _) = install_in(&fake_home);
    for name in CREDENTIALS {
        let path = fake_home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("secret in {name}")).unwrap();
    }
    std::fs::write(workspace.join(".tool-versions"), "nodejs 20.11.0\n").unwrap();
    let script = |at: &Path, text: &str| {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        keel_harness::testing::write_script(at, text).unwrap();
    };
    let node = "#!/bin/sh\ncat \"${0%/*}/../lib/node_modules/npm/installed\"\n";
    let installed = |bin: &Path, by: &str| {
        script(&bin.join("node"), node);
        let lib = bin.join("../lib/node_modules/npm");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("installed"), format!("node from {by}\n")).unwrap();
    };
    let nvm_bin = fake_home.join(".nvm/versions/node/v20.11.0/bin");
    installed(&nvm_bin, "nvm");
    installed(&fake_home.join(".asdf/installs/nodejs/20.11.0/bin"), "asdf");
    script(
        &fake_home.join(".asdf/bin/asdf"),
        "#!/bin/sh\n[ \"$1\" = exec ] || exit 2\nshift\nprogram=$1\nshift\n\
         version=$(sed -n 's/^nodejs //p' .tool-versions)\n\
         exec \"$ASDF_DATA_DIR/installs/nodejs/$version/bin/$program\" \"$@\"\n",
    );
    script(
        &fake_home.join(".asdf/shims/node"),
        "#!/bin/sh\n# asdf-plugin: nodejs 20.11.0\nexec asdf exec \"node\" \"$@\"\n",
    );

    for (manager, path, folder) in [
        ("nvm", nvm_bin.display().to_string(), "~/.nvm"),
        (
            "asdf",
            format!(
                "{}:{}",
                fake_home.join(".asdf/bin").display(),
                fake_home.join(".asdf/shims").display()
            ),
            "~/.asdf",
        ),
    ] {
        let path_env = format!("{path}:/usr/bin:/bin:/usr/sbin:/sbin");
        let check = "node --test";
        let found = toolchains::find(&[check], &path_env, &fake_home);
        assert_eq!(
            found,
            [toolchains::Found {
                folder: folder.to_owned(),
                programs: vec!["node".to_owned()],
            }],
            "{manager}"
        );
        let read: Vec<String> = found.into_iter().map(|found| found.folder).collect();
        let with = check_in(&fake_home, &workspace, &read, &path_env, check);
        assert!(
            with.status.success(),
            "{manager}: {}",
            String::from_utf8_lossy(&with.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&with.stdout),
            format!("node from {manager}\n")
        );
        let without = check_in(&fake_home, &workspace, &[], &path_env, check);
        assert!(
            !without.status.success() && without.stdout.is_empty(),
            "{manager}: if this passes, the test proves nothing"
        );
        for name in CREDENTIALS {
            let cat = format!("cat {}", quoted(&fake_home.join(name)));
            refused(
                &check_in(&fake_home, &workspace, &read, &path_env, &cat),
                &format!("{manager}: {name}"),
            );
        }
    }
}

/// #120 on this Mac: cargo, rustup and pnpm where their own installers put them, in the home
/// folder. Every project could read them before. Now Keel finds them, and a check runs them with
/// what it found and nothing else.
#[test]
fn the_toolchains_on_this_mac_are_found_and_run_in_the_sandbox() {
    keel_harness::needs!(sandbox);
    let real_home = PathBuf::from(std::env::var("HOME").unwrap());
    // The PATH the app gives commands, from the login shell.
    let tools = keel_harness::Tools::detect();
    // Outside the home folder, so the test writes nothing in it.
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (check, wanted) in [
        ("cargo --version", &["~/.cargo/bin", "~/.rustup"][..]),
        ("pnpm --version", &["~/Library/pnpm"][..]),
    ] {
        let program = check.split(' ').next().unwrap();
        match toolchains::locate(&[program.to_owned()], &tools.path_env).first() {
            None => {
                keel_harness::testing::skipped(&format!("{program} isn't installed"));
                continue;
            }
            Some((_, at)) if !at.starts_with(&real_home) => {
                keel_harness::testing::skipped(&format!("{program} isn't in the home folder"));
                continue;
            }
            Some(_) => {}
        }
        let found = toolchains::find(&[check], &tools.path_env, &real_home);
        let read: Vec<String> = found.iter().map(|found| found.folder.clone()).collect();
        eprintln!("{check}: Keel proposes {found:?}");
        for folder in wanted {
            assert!(read.iter().any(|f| f == folder), "{folder} in {read:?}");
        }
        assert!(!read.iter().any(|f| f == "~/.cargo"), "{read:?}");
        let with = check_in(&real_home, &workspace, &read, &tools.path_env, check);
        assert!(
            with.status.success(),
            "{check}: {}",
            String::from_utf8_lossy(&with.stderr)
        );
        let without = check_in(&real_home, &workspace, &[], &tools.path_env, check);
        assert!(
            !without.status.success(),
            "{check}: if this passes, the test proves nothing"
        );
    }
    // Cargo's credentials sit right beside what it needs, and stay out of reach.
    let credentials = real_home.join(".cargo/credentials.toml");
    if credentials.exists() {
        let read = vec!["~/.cargo/bin".to_owned(), "~/.rustup".to_owned()];
        let cat = format!("cat {}", quoted(&credentials));
        refused(
            &check_in(&real_home, &workspace, &read, &tools.path_env, &cat),
            "cargo's credentials",
        );
    }
}

#[test]
fn temp_folders_work_because_test_suites_need_them() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    assert!(s.allowed("f=$(mktemp) && echo x > \"$f\" && rm \"$f\""));
}

#[test]
fn the_keychain_is_out_of_reach() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    assert!(!s.allowed("security find-generic-password -s gh:github.com -w >/dev/null 2>&1"));
    assert!(
        !s.allowed(
            "security find-generic-password -s 'Claude Code-credentials' -w >/dev/null 2>&1"
        )
    );
}

#[test]
fn there_is_no_network() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(!s.allowed(&format!(
        "curl -fsS -m 3 -o /dev/null http://127.0.0.1:{port}/"
    )));
    assert!(!s.allowed("curl -fsS -m 4 -o /dev/null https://example.com"));
}

/// A server a test starts on localhost, on a port the system picks, and talks to: on 127.0.0.1
/// and on ::1, as supertest and most suites do.
const OWN_SERVER: &str = "python3 -c \"
import socket
for family, host in ((socket.AF_INET, '127.0.0.1'), (socket.AF_INET6, '::1')):
    server = socket.socket(family)
    server.bind((host, 0))
    server.listen(1)
    client = socket.create_connection(server.getsockname()[:2], 2)
    peer, _ = server.accept()
    client.sendall(b'hi')
    assert peer.recv(2) == b'hi'
print('answered')
\"";

/// #123: a project whose tests start servers on localhost opens it to its check. Closed, as every
/// project is unless a person says so, the same suite is refused.
#[test]
fn with_localhost_open_a_check_can_start_a_server_and_reach_it() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    refused(&s.run(OWN_SERVER), "localhost closed");
    let open = s.run_in(&with_localhost(s.profile()), OWN_SERVER);
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&open.stdout).trim(), "answered");
}

/// Open, it's all of localhost: whatever listens on this Mac outside the sandbox too, which is the
/// risk the setting says it takes. Here that's a server this test starts. Nothing further opens.
#[test]
fn with_localhost_open_a_check_reaches_what_listens_on_this_mac_and_nothing_further() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect =
        format!("python3 -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 2)\"");
    refused(&s.run(&connect), "localhost closed");
    let open = with_localhost(s.profile());
    let reached = s.run_in(&open, &connect);
    assert!(
        reached.status.success(),
        "{}",
        String::from_utf8_lossy(&reached.stderr)
    );
    assert!(
        !s.run_in(&open, "curl -fsS -m 4 -o /dev/null https://example.com")
            .status
            .success(),
        "the internet"
    );
    // Nor the sockets of this Mac, which only a project's services open.
    let path = s.canary_dir.path().join("s.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let unix = format!(
        "python3 -c \"import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')\"",
        path.display()
    );
    refused(&s.run_in(&open, &unix), "a unix socket");
}

/// Keel's own tests run inside a check when it works on itself, and those that need what it takes
/// away stop there and say why. These are the refusals `keel_harness::testing` goes by, so one
/// can't pass for a real failure.
#[test]
fn inside_a_check_what_tests_may_need_is_refused_in_a_way_they_can_tell() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    for (script, says) in [
        (
            "/usr/bin/sandbox-exec -p '(version 1)(allow default)(deny network*)' /usr/bin/true",
            keel_harness::testing::REFUSAL,
        ),
        (
            "python3 -c \"import socket; socket.socket().bind(('127.0.0.1', 0))\"",
            "Operation not permitted",
        ),
        ("/bin/ps -o pid= -p $$", "Operation not permitted"),
    ] {
        let output = s.run(script);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{script}");
        assert!(stderr.contains(says), "{script}: {stderr}");
    }
}

#[test]
fn listed_paths_can_be_read() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let allowed = s.canary_dir.path().to_string_lossy().into_owned();
    let profile = check_profile(s.workspace.path(), &s.home, &[allowed], &[]);
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/cat", &s.canary.to_string_lossy()])
        .current_dir(s.workspace.path())
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn local_unix_sockets_are_out_of_reach_too() {
    keel_harness::needs!(sandbox);
    // The Docker socket is one of these, and reaching it would undo the whole sandbox.
    let s = sandbox();
    let path = s.canary_dir.path().join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let script = format!(
        "python3 -c \"import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')\"",
        path.to_string_lossy()
    );
    assert!(!s.allowed(&script));
}

#[test]
fn a_listed_socket_works_and_no_other_does() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let allowed_dir = root_path.join("db");
    let other_dir = root_path.join("other");
    std::fs::create_dir_all(&allowed_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();
    let allowed = allowed_dir.join("db.sock");
    let other = other_dir.join("other.sock");
    let listener = std::os::unix::net::UnixListener::bind(&allowed).unwrap();
    let other_listener = std::os::unix::net::UnixListener::bind(&other).unwrap();
    std::thread::spawn(move || for _ in listener.incoming() {});
    std::thread::spawn(move || for _ in other_listener.incoming() {});
    let profile = check_profile(
        s.workspace.path(),
        &s.home,
        &[],
        &[allowed_dir.to_string_lossy().into_owned()],
    );
    let connect = |path: &Path| {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "python3",
                "-c",
                &format!(
                    "import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')",
                    path.display()
                ),
            ])
            .current_dir(s.workspace.path())
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(connect(&allowed), "the listed socket");
    assert!(!connect(&other), "any other socket");
}

fn run_setup(s: &Sandbox, cache: &Path, script: &str) -> bool {
    let profile = setup_profile(s.workspace.path(), &s.home, &[], &[], cache);
    Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/sh", "-c", script])
        .current_dir(s.workspace.path())
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn setup_can_reach_the_internet_and_its_cache() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    assert!(
        run_setup(
            &s,
            cache.path(),
            "curl -fsS -m 10 -o /dev/null https://pypi.org/pypi/pip/json"
        ),
        "the internet, including DNS"
    );
    assert!(
        run_setup(
            &s,
            cache.path(),
            &format!(
                "echo x > {}/f && cat {}/f",
                quoted(cache.path()),
                quoted(cache.path())
            )
        ),
        "its cache"
    );
    assert!(
        run_setup(&s, cache.path(), "echo x > installed.txt"),
        "the workspace"
    );
}

#[test]
fn setup_still_cannot_reach_anything_on_this_machine() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!("curl -fsS -m 3 -o /dev/null http://127.0.0.1:{port}/")
        ),
        "local ports"
    );
    assert!(
        !run_setup(&s, cache.path(), &format!("cat {}", quoted(&s.canary))),
        "the home folder"
    );
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!("touch {}", quoted(&s.home.join(".keel-sandbox-write")))
        ),
        "writes to home"
    );
    assert!(
        !run_setup(
            &s,
            cache.path(),
            "security find-generic-password -s gh:github.com -w >/dev/null 2>&1"
        ),
        "the keychain"
    );
    let path = s.canary_dir.path().join("s.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert!(
        !run_setup(
            &s,
            cache.path(),
            &format!(
                "python3 -c \"import socket; c = socket.socket(socket.AF_UNIX); c.connect('{}')\"",
                path.to_string_lossy()
            )
        ),
        "local sockets"
    );
}

/// Where a real install keeps a project: its cache beside its mirror, apart from the run, with
/// other projects next door. uv stopped dead there, and a cache right in the home folder hid it.
#[test]
fn setup_can_find_its_way_to_its_cache_and_learns_nothing_else_on_the_way() {
    keel_harness::needs!(sandbox);
    let d = deep_run();
    let projects = d.root.path().join("Application Support/projects");
    let project = projects.join("p1");
    let cache = project.join("cache");
    let mirror = project.join("mirror.git");
    let other_project = projects.join("p2");
    let other_cache = other_project.join("cache");
    for folder in [&cache, &mirror, &other_cache] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(mirror.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(other_cache.join("wheel"), "theirs").unwrap();

    let run = |profile: &str, script: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/bin/sh", "-c", script])
            .current_dir(&d.workspace)
            .output()
            .unwrap()
    };
    let profile = setup_profile(&d.workspace, &d.home, &[], &[], &cache);
    let allowed = |script: &str| {
        let output = run(&profile, script);
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    // `realpath` needs what uv needs here: to ask about every folder on the way down.
    let real_path = format!("/bin/realpath {}", quoted(&cache));
    assert_eq!(
        String::from_utf8_lossy(&allowed(&real_path).stdout).trim(),
        cache.canonicalize().unwrap().to_string_lossy()
    );
    let without = [&projects, &project]
        .iter()
        .fold(profile.clone(), |rules, folder| {
            rules.replace(&format!(" (literal \"{}\")", folder.display()), "")
        });
    assert_ne!(without, profile);
    assert!(
        !run(&without, &real_path).status.success(),
        "if this passes, the test proves nothing"
    );
    let inside = quoted(&cache.join("uv/sdists-v10"));
    allowed(&format!(
        "mkdir -p {inside} && echo x > {inside}/f && cat {inside}/f && mv {inside}/f {inside}/g"
    ));

    // That the folders on the way exist, and nothing else. Each of these is there, so being
    // refused can't be mistaken for not finding it.
    allowed(&format!("ls -ld {}", quoted(&projects)));
    allowed(&format!("ls -ld {}", quoted(&project)));
    for script in [
        format!("ls {}", quoted(&projects)),
        format!("ls {}", quoted(&project)),
        format!("ls {}", quoted(&mirror)),
        format!("ls -ld {}", quoted(&mirror)),
        format!("cat {}", quoted(&mirror.join("HEAD"))),
        format!("ls -ld {}", quoted(&other_project)),
        format!("ls {}", quoted(&other_cache)),
        format!("ls -ld {}", quoted(&other_cache)),
        format!("cat {}", quoted(&other_cache.join("wheel"))),
        format!("touch {}", quoted(&projects.join("new"))),
        format!("touch {}", quoted(&project.join("new"))),
        format!("echo x >> {}", quoted(&mirror.join("HEAD"))),
        format!("touch {}", quoted(&other_cache.join("poison"))),
    ] {
        let output = run(&profile, &script);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{script}");
        assert!(
            stderr.contains("Operation not permitted"),
            "{script}: {stderr}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(mirror.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    assert!(!other_cache.join("poison").exists());
    assert!(!project.join("new").exists());
    assert!(!projects.join("new").exists());
}

#[test]
fn the_cache_is_only_writable_during_setup() {
    keel_harness::needs!(sandbox);
    let s = sandbox();
    let cache = tempfile::Builder::new()
        .prefix(".keel-sandbox-cache")
        .tempdir_in(&s.home)
        .unwrap();
    assert!(
        !s.allowed(&format!("touch {}/poison", quoted(cache.path()))),
        "a check can't write to the shared cache"
    );
    assert!(!cache.path().join("poison").exists());
}

/// Credentials tools keep in the home folder. A command gets a home of its own, and these stay in
/// the real one, out of reach.
const CREDENTIALS: [&str; 6] = [
    ".npmrc",
    ".m2/settings.xml",
    ".gradle/gradle.properties",
    ".ssh/id_ed25519",
    ".cargo/credentials.toml",
    ".cargo/credentials",
];

/// Runs `script` as setup does, given its cache, or as a check does, with `real_home` as the home
/// folder Keel runs in: the same profile and the variables that put it in a home of its own.
fn in_own_home(
    real_home: &Path,
    workspace: &Path,
    setup_cache: Option<&Path>,
    script: &str,
) -> std::process::Output {
    let (profile, own) = match setup_cache {
        Some(cache) => (
            setup_profile(workspace, real_home, &[], &[], cache),
            cache.join(home::FOLDER),
        ),
        None => (
            check_profile(workspace, real_home, &[], &[]),
            workspace.join(".keel-cache").join(home::FOLDER),
        ),
    };
    home::create(&own).unwrap();
    Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/sh", "-c", script])
        .current_dir(workspace)
        .env_clear()
        .envs(clean_env(&std::env::var("PATH").unwrap()))
        .envs(home::env(&own, real_home))
        .output()
        .unwrap()
}

/// Where a real install keeps a run's copy and a project's cache, under `home`.
fn install_in(home: &Path) -> (PathBuf, PathBuf) {
    let data = home.join("Library/Application Support/keel");
    let workspace = data.join("runs/r1/workspace");
    let cache = data.join("projects/p1/cache");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    (workspace, cache)
}

fn refused(output: &std::process::Output, what: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{what}");
    assert!(
        stderr.contains("Operation not permitted"),
        "{what}: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{what}");
}

#[test]
fn credentials_in_the_home_folder_stay_out_of_reach_with_a_home_of_its_own() {
    keel_harness::needs!(sandbox);
    // A home folder of the test's own with every one of them in it, so nothing is made in the real
    // one, and Keel's data in it as in a real install.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let real_home = root.path().canonicalize().unwrap().join("Users/me");
    for name in CREDENTIALS {
        let path = real_home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("secret in {name}")).unwrap();
    }
    let (workspace, cache) = install_in(&real_home);
    for (step, setup_cache) in [("check", None), ("setup", Some(cache.as_path()))] {
        for name in CREDENTIALS {
            let path = quoted(&real_home.join(name));
            refused(
                &in_own_home(&real_home, &workspace, setup_cache, &format!("cat {path}")),
                &format!("{step}: {name}"),
            );
            // The home folder it's told about is its own, and none of them is there.
            let own = in_own_home(
                &real_home,
                &workspace,
                setup_cache,
                &format!("cat \"$HOME/{name}\""),
            );
            assert!(!own.status.success(), "{step}: ~/{name}");
            assert!(!String::from_utf8_lossy(&own.stdout).contains("secret"));
        }
        for folder in [".ssh", ".cargo", ".m2", ".gradle"] {
            refused(
                &in_own_home(
                    &real_home,
                    &workspace,
                    setup_cache,
                    &format!("ls {}", quoted(&real_home.join(folder))),
                ),
                &format!("{step}: {folder}"),
            );
        }
    }
}

#[test]
fn the_real_credentials_stay_out_of_reach_too() {
    keel_harness::needs!(sandbox);
    let real_home = PathBuf::from(std::env::var("HOME").unwrap());
    let mut present: Vec<PathBuf> = CREDENTIALS
        .iter()
        .map(|name| real_home.join(name))
        .filter(|path| path.exists())
        .collect();
    // Whatever `~/.ssh` has, and the folder itself.
    let ssh = real_home.join(".ssh");
    if ssh.is_dir() {
        present.push(ssh.clone());
        present.extend(
            std::fs::read_dir(&ssh)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file()),
        );
    }
    if present.is_empty() {
        keel_harness::testing::skipped(
            "this Mac has none of them, and the test above tries them all in a home of its own",
        );
        return;
    }
    // Keel's data where this test's build is, which is in the home folder like a real install.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (workspace, cache) = install_in(root.path());
    for (step, setup_cache) in [("check", None), ("setup", Some(cache.as_path()))] {
        for path in &present {
            let read = if path.is_dir() { "ls" } else { "cat" };
            refused(
                &in_own_home(
                    &real_home,
                    &workspace,
                    setup_cache,
                    &format!("{read} {}", quoted(path)),
                ),
                &format!("{step}: {}", path.display()),
            );
        }
    }
}

/// #94: when a repo's settings don't name an ignore file, git, and cargo through it, look for one
/// in `.config` in the home folder. Any repo but the run's own clone is like that: one a test makes,
/// a submodule. In the real home folder that was refused, so git complained on every command and
/// cargo couldn't build a crate with a build script there. A home of its own has nothing to refuse.
#[test]
fn a_command_reads_no_settings_from_the_real_home_folder() {
    keel_harness::needs!(sandbox);
    let real_home = PathBuf::from(std::env::var("HOME").unwrap());
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (workspace, _) = install_in(root.path());
    let status = "git -C repo status --porcelain";
    let own = in_own_home(
        &real_home,
        &workspace,
        None,
        &format!("git init -q repo && {status} && echo \"$XDG_CONFIG_HOME\""),
    );
    let stderr = String::from_utf8_lossy(&own.stderr);
    assert!(own.status.success(), "{stderr}");
    assert!(!stderr.contains("not permitted"), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&own.stdout).trim(),
        workspace.join(".keel-cache/home/.config").to_string_lossy()
    );

    // With the real home folder, as a check had before.
    let profile = check_profile(&workspace, &real_home, &[], &[]);
    let before = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile, "/bin/sh", "-c", status])
        .current_dir(&workspace)
        .env_clear()
        .envs(clean_env(&std::env::var("PATH").unwrap()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&before.stderr);
    assert!(
        stderr.contains(".config/git/ignore") && stderr.contains("not permitted"),
        "if this fails, the test proves nothing: {stderr}"
    );
}
