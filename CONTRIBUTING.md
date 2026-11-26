# Contributing to Keel

Keel is a Tauri 2 app: a React and TypeScript UI over a Rust engine that drives Claude Code. This
guide covers running it from source, testing it, and how the code is organized.

## Requirements

- macOS 13 or later on Apple Silicon, with Xcode Command Line Tools
- Rust (`rustup`), Node 24 and pnpm
- [Claude Code](https://claude.com/product/claude-code) and the [GitHub CLI](https://cli.github.com),
  signed in, to run real runs

## Develop

```sh
pnpm install
pnpm app              # the app, built from the branch you're on
pnpm dev              # the UI alone, in a browser, with made-up data
```

`pnpm app` runs a Keel built from the branch you're on, next to the one you installed, and the two
share nothing either writes. The branch build keeps its data in
`~/Library/Application Support/io.github.naitsric.keel.dev` and its runs in `~/.keel-dev`, and its
MCP server is `keel-dev`, on port 7414 unless you change it. It never opens the installed Keel's
database. It does use your `claude` and `gh`, so its runs spend your Claude usage and its pull
requests are real. Delete those two folders to start it from nothing. Two branch builds share them,
so only one runs at a time.

`pnpm dev`, then `/dev/preview.html`, shows the real screens with made-up data, for example
`/dev/preview.html?view=run&state=stalled&lang=es`. `dev/preview.tsx` lists what each parameter
shows.

## Test

```sh
pnpm typecheck
pnpm test             # UI tests (vitest)
pnpm check:i18n       # English and Spanish stay complete
pnpm build            # the app needs it to compile
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

That's what CI runs, in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

Inside a sandbox, as when Keel runs this project's check, macOS won't let a test start a sandbox
of its own, open a local socket or run `ps`. The tests that need one of these print
`skipped <test>: <why>` and stop, so they count as passed there. That's most of the engine's
tests. Run `cargo test --workspace` outside Keel to run them all.

`KEEL_SAMPLE_REPOS=all cargo test -p keel-engine --test samples -- --nocapture` has Keel guess the
commands of small sample repos (npm, pnpm, Yarn 1, pip, uv, Poetry, Rust, Go, Maven, Gradle,
Bundler, Composer, Swift, and an npm suite that starts its own server) and run their setup and
check in the sandbox. A sample whose tools aren't installed says so and stops. Java and Ruby come
from Homebrew's `openjdk` and `ruby`, which it keeps off the PATH, and Swift needs Xcode.
They download dependencies, so they only run when asked. `KEEL_SAMPLE_REPOS=keel` does the same
with this repo, built from scratch.

## Project layout

```
src/                 React UI
  views/             the screens: board, run, workflows, Brain, projects, settings
  commands/          commands, their keys, and Search Everywhere
  menu/              the native menu bar, built from the commands
  lib/               the backend's API, the app's state, and what screens share
  i18n/              English and Spanish
src-tauri/           the desktop shell: window, menu, notifications, MCP server's lifecycle
crates/
  keel-core/         types shared by everything, and Keel's log
  keel-store/        SQLite: projects, runs, steps and the audit log
  keel-harness/      everything Keel knows about driving Claude Code, and gh
  keel-engine/       runs workflows: specs, sandbox, checks, reviews, delivery, the Brain
  keel-hook/         what Claude Code runs around each Bash command of a step that can edit
  keel-mcp/          the MCP server: tools, tokens, audit
dev/                 the preview page, with made-up data
docs/m0-report.md    the spike behind the isolation, and its results in spikes/m0
```

## Architecture notes

**The engine has the rules.** Tauri commands are thin: what a run may do, and when, lives in
`keel-engine`. Everything runs inside the app process, without a daemon, but each agent is its own
process writing to a file, so it outlives the app and can be picked up again.

**Reasons are codes.** Nothing in the backend holds text a person reads. A step that stops or is
skipped says why with a `Reason` and its data, and the UI says it in the person's language.

**Migrations.** How the database is laid out changes one numbered migration at a time, in
`MIGRATIONS` in `crates/keel-store/src/lib.rs`. Only ever add one at the end. A database keeps a
hash of each one it ran, and Keel refuses to open one where a migration differs from its own, as
happens when two branches each add the next number, or one that has more than it knows.
`cargo test` fails if a migration that shipped is edited or moved.

**Agents are kept apart.** Each rule in `keel-harness` comes from something observed in the M0
spike: see [docs/m0-report.md](docs/m0-report.md). Change the sandbox policy only with a probe that
shows what the change allows. The security model is in [SECURITY.md](SECURITY.md).

**What a person reads is brief.** Keel's text, and what its prompts ask agents to write, should be
short, plain and free of jargon. A limit that matters goes in the engine, not only in a prompt.

## Pull requests

- Keep each one focused, and say what changed, why, and how it was tested.
- Everything under [Test](#test) passes.
- Behavior gets a test where it lives: the engine in Rust, screens in vitest, and both languages in
  `src/i18n`.
