---
name: gopola
description: Use when the user asks about PREVIOUS sessions — "did we already do this?", "how did we solve X last time?", "what happened last week?", "who touched this file?", "why was that session slow / what filled its context?" — or when a session-start block tagged a session [long, unsummarised]. gopola reads Claude Code's own transcripts and GitHub Copilot CLI's sessions; no cloud model call unless you run summarize.
---

# gopola

This skill gives you past sessions, from the transcripts, as commands. Every command takes `--json`. The default project is the
basename of the git toplevel. `--project all` makes the search wider.

    gopola search <words> [--kind prompt|reply|command|commit|summary|title|observation] [--since D] [-n 10] [--words | --meaning]
    gopola embed                                     # give new turns their vectors (a SessionEnd hook does this)
    gopola show <session-prefix> [--turn N]          # the turns; --turn N is the full prompt and reply
    gopola show <session-prefix> --grep <word>       # only the turns that mention it, with the commands that did
    gopola timeline <session[:turn] | sha>           # what happened around a hit
    gopola file <path>                               # which past sessions edited it, and why
    gopola digest --since 2026-09-15 [--project all] # per project per day: sessions, turns, commits
    gopola recent [-n 6]                             # the session-start block, on demand
    gopola weight <session-prefix> [--agents]        # context per model call, compactions, and what filled it; --agents: each subagent
    gopola skills [<name>...]                        # how many times each skill was really called, all projects
    gopola hooks <session-prefix> [--head N]         # each hook run: hook, status (ok, exit N, blocked, timed out), duration, command, stderr
    gopola context <session-prefix> [--grep <text>]  # a Copilot session's system prompt, instruction updates and hook context; --grep counts a text per part
    gopola handback <agent-id-prefix>                # the last report a subagent returned to its session, as received (exit 1: none yet)
    gopola summarize <session-prefix>                # ONE Sonnet call: asked / done / next; only by hand or the hooks
    gopola authored <repo> --ref origin/dev --path lib   # the commits on the ref that came from Claude Code sessions
    gopola blame <file|dir> --ref origin/dev [--summary]  # each surviving line, marked with the session that wrote it
    gopola import <file | ref:path>                  # a cloud session's transcript, exported to its branch; run it in the repo

**Copilot CLI sessions are in the index too**, marked `copilot` in `recent`, `show`, `search` and `file`, in the same
project as a Claude session started in that directory. `recent` folds scripted `copilot -p` runs into one line
(`+ 6 scripted Copilot runs, last …`); `recent --all` lists each, and `search`, `show` and `file` find them anyway.
`commands`, `dispatches` and `hooks` read Claude Code sessions only and refuse a Copilot one. `weight` reads
Copilot's `events.jsonl` by session id or path; see [the reference](../../docs/reference.md#weight).

**A cloud session (claude.ai/code) is not in the index** until someone imports its transcript. Its brief's last step
exports the transcript to a `transcript/<job>` branch (gopola's README, "Cloud sessions"). On the Mac, run
`git fetch origin transcript/<job> && gopola import origin/transcript/<job>:transcript.jsonl` in that repo. Then
`show`, `search` and `weight` work on it.

## How to use it

1. Use `search` first. It gives one line for each hit: `date kind ref snippet`. It ranks by words and meaning together, so a
   question in your own words finds a session that used other words. Use `--words` for an exact term (a sha, a flag,
   a ticket id). Use `--meaning` for meaning only. A `# meaning search off:` line means that Ollama is down or the model is
   missing. The results are then words only, and `gopola doctor` says why. With `--json`, `via` says which ranking found a hit. Kind `turn` means that only meaning found the hit (the whole turn, not one
   of its prompt, reply or command rows). `# no result has every word; nearest by meaning` means that no row had all your words.
   `--kind` or `--words` turns meaning off. Without meaning, two-term searches fall back to any-term and say so.
2. Narrow the result with `--kind` or `--since`. Then use `show <id> --turn N` or `timeline <id:N>` to read the text around the hit.
   **For "which session ran X / why did it need X"**, use `--kind command` (the Bash commands and skills of each turn), then
   `show <id> --grep X`. The words of a session often never name the tool that it ran. Never use `jq` over transcripts
   for this: these two commands replaced it (2026-10-13). Use `--project all` when the session can be in another repo.
3. Quote what you found with its session id and turn, so the user can open it.

**For "which code on this branch did Claude sessions write"**, use `authored` and `blame`. Never build it from
`digest`, `sed` and `git log`: on 2026-10-14 that pipeline matched 0 commits on its first attempt. The tool keeps only
commits by the `user.email` of the repository, and says how each commit reached the ref (`sha`, `patch-id`,
`subject-only`). A commit that the owner made by hand while a session was open also counts.

Do not use a model to summarise a session when its `recent` line, commits and closing reply already answer the
question. Use `summarize` for a long session when those lines do not show the thread.

The tool never stores, searches or sends to a model the `<private>…</private>` spans in prompts and replies.
