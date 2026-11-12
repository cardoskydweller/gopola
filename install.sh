#!/usr/bin/env bash
# Build gopola in release mode and install it as a COPY (never a symlink into this checkout) in a bin
# directory on PATH (default ~/.local/bin). Owner decision 2026-10-07: a tool must keep working
# while its checkout is mid-edit or on another branch, so the installed file is independent of it.
set -euo pipefail
bin="${1:-$HOME/.local/bin}"
case "$bin" in */bin) ;; *) echo "install.sh: bin directory must end in /bin (cargo installs to <root>/bin): $bin" >&2; exit 2;; esac
mkdir -p "$bin"
# Absolute: the Copilot hook runs from the session's directory, and cargo below runs from the checkout.
bin="$(cd "$bin" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
# Injected so `gopola --version` reports the checkout it was built from, not the one on disk now.
sha=$(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)
if [ -n "$(git -C "$here" status --porcelain 2>/dev/null)" ]; then sha="$sha-dirty"; fi
# cargo install replaces an existing file; an old symlink is removed first so the link target is never written through.
if [ -L "$bin/gopola" ]; then rm "$bin/gopola"; fi
(cd "$here" && GOPOLA_BUILD_SHA="$sha" GOPOLA_BUILD_DATE="$(date +%Y-%m-%d)" cargo install --path . --root "${bin%/bin}" --locked --force --quiet)
echo "installed $bin/gopola (copy, built from $sha): $("$bin/gopola" --version 2>&1 | head -1)"
skills="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills"; mkdir -p "$skills"
ln -sfn "$here/skills/gopola" "$skills/gopola"
echo "linked $skills/gopola -> $here/skills/gopola"
# Copilot CLI reads machine-wide hooks from <COPILOT_HOME>/hooks/*.json. Only where Copilot is installed; the file is
# rewritten whenever it differs from what this run would write, so it always names the bin just installed.
copilot="${COPILOT_HOME:-$HOME/.copilot}"
if [ -d "$copilot" ]; then
  hook="$copilot/hooks/gopola.json"
  # The bin folder is shell-quoted (`'` becomes `'\''`), then put in a JSON string (`\` and `"` escaped). Only the
  # folder is quoted, so the command still reads `gopola copilot-hook`, which doctor looks for.
  quoted="'$(printf '%s' "$bin" | sed "s/'/'\\\\''/g")'"
  want=$(printf '{"version":1,"hooks":{"sessionStart":[{"type":"command","bash":"%s/gopola copilot-hook","timeoutSec":10}]}}' \
    "$(printf '%s' "$quoted" | sed 's/[\\"]/\\&/g')")
  if [ "$(cat "$hook" 2>/dev/null)" = "$want" ]; then
    echo "left $hook alone, unchanged"
  else
    mkdir -p "$copilot/hooks"
    printf '%s\n' "$want" > "$hook"
    echo "wrote $hook (Copilot sessionStart hook)"
  fi
fi
# Codex CLI reads user-level hooks from <CODEX_HOME>/hooks.json, which may hold the owner's own handlers: gopola merges
# its one SessionStart handler in (naming the bin just installed) and keeps the rest. A failed merge leaves the file alone.
codex="${CODEX_HOME:-$HOME/.codex}"
if [ -d "$codex" ]; then
  "$bin/gopola" codex-hook --install "$bin" || echo "install.sh: warning: Codex hook not added; the install goes on" >&2
fi
# gopola was called recall until 2026-11-09: the old name says where the tool went instead of failing silently.
# Exit 1, not 2: a hook still calling recall gets an error, and exit 2 would block the prompt or tool call.
printf '#!/bin/sh\necho "recall is now gopola: run gopola $*" >&2\nexit 1\n' > "$bin/recall"; chmod +x "$bin/recall"
echo "wrote $bin/recall (points to gopola)"
