# Parity with claude-mem 13.25.3

This document maps every runtime behaviour of claude-mem to what gopola does. The map comes from a source audit of the repository of claude-mem on 2026-09-22
(`scratchpad/claude-mem-audit.md`; the citations are `path:line` in that clone). The status has these values: **same** (gopola does it), **better** (gopola does it with less cost or more truth),
**built today** (someone added it in this parity pass), **needs a model** (claude-mem does it with an Opus/Haiku
call for each event; gopola does not, by the rule of the owner), **not built** (deliberate, with the reason).

## Lifecycle hooks

| claude-mem hook | What it did (audit §1) | gopola | Status |
|---|---|---|---|
| SessionStart `startup\|resume\|clear\|compact` | inject a context block via `hookSpecificOutput.additionalContext`, ≤10,000 chars, welcome hint when the project has no observations | `gopola index --quiet && gopola recent`: 30-line block (measured 28 lines / ~500 tokens on a real project), nothing for a new project | better |
| UserPromptSubmit | store the prompt, dedupe within 10 s, strip `<private>`, skip `<task-notification>`-only prompts, start the observer; `SEMANTIC_INJECT` off by default | `gopola index --quiet` keeps the index current mid-session (25 ms unchanged); prompts come from the transcript with the same stripping and skips | same |
| PreToolUse `Read` | inject past observations about the file: ≥1,500-byte files only, mtime-vs-observation gate, once per session per file, 7-day TTL | `gopola file --hook`: ≤3 lines, only when a *different* session edited the file, once per session and file | same (gates differ: gopola keys on edits, not size or mtime) |
| PostToolUse `*` | store raw tool I/O in `tool_uses`, queue for the observer (every Read/Bash/Grep/Edit, nothing skipped for size) | `gopola index --quiet`, async, on `Edit\|Write\|MultiEdit\|NotebookEdit` only (2026-11-09): the transcript already holds every tool call, so nothing is stored; the index catches up so another session's Read hook sees the edit; `gopola tools` counts calls | better (no per-call model call, no second copy of tool output, no process for reads or commands) |
| Stop | summarise the last assistant turn into request/investigated/learned/completed/next, **every** assistant turn | `gopola summarize --auto`: one Sonnet call when a session reaches 20 turns, then every 20 | better (one call per 20 turns vs one per turn) |
| SessionEnd | request a Telegram wrap-up | `gopola summarize --auto --final`: one more summary if a long session grew | built today |
| Setup | `bun install` on version change | `install.sh` | same |
| fail-loud counter (exit 2 after 3 transport failures) | | hooks always exit 0; `gopola doctor` reports a stale index instead | not built (a blocking hook is the failure mode the owner rejected) |

## The session-start block (audit §2)

| claude-mem | gopola |
|---|---|
| Newest 50 observation titles + 10 session summaries, day-grouped, legend, "Stats: n obs, t read, pct savings" (savings = observer-model tokens, chars/4) | Last 6 sessions: date, id, branch, turns, commits, Claude Code's own `ai-title`, first prompt, last commit subjects, closing reply line; `[long, unsummarised]` tag; one footer line naming the verbs |
| 10,000-char budget trimmed in a fixed order | 30-line cap, sessions dropped from the tail |
| Last summary fields when newer than the newest observation | A summary's three lines replace the prompt/reply lines once it exists |
| "Previously: last assistant message" (off by default, reads the observer's own session file, never a Claude Code transcript) | the `<` line, from the real transcript, on by default |
| Welcome hint until the first observation | nothing printed for a project with no sessions |

## Observation extraction (audit §3): needs a model

For each tool call, claude-mem makes: type, title, subtitle, facts, narrative, and concepts; a noise judgement; the condensation of oversized fields;
and summaries for each turn. gopola keeps the verbatim prompt, the closing reply, the files that it edited
and read, the tools that it used, and the commits. It offers `summarize` on demand and at the two thresholds above.
gopola imported the 20,363 observations that claude-mem produced before its removal, and they are searchable
(`gopola import-claude-mem`).

## MCP tools and skills (audit §4–5)

| claude-mem | gopola | Status |
|---|---|---|
| `search` (FTS5 or Chroma; type/date/project filters; `orderBy date_desc|date_asc`; offset) | `gopola search --kind --since --until --order rank\|date -n`, any-term fallback; words and meaning fused, `--words`/`--meaning` | better: words and meaning (local Ollama), no Chroma process |
| `timeline` (anchor id or query; default depth 10 despite the description saying 3) | `gopola timeline <session:turn|sha>` and `--query <words>`, `--before/--after` default 3 | built today (`--query`) |
| `get_observations` | `gopola show <id> --turn N`; imported observations via `search --kind observation` | same |
| `get_tool_uses` (raw I/O, never invoked in 1,175 sessions) | `gopola tools` counts only | not built (raw output is on disk in the transcript) |
| `session_start_context` | `gopola recent` | same |
| `important_workflow` | the `gopola` skill (`skills/gopola/SKILL.md`, installed to `~/.claude/skills/gopola`) | same |
| `smart_search/outline/unfold` (tree-sitter code views) | out of scope: not memory (graphify covers code structure) | not built |
| corpus tools / knowledge-agent (Agent SDK sessions) | needs a model | not built |
| mem-search skill | `gopola` skill | same |
| learn-codebase (Read every file so the observer records it) | needs a model | not built |
| standup, timeline-report, weekly-digests (narratives) | `gopola digest --since --until --project all` | better (same facts, no model) |

## Storage and identity (audit §6, §1)

| claude-mem | gopola | Status |
|---|---|---|
| Project = `CLAUDE_PROJECT_DIR` else git toplevel basename; worktrees query `parent` and `parent/worktree` | `CLAUDE_PROJECT_DIR` honoured; a session under `<repo>-wt/<task>` is filed under `<repo>` with the task as `worktree` | built today |
| `<private>` and five harness tags stripped from prompt, tool I/O and last message; all-private prompt suppresses everything for that prompt | same tag list stripped from prompts and replies; all-private and task-notification prompts are not turns | built today |
| `files_read` / `files_modified` from tool inputs (model's list discarded); Bash-touched files never attributed | `turn_files` with kind `edit`/`read`; `gopola file --reads` | built today (reads); Bash gap shared, listed under Extending |
| Excluded projects glob | `GOPOLA_EXCLUDED_PROJECTS` names or path fragments | built today |
| No secret redaction anywhere on the observation path; raw tool I/O stored verbatim | AWS keys, GitHub/Slack tokens, `sk-` keys, JWTs, bearer tokens and PEM private keys replaced by `[redacted:<kind>]` before storage | better |
| No retention; no backfill from Claude Code transcripts | the transcripts are the source; nothing to retain separately | better |
| Content-hash dedupe of observations | turns are keyed by session and number; incremental indexing rewrites the last turn | n/a |
| Worktree adoption (`merged_into_project`) | worktree mapping above | same |
| Chroma vector index (350 MB Python process) | vectors in gopola.db through local Ollama | replaced: vectors in gopola.db through local Ollama (0.4.0) |
| Cloud sync, Telegram, Grok bot, CCS align, folder CLAUDE.md rewrite, statusline, web viewer, Codex/Cursor/Windsurf adapters, 30 language modes | none | not built (out of scope or declined) |

## Privacy (audit §8)

The tag stripping is the same, and project exclusion is the same. gopola adds secret redaction and never stores tool output.
Image payloads never enter gopola (it reads only string prompts and text blocks).

## What claude-mem had that gopola does not, and why

- Narratives and types for each tool call: each needs one model call. The rule of the owner is to use a model only when the
  situation demands it. The verbatim record plus the summaries at thresholds replaces them.
- Semantic search: built in 0.4.0. Ollama is the embedding process.
- Raw tool output store: nobody read it in 1,175 sessions, and the transcript keeps the output anyway.
- A viewer, statusline, cloud sync, and chat-bot fan-out: declined.

Measured cost of the difference on 2026-09-22: the block of claude-mem was 65 lines plus 15 tool names and 20
skill names, injected at every message. The block of gopola is 28 lines one time at session start, and one skill.
