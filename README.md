# Keel

A local software factory on top of [Claude Code](https://claude.com/product/claude-code). You
define the workflow and pick the model for each step. Keel runs the agents in isolation, shows
what each step did, and keeps an audit record you can check. A person always decides what ships.

**Status: early, macOS only.** This version runs one agent step at a time, visible and audited.
Workflows, reviewers, pull requests and the per-project Brain are on the way. See the
[M0 report](docs/m0-report.md) for what has been tested.

## How it isolates agents

- Each run works in its own private clone. Your repo isn't touched.
- Agents can't read your home folder, including your credentials, or reach the network
  beyond the domains a project allows. `docs/m0-report.md` has the tests behind this.
- Keel uses your own signed-in `claude` and never stores a credential.

## Requirements

- macOS 13 or later, on Apple Silicon
- [Claude Code](https://claude.com/product/claude-code), signed in
- Git

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
