# gopola reference

Every command, what it prints, and how it gets its facts. The [README](../README.md) is the short version.

- [Setup](#setup): install, hooks, `doctor`, the skill
- [The index](#the-index): what is stored, privacy, projects, [Copilot CLI](#copilot-cli), [Codex CLI](#codex-cli),
  [the tables](#the-tables)
- [Finding past work](#finding-past-work): `search`, `show`, `timeline`, `recent`, `file`, `digest`, `tools`
- [Inside a session](#inside-a-session): `commands`, `dispatches`, `hooks`, `handback`, `weight`
- [Code from sessions](#code-from-sessions): `authored`, `blame`
- [Model calls](#model-calls): `summarize`, `embed`
- [Cloud sessions](#cloud-sessions): `import`
- [Other commands](#other-commands): `index`, `import-claude-mem`, `version`
- [Limits](#limits)

Common rules:

- Every command takes `--json`.
- A session is named by any unique prefix of its id (`5ca1ab1e`).
- `--project P` picks a project by name. `--project all` searches every project. The default is the project of the
  current directory.
- `--since D` and `--until D` take a date (`2026-10-01`).
- Exit codes: `0` success, `1` what you asked about is missing or in a bad state, `2` misuse.

## Setup

### Install

    ./install.sh [<prefix>/bin]

`install.sh` builds a release binary and installs a **copy** into `~/.local/bin` (or the directory you give, which
must end in `/bin`). It is not a symlink, so the installed tool keeps working while you edit or switch branches in
the checkout. The build records the commit and date, so `gopola version` names the build that is running. It also
links `skills/gopola` into `~/.claude/skills` (or `$CLAUDE_CONFIG_DIR/skills`). When the Copilot folder exists
(`~/.copilot`, or `$COPILOT_HOME`), it also writes Copilot CLI's session-start hook (see
[Copilot CLI](#copilot-cli)) with the absolute path of the bin it installed to, and says so; a hook file that already
says exactly that is left alone, and any other is rewritten. When the Codex folder exists (`~/.codex`, or
`$CODEX_HOME`), it runs `gopola codex-hook --install <bin>`, which adds Codex CLI's session-start hook to Codex's
`hooks.json` and keeps every other handler there (see [Codex CLI](#codex-cli)); if that fails, the install warns and
goes on.

gopola needs Rust stable and git. SQLite with full-text search is compiled in. Meaning search also needs
[Ollama](https://ollama.com) with the model pulled (`ollama pull embeddinggemma-2:270m`). Without Ollama, everything
works except `embed` and the meaning half of `search`.

### Hooks

Add these to `~/.claude/settings.json`:

```json
"hooks": {
  "SessionStart": [{"matcher": "startup|clear|compact", "hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola index --quiet && $HOME/.local/bin/gopola recent", "timeout": 30}]}],
  "UserPromptSubmit": [{"hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola index --quiet", "timeout": 15}]}],
  "PreToolUse": [{"matcher": "Read", "hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola file --hook", "timeout": 10}]}],
  "PostToolUse": [{"matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola index --quiet", "timeout": 15, "async": true}]}],
  "Stop": [{"hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola summarize --auto", "timeout": 120, "async": true}]}],
  "SessionEnd": [{"hooks": [
    {"type": "command", "command": "$HOME/.local/bin/gopola summarize --auto --final", "timeout": 120},
    {"type": "command", "command": "$HOME/.local/bin/gopola embed --quiet", "timeout": 120}]}]
}
```

| Event | Command | What it does |
|---|---|---|
| SessionStart | `index --quiet && recent` | Brings the index up to date and prints the session-start block. |
| UserPromptSubmit | `index --quiet` | Keeps the index current inside a session (10–50 ms when nothing changed), so `search` finds this session's earlier turns. |
| PreToolUse (Read) | `file --hook` | Tells the session when a different session edited the file it is about to read. |
| PostToolUse (edits) | `index --quiet`, async | Indexes after each `Edit`, `Write`, `MultiEdit` or `NotebookEdit`, so another session's Read hook sees the edit when it happens. No model call, no copy of the tool's output. |
| Stop | `summarize --auto` | Summarises a long session every 20 turns. See [Model calls](#model-calls). |
| SessionEnd | `summarize --auto --final`, `embed --quiet` | A last summary if the session grew, then meaning-search vectors for its new turns. |

Every hook form exits 0, so a broken index never stops a session. Errors go to stderr. The hook forms print
nothing when they have nothing to say: `recent` for a project with no sessions, `file --hook` for a file with no
history, `summarize --auto` always.

`index` and `recent` are the core. The other hooks are optional.

Copilot CLI has its own session-start hook, which `install.sh` writes (see [Copilot CLI](#copilot-cli)):

```json
{"version":1,"hooks":{"sessionStart":[{"type":"command","bash":"'/Users/you/.local/bin'/gopola copilot-hook","timeoutSec":10}]}}
```

in `~/.copilot/hooks/gopola.json` (`$COPILOT_HOME/hooks/gopola.json`; the installer writes the full path of the
`bin` it installed to, in single quotes so a space or a quote in it still runs). `gopola copilot-hook` reads the hook's JSON on stdin, brings the index up to date, and answers
`{"additionalContext": "<the recent block of the session's project>"}`. It always exits 0 and prints nothing when it
fails or has nothing to say.

Codex CLI's session-start hook is one handler in `~/.codex/hooks.json` (`$CODEX_HOME/hooks.json`), which
`gopola codex-hook --install <bin>` adds (see [Codex CLI](#codex-cli)):

```json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/Users/you/.local/bin'/gopola codex-hook","timeout":10}]}]}}
```

`gopola codex-hook` reads the hook's JSON on stdin, brings the index up to date, and answers
`{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"<the recent block>"}}`. It always exits 0
and prints nothing when it fails or has nothing to say.

### `doctor`

    gopola doctor

Checks, one line each:

- whether the index is current (transcripts not yet indexed);
- each of the six hook events (for SessionEnd, which of its two hooks is missing), in `$CLAUDE_CONFIG_DIR` when it
  is set;
- whether the skill is linked;
- when the Copilot folder exists, four Copilot lines: whether a `*.json` file in its `hooks` folder runs
  `gopola copilot-hook`, the record types in Copilot sessions that gopola does not know (a Copilot update that adds
  one shows here; the record is skipped and the session's other records are still indexed, except a `subagent.*`
  record, which makes that session indexed as nothing), how many lines of a known type gopola cannot read, per
  session (a field Copilot renamed shows here; a last line with no newline yet is not counted), and how many Copilot sessions there are, by
  the CLI version each started on (a resumed session keeps its first version);
- when the Codex folder exists, the same four lines for Codex: whether `hooks.json` or `config.toml` in it runs
  `gopola codex-hook` (`Codex SessionStart hook wired` or `missing`), the record types in Codex rollouts that gopola
  does not know (a subagent's rollout shows here as `subagent.session_meta`), the lines of a known type gopola cannot
  read, per session, and how many Codex sessions there are, by CLI version. A rollout Codex has compressed is read
  through; one that only gopola's archive still holds is left out;
- how many turns have meaning-search vectors, and whether Ollama answers;
- searches that found nothing in the last 30 days, and long sessions with no summary.

gopola logs every query command locally: `search`, `recent`, `show`, `file` (and `file --hook`), `timeline` (and
`timeline --query`), `tools`, `digest`, `commands`, `dispatches`, `hooks`, `weight`, `authored`, `blame` and `embed`. It logs
the command, its argument and the hit count; nothing leaves the machine. `doctor` reads that log to show what gopola failed to
answer.

### The skill

`skills/gopola/SKILL.md` tells a Claude Code session when to search past sessions instead of re-reading
transcripts, and which command answers which question.

## The index

The database is `~/.local/share/gopola/gopola.db` (`GOPOLA_DB`). It is built from the transcripts in
`~/.claude/projects` (`GOPOLA_PROJECTS`), the Copilot CLI sessions in `~/.copilot/session-state` (`COPILOT_HOME`; see
[Copilot CLI](#copilot-cli)), the Codex CLI rollouts in `~/.codex` (`CODEX_HOME`; see [Codex CLI](#codex-cli)) and the
transcripts that `import` copied into `imports/` next to the database.

### What a turn is

A turn is one of your prompts, up to your next prompt. It stores:

- your words, verbatim;
- the last text block of the assistant's reply;
- the files the assistant edited or wrote;
- each Bash command and each skill (`Skill <name> <args>`) it ran.

The CLI writes a prompt as a string. The VS Code extension writes it as a list of text blocks; gopola reads those
and leaves out the editor context (`<ide_opened_file>`).

Harness blocks are removed before storage: `<system-reminder>`, `<task-notification>`, `<persisted-output>` and
system-instruction tags. A prompt that is only a task notification, or that is entirely private, is not a turn.

### Commits

A session's commits come from git, not from the transcript. gopola runs `git log --all` in the session's launch
directory between the session's first and last timestamp. That works however the commit was made (a heredoc
message, a commit from a script). A commit made while two sessions were open in one repository is listed under both.
The stored list has missed commits once (0 recorded where the rule, run again, found 9; the cause is not known yet),
so `authored` and `blame` run the rule again instead of reading it, while `recent`, `timeline` and `digest` still
read the list.

### Projects

The project is the base name of the launch directory, decoded from the name of Claude Code's transcript folder.
Claude Code turns every character that is not a letter or digit into `-`, so gopola matches the segments against
real directories (`my_app`, `.claude`).

- **Two checkouts with the same name** are two projects. The one with the shorter path keeps the plain name; the
  other is `<parent>/<name>`.
- **A task worktree** (`<repo>-wt/<task>`) is filed under `<repo>`.
- **A moved checkout** would split its history in two, because sessions are filed under the folder they were
  launched in. `gopola moved <old path> <new path>` records the move once: it refiles the indexed sessions of the
  checkout and its `-wt` task worktrees under the new path and project name, and every later `index`, `--full`
  included, reads the old transcript folders as the new path. `authored` and `blame` then run git in the new folder.
  You can also rename Claude Code's transcript folder, so that `claude --resume` finds the sessions from the new path.
  Then the next `index` records each transcript's new file.
- **`CLAUDE_PROJECT_DIR`** overrides the project for scripted sessions.
- **`GOPOLA_EXCLUDED_PROJECTS`** (comma-separated names or path fragments) keeps projects out of the index.
- **SDK sessions** (`entrypoint: sdk-cli`: scripted agents, other tools' observers) are recorded as seen and indexed
  as nothing. Copilot's scripted runs (`copilot -p`) and Codex's (`codex exec`) are the opposite: they are indexed,
  and `recent` folds them (see [Copilot CLI](#copilot-cli) and [Codex CLI](#codex-cli)).

### Privacy

- Text between `<private>` and `</private>` in a prompt or reply is stored as `[private]`. It cannot be searched
  and is never sent to a model.
- Secret-shaped strings are replaced with `[redacted:<kind>]` before storage and in every command's output: AWS
  keys, GitHub and Slack tokens, `sk-` keys, JWTs, bearer tokens and PEM private keys.
- The transcripts on disk are never changed.

### Incremental indexing

Transcripts are append-only, so `index` starts again from the byte where the last turn began. It rewrites that
turn and never duplicates it. As a guide: 1,090 transcripts (870 MB) took 7.5 s to index from scratch into a 19 MB
database, and an unchanged run takes 10–50 ms.

`index --full` rebuilds from scratch, reading each transcript, or its copy in the archive once Claude Code has
deleted it. It keeps summaries and imports, and a session with neither a transcript nor a copy keeps everything the
index holds for it.

### The archive

Claude Code deletes transcripts older than its `cleanupPeriodDays` setting (30 days by default). gopola keeps its
own copy of each one, so nothing it has read is lost:

- **What:** every file under the projects folder (sessions' transcripts, their subagents' transcripts and the large
  outputs saved beside them), except each project's `memory/` folder.
- **When:** once a file has gone an hour without changes, and again whenever it changes. `index` does this at most
  every 10 minutes; `gopola archive` does it now.
- **Where:** `archive/` next to the database, in the same layout as the projects folder, each file compressed with
  zstd (`<session>.jsonl.zst`) and stamped with its original's modification time.
- **How much:** on 2026-11-09, the 4,387 files under `~/.claude/projects` (8.4 GB) became 1.1 GB, in 39 s, holding
  at most 14 MB of memory. A pass with nothing to copy takes about 0.1 s.

`commands`, `weight`, `dispatches`, `hooks`, `handback` and `index` read the original while it is there and the copy once it
is gone. Sessions deleted before the archive existed (before 0.14.0) keep what the index already holds: their
turns, commits and summaries, but not their commands or weight.

Copilot sessions are archived the same way, in their own folder, `archive-copilot/` next to the database (a Copilot
session's `<id>/events.jsonl` becomes `archive-copilot/<id>/events.jsonl.zst`). Only `events.jsonl` is copied:
gopola never reads `workspace.yaml`. No deletion by Copilot has been seen (a session from 2026-08-22 was still there
81 days later), so this is insurance. `index` reads the copy once the original is gone.

Codex rollouts are archived in `archive-codex/`, in the layout of the Codex folder (`sessions/2026/11/11/rollout-….jsonl`
becomes `archive-codex/sessions/2026/11/11/rollout-….jsonl.zst`). Only plain rollouts are copied. Codex compresses a
quiet rollout itself (`rollout-….jsonl.zst`), and `codex archive` moves one under `archived_sessions/`; gopola reads
either and keeps one row for the session, whose path follows the file. `codex delete` removes a rollout, and `index`
then reads gopola's copy; with a copy under both `sessions/` and `archived_sessions/` (`codex archive` and
`codex unarchive` move a session both ways), the newer copy wins.

### Copilot CLI

gopola indexes GitHub Copilot CLI sessions beside Claude Code's, so `search`, `show`, `recent`, `file`, `timeline`,
`digest` and the archive cover both. A Copilot session is in `~/.copilot/session-state/<id>/events.jsonl`
(`$COPILOT_HOME/session-state` when `COPILOT_HOME` is set; the tests point it at a temporary folder). The session id
is the folder's name.

**What a Copilot session gives each command**

- **The turn** is one prompt you typed and everything up to the next one, as for Claude. Copilot's own `turnId`
  counts model turns, so gopola does not use it. Text that a skill injected is not a prompt. The reply is the last
  non-empty assistant message of the turn. Commands are the `bash` calls and `Skill <name> <args>`, files edited are
  Copilot's file edits, and files read are its `view` calls.
- **The project** is the one a Claude session started in the same directory is filed under, so one project's list
  has both harnesses. The launch directory is the session's first record.
- **The title** is the one Copilot asks a small model for at the start of an interactive session (CLI 1.0.105 and
  later).
- **The mark.** `recent`, `show`, `search` hits and `file` rows say `copilot` for a Copilot session and nothing for a
  Claude one. `--json` has `"harness": "claude" | "copilot"` and `"scripted"`.
- **`commands`, `dispatches` and `hooks` read Claude Code's records only** for now. Given a Copilot session they exit 1
  and say so, instead of printing nothing. `weight` reads Copilot's `events.jsonl` by session id or path.

**Scripted runs.** A Copilot session on CLI 1.0.105 or later that never asked for a title is a scripted run
(`copilot -p`); older CLIs have no title call, so their sessions are never marked scripted. gopola indexes it:
`search`, `show` and `file` find it. `recent` and the
session-start block do not list it: a burst of probes would push your own sessions out of the list, so each project
gets one line, `+ 6 scripted Copilot runs, last 2026-11-11 11:09` (with `--project all`, the count per project too).
`recent --all` lists them one by one. A project whose only sessions are scripted runs prints that line and no
sessions. A session that gains its title call later stops being scripted at the next `index`.

**The session-start hook.** `install.sh` writes `~/.copilot/hooks/gopola.json`, a `sessionStart` hook that runs
`gopola copilot-hook`. It runs `index --quiet`, then `recent --exclude <its sessionId>` for the project of the
session's `cwd` (the hook fires before Copilot's title call, so the session would count itself as a scripted run), and hands the
text to Copilot as `additionalContext`, so a new Copilot session opens with the same block a Claude session opens
with (and `index` there already sees Copilot sessions, so a Claude session lists them too). Copilot runs the hook
with the session's JSON on stdin and treats exit 2 as a refusal, so `copilot-hook` exits 0 whatever happens and
prints nothing when the index or `recent` fails.

**No list in a `copilot -p` run.** A reviewer or a trial run should see only its brief, and every run would pay for
the extra context. The hook finds out from the process that started it: it walks up to four ancestors with
`ps -o ppid=,args=` to the first whose program is named `copilot`, and prints nothing when that process's arguments
hold `-p`, `--prompt` or `--prompt=…`. The check runs first, so a scripted run costs one `ps` call. When no ancestor is
named `copilot`, the hook cannot tell and prints nothing too: it gives the list only when it found a `copilot` ancestor
whose arguments hold none of those. No launcher has to set anything; this was checked on CLI
1.0.107 (a scripted run's parent was `copilot -p <prompt> --no-ask-user --max-ai-credits 30`).
`GOPOLA_COPILOT_PARENT_ARGS` replaces the `ps` lookup with the arguments you give it, as if they were the `copilot`
ancestor's; set but empty, it stands for "no `copilot` ancestor found", so the hook prints nothing. The tests use it.

### Codex CLI

gopola indexes OpenAI Codex CLI sessions beside Claude Code's and Copilot's, so `search`, `show`, `recent`, `file`,
`timeline`, `digest` and the archive cover them. A Codex session is one rollout file,
`~/.codex/sessions/YYYY/MM/DD/rollout-<local time>-<id>.jsonl` (`$CODEX_HOME` instead of `~/.codex` when
`CODEX_HOME` is set and not empty, as Codex itself does; the tests point it at a temporary folder), or the same name
under `archived_sessions/`, plain or compressed by Codex (`.jsonl.zst`). The session id is the last 36 characters of
the name. gopola never writes under the Codex folder, except `codex-hook --install` adding its handler to
`hooks.json`, and never opens Codex's SQLite files or `history.jsonl`. Checked on CLI 0.167.0.

**What a Codex session gives each command**

- **The turn** is one prompt you typed (a completed `UserMessage` item) and everything up to the next one. The reply
  is the turn's last agent message. Commands are the command lines of the `CommandExecution` items Codex records for
  every command it runs; files read are the paths Codex itself classified as reads in them (`sed -n 1p README.md` reads
  `README.md`), and files edited are those of its `FileChange` (`apply_patch`) items. A file written through the shell
  is not an edit, as with Claude's Bash. gopola does not parse the JavaScript of Codex's `exec` tool.
- **The project** is the one a Claude session started in the same directory is filed under. The launch directory,
  branch and CLI version are the rollout's first record (`session_meta`). A resumed session appends to its rollout and
  keeps its first version.
- **The title** is the newest `thread_name` Codex wrote for the session in `~/.codex/session_index.jsonl`. `codex exec`
  runs have none.
- **A fork** is its own session, holding only its own turns. **A subagent's rollout** gets a row and no turns, and
  `doctor` names it.
- **The mark.** `recent`, `show`, `search` hits and `file` rows say `codex`; `--json` has `"harness": "codex"`.
- **`commands`, `dispatches`, `weight`, `hooks` and `context` read other harnesses' records only.** Given a Codex
  session (an id, or for `weight` a rollout's path under the Codex folder, `.jsonl` or Codex's `.jsonl.zst`) they exit
  1 and say so.

**Scripted runs.** A rollout whose first record says `source: "exec"` or `originator: "codex_exec"` is a scripted
`codex exec` run. As with Copilot's, it is indexed, `search`, `show` and `file` find it, and `recent` folds it into one
line per project, counted per harness: `+ 2 scripted Copilot runs, 1 scripted Codex run, last …`.

**The session-start hook.** `gopola codex-hook --install <bin>` (which `install.sh` runs) adds one `SessionStart`
handler, `'<bin>'/gopola codex-hook` (a relative `<bin>` made absolute) with a 10 s timeout, to
`<CODEX_HOME>/hooks.json`. It keeps every other handler and event in the file, replaces an older gopola handler where it
stands (one from another bin, say), prints `left … alone, unchanged` when the file already says exactly that, and
otherwise writes the file through a temporary file and prints `wrote …`. A rewrite keeps every value but sorts the
keys. A `hooks.json` that is not JSON is left alone: the command exits 1. **Codex runs a new hook only after you
approve it once** in its interactive UI. Codex saves that approval in `config.toml`, under
`hooks.state."<path of hooks.json>:session_start:0:0"`, an entry that holds a `trusted_hash` of the handler. So a
handler `--install` rewrites (from another bin, say) may ask for approval again, and so may another handler in the file
whose keys the rewrite sorted, if the hash covers the text as written (neither is checked yet on a real approval).

`gopola codex-hook` reads Codex's `SessionStart` JSON on stdin (`cwd`, `hook_event_name`, `model`,
`permission_mode`, `session_id`, `source`, `transcript_path`). For `source` `startup` or `resume` it runs
`index --quiet`, then `recent --exclude <its session_id>` for the project of `cwd`, and answers with the block as
`hookSpecificOutput.additionalContext`. After `compact` or `clear` it says nothing. It never reads `transcript_path`,
because the rollout can appear late. It always exits 0.

**No list in a `codex exec` run.** The hook walks up to four ancestors with `ps -o ppid=,args=` to the first whose
program is named `codex`, and prints nothing when that process's subcommand is `exec` (or its alias `e`): the first
word after the root options of `codex [OPTIONS] <COMMAND>` and their values (`codex -c k=v exec …` is an exec run;
`codex -m exec` is not). In every other case, no `codex` ancestor found included, it gives the list. Checked on CLI
0.167.0 on 2026-11-12: in a `codex exec "say hi"` run, the hook fired after the prompt was read, and its parent was
`codex exec say hi`.
`GOPOLA_CODEX_PARENT_ARGS` replaces the `ps` lookup with the arguments you give it, as if they were the `codex`
ancestor's. The tests use it.

### The tables

`gopola.db` is one SQLite file. gopola upgrades an older database in place when it opens it, and records the schema
version (9) in `meta`.

| Table | One row per | What it holds |
|---|---|---|
| `sessions` | session | The id, project, launch directory, task worktree, branch, first and last timestamp, first prompt and title; the harness (`claude`, `copilot` or `codex`), its version, and whether the session is a scripted run; the prompt count and the transcript's path; how far `index` has read it (the bytes read, and the byte and turn number where the last turn began); the summary, when it was made and at how many turns. |
| `turns` | turn of a session | The timestamp, your prompt, the last text of the reply, the files edited (a JSON list), the tools used (JSON, name to count) and the commands run (a JSON list). |
| `turn_files` | file a turn touched | The path, and whether the turn edited or read it. `file` looks paths up here. |
| `commits` | commit made during a session | The sha, timestamp and subject. |
| `fts` | piece of searchable text | A full-text index (SQLite FTS5, with stemming) of every prompt, reply, turn's commands, title, summary, commit subject and imported observation, with its kind, what it points at (`<session>:<turn>`, a sha or an observation id), its project and its timestamp. |
| `fts_ref` | row of `fts` | Its kind and what it points at, so a turn's rows are removed without scanning the index. |
| `vectors` | turn and embedding model | The meaning-search vector, and a hash of the text it was made from. |
| `observations` | imported claude-mem observation | What `import-claude-mem` copied. |
| `moves` | moved checkout | The old path and the new one, from `gopola moved`. |
| `injected` | session and file | That `file --hook` has told that session about that file, so it says so once. |
| `usage` | query command run | The command, its argument and how many hits it had; `doctor` reads it. |
| `meta` | setting | The schema version, and per harness the record types `index` could not place, each with the first session that held it: `copilot_unknown_types` and `codex_unknown_types`. |

## Finding past work

### `search`

    gopola search <query> [--project P|all] [--since D] [--until D]
                  [--kind prompt|reply|command|commit|summary|title|observation]
                  [--order rank|date] [-n 10] [--words | --meaning]

Ranks turns by your words and, when vectors exist, by meaning, fused into one list. Each hit names its session and
turn (`c0ffee42:1`), so `show` or `timeline` can open it.

- `--words` is word search only. `--meaning` is meaning only. `--kind` also turns meaning off.
- `--kind command` searches the commands sessions ran. Use it when a command, not a prompt, holds the answer
  ("which session ran `docker compose`?").
- When no turn holds all the words, `search` falls back to any of the words and says so. With vectors, meaning
  replaces that fallback.
- With no vectors, Ollama is not asked. With vectors but Ollama down, the first line is `# meaning search off: …`
  and the search uses words.

### `show`

    gopola show <session> [--turn N] [--grep <word>]

Prints a session turn by turn: time, your prompt (`>`), the reply (`<`), files, commands (`$ …`). `--turn N` prints
one turn in full. `--grep` keeps the turns whose prompt, reply or commands mention the word, with the matching
commands. For a heredoc, it shows the first line and the line that matched.

### `timeline`

    gopola timeline <session:turn | session | commit-sha> [--before 3] [--after 3]
    gopola timeline --query <words>

What happened around a point:

- `session:turn`: the turns before and after that turn.
- `session`: the sessions before and after it in the same project.
- `commit-sha`: the turns around the commit.
- `--query`: the best prompt or reply hit becomes the anchor.

### `recent`

    gopola recent [--project P|all] [-n 6] [--commits 2] [--lines 30] [--no-footer] [--all] [--exclude <session-id>]

The session-start block, at most `--lines` lines. It lists Claude Code, Copilot and Codex sessions together; a
Copilot or Codex session has `copilot` or `codex` after its id, and scripted runs are one line at the end, counted per
harness (`--all` lists them one by one; see [Copilot CLI](#copilot-cli) and [Codex CLI](#codex-cli)). `--exclude`
leaves one session (its full id) out of the total, the list and the scripted line; `copilot-hook` and `codex-hook`
pass their own session:

    # gopola: myapp, last 4 sessions (of 4)
    2026-10-07 12:00  f00dcafe  main  1 turn  1 commit  Expire old tokens
      > now expire tokens after 30 days
      ✓ session: expire tokens after 30 days
      < Tokens older than 30 days are deleted each time one is issued.
    2026-10-07 10:00  decade01  main  2 turns  2 commits  Move sessions into a token store
      > move sessions into a token store
      ✓ session: store tokens in sqlite
      ✓ migrations: the tokens table
      < Tokens now live in sqlite, and sign-in issues them from the store.
    …
    # gopola search <words> | show <id> | timeline <id:turn> | file <path> | digest --since <date>

For each session:

- the start time, id, branch, turn count and commit count;
- the title Claude Code gave the session (from its `ai-title` records; "Continue session" is ignored);
- your first real prompt (`>`);
- the last `--commits` commit subjects (`✓`);
- the first line of the assistant's last reply (`<`).

A summarised session shows its three summary lines instead of `>` and `<`. A session of 20 or more turns with no
summary is tagged `[long, unsummarised]`. The block ends with one line that names the other commands;
`--no-footer` removes it. A typical block is under 30 lines, about 500 tokens.

### `file`

    gopola file <path> [-n 10] [--reads]
    gopola file --hook

Lists past sessions that edited the file (or read it, with `--reads`), with what each was asked.

`--hook` is the PreToolUse form. It reads the hook's JSON on stdin and answers with at most three lines as
`additionalContext`, only when a *different* session edited the file, and once per session and file. A file with no
history adds nothing, so the common case costs nothing.

### `digest`

    gopola digest [--since D] [--until D] [--project P|all] [--commits 3]

Per project, per day: sessions, turns, commits, session titles and the last commit subjects. A stand-up or weekly
summary with no model.

### `tools`

    gopola tools [--project P|all] [--since D] [-n 20] [--programs]

Counts the tools past sessions called (calls, and sessions that used each one). `--programs` counts the programs
that Bash commands ran instead: the first word of each simple command, after variable assignments and wrappers such
as `sudo`, `env` and `xargs`. Quoted text, heredoc bodies, shell keywords and skill calls are not counted. A program
repeated within one turn counts once.

## Inside a session

These commands read the transcripts when you run them. They add nothing to the index.

### `commands`

    gopola commands <session> [--grep <text>] [--head N]
    gopola commands --grep <text> [--project P|all] [--since D] [--until D] [--head N]

Each Bash call, in full: the time of the call, the session, the source (`session` or `agent <id>`), the status,
the time of the result, the command and the output.

- `--grep` keeps the calls whose command contains the text exactly (case-sensitive). Without a session, `--grep` is
  required and every session of the project in the date range is read.
- `--head N` cuts each output to N lines and says how many it cut. Nothing is cut without it.
- Subagents' calls are included, from their own transcripts.
- A large output that Claude Code saved to a file is printed from that file; if the file is gone, from the 2 KB
  preview in the transcript.
- Secrets are redacted with the same rules as the index.

The status is one of:

| Status | Meaning |
|---|---|
| `ok` | the call succeeded |
| `ok (<why>)` | a non-zero exit that Claude Code read as success, such as "No matches found" |
| `exit N` | the call failed with code N (the `Exit code N` line is removed from the output) |
| `interrupted` | the call was stopped |
| `background <task id>` | the call was moved to the background |
| `error` | a hook or a permission refused the call, or the tool failed; the output says which |
| `no result` | the session stopped first, or the command is still running |

Example:

    $ gopola commands --grep 'cargo test auth' --project all
    # commands all matching "cargo test auth": 3 Bash call(s), 0 of them by subagents; 4 session(s) read

    2026-10-05 09:03:00Z  5ca1ab1e  session  exit 101  → 09:03:41Z
    $ cargo test auth
    running 3 tests
    test auth::tests::allows_five_tries ... ok
    test auth::tests::blocks_the_sixth_try ... FAILED
    test auth::tests::resets_after_a_minute ... ok

    test result: FAILED. 2 passed; 1 failed

    2026-10-05 09:06:00Z  5ca1ab1e  session  ok  → 09:06:38Z
    $ cargo test auth
    running 3 tests
    test auth::tests::allows_five_tries ... ok
    test auth::tests::blocks_the_sixth_try ... ok
    test auth::tests::resets_after_a_minute ... ok

    test result: ok. 3 passed; 0 failed
    …

The first run failed with exit code 101: that is its status, and the output starts after the `Exit code 101` line.

`search --kind command` ranks by words and can rank another session first. `commands --grep` is an exact match.

`--json`: `{scope, grep, sessions_read, commands: [{session, source, agent, ts, result_ts, status, exit, command,
output}]}`.

### `dispatches`

    gopola dispatches <session> [--grep <text>] [--head N]

Each `Agent` and `SendMessage` call of a session, in order. Each row has:

- the time, the tool and the description (for `SendMessage`, its summary or the first 60 characters of the message);
- the agent the call reached, and for `Agent`, the model, type and whether it ran in the background;
- the outcome: `async launched`, `completed`, `sent`, `not sent: <why>`, `refused: <first line>` or `no result`;
- the hand-back: `returned <time>`, `stopped <time>` (the agent was killed, so its last text is not a report), or
  `none`.

Then the whole prompt or message. `--head N` cuts each prompt to N lines. `--grep` keeps the calls whose prompt or
description contains the text.

Use it to resume or re-dispatch stopped agents: the row says which agents handed back, and the prompt is the brief to
send again.

    $ gopola dispatches c0ffee42
    # dispatches c0ffee42: 2 Agent call(s), 0 SendMessage call(s); a hand-back after 1 of the 2 that reached an agent

    2026-10-06 14:01:00Z  Agent  "Review the sign-in limit"  → agent a3d9f1c27b4e8a605 (general-purpose, background)  async launched  hand-back: returned 14:12:00Z
    Review the sign-in limit in src/auth.rs: five tries per address per minute.
    Report anything that lets a client get past it.

    2026-10-06 14:01:30Z  Agent  "Write the docs page"  → agent a8e2b6d04c1f9a317 (general-purpose, background)  async launched  hand-back: none
    Write docs/auth.md: what the sign-in limit is and what a blocked client sees.

A hand-back counts for a call when it arrives after that call and before the next call to the same agent. A message
sent to a named agent or another session shows the name and has no hand-back state. The session must be indexed.

`--json`: `{session, transcript, grep, dispatches: [{ts, tool, label, to, recipient_kind, model, subagent_type,
background, agent, outcome, handback: {ts, state} | null, prompt}]}`.

### `hooks`

    gopola hooks <session> [--grep <text>] [--head N]

Each hook run of a session and its subagents, in time order. Each row has the time, the hook (for example
`SessionStart:startup`, `PreToolUse:Bash` or `Stop`), the status, the duration and the command. The table shows the
first line of the command, cut at 160 characters. `--json` gives the whole command.

The status is one of these:

- `ok` or `exit N`: the hook ran and exited with that code.
- `blocked`: the hook stopped a tool call or a stop. Claude Code records a block only in the tool result, or in a
  `Stop hook feedback` record, so a block has no duration.
- `timed out` or `cancelled`.
- `message`: the hook showed a message.
- `context`: the hook added text to the context.

Under a row, the lines that start with `!` show the stderr, the reason for a block, or the message. `--head N` adds
the first N lines of the output (the stdout, or the text that the hook added to the context) with `>`. `--grep` keeps
the runs whose command or hook name contains the text. An async hook records no command, and a hook that blocks with a JSON reason records no command either. For these
runs, the command shows as `-`.

    $ gopola hooks 5ca1ab1e
    # hooks 5ca1ab1e: 4 hook run(s), 0 by subagents, 2 not ok (exit, blocked, timed out, cancelled)
    2026-10-05 09:00:01Z  5ca1ab1e  session  SessionStart:startup  ok  410ms  gopola index --quiet
    2026-10-05 09:33:00Z  5ca1ab1e  session  PreToolUse:Bash  ok  9ms  ~/.local/bin/commitguard
    2026-10-05 09:34:00Z  5ca1ab1e  session  PreToolUse:Bash  blocked  -  ~/.local/bin/commitguard
      ! --no-verify skips the pre-commit checks; commit without it
    2026-10-05 09:36:01Z  5ca1ab1e  session  Stop  exit 1  230ms  ~/.local/bin/docscheck
      ! docs/auth.md: no such file (README links to it)

The session must be indexed.

`--json`: `{session, grep, hooks: [{session, source, agent, ts, hook, status, duration_ms, command, message,
output}]}`.

### `context`

```
gopola context <session-prefix> [--grep <text>] [--json]
```

What a Copilot session's model was told, in the order the session recorded it: its system prompt
(`system.message`), each instruction update (`instruction.update`, by name) and the context each hook added
(`hook.end`'s `additionalContext`, by hook type). Copilot merges the output of every hook of one type into one record,
so a `sessionStart` part holds gopola's block and any other hook's text together. Each part prints a `--- <time>
<kind> <name>  <N> chars` line and then its text; secret-shaped strings are redacted, as everywhere in gopola.

`--grep <text>` (an exact, case-sensitive substring) prints, for each part, how many times the text occurs and the
lines that hold it, then a total. This is the check that a rule reached the prompt: `gopola context <id> --grep
"docs/board.md"` says `0 matches` when the rule never arrived. The command reads the archive copy once Copilot's own
file is gone.

A Claude Code session is refused: its transcript holds no system prompt, and its hooks' context is in `gopola hooks`.

`--json`: `{session, parts: [{ts, kind, name, chars, text}]}`; with `--grep`, each part has `matches` and `lines` in
place of `text`, and the object has the total `matches`.

### `handback`

    gopola handback <agent-id-prefix>

Prints the last report a subagent returned to the session that started it, without the frame and indent the
harness adds. Useful when an agent returned its report as its final message and wrote no report file.

- Exit 0: a report was printed.
- Exit 1: the agent has returned nothing yet (the message names the transcript it read).
- Exit 2: no such agent, or the prefix matches two agents (the message names them).

An agent resumed with `SendMessage` has several reports; the last one is printed. A stopped agent has no report: the
command prints its last text with exit 0, so check before using it as a report.

gopola finds the report in the parent session's transcript, not in the agent's own transcript, which does not hold
it reliably. It reads four record shapes:

| Shape | Record | Where the report is |
|---|---|---|
| `peer_message` | `user`, text content | after `<agent-message from="<id>">` and a `[Subagent hand-back]` line |
| `queued_command` | `attachment` of type `queued_command` | `attachment.prompt`, in the same frame or as a task notification |
| `task_notification` | `user`, text starting with `<task-notification>` | between `<result>` and `</result>` |
| `tool_result` | `user`, with `toolUseResult.agentId` | `toolUseResult.content`, from a foreground `Agent` call |

A notification that only says the report was delivered as a message holds no report and is skipped.

`--json`: `{agent, transcript, shape, text}`.

### `weight`

    gopola weight <session | transcript.jsonl> [-n 8]

Why a session got slow, and what filled its context: the context each model call carried, when compaction fired,
and what started each turn. It reads the transcript directly, so it works on a session that is not yet indexed.

    $ gopola weight decade01
    # weight decade01  myapp  2026-10-07 10:00 → 2026-10-07 11:26
    chain decade01: 2 transcripts, a new one at each /clear; the figures after this table are part 1's
      part  session   from              to                 calls  first  median     max  compactions
         1  decade01  2026-10-07 10:00  2026-10-07 11:26      9    22k     66k    161k  1
         2  f00dcafe  2026-10-07 12:00  2026-10-07 12:21      3    20k     27k     35k  0

    model calls  9   context per call: first 22k  median 66k  p90 142k  max 161k   output 5k
    compactions  1 (idle: no record for 5 min or more before it)
      2026-10-07 11:19  auto  167k → 24k  16s quiet before it, 41s to compact

    what started each turn      turns   calls   context processed   its text
      human                         2       9       705k (100%)   ~17

    tool results (≈ bytes/4; images at most 4784 tokens each)
      Bash                         3 calls   ~51
      Edit                         3 calls   ~33
      Read                         1 calls   ~23

    injected by hooks  ~0

What is measured, and what is estimated:

- **Measured:** each call's context is the `usage` Claude Code logged for it (input + cache creation + cache read).
  One API message spans several records, so a call is counted once per `message.id`. Compactions come from
  `compact_boundary` records.
- **Quiet before a compaction:** the time from the record before it to its start. A compaction is marked `idle`
  when that is 5 minutes or more. An idle compaction is not a size limit: the session sat still before it.
- **Attributed:** each call is charged to its turn, and each turn to what started it: `human`, `peer` (another
  agent's message or hand-back), `task_notification` or `scheduled`. A compaction summary keeps the origin of the turn
  it interrupted.
- **Estimated:** tool results and prompt text at four bytes per token. Images are counted at the per-image cap. Hook
  output counts only what reaches the model, never raw stdout.
- `first` is the first call's context: what a session pays before any work.

**`/clear` chains.** A `/clear` starts a new transcript that keeps the first transcript's id in `session_id`.
When a transcript is part of such a chain, `weight` prints one row per part first, as in the example above:
`decade01` and the transcript `f00dcafe` that its `/clear` started.

A job moved to the background starts with a copy of the session it came from, so its copied calls count in both. Do
not add up the parts of two chains.

`--json` adds `quiet_s`, `duration_ms` and `idle` per compaction, and `chain: {id, parts: […]}` or `null`.

**Copilot CLI.** For a Copilot session, `weight` reads only its `events.jsonl`; it never prints prompt, system, reasoning,
tool-argument, error or file content. The report has a segment for the start and each resume, preserving the cap from
the original start across resumes. It reports the context and Git state at each segment's start and stop, the commits
on its branch, per-call model and usage (including Fusion's model choice per call), byte counts for the first brief and
system prompt, tool counts, and refusal sources. The call table is shown only with `-n N`; `--json` always carries the
complete call array and segment totals.
For a refused shell command, a bare command name of 1–40 ASCII letters, digits, `.`, `_`, `+` or `-` is shown;
an invoked path is shown as `<path>`, and any other command word as `?`.
Zshguard rule names come from zshguard 0.13.0's list; other bracketed rule names are shown as `?`.

A segment with no shutdown is `killed`, even if later resumed; its wall time ends at its own last record.
Read the report after the seat ends: a still-running seat also has no shutdown and will read as `killed`.
A shut-down segment stops as `cap` when its last `Session limits` line says the next model call is final, or its
recorded credits reach its start cap. Otherwise, a shutdown type containing `error` or `fail` is `error`, and a normal
shutdown is `done`. The error rule is tested with invented records, not verified against a real error shutdown;
the per-call finish reason does not decide the segment's stop reason. `--json` includes credits used and cap, wall time,
each segment's own shutdown token split and sanitized code-change counts. A missing repository or branch yields `unknown` for the
stop-time HEAD and `unknown` for commits (`null` in `--json`), not an empty list. Commit windows and stop-time HEAD
use author time, so a later rebase keeps the seat's commits with their current shas. The window can also include base
commits authored during the seat and rebased into the branch. Run `weight` after the seat ends and before its worktree
is removed; once the Git root is gone, its commit history is unavailable.

## Code from sessions

Git cannot tell code a session wrote from code you wrote, when both are committed under your name. These commands
can.

### `authored`

    gopola authored [<repo>] [--ref R] [--since D] [--path P] [--author E] [--no-merges] [--no-bumps]

Lists the commits on a ref that came from Claude Code sessions, one line each: date, sha, sessions, how it was
matched, lines added and deleted under `--path`, and subject. Then the totals, the top directories by lines added,
and the commits it did not count.

The default ref is `origin/HEAD`, else `HEAD`. A commit is counted when:

1. gopola recorded it for a session of this repository (a checkout, a subdirectory, or a worktree), or `git log` in
   the session's window finds it now;
2. its author email is the repository's `user.email` (or `--author`), which removes teammates' commits that fall
   inside a session's window;
3. it can be followed onto the ref. The `match` column says how:
   - `sha`: the same commit is on the ref.
   - `patch-id`: a commit on the ref, not older than the session, has the same `git patch-id --stable`. This finds
     rebases and cherry-picks.
   - `subject-only`: the first commit on the ref with the same subject, not older than the session. This finds
     squashes and rewrites, and can be wrong for a generic subject.

`--no-merges` and `--no-bumps` (version-bump subjects) remove those commits and say how many. `--json` lists the
commits that were not counted under `not_counted`, so you can audit the result.

    $ gopola authored
    # authored  myapp  HEAD  (sessions commit as dev@example.com)
    2026-10-07  4e858a2b5  f00dcafe           sha              +3     -0  session: expire tokens after 30 days
    2026-10-07  de3fc10f6  decade01           sha              +1     -0  migrations: the tokens table
    2026-10-07  bd2dd1d92  decade01           sha             +12     -1  session: store tokens in sqlite
    2026-10-06  d8a5169ad  c0ffee42           sha              +3     -0  docs: describe the sign-in limit
    2026-10-06  041a901ce  c0ffee42           sha              +4     -3  auth: key the limit on the forwarded address behind a trusted proxy
    2026-10-05  5d8f3df6e  5ca1ab1e           sha              +1     -0  auth: log blocked sign-in attempts
    2026-10-05  9d1392983  5ca1ab1e           sha              +8     -1  auth: limit sign-in attempts per address
    7 commits from 4 sessions · +32 -5 lines in 4 files · match: 7 sha
    top directories (lines added): src +28 · docs +3 · migrations +1
    not counted: 1 recorded by other authors (Other Dev 1)

### `blame`

    gopola blame <file|dir> [--ref R] [--summary | --ranges] [--no-ignore-whitespace] [--since D] [--author E]

Marks each line that survives on the ref with the session whose commit wrote it, using the same commits as
`authored`. It runs `git blame --line-porcelain -w` for each file, on all cores. `-w` ignores whitespace-only
changes, so a re-indent does not take a line away from the session that wrote it; `--no-ignore-whitespace` turns
that off.

- Default: every line with its sha and session (`-` when no session wrote it).
- `--summary`: each file with session lines, then a count of files with none.
- `--ranges`: for each file, one line per session with its lines folded into ranges.

```text <!-- planlint-ok: the demo's output, about a sample project -->
$ gopola blame src/auth.rs --ranges
# src/auth.rs @ HEAD: 10 of 15 lines from Claude sessions (66.7%)
5ca1ab1e  2 5-6 11-12
c0ffee42  7-10
decade01  14
```

The path can be a file that is on the ref but not on disk.

### What these commands cannot tell apart

- A commit you made by hand while a session was open in the same repository counts as the session's: the time and
  the author are the same.
- A commit made while two sessions were open lists both. So does a rebase during a later session.
- Edits made with the Edit and Write tools are not used as evidence, on purpose: sessions often write files through
  Bash, which those tools never see.
- `git blame` gives a line to the last commit that changed it, so a line a later commit moved or reformatted belongs
  to that commit.

## Model calls

gopola works without any model. Two commands use one.

### `summarize` (cloud)

    gopola summarize <session> [--model claude-sonnet-5]
    gopola summarize --auto [--final]

Sends a digest gopola already holds (prompts, closing lines, files, commit subjects, capped near 3k tokens) through
`claude -p --bare --tools "" --no-session-persistence`, and stores three lines: `asked:`, `done:`, `next:`.

`--auto` is the Stop hook form. It reads the hook's JSON, indexes, and calls the model only when the session has
reached 20 turns and grown by 20 since its last summary, and never inside a stop-hook continuation. `--final` (the
SessionEnd form) summarises once more if the session grew since its last summary. Short sessions cost nothing.

The default model is Sonnet: turning a digest into three lines is bounded work. Pass `--model` for a session with
many decisions. Summaries survive `index --full`.

### `embed` (local)

    gopola embed [--limit N] [--quiet]

Sends each turn (the first 2,000 characters of the prompt and the first 4,000 of the reply) to Ollama's
`/api/embed` and stores the vector in `gopola.db`. The key is a hash of that text, so an unchanged turn is never
embedded twice. It uses no quota.

| Variable | Default |
|---|---|
| `GOPOLA_OLLAMA` | `http://localhost:11434` |
| `GOPOLA_EMBED_MODEL` | `embeddinggemma-2:270m` |

The default model is about 350 MB while loaded. Ollama unloads it after five idle minutes, so the first search after
that waits about a second. `--quiet` is the hook form: it always exits 0, and when Ollama is down it prints one line
on stderr.

## Cloud sessions

A cloud session (claude.ai/code, `claude --cloud`) writes its transcript on its own machine, so the local index never
sees it. `import` brings one in.

### `import`

    gopola import <file | ref:path>

Run it in the session's repository. gopola copies the transcript to `imports/<launch dir>/<session>.jsonl` next to
the database and files the session under that repository. Then `show`, `search`, `weight`, `recent` and `timeline`
find it. A `ref:path` argument is read with `git show`.

- An import is a copy. If the session continued after the export, export and import again; the new copy replaces
  the old one.
- A session has one copy. An import from a second repository replaces the first.
- An import wins over Claude Code's own copy of the same session.
- `index --full` keeps imports, and `GOPOLA_EXCLUDED_PROJECTS` applies to them.
- A file with no `sessionId`, or an id with characters other than letters, digits, `-` and `_`, is refused.

### Exporting a cloud session's transcript

Add this as the last step of a cloud session's brief, run in its checkout after its last commit:

    t=$(ls -t ~/.claude/projects/*/*.jsonl | head -1)
    blob=$(git hash-object -w "$t")
    tree=$(printf '100644 blob %s\ttranscript.jsonl\n' "$blob" | git mktree)
    git push origin "$(git commit-tree "$tree" -m "transcript: <job>"):refs/heads/transcript/<job>"

It pushes one commit holding one file on its own branch, and does not touch the work branch or the working tree.
Then, locally:

    git fetch origin transcript/<job> && gopola import origin/transcript/<job>:transcript.jsonl
    git push origin --delete transcript/<job>

**Delete the branch after the import.** It holds the raw transcript: tool output, file contents, and any secret a
command printed. gopola redacts secrets only in what it stores.

The turns after the export step are not in the export.

### A cloud session that was never exported

`claude --teleport <session-id>` writes a copy of a cloud session under `~/.claude/projects`, and `gopola import`
reads that copy. Know the costs first:

- Teleport checks out the session's branch where it runs, and the working tree must be clean. Use a scratch clone.
- In an untrusted folder, the interactive form stops at the trust question. `-p` skips it but sends its prompt:
  one model call carrying the session's whole context. That prompt and reply become the copy's last turn.
- A `-p` copy is marked as an SDK session, which `index` skips but `import` reads.
- The copy is filed under the scratch clone. Run `import` in the real checkout to file it correctly.

The session id is in the session's URL (`claude.ai/code/session_…`).

## Other commands

| Command | What it does |
|---|---|
| `gopola index [--full] [--quiet]` | Brings the index up to date; `--full` rebuilds it (see [Incremental indexing](#incremental-indexing)). |
| `gopola archive [--json]` | Copies every transcript quiet for an hour into the archive now (see [The archive](#the-archive)). |
| `gopola moved <old path> <new path>` | After moving a checkout on disk: its sessions keep their history under the new path (see [Projects](#projects)). |
| `gopola import-claude-mem [path]` | One-off import of claude-mem's observations (default `~/.claude-mem/claude-mem.db`); search them with `--kind observation`. |
| `gopola copilot-hook` | Copilot CLI's `sessionStart` hook: reads `{"cwd": …, "sessionId": …}` on stdin and prints `{"additionalContext": <the recent block>}`, leaving its own session out. Prints nothing in a `copilot -p` run, and always exits 0 (see [Copilot CLI](#copilot-cli)). |
| `gopola version` (`-V`) | The version, the commit it was built from, and the build date. |

## Limits

- **Files written through Bash** (a heredoc, `sed -i`) are not tool file paths, so `file` does not know a session
  changed them.
- **Subagents' transcripts are not indexed.** Their work shows in the parent's replies; `commands` reads them when
  you ask.
- **A session whose transcript is gone** keeps its turns in the index, but `commands`, `dispatches` and `weight`
  have nothing to read.
- **Copilot sessions** have no `commands`, `dispatches` or `hooks`: those read Claude Code's records and refuse a
  Copilot session. `weight` reads its `events.jsonl`. A Copilot session that delegates to a subagent (`subagent.*` records) is indexed as
  nothing until that format is read. A Copilot session's turns reach the index when `index` next runs: there is no
  Copilot hook after each prompt, so that is the next Copilot start or any Claude hook.
- **`weight` estimates** tool results and prompt text at four bytes per token.
- **`authored` and `blame`** attribute by time window and author; see
  [What these commands cannot tell apart](#what-these-commands-cannot-tell-apart).
- **One machine.** There is no sync between machines.
