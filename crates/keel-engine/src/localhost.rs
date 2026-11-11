//! Whether a check failed because the sandbox refused its tests a server on localhost (#123). A
//! project opens localhost to its tests with a setting of its own, closed until a person opens it,
//! and a check that needed it says so, so the person knows where to look. Only what Node, Python
//! and Go print when that happens counts: a refused connection elsewhere has nothing to do with it.

/// Whether what a check printed says a server on localhost was refused.
pub fn refused(text: &str) -> bool {
    // Python names the call before the error: the line the failing call is on, and in 3.11 and
    // later the marks under it.
    let mut bind_before = false;
    for line in text.lines() {
        let line = line.to_ascii_lowercase();
        if refused_in(&line) || (bind_before && python_refused(&line)) {
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
            "test result: FAILED. 3 passed; 1 failed",
            "",
        ] {
            assert!(!refused(text), "{text}");
        }
    }
}
