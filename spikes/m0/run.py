#!/usr/bin/env python3
"""M0 go/no-go spike for Keel. See spikes/m0/README.md.

Drives the real `claude` CLI against synthetic repos and records what actually happens.
Standard library only. Results and fixtures are public, so nothing personal may reach
them: secrets go to /dev/null in the probes, and fixtures pass through scrub() and a
denylist check before they are written.
"""

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
RESULTS = HERE / "results"
ENV_ALLOW = ("HOME", "PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "SHELL", "TERM")
RESULT_RE = re.compile(r"^([a-z_]+): (REACHED|BLOCKED)$", re.M)


def clean_env(**extra):
    env = {k: os.environ[k] for k in ENV_ALLOW if k in os.environ}
    env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] = "1"
    env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] = "1"
    env.update(extra)
    return env


def sh(args, cwd=None, env=None):
    return subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)


class Scrubber:
    """Removes anything that identifies the machine or its owner from fixture text."""

    def __init__(self, work: Path):
        self.pairs = [(str(work.resolve()), "/work"), (str(work), "/work"), (str(Path.home()), "/home/user")]
        user = os.environ.get("USER", "")
        if user:
            self.pairs.append((user, "user"))
        status = sh(["claude", "auth", "status", "--json"], env=clean_env())
        try:
            data = json.loads(status.stdout)
        except json.JSONDecodeError:
            data = {}
        for key in ("email", "orgName", "orgId", "accountUuid", "organizationUuid"):
            if data.get(key):
                self.pairs.append((str(data[key]), f"<{key}>"))
        self.deny = [p for p, _ in self.pairs if len(p) > 3]

    DROP = {"mcp_servers", "plugins", "agents", "slash_commands", "skills", "memory_paths", "mcp_server_errors", "plugin_errors"}
    TOKEN = re.compile(r"(sk-ant-[\w-]+|gh[opsu]_\w+|github_pat_\w+)")
    TMPDIR = re.compile(r"/(private/)?var/folders/[\w/.+-]+")

    def line(self, raw: str) -> str:
        try:
            obj = json.loads(raw)
        except json.JSONDecodeError:
            return ""
        if obj.get("type") == "system" and obj.get("subtype") == "init":
            for key in self.DROP & obj.keys():
                obj[key] = []
        text = json.dumps(obj, ensure_ascii=False)
        for old, new in self.pairs:
            text = text.replace(old, new)
        text = self.TMPDIR.sub("/tmpdir", text)
        # Account usage is personal: keep the shape, drop the numbers.
        text = re.sub(r'"utilization": ?[0-9.]+', '"utilization": 0.5', text)
        text = re.sub(r'"resetsAt": ?[0-9]+', '"resetsAt": 1800000000', text)
        return self.TOKEN.sub("<token>", text)

    def verify(self, text: str, name: str):
        hits = [d for d in self.deny if d in text] + self.TOKEN.findall(text)
        if hits:
            sys.exit(f"refusing to write fixture {name}: found identifying text")


class Run:
    def __init__(self, events, code, seconds, raw):
        self.events, self.code, self.seconds, self.raw = events, code, seconds, raw

    @property
    def init(self):
        return next((e for e in self.events if e.get("type") == "system" and e.get("subtype") == "init"), {})

    @property
    def result(self):
        return next((e for e in reversed(self.events) if e.get("type") == "result"), {})

    def tool_results(self):
        for e in self.events:
            if e.get("type") != "user":
                continue
            for block in e.get("message", {}).get("content", []) or []:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    content = block.get("content")
                    if isinstance(content, list):
                        content = "\n".join(c.get("text", "") for c in content if isinstance(c, dict))
                    yield {"is_error": bool(block.get("is_error")), "text": content or "", "extra": e.get("tool_use_result")}

    def text(self):
        return "\n".join(r["text"] for r in self.tool_results())


class Spike:
    def __init__(self, work: Path, model: str):
        self.work, self.model = work, model
        self.scrub = Scrubber(work)
        self.out = {}

    def claude(self, name, prompt, cwd, settings=None, flags=(), env=None, stop_after=None, stop_signal=signal.SIGINT, timeout=240):
        session = str(uuid.uuid4())
        args = ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose", "--model", self.model, "--session-id", session, "--no-session-persistence", *flags]
        if settings is not None:
            path = self.work / f"settings-{name}.json"
            path.write_text(json.dumps(settings))
            args += ["--settings", str(path)]
        raw_path = self.work / f"{name}.jsonl"
        started = time.monotonic()
        with raw_path.open("w") as sink:
            proc = subprocess.Popen(args, cwd=cwd, env=clean_env(**(env or {})), stdout=sink, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, start_new_session=True)
            if stop_after:
                time.sleep(stop_after)
                os.killpg(proc.pid, stop_signal)
            try:
                code = proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                code = "timeout"
        raw = raw_path.read_text()
        events = []
        for line in raw.splitlines():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        return Run(events, code, round(time.monotonic() - started, 1), raw)

    def save_fixture(self, name, run: Run):
        FIXTURES.mkdir(parents=True, exist_ok=True)
        text = "\n".join(filter(None, (self.scrub.line(l) for l in run.raw.splitlines()))) + "\n"
        self.scrub.verify(text, name)
        (FIXTURES / f"{name}.jsonl").write_text(text)

    def summary(self, run: Run):
        r = run.result
        i = run.init
        return {
            "exit": run.code,
            "seconds": run.seconds,
            "subtype": r.get("subtype"),
            "is_error": r.get("is_error"),
            "turns": r.get("num_turns"),
            "cost_usd": r.get("total_cost_usd"),
            "apiKeySource": i.get("apiKeySource"),
            "model_init": i.get("model"),
            "models_used": sorted((r.get("modelUsage") or {}).keys()),
            "permissionMode": i.get("permissionMode"),
            "tools": i.get("tools"),
            "denials": [d.get("tool_name") for d in r.get("permission_denials") or []],
            "event_types": sorted({e.get("type", "?") + ("/" + e["subtype"] if e.get("subtype") else "") for e in run.events}),
        }


def git(*args, cwd):
    out = sh(["git", *args], cwd=cwd)
    if out.returncode:
        sys.exit(f"git {' '.join(args)} failed: {out.stderr}")
    return out.stdout.strip()


def make_repo(path: Path):
    path.mkdir(parents=True)
    git("init", "-q", "-b", "main", cwd=path)
    git("config", "user.name", "Keel Spike", cwd=path)
    git("config", "user.email", "spike@keel.invalid", cwd=path)
    (path / "README.md").write_text("synthetic repo\n")
    git("add", "-A", cwd=path)
    git("commit", "-qm", "init", cwd=path)
    return path


def make_worktree(repo: Path, name: str):
    wt = repo.parent / f"wt-{name}"
    git("worktree", "add", "-q", "-b", f"keel/{name}", str(wt), "main", cwd=repo)
    git("commit", "--allow-empty", "-qm", "run commit", cwd=wt)
    return wt


def base_settings(extra_deny_read=(), network=None, filesystem=None):
    home = "~"
    deny = [f"{home}/.ssh", f"{home}/.aws", f"{home}/.config/gh", f"{home}/.claude.json", f"{home}/.claude/.credentials.json", *extra_deny_read]
    fs = {"denyRead": deny}
    fs.update(filesystem or {})
    return {
        "sandbox": {
            "enabled": True,
            "failIfUnavailable": True,
            "allowUnsandboxedCommands": False,
            "network": {"allowedDomains": ["pypi.org"], **(network or {})},
            "filesystem": fs,
        },
        "permissions": {"deny": [f"Read({p}/**)" for p in deny] + [f"Read({p})" for p in deny]},
    }


def plant(repo: Path, marker: Path):
    """A repo that tries to run code as soon as Claude Code opens it."""
    claude = repo / ".claude"
    (claude / "skills" / "planted").mkdir(parents=True)
    (claude / "skills" / "planted" / "SKILL.md").write_text("---\nname: planted\ndescription: planted skill\n---\nplanted\n")
    hook = {"hooks": [{"type": "command", "command": f"touch {marker}"}]}
    (claude / "settings.json").write_text(json.dumps({"hooks": {"SessionStart": [hook], "PreToolUse": [{"matcher": "*", **hook}]}}))
    (repo / ".mcp.json").write_text(json.dumps({"mcpServers": {"planted": {"command": "sh", "args": ["-c", f"touch {marker}.mcp; sleep 1"]}}}))
    (repo / "CLAUDE.md").write_text("Project memory: the codeword is ZEBRA.\n")
    git("add", "-A", cwd=repo)
    git("commit", "-qm", "plant", cwd=repo)


def probe_auth(s: Spike):
    data = json.loads(sh(["claude", "auth", "status", "--json"], env=clean_env()).stdout)
    s.out["auth_status"] = {k: data.get(k) for k in ("loggedIn", "authMethod", "apiProvider", "subscriptionType")}
    s.out["auth_status"]["has_loginExpiresAt"] = bool(data.get("loginExpiresAt"))
    fresh = s.work / "config-fresh"
    fresh.mkdir()
    other = json.loads(sh(["claude", "auth", "status", "--json"], env=clean_env(CLAUDE_CONFIG_DIR=str(fresh))).stdout)
    s.out["config_dir_fresh_logged_in"] = other.get("loggedIn")


def probe_modes(s: Spike):
    """Which launch mode works with a subscription, and what a hostile repo can do in each."""
    modes = {
        "default": [],
        "restricted": ["--restricted", "--tools", "Bash,Read", "--strict-mcp-config"],
        "setting_sources_user": ["--setting-sources", "user", "--tools", "Bash,Read", "--strict-mcp-config"],
    }
    out = {}
    for mode, flags in modes.items():
        marker = s.work / f"marker-{mode}"
        repo = make_repo(s.work / f"planted-{mode}")
        plant(repo, marker)
        run = s.claude(f"mode-{mode}", "Run the Bash command `true`. Then answer with the project codeword if you know one, otherwise NONE.", repo, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"])
        summary = s.summary(run)
        summary["planted_hook_ran"] = marker.exists()
        summary["planted_mcp_ran"] = Path(f"{marker}.mcp").exists()
        summary["planted_mcp_listed"] = any(m.get("name") == "planted" for m in run.init.get("mcp_servers", []))
        summary["planted_skill_listed"] = "planted" in json.dumps(run.init.get("skills", []))
        summary["read_claude_md"] = "ZEBRA" in str(run.result.get("result", ""))
        out[mode] = summary
        if mode == "restricted":
            s.save_fixture("success", run)
    s.out["launch_modes"] = out


def probe_sandbox(s: Spike, flags):
    """Run the escape script under each candidate filesystem policy."""
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(16)
    port = listener.getsockname()[1]
    threading.Thread(target=lambda: [listener.accept()[0].close() for _ in iter(int, 1)], daemon=True).start()

    canaries = s.work / "canaries"
    canaries.mkdir()
    denied, opened = canaries / "denied.txt", canaries / "open.txt"
    denied.write_text("KEEL-CANARY-DENIED\n")
    opened.write_text("KEEL-CANARY-OPEN\n")
    script = HERE / "probes" / "sandbox.sh"
    env = {"KEEL_DENIED_CANARY": str(denied), "KEEL_OPEN_CANARY": str(opened), "KEEL_LOOPBACK_PORT": str(port)}

    policies = {
        "deny_listed_paths": base_settings(extra_deny_read=[str(denied)]),
        "deny_home_allow_worktree": None,
    }
    out = {}
    for policy, settings in policies.items():
        repo = make_repo(s.work / f"sandbox-{policy}")
        wt = make_worktree(repo, policy)
        if settings is None:
            settings = base_settings(extra_deny_read=[str(denied), str(canaries)], filesystem={"denyRead": ["~"], "allowRead": [str(wt), str(script.parent), "~/.local", "~/.claude/local"]})
        before = git("for-each-ref", "--format=%(refname) %(objectname)", cwd=repo)
        main_before = git("rev-parse", "main", cwd=repo)
        cfg_before = (repo / ".git" / "config").read_text()
        pointer_before = (wt / ".git").read_text()
        prompt = f"Run exactly this one Bash command, once, and do nothing else: bash {script}\nThen reply DONE."
        run = s.claude(f"sandbox-{policy}", prompt, wt, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"], env=env)
        results = dict(RESULT_RE.findall(run.text()))
        out[policy] = {
            "probes": results,
            "run": s.summary(run),
            "refs_changed": before != git("for-each-ref", "--format=%(refname) %(objectname)", cwd=repo),
            "main_moved": git("rev-parse", "main", cwd=repo) != main_before,
            "shared_config_changed": cfg_before != (repo / ".git" / "config").read_text(),
            "pointer_changed": pointer_before != (wt / ".git").read_text(),
            "tool_error": next((r["text"][:200] for r in run.tool_results() if r["is_error"]), None),
        }
        if policy == "deny_listed_paths":
            s.save_fixture("bash-sandboxed", run)
    s.out["sandbox"] = out
    return env


def probe_read_tool(s: Spike, flags):
    """The sandbox only covers Bash, so Read needs permission rules. Uses a canary, not a secret."""
    canaries = s.work / "read-canaries"
    canaries.mkdir()
    secret = canaries / "secret.txt"
    secret.write_text("KEEL-READ-CANARY\n")
    repo = make_repo(s.work / "read-repo")
    settings = base_settings(extra_deny_read=[str(canaries)])
    prompt = f"Use the Read tool on {secret} once. Reply only READ_OK or READ_DENIED."
    run = s.claude("read-denied", prompt, repo, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Read"])
    s.out["read_tool_denied"] = {**s.summary(run), "canary_leaked": "KEEL-READ-CANARY" in run.raw}
    s.save_fixture("read-denied", run)

    allowed = s.claude("read-open", f"Use the Read tool on {secret} once. Reply only READ_OK or READ_DENIED.", repo, settings=base_settings(), flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Read"])
    s.out["read_tool_without_rule"] = {**s.summary(allowed), "canary_leaked": "KEEL-READ-CANARY" in allowed.raw}


def probe_fixtures(s: Spike, flags):
    """Failure shapes the harness has to parse."""
    repo = make_repo(s.work / "fixtures-repo")
    common = [*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"]
    settings = base_settings()

    run = s.claude("bash-exit-3", "Run the Bash command `exit 3` once. Then reply DONE.", repo, settings=settings, flags=common)
    s.out["bash_exit_code"] = {**s.summary(run), "tool_results": [{"is_error": r["is_error"], "text": r["text"][:120], "extra": r["extra"]} for r in run.tool_results()]}
    s.save_fixture("bash-exit-3", run)

    run = s.claude("max-turns", "Run `true` with Bash, then run `true` again, then again.", repo, settings=settings, flags=[*common, "--max-turns", "1"])
    s.out["max_turns"] = s.summary(run)
    s.save_fixture("max-turns", run)

    run = s.claude("max-budget", "Reply OK.", repo, settings=settings, flags=[*common, "--max-budget-usd", "0.000001"])
    s.out["max_budget"] = s.summary(run)
    s.save_fixture("max-budget", run)

    schema = json.dumps({"type": "object", "properties": {"verdict": {"type": "string", "enum": ["approved", "changes"]}}, "required": ["verdict"], "additionalProperties": False})
    run = s.claude("structured", "Return verdict approved.", repo, settings=settings, flags=[*common, "--json-schema", schema])
    s.out["structured_output"] = {**s.summary(run), "structured": run.result.get("structured_output")}
    s.save_fixture("structured", run)

    for sig in (signal.SIGINT, signal.SIGTERM):
        run = s.claude(f"killed-{sig.name}", "Run the Bash command `sleep 60` once.", repo, settings=settings, flags=common, stop_after=15, stop_signal=sig, timeout=40)
        s.out[f"killed_{sig.name}"] = {**s.summary(run), "has_result_event": bool(run.result), "lines": len(run.events)}
        s.save_fixture(f"killed-{sig.name}", run)


def probe_parallel(s: Spike, flags):
    """Can one turn start two Bash commands at once? That is what jammed the Archon pilot."""
    repo = make_repo(s.work / "parallel-repo")
    log = repo / "times.log"
    cmd = f'echo "start $(date +%s.%N 2>/dev/null || date +%s)" >> {log}; sleep 6; echo "end $(date +%s)" >> {log}'
    prompt = f"In one single response, issue two Bash tool calls at the same time, both running exactly: {cmd}\nThen reply DONE."
    settings = base_settings()
    settings["sandbox"]["filesystem"]["allowWrite"] = [str(repo)]
    run = s.claude("parallel", prompt, repo, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"])
    calls_per_message = [sum(1 for b in e.get("message", {}).get("content", []) if isinstance(b, dict) and b.get("type") == "tool_use") for e in run.events if e.get("type") == "assistant"]
    lines = log.read_text().splitlines() if log.exists() else []
    order = [l.split()[0] for l in lines]
    s.out["parallel_bash"] = {**s.summary(run), "tool_use_per_message": calls_per_message, "order": order, "overlapped": order[:2] == ["start", "start"]}

    run = s.claude("background", "Use the Bash tool with run_in_background set to true to run `sleep 30`. Then reply with whether it was accepted.", repo, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"])
    s.out["background_bash"] = {**s.summary(run), "answer": str(run.result.get("result", ""))[:200], "tool_results": [r["text"][:160] for r in run.tool_results()]}


def pg_bin():
    """A Postgres that needs no system install, in a throwaway venv."""
    venv = Path(tempfile.mkdtemp(prefix="keelpg"))
    if sh(["uv", "venv", "-q", "--python", "3.12", str(venv)]).returncode:
        return None, venv
    py = venv / "bin" / "python"
    if sh(["uv", "pip", "install", "-q", "--python", str(py), "pgserver", "psycopg[binary]"]).returncode:
        return None, venv
    found = list(venv.rglob("bin/initdb"))
    return (found[0].parent if found else None), venv


CONNECT = """
import os, sys, psycopg
try:
    psycopg.connect(sys.argv[1], connect_timeout=5).close()
    print(os.environ["PROBE"] + ": REACHED")
except Exception:
    print(os.environ["PROBE"] + ": BLOCKED")
"""

INSIDE = """#!/usr/bin/env bash
# Starts a Postgres inside the sandbox, which is how a test suite could get its own database.
set -u
d="$(mktemp -d "$PWD/.pgXXXX")"
s="$(mktemp -d "${TMPDIR:-/tmp}/kp.XXXX")"
"$PG_BIN/initdb" -D "$d" -U postgres --auth=trust >/dev/null 2>&1 || { echo "inside_initdb: BLOCKED"; exit 0; }
echo "inside_initdb: REACHED"
"$PG_BIN/pg_ctl" -D "$d" -w -t 20 -o "-p 54399 -c listen_addresses=127.0.0.1 -c unix_socket_directories=$s" -l "$d/log" start >/dev/null 2>&1 || { echo "inside_start: BLOCKED"; exit 0; }
echo "inside_start: REACHED"
PROBE=inside_tcp "$PY" "$CONNECT_PY" "host=127.0.0.1 port=54399 user=postgres dbname=postgres"
PROBE=inside_socket "$PY" "$CONNECT_PY" "host=$s port=54399 user=postgres dbname=postgres"
"$PG_BIN/pg_ctl" -D "$d" -m immediate stop >/dev/null 2>&1
"""


def probe_database(s: Spike, flags):
    """Risk 1: can a test suite reach Postgres from inside the sandbox?"""
    bin_dir, venv = pg_bin()
    if not bin_dir:
        s.out["database"] = {"error": "could not install a throwaway Postgres"}
        return
    py = venv / "bin" / "python"
    connect = s.work / "pg_connect.py"
    connect.write_text(CONNECT)
    inside = s.work / "pg_inside.sh"
    inside.write_text(INSIDE)
    data = s.work / "pgdata"
    sock = Path(tempfile.mkdtemp(prefix="kps"))
    port = 54398
    sh([str(bin_dir / "initdb"), "-D", str(data), "-U", "postgres", "--auth=trust"])
    sh([str(bin_dir / "pg_ctl"), "-D", str(data), "-w", "-t", "20", "-o", f"-p {port} -c listen_addresses=127.0.0.1 -c unix_socket_directories={sock}", "-l", str(s.work / "pg.log"), "start"])
    out = {}
    try:
        host_ok = sh([str(py), "-c", f"import psycopg; psycopg.connect('host=127.0.0.1 port={port} user=postgres dbname=postgres', connect_timeout=5).close(); print('ok')"]).stdout.strip()
        out["server_outside_sandbox_up"] = host_ok == "ok"
        tcp = f"host=127.0.0.1 port={port} user=postgres dbname=postgres"
        unix = f"host={sock} port={port} user=postgres dbname=postgres"
        cmd = f'PROBE=outside_tcp {py} {connect} "{tcp}"; PROBE=outside_socket {py} {connect} "{unix}"'
        variants = {
            "default": {},
            "allow_unix_socket": {"allowUnixSockets": [str(sock)]},
            "allow_local_binding": {"allowLocalBinding": True},
            "allow_localhost_domain": {"allowedDomains": ["pypi.org", "localhost", "127.0.0.1"]},
        }
        for name, net in variants.items():
            repo = make_repo(s.work / f"db-{name}")
            settings = base_settings(network=net)
            run = s.claude(f"db-{name}", f"Run exactly this one Bash command, once, and do nothing else: {cmd}\nThen reply DONE.", repo, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"])
            out[name] = dict(RESULT_RE.findall(run.text()))
        repo = make_repo(s.work / "db-inside")
        settings = base_settings(network={"allowLocalBinding": True})
        env = {"PG_BIN": str(bin_dir), "PY": str(py), "CONNECT_PY": str(connect)}
        run = s.claude("db-inside", f"Run exactly this one Bash command, once, and do nothing else: bash {inside}\nThen reply DONE.", repo, settings=settings, flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"], env=env)
        out["server_inside_sandbox"] = dict(RESULT_RE.findall(run.text()))
    finally:
        sh([str(bin_dir / "pg_ctl"), "-D", str(data), "-m", "immediate", "stop"])
        shutil.rmtree(venv, ignore_errors=True)
        shutil.rmtree(sock, ignore_errors=True)
    s.out["database"] = out


def deny_home(cwd, script_dir=None):
    """The policy the first pass pointed to: nothing in the home folder unless listed."""
    allow = [str(cwd), "~/.local", "~/.claude/local"]
    if script_dir:
        allow.append(str(script_dir))
    return base_settings(filesystem={"denyRead": ["~"], "allowRead": allow})


def probe_network_diagnosis(s: Spike, flags):
    """Why did an allowed domain fail under the deny-home policy?"""
    cmd = "curl -sS -m 8 -o /dev/null -w 'code=%{http_code}\\n' https://pypi.org/pypi/pip/json 2>&1; echo curl_exit=$?; env | grep -iE '^(https?|all)_proxy=' | sed -E 's/=.*/=set/'"
    out = {}
    for name, make in {"deny_listed": lambda r: base_settings(), "deny_home": lambda r: deny_home(r)}.items():
        repo = make_repo(s.work / f"netdiag-{name}")
        run = s.claude(f"netdiag-{name}", f"Run exactly this one Bash command, once, and do nothing else: {cmd}\nThen reply DONE.", repo, settings=make(repo), flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"])
        out[name] = run.text()[:400]
    s.out["network_diagnosis"] = out


def probe_clone(s: Spike, flags):
    """Per-run clone from an app-owned bare mirror, laid out where the real app keeps its data.

    The sandbox allows writes under temp folders, so this has to live in the home folder to
    say anything about a real install. Everything here is removed afterwards.
    """
    root = Path.home() / ("Library/Application Support" if sys.platform == "darwin" else ".local/share") / "keel-m0-probe"
    root.mkdir(parents=True)
    try:
        origin = make_repo(root / "origin")
        mirror = root / "mirror.git"
        git("clone", "-q", "--bare", str(origin), str(mirror), cwd=root)
        run_dir = root / "runs" / "current"
        run_dir.parent.mkdir()
        git("clone", "-q", "--no-hardlinks", str(mirror), str(run_dir), cwd=root)
        git("config", "user.name", "Keel Spike", cwd=run_dir)
        git("config", "user.email", "spike@keel.invalid", cwd=run_dir)
        git("checkout", "-q", "-b", "keel/run", cwd=run_dir)
        git("commit", "--allow-empty", "-qm", "run commit", cwd=run_dir)
        sibling = root / "runs" / "other"
        sibling.mkdir()
        (sibling / "c.txt").write_text("c\n")

        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        script = HERE / "probes" / "sandbox.sh"
        env = {"KEEL_DENIED_CANARY": str(sibling / "c.txt"), "KEEL_OPEN_CANARY": str(mirror / "HEAD"), "KEEL_LOOPBACK_PORT": str(listener.getsockname()[1])}
        refs = lambda path: git("for-each-ref", "--format=%(refname) %(objectname)", cwd=path)
        mirror_before, origin_before = refs(mirror), refs(origin)
        config_before = (run_dir / ".git" / "config").read_text()

        run = s.claude("clone", f"Run exactly this one Bash command, once, and do nothing else: bash {script}\nThen reply DONE.", run_dir, settings=deny_home(run_dir, script.parent), flags=[*flags, "--permission-mode", "dontAsk", "--allowedTools", "Bash"], env=env)
        listener.close()
        mirror_unchanged = mirror_before == refs(mirror)
        fetched = sh(["git", "-c", "core.hooksPath=/dev/null", "fetch", "-q", str(run_dir), "keel/run:refs/heads/keel/run"], cwd=mirror)
        s.out["clone_policy"] = {
            "layout": "home folder, as a real install",
            "probes": dict(RESULT_RE.findall(run.text())),
            "mirror_refs_unchanged_by_agent": mirror_unchanged,
            "origin_refs_unchanged": origin_before == refs(origin),
            "clone_config_changed": config_before != (run_dir / ".git" / "config").read_text(),
            "app_can_fetch_run_branch": fetched.returncode == 0,
            "mirror_main_unchanged_after_fetch": git("rev-parse", "main", cwd=mirror) == git("rev-parse", "main", cwd=origin),
        }
    finally:
        shutil.rmtree(root, ignore_errors=True)


def probe_inject(s: Spike, flags):
    """Brain and CLAUDE.md must reach a step under --restricted, which skips CLAUDE.md."""
    repo = make_repo(s.work / "inject-repo")
    brain = s.work / "brain.md"
    brain.write_text("Project knowledge: the deploy codeword is OTTER.\n")
    run = s.claude("inject", "What is the deploy codeword? Reply with the word only, or NONE.", repo, settings=base_settings(), flags=[*flags, "--permission-mode", "dontAsk", "--append-system-prompt-file", str(brain)])
    s.out["system_prompt_file_under_restricted"] = {"subtype": run.result.get("subtype"), "answer": str(run.result.get("result", ""))[:40]}


def probe_api_key(s: Spike, flags):
    """What the harness sees when an API key leaks into the environment. The key is fake."""
    repo = make_repo(s.work / "apikey-repo")
    run = s.claude("apikey", "Reply OK.", repo, flags=[*flags, "--permission-mode", "dontAsk"], env={"ANTHROPIC_API_KEY": "sk-ant-fake-for-keel-spike"}, timeout=60)
    s.out["fake_api_key"] = {"exit": run.code, "apiKeySource": run.init.get("apiKeySource"), "subtype": run.result.get("subtype"), "is_error": run.result.get("is_error"), "error": str(run.result.get("result", ""))[:120]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--work", required=True, help="scratch directory for repos and raw streams")
    parser.add_argument("--model", default="haiku")
    parser.add_argument("--only", default="", help="comma-separated probe names; merges into the existing results")
    args = parser.parse_args()
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)

    s = Spike(work, args.model)
    results_file = RESULTS / f"{sys.platform}.json"
    if args.only and results_file.exists():
        s.out = json.loads(results_file.read_text())
    s.out["cli_version"] = sh(["claude", "--version"]).stdout.strip()
    s.out["platform"] = sh(["uname", "-sm"]).stdout.strip()

    every = [probe_sandbox, probe_read_tool, probe_fixtures, probe_parallel, probe_database, probe_network_diagnosis, probe_clone, probe_inject, probe_api_key]
    if args.only:
        wanted = set(args.only.split(","))
        chosen = [p for p in every if p.__name__ in wanted]
    else:
        probe_auth(s)
        probe_modes(s)
        chosen = every
    # Later probes use whichever hardened mode works with this login.
    modes = s.out["launch_modes"]
    restricted_ok = modes["restricted"]["subtype"] == "success"
    flags = ["--restricted", "--tools", "Bash,Read", "--strict-mcp-config"] if restricted_ok else ["--setting-sources", "user", "--tools", "Bash,Read", "--strict-mcp-config"]
    s.out["hardened_mode_used"] = "restricted" if restricted_ok else "setting_sources_user"
    for probe in chosen:
        try:
            probe(s, flags)
        except Exception as exc:  # keep what was learned so far
            s.out[f"{probe.__name__}_crashed"] = repr(exc)[:300]
        RESULTS.mkdir(parents=True, exist_ok=True)
        text = json.dumps(s.out, indent=2, ensure_ascii=False) + "\n"
        for old, new in s.scrub.pairs:
            text = text.replace(old, new)
        text = s.scrub.TMPDIR.sub("/tmpdir", text)
        s.scrub.verify(text, "results")
        (RESULTS / f"{sys.platform}.json").write_text(text)
    print(f"results: {RESULTS / (sys.platform + '.json')}")


if __name__ == "__main__":
    main()
