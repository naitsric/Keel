# Security

## Reporting

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/naitsric/Keel/security/advisories/new),
not in an issue.

## What Keel protects

**Your Mac from the agents.** Each run works in its own clone, in Claude Code's sandbox with a
policy of Keel's: no home folder, no credentials, no network beyond the domains a project allows,
and no localhost unless a project opens it. Agents can't change a repo's `.git/config` or its hooks,
and never get GitHub access: Keel pushes and opens the pull request itself. The details are in the
README, under [How it isolates agents](README.md#how-it-isolates-agents), and the tests behind them
in [docs/m0-report.md](docs/m0-report.md).

**What ships from what an agent says.** Nothing is pushed until a person approves it, and then only
as a draft pull request. Keel never merges. An agent's claim that a change is small, or safe, or
already reviewed doesn't make it so: Keel decides what to skip from what it can see itself.

**The record.** The audit log is append-only, and the store's triggers refuse to rewrite an event or
what identifies a step once it started. The raw output of each step and its SHA-256 back it up. This
keeps a bug in the app from rewriting history. It doesn't protect against someone who owns the Mac.

**Keel from programs outside.** The MCP server is off until you turn it on, and listens on
`127.0.0.1` only. It refuses web pages (any `Origin`) and hosts that aren't this Mac, and every
request needs the token of a client you created, of which only a hash is kept. A client can't
approve, answer, merge, change what a client may do or let anything into the Brain: there is no tool
for it. Nor can it change settings, with one exception, off until you turn it on for a client: it can
add projects and set their GitHub account and their setup and check commands. Each such change is
recorded with the client and the values before and after, and shown in Settings. Every call is
recorded. Agents that Keel runs can't reach it.

## What it does not

**An agent can be misled.** The text of an issue, a repo's files and what a tool prints can be
written to steer an agent, and reviewers and the approver are agents too. Keel fences what comes
from outside and tells agents not to follow it, which is no guarantee. Read the change, not only
what the agents said about it, and look at the draft pull request on GitHub before merging.

**Secrets are hidden by their shape.** Keel masks known token shapes and names like `API_KEY` in
what it shows, so some will slip through. It doesn't stop a secret an agent committed from being
pushed with the branch.

**Some steps reach further.** Setup has internet access, to install dependencies. A project that
opens localhost to its check and builder lets them reach any service on your Mac. Services a
project declares, like a test database, run outside the sandbox. Open only what a project needs.

**Your own tools are trusted.** Keel runs your signed-in `claude` and `gh`, and the commands you set
for each project, as they are. A client you let add and configure projects sets those commands too,
without asking you each time: they run in the same sandbox, with whatever you opened to that project.
Let only a client you trust.
