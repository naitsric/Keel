# Keel

A local software factory on top of [Claude Code](https://claude.com/product/claude-code). You
define the workflow and pick the model for each step. Keel runs the agents in isolation, shows
what each step did, and keeps an audit record you can check. A person always decides what ships.

**Status: early, macOS only.** Keel takes a request from the app or a GitHub issue through a spec
writer, a builder and a check, then has reviewers look at the change at the same time. An approver
can send the work back, at most twice, and every review runs again. Then a person approves, and
Keel opens a draft pull request. You can edit each project's workflows in the app. The rest of the
per-project Brain is on the way. See the [M0 report](docs/m0-report.md) for what's behind the
isolation.

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

## Requirements

- macOS 13 or later, on Apple Silicon
- [Claude Code](https://claude.com/product/claude-code), signed in
- Git
- [GitHub CLI](https://cli.github.com), signed in, to open pull requests

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

## License

[Apache-2.0](LICENSE)
