# Changelog

What changed in each version, and why. Every feature here was built after a real question went unanswered, so each
entry names the question.

## 0.20.0 — 2026-11-14

**New: `handback --since <time>`.** The question: what did a fixer report across a whole fix round? `handback` prints
the last report only, so a fixer resumed with SendMessage lost its earlier reports. In tessera session 465b838a, a
round's re-review got the fixer's follow-up and not the fix wave's report. `--since` prints every report at or after a
UTC time, oldest first, each under `--- hand-back <time> ---`; `--json` gives `{agent, handbacks: [...]}`. ledger
0.25.0's `rereview-prompt` calls it.

## 0.19.1 — 2026-11-13

**Fixes from 0.19.0's review.** `weight --agents`, `commands` and `hooks` read a Workflow's agents
(`subagents/workflows/wf_<run>/agent-<id>.jsonl`) beside the session's other subagents; before, a session that ran a
Workflow showed none of their calls. `weight --agents` on a Copilot session says on stderr that it reads Claude Code
subagents only, where it was dropped without a word. `skills` logs as hits only the names with a call or a `/<name>`
turn, so a query whose names were never called is logged as finding nothing. `codex-hook --install` names the file it
could not write (a `hooks.json` link into a missing folder printed only "No such file or directory").

## 0.19.0 — 2026-11-13

**New: `weight --agents`.** The question: how big is each subagent's first call, by agent type? `weight` on a
subagent's transcript printed "no model calls with usage recorded", so the sizes were read with a hand `jq`, three
times in one session. `--agents` adds one row per subagent: calls, the first call exactly, median and maximum context,
output, and the agent's type and description from its `.meta.json`. `weight` also measures a subagent transcript
given by its path.

**New: `commands --no-output`.** The question: can a session's commands be kept when their output holds a secret? Both
views always printed the output, so the commands were read with `jq` over the transcript. `--no-output` prints none, in
both views.

**New: `skills [<name>...]`.** The question: how many times was a skill really called? A hand `search | rg -c` stopped
at 10 rows without `-n 1000`, missed calls whose search snippet showed another match, and counted a command that
only quoted the words. `skills` counts the Skill calls in the index by name, in all projects: calls (a turn counts
once), sessions, `/<name>` turns apart, first and last date, and projects.

## 0.18.3 — 2026-11-13

**Fixes from review.** `doctor` reads Codex's `config.toml` as TOML, so a `SessionStart` hook written as an inline
table counts as wired, and a lone `[[hooks.SessionStart.hooks]]` (a table, not the list of matcher groups Codex reads)
does not. `codex-hook --install` writes through a `hooks.json` link whose target does not exist yet, keeping the link.
A Codex read path that is relative and has no `cwd` is dropped (it named no file), and only a `file://` cwd is
%-decoded. New tests: the `sessions/` copy of a rollout wins over its `archived_sessions/` copy, and the title query is
the one that reports a bad row. New dependency: `toml` (parse and serde only).

## 0.18.2 — 2026-11-13

**New: release binaries are built in the public repo.** A binary built on a Mac named a private commit, so nobody could
check it against the public source. `release.yml` now builds macOS arm64 and Linux musl binaries from a `v*` tag,
stamps `--version` with the public commit and its date, attests build provenance, and publishes the binaries with
`SHA256SUMS`. The README shows how to verify them.

## 0.18.1 — 2026-11-13

**Fixes from review.** Codex ignores declined file changes, reports compressed rollouts that changed after indexing,
checks the actual SessionStart hook, preserves symlinked hook files, and cleans failed temporary writes. Index row
conversion errors are surfaced. Copilot weight no longer counts numbered Markdown links as zshguard rules and
recognizes `git-grep-escape`.

## 0.18.0 — 2026-11-12

**New: `weight` for Copilot CLI sessions.** Copilot write seats need to be tuned from real runs, not test runs. `weight`
now reads a Copilot session's `events.jsonl` by indexed session id or direct path, and prints one record per session
with one segment per start and resume. It reports model calls, credits, token usage, Git state and commits, wall time,
tool counts, and refusal sources without exposing prompt, reasoning, file, tool-argument or error content. A resume
keeps the original start's credit cap. `--json` includes the full call array; `-n N` opts into the call table in text.
Claude Code's `weight` output is unchanged.

## 0.17.0 — 2026-11-12

**New: gopola reads OpenAI Codex CLI sessions.** The question: after a session in Codex, can a Claude or Copilot
session in the same repo see what it did, and can a Codex session see what the others did? Neither could; to gopola, a
Codex session did not exist. `index` now reads each Codex rollout (`~/.codex/sessions/…/rollout-…-<id>.jsonl`, or
`$CODEX_HOME`; CLI 0.167.0 seen), turning prompts, replies, the commands Codex records for each one it runs, the files
those commands read and the files its `apply_patch` edits into turns. These go in the same project as Claude's and
Copilot's sessions from that directory. `search`, `show`, `recent`, `file`, `timeline` and `digest` cover them, and each
one is marked `codex`. The title is the name Codex gives the session. A resumed session grows in place, a fork is its own
session, and a subagent's rollout gets a row with no turns.

**New: Codex sessions survive Codex's housekeeping.** A rollout Codex compresses (`.jsonl.zst`) or moves under
`archived_sessions/` keeps its one row and its turns. gopola's archive keeps its own copy in `archive-codex/`, which
`index` reads once Codex has deleted the rollout.

**New: `codex exec` runs are folded, per harness.** They are indexed, but `recent` counts them on the scripted line:
`+ 2 scripted Copilot runs, 1 scripted Codex run, last …`. `commands`, `dispatches`, `weight` and `hooks` refuse a
Codex session (they read Claude Code's records only), as `context` does (it reads Copilot's), rather than printing
nothing.

**New: `gopola codex-hook`, Codex's `SessionStart` hook.** A Codex session you start opens with the recent block, given
as `hookSpecificOutput.additionalContext` at `startup` and `resume`, and not after `compact` or `clear`. In a
`codex exec` run it says nothing: a probe on 2026-11-12 found an exec run's hook a child of `codex exec say hi`, so the
hook checks its `codex` ancestor's subcommand, after any root options (`codex -c k=v exec …`).
`gopola codex-hook --install <bin>` adds the hook to Codex's `hooks.json` and keeps every other handler there.
`install.sh` runs it when `~/.codex` exists. Codex asks you once, in its interactive UI, to approve a new hook.

**New: `doctor` reports Codex.** When the Codex folder exists: whether the hook is wired (in `hooks.json` or
`config.toml`), record types gopola does not know, lines it cannot read, the sessions by CLI version, and rollouts that
grew since the last index. The unknown types are kept per harness in `meta` (`codex_unknown_types` beside
`copilot_unknown_types`).

## 0.16.3 — 2026-11-11

**Fixed: a renamed Copilot field went unnoticed.** The question: if Copilot renames `content` in `user.message`, would
anything say so? Every such line was skipped by `index` without a word, and `doctor` named only unknown record types.
`doctor` now counts, per session, the complete lines gopola cannot read ("Copilot lines gopola cannot read: 2 in
c1000000"); a last line still being written is not counted.

**Fixed: a Copilot hook with a space or a quote in its bin path did not run.** `install.sh` wrote the bin path into the
hook's command unquoted. The bin folder is now shell-quoted (`'<bin>'/gopola copilot-hook`); an existing hook file is
rewritten once by the next install.

**Fixed: four smaller Copilot reader faults** from the reader's final review. Only prompts, replies and file edits need a timestamp: a record type
gopola ignores is ignored without one (it counted as unreadable), and a `session.start` or a title without one is now
read. A database error while checking whether a session is a
Copilot one is an error, not "not Copilot". `recent` no longer drops the only listed session to make room for the
fold line. `doctor` no longer calls a Copilot folder stale forever when the index skips it (its first line is not
`session.start`).

**Changed: no private project names in the code, tests or CI.** Fixtures and comments use neutral names, ahead of a
public release. `docs/reference.md` describes the database's tables (it was only in an internal document) and what an
empty `GOPOLA_COPILOT_PARENT_ARGS` means.

## 0.16.2 — 2026-11-11

**Fixed: a Copilot session counted itself in its own opening list.** The question: why did the owner's session
55055b3e open with "2 scripted Copilot runs" when only one had run? The hook fires at the first prompt, about 3 s
before Copilot's title call, so the session was indexed as scripted and folded into its own line. `recent` takes
`--exclude <session-id>`, which leaves one session out of the total, the list and the scripted line, and
`copilot-hook` passes its `sessionId`.

**Fixed: `index --full` filed a moved checkout's Copilot sessions under the old path.** The question: after `gopola
moved`, why did a rebuild put Copilot sessions back under the old project? A rebuild deletes the rows, and the launch
dir read again from `session.start` did not go through the moves, as Claude's folder names do. Now it does: the old
path, a path under it, or a `-wt` worktree beside it becomes the new one.

**Fixed: `doctor` kept naming record types gopola has since learned.** The question: is the "record types gopola does
not know" line still true? The row that holds them is never cleared, so a type added to the reader stayed listed.
`doctor` now leaves out every type the reader knows.

**Fixed: `install.sh` could leave a wrong Copilot hook file.** The question: does the hook file run the gopola just
installed? A file that named another bin was left alone, a relative bin was written relative, and a `"` or `\` in
the path broke the JSON. The bin is made absolute, the path is escaped, and the file is rewritten whenever it differs
from what the run would write ("wrote", or "left … alone, unchanged").

## 0.16.1 — 2026-11-11

**Added: `gopola context <session> [--grep <text>]`.** The question: did the owner's rules reach a Copilot session's
prompt? Nothing showed what a Copilot session's model was told without a hand-written script over `events.jsonl`. Now
the command prints a Copilot session's system prompt, instruction updates and hook context in session order, and
`--grep` counts a text per part. On the owner's session 55055b3e it found gopola's block in the `sessionStart` context
(`1 match` for `# gopola:`) and `0 matches` for `docs/board.md` anywhere. A Claude Code session is refused: its
transcript holds no system prompt.

## 0.16.0 — 2026-11-11

**Added: GitHub Copilot CLI sessions.** The question: after working in Copilot CLI for a while and switching back to
Claude Code (or the other way round), what did the other session do, and why does a new Copilot session start
without the list a Claude session gets? gopola knew only Claude Code's transcripts, and Copilot's `copilot -p` runs
(reviewers, trials, probes) were most of the Copilot sessions on the machine.

- `index` reads `~/.copilot/session-state/<id>/events.jsonl` (`COPILOT_HOME`) into the same tables, so `search`,
  `show`, `recent`, `file`, `timeline`, `digest` and the archive cover Copilot sessions. A Copilot session is filed
  under the project of its launch directory, the same rule as a Claude session. Copilot sessions keep their CLI
  version, and `recent`, `show`, `search` and `file` mark them `copilot` (`--json`: `harness`, `scripted`).
- **Scripted runs are indexed, and `recent` folds them.** A session on CLI 1.0.105 or later with no title call is a
  `copilot -p` run. `recent` and the session-start block show them as one line per project (`+ 6 scripted Copilot
  runs, last …`); `recent --all` lists each one. A project with only scripted runs still gets the line.
- **`gopola copilot-hook` and a Copilot `sessionStart` hook.** `install.sh` writes `~/.copilot/hooks/gopola.json`
  when `~/.copilot` exists, so a new Copilot session opens with the recent block. The hook finds a scripted run from
  the `copilot` parent process's arguments (`-p`, `--prompt`) and then says nothing, and it always exits 0.
  Checked on CLI 1.0.107: the hook ran in a `copilot -p` run and its `additionalContext` reached the model.
- **The archive keeps Copilot sessions** in `archive-copilot/` (`events.jsonl` only).
- **`doctor` has three Copilot lines:** the hook is wired, record types gopola does not know (a Copilot update that
  changes the format shows here instead of silently shrinking the index), and the CLI versions seen.
- **`commands`, `dispatches`, `weight` and `hooks` refuse a Copilot session** (`weight` also refuses a path under the
  Copilot folder) with a message, instead of printing nothing. Reading Copilot's tool records for them comes later.

## 0.15.3 — 2026-11-11

**Changed: a closed pipe ends gopola through a silent panic, not SIGPIPE.** 0.15.2 restored SIGPIPE's default, so
the process ended at once and skipped its cleanup. In other tools that cleanup restores files or releases a lock.
Now gopola ignores SIGPIPE, as Rust does by default, and a panic hook prints nothing for the "failed printing to
stdout" panic only. The panic unwinds, so every cleanup runs, and stderr stays empty. A check program with a cleanup
on drop, run into a closed pipe, wrote 0 bytes to stderr and ran its cleanup.

## 0.15.2 — 2026-11-10

**Fixed: a closed pipe made gopola panic.** `gopola hooks 49973e08 --json | head -c 100` printed "failed printing to
stdout: Broken pipe" and a panic, and every command could do the same. Now gopola stops without a message when its
reader closes, as other Unix commands do.

**Fixed: `index` follows a renamed transcript folder.** When you move a checkout, you can also rename Claude Code's
folder for it, so that `claude --resume` finds the sessions from the new path. `moved` refiled the sessions, but each
row kept the old transcript path. Readers then needed the archive copy, and failed until the archive had one. Now
`index` records the new file when the old one is gone. Found on 2026-11-10 when a checkout moved: one row was fixed
by hand. On a copy of the real database with that row put back, `index` changed that row only.

## 0.15.1 — 2026-11-10

**Fixed `hooks`: a block with a JSON reason has no command.** A command hook that exits 2 leaves
`[<command>]: <stderr>`. A hook that blocks with a JSON reason leaves only the reason, and that reason can start with a
bracket: `[Fact-Forcing Gate]\n\n…` or `[Fact-Forcing Gate] (denial #4 …`. 0.15.0 read the bracket as the command in
the first form and failed to read the second form. Now only `]: ` on the first line ends a command. Found on the
first run of the installed 0.15.0 against this session: 4 of its 8 gate blocks matched `--grep Fact-Forcing`.

## 0.15.0 — 2026-11-10

**Added `hooks`:** each hook run of a session and its subagents, with its time, hook, status, duration and
command. The stderr, the reason for a block or the message is under the row; `--head N` adds the output. The
question: "which hooks ran in that session, and did any fail or block?". On 2026-11-09 (session 49a0f556) the checks
after the rename got the answer twice with `rg -o … | sort | uniq -c` over the raw transcript.

gopola reads six attachment types and two block records. A block has no attachment: only the tool result
(`PreToolUse:Bash hook error: [<command>]: <reason>`) or a `Stop hook feedback` record holds it. Before the release,
gopola ran on three real sessions. In each, the rows were the hook attachments plus the blocks in user records:
1,078 + 20, 15,823 + 41 and 401 + 8. The copies of a request (`api-request-blob`) repeat old blocks, and gopola does
not count them.

Not built: a search across sessions, a stored table, and `stop_hook_summary` (it repeats the rows of the Stop hooks).

## 0.14.0 — 2026-11-09

**Added the archive:** gopola keeps a zstd copy of every transcript, subagent transcript and saved output in
`archive/` next to the database, once a file has gone an hour without changes and again whenever it changes.
`index` does it at most every 10 minutes; `gopola archive` does it now. `commands`, `weight`, `dispatches`,
`handback` and `index` read the copy once Claude Code has deleted the original. On this machine the first pass
copied 4,387 files, 8.4 GB into 1.1 GB, in 39 s.

Why: Claude Code deletes transcripts after `cleanupPeriodDays` (30 by default). On 2026-11-09 the index went back
to 2026-08-20 but the oldest transcript left was from 2026-10-10, so `commands`, `weight` and `dispatches` knew
nothing before 10 October.

**Changed:** `index --full` rebuilds from the archive's copies and keeps every session it cannot read again, where
before one rebuild dropped every session whose transcript was gone.

## 0.13.0 — 2026-11-09

**Added a PostToolUse hook on edits:** `gopola index --quiet`, in the background, after each `Edit`, `Write`,
`MultiEdit` or `NotebookEdit`, so another session's Read hook learns of an edit when it happens, not at this
session's next prompt.

Why: the Read hook only knew of edits up to the editing session's last prompt. Edits only, because indexing after
every tool call was measured at about 5.5% of one core at a heavy user's peak.

**Changed:** the database waits up to 5 s for a lock, so hooks indexing at once in several sessions queue instead of
failing with "database is locked". `doctor` checks each hook under its own event (before, a hook string anywhere in
the settings passed for any event), checks PostToolUse, and reads the settings and the skill from
`CLAUDE_CONFIG_DIR` when it is set, as `install.sh` does.

## 0.12.0 — 2026-11-09

**Renamed recall to gopola** (Setswana: "to remember"). The crate, the command, the skill and every message say
gopola. `RECALL_*` settings are now `GOPOLA_*`, and the default database is `~/.local/share/gopola/gopola.db`; the
old names are not read, so move the database by hand.

Why: the `recall` crate on crates.io belongs to another Claude Code tool, which also installs a `recall` command.

**Added `moved`:** `gopola moved <old path> <new path>` keeps a moved checkout's history under its new path (see
[Projects](docs/reference.md#projects)).

Why: moving the checkout for the rename split its history in two, and `authored` and `blame` ran git in a folder
that was gone.

## 0.11.0 — 2026-11-06

**Added `dispatches`:** each `Agent` and `SendMessage` call of a session, with the agent it reached, whether a
hand-back came, and the whole brief.

Why: to resume a set of stopped builder agents, two sessions rebuilt a `jq` over the transcript to get each brief,
then ran `handback` once per agent. Replayed over 218 sessions (1,786 `Agent` and 945 `SendMessage` calls) with no
failures.

Not built: subagents' own dispatches (none seen), Workflow agents (they are not `Agent` calls), and replies to a
message sent to another session (those are in the other session's transcript).

## 0.10.0 — 2026-11-05

**Added `commands`:** a session's or a search's Bash calls in full, with time, status and output, subagents
included. Reads transcripts at query time; the index and its schema do not change.

Why: "how did that session start Docker?" took four commands and two hand-written `jq` programs.

**`weight`: idle compactions.** Each compaction now shows the quiet before it and its duration, marked `idle` at 5
minutes or more. Why: a compaction at 367k was read as a size limit; the session had been idle for 50 minutes.

**`weight`: `/clear` chains.** A transcript that is part of a chain prints one row per part. Why: `weight` showed
only the transcript before the first `/clear` of a background job with 11 parts.

Not built: matching on output text; tools other than Bash; a chain split across two folders; a sum over a chain's
parts (a forked job's copied records would count twice).

## 0.9.0 — 2026-10-26

**Added `import`:** a cloud session's transcript from a file or `<ref>:<path>`, filed under the current repository,
and the export step for a cloud session's brief.

Why: a cloud session never appeared in `search`, `weight` or the session-start block, so its retro could not start.
Reading events through the cloud session API was rejected: the endpoint is undocumented and needs the claude.ai
credentials. `claude --teleport` covers sessions that never exported.

Not built: a call to the events API; an `import --teleport` (a model call and a branch checkout); automatic
fetching of `transcript/*` branches; a list of cloud sessions.

## 0.8.0 — 2026-10-19

**Added `tools --programs`:** counts the programs that Bash commands ran, not just Claude Code's own tools. Why: to
find out which command-line tools sessions actually use.

## 0.7.0 — 2026-10-14

**`blame` ignores whitespace by default** and gains `--ranges`. Why: a re-indent took lines away from the session
that wrote them, and the counts had to be corrected by hand several times in one session.

**Two clones with one name are two projects.** The clone with the shorter path keeps the plain name; the other is
`<parent>/<name>`. Run `gopola index --full` to rename sessions already indexed.

Not built: renaming old rows without a full rebuild; a rule for three clones at the same depth; a `--project` alias
for the old shared name.

## 0.6.0 — 2026-10-14

**Added `authored` and `blame`:** which commits on a ref came from Claude Code sessions, and their surviving lines.

Why: to simplify the code sessions wrote, it had to be separated from hand-written code, which git cannot do. Done
by hand twice in a few minutes, the first attempt matched nothing (BSD `sed` has no `\s`) and the second kept
teammates' commits.

Not built, on purpose: evidence from Edit and Write (wrong for edits made through Bash); subagent transcripts;
`blame -M` and `-C`.

## 0.5.0 — 2026-10-14

**Added `handback`:** prints a subagent's last report from the parent session's transcript. Why: 5 of 11 agents in
one plan returned their report as a final message and wrote no file, so the controller retyped each one.

Not built: every report of a resumed agent (the last one is printed); a search over reports (`search` already
indexes replies).

## 0.4.0 — 2026-10-13

**Meaning search through local Ollama:** `embed`, and `search` that ranks by words and meaning together. `doctor`
checks vector coverage and Ollama.

Why: a probe of 24 questions. On questions in the sessions' own words, words alone scored MRR 0.85 and words plus
meaning 0.92. On the same questions reworded, words alone scored 0.12 (3 of 12 in the top 5) and words plus meaning
0.52 (10 of 12). `embeddinggemma-2:270m` was chosen over `qwen3-embedding:0.6b` (slightly better, but 2.4 GB against
346 MB, and half the speed).

Fusing meaning with the any-word fallback made results worse (it ranks on words like "on" and "my"), so meaning
replaces that fallback instead. Ollama's answers are pinned as test fixtures and replayed against a fake server.

Not built: a vector index (a brute-force scan of ~1,500 vectors takes under a millisecond); chunking long turns;
embedding imported claude-mem observations.

## 0.3.0 — 2026-10-13

**Search what a session ran:** `--kind command` and `show --grep`. Why: "which session ran that login command?"
found nothing, because the answer was in a command, not in words.

**VS Code prompts.** The extension writes prompts as lists of text blocks, which were read as tool results. 12
sessions and 450 turns came back. The schema change re-reads every transcript once.

## Before 0.3.0 — 2026-09-22 to 2026-10-10

- **2026-09-22, first release:** a deterministic replacement for claude-mem's session-start block. The indexer,
  `recent`, `search`, `show`, `timeline`, `file` and its Read hook, `tools`, `digest`, `doctor`, on-demand and
  automatic summaries, session titles, `<private>` stripping, the skill, and a parity pass against claude-mem's
  source (`docs/parity.md`).
- **2026-09-23:** `version` names the commit the binary was built from.
- **2026-10-07:** `install.sh` installs a copy, not a symlink into the checkout.
- **2026-10-10:** `weight`: context per model call, compactions, and what filled a session. Hook output counts only
  what reaches the model; counting raw stdout made hooks look like 15 MB of context.

## Fixed along the way

Underscore and dot directory names; SDK sessions; heredoc commits; task-notification prompts counted as turns; a
`\b` that became a backspace when a script wrote regexes into Rust source; two-word searches whose words never
appear together (now falls back to any word and says so); full-text deletes through a rowid map (a full rebuild had
grown from 8 s to 53 s); the schema version written once and never read; the turn span of a teleported copy.
