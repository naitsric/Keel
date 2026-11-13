//! Whether a check failed because the sandbox refused its tests a server on localhost (#123). A
//! project opens localhost to its tests with a setting of its own, closed until a person opens it,
//! and a check that needed it says so, so the person knows where to look. Only what Node, Python,
//! Go, Java and Gradle print when that happens counts: a refused connection elsewhere has nothing to
//! do with it.

/// How far below Java's error the call that failed is named.
const JAVA_FRAMES: usize = 3;

/// Whether what a check printed says a server on localhost was refused.
pub fn refused(text: &str) -> bool {
    let lines: Vec<String> = text.lines().map(str::to_ascii_lowercase).collect();
    // Python names the call before the error: the line the failing call is on, and in 3.11 and
    // later the marks under it.
    let mut bind_before = false;
    for (at, line) in lines.iter().enumerate() {
        if refused_in(line)
            || (bind_before && python_refused(line))
            || (java_refused(line)
                && lines[at + 1..]
                    .iter()
                    .take(JAVA_FRAMES)
                    .any(|frame| java_binds(frame)))
        {
            return true;
        }
        let marks = line.trim();
        if marks.is_empty() || !marks.chars().all(|c| matches!(c, '^' | '~')) {
            bind_before = line.contains(".bind(");
        }
    }
    false
}

/// One line, in lower case, that says it on its own.
fn refused_in(line: &str) -> bool {
    // Node, starting a server: `listen EPERM: operation not permitted 127.0.0.1`.
    line.contains("listen eperm")
        || line.contains("bind eperm")
        // Node, reaching one: `connect EPERM 127.0.0.1:5432`. Anywhere else isn't localhost.
        || line.split("connect eperm ").skip(1).any(loopback)
        // Python's asyncio, and what's built on it, such as uvicorn.
        || (line.contains("error while attempting to bind on address")
            && line.contains("operation not permitted"))
        // Go: `listen tcp 127.0.0.1:0: bind: operation not permitted`.
        || line.contains("bind: operation not permitted")
        // Gradle, which needs a socket on localhost to start at all.
        || line.contains("could not create service of type filelockcontentionhandler")
}

/// Java's error, which says nothing of where: the call under it does.
fn java_refused(line: &str) -> bool {
    ["java.net.socketexception", "java.net.bindexception"]
        .iter()
        .any(|error| line.contains(&format!("{error}: operation not permitted")))
}

/// A call that binds a socket, which is always on this Mac.
fn java_binds(frame: &str) -> bool {
    let frame = frame.trim_start();
    frame.starts_with("at ") && (frame.contains(".bind0(") || frame.contains(".bind("))
}

fn loopback(address: &str) -> bool {
    ["127.", "::1", "localhost"]
        .iter()
        .any(|start| address.starts_with(start))
}

/// Python's own error, with no file after it: a file it couldn't open would be named.
fn python_refused(line: &str) -> bool {
    line.trim_end()
        .ends_with("permissionerror: [errno 1] operation not permitted")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `node --test` printed in a check, starting a server on a port the system picks.
    const NODE_LISTEN: &str = "✖ a server the test starts on localhost answers it (2.307459ms)
  Error: listen EPERM: operation not permitted 127.0.0.1
      at Server.setupListenHandle [as _listen2] (node:net:2368:21)
      at listenInCluster (node:net:2499:12)
    code: 'EPERM',
    errno: -1,
    syscall: 'listen',
    address: '127.0.0.1'
";

    #[test]
    fn a_server_node_was_refused_on_localhost() {
        assert!(refused(NODE_LISTEN));
        for line in [
            "Error: listen EPERM: operation not permitted ::1",
            "Error: listen EPERM: operation not permitted 0.0.0.0",
            "Error: bind EPERM 0.0.0.0",
            "  [cause]: Error: connect EPERM 127.0.0.1:5432 - Local (0.0.0.0:0)",
            "TypeError: fetch failed connect EPERM ::1:3000 - Local (:::0)",
            "Error: connect EPERM localhost:8080",
        ] {
            assert!(refused(line), "{line}");
        }
    }

    #[test]
    fn a_server_python_was_refused_on_localhost() {
        // asyncio, and what's built on it, such as uvicorn.
        assert!(refused(
            "PermissionError: [Errno 1] error while attempting to bind on address ('127.0.0.1', 0): operation not permitted"
        ));
        // `socketserver`, `http.server` and what's built on them, as Python and pytest print it.
        for text in [
            "  File \"/lib/python3.9/socketserver.py\", line 466, in server_bind\n    self.socket.bind(self.server_address)\nPermissionError: [Errno 1] Operation not permitted\n",
            ">       s.bind((\"127.0.0.1\", 0))\nE       PermissionError: [Errno 1] Operation not permitted\n\ntest_server.py:7: PermissionError\n",
            "    s.bind((\"127.0.0.1\", 0))\n    ~~~~~~^^^^^^^^^^^^^^^^^^\nPermissionError: [Errno 1] Operation not permitted\n",
        ] {
            assert!(refused(text), "{text}");
        }
    }

    /// Gradle, which can't start without a socket on localhost, and a server Java starts: what it
    /// binds is always on this Mac.
    #[test]
    fn gradle_and_a_server_java_were_refused_on_localhost() {
        for text in [
            "* What went wrong:\nGradle could not start your build.\n> Could not create service of type FileLockContentionHandler using BasicGlobalScopeServices.createFileLockContentionHandler().\n   > java.net.SocketException: Operation not permitted\n",
            "Caused by: java.net.SocketException: Operation not permitted\n\tat java.base/sun.nio.ch.Net.bind0(Native Method)\n\tat java.base/sun.nio.ch.Net.bind(Net.java:525)\n",
            "java.net.BindException: Operation not permitted\n    at java.base/sun.nio.ch.Net.bind0(Native Method)\n",
        ] {
            assert!(refused(text), "{text}");
        }
    }

    #[test]
    fn a_server_go_was_refused_on_localhost() {
        assert!(refused(
            "--- FAIL: TestServer (0.00s)\n    server_test.go:12: listen tcp 127.0.0.1:0: bind: operation not permitted\n"
        ));
    }

    /// Everything else a sandbox refuses, a server out on the internet included, has nothing to do
    /// with the setting, so it mustn't be pointed at.
    #[test]
    fn nothing_else_the_sandbox_refuses_counts() {
        for text in [
            "Error: connect EPERM 93.184.216.34:443 - Local (0.0.0.0:0)",
            "Error: listen EADDRINUSE: address already in use 127.0.0.1:3000",
            "cat: /Users/me/.ssh/id_ed25519: Operation not permitted",
            "    with open(path) as f:\nPermissionError: [Errno 1] Operation not permitted: '/Users/me/.npmrc'\n",
            // A connection Python was refused could be to anywhere.
            "    sock.connect(sa)\nPermissionError: [Errno 1] Operation not permitted\n",
            "s.bind((\"127.0.0.1\", 0))\nall fine\nPermissionError: [Errno 1] Operation not permitted\n",
            "npm error code ENOTCACHED",
            // A connection Java was refused could be to anywhere too.
            "java.net.SocketException: Operation not permitted\n\tat java.base/sun.nio.ch.Net.connect0(Native Method)\n\tat java.base/sun.nio.ch.Net.connect(Net.java:589)\n",
            "java.net.UnknownHostException: repo.maven.apache.org",
            "test result: FAILED. 3 passed; 1 failed",
            "",
        ] {
            assert!(!refused(text), "{text}");
        }
    }
}
