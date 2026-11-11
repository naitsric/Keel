//! For tests that need what a sandbox takes away. When Keel works on itself, its tests run inside
//! a step's sandbox, and there macOS refuses to start another sandbox, to open local sockets and
//! to run setuid programs such as `ps`. Those tests say so and stop, instead of failing for a
//! reason that has nothing to do with the change. Anywhere else they run in full.
//!
//! It also writes the scripts tests run in place of `claude` and other tools.

use std::io::{ErrorKind, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// What `sandbox-exec` says when macOS won't start a sandbox inside another one.
pub const REFUSAL: &str = "sandbox_apply: Operation not permitted";

/// Why this process can't start a sandbox, or `None` if it can. Only the refusal macOS gives inside
/// another sandbox counts: anything else is a real failure, and the test has to show it.
pub fn sandbox_refused() -> Option<&'static str> {
    static WHY: OnceLock<Option<String>> = OnceLock::new();
    WHY.get_or_init(|| {
        let output = Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                "(version 1)(allow default)(deny network*)",
                "/usr/bin/true",
            ])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        (!output.status.success() && stderr.contains(REFUSAL)).then(|| {
            format!(
                "it starts a sandbox, and macOS refuses that inside the one this runs in ({})",
                stderr.trim()
            )
        })
    })
    .as_deref()
}

/// Why this process can't open local sockets, or `None` if it can. A check's sandbox has no
/// network at all, and that includes loopback and Unix sockets. Only a refusal counts, as above.
pub fn sockets_refused() -> Option<&'static str> {
    static WHY: OnceLock<Option<String>> = OnceLock::new();
    WHY.get_or_init(|| {
        let refused = |error: std::io::Error| {
            (error.kind() == ErrorKind::PermissionDenied).then(|| {
                format!(
                    "it opens local sockets, and the sandbox this runs in refuses them ({error})"
                )
            })
        };
        if let Err(error) = TcpListener::bind(("127.0.0.1", 0)) {
            return refused(error);
        }
        // Short, as socket paths have to be.
        let dir = std::env::temp_dir().join(format!("keel-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let bound = UnixListener::bind(dir.join("s"));
        let _ = std::fs::remove_dir_all(&dir);
        bound.err().and_then(refused)
    })
    .as_deref()
}

/// Why this process can't run `/bin/ps`, or `None` if it can. It's setuid root, and a check's
/// sandbox won't run anything setuid. Only that refusal counts, as above.
pub fn ps_refused() -> Option<&'static str> {
    static WHY: OnceLock<Option<String>> = OnceLock::new();
    WHY.get_or_init(|| {
        let error = Command::new("/bin/ps")
            .args(["-o", "pid=", "-p", &std::process::id().to_string()])
            .output()
            .err()?;
        (error.kind() == ErrorKind::PermissionDenied).then(|| {
            format!(
                "it runs /bin/ps, which is setuid, and the sandbox this runs in refuses that ({error})"
            )
        })
    })
    .as_deref()
}

/// Says which test stopped and why. Straight to stderr, which the test harness doesn't capture, so
/// it shows even though the test counts as passed.
pub fn skipped(why: &str) {
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("a test");
    let _ = writeln!(std::io::stderr(), "skipped {test}: {why}");
}

/// Writes an executable script for a test, such as a fake `claude`. macOS checks a new executable
/// the first time it runs, and on a busy Mac that has taken from under a second to minutes, while
/// later runs of the same file take milliseconds, even after it's rewritten. So the file first runs
/// once as an empty script, here, where nothing is timed, and only then gets `text`. A test's time
/// limit then measures the script, not that check, and the script itself never runs early.
pub fn write_script(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, "#!/bin/sh\n")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Command::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    std::fs::write(path, text)
}

/// Ends the test right here, saying why, if the sandbox it runs in takes away what it needs:
/// `sandbox` to start one of its own, `sockets` to open local sockets, `ps` to run `/bin/ps`.
#[macro_export]
macro_rules! needs {
    (sandbox) => {
        if let Some(why) = $crate::testing::sandbox_refused() {
            $crate::testing::skipped(why);
            return;
        }
    };
    (sockets) => {
        if let Some(why) = $crate::testing::sockets_refused() {
            $crate::testing::skipped(why);
            return;
        }
    };
    (ps) => {
        if let Some(why) = $crate::testing::ps_refused() {
            $crate::testing::skipped(why);
            return;
        }
    };
    ($first:ident, $($rest:ident),+) => {
        $crate::needs!($first);
        $crate::needs!($($rest),+);
    };
}
