#!/usr/bin/env bash
# Builds a throwaway repository and fake Claude Code transcripts, then runs gopola against them. Every example in
# README.md and docs/reference.md is this script's output, so anyone can regenerate them:
#
#     bash demo/demo.sh
#
# Everything lives under GOPOLA_DEMO_DIR (default /tmp/gopola-demo), which is deleted and recreated on each run.
# gopola is pointed at a database and a transcript folder there, so your own index and ~/.claude are never read.
# GOPOLA_BIN picks the gopola to run; by default this checkout is built and its release binary is used.
set -euo pipefail

ROOT=${GOPOLA_DEMO_DIR:-/tmp/gopola-demo}
HERE=$(cd "$(dirname "$0")/.." && pwd)
if [ -z "${GOPOLA_BIN:-}" ]; then
  cargo build --release --quiet --manifest-path "$HERE/Cargo.toml"
  GOPOLA_BIN=$HERE/target/release/gopola
fi

# Only a folder this script made (it holds .gopola-demo) or an empty one is deleted, so a mistaken
# GOPOLA_DEMO_DIR=~ deletes nothing.
if [ -e "$ROOT" ] && [ ! -e "$ROOT/.gopola-demo" ] && [ -n "$(ls -A "$ROOT")" ]; then
  echo "demo.sh: $ROOT exists and was not made by this script; choose another GOPOLA_DEMO_DIR" >&2
  exit 2
fi
rm -rf "${ROOT:?}"
mkdir -p "$ROOT/home" "$ROOT/projects" "$ROOT/myapp"
touch "$ROOT/.gopola-demo"
REPO=$ROOT/myapp

# Isolation: gopola's database, transcripts, Claude Code folder and home all point under $ROOT; no Ollama, no
# `claude -p`, no git config from this machine; times print in UTC.
export GOPOLA_DB=$ROOT/gopola.db GOPOLA_PROJECTS=$ROOT/projects GOPOLA_OLLAMA=http://127.0.0.1:9 \
  GOPOLA_CLAUDE=false HOME=$ROOT/home CLAUDE_CONFIG_DIR=$ROOT/home/.claude TZ=UTC \
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset GOPOLA_EXCLUDED_PROJECTS GOPOLA_EMBED_MODEL CLAUDE_PROJECT_DIR

# --- the repository -------------------------------------------------------------------------------------------

git -C "$REPO" init -q -b main
git -C "$REPO" config user.name "Demo Dev"
git -C "$REPO" config user.email dev@example.com

put() { mkdir -p "$(dirname "$REPO/$1")"; cat > "$REPO/$1"; }
commit() { # <iso time> <subject> [<author name> <author email>]
  git -C "$REPO" add -A
  GIT_AUTHOR_DATE=$1 GIT_COMMITTER_DATE=$1 GIT_AUTHOR_NAME=${3:-Demo Dev} GIT_AUTHOR_EMAIL=${4:-dev@example.com} \
    git -C "$REPO" commit -q -m "$2"
}

# --- transcripts ----------------------------------------------------------------------------------------------
# One JSON record per line, in the shapes Claude Code writes (tests/cli.rs builds the same ones). Text arguments
# are JSON string bodies: \n is a newline, \" a quote.

TRANSCRIPTS=$GOPOLA_PROJECTS/$(printf '%s' "$REPO" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$TRANSCRIPTS"

session() { # <id> [<chain id>]: later records go to <id>.jsonl; a /clear part keeps its chain's id in session_id
  SID=$1 CHAIN=${2:-$1} OUT=$TRANSCRIPTS/$1.jsonl N=0
}
meta() { printf '"timestamp":"%s","cwd":"%s","sessionId":"%s","session_id":"%s","gitBranch":"main"' "$1" "$REPO" "$SID" "$CHAIN"; }
prompt() { # <ts> <text> [<origin>]
  printf '{"type":"user","turnOrigin":"%s",%s,"message":{"role":"user","content":"%s"}}\n' "${3:-human}" "$(meta "$1")" "$2" >> "$OUT"
}
say() { # <ts> <context tokens> <text> [<tool_use block>...]: one model call
  local ts=$1 ctx=$2 blocks="{\"type\":\"text\",\"text\":\"$3\"}"
  shift 3
  for b in "$@"; do blocks="$blocks,$b"; done
  N=$((N + 1))
  printf '{"type":"assistant","uuid":"%s-%d",%s,"message":{"id":"msg_%s_%d","role":"assistant","content":[%s],"usage":{"input_tokens":%s,"output_tokens":600}}}\n' \
    "${SID:0:8}" "$N" "$(meta "$ts")" "${SID:0:8}" "$N" "$blocks" "$ctx" >> "$OUT"
}
bash_use() { printf '{"type":"tool_use","id":"%s","name":"Bash","input":{"command":"%s"}}' "$1" "$2"; }
edit_use() { printf '{"type":"tool_use","id":"%s","name":"Edit","input":{"file_path":"%s/%s","old_string":"a","new_string":"b"}}' "$1" "$REPO" "$2"; }
read_use() { printf '{"type":"tool_use","id":"%s","name":"Read","input":{"file_path":"%s/%s"}}' "$1" "$REPO" "$2"; }
agent_use() { printf '{"type":"tool_use","id":"%s","name":"Agent","input":{"description":"%s","prompt":"%s","subagent_type":"general-purpose","run_in_background":true}}' "$1" "$2" "$3"; }
result() { # <ts> <tool_use id> <output>
  printf '{"type":"user",%s,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"%s","content":"%s","is_error":false}]},"toolUseResult":{"stdout":"%s","stderr":"","interrupted":false}}\n' \
    "$(meta "$1")" "$2" "$3" "$3" >> "$OUT"
}
failed() { # <ts> <tool_use id> <exit code> <output>
  printf '{"type":"user",%s,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"%s","content":"Exit code %s\\n%s","is_error":true}]},"toolUseResult":"Error: Exit code %s\\n%s"}\n' \
    "$(meta "$1")" "$2" "$3" "$4" "$3" "$4" >> "$OUT"
}
launched() { # <ts> <tool_use id> <agent id>
  printf '{"type":"user",%s,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"%s","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"toolUseResult":{"status":"async_launched","agentId":"%s"}}\n' \
    "$(meta "$1")" "$2" "$3" >> "$OUT"
}
title() { printf '{"type":"ai-title","aiTitle":"%s","sessionId":"%s"}\n' "$1" "$SID" >> "$OUT"; }
hookran() { # <ts> <hook name> <command> <exit code> <ms> [<stderr>]: a hook that ran, as Claude Code records it
  local type=hook_success; [ "$4" = 0 ] || type=hook_non_blocking_error
  printf '{"type":"attachment",%s,"attachment":{"type":"%s","hookName":"%s","hookEvent":"%s","command":"%s","exitCode":%s,"durationMs":%s,"stdout":"","stderr":"%s"}}\n' \
    "$(meta "$1")" "$type" "$2" "${2%%:*}" "$3" "$4" "$5" "${6:-}" >> "$OUT"
}
blocked() { # <ts> <tool_use id> <hook name> <command> <reason>: a hook stopped the call; only the tool result records it
  printf '{"type":"user",%s,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"%s","content":"%s hook error: [%s]: %s","is_error":true}]},"toolUseResult":"Error: %s hook error: [%s]: %s"}\n' \
    "$(meta "$1")" "$2" "$3" "$4" "$5" "$3" "$4" "$5" >> "$OUT"
}

TESTS_3='running 3 tests\ntest auth::tests::allows_five_tries ... ok\ntest auth::tests::blocks_the_sixth_try ... ok\ntest auth::tests::resets_after_a_minute ... ok\n\ntest result: ok. 3 passed; 0 failed'

# Before any session: the sign-in handler.
put README.md <<'EOF'
# myapp
EOF
put src/auth.rs <<'EOF'
use crate::session;

pub fn sign_in(user: &str, password: &str) -> Result<session::Token, Error> {
    check_password(user, password)?;
    Ok(session::new_token(user))
}
EOF
commit 2026-10-01T09:00:00Z "sign-in handler"

# Session 1: a rate limit on sign-in. The first test run fails, the second passes.
session 5ca1ab1e-0d1e-4a5b-9c3d-2f6e8a0b1c01
hookran 2026-10-05T09:00:01Z SessionStart:startup 'gopola index --quiet' 0 410
prompt 2026-10-05T09:00:05Z 'add a rate limit to sign-in: five tries per address per minute'
say 2026-10-05T09:00:20Z 18200 'Reading the sign-in handler first.' "$(read_use r1 src/auth.rs)"
result 2026-10-05T09:00:21Z r1 'use crate::session;\n\npub fn sign_in(user: &str, password: &str) -> Result<session::Token, Error> {'
# A teammate's commit lands inside the session's window: authored leaves it out.
put src/config.rs <<'EOF'
pub const PORT: u16 = 8080;
EOF
commit 2026-10-05T09:01:00Z "config: read the port from the environment" "Other Dev" other@example.com
say 2026-10-05T09:02:10Z 21500 'Adding a per-address limiter.' "$(edit_use e1 src/auth.rs)"
result 2026-10-05T09:02:11Z e1 'The file src/auth.rs has been updated.'
say 2026-10-05T09:03:00Z 23800 'Running the auth tests.' "$(bash_use b1 'cargo test auth')"
failed 2026-10-05T09:03:41Z b1 101 'running 3 tests\ntest auth::tests::allows_five_tries ... ok\ntest auth::tests::blocks_the_sixth_try ... FAILED\ntest auth::tests::resets_after_a_minute ... ok\n\ntest result: FAILED. 2 passed; 1 failed'
say 2026-10-05T09:05:00Z 26900 'The window starts at the first try, not the last. Fixing.' "$(edit_use e2 src/auth.rs)"
result 2026-10-05T09:05:01Z e2 'The file src/auth.rs has been updated.'
say 2026-10-05T09:06:00Z 28400 'Running them again.' "$(bash_use b2 'cargo test auth')"
result 2026-10-05T09:06:38Z b2 "$TESTS_3"
say 2026-10-05T09:07:00Z 30100 'The limit works: five tries per address per minute, and the sixth gets a 429.'
put src/auth.rs <<'EOF'
use crate::session;
use std::time::{Duration, Instant};

const MAX_TRIES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);

pub fn sign_in(limits: &mut Limits, addr: &str, user: &str, password: &str) -> Result<session::Token, Error> {
    if !limits.allow(addr, Instant::now(), MAX_TRIES, WINDOW) {
        return Err(Error::TooManyTries);
    }
    check_password(user, password)?;
    Ok(session::new_token(user))
}
EOF
commit 2026-10-05T09:20:00Z "auth: limit sign-in attempts per address"
prompt 2026-10-05T09:30:00Z 'now log each blocked attempt'
say 2026-10-05T09:31:00Z 33000 'Logging the address and the count.' "$(edit_use e3 src/auth.rs)"
result 2026-10-05T09:31:01Z e3 'The file src/auth.rs has been updated.'
say 2026-10-05T09:33:00Z 34500 'Tests again.' "$(bash_use b3 'cargo test auth')"
hookran 2026-10-05T09:33:00Z PreToolUse:Bash '~/.local/bin/commitguard' 0 9
result 2026-10-05T09:33:30Z b3 "$TESTS_3"
say 2026-10-05T09:34:00Z 35000 'Committing.' "$(bash_use b4 'git commit --no-verify -am wip')"
blocked 2026-10-05T09:34:00Z b4 PreToolUse:Bash '~/.local/bin/commitguard' '--no-verify skips the pre-commit checks; commit without it'
put src/auth.rs <<'EOF'
use crate::session;
use std::time::{Duration, Instant};

const MAX_TRIES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);

pub fn sign_in(limits: &mut Limits, addr: &str, user: &str, password: &str) -> Result<session::Token, Error> {
    if !limits.allow(addr, Instant::now(), MAX_TRIES, WINDOW) {
        log::warn!("sign-in blocked for {addr}: {} tries", limits.count(addr));
        return Err(Error::TooManyTries);
    }
    check_password(user, password)?;
    Ok(session::new_token(user))
}
EOF
commit 2026-10-05T09:35:00Z "auth: log blocked sign-in attempts"
say 2026-10-05T09:36:00Z 36000 'Committed both: the limit and the log line.'
hookran 2026-10-05T09:36:01Z Stop '~/.local/bin/docscheck' 1 230 'docs/auth.md: no such file (README links to it)'
title 'Rate limit on sign-in'

# Session 2: a review and a docs page, each handed to a background agent. The reviewer reports back; the docs
# agent never does.
REVIEWER=a3d9f1c27b4e8a605 WRITER=a8e2b6d04c1f9a317
session c0ffee42-7a1b-4c2d-8e3f-4a5b6c7d8e02
prompt 2026-10-06T14:00:00Z 'review the sign-in limit and write a docs page for it'
say 2026-10-06T14:01:00Z 24100 'Starting a reviewer and a docs writer.' \
  "$(agent_use u1 'Review the sign-in limit' 'Review the sign-in limit in src/auth.rs: five tries per address per minute.\nReport anything that lets a client get past it.')"
launched 2026-10-06T14:01:01Z u1 "$REVIEWER"
say 2026-10-06T14:01:30Z 25300 '' \
  "$(agent_use u2 'Write the docs page' 'Write docs/auth.md: what the sign-in limit is and what a blocked client sees.')"
launched 2026-10-06T14:01:31Z u2 "$WRITER"
prompt 2026-10-06T14:12:00Z "<task-notification>\n<task-id>$REVIEWER</task-id>\n<status>completed</status>\n<summary>Agent \\\"Review the sign-in limit\\\" finished</summary>\n<result>Verdict: the limit holds.\nOne gap: clients behind a proxy share one address. Key on the forwarded address when the proxy is trusted.</result>\n</task-notification>" task_notification
say 2026-10-06T14:13:00Z 29800 'The reviewer found one gap: clients behind a proxy. Keying on the forwarded address.' "$(edit_use e1 src/auth.rs)"
result 2026-10-06T14:13:01Z e1 'The file src/auth.rs has been updated.'
put src/auth.rs <<'EOF'
use crate::session;
use std::time::{Duration, Instant};

const MAX_TRIES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);

pub fn sign_in(limits: &mut Limits, req: &Request, user: &str, password: &str) -> Result<session::Token, Error> {
    let addr = req.client_addr(TRUSTED_PROXIES);
    if !limits.allow(&addr, Instant::now(), MAX_TRIES, WINDOW) {
        log::warn!("sign-in blocked for {addr}: {} tries", limits.count(&addr));
        return Err(Error::TooManyTries);
    }
    check_password(user, password)?;
    Ok(session::new_token(user))
}
EOF
commit 2026-10-06T14:20:00Z "auth: key the limit on the forwarded address behind a trusted proxy"
put docs/auth.md <<'EOF'
# Sign-in limit

Five tries per address per minute. The sixth gets `429 Too Many Requests`.
EOF
commit 2026-10-06T14:28:00Z "docs: describe the sign-in limit"
say 2026-10-06T14:30:00Z 33700 'The docs agent has not reported back, so I wrote docs/auth.md myself and committed both.'
title 'Review and document the sign-in limit'

# By hand, between sessions: one line of src/auth.rs.
sed -i.bak 's/^const MAX_TRIES: u32 = 5;$/const MAX_TRIES: u32 = 5; \/\/ per address, per WINDOW/' "$REPO/src/auth.rs"
rm "$REPO/src/auth.rs.bak"
commit 2026-10-06T17:00:00Z "auth: say what MAX_TRIES counts"

# Sessions 3 and 4: one job, split by a /clear. The first part grows until it is compacted.
CHAIN=decade01-3c4d-4e5f-9a6b-7c8d9e0f1a03
session "$CHAIN"
prompt 2026-10-07T10:00:00Z 'move sessions into a token store'
say 2026-10-07T10:00:30Z 22400 'Finding every place a token is made.' "$(bash_use b1 'rg -n new_token src')"
result 2026-10-07T10:00:31Z b1 'src/auth.rs:14:    Ok(session::new_token(user))\nsrc/session.rs:3:pub fn new_token(user: &str) -> Token {'
say 2026-10-07T10:04:00Z 41000 'Reading the session module.' "$(read_use r1 src/session.rs)"
result 2026-10-07T10:04:01Z r1 'pub struct Token(String);\n\npub fn new_token(user: &str) -> Token {\n    Token(random_id(user))\n}'
say 2026-10-07T10:12:00Z 66000 'Writing the store.' "$(edit_use e1 src/session.rs)"
result 2026-10-07T10:12:01Z e1 'The file src/session.rs has been updated.'
say 2026-10-07T10:25:00Z 90500 'Sign-in now issues tokens from the store.' "$(edit_use e2 src/auth.rs)"
result 2026-10-07T10:25:01Z e2 'The file src/auth.rs has been updated.'
say 2026-10-07T10:40:00Z 118000 'Running the tests.' "$(bash_use b2 'cargo test')"
result 2026-10-07T10:41:10Z b2 'running 9 tests\ntest result: ok. 9 passed; 0 failed'
put src/session.rs <<'EOF'
pub struct Token(String);

pub struct Store { db: rusqlite::Connection }

impl Store {
    pub fn issue(&self, user: &str) -> Token {
        let token = Token(random_id(user));
        self.db.execute("insert into tokens (token, user, issued) values (?1, ?2, unixepoch())", (&token.0, user)).unwrap();
        token
    }
}
EOF
sed -i.bak 's/Ok(session::new_token(user))/Ok(session::store().issue(user))/' "$REPO/src/auth.rs"
rm "$REPO/src/auth.rs.bak"
commit 2026-10-07T11:00:00Z "session: store tokens in sqlite"
prompt 2026-10-07T11:05:00Z 'add a migration for the tokens table'
say 2026-10-07T11:06:00Z 142000 'Adding the migration.' "$(edit_use e3 migrations/0002_tokens.sql)"
result 2026-10-07T11:06:01Z e3 'The file migrations/0002_tokens.sql has been updated.'
say 2026-10-07T11:19:00Z 161000 'Checking it applies on a fresh database.' "$(bash_use b3 'cargo test migrations')"
result 2026-10-07T11:19:13Z b3 'running 2 tests\ntest result: ok. 2 passed; 0 failed'
printf '{"type":"system","subtype":"compact_boundary",%s,"logicalParentUuid":"%s-%d","compactMetadata":{"trigger":"auto","preTokens":167000,"postTokens":24000,"durationMs":41000}}\n' \
  "$(meta 2026-10-07T11:19:57Z)" "${SID:0:8}" "$N" >> "$OUT"
say 2026-10-07T11:21:00Z 31000 'The migration applies; committing it.'
put migrations/0002_tokens.sql <<'EOF'
create table tokens (token text primary key, user text not null, issued integer not null);
EOF
commit 2026-10-07T11:25:00Z "migrations: the tokens table"
say 2026-10-07T11:26:00Z 33500 'Tokens now live in sqlite, and sign-in issues them from the store.'
title 'Move sessions into a token store'

session f00dcafe-5e6f-4a7b-8c9d-0e1f2a3b4c04 "$CHAIN"
prompt 2026-10-07T12:00:00Z 'now expire tokens after 30 days'
say 2026-10-07T12:01:00Z 19500 'Adding an expiry to the store.' "$(edit_use e1 src/session.rs)"
result 2026-10-07T12:01:01Z e1 'The file src/session.rs has been updated.'
say 2026-10-07T12:10:00Z 27000 'Testing it.' "$(bash_use b1 'cargo test session')"
result 2026-10-07T12:10:20Z b1 'running 3 tests\ntest result: ok. 3 passed; 0 failed'
put src/session.rs <<'EOF'
pub struct Token(String);

pub struct Store { db: rusqlite::Connection }

const MAX_AGE_DAYS: i64 = 30;

impl Store {
    pub fn issue(&self, user: &str) -> Token {
        self.db.execute("delete from tokens where issued < unixepoch() - ?1 * 86400", [MAX_AGE_DAYS]).unwrap();
        let token = Token(random_id(user));
        self.db.execute("insert into tokens (token, user, issued) values (?1, ?2, unixepoch())", (&token.0, user)).unwrap();
        token
    }
}
EOF
commit 2026-10-07T12:20:00Z "session: expire tokens after 30 days"
say 2026-10-07T12:21:00Z 35000 'Tokens older than 30 days are deleted each time one is issued.'
title 'Expire old tokens'

# --- gopola ---------------------------------------------------------------------------------------------------

cd "$REPO"
"$GOPOLA_BIN" index --quiet
run() { echo "\$ gopola $1"; eval "\"\$GOPOLA_BIN\" $1"; echo; }

run 'recent'
run 'search "sign-in limit"'
run "commands --grep 'cargo test auth' --project all"
run 'dispatches c0ffee42'
run 'hooks 5ca1ab1e'
run 'weight decade01'
run 'authored'
run 'blame src --summary'
run 'blame src/auth.rs --ranges'
