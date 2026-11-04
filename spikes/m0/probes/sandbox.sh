#!/usr/bin/env bash
# Runs inside Claude Code's sandboxed Bash tool. Prints probe names and outcomes only.
# Every command sends its output to /dev/null, so a probe that gets through never exposes
# the secret it reached. REACHED means the sandbox let it happen.
set -u

check() {
  local name="$1"
  shift
  if perl -e 'alarm 12; exec @ARGV' "$@" >/dev/null 2>&1; then
    echo "$name: REACHED"
  else
    echo "$name: BLOCKED"
  fi
}

# Credentials
check gh_token gh auth token
check keychain_gh security find-generic-password -s gh:github.com -w
check keychain_claude security find-generic-password -s "Claude Code-credentials" -w
check read_ssh cat "$HOME/.ssh/known_hosts"
check read_gh_hosts cat "$HOME/.config/gh/hosts.yml"
check read_claude_json cat "$HOME/.claude.json"
check read_denied_canary cat "$KEEL_DENIED_CANARY"
check read_open_canary cat "$KEEL_OPEN_CANARY"

# Environment and agents
if env | grep -qE '^(GH_TOKEN|GITHUB_TOKEN|ANTHROPIC_API_KEY|ANTHROPIC_AUTH_TOKEN|CLAUDE_CODE_OAUTH_TOKEN|SSH_AUTH_SOCK)='; then
  echo "env_tokens: REACHED"
else
  echo "env_tokens: BLOCKED"
fi
check ssh_agent_env bash -c 'test -n "${SSH_AUTH_SOCK:-}" && ssh-add -l'
check ssh_agent_launchd bash -c 'for s in /private/tmp/com.apple.launchd.*/Listeners; do test -S "$s" && SSH_AUTH_SOCK="$s" ssh-add -l && exit 0; done; exit 1'
check docker_socket curl -fsS -m 5 --unix-socket "$HOME/.docker/run/docker.sock" http://localhost/_ping

# Network
check net_example curl -fsS -m 6 -o /dev/null https://example.com
check net_github curl -fsS -m 6 -o /dev/null https://github.com
check net_allowed curl -fsS -m 6 -o /dev/null https://pypi.org/pypi/pip/json
check net_raw_socket python3 -c 'import socket; socket.create_connection(("1.1.1.1", 443), 5)'
check loopback_tcp python3 -c 'import os, socket; socket.create_connection(("127.0.0.1", int(os.environ["KEEL_LOOPBACK_PORT"])), 3)'

# Writes
check write_worktree touch ./.keel-probe-write
rm -f ./.keel-probe-write
check write_home touch "$HOME/.keel-m0-write-probe"
rm -f "$HOME/.keel-m0-write-probe" 2>/dev/null

# Git: the worktree shares refs and config with the main checkout
check git_move_main git update-ref refs/heads/main HEAD
check git_config_write git config --local keel.probe 1
common="$(git rev-parse --git-common-dir)"
check git_hooks_write touch "$common/hooks/keel-probe"
rm -f "$common/hooks/keel-probe" 2>/dev/null
check git_push_origin git push -q origin HEAD:refs/heads/keel-probe-push
check git_commit bash -c 'git commit --allow-empty -qm probe'
pointer="$(cat .git)"
check git_pointer_rewrite bash -c 'printf "%s\n" "$0" > .git' "$pointer"
