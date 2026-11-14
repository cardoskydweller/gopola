//! gopola — what happened in past Claude Code sessions, from the transcripts; local embeddings for meaning search, one cloud model call (summarize).

mod archive;
mod authored;
mod cmds;
mod codex;
mod commands;
mod context;
mod copilot;
mod db;
mod dispatches;
mod embed;
mod handback;
mod hooks;
mod index;
mod insight;
mod transcript;
mod weight;

use cmds::Opts;

const USAGE: &str = "\
gopola — past Claude Code sessions, from the transcripts; meaning search through local Ollama

  gopola index [--full] [--quiet]
  gopola archive                          # copy transcripts quiet for an hour into gopola's archive (index does it too)
  gopola recent [--project P|all] [-n 6] [--commits 2] [--lines 30] [--all] [--exclude <session-id>]   # scripted Copilot runs are one folded line; --all lists them; --exclude leaves one session out
  gopola search <query> [--project P|all] [--since D] [--until D] [--kind prompt|reply|command|commit|summary|title|observation] [--order rank|date] [-n 10] [--words | --meaning]
  gopola embed [--limit N] [--quiet]      # vectors for meaning search, through local Ollama (GOPOLA_OLLAMA, GOPOLA_EMBED_MODEL)
  gopola show <session-prefix> [--turn N] [--grep <word>]   # --grep: only turns whose prompt, reply or commands mention it
  gopola timeline <session[:turn] | commit-sha> [--before 3] [--after 3]
  gopola timeline --query <words>         # the best prompt/reply hit becomes the anchor
  gopola file <path> [-n 10] [--reads]    # past sessions that edited (or read) it; --hook answers PreToolUse on stdin
  gopola tools [--project P|all] [--since D] [-n 20] [--programs]   # --programs: the commands Bash calls ran, by program
  gopola skills [<name>...] [--project P] [--since D] [--until D] [--json]   # Skill tool calls by name, all projects; /<name> turns apart
  gopola weight <session-prefix | transcript.jsonl> [-n 8] [--agents]   # context per model call, and what filled it; --agents: each subagent's calls
  gopola commands [<session-prefix>] [--grep <text>] [--project P|all] [--since D] [--until D] [--head N] [--no-output]   # Bash calls in full with time, status, output; subagents' too; --grep is an exact substring
  gopola dispatches <session-prefix> [--grep <text>] [--head N] [--json]   # each Agent and SendMessage call: time, description, agent id, hand-back or none, the whole prompt
  gopola hooks <session-prefix> [--grep <text>] [--head N] [--json]   # every hook run: time, hook, status, duration, command (first line, 160 chars); stderr or a block's reason always, output with --head
  gopola context <session-prefix> [--grep <text>] [--json]   # a Copilot session's system prompt, instruction updates and hook context; --grep: matches per part
  gopola digest [--since D] [--until D] [--project P|all] [--commits 3]   # per project per day, no model
  gopola summarize <session-prefix> [--model claude-sonnet-5]   # the one cloud model call; --auto = Stop hook, --auto --final = SessionEnd
  gopola handback <agent-id-prefix> [--since <time>]   # the last report a subagent returned to its session, as received (exit 1: none yet); --since: every report at or after a UTC time
  gopola authored [<repo>] [--ref R] [--since D] [--path P] [--author E] [--no-merges] [--no-bumps]   # commits on R from Claude Code sessions
  gopola blame <file|dir> [--ref R] [--summary | --ranges] [--no-ignore-whitespace] [--since D] [--author E]   # surviving lines on R (whitespace-only changes ignored), marked with the session that wrote them
  gopola doctor                           # index current? hooks and skill wired? what did gopola fail to answer?
  gopola copilot-hook                     # Copilot CLI's sessionStart hook (stdin JSON): the recent block as additionalContext; silent in copilot -p runs
  gopola codex-hook [--install <bin>]     # Codex CLI's SessionStart hook, the same block; silent in codex exec runs; --install adds it to <CODEX_HOME>/hooks.json
  gopola import <file | ref:path>         # a transcript from elsewhere (a cloud session's, exported to its branch), filed under this repo
  gopola import-claude-mem [path]         # default ~/.claude-mem/claude-mem.db
  gopola moved <old path> <new path>      # a checkout moved on disk: its sessions (and its -wt worktrees') keep their history under the new name
  gopola version                          # what is running: version, build sha, build date (-V)

Every command takes --json. Data: ~/.local/share/gopola/gopola.db (GOPOLA_DB);
transcripts read from ~/.claude/projects (GOPOLA_PROJECTS), Copilot CLI sessions from
~/.copilot/session-state (COPILOT_HOME) and Codex CLI rollouts from ~/.codex (CODEX_HOME), and from
gopola's own copies in ~/.local/share/gopola/archive, archive-copilot and archive-codex once the original is deleted.";

/// What is actually running. The sha is injected by `install.sh` at build time, so a binary built
/// from an older checkout says so instead of reporting the repo's current HEAD.
fn version() -> String {
    format!(
        "gopola {} ({}, built {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("GOPOLA_BUILD_SHA").unwrap_or("unknown sha"),
        option_env!("GOPOLA_BUILD_DATE").unwrap_or("unknown date"),
    )
}

fn main() {
    // `gopola … | head`: the first print after head quits panics. Say nothing for that one panic; it still unwinds,
    // so every cleanup runs (restoring SIGPIPE's default would end the process before them).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info.to_string().contains("failed printing to stdout") { default_hook(info) }
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts { json: false, project: None, n: 0, commits: 2, lines: 30, since: None, kind: None,
        turn: None, full: false, quiet: false, model: "claude-sonnet-5".into(),
        until: None, before: 3, after: 3, hook: false, auto: false, final_: false, no_footer: false,
        reads: false, query: false, order: "rank".into(), grep: None, words: false, meaning: false,
        git_ref: None, path: None, author: None, summary: false, no_merges: false, no_bumps: false,
        ranges: false, no_ignore_whitespace: false, programs: false, head: None, all: false, exclude: None, install: None,
        no_output: false, agents: false };
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    fn need(it: &mut std::vec::IntoIter<String>, flag: &str) -> String {
        it.next().unwrap_or_else(|| { eprintln!("gopola: {flag} needs a value"); std::process::exit(2) })
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => o.json = true,
            "--full" => o.full = true,
            "--quiet" | "-q" => o.quiet = true,
            "--project" | "-p" => o.project = Some(need(&mut it, "--project")),
            "-n" | "--limit" => o.n = need(&mut it, "-n").parse().unwrap_or(0),
            "--commits" => o.commits = need(&mut it, "--commits").parse().unwrap_or(2),
            "--lines" => o.lines = need(&mut it, "--lines").parse().unwrap_or(30),
            "--since" => o.since = Some(need(&mut it, "--since")),
            "--kind" => o.kind = Some(need(&mut it, "--kind")),
            "--model" => o.model = need(&mut it, "--model"),
            "--until" => o.until = Some(need(&mut it, "--until")),
            "--before" => o.before = need(&mut it, "--before").parse().unwrap_or(3),
            "--after" => o.after = need(&mut it, "--after").parse().unwrap_or(3),
            "--hook" => o.hook = true,
            "--auto" => o.auto = true,
            "--final" => o.final_ = true,
            "--no-footer" => o.no_footer = true,
            "--reads" => o.reads = true,
            "--programs" => o.programs = true,
            "--no-output" => o.no_output = true,
            "--agents" => o.agents = true,
            "--all" => o.all = true,
            "--exclude" => o.exclude = Some(need(&mut it, "--exclude")),
            "--install" => o.install = Some(need(&mut it, "--install")),
            "--head" => o.head = need(&mut it, "--head").parse().ok(),
            "--query" => o.query = true,
            "--order" => o.order = need(&mut it, "--order"),
            "--turn" => o.turn = need(&mut it, "--turn").parse().ok(),
            "--grep" => o.grep = Some(need(&mut it, "--grep")),
            "--words" => o.words = true,
            "--meaning" => o.meaning = true,
            "--ref" => o.git_ref = Some(need(&mut it, "--ref")),
            "--path" => o.path = Some(need(&mut it, "--path")),
            "--author" => o.author = Some(need(&mut it, "--author")),
            "--summary" => o.summary = true,
            "--no-merges" => o.no_merges = true,
            "--no-bumps" => o.no_bumps = true,
            "--ranges" => o.ranges = true,
            "--no-ignore-whitespace" => o.no_ignore_whitespace = true,
            "-V" | "--version" | "version" => { println!("{}", version()); return; }
            "-h" | "--help" | "help" => { println!("{USAGE}"); return; }
            s if s.starts_with('-') && positional.is_empty() => {
                eprintln!("gopola: unknown flag {s}\n{USAGE}");
                std::process::exit(2)
            }
            s => positional.push(s.to_string()),
        }
    }
    let Some(cmd) = positional.first().cloned() else { println!("{USAGE}"); std::process::exit(2) };
    let arg = positional.get(1).cloned();
    let result = match cmd.as_str() {
        "index" => cmds::index(&o),
        "archive" => archive::archive_cmd(&o),
        "copilot-hook" => cmds::copilot_hook(&o),
        "codex-hook" => match &o.install { Some(bin) => cmds::codex_install(bin), None => cmds::codex_hook(&o) },
        "embed" => embed::embed_cmd(&o),
        "recent" => { if o.n == 0 { o.n = 6 } cmds::recent(&o) }
        "search" => {
            if o.n == 0 { o.n = 10 }
            match positional.get(1..) {
                Some(q) if !q.is_empty() => cmds::search(&o, &q.join(" ")),
                _ => Err("search needs a query".into()),
            }
        }
        "show" => match arg { Some(p) => cmds::show(&o, &p), None => Err("show needs a session prefix".into()) },
        "summarize" => match (o.auto, arg.as_deref()) {
            (true, _) => cmds::summarize(&o, ""),
            (false, Some(p)) => cmds::summarize(&o, p),
            (false, None) => Err("summarize needs a session prefix".into()),
        },
        "timeline" => match positional.get(1..) {
            Some(q) if !q.is_empty() => insight::timeline(&o, &q.join(" ")),
            _ => Err("timeline needs a session[:turn], a commit, or --query <words>".into()),
        },
        "file" => { if o.n == 0 { o.n = 10 } insight::file(&o, arg.as_deref()) }
        "tools" => { if o.n == 0 { o.n = 20 } insight::tools(&o) }
        "skills" => insight::skills(&o, positional.get(1..).unwrap_or_default()),
        "weight" => match arg { Some(p) => weight::weight(&o, &p), None => Err("weight needs a session prefix or a transcript path".into()) },
        "handback" => std::process::exit(handback::run(&o, arg.as_deref())),
        "commands" => commands::commands(&o, arg.as_deref()),
        "dispatches" => dispatches::dispatches(&o, arg.as_deref()),
        "hooks" => hooks::hooks(&o, arg.as_deref()),
        "context" => context::context(&o, arg.as_deref()),
        "doctor" => insight::doctor(&o),
        "authored" => authored::authored(&o, arg.as_deref()),
        "blame" => authored::blame(&o, arg.as_deref()),
        "digest" => { if o.commits == 2 { o.commits = 3 } cmds::digest(&o) }
        "import-claude-mem" => cmds::import_claude_mem(&o, arg.as_deref()),
        "import" => match arg { Some(s) => cmds::import(&o, &s), None => Err("import needs a transcript file or <ref>:<path>".into()) },
        "moved" => match positional.get(1..) {
            Some([old, new]) => cmds::moved(&o, old, new),
            _ => Err("moved needs <old path> <new path>".into()),
        },
        other => Err(format!("unknown command '{other}'\n{USAGE}").into()),
    };
    if let Err(e) = result {
        eprintln!("gopola: {e}");
        // Hooks must never block a session: index, and the --hook/--auto forms, always exit 0.
        std::process::exit(if cmd == "index" || (cmd == "embed" && o.quiet) || o.hook || o.auto { 0 } else { 1 });
    }
}
