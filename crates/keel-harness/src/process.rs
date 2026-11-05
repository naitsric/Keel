//! Starts a step and keeps track of it. The output goes to a file, not a pipe, so the agent
//! and its audit trail both survive if the app dies; a reopened app can reconnect.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// A private file: audit data can include whatever a step read.
pub fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Spawns the step in its own process group, reading the prompt from a file and writing the
/// stream to another.
pub fn spawn_step(
    mut command: Command,
    prompt_file: &Path,
    raw_file: &Path,
    stderr_file: &Path,
) -> io::Result<Child> {
    command
        .stdin(Stdio::from(File::open(prompt_file)?))
        .stdout(Stdio::from(create_private(raw_file)?))
        .stderr(Stdio::from(create_private(stderr_file)?))
        .process_group(0);
    command.spawn()
}

/// Reads complete lines added to a file since the last call. A partial line stays for later.
pub struct Tail {
    pub offset: u64,
}

impl Tail {
    pub fn read_new(&mut self, path: &Path) -> io::Result<Vec<String>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        loop {
            let mut buffer = Vec::new();
            let read = reader.read_until(b'\n', &mut buffer)?;
            if read == 0 || buffer.last() != Some(&b'\n') {
                break;
            }
            self.offset += read as u64;
            lines.push(String::from_utf8_lossy(&buffer).trim_end().to_owned());
        }
        Ok(lines)
    }
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The process's start time, which tells a live step from an unrelated reused pid.
pub fn process_start(pid: i32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

pub fn is_same_process(pid: i32, started: &str) -> bool {
    process_start(pid).is_some_and(|s| s == started)
}

/// Sends a signal to a step's whole group. SIGINT first: it's the only one that leaves a
/// `result` event behind, as M0 measured.
pub fn signal_group(pid: i32, signal: i32) -> bool {
    pid > 1 && unsafe { libc::kill(-pid, signal) == 0 }
}

pub fn signal_process(pid: i32, signal: i32) -> bool {
    pid > 1 && unsafe { libc::kill(pid, signal) == 0 }
}

/// What a command printed, if it finished within `limit`. If it didn't, it and whatever it started
/// are killed and this is `None`.
pub fn stdout_within(mut command: Command, limit: Duration) -> io::Result<Option<Vec<u8>>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn()?;
    let mut pipe = child.stdout.take();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = sender.send(bytes);
    });
    let deadline = Instant::now() + limit;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            signal_group(child.id() as i32, SIGKILL);
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Something it left behind could hold the pipe open, and that's no reason to keep waiting.
    Ok(Some(
        receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default(),
    ))
}

pub const SIGINT: i32 = libc::SIGINT;
pub const SIGTERM: i32 = libc::SIGTERM;
pub const SIGKILL: i32 = libc::SIGKILL;

/// What the step's process tree is doing, to explain a lack of progress.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    /// The longest-running command below the agent, if any.
    pub command: Option<String>,
    pub pid: Option<i32>,
    pub cpu_percent: f32,
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    pid: i32,
    ppid: i32,
    cpu: f32,
    elapsed_secs: u64,
    command: String,
}

fn parse_elapsed(text: &str) -> u64 {
    let (days, rest) = text
        .split_once('-')
        .map_or((0, text), |(d, r)| (d.parse().unwrap_or(0), r));
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().unwrap_or(0)).collect();
    let clock = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => 0,
    };
    days * 86400 + clock
}

fn parse_ps(text: &str) -> Vec<ProcessInfo> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let cpu = fields.next()?.parse().ok()?;
            let elapsed_secs = parse_elapsed(fields.next()?);
            let command = fields.collect::<Vec<_>>().join(" ");
            Some(ProcessInfo {
                pid,
                ppid,
                cpu,
                elapsed_secs,
                command,
            })
        })
        .collect()
}

fn summarize(root: i32, processes: &[ProcessInfo]) -> Activity {
    let mut descendants = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for process in processes.iter().filter(|p| p.ppid == parent) {
            frontier.push(process.pid);
            descendants.push(process);
        }
    }
    let cpu_percent = descendants.iter().map(|p| p.cpu).sum();
    // Claude Code wraps commands in shells and sandbox launchers. The leaf is the real work.
    let leaf = descendants
        .iter()
        .filter(|p| !descendants.iter().any(|d| d.ppid == p.pid))
        .max_by_key(|p| p.elapsed_secs);
    Activity {
        command: leaf.map(|p| p.command.chars().take(160).collect()),
        pid: leaf.map(|p| p.pid),
        cpu_percent,
    }
}

pub fn activity(root: i32) -> Activity {
    let output = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,ppid=,pcpu=,etime=,command="])
        .output();
    let text = output
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    summarize(root, &parse_ps(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tail_returns_only_complete_lines_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "one\ntwo\npart").unwrap();
        file.flush().unwrap();
        let mut tail = Tail { offset: 0 };
        assert_eq!(tail.read_new(&path).unwrap(), ["one", "two"]);
        assert!(tail.read_new(&path).unwrap().is_empty());
        writeln!(file, "ial").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.read_new(&path).unwrap(), ["partial"]);
        let mut resumed = Tail { offset: 4 };
        assert_eq!(resumed.read_new(&path).unwrap(), ["two", "partial"]);
    }

    #[test]
    fn tail_of_a_missing_file_is_empty() {
        let mut tail = Tail { offset: 0 };
        assert!(
            tail.read_new(Path::new("/nonexistent/raw.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(parse_elapsed("45"), 45);
        assert_eq!(parse_elapsed("11:02"), 662);
        assert_eq!(parse_elapsed("01:00:05"), 3605);
        assert_eq!(parse_elapsed("2-00:00:01"), 172801);
    }

    #[test]
    fn finds_the_real_command_below_shells() {
        let ps = "\
            100 1 2.0 20:00 claude -p\n\
            101 100 0.0 11:00 /bin/zsh -c eval\n\
            102 101 0.5 11:00 uv run pytest -q\n\
            103 102 0.0 11:00 python -m pytest -q\n\
            200 1 90.0 50:00 unrelated";
        let activity = summarize(100, &parse_ps(ps));
        assert_eq!(activity.command.as_deref(), Some("python -m pytest -q"));
        assert_eq!(activity.pid, Some(103));
        assert!((activity.cpu_percent - 0.5).abs() < 0.01);
    }

    #[test]
    fn no_descendants_means_waiting_on_the_model() {
        let activity = summarize(100, &parse_ps("100 1 0.1 02:00 claude -p"));
        assert!(activity.command.is_none());
    }

    #[test]
    fn sha_detects_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "a").unwrap();
        let first = sha256_file(&path).unwrap();
        assert_eq!(first, sha256_file(&path).unwrap());
        std::fs::write(&path, "b").unwrap();
        assert_ne!(first, sha256_file(&path).unwrap());
    }

    #[test]
    fn raw_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw");
        create_private(&path).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_pid_is_recognised_only_with_its_start_time() {
        let pid = std::process::id() as i32;
        let started = process_start(pid).unwrap();
        assert!(is_same_process(pid, &started));
        assert!(!is_same_process(pid, "Thu Jan  1 00:00:00 1970"));
    }
}
