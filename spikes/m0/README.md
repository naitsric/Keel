# M0 spike

Go/no-go checks for driving Claude Code from Keel, run against the real CLI on synthetic
repos. Findings are in [`docs/m0-report.md`](../../docs/m0-report.md).

```bash
python3 -I spikes/m0/run.py --work /some/scratch/dir
python3 -I spikes/m0/run.py --work /some/scratch/dir --only probe_clone
```

Needs a signed-in `claude`, `gh`, `git` and `uv`. It uses your subscription (about $0.02
of Haiku per run). Run it again with every new Claude Code version.

- `probes/sandbox.sh` runs inside the sandbox and tries to reach credentials, the network
  and git state. It sends every output to `/dev/null`, so a probe that gets through still
  prints only `REACHED`.
- `results/<platform>.json` holds the outcomes.
- `fixtures/` holds scrubbed `stream-json` captures for the harness tests.

`run.py` refuses to write a fixture containing your account details, home path or a token.
