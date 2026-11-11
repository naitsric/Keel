//! Git on the project, run the way Keel runs it on a run's workspace elsewhere (`harden` and
//! `run_in_workspace` in `keel-engine/src/git.rs`): the git Keel found, in the sandbox of a
//! `check` step, with nothing from the environment, no hooks, no fsmonitor, no submodules, and no
//! lock the agent's own git could trip over. The project is the agent's to change, so whatever it
//! could make git run gets no more than the agent's own commands do.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

/// The most a git answer may be. Past it, there's no note.
const MOST_OUTPUT: u64 = 16 << 20;

/// What `harden` in `keel-engine/src/git.rs` sets with `-c`, and one more: no submodules. A test
/// there keeps the two in step.
pub const CONFIG: [&str; 7] = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "protocol.ext.allow=never",
    "commit.gpgsign=false",
    "gc.auto=0",
    "color.ui=false",
    "submodule.recurse=false",
];

pub struct Git<'a> {
    pub bin: &'a Path,
    pub workspace: &'a Path,
    /// The sandbox profile Keel wrote for this run's git.
    pub profile: &'a Path,
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGKILL: i32 = 9;

impl Git<'_> {
    /// What git prints, or `None` when it fails, says too much, or isn't done by `deadline`.
    pub fn run(&self, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
        if !self.bin.is_absolute() || !self.workspace.is_absolute() || !self.profile.is_file() {
            return None;
        }
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(self.profile)
            .arg(self.bin)
            .current_dir(self.workspace)
            .env_clear()
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Status would otherwise refresh the index, and its lock could fail the agent's git.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // This project's repository and work tree, whatever a config in it says.
            .env("GIT_DIR", self.workspace.join(".git"))
            .env("GIT_WORK_TREE", self.workspace)
            .arg("-C")
            .arg(self.workspace);
        for setting in CONFIG {
            command.args(["-c", setting]);
        }
        // A path is a path, never a pattern: `app/[id]` means that folder.
        command
            .arg("--literal-pathspecs")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Its own group, so whatever it starts goes with it.
            .process_group(0);
        let mut child = command.spawn().ok()?;
        let group = i32::try_from(child.id()).ok()?;
        // Read on another thread, so a git that hangs or floods can be stopped on time.
        let mut stdout = child.stdout.take()?;
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let read = (&mut stdout).take(MOST_OUTPUT + 1).read_to_end(&mut out);
            let _ = send.send(read.ok().map(|_| out));
        });
        let out = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
            .flatten()
            .filter(|out| out.len() as u64 <= MOST_OUTPUT);
        // Git's own answer first. Then whatever it started that stays in its group goes, while git
        // is exited but not yet reaped: until then the group's id can't go to anyone else.
        let finished = out.is_some() && exited(child.id(), deadline);
        unsafe {
            kill(-group, SIGKILL);
        }
        let status = child.wait().ok()?;
        out.filter(|_| finished && status.success())
    }
}

unsafe extern "C" {
    fn waitid(idtype: i32, id: u32, info: *mut std::ffi::c_void, options: i32) -> i32;
}

const P_PID: i32 = 1;
const WNOHANG: i32 = 1;
const WEXITED: i32 = 4;
#[cfg(target_os = "macos")]
const WNOWAIT: i32 = 0x20;
#[cfg(not(target_os = "macos"))]
const WNOWAIT: i32 = 0x0100_0000;

/// Whether the child has exited by `deadline`, without reaping it.
fn exited(pid: u32, deadline: Instant) -> bool {
    loop {
        // Room for a `siginfo_t`, whose first field, the signal, is 0 until the child has exited.
        let mut info = [0u64; 32];
        let asked = unsafe {
            waitid(
                P_PID,
                pid,
                info.as_mut_ptr().cast(),
                WEXITED | WNOWAIT | WNOHANG,
            )
        };
        if asked == 0 && info[0] & 0xffff_ffff != 0 {
            return true;
        }
        if asked != 0 || Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
