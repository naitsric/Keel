//! Services a project's commands need, such as a test database. M0 found that a sandboxed command
//! can only reach a local server through a Unix socket it was told about, and that opening TCP
//! would expose every local service. So Keel runs these outside the sandbox and hands steps one
//! socket per service. They come from the app's settings, never from the repo, and they live only
//! as long as the step that uses them.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use keel_core::{EnvVar, Reason, Service};
use keel_harness::process;

/// What `sockaddr_un` can hold on macOS.
const SOCKET_PATH_LIMIT: usize = 103;
const COMMAND_START_LIMIT: Duration = Duration::from_secs(30);
const STOP_LIMIT: Duration = Duration::from_secs(5);
const PID_FILE: &str = ".keel-pid";

/// Variables Keel controls or that carry credentials. A project can't set these.
const RESERVED_ENV: [&str; 12] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "SSH_AUTH_SOCK",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

fn invalid(field: &str, value: &str) -> Reason {
    Reason::InvalidSetting {
        field: field.to_owned(),
        value: value.to_owned(),
    }
}

/// Checks what a person typed in Settings, before it can reach a process.
pub fn validate(services: &[Service], env: &[EnvVar]) -> Result<(), Reason> {
    let mut names = HashSet::new();
    for service in services {
        let name = service.name();
        let good = !name.is_empty()
            && name.len() <= 20
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !good {
            return Err(invalid("serviceName", name));
        }
        if !names.insert(name) {
            return Err(invalid("serviceDuplicate", name));
        }
        if let Some(socket) = service.socket_file()
            && (socket.is_empty() || socket.contains('/') || socket == "." || socket == "..")
        {
            return Err(invalid("serviceSocket", socket));
        }
        match service {
            Service::Bridge { port, .. } if *port == 0 => return Err(invalid("servicePort", "0")),
            Service::Command { start, .. } if start.trim().is_empty() => {
                return Err(invalid("serviceStart", name));
            }
            _ => {}
        }
    }
    for var in env {
        let name = var.name.as_str();
        let good = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !good {
            return Err(invalid("envName", name));
        }
        if RESERVED_ENV.contains(&name)
            || name.starts_with("CLAUDE_")
            || name.starts_with("ANTHROPIC_")
            || name.starts_with("GIT_")
            || name.starts_with("KEEL_")
        {
            return Err(invalid("envReserved", name));
        }
    }
    Ok(())
}

/// Where a project's sockets live. Short, because socket paths have a small limit, and resolved,
/// because the sandbox compares real paths. One command step runs per project, so one is enough.
pub fn root_for(project_id: &str) -> PathBuf {
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let short: String = project_id.chars().take(8).collect();
    temp.join("keel").join(short)
}

pub fn socket_dir(root: &Path, service: &Service) -> PathBuf {
    root.join(service.name())
}

pub fn socket_path(root: &Path, service: &Service) -> PathBuf {
    socket_dir(root, service).join(
        service
            .socket_file()
            .map_or_else(|| format!("{}.sock", service.name()), str::to_owned),
    )
}

/// The folders a sandbox has to let through, one per service and nothing else.
pub fn socket_dirs(root: &Path, services: &[Service]) -> Vec<String> {
    services
        .iter()
        .map(|s| socket_dir(root, s).to_string_lossy().into_owned())
        .collect()
}

/// Fills in `{{socket:name}}` and `{{socketDir:name}}`. One pass, so a value can't expand another.
pub fn resolve_env(env: &[EnvVar], services: &[Service], root: &Path) -> Vec<(String, String)> {
    env.iter()
        .map(|var| {
            let mut value = String::new();
            let mut rest = var.value.as_str();
            while let Some(start) = rest.find("{{") {
                value.push_str(&rest[..start]);
                let after = &rest[start + 2..];
                let Some(end) = after.find("}}") else {
                    value.push_str(&rest[start..]);
                    rest = "";
                    break;
                };
                let token = after[..end].trim();
                let found = token.split_once(':').and_then(|(kind, name)| {
                    let service = services.iter().find(|s| s.name() == name)?;
                    match kind {
                        "socket" => Some(socket_path(root, service)),
                        "socketDir" => Some(socket_dir(root, service)),
                        _ => None,
                    }
                });
                match found {
                    Some(path) => value.push_str(&path.to_string_lossy()),
                    None => value.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            value.push_str(rest);
            (var.name.clone(), value)
        })
        .collect()
}

struct Bridge {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn copy_until_closed(mut from: impl io::Read, mut to: impl io::Write, close: impl FnOnce() + Send) {
    let _ = io::copy(&mut from, &mut to);
    close();
}

/// Forwards one connection. Only ever to this port on loopback, whatever the client sends.
fn forward(client: UnixStream, port: u16) -> io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let server = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let (client_read, server_write) = (client.try_clone()?, server.try_clone()?);
    let (client_write, server_read) = (client, server);
    let close_server = server_write.try_clone()?;
    let close_client = client_write.try_clone()?;
    std::thread::spawn(move || {
        copy_until_closed(client_read, server_write, move || {
            let _ = close_server.shutdown(Shutdown::Write);
        });
    });
    std::thread::spawn(move || {
        copy_until_closed(server_read, client_write, move || {
            let _ = close_client.shutdown(Shutdown::Write);
        });
    });
    Ok(())
}

fn start_bridge(path: &Path, port: u16) -> Result<Bridge, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|_| format!("nothing is listening on port {port}"))?;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((client, _)) => {
                    if client.set_nonblocking(false).is_ok() {
                        // A failure here reaches the client as a closed connection, as a real
                        // server going away would.
                        let _ = forward(client, port);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    Ok(Bridge {
        stop,
        thread: Some(thread),
        path: path.to_owned(),
    })
}

struct Server {
    child: Child,
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.id() as i32;
        process::signal_group(pid, process::SIGTERM);
        let deadline = Instant::now() + STOP_LIMIT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            process::signal_group(pid, 9);
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_file(self.dir.join(PID_FILE));
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Ends a server left behind if Keel died while running a step.
fn stop_leftover(dir: &Path) {
    let Ok(text) = std::fs::read_to_string(dir.join(PID_FILE)) else {
        return;
    };
    let mut lines = text.lines();
    let (Some(pid), Some(started)) = (
        lines.next().and_then(|p| p.parse::<i32>().ok()),
        lines.next(),
    ) else {
        return;
    };
    if process::is_same_process(pid, started) {
        process::signal_group(pid, process::SIGTERM);
        let deadline = Instant::now() + STOP_LIMIT;
        while process::is_same_process(pid, started) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if process::is_same_process(pid, started) {
            process::signal_group(pid, 9);
        }
    }
    let _ = std::fs::remove_file(dir.join(PID_FILE));
}

fn start_server(
    start: &str,
    dir: &Path,
    path: &Path,
    log: &Path,
    path_env: &str,
) -> Result<Server, String> {
    stop_leftover(dir);
    let _ = std::fs::remove_file(path);
    let log_file = File::create(log).map_err(|e| e.to_string())?;
    let errors = log_file.try_clone().map_err(|e| e.to_string())?;
    let mut command = Command::new("/bin/sh");
    command.args(["-c", start]).current_dir(dir).env_clear();
    for name in [
        "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "SHELL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("PATH", path_env)
        .env("KEEL_SOCKET_DIR", dir)
        .env("KEEL_SOCKET", path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(errors))
        .process_group(0);
    let child = command.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    if let Some(started) = process::process_start(pid) {
        let _ = std::fs::write(dir.join(PID_FILE), format!("{pid}\n{started}\n"));
    }
    let mut server = Server {
        child,
        dir: dir.to_owned(),
        path: path.to_owned(),
    };

    let deadline = Instant::now() + COMMAND_START_LIMIT;
    loop {
        if UnixStream::connect(path).is_ok() {
            return Ok(server);
        }
        let tail = || {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            text.lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        };
        if let Ok(Some(status)) = server.child.try_wait() {
            return Err(format!(
                "it exited with {status} before opening its socket. {}",
                tail()
            ));
        }
        if Instant::now() > deadline {
            return Err(format!(
                "it didn't open its socket within {} seconds. {}",
                COMMAND_START_LIMIT.as_secs(),
                tail()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A step's running services. Dropping this stops them.
pub struct ServiceSet {
    bridges: Vec<Bridge>,
    servers: Vec<Server>,
    dirs: Vec<PathBuf>,
}

impl Drop for ServiceSet {
    fn drop(&mut self) {
        self.bridges.clear();
        self.servers.clear();
        // Only removes them if empty, so a server's own leftovers stay for whoever looks.
        for dir in &self.dirs {
            let _ = std::fs::remove_dir(dir);
        }
        if let Some(root) = self.dirs.first().and_then(|d| d.parent()) {
            let _ = std::fs::remove_dir(root);
        }
    }
}

impl ServiceSet {
    /// Starts everything or nothing: what started is stopped again if a later one fails.
    pub fn start(
        services: &[Service],
        root: &Path,
        log_dir: &Path,
        path_env: &str,
    ) -> Result<Self, Reason> {
        let mut set = Self {
            bridges: Vec::new(),
            servers: Vec::new(),
            dirs: Vec::new(),
        };
        for service in services {
            let failed = |detail: String| Reason::ServiceFailed {
                name: service.name().to_owned(),
                detail,
            };
            let dir = socket_dir(root, service);
            let path = socket_path(root, service);
            if path.as_os_str().len() > SOCKET_PATH_LIMIT {
                return Err(failed(format!(
                    "its socket path is too long: {}",
                    path.display()
                )));
            }
            std::fs::create_dir_all(&dir).map_err(|e| failed(e.to_string()))?;
            set.dirs.push(dir.clone());
            // Only this user can reach the socket, whatever its own permissions.
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            match service {
                Service::Bridge { port, .. } => set
                    .bridges
                    .push(start_bridge(&path, *port).map_err(failed)?),
                Service::Command { start, .. } => {
                    let log = log_dir.join(format!("service-{}.log", service.name()));
                    set.servers
                        .push(start_server(start, &dir, &path, &log, path_env).map_err(failed)?);
                }
            }
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn bridge(name: &str, port: u16) -> Service {
        Service::Bridge {
            name: name.into(),
            port,
            socket: None,
        }
    }

    fn var(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn settings_are_checked_before_they_reach_a_process() {
        assert!(validate(&[bridge("db", 5432)], &[var("DATABASE_URL", "x")]).is_ok());
        for bad in [
            "",
            "Bad",
            "a b",
            "a/b",
            "../x",
            "this-name-is-far-too-long-to-use",
        ] {
            assert!(validate(&[bridge(bad, 5432)], &[]).is_err(), "{bad}");
        }
        assert!(validate(&[bridge("db", 5432), bridge("db", 5433)], &[]).is_err());
        assert!(validate(&[bridge("db", 0)], &[]).is_err());
        assert!(
            validate(
                &[Service::Bridge {
                    name: "db".into(),
                    port: 1,
                    socket: Some("../x".into())
                }],
                &[]
            )
            .is_err()
        );
        assert!(
            validate(
                &[Service::Command {
                    name: "db".into(),
                    start: "  ".into(),
                    socket: None
                }],
                &[]
            )
            .is_err()
        );
        for bad in ["", "lower", "1X", "A B", "A=B"] {
            assert!(validate(&[], &[var(bad, "x")]).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_project_cannot_set_credentials_or_keel_variables() {
        for name in [
            "PATH",
            "HOME",
            "GH_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_X",
            "GIT_SSH_COMMAND",
            "KEEL_SOCKET",
            "SSH_AUTH_SOCK",
        ] {
            assert!(validate(&[], &[var(name, "x")]).is_err(), "{name}");
        }
    }

    #[test]
    fn env_values_get_socket_paths_once() {
        let services = [bridge("db", 5432)];
        let root = Path::new("/r");
        let env = resolve_env(
            &[
                var("A", "host={{socketDir:db}} port=5432"),
                var("B", "{{socket:db}}"),
                var("C", "{{socket:none}} {{other}} {{open"),
            ],
            &services,
            root,
        );
        assert_eq!(env[0].1, "host=/r/db port=5432");
        assert_eq!(env[1].1, "/r/db/db.sock");
        assert_eq!(env[2].1, "{{socket:none}} {{other}} {{open");
    }

    #[test]
    fn postgres_style_sockets_keep_their_required_name() {
        let service = Service::Bridge {
            name: "db".into(),
            port: 5432,
            socket: Some(".s.PGSQL.5432".into()),
        };
        assert_eq!(
            socket_path(Path::new("/r"), &service),
            Path::new("/r/db/.s.PGSQL.5432")
        );
    }

    #[test]
    fn a_bridge_forwards_to_its_port_and_stops_cleanly() {
        keel_harness::needs!(sockets);
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = server.accept() {
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 16];
                    while let Ok(read) = stream.read(&mut buffer) {
                        if read == 0 || stream.write_all(&buffer[..read]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let services = [bridge("db", port)];
        let set = ServiceSet::start(&services, dir.path(), dir.path(), "/usr/bin:/bin").unwrap();
        let path = socket_path(dir.path(), &services[0]);
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"ping").unwrap();
        let mut answer = [0u8; 4];
        client.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"ping");
        assert_eq!(
            std::fs::metadata(socket_dir(dir.path(), &services[0]))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(set);
        assert!(!path.exists());
    }

    #[test]
    fn a_bridge_to_nothing_says_so() {
        keel_harness::needs!(sockets);
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let Err(Reason::ServiceFailed { name, detail }) = ServiceSet::start(
            &[bridge("db", closed)],
            dir.path(),
            dir.path(),
            "/usr/bin:/bin",
        ) else {
            panic!("should fail");
        };
        assert_eq!(name, "db");
        assert!(detail.contains(&closed.to_string()));
    }

    #[test]
    fn a_socket_path_too_long_for_the_system_is_refused_with_the_reason() {
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(120)));
        let Err(Reason::ServiceFailed { detail, .. }) =
            ServiceSet::start(&[bridge("db", 1)], &long, &long, "/usr/bin")
        else {
            panic!("should fail");
        };
        assert!(detail.contains("too long"));
    }

    const SERVER: &str = "import os, socket, time\ns = socket.socket(socket.AF_UNIX)\ns.bind(os.environ['KEEL_SOCKET'])\ns.listen(4)\nprint('up', flush=True)\nwhile True:\n    c, _ = s.accept()\n    try:\n        c.sendall(b'hello')\n    except OSError:\n        pass\n    c.close()\n";

    #[test]
    fn a_command_service_is_started_waited_for_and_stopped() {
        keel_harness::needs!(sockets);
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, SERVER).unwrap();
        let service = Service::Command {
            name: "db".into(),
            start: format!("exec python3 {}", script.display()),
            socket: None,
        };
        let root = dir.path().join("r");
        let path = socket_path(&root, &service);
        let set = ServiceSet::start(
            std::slice::from_ref(&service),
            &root,
            dir.path(),
            "/usr/bin:/bin:/opt/homebrew/bin",
        )
        .unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let mut answer = [0u8; 5];
        client.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"hello");
        let pid_file = socket_dir(&root, &service).join(PID_FILE);
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        drop(set);
        assert!(!process::signal_process(pid, 0), "the server is gone");
        assert!(!path.exists() && !pid_file.exists());
    }

    #[test]
    fn a_command_service_that_dies_reports_its_output() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let service = Service::Command {
            name: "db".into(),
            start: "echo cannot-start-db; exit 3".into(),
            socket: None,
        };
        let Err(Reason::ServiceFailed { detail, .. }) = ServiceSet::start(
            &[service],
            &dir.path().join("r"),
            dir.path(),
            "/usr/bin:/bin",
        ) else {
            panic!("should fail");
        };
        assert!(detail.contains("cannot-start-db"), "{detail}");
    }

    #[test]
    fn a_server_left_by_a_crash_is_stopped_before_starting_again() {
        keel_harness::needs!(sockets);
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, SERVER).unwrap();
        let service = Service::Command {
            name: "db".into(),
            start: format!("exec python3 {}", script.display()),
            socket: None,
        };
        let root = dir.path().join("r");
        let first = ServiceSet::start(
            std::slice::from_ref(&service),
            &root,
            dir.path(),
            "/usr/bin:/bin:/opt/homebrew/bin",
        )
        .unwrap();
        let pid_file = socket_dir(&root, &service).join(PID_FILE);
        let old: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        std::mem::forget(first); // The app died: nobody stopped it.
        let second = ServiceSet::start(
            std::slice::from_ref(&service),
            &root,
            dir.path(),
            "/usr/bin:/bin:/opt/homebrew/bin",
        )
        .unwrap();
        let new: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(old, new);
        // The new server answers. The old one was asked to stop before it started.
        let mut client = UnixStream::connect(socket_path(&root, &service)).unwrap();
        let mut answer = [0u8; 5];
        client.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"hello");
        drop(second);
    }
}
