# Keel

A local software factory on top of [Claude Code](https://claude.com/product/claude-code). You
define the workflow and pick the model for each step. Keel runs the agents in isolation, shows
what each step did, and keeps an audit record you can check. A person always decides what ships.

**Status: early, macOS only.** Keel takes a request from the app or a GitHub issue and works out
what it will change. If something can only be decided by you, the run stops and asks. It recommends
an option only when the code or the request backs one up, and taking it costs nothing. When nothing
does, such as an amount or a business rule, it recommends none and you choose. A builder does the work and a check runs. Then
an agent goes through what was asked and says what a test really proves and what nothing does.
Reviewers look at the change at the same time, and an approver can send the work back, at most
twice, and every review runs again. Then a person approves, and Keel opens a draft pull request. At
that approval you can also change anything it decided on its own, which sends the work back without
starting over. You can edit each project's workflows in the app. See the
[M0 report](docs/m0-report.md) for what's behind the isolation.

Keel looks up what became of each pull request it opened, and never writes to one again. The
**Workflows** page compares workflows with that: what got merged, at what cost, how often work went
back, and how often a run stopped to ask.

## A workflow that fits the change

The built-in **Fits the change** skips the reviews and the approver when a change is small. Keel
decides that itself from what it can see, and an agent or an issue saying so doesn't make it so:

- What was asked was worked out as small, with no risks named, such as money or personal data.
- The project's check passed, on this code. With no check, nothing is ever small.
- The run doesn't come from an issue by anyone other than the project's GitHub account, or from an
  assistant over MCP.
- The change touches nothing sensitive: `.github/`, build and dependency files, test settings,
  migrations, SQL schema, sign-in and permissions, secrets, environment files, lock files, and
  whatever else a project lists in **Settings**.
- The change is under 400 lines.
- The check against what was asked found nothing missing.

Your approval, the sandbox and the check are never skipped. The approval says what didn't run and
why, with a button to run it now. Once a change gets the full review, it keeps getting it for the rest
of the run. This workflow isn't the default: you choose it.

## The Brain

Each project has a small Brain, kept in the app on this Mac, and every agent gets it. It holds at
most about 4 KB, so when it's full, adding something means retiring something.

- **Map:** an agent writes it from the code when you add the project. Each line cites files and a
  commit, and it's marked out of date when those files change.
- **Decisions:** what you say when approving or rejecting a change. Agents follow these first.
- **Learned:** at the end of a run, an agent can propose up to 3 things it learned, with evidence.
  They only go in if you accept them.

## How it isolates agents

- Each run works in its own private clone. Your repo isn't touched.
- Agents can't read your home folder, including your credentials, or reach the network
  beyond the domains a project allows. `docs/m0-report.md` has the tests behind this.
- Keel uses your own signed-in `claude` and `gh`, and never stores a credential. Agents never
  get GitHub access: Keel pushes the branch and opens the pull request itself, as a draft. It
  doesn't merge.
- Git would run whatever a repo's `.git/config` names, so no step can change that file or the
  hooks, and everything Keel runs with git on a run's copy goes in the same sandbox.
- Services a project needs, like a test database, run outside the sandbox while a step uses
  them, and commands reach them only through a Unix socket.
- A setup command installs dependencies once per run, before any agent. It's the one step with
  internet access, and it still can't reach your home folder or anything else on your Mac.

## Secrets in what it shows

Keel hides known secrets when it shows a run, when an assistant reads one over MCP, and in a pull
request's title and description. The audit record and what agents receive keep the original.

It goes by known token shapes and names like `API_KEY`, so some secrets will slip through. It also
doesn't stop a secret an agent committed from being pushed with the branch, so review the draft
before merging.

## Install

Download the latest `.dmg` from [Releases](https://github.com/naitsric/Keel/releases/latest) and
drag Keel to Applications.

Builds aren't notarized yet, so macOS blocks the first launch. Run this once after installing:

```sh
xattr -dr com.apple.quarantine /Applications/Keel.app
```

You need:

- macOS 13 or later, on Apple Silicon
- [Claude Code](https://claude.com/product/claude-code), signed in
- Git
- [GitHub CLI](https://cli.github.com), signed in. The built-in workflows end in a pull request, so they need it. Without it, only a single step or a custom workflow with no delivery can run.

The first screen checks these and says how to get what's missing.

## Develop

```bash
pnpm install
pnpm app              # run the app
pnpm test             # UI tests
pnpm check:i18n       # English and Spanish stay complete
cargo test --workspace
```

`pnpm dev`, then `/dev/preview.html`, shows the real screens with made-up data, for example
`/dev/preview.html?view=run&state=stalled&lang=es`.

## Connect an assistant over MCP

Keel has a built-in [MCP](https://modelcontextprotocol.io) server, so an assistant can see your projects and start runs while you watch them in the app. Only this Mac can reach it, and it's off until you turn it on.

1. In **Settings → MCP server**, turn it on.
2. Press **Connect Claude Code**. A new Claude Code session sees it on its own. In one that's already open, run `/mcp`.

For another client, create a token there and give it the URL and the header it shows, once.

A client can read projects, workflows, runs, issues and the Brain. If you allow it, it can also start and stop runs, close the runs it started, and propose workflows. It can't approve or reject a step, answer a step's questions, merge, change settings, save a workflow or accept anything into the Brain: a run that needs you waits for you in the app. **Settings** shows every call, and revoking a client cuts it off at once.

A client that can start runs can also hand out work, the way an assistant hands it to helpers. It splits a request into runs, and into read-only tasks that only look into something, waits on several at once, and reads what each one did. Each client can keep 3 runs open at a time, counting those that wait for you, and you can change that for it in **Settings**, up to 10. A client frees a place by closing a run it started and won't take further, such as one that failed: nothing is delivered, and its card goes back to the Backlog. If you also let it give feedback there, it can send work back with instructions when it reaches your approval, and retry a step that failed or stopped. The run says which client did it.

A workflow a client proposes is a draft on the **Workflows** page. It shows the commands it would run, the agents that could change files, and any work that would happen after your approval. Nothing can run it until you save it, you can change it first, and saving always creates a new workflow rather than replacing one.

A project a client proposes waits in a bar at the top of the window, with its folder and GitHub repository. It isn't added, and nothing can run on it, until you add it. Then you choose its account and commands as with any other, and its map starts. All a client can name is the folder.

Connecting stores the token in Claude Code's own configuration, `~/.claude.json`, as it does for any MCP server.

## License

[Apache-2.0](LICENSE)
