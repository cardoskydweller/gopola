# gopola
<!-- tool: scope=any | when=before re-deriving anything about a past Claude Code session — "did we already do this?", "which session ran X?", "why was that session slow?", "what did that agent report?", "which hooks blocked?" — search by words or meaning instead of re-reading transcripts | run=gopola search <words> [--project all] [--words|--meaning]; gopola show <session>; gopola commands <session> --grep <text>; gopola weight <session>; gopola dispatches|hooks <session>; gopola handback <agent>; gopola doctor; gopola recent [--project P|all] [-n 6]; gopola blame <file|dir> [--summary] -->

**Search your past Claude Code sessions: what you asked, what was done, what ran, and which code came out of them.**

gopola reads the transcripts Claude Code already writes to `~/.claude/projects` and indexes them into one local
SQLite file. It runs no daemon and no vector database, and it calls no cloud model unless you ask it to.

    $ gopola search "sign-in limit"
    $ gopola commands --grep 'cargo test auth' --project all
    $ gopola weight decade01          # why did that session get slow?
    $ gopola blame src --summary      # which lines on this branch came from Claude sessions?

The command output shown in these docs comes from [`demo/demo.sh`](demo/demo.sh), which builds a throwaway repository
and made-up transcripts under `/tmp/gopola-demo` and runs gopola on them, so you can regenerate each one.

## Why it exists

gopola was built for one developer running many Claude Code sessions a day, often with subagents working in
parallel. The same questions kept coming up: did we already do this, which session ran that command, why did that
session get slow, what did that agent report before it stopped, which of this code did a session write. Each one
took hand-written `jq` over raw transcripts, or re-reading them, every time.

Each command in gopola exists because one of those questions went unanswered in real work. The
[changelog](CHANGELOG.md) names the question behind each version.

Two rules hold throughout:

- **The facts come from what is already on disk:** the transcripts, and git. The index is a searchable cache of
  them, and gopola stores no second copy of tool output.
- **No model unless the work needs one.** Search, timelines, digests and attribution are deterministic. The only
  model calls are optional: summaries of long sessions, and local embeddings for meaning search.

It is fast enough to run on every prompt. On 1,090 transcripts (870 MB), a full index took 7.5 s and made a 19 MB
database; an unchanged re-run takes 10–50 ms.

## What it answers

| Question | Command |
|---|---|
| Did we already do this? Which session was it? | `gopola search <words> [--project all]` |
| What happened in that session? | `gopola show <session> [--turn N] [--grep <word>]` |
| What happened around that hit or commit? | `gopola timeline <session:turn \| sha \| --query <words>>` |
| Which past sessions touched this file? | `gopola file <path>` |
| What exactly did a session run, with its output? | `gopola commands [<session>] --grep <text>` |
| Why did that session get slow, and what filled its context? | `gopola weight <session>` |
| What did that session hand to subagents, and which came back? | `gopola dispatches <session>` |
| Which hooks ran in that session, and which failed or blocked? | `gopola hooks <session>` |
| What was that Copilot session's model told: did a rule reach its prompt? | `gopola context <session> --grep <text>` |
| What did that subagent report? | `gopola handback <agent-id>` |
| Which commits on this branch came from Claude Code sessions? | `gopola authored [<repo>] --ref origin/main` |
| Which surviving lines did they write? | `gopola blame <file\|dir> --summary` |
| What did I do this week? | `gopola digest --since <date> --project all` |
| Which tools and programs do my sessions actually use? | `gopola tools [--programs]` |
| Is everything wired up? | `gopola doctor` |

Every command takes `--json`. Session ids can be given by prefix.

## Install

You need Rust 1.85 or later. gopola runs on macOS and Linux.

    git clone https://github.com/cardoskydweller/gopola
    cd gopola
    ./install.sh                 # installs a copy to ~/.local/bin and links the skill into ~/.claude/skills

Or, without the skill:

    cargo install --git https://github.com/cardoskydweller/gopola --locked

Or download a binary and `SHA256SUMS` from the [Releases page](https://github.com/cardoskydweller/gopola/releases).
Each binary is built by the release workflow from the public source; check it before you run it:

    gh attestation verify gopola-<version>-<target> --repo cardoskydweller/gopola
    shasum -a 256 -c SHA256SUMS --ignore-missing

Then build the index once and check the setup:

    gopola index
    gopola doctor

## Wire it into Claude Code

Add these hooks to `~/.claude/settings.json`. `gopola doctor` tells you which ones are missing.

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

What each one does:

- **SessionStart** prints the last few sessions of this project, in at most 30 lines (about 500 tokens).
- **UserPromptSubmit** keeps the index current, so a search finds earlier turns of the session you are in.
- **PreToolUse on Read** adds up to three lines when a *different* session edited the file being read: who, when
  and what they were asked. A file with no history adds nothing.
- **PostToolUse on edits** indexes in the background after each edit, so the Read hook in another session learns of
  the edit when it happens, not at this session's next prompt. It runs on edits only, makes no model call and keeps
  no copy of the tool's output.
- **Stop and SessionEnd** summarise long sessions (see "Model calls") and add meaning-search vectors.

Every hook form exits 0, so a broken index never stops a session. Errors go to stderr. You can use only the hooks
you want; the first two are the core.

## Copilot CLI sessions

gopola indexes GitHub Copilot CLI sessions (`~/.copilot/session-state`, or `$COPILOT_HOME`) into the same index, so
`search`, `show`, `recent`, `file` and the archive cover them, and each one is marked `copilot` where gopola lists
sessions. A Copilot session in a directory belongs to the same project as a Claude session started there.

`./install.sh` writes a Copilot `sessionStart` hook (`~/.copilot/hooks/gopola.json`, only when `~/.copilot` exists)
that runs `gopola copilot-hook`, so a new Copilot session opens with the same block a Claude session does. `gopola
doctor` says whether it is wired. The hook says nothing in a scripted `copilot -p` run, which should see only its
brief, and always exits 0. Scripted runs are indexed, and `recent` shows them as one line (`+ 6 scripted Copilot runs,
last …`) so a burst of probes never crowds out your own sessions; `recent --all` lists each one. `commands`, `dispatches` and `hooks` still read Claude Code's records only and refuse a
Copilot session. `weight` reads Copilot's `events.jsonl` by session id or path. Details:
[reference](docs/reference.md#copilot-cli). Copilot `weight` stop reasons are explained under
[`weight`](docs/reference.md#weight).

## Codex CLI sessions

gopola indexes OpenAI Codex CLI sessions (the rollouts in `~/.codex/sessions` and `~/.codex/archived_sessions`, or
under `$CODEX_HOME`) into the same index, so `search`, `show`, `recent`, `file` and the archive cover them, and each one
is marked `codex`. Commands and files read come from the record Codex writes for each command it runs, files edited
from its `apply_patch` edits, and the title from the name Codex gives the session. Scripted `codex exec` runs are
indexed and folded into `recent`'s scripted line (`+ 1 scripted Codex run, last …`). `commands`, `dispatches`,
`weight`, `hooks` and `context` refuse a Codex session.

`./install.sh` adds a `SessionStart` hook that runs `gopola codex-hook` to `~/.codex/hooks.json` (only when `~/.codex`
exists). Every other handler in that file is kept. A Codex session you start then opens with the same block a Claude
session does. **Codex runs a new hook only after you approve it once**, when its interactive UI asks you to review it.
Codex keeps a hash of each approved handler, so one `--install` rewrites later (from another bin, say) may ask again.
The hook says nothing in a `codex exec` run and always exits 0. `gopola doctor` says whether it is wired. Details:
[reference](docs/reference.md#codex-cli).

## The session-start block

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

For each session: when it started, its id, branch, turn and commit counts, and Claude Code's own title for it. Then
your first real prompt (`>`), the last commit subjects (`✓`), and the first line of the assistant's last reply
(`<`). A summarised session shows its three summary lines instead.

## Where the facts come from

- **Turns** are your prompts and the assistant's final reply, with the files it edited and the commands and skills
  it ran. Harness noise (system reminders, task notifications) is removed; prompts that were only harness noise are
  not turns.
- **Commits** come from git itself, in the session's launch directory, between its first and last timestamp, however
  the commit message was written. The stored list has missed commits once (0 recorded where the rule, run again,
  found 9; the cause is not known yet), so `authored` and `blame` run the rule again rather than trust it, while
  `recent`, `timeline` and `digest` still read the list.
- **Projects** are decoded from Claude Code's transcript folder names and matched against real directories. A
  session started in a worktree named `<repo>-wt/<task>` is filed under `<repo>`.
- **Commands and subagent reports** are read from the transcripts when you ask, including subagents' transcripts and
  large outputs Claude Code saved to files.

Transcripts are append-only, so indexing resumes from where the last turn began. Claude Code deletes transcripts
after 30 days by default, so gopola keeps a compressed copy of each one in `archive/` next to the index (8.4 GB
became 1.1 GB) and reads the copy once the original is gone ([reference](docs/reference.md#the-archive)).

## Privacy

Nothing leaves your machine except session summaries (see "Model calls"). With the Stop and SessionEnd hooks
installed, a summary is sent on its own once a session reaches 20 turns. To keep everything local, leave out the
two `summarize --auto` hooks; `embed` talks only to a local Ollama.

- Text inside `<private>…</private>` in a prompt or reply is stored as `[private]`, so it is never searchable and
  never sent to a model.
- Secret-shaped strings (AWS keys, GitHub and Slack tokens, `sk-` keys, JWTs, bearer tokens, PEM private keys) are
  replaced with `[redacted:<kind>]` before storage and in every output. The transcripts on disk are not changed.
- `GOPOLA_EXCLUDED_PROJECTS` keeps named projects out of the index entirely.

## Model calls

gopola works with no model at all. Two optional features use one:

- **Summaries (cloud).** `gopola summarize <session>` sends a digest gopola already holds (prompts, closing lines,
  files, commit subjects; capped near 3k tokens) through `claude -p` and stores three lines: `asked:`, `done:`,
  `next:`. The Stop hook calls it only when a session has reached 20 turns and grown by 20 since its last summary,
  so short sessions cost nothing. The default model is Sonnet; pass `--model` for a stronger one.
- **Meaning search (local).** `gopola embed` sends each turn to a local [Ollama](https://ollama.com) (`ollama pull
  embeddinggemma-2:270m` first) and stores the vector in the same database. Unchanged turns are never embedded twice. `search` then ranks by words and meaning
  together; `--words` or `--meaning` picks one. With Ollama down, search says so in one line and uses words.

## Configuration

| Variable | Default | What it sets |
|---|---|---|
| `GOPOLA_DB` | `~/.local/share/gopola/gopola.db` | the index |
| `GOPOLA_PROJECTS` | `~/.claude/projects` | where transcripts are read from |
| `GOPOLA_EXCLUDED_PROJECTS` | none | comma-separated project names or path fragments to skip |
| `GOPOLA_OLLAMA` | `http://localhost:11434` | the Ollama server for meaning search |
| `GOPOLA_EMBED_MODEL` | `embeddinggemma-2:270m` | the embedding model |
| `CLAUDE_PROJECT_DIR` | the launch directory | overrides the project for scripted sessions |

## Cloud sessions

A Claude Code session that ran in the cloud keeps its transcript there. Export it to the session's branch and
import it:

    gopola import origin/transcript/<job>:transcript.jsonl

See [the reference](docs/reference.md#exporting-a-cloud-sessions-transcript) for the export step to put in a cloud brief.

## Coming from claude-mem?

gopola replaced [claude-mem](https://github.com/thedotmack/claude-mem) on the author's machine, and takes a
different approach: claude-mem asks a model to write an observation for each tool call, while gopola reads the
facts straight from the transcripts and git. It hooks the same session events, PostToolUse on edits only where
claude-mem runs on every tool call, and keeps the same `<private>` convention and project exclusion.

- `gopola import-claude-mem` imports your existing observations, searchable with `--kind observation`.
- [docs/parity.md](docs/parity.md) maps each claude-mem behaviour (as of version 13.25.3) to what gopola does
  instead, and what it deliberately leaves out.

## Exit codes

`0` success, `1` the thing you asked about is missing or in a bad state (no report, no match), `2` misuse.

## More

- [docs/reference.md](docs/reference.md): every command in detail, its output and `--json` shape, and how each
  figure is measured.
- [CHANGELOG.md](CHANGELOG.md): what each version added, and the question that led to it.
- `skills/gopola/SKILL.md`: the skill that tells a Claude Code session when and how to search.

## Extending

- **Index and query sessions.** `src/index.rs`, `src/cmds.rs`, and `src/insight.rs` index transcripts and provide
  `recent`, `search`, `show`, `summarize`, `import-claude-mem`, and tool counts. Not built: subagent transcript
  indexes, sync, or a status line. Add transcript rules to `src/index.rs`; add query behavior to `src/cmds.rs`
  or `src/insight.rs`.
- **Measure session context.** `src/weight.rs` reports model-call size and context sources. Not built: cloud log
  summaries, screenshot size reduction, dollar cost, or a per-turn timeline. Add weight measures to
  `src/weight.rs`.
- **Search by meaning.** `src/embed.rs` and `src/cmds.rs` rank sessions by words and local vectors. Not built:
  `ask`, friction detection, or in-process embeddings. Add vector logic to `src/embed.rs` and query behavior to
  `src/cmds.rs`.
- **Ignore whitespace in blame and show line ranges.** `src/authored.rs` does both. Not built: moved-line or
  copied-line detection. Add blame behavior to `src/authored.rs`.
- **Search commands and filter turns.** `src/index.rs` stores command data; `src/cmds.rs` handles
  `search --kind command` and `show --grep`. Not built: edited file paths or subagent command search.
  Add indexed fields to `src/index.rs` and query behavior to `src/cmds.rs`.
- **Separate clones by project name.** `src/index.rs` gives a second checkout a qualified name. Not built:
  rename older session rows. Add project-name rules to `src/index.rs`.
- **Find session commits and lines.** `src/authored.rs` provides `authored` and `blame`. Not built: Edit or Write
  evidence, subagent attribution, stale commit-table repair, or moved-line detection. Add attribution behavior to
  `src/authored.rs`.
- **Import a cloud transcript.** `src/cmds.rs` accepts a file or ref path; `src/index.rs` files the import.
  Not built: the teleport API or automatic branch imports. Add import logic to `src/cmds.rs` and
  `src/index.rs`.
- **Measure `/clear` chains.** `src/weight.rs` reports each transcript part. Not built: chains across folders or
  a chain total. Add chain rules to `src/weight.rs`.
- **Read full Bash calls.** `src/commands.rs` reads command text, time, status, and output, plus subagent calls.
  Not built: output-text search or non-Bash tools. Add command parsing to `src/commands.rs`.
- **Report idle compactions.** `src/weight.rs` reports each compaction and its quiet gap. Not built: a per-turn
  timeline. Add compaction measures to `src/weight.rs`.
- **List agent dispatches.** `src/dispatches.rs` reports Agent and SendMessage calls, results, and hand-backs.
  Not built: subagent calls, Workflow agents, or replies from other sessions. Add dispatch parsing to
  `src/dispatches.rs`.
- **Read Copilot sessions.** `src/copilot.rs` maps Copilot event records to gopola's session events. Not built:
  Copilot subagent sessions or hooks that run during a session. Add Copilot record rules to `src/copilot.rs`.
- **Read Codex sessions.** `src/codex.rs` maps Codex rollout records to gopola's session events. Not built:
  Codex subagent rollouts. Add Codex record rules to `src/codex.rs`.
- **Show Copilot context.** `src/context.rs` reports the system message, instruction updates, and context that hooks
  added. Not built: a Claude Code system prompt; its transcript holds no system prompt. Add context parts to
  `src/context.rs`.
- **List hook runs.** `src/hooks.rs` reports hook runs from a Claude Code session and its subagents. Not built:
  Copilot mid-session hooks or hook runs for Codex sessions. Add hook record rules to `src/hooks.rs`.
- **Read subagent hand-backs.** `src/handback.rs` reads the last report from the parent session transcript. Not built:
  the agent's brief, full tool history, changed files, last words, stop point, or nested dispatches. Add hand-back
  record shapes to `src/handback.rs`.
- **Keep transcript copies.** `src/archive.rs` copies quiet transcript files and reads the copies when the originals
  are gone. Not built: copies of Copilot's `workspace.yaml` file. Add archive paths and copy rules to `src/archive.rs`.
- **Parse Claude Code transcript records.** `src/transcript.rs` reads prompt, reply, and title records. It also removes
  private text and redacts secrets. Not built: a `doctor` warning for a new Claude Code record shape. Add record
  parsing rules to `src/transcript.rs`.
- **Measure Copilot sessions.** `src/weight.rs` reports model calls, context, credits, session segments, and tool
  calls. Not built: prompt text, system text, internal thought text, tool arguments, errors, or file content. Add Copilot
  measures to `src/weight.rs`.
- **Add session-start hook commands.** `src/cmds.rs` handles Copilot and Codex hooks that add recent sessions as
  context. Not built: recent-session context in scripted `copilot -p` or `codex exec` runs. Add hook behavior to
  `src/cmds.rs`.

Dispatch each verb in `src/main.rs`. Put its CLI tests in `tests/cli.rs` and unit tests beside its source.
Update `README.md`, `docs/reference.md`, and `skills/gopola/SKILL.md`.

## Licence

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
