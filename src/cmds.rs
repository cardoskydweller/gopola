//! The subcommands. Output is either a terse table or `--json`.

use crate::embed::Hit;
use crate::transcript::{clip, first_line};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

pub type Res = Result<(), Box<dyn std::error::Error>>;

pub struct Opts {
    pub json: bool,
    pub project: Option<String>,
    pub n: usize,
    pub commits: usize,
    pub lines: usize,
    pub since: Option<String>,
    pub kind: Option<String>,
    pub turn: Option<i64>,
    pub full: bool,
    pub quiet: bool,
    pub model: String,
    pub until: Option<String>,
    pub before: usize,
    pub after: usize,
    pub hook: bool,
    pub auto: bool,
    pub final_: bool,
    pub no_footer: bool,
    pub reads: bool,
    pub query: bool,
    pub order: String,
    pub grep: Option<String>,
    pub words: bool,
    pub meaning: bool,
    pub git_ref: Option<String>,
    pub path: Option<String>,
    pub author: Option<String>,
    pub summary: bool,
    pub no_merges: bool,
    pub no_bumps: bool,
    pub ranges: bool,
    pub no_ignore_whitespace: bool,
    pub programs: bool,
    pub head: Option<usize>,
    pub all: bool,
    pub exclude: Option<String>,
    pub install: Option<String>,
}

/// A session this long is worth a summary; below it, the block speaks for itself.
pub const LONG_SESSION_TURNS: i64 = 20;

pub fn default_project() -> String {
    // The same override claude-mem honoured, for scripted or nested sessions.
    let name = |dir: &Path| match crate::db::open() {
        Ok(c) => crate::index::project_for_dir(&c, dir),
        Err(_) => crate::index::project_name(dir),
    };
    if let Ok(d) = std::env::var("CLAUDE_PROJECT_DIR") {
        if !d.trim().is_empty() {
            return name(Path::new(d.trim()));
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dir = top.map(std::path::PathBuf::from).unwrap_or(cwd);
    name(&dir)
}

pub fn project_filter(o: &Opts) -> String {
    o.project.clone().unwrap_or_else(default_project)
}

pub fn short_ts(ts: &str) -> String {
    // 2026-09-21T12:09:44.110Z -> 2026-09-21 12:09
    ts.chars().take(16).collect::<String>().replacen('T', " ", 1)
}

pub fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

pub fn index(o: &Opts) -> Res {
    let mut c = crate::db::open()?;
    let start = std::time::Instant::now();
    let r = crate::index::run(&mut c, o.full)?;
    let ms = start.elapsed().as_millis() as u64;
    if o.json {
        println!("{}", json!({"files_seen": r.files_seen, "files_indexed": r.files_indexed,
            "turns_written": r.turns_written, "lines_skipped": r.lines_skipped, "ms": ms}));
    } else if !o.quiet {
        println!("indexed {} of {} transcripts, {} turns, skipped {} lines, {} ms",
            r.files_indexed, r.files_seen, r.turns_written, r.lines_skipped, ms);
    }
    Ok(())
}

struct SessionRow {
    id: String,
    branch: Option<String>,
    started: String,
    first_prompt: Option<String>,
    turns: i64,
    summary: Option<String>,
    title: Option<String>,
    harness: String,
    scripted: bool,
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

pub fn recent(o: &Opts) -> Res {
    let c = crate::db::open()?;
    let project = project_filter(o);
    let since = o.since.clone().unwrap_or_default();
    let until = o.until.clone().unwrap_or_else(|| "9999".into());
    // The Copilot hook's own session: it fires before the title call, so it would count itself as a scripted run.
    let exclude = o.exclude.as_deref().unwrap_or("");
    let total: i64 = c.query_row(
        "select count(*) from sessions where (project = ?1 or ?1 = 'all') and prompt_count > 0
           and started_at >= ?2 and started_at < ?3 and (scripted = 0 or ?4) and id != ?5",
        params![project, since, until, o.all, exclude], |r| r.get(0))?;
    crate::db::log_usage(&c, "recent", &project, total);
    // Nothing to list, but scripted runs still get their line; with none either, the hook stays silent.
    if total == 0 && (o.all || scripted_fold(&c, &project, &since, &until, exclude)?.is_empty()) {
        if o.json { println!("{}", json!({"project": project, "total": 0, "sessions": [], "scripted": {"count": 0, "last": null}})); }
        return Ok(());
    }
    let mut st = c.prepare(
        "select id, branch, started_at, first_prompt, prompt_count, summary, title, harness, scripted from sessions
         where (project = ?1 or ?1 = 'all') and prompt_count > 0 and started_at >= ?3 and started_at < ?4
           and (scripted = 0 or ?5) and id != ?6
         order by started_at desc limit ?2")?;
    let rows: Vec<SessionRow> = st.query_map(params![project, o.n as i64, since, until, o.all, exclude], |r| Ok(SessionRow {
        id: r.get(0)?, branch: r.get(1)?, started: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        first_prompt: r.get(3)?, turns: r.get(4)?, summary: r.get(5)?, title: r.get(6)?, harness: r.get(7)?, scripted: r.get(8)?,
    }))?.flatten().collect();
    // One footer line so a session knows the verbs exist; counted against --lines.
    let footer = if o.no_footer { 0 } else { 1 };

    let mut out_json = Vec::new();
    let mut lines: Vec<String> = vec![format!("# gopola: {project}, last {} sessions (of {total})", rows.len())];
    // Where each listed session's block starts in `lines`, and when it started.
    let mut listed: Vec<(usize, String)> = Vec::new();
    for s in rows {
        let commits: Vec<String> = c.prepare(
            "select subject from commits where session_id = ?1 order by ts desc limit ?2")?
            .query_map(params![s.id, o.commits as i64], |r| r.get(0))?.flatten().collect();
        let ncommits: i64 = c.query_row("select count(*) from commits where session_id = ?1", [&s.id], |r| r.get(0))?;
        let last_reply: Option<String> = c.query_row(
            "select reply from turns where session_id = ?1 and reply is not null order by n desc limit 1",
            [&s.id], |r| r.get(0)).ok();
        let last_reply = last_reply.as_deref().map(|r| first_line(r).to_string());
        let first = s.first_prompt.as_deref().map(first_line).unwrap_or("");
        let tag = if s.summary.is_none() && s.turns >= LONG_SESSION_TURNS { "  [long, unsummarised]" } else { "" };
        let head = match &s.title { Some(t) => format!("  {}", clip(t, 60)), None => String::new() };
        let mark = harness_mark(Some(&s.harness)).map(|h| format!("  {h}")).unwrap_or_default();
        let mut block = vec![format!("{}  {}{mark}  {}  {}  {}{head}{tag}",
            short_ts(&s.started), short_id(&s.id), s.branch.as_deref().unwrap_or("-"),
            plural(s.turns, "turn"), plural(ncommits, "commit"))];
        match &s.summary {
            Some(sum) => {
                for l in sum.lines().filter(|l| !l.trim().is_empty()).take(3) { block.push(format!("  {}", clip(l.trim(), 110))); }
                for subj in commits.iter().rev() { block.push(format!("  ✓ {}", clip(subj, 80))); }
            }
            None => {
                if !first.is_empty() { block.push(format!("  > {}", clip(first, 80))); }
                for subj in commits.iter().rev() { block.push(format!("  ✓ {}", clip(subj, 80))); }
                if let Some(r) = &last_reply { block.push(format!("  < {}", clip(r, 100))); }
            }
        }
        if !o.json && lines.len() + block.len() + footer > o.lines { break; }
        listed.push((lines.len(), s.started.clone()));
        lines.extend(block);
        out_json.push(json!({"id": s.id, "branch": s.branch, "started_at": s.started, "turns": s.turns,
            "title": s.title, "first_prompt": first, "commits": commits, "last_reply": last_reply, "summary": s.summary,
            "harness": s.harness, "scripted": s.scripted}));
    }
    // Scripted runs fold into one line, counted from the oldest listed session (or from --since).
    let fold_from = |oldest: Option<&(usize, String)>| {
        let bound = if o.since.is_some() { since.as_str() } else { oldest.map_or("", |l| l.1.as_str()) };
        if o.all { Ok(Vec::new()) } else { scripted_fold(&c, &project, bound, &until, exclude) }
    };
    let mut fold = fold_from(listed.last())?;
    // No room for the fold line: dropping the oldest block moves the fold's start to the block before it, so drop it
    // only when runs still fold from there; otherwise (or with no block before it) keep the block and leave the line out.
    if !o.json && !fold.is_empty() && lines.len() + 1 + footer > o.lines {
        fold = match listed.iter().rev().nth(1) { Some(before) => fold_from(Some(before))?, None => Vec::new() };
        if !fold.is_empty() {
            if let Some((start, _)) = listed.pop() {
                lines.truncate(start);
                out_json.pop();
            }
        }
    }
    let scripted_count: i64 = fold.iter().map(|f| f.2).sum();
    let scripted_last = fold.iter().map(|f| f.3.as_str()).max();
    if o.json {
        println!("{}", json!({"project": project, "total": total, "sessions": out_json,
            "scripted": {"count": scripted_count, "last": scripted_last}}));
    } else {
        if let Some(last) = scripted_last {
            let by_harness = fold_counts(&fold, |f| &f.0).iter()
                .map(|(h, n)| plural(*n, &format!("scripted {} run", harness_name(h)))).collect::<Vec<_>>().join(", ");
            let by_project = if project == "all" {
                format!(" ({})", fold_counts(&fold, |f| &f.1).iter().map(|(p, n)| format!("{p} {n}")).collect::<Vec<_>>().join(", "))
            } else { String::new() };
            lines.push(format!("  + {by_harness}, last {}{by_project}", short_ts(last)));
        }
        if footer == 1 { lines.push("# gopola search <words> | show <id> | timeline <id:turn> | file <path> | digest --since <date>".into()); }
        println!("{}", lines.join("\n"));
    }
    Ok(())
}

/// (harness, project, runs, last start) of the scripted runs `recent` folds, from `bound` up to `until`, but `exclude`.
fn scripted_fold(c: &Connection, project: &str, bound: &str, until: &str, exclude: &str) -> rusqlite::Result<Vec<(String, String, i64, String)>> {
    c.prepare(
        "select harness, project, count(*), max(started_at) from sessions
         where scripted = 1 and prompt_count > 0 and (project = ?1 or ?1 = 'all') and started_at >= ?2 and started_at < ?3
           and id != ?4
         group by harness, project")?
        .query_map(params![project, bound, until, exclude], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect()
}

/// The runs of `fold` summed by one of its keys, most first, then by name.
fn fold_counts(fold: &[(String, String, i64, String)], key: fn(&(String, String, i64, String)) -> &str) -> Vec<(&str, i64)> {
    let mut by: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
    for f in fold { *by.entry(key(f)).or_default() += f.2; }
    let mut v: Vec<(&str, i64)> = by.into_iter().collect();
    v.sort_by_key(|&(k, n)| (std::cmp::Reverse(n), k));
    v
}

/// `digest`: what happened per project per day in a window, deterministically. The standup and
/// weekly-digest that claude-mem produced with a model, from the same facts without one.
pub fn digest(o: &Opts) -> Res {
    let c = crate::db::open()?;
    let project = o.project.clone().unwrap_or_else(|| "all".into());
    let since = o.since.clone().unwrap_or_else(|| iso_from_secs(now_secs().saturating_sub(7 * 86_400))[..10].to_string());
    let until = o.until.clone().unwrap_or_else(|| "9999".into());
    let mut st = c.prepare(
        "select substr(started_at, 1, 10) as day, project, count(*), sum(prompt_count),
                (select count(*) from commits k join sessions s2 on s2.id = k.session_id
                  where s2.project = s.project and substr(s2.started_at, 1, 10) = substr(s.started_at, 1, 10)),
                group_concat(coalesce(title, substr(first_prompt, 1, 60)), char(31))
         from sessions s where prompt_count > 0 and (project = ?1 or ?1 = 'all') and started_at >= ?2 and started_at < ?3
         group by day, project order by day desc, project")?;
    let rows: Vec<(String, String, i64, i64, i64, String)> = st.query_map(params![project, since, until],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, Option<String>>(5)?.unwrap_or_default())))?
        .flatten().collect();
    crate::db::log_usage(&c, "digest", &format!("{project} {since}..{until}"), rows.len() as i64);
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(d, p, s, t, k, titles)| json!({"day": d, "project": p, "sessions": s, "turns": t,
            "commits": k, "titles": titles.split('\u{1f}').filter(|x| !x.is_empty()).collect::<Vec<_>>()})).collect();
        println!("{}", json!({"since": since, "until": until, "days": v}));
        return Ok(());
    }
    if rows.is_empty() { println!("no sessions between {since} and {until} for {project}"); return Ok(()); }
    println!("# digest {since}..{} ({project})", if until == "9999" { "now".to_string() } else { until.clone() });
    let mut last_day = String::new();
    for (d, p, s, t, k, titles) in rows {
        if d != last_day { println!("{d}"); last_day = d.clone(); }
        let ts: Vec<&str> = titles.split('\u{1f}').filter(|x| !x.is_empty()).collect();
        println!("  {:<18} {:>2} sessions {:>4} turns {:>3} commits  {}", clip(&p, 18), s, t, k, clip(&ts.join(" | "), 90));
        let subjects: Vec<String> = c.prepare(
            "select k.subject from commits k join sessions s2 on s2.id = k.session_id
             where s2.project = ?1 and substr(s2.started_at, 1, 10) = ?2 order by k.ts desc limit ?3")?
            .query_map(params![p, d, o.commits as i64], |r| r.get(0))?.flatten().collect();
        for sub in subjects.iter().rev() { println!("      ✓ {}", clip(sub, 80)); }
    }
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Quote each whitespace-separated term so user input cannot break FTS5 query syntax.
/// `any` joins terms with OR; bm25 still ranks documents holding more of them first.
fn fts_query(q: &str, any: bool) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(if any { " OR " } else { " " })
}

/// `<session>:<8-digit n>` -> `<8 chars>:<n>`; a commit sha -> its first 8 chars.
fn short_ref(kind: &str, r: &str) -> String {
    match kind {
        "prompt" | "reply" | "command" | "turn" => match r.split_once(':') {
            Some((id, n)) => format!("{}:{}", short_id(id), n.trim_start_matches('0')),
            None => r.to_string(),
        },
        "commit" => short_id(r).to_string(),
        "summary" | "title" => short_id(r).to_string(),
        _ => format!("obs {r}"),
    }
}

pub fn search(o: &Opts, query: &str) -> Res {
    let c = crate::db::open()?;
    let project = project_filter(o);
    let since = o.since.clone().unwrap_or_default();
    let until = o.until.clone().unwrap_or_else(|| "9999".into());
    let kind = o.kind.clone().unwrap_or_default();
    // Meaning ranks turns, so it applies only when no --kind narrows the rows, and never with --words.
    // rank_turns asks Ollama nothing when no turn has a vector for the model; then no note either.
    let mut note: Option<String> = None;
    if o.meaning && !o.words {
        if !kind.is_empty() {
            note = Some("# meaning not used: --kind searches words only".into());
        } else if !crate::embed::has_vectors(&c, &crate::embed::Embedder::from_env().model).unwrap_or(true) {
            note = Some(format!("# meaning not used: no turn has a vector for {} (run `gopola embed`)",
                crate::embed::Embedder::from_env().model));
        }
    }
    let meaning: Vec<Hit> = if o.words || !kind.is_empty() { Vec::new() } else {
        let e = crate::embed::Embedder::from_env();
        match crate::embed::rank_turns(&c, &e, query, &project, &since, &until, 200) {
            Ok(m) => m,
            Err(err) => { note = Some(format!("# meaning search off: {err}; words only")); Vec::new() }
        }
    };
    let fused = !meaning.is_empty();
    // Words only: 0.3.0's query, order and LIMIT exactly. Fused: by rank, and enough rows to fuse;
    // `--order date` then orders the fused top n (fuse).
    let order = if o.order == "date" && !fused { "ts desc" } else { "rank" };
    let limit = if fused { o.n.max(50) } else { o.n };
    let mut st = c.prepare(&format!(
        "select kind, ref, project, ts, snippet(fts, 4, '[', ']', '…', 8)
         from fts where fts match ?1 and (project = ?2 or ?2 = 'all')
           and ts >= ?3 and ts < ?6 and (kind = ?4 or ?4 = '')
         order by {order} limit ?5"))?;
    let mut any_term = false;
    let mut word_rows: Vec<Hit> = st.query_map(
        params![fts_query(query, false), project, since, kind, limit as i64, until],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.flatten().collect();
    if word_rows.is_empty() && !fused && query.split_whitespace().count() > 1 {
        // 0.3.0's any-word fallback, only when meaning has nothing to offer: it ranks on "on" and "my" too.
        any_term = true;
        word_rows = st.query_map(
            params![fts_query(query, true), project, since, kind, o.n as i64, until],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.flatten().collect();
    }
    // `doctor` lists searches that found nothing from this count: word hits, never fused ones.
    crate::db::log_usage(&c, "search", query, word_rows.len() as i64);
    let nearest = fused && word_rows.is_empty() && !o.meaning;
    let rows: Vec<(Hit, &str)> = if fused {
        fuse(&word_rows, &meaning, o)
    } else {
        word_rows.into_iter().map(|h| (h, "words")).collect()
    };
    if o.json {
        let v: Vec<Value> = rows.iter().map(|((k, rf, p, ts, s), via)|
            json!({"kind": k, "ref": rf, "project": p, "ts": ts, "snippet": s, "via": via})).collect();
        if let Some(n) = &note {
            eprintln!("{n}"); // stdout stays one JSON document
        }
        if nearest {
            eprintln!("# no result has every word; nearest by meaning");
        }
        println!("{}", json!(v));
        return Ok(());
    }
    if let Some(n) = &note {
        println!("{n}");
    }
    if rows.is_empty() {
        println!("no matches for '{query}' in {project}");
        return Ok(());
    }
    if nearest {
        println!("# no result has every word; nearest by meaning");
    }
    if any_term {
        println!("# no result has every term; these match some of '{query}'");
    }
    for ((k, rf, p, ts, s), _) in rows {
        let proj = if project == "all" { format!("{p}  ") } else { String::new() };
        let mark = harness_mark(harness_of(&c, rf.split(':').next().unwrap_or(""))?.as_deref()).map(|h| format!("{h}  ")).unwrap_or_default();
        println!("{}  {:<11} {}{:<14} {mark}{}", &ts[..10.min(ts.len())], k, proj, short_ref(&k, &rf),
            s.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    Ok(())
}

/// The harness a session came from; an id that names no session (a commit's sha) has none.
pub fn harness_of(c: &Connection, session: &str) -> rusqlite::Result<Option<String>> {
    c.query_row("select harness from sessions where id = ?1", [session], |r| r.get(0)).optional()
}

/// What a session's mark prints: nothing for Claude Code's own sessions, the harness for any other.
pub fn harness_mark(harness: Option<&str>) -> Option<&str> {
    harness.filter(|h| *h != "claude")
}

/// The name a harness goes by in a sentence.
pub fn harness_name(harness: &str) -> &str {
    match harness {
        "claude" => "Claude Code",
        "copilot" => "Copilot",
        "codex" => "Codex",
        h => h,
    }
}

/// The commands that read Claude Code's transcript shapes refuse another harness's session instead of printing nothing.
pub fn claude_only(c: &Connection, id: &str, cmd: &str) -> Res {
    if let Some(h) = harness_mark(harness_of(c, id)?.as_deref()) { return Err(refusal(id, cmd, h).into()); }
    Ok(())
}

pub fn refusal(id: &str, cmd: &str, harness: &str) -> String {
    format!("{cmd} reads Claude Code sessions only for now; {} is a {} session", short_id(id), harness_name(harness))
}

/// Copilot's `sessionStart` hook: the session-start block as `additionalContext`, only in an interactive session (a
/// scripted `copilot -p` run's reviewer should see only its brief). A hook that fails says nothing and exits 0: a nonzero
/// exit blocks Copilot.
pub fn copilot_hook(_o: &Opts) -> Res {
    // Read it all first: a scripted run's stdin holds its whole prompt, and a reader that quits early breaks Copilot's write.
    let input = crate::insight::read_stdin_json();
    if !interactive_parent() { return Ok(()); }
    let Some(cwd) = input.get("cwd").and_then(Value::as_str) else { return Ok(()) };
    let me = input.get("sessionId").and_then(Value::as_str).unwrap_or_default();
    if let Some(text) = session_start_context(cwd, me) {
        println!("{}", json!({"additionalContext": text}));
    }
    Ok(())
}

/// Codex's `SessionStart` hook: the same block as `hookSpecificOutput.additionalContext`, at a session's start or resume
/// (not after `compact` or `clear`), never in a `codex exec` run. It says nothing on failure and exits 0.
pub fn codex_hook(_o: &Opts) -> Res {
    // Read it all first, as Copilot's hook does: a reader that quits early breaks the writer.
    let input = crate::insight::read_stdin_json();
    if codex_exec_run() || !matches!(input.get("source").and_then(Value::as_str), Some("startup" | "resume")) { return Ok(()); }
    let Some(cwd) = input.get("cwd").and_then(Value::as_str) else { return Ok(()) };
    let me = input.get("session_id").and_then(Value::as_str).unwrap_or_default();
    if let Some(text) = session_start_context(cwd, me) {
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": text}}));
    }
    Ok(())
}

/// The session-start block for the project of `cwd`, without the session `me`: index quietly, then `recent`. None when
/// any step fails or the project has nothing to show.
fn session_start_context(cwd: &str, me: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    // Its stdout is the hook's: nothing of the index's may reach it. It exits 0 even when it fails.
    let _ = std::process::Command::new(&exe).args(["index", "--quiet"]).stdout(std::process::Stdio::null()).status();
    let c = crate::db::open().ok()?;
    let project = crate::index::project_for_dir(&c, Path::new(cwd));
    let out = std::process::Command::new(exe).args(["recent", "--project", &project, "--exclude", me]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    (out.status.success() && !text.trim().is_empty()).then_some(text)
}

/// Whether the `copilot` process that started this hook is an interactive one: the first of up to four ancestors whose
/// program is named `copilot`, given neither `-p` nor `--prompt`. With no such ancestor the hook cannot tell, so it says no.
/// An empty `GOPOLA_COPILOT_PARENT_ARGS` stands for that case in the tests.
fn interactive_parent() -> bool {
    parent_args("GOPOLA_COPILOT_PARENT_ARGS", "copilot")
        .is_some_and(|a| !a.split_whitespace().any(|w| w == "-p" || w == "--prompt" || w.starts_with("--prompt=")))
}

/// Whether this hook runs under `codex exec` (alias `codex e`): the first of up to four ancestors whose program is named
/// `codex` has `exec` as its subcommand, the first word after its root options (`codex [OPTIONS] <COMMAND>`) and their
/// values. The 2026-11-12 probe found an exec run's hook a child of `codex exec say hi`.
/// Any other ancestry, none found included, is a session the owner is in. `GOPOLA_CODEX_PARENT_ARGS` stands in for `ps`.
fn codex_exec_run() -> bool {
    // The root options that take a value, from `codex --help` (0.167.0); `--opt=value` is one word.
    const TAKES_VALUE: &[&str] = &["-c", "--config", "--enable", "--disable", "--remote", "--remote-auth-token-env", "-i", "--image",
        "-m", "--model", "--local-provider", "-p", "--profile", "-s", "--sandbox", "-C", "--cd", "--add-dir", "-a", "--ask-for-approval"];
    parent_args("GOPOLA_CODEX_PARENT_ARGS", "codex").is_some_and(|a| {
        let mut words = a.split_whitespace().skip(1);
        while let Some(w) = words.next() {
            if !w.starts_with('-') { return matches!(w, "exec" | "e") }
            if TAKES_VALUE.contains(&w) { words.next(); }
        }
        false
    })
}

/// The args of the first of up to four ancestors whose program is named `program`, from `ps`; the variable `var`, when
/// set, stands in for the lookup (empty: no such ancestor).
fn parent_args(var: &str, program: &str) -> Option<String> {
    std::env::var(var).map(|a| Some(a).filter(|a| !a.is_empty())).unwrap_or_else(|_| {
        let mut pid = std::os::unix::process::parent_id().to_string();
        for _ in 0..4 {
            let out = std::process::Command::new("ps").args(["-o", "ppid=,args=", "-p", &pid]).output().ok()?;
            let line = String::from_utf8_lossy(&out.stdout);
            let (ppid, args) = line.trim().split_once(char::is_whitespace)?;
            let args = args.trim_start();
            let name = args.split_whitespace().next().and_then(|p| Path::new(p).file_name());
            if name.is_some_and(|p| p == program) { return Some(args.to_owned()); }
            pid = ppid.to_owned();
        }
        None
    })
}

/// `codex-hook --install <bin>`: gopola's one handler under `SessionStart` in `<CODEX_HOME>/hooks.json`, every other
/// handler kept; an older gopola handler is replaced in place. A file that is not JSON is left alone (exit 1).
pub fn codex_install(bin: &str) -> Res {
    let path = crate::index::codex_home().join("hooks.json");
    let refuse = |why: String| format!("{} {why}; left it alone", path.display());
    let old: Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| refuse(format!("is not JSON ({e})")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(refuse(format!("cannot be read ({e})")).into()),
    };
    // Codex runs it from each session's cwd, so a relative bin is made absolute here.
    let bin = std::path::absolute(bin)?.to_string_lossy().into_owned();
    // Only the folder is shell-quoted, so the command still reads `gopola codex-hook`, which doctor looks for.
    let handler = json!({"type": "command", "command": format!("'{}'/gopola codex-hook", bin.replace('\'', r"'\''")), "timeout": 10});
    let mut new = old.clone();
    let Some(groups) = new.as_object_mut().map(|o| o.entry("hooks").or_insert(json!({})))
        .and_then(Value::as_object_mut).map(|h| h.entry("SessionStart").or_insert(json!([])))
        .and_then(Value::as_array_mut) else { return Err(refuse("has no hooks.SessionStart list".into()).into()) };
    let mut placed = false;
    for h in groups.iter_mut().filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut)).flatten() {
        if h["command"].as_str().is_some_and(|c| c.contains("gopola codex-hook")) {
            *h = handler.clone();
            placed = true;
        }
    }
    if !placed { groups.push(json!({"hooks": [handler]})); }
    if new == old {
        println!("left {} alone, unchanged", path.display());
        return Ok(());
    }
    let target = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&path)?,
        Ok(_) => path.clone(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.clone(),
        Err(e) => return Err(e.into()),
    };
    // Written beside the target, then renamed: a write cut short never leaves the owner's hooks half a file.
    write_codex_hooks(&target, &(serde_json::to_string_pretty(&new)? + "\n"))?;
    println!("wrote {} (Codex SessionStart hook)", path.display());
    Ok(())
}

fn write_codex_hooks(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.gopola-tmp");
    std::fs::write(&tmp, contents)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        return match std::fs::remove_file(&tmp) {
            Ok(()) => Err(e),
            Err(cleanup) => Err(std::io::Error::new(
                e.kind(),
                format!("{e}; could not remove {}: {cleanup}", tmp.display()),
            )),
        };
    }
    Ok(())
}

#[cfg(test)]
mod codex_install_tests {
    use super::*;

    #[test]
    fn failed_codex_hook_rename_removes_its_temp_file() {
        let root = std::env::temp_dir().join(format!(
            "gopola-codex-hook-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("hooks.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("existing"), "unchanged").unwrap();
        let tmp = path.with_extension("json.gopola-tmp");

        let failed = write_codex_hooks(&path, "{}").is_err();
        let leftover = tmp.exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(failed);
        assert!(!leftover);
    }
}

/// Words and meaning as one ranking. Rows of one turn share a key, so the turn appears once, with the
/// word row's kind and snippet when words found it (a better snippet than a prompt's first line).
/// Relevance picks the top n (`--meaning`: meaning alone); `--order date` then orders those n by date,
/// newest first, ties by ref, so the result never depends on HashMap order.
fn fuse(word_rows: &[Hit], meaning: &[Hit], o: &Opts) -> Vec<(Hit, &'static str)> {
    let key = |h: &Hit| if matches!(h.0.as_str(), "prompt" | "reply" | "command" | "turn") {
        format!("t:{}", h.1)
    } else {
        format!("{}:{}", h.0, h.1)
    };
    let mut by_key: std::collections::HashMap<String, (Hit, &'static str)> = std::collections::HashMap::new();
    for h in word_rows {
        by_key.entry(key(h)).or_insert((h.clone(), "words"));
    }
    for h in meaning {
        by_key.entry(key(h)).and_modify(|e| e.1 = "both").or_insert((h.clone(), "meaning"));
    }
    let order: Vec<String> = if o.meaning {
        meaning.iter().map(key).collect()
    } else {
        let w: Vec<String> = word_rows.iter().map(key).collect();
        let m: Vec<String> = meaning.iter().map(key).collect();
        fuse_keys(w, m)
    };
    let mut top: Vec<(Hit, &'static str)> = order.iter().filter_map(|k| by_key.get(k).cloned()).take(o.n).collect();
    if o.order == "date" {
        top.sort_by(|a, b| b.0.3.cmp(&a.0.3).then_with(|| a.0.1.cmp(&b.0.1)));
    }
    top
}

pub fn show(o: &Opts, prefix: &str) -> Res {
    let c = crate::db::open()?;
    let ids: Vec<(String, String, Option<String>, String, bool)> = c.prepare(
        "select id, project, started_at, harness, scripted from sessions where id like ?1 order by started_at desc")?
        .query_map([format!("{prefix}%")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.flatten().collect();
    let (id, project, started, harness, scripted) = match ids.len() {
        0 => return Err(format!("no session starting with '{prefix}'").into()),
        1 => ids.into_iter().next().unwrap(),
        n => return Err(format!("{n} sessions start with '{prefix}'; give more characters").into()),
    };
    let mut st = c.prepare(
        "select n, ts, prompt, reply, files, commands from turns where session_id = ?1 and (n = ?2 or ?2 = 0) order by n")?;
    let json_list = |s: Option<String>| -> Vec<String> { serde_json::from_str(&s.unwrap_or_default()).unwrap_or_default() };
    type Turn = (i64, String, String, Option<String>, Vec<String>, Vec<String>);
    let mut turns: Vec<Turn> = st.query_map(
        params![id, o.turn.unwrap_or(0)],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, json_list(r.get(4)?), json_list(r.get(5)?))))?
        .flatten().collect();
    // --grep: only the turns whose prompt, reply or commands mention the word, and of the commands only those.
    let needle = o.grep.as_deref().map(str::to_lowercase);
    let mentions = |s: &str| needle.as_deref().is_none_or(|w| s.to_lowercase().contains(w));
    if needle.is_some() {
        turns.retain(|(_, _, p, r, _, cmds)| {
            mentions(p) || r.as_deref().is_some_and(&mentions) || cmds.iter().any(|x| mentions(x))
        });
        for t in &mut turns {
            t.5.retain(|x| mentions(x));
        }
    }
    crate::db::log_usage(&c, "show", &format!("{id}{}", o.grep.as_deref().map(|g| format!(" --grep {g}")).unwrap_or_default()),
        if needle.is_some() { turns.len() as i64 } else { 1 });
    if o.json {
        let v: Vec<Value> = turns.iter().map(|(n, ts, p, r, f, cmds)|
            json!({"n": n, "ts": ts, "prompt": p, "reply": r, "files": f, "commands": cmds})).collect();
        println!("{}", json!({"id": id, "project": project, "started_at": started, "harness": harness, "scripted": scripted, "turns": v}));
        return Ok(());
    }
    let mark = harness_mark(Some(&harness)).map(|h| format!("  {h}")).unwrap_or_default();
    println!("# {id}{mark}  {project}  {}", started.as_deref().map(short_ts).unwrap_or_default());
    if let (Some(g), true) = (&o.grep, turns.is_empty()) {
        println!("no turn mentions '{g}'");
    }
    let full = o.turn.is_some();
    for (n, ts, p, r, files, cmds) in turns {
        if full {
            println!("--- turn {n}  {}\n> {p}\n", short_ts(&ts));
            if let Some(r) = r { println!("{r}"); }
            if !files.is_empty() { println!("\nfiles: {}", files.join(", ")); }
            if needle.is_some() && !cmds.is_empty() { println!("\ncommands:\n{}", cmds.join("\n")); }
        } else {
            let hm = short_ts(&ts);
            println!("{n:>3}  {}  > {}", hm.get(11..).unwrap_or(""), clip(first_line(&p), 100));
            if !files.is_empty() {
                let short: Vec<&str> = files.iter().map(|x| x.rsplit('/').next().unwrap_or(x)).collect();
                println!("     files: {}", clip(&short.join(", "), 100));
            }
            if let Some(w) = &needle {
                for x in &cmds {
                    println!("     $ {}", clip(&matching_line(x, w), 140));
                }
            }
            if let Some(r) = r { println!("     < {}", clip(first_line(&r), 100)); }
        }
    }
    Ok(())
}

/// A command's first line, and the line that mentions `word` (lowercase) when that is a later one: a
/// heredoc's match is rarely on its first line.
fn matching_line(cmd: &str, word: &str) -> String {
    let first = first_line(cmd);
    match cmd.lines().find(|l| l.to_lowercase().contains(word)) {
        Some(l) if l != first => format!("{} … {}", clip(first, 60), l.trim()),
        _ => first.to_string(),
    }
}

pub fn import_claude_mem(o: &Opts, path: Option<&str>) -> Res {
    let home = std::env::var("HOME").unwrap_or_default();
    let default = format!("{home}/.claude-mem/claude-mem.db");
    let src = Path::new(path.unwrap_or(&default));
    if !src.is_file() {
        return Err(format!("no claude-mem database at {}", src.display()).into());
    }
    let s = crate::db::open_read_only(src)?;
    let mut c = crate::db::open()?;
    let mut st = s.prepare(
        "select id, project, created_at, type, title, subtitle, facts, narrative from observations order by id")?;
    type Row = (i64, String, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows = st.query_map([], |r| Ok::<Row, _>((
        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?;
    let tx = c.transaction()?;
    let mut imported = 0i64;
    for (id, project, ts, typ, title, subtitle, facts_json, narrative) in rows.flatten() {
        let facts = facts_json.as_deref()
            .and_then(|f| serde_json::from_str::<Vec<String>>(f).ok())
            .map(|v| v.join("; "))
            .or(facts_json);
        insert_observation(&tx, id, &project, &ts, [typ.as_deref(), title.as_deref(), subtitle.as_deref(),
            facts.as_deref(), narrative.as_deref()])?;
        imported += 1;
    }
    tx.commit()?;
    let total: i64 = c.query_row("select count(*) from observations", [], |r| r.get(0))?;
    if o.json {
        println!("{}", json!({"imported": imported, "total": total}));
    } else {
        println!("imported {imported} observations from {}; {total} now in gopola", src.display());
    }
    Ok(())
}

/// `gopola import <file | ref:path>`: copy a transcript that is not under ~/.claude/projects (a cloud
/// session's, exported to its branch) into gopola's imports, filed under the repository it is run in.
pub fn import(o: &Opts, src: &str) -> Res {
    let bytes = if Path::new(src).is_file() {
        std::fs::read(src)?
    } else if src.contains(':') {
        let out = std::process::Command::new("git").args(["show", "--end-of-options", src]).output()?;
        if !out.status.success() {
            return Err(format!("git show {src}: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
        }
        out.stdout
    } else {
        return Err(format!("{src}: no such file, and not a <ref>:<path>").into());
    };
    let id = transcript_session_id(&bytes)
        .ok_or_else(|| format!("{src}: not a Claude Code transcript (no record has a sessionId of letters, digits, - or _)"))?;
    let out = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output()?;
    let launch = if out.status.success() {
        std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    } else {
        std::env::current_dir()?
    };
    let dir = crate::index::imports_root().join(crate::index::encode(&launch.to_string_lossy()));
    std::fs::create_dir_all(&dir)?;
    // One copy per session: an earlier import from another repository would be indexed against this one on every run.
    for other in std::fs::read_dir(crate::index::imports_root())?.flatten().map(|d| d.path().join(format!("{id}.jsonl"))) {
        if other.parent() != Some(dir.as_path()) && other.is_file() {
            std::fs::remove_file(&other)?;
        }
    }
    let file = dir.join(format!("{id}.jsonl"));
    std::fs::write(&file, &bytes)?;
    let mut c = crate::db::open()?;
    crate::index::run(&mut c, false)?;
    let (project, turns, started, ended): (String, i64, Option<String>, Option<String>) = c.query_row(
        "select project, prompt_count, started_at, ended_at from sessions where id = ?1", [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let short = &id[..8.min(id.len())];
    if o.json {
        println!("{}", json!({"session": id, "project": project, "turns": turns, "started_at": started,
            "ended_at": ended, "source": src, "path": file}));
    } else {
        println!("imported {short} into {project}: {turns} turns, {} → {}, from {src}\n  gopola show {short}",
            started.as_deref().unwrap_or("?"), ended.as_deref().unwrap_or("?"));
    }
    Ok(())
}

/// `gopola moved <old> <new>`: a checkout moved on disk. Its sessions, and those of its `-wt` task
/// worktrees, are refiled under the new path and the new project name, and `index` (`--full` too)
/// reads their transcript folders as under the new path from now on.
pub fn moved(o: &Opts, old: &str, new: &str) -> Res {
    let old = old.trim_end_matches('/');
    if !Path::new(old).is_absolute() {
        return Err(format!("{old}: the old path must be absolute").into());
    }
    let new = std::fs::canonicalize(new).map_err(|e| format!("{new}: {e}"))?;
    let new_s = new.to_string_lossy().to_string();
    let to = crate::index::project_name(&new);
    let c = crate::db::open()?;
    let tx = c.unchecked_transaction()?;
    tx.execute("insert or replace into moves (old, new) values (?1, ?2)", params![old, new_s])?;
    let under = "(launch_dir = ?1 or substr(launch_dir, 1, length(?1) + 1) = ?1 || '/'
                  or substr(launch_dir, 1, length(?1) + 4) = ?1 || '-wt/')";
    let from: Vec<String> = tx.prepare(&format!("select distinct project from sessions where {under}"))?
        .query_map([old], |r| r.get(0))?.flatten().collect();
    let n = tx.execute(&format!(
        "update sessions set project = ?3, launch_dir = ?2 || substr(launch_dir, length(?1) + 1) where {under}"),
        params![old, new_s, to])?;
    for p in &from {
        tx.execute("update fts set project = ?2 where project = ?1", params![p, to])?;
        tx.execute("update observations set project = ?2 where project = ?1", params![p, to])?;
    }
    tx.commit()?;
    if o.json {
        println!("{}", json!({"old": old, "new": new_s, "project": to, "was": from, "sessions": n}));
    } else {
        println!("moved {old} → {new_s}: {n} session(s) now under {to}{}",
            if from.is_empty() { String::new() } else { format!(" (were {})", from.join(", ")) });
    }
    Ok(())
}

/// The `sessionId` of the first record that has one. It names a file, so only letters, digits, `-` and `_` pass.
fn transcript_session_id(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes).lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v.get("sessionId").and_then(Value::as_str).map(str::to_owned))
        .filter(|id| !id.is_empty() && id.len() <= 100 && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
}

/// `fields` is [type, title, subtitle, facts, narrative].
fn insert_observation(tx: &Connection, id: i64, project: &str, ts: &str, fields: [Option<&str>; 5]) -> rusqlite::Result<()> {
    let [typ, title, subtitle, facts, narrative] = fields;
    tx.execute(
        "insert or replace into observations (id, project, ts, type, title, subtitle, facts, narrative)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, project, ts, typ, title, subtitle, facts, narrative])?;
    let text = [title, subtitle, facts, narrative].iter().flatten().copied().collect::<Vec<_>>().join("\n");
    crate::db::fts_delete(tx, "observation", &id.to_string())?;
    crate::db::fts_insert(tx, "observation", &id.to_string(), project, ts, &text)?;
    Ok(())
}

/// One model call, on demand: a compact digest of the session in, three lines out.
/// The binary is `claude` on PATH (override GOPOLA_CLAUDE, used by the test).
pub fn summarize(o: &Opts, prefix: &str) -> Res {
    let mut c = crate::db::open()?;
    let prefix: String = if o.auto {
        let input = crate::insight::read_stdin_json();
        if input.get("stop_hook_active").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            return Ok(());
        }
        let Some(sid) = input.get("session_id").and_then(serde_json::Value::as_str) else { return Ok(()) };
        crate::index::run(&mut c, false)?;
        let due: Option<(i64, i64)> = c.query_row(
            "select prompt_count, summarized_turns from sessions where id = ?1", [sid], |r| Ok((r.get(0)?, r.get(1)?))).ok();
        // Stop: every 20 turns of a long session. SessionEnd (--final): once more if anything grew since.
        match due {
            Some((turns, done)) if turns >= LONG_SESSION_TURNS && turns - done >= LONG_SESSION_TURNS => sid.to_string(),
            Some((turns, done)) if o.final_ && turns >= LONG_SESSION_TURNS && turns > done => sid.to_string(),
            _ => return Ok(()),
        }
    } else {
        prefix.to_string()
    };
    let prefix = prefix.as_str();
    let ids: Vec<(String, String, Option<String>)> = c.prepare(
        "select id, project, started_at from sessions where id like ?1 and prompt_count > 0")?
        .query_map([format!("{prefix}%")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect();
    let (id, project, started) = match ids.len() {
        0 => return Err(format!("no session starting with '{prefix}'").into()),
        1 => ids.into_iter().next().unwrap(),
        n => return Err(format!("{n} sessions start with '{prefix}'; give more characters").into()),
    };
    let digest = session_digest(&c, &id, &project, started.as_deref().unwrap_or(""))?;
    let prompt = "Below is a digest of one Claude Code session: the user's prompts (>), the assistant's \
closing line per turn (<), files edited (=) and commits. Write exactly three lines, plain text, no \
markdown, each under 40 words:\nasked: what the user wanted\ndone: what was actually completed\nnext: \
what was left open or said to be next. Use the user's own words where they are specific. Write \
'nothing' if a line has no content.\n\n";
    let bin = std::env::var("GOPOLA_CLAUDE").unwrap_or_else(|_| "claude".into());
    let out = std::process::Command::new(&bin)
        .args(["-p", "--bare", "--no-session-persistence", "--output-format", "text", "--tools", "",
               "--model", &o.model, "--"])
        .arg(format!("{prompt}{digest}"))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{bin} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let summary: String = String::from_utf8_lossy(&out.stdout).lines()
        .map(str::trim).filter(|l| !l.is_empty()).take(3).collect::<Vec<_>>().join("\n");
    if summary.is_empty() {
        return Err("model returned nothing".into());
    }
    let now = now_iso();
    let tx = c.transaction()?;
    tx.execute("update sessions set summary = ?1, summarized_at = ?2,
                  summarized_turns = (select prompt_count from sessions where id = ?3) where id = ?3",
        params![summary, now, id])?;
    crate::db::log_usage(&tx, if o.auto { "summarize-auto" } else { "summarize" }, &id, digest.len() as i64);
    crate::db::fts_delete(&tx, "summary", &id)?;
    crate::db::fts_insert(&tx, "summary", &id, &project, started.as_deref().unwrap_or(""), &summary)?;
    tx.commit()?;
    if o.auto {
        return Ok(());
    }
    if o.json {
        println!("{}", json!({"id": id, "summary": summary, "digest_bytes": digest.len(), "model": o.model}));
    } else {
        println!("# {}  {project}  {}\n{summary}", short_id(&id), started.as_deref().map(short_ts).unwrap_or_default());
    }
    Ok(())
}

/// What the model sees: capped so a long session costs a few thousand tokens, not the transcript.
pub fn session_digest(c: &Connection, id: &str, project: &str, started: &str) -> rusqlite::Result<String> {
    const BUDGET: usize = 12_000; // bytes, roughly 3k tokens
    let mut st = c.prepare("select n, prompt, reply, files from turns where session_id = ?1 order by n")?;
    let turns: Vec<(i64, String, Option<String>, String)> = st.query_map([id], |r| Ok((
        r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<String>>(3)?.unwrap_or_default())))?.flatten().collect();
    let commits: Vec<String> = c.prepare("select subject from commits where session_id = ?1 order by ts")?
        .query_map([id], |r| r.get(0))?.flatten().collect();
    let per_turn = (BUDGET / turns.len().max(1)).clamp(120, 600);
    let mut d = format!("session {} in {project}, started {}, {} turns, {} commits\n\n",
        short_id(id), short_ts(started), turns.len(), commits.len());
    for (n, p, r, f) in &turns {
        d.push_str(&format!("{n}> {}\n", clip(&p.split_whitespace().collect::<Vec<_>>().join(" "), per_turn / 2)));
        if let Some(r) = r {
            d.push_str(&format!("{n}< {}\n", clip(first_line(r), per_turn / 2)));
        }
        let files: Vec<String> = serde_json::from_str(f).unwrap_or_default();
        if !files.is_empty() {
            let short: Vec<&str> = files.iter().map(|x| x.rsplit('/').next().unwrap_or(x)).collect();
            d.push_str(&format!("{n}= {}\n", clip(&short.join(", "), 120)));
        }
    }
    if !commits.is_empty() {
        d.push_str("\ncommits:\n");
        for s in commits.iter().take(40) { d.push_str(&format!("- {}\n", clip(s, 100))); }
    }
    Ok(d)
}

pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    iso_from_secs(secs)
}

pub fn iso_from_secs(secs: u64) -> String {
    // civil-from-days (Howard Hinnant): an ISO timestamp without a date crate.
    let days = (secs / 86_400) as i64;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Reciprocal-rank fusion of the word ranking and the meaning ranking by turn key. A turn's prompt, reply
/// and command rows share a key, so the word ranking may list one turn twice; rrf adds a score per
/// occurrence, so each key counts once, at its first (best) position.
fn fuse_keys(words: Vec<String>, meaning: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let words: Vec<String> = words.into_iter().filter(|k| seen.insert(k.clone())).collect();
    crate::embed::rrf(&[words, meaning], 60.0)
}

#[cfg(test)]
mod tests {
    use super::{fuse_keys, transcript_session_id};

    #[test]
    fn an_import_takes_the_first_session_id_and_refuses_one_that_is_not_a_file_name() {
        let t = b"{\"type\":\"queue-operation\"}\nnot json\n{\"type\":\"user\",\"sessionId\":\"0a1b-c_2\"}\n{\"sessionId\":\"other\"}\n";
        assert_eq!(transcript_session_id(t).as_deref(), Some("0a1b-c_2"));
        assert_eq!(transcript_session_id(b"{\"type\":\"user\"}\n"), None);
        assert_eq!(transcript_session_id(b"{\"sessionId\":\"../../etc/x\"}\n"), None);
        assert_eq!(transcript_session_id(b"{\"sessionId\":\"\"}\n"), None);
    }

    #[test]
    fn a_turn_counts_once_per_ranking() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let order = fuse_keys(s(&["A", "A", "B"]), s(&["B"]));
        assert_eq!(order, s(&["B", "A"]), "B is in both rankings; A twice in one must not outrank it");
    }

    #[test]
    fn a_query_error_is_not_read_as_no_harness() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("create table sessions (id text, harness text)").unwrap();
        assert_eq!(super::harness_of(&c, "a commit sha").ok(), Some(None), "no such session");
        c.execute_batch("alter table sessions drop column harness").unwrap();
        assert!(super::harness_of(&c, "x").is_err());
    }
}
