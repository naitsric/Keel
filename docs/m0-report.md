# M0 report

Tested on macOS (Apple Silicon) with Claude Code 2.1.318 and a Max subscription. Raw
outcomes: [`spikes/m0/results/darwin.json`](../spikes/m0/results/darwin.json).
**Linux has not been tested.**

## Verdict

Keel can run on Claude Code with native isolation on macOS, with two design changes and
one open problem for database tests.

## Answers

| Question | Result | Consequence |
|---|---|---|
| Does denying credential paths isolate an agent? | **No.** With `~/.ssh`, `~/.config/gh` and others denied, sandboxed Bash still read the `gh` and Claude Code tokens from Keychain through `security`. | Deny the whole home folder and allow only what a project needs. With that policy, every credential probe was blocked. |
| Can an agent in a git worktree change shared state? | **Yes.** `git update-ref refs/heads/main HEAD` moved `main` in the main checkout. | No worktrees. Each run gets its own clone, from a bare mirror owned by the app. |
| Is a per-run clone safe? | **Yes, in a real install layout.** The agent could not push to the mirror, and the app could fetch the run branch from the clone. `.git/config` and `.git/hooks` stayed read-only. | The app pushes from the mirror, so credentials never touch a repo an agent can write to. |
| Does `--restricted` work with a subscription? | **Yes.** It also stopped a planted hook, MCP server and skill, all of which ran in the default mode. | Use it for every step. It skips `CLAUDE.md` too, so Keel injects that with the Brain. `--append-system-prompt-file` works under it. |
| Can tests reach a database from the sandbox? | **Partly.** See below. | Open problem. |
| Can an agent run tests in the background? | **No.** `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` removes the parameter. | This closes what jammed the Archon pilot. |
| Which auth is in use? | `apiKeySource` is `none` with a subscription and `ANTHROPIC_API_KEY` with a key. | The harness can stop a step that uses the wrong one. |

### Probes under the final policy

Deny home, per-run clone, data under the home folder.

**Blocked:** `gh auth token`, both Keychain entries, `~/.ssh`, `~/.config/gh`,
`~/.claude.json`, tokens in the environment, the SSH agent, the Docker socket, undeclared
domains (including `github.com`), raw sockets, loopback TCP, writes to home, writes to
`.git/config` and `.git/hooks`, push to the mirror, and reads of the mirror and of another
run's folder.

**Allowed, as intended:** a declared domain, writes inside the clone, commits, and moving
refs in the clone, which affects nothing else.

### Databases

| Sandbox setting | TCP to a local server | Unix socket to a local server |
|---|---|---|
| Default | Blocked | Blocked |
| `allowUnixSockets: [dir]` | Blocked | **Works** |
| `allowLocalBinding: true` | Works | Blocked |
| `localhost` in `allowedDomains` | Blocked | Blocked |

- `allowUnixSockets` opens only the listed path, so it is the right choice on macOS.
- `allowLocalBinding` opens every port on localhost, according to the docs, including other
  local services. Keel should not use it.
- A database in Docker publishes only TCP, so it needs a bridge from a socket to its port.
- Starting Postgres inside the sandbox failed at `initdb`. The cause was not investigated.
- On Linux, the docs say `allowLocalBinding` has no effect and describe `allowUnixSockets` as
  macOS only. `allowAllUnixSockets` opens every socket, including Docker's, so Linux has no
  safe option yet.

### Decision

Keel starts each project's services outside the sandbox and connects them by Unix socket, with
a bridge for databases in Docker. Linux waits until it has a safe option, so v0.1 is macOS only.
A real agent and a real check have since reached Postgres in Docker this way, with direct TCP
still blocked (`crates/keel-engine/examples/services_e2e.rs`).

## Findings for the harness

- `subtype` is not enough. A bad API key returned `subtype: success` with `is_error: true`.
- SIGINT exits with 0 and writes a `result` with `error_during_execution`. SIGTERM exits with
  143 and writes no `result`. Stop with SIGINT first.
- A failed command appears as `is_error: true` with the text `Exit code N`, so no hook is
  needed for exit codes.
- `--max-budget-usd` stops after the first request, and the run can exceed the cap.
- `rate_limit_event` reports usage limits and reset times, which the UI can show.
- `--json-schema` returns the value in `structured_output`.

## Not resolved

- **Linux:** nothing was run there.
- **Toolchains in the home folder:** `uv` works, through a setup step with internet access that
  runs before any agent, and a cache each run keeps in its private copy. `cargo` and `nvm` haven't
  been tried, and will likely need their folders listed in the project's settings.
- **Parallel Bash:** one attempt ran in sequence, even when asked for parallel calls. That
  does not prove it can't happen, so the engine keeps its one-Bash-step rule.
- **Separate Claude accounts:** a fresh `CLAUDE_CONFIG_DIR` reports logged out, but no second
  login was tried.
- **Launch from Finder:** needs a built app, so it moves to M1.
- **Fixtures not captured:** API retries and a real rate limit.
- A large real test suite has not run under this policy. A small one with `uv`, `pytest` and Postgres has.
