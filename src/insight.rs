//! Questions over the index beyond "recent" and "search": what happened around a hit, what past
//! sessions did to a file, which tools were used, and what gopola itself should improve next.

use crate::cmds::{Opts, Res, short_id, short_ts};
use crate::transcript::{clip, first_line};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// `timeline <session-prefix>[:<turn>]` or `timeline <commit-sha-prefix>`: the turns around a turn,
/// or the sessions around a session in the same project.
pub fn timeline(o: &Opts, r#ref: &str) -> Res {
    let c = crate::db::open()?;
    let (before, after) = (o.before as i64, o.after as i64);
    // --query: the best prompt/reply hit becomes the anchor, as claude-mem's timeline(query=…) did.
    if o.query {
        let project = o.project.clone().unwrap_or_else(crate::cmds::default_project);
        let q = r#ref.split_whitespace().map(|t| format!("\"{}\"", t.replace('"', "\"\""))).collect::<Vec<_>>().join(" ");
        let hit: Option<String> = c.query_row(
            "select ref from fts where fts match ?1 and (project = ?2 or ?2 = 'all') and kind in ('prompt','reply') order by rank limit 1",
            params![q, project], |r| r.get(0)).ok();
        let Some(hit) = hit else { println!("no prompt or reply matches '{ref}' in {project}"); return Ok(()); };
        let (sid, n) = hit.split_once(':').ok_or("bad ref")?;
        let n: i64 = n.parse()?;
        println!("# anchor: {}:{n} for '{ref}'", crate::cmds::short_id(sid));
        crate::db::log_usage(&c, "timeline-query", r#ref, 1);
        return turns_around(&c, o, sid, n, before, after);
    }
    if let Some((sid, n)) = r#ref.split_once(':') {
        let n: i64 = n.parse().map_err(|_| format!("turn must be a number: {n}"))?;
        let id = resolve_session(&c, sid)?;
        crate::db::log_usage(&c, "timeline", r#ref, 1);
        return turns_around(&c, o, &id, n, before, after);
    }
    if r#ref.len() >= 6 && r#ref.chars().all(|ch| ch.is_ascii_hexdigit()) {
        let hit: Option<(String, String)> = c
            .query_row("select session_id, ts from commits where sha like ?1 order by ts desc limit 1",
                [format!("{ref}%")], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        if let Some((sid, ts)) = hit {
            let n: i64 = c.query_row(
                "select coalesce(max(n), 1) from turns where session_id = ?1 and ts <= ?2", params![sid, ts], |r| r.get(0))?;
            crate::db::log_usage(&c, "timeline", r#ref, 1);
            return turns_around(&c, o, &sid, n, before, after);
        }
    }
    let id = resolve_session(&c, r#ref)?;
    let (project, started): (String, String) = c.query_row(
        "select project, coalesce(started_at, '') from sessions where id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut rows: Vec<(String, String, Option<String>, i64)> = c.prepare(
        "select id, coalesce(started_at, ''), first_prompt, prompt_count from sessions
         where project = ?1 and prompt_count > 0 and started_at < ?2 order by started_at desc limit ?3")?
        .query_map(params![project, started, before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .flatten().collect();
    rows.reverse();
    let mut later: Vec<(String, String, Option<String>, i64)> = c.prepare(
        "select id, coalesce(started_at, ''), first_prompt, prompt_count from sessions
         where project = ?1 and prompt_count > 0 and started_at >= ?2 order by started_at asc limit ?3")?
        .query_map(params![project, started, after + 1], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .flatten().collect();
    rows.append(&mut later);
    crate::db::log_usage(&c, "timeline", r#ref, rows.len() as i64);
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(i, s, p, n)| json!({"id": i, "started_at": s, "first_prompt": p, "turns": n, "anchor": *i == id})).collect();
        println!("{}", json!({"project": project, "sessions": v}));
        return Ok(());
    }
    println!("# {project}: sessions around {}", short_id(&id));
    for (i, s, p, n) in rows {
        let mark = if i == id { ">" } else { " " };
        println!("{mark} {}  {}  {:>3} turns  {}", short_ts(&s), short_id(&i), n,
            clip(p.as_deref().map(first_line).unwrap_or(""), 80));
    }
    Ok(())
}

fn turns_around(c: &Connection, o: &Opts, id: &str, n: i64, before: i64, after: i64) -> Res {
    let rows: Vec<(i64, String, String, Option<String>, String)> = c.prepare(
        "select n, ts, prompt, reply, coalesce(files, '[]') from turns
         where session_id = ?1 and n between ?2 and ?3 order by n")?
        .query_map(params![id, n - before, n + after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .flatten().collect();
    if rows.is_empty() {
        return Err(format!("no turn {n} in session {}", short_id(id)).into());
    }
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(k, ts, p, r, f)| json!({"n": k, "ts": ts, "prompt": p, "reply": r,
            "files": serde_json::from_str::<Value>(f).unwrap_or(json!([])), "anchor": *k == n})).collect();
        println!("{}", json!({"id": id, "turns": v}));
        return Ok(());
    }
    println!("# {}: turns around {n}", short_id(id));
    for (k, ts, p, r, f) in rows {
        let mark = if k == n { ">" } else { " " };
        println!("{mark}{k:>3}  {}  > {}", short_ts(&ts).get(11..).unwrap_or(""), clip(first_line(&p), 100));
        let files: Vec<String> = serde_json::from_str(&f).unwrap_or_default();
        if !files.is_empty() {
            let short: Vec<&str> = files.iter().map(|x| x.rsplit('/').next().unwrap_or(x)).collect();
            println!("      files: {}", clip(&short.join(", "), 100));
        }
        if let Some(r) = r { println!("      < {}", clip(first_line(&r), 100)); }
    }
    Ok(())
}

pub fn resolve_session(c: &Connection, prefix: &str) -> Result<String, Box<dyn std::error::Error>> {
    let ids: Vec<String> = c.prepare("select id from sessions where id like ?1 order by started_at desc")?
        .query_map([format!("{prefix}%")], |r| r.get(0))?.flatten().collect();
    match ids.len() {
        0 => Err(format!("no session starting with '{prefix}'").into()),
        1 => Ok(ids.into_iter().next().unwrap()),
        n => Err(format!("{n} sessions start with '{prefix}'; give more characters").into()),
    }
}

/// `file <path>`: which past sessions edited this file, and why. With `--hook`, read Claude Code's
/// PreToolUse JSON on stdin and answer as `additionalContext`, at most three lines, once per session
/// and file, only when another session edited it. Always exit 0.
pub fn file(o: &Opts, path: Option<&str>) -> Res {
    let c = crate::db::open()?;
    if o.hook {
        let input: Value = read_stdin_json();
        let Some(p) = input.get("tool_input").and_then(|t| t.get("file_path")).and_then(Value::as_str) else { return Ok(()) };
        let session = input.get("session_id").and_then(Value::as_str).unwrap_or("");
        let p = absolute(p);
        let seen: i64 = c.query_row("select count(*) from injected where session_id = ?1 and path = ?2",
            params![session, p], |r| r.get(0)).unwrap_or(0);
        if seen > 0 {
            return Ok(());
        }
        let rows = edits_of(&c, &p, session, 3, false)?;
        if rows.is_empty() {
            return Ok(());
        }
        let _ = c.execute("insert or ignore into injected (session_id, path, ts) values (?1, ?2, ?3)",
            params![session, p, crate::cmds::now_iso()]);
        crate::db::log_usage(&c, "file-hook", &p, rows.len() as i64);
        let name = p.rsplit('/').next().unwrap_or(&p);
        let mut lines = vec![format!("gopola: {name} was edited in {} past session(s):", rows.len())];
        for (sid, ts, _n, prompt) in &rows {
            lines.push(format!("  {} {}  > {}", &ts[..10.min(ts.len())], short_id(sid), clip(first_line(prompt), 90)));
        }
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": lines.join("\n")}}));
        return Ok(());
    }
    let Some(p) = path else { return Err("file needs a path".into()) };
    let p = absolute(p);
    let rows = edits_of(&c, &p, "", o.n.max(1) as i64, o.reads)?;
    crate::db::log_usage(&c, "file", &p, rows.len() as i64);
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(s, ts, n, pr)| json!({"session": s, "ts": ts, "turn": n, "prompt": pr})).collect();
        println!("{}", json!({"path": p, "edits": v}));
        return Ok(());
    }
    if rows.is_empty() {
        println!("no past session edited {p}");
        return Ok(());
    }
    println!("# {p}");
    for (sid, ts, n, prompt) in rows {
        let mark = crate::cmds::harness_mark(crate::cmds::harness_of(&c, &sid)?.as_deref()).map(|h| format!(" {h}")).unwrap_or_default();
        println!("{}  {}:{n:<4}{mark} > {}", short_ts(&ts), short_id(&sid), clip(first_line(&prompt), 100));
    }
    Ok(())
}

/// (session, ts, turn, prompt) of turns that edited `path` (or also read it, with `reads`), newest
/// first, excluding `not_session`.
fn edits_of(c: &Connection, path: &str, not_session: &str, limit: i64, reads: bool) -> rusqlite::Result<Vec<(String, String, i64, String)>> {
    let mut st = c.prepare(
        "select t.session_id, t.ts, t.n, t.prompt from turn_files f join turns t
           on t.session_id = f.session_id and t.n = f.n
         where f.path = ?1 and t.session_id != ?2 and (f.kind = 'edit' or ?4 = 1) order by t.ts desc limit ?3")?;
    let rows = st.query_map(params![path, not_session, limit, reads as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .flatten().collect();
    Ok(rows)
}

fn absolute(p: &str) -> String {
    if p.starts_with('/') {
        return p.to_string();
    }
    std::env::current_dir().map(|d| d.join(p).to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string())
}

pub fn read_stdin_json() -> Value {
    use std::io::Read;
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// `tools`: which tools past sessions actually invoked, by call count and sessions using them.
pub fn tools(o: &Opts) -> Res {
    let c = crate::db::open()?;
    let project = o.project.clone().unwrap_or_else(crate::cmds::default_project);
    let since = o.since.clone().unwrap_or_default();
    let rows = if o.programs {
        program_rows(&c, &project, &since, o.n.max(1))?
    } else {
        let mut st = c.prepare(
            "select j.key, sum(j.value), count(distinct t.session_id)
             from turns t join sessions s on s.id = t.session_id, json_each(coalesce(t.tools, '{}')) j
             where (s.project = ?1 or ?1 = 'all') and t.ts >= ?2
             group by j.key order by 2 desc limit ?3")?;
        st.query_map(params![project, since, o.n.max(1) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten().collect()
    };
    crate::db::log_usage(&c, "tools", &project, rows.len() as i64);
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(t, n, s)| json!({"tool": t, "calls": n, "sessions": s})).collect();
        println!("{}", json!({"project": project, "tools": v}));
        return Ok(());
    }
    if rows.is_empty() {
        println!("no tool calls recorded for {project}; run `gopola index --full` once after upgrading");
        return Ok(());
    }
    println!("# {} in {project}: calls  sessions", if o.programs { "programs" } else { "tools" });
    for (t, n, s) in rows {
        println!("{n:>7}  {s:>4}  {t}");
    }
    Ok(())
}

/// `tools --programs`: each program the indexed Bash commands ran, by calls and sessions. A turn keeps a
/// repeated command once, so a call is a distinct command in a turn.
fn program_rows(c: &Connection, project: &str, since: &str, n: usize) -> rusqlite::Result<Vec<(String, i64, i64)>> {
    let mut st = c.prepare(
        "select t.session_id, t.commands from turns t join sessions s on s.id = t.session_id
         where (s.project = ?1 or ?1 = 'all') and t.ts >= ?2 and t.commands is not null")?;
    let mut count: std::collections::HashMap<String, (i64, std::collections::HashSet<String>)> = Default::default();
    for (sid, cmds) in st.query_map(params![project, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.flatten() {
        // The index keeps Skill tool calls among the commands, as `Skill <name> …`.
        for cmd in serde_json::from_str::<Vec<String>>(&cmds).unwrap_or_default().iter().filter(|c| !c.starts_with("Skill ")) {
            for p in programs(cmd) {
                let e = count.entry(p).or_default();
                e.0 += 1;
                e.1.insert(sid.clone());
            }
        }
    }
    let mut rows: Vec<(String, i64, i64)> = count.into_iter().map(|(p, (n, s))| (p, n, s.len() as i64)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows.truncate(n);
    Ok(rows)
}

/// The program each simple command in a shell command line runs: the first word after assignments and
/// wrappers (`sudo`, `env`, `xargs`…), path stripped. Heredoc bodies are skipped.
fn programs(cmd: &str) -> Vec<String> {
    const WRAPPERS: &[&str] = &["sudo", "env", "time", "command", "exec", "nohup", "xargs", "then", "do", "else",
        "if", "elif", "while", "until", "!", "{", "}"];
    let mut text = String::new();
    let mut heredoc_end: Option<String> = None;
    for line in cmd.lines() {
        if let Some(end) = &heredoc_end {
            if line.trim() == end {
                heredoc_end = None;
            }
            continue;
        }
        if let Some(i) = line.find("<<") {
            let w = line[i + 2..].trim_start_matches('-').split_whitespace().next().unwrap_or("");
            heredoc_end = Some(w.trim_matches(|c| c == '\'' || c == '"').to_string());
        }
        text.push_str(line);
        text.push('\n');
    }
    // Split into simple commands at separators outside quotes; a quoted string is dropped, as it is never a
    // program name that matters here.
    let text = text.replace(">&", ">").replace("&>", ">");
    let mut segs = vec![String::new()];
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        match (quote, ch) {
            (Some(q), _) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '|' | ';' | '&' | '(' | ')' | '`' | '\n') => segs.push(String::new()),
            (None, _) => segs.last_mut().unwrap().push(ch),
        }
    }
    const KEYWORDS: &[&str] = &["for", "case", "esac", "done", "fi", "break", "continue", "function", "return"];
    let mut out = Vec::new();
    for seg in segs.iter().filter(|s| !s.split_whitespace().next().is_some_and(|w| KEYWORDS.contains(&w))) {
        let word = seg.split_whitespace()
            .find(|w| !w.contains('=') && !WRAPPERS.contains(w) && !w.starts_with('-'))
            .map(|w| w.trim_start_matches('$'));
        if let Some(w) = word.map(|w| w.rsplit('/').next().unwrap_or(w))
            && !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
        {
            out.push(w.to_string());
        }
    }
    out
}

/// Codex reads the same matcher groups from `hooks.json` and from `[hooks]` in `config.toml`.
fn codex_hook_wired(home: &std::path::Path) -> bool {
    let wired = |v: Value| v["hooks"]["SessionStart"].as_array().is_some_and(|groups| groups.iter().any(|g| {
        g["hooks"].as_array().is_some_and(|hooks| hooks.iter().any(|h| {
            h["command"].as_str().is_some_and(|c| c.contains("gopola codex-hook"))
        }))
    }));
    let read = |name: &str| std::fs::read_to_string(home.join(name)).ok();
    read("hooks.json").and_then(|t| serde_json::from_str(&t).ok()).is_some_and(wired)
        || read("config.toml").and_then(|t| toml::from_str(&t).ok()).is_some_and(wired)
}

#[cfg(test)]
mod tests {
    use super::programs;

    #[test]
    fn programs_of_a_command_line() {
        assert_eq!(programs("cd ~/x && FOO=1 cargo test -q 2>&1 | tail -3; ~/.local/bin/board --version"),
            ["cd", "cargo", "tail", "board"]);
        assert_eq!(programs("x=$(git rev-parse HEAD); sudo env A=1 ledger status"), ["git", "ledger"]);
        assert_eq!(programs("python3 - <<'PY'\nimport os\nprint(1)\nPY\nwt list"), ["python3", "wt"]);
        assert_eq!(programs("cargo test 2>&1 | grep -E 'test result|FAILED'; python3 -c \"import sys\nprint(1)\""),
            ["cargo", "grep", "python3"]);
        assert_eq!(programs("for f in a b; do wc -l $f; done"), ["wc"]);
    }
}

/// `doctor`: is the index current, are the hooks wired, and what did gopola fail to answer lately.
pub fn doctor(o: &Opts) -> Res {
    let c = crate::db::open()?;
    let mut findings: Vec<(&str, String)> = Vec::new();

    let stale = stale_transcripts(&c);
    findings.push(if stale == 0 { ("ok", "index is current".into()) }
        else { ("fix", format!("{stale} transcript(s) changed since the last index: run `gopola index`")) });

    // Claude Code's folder: CLAUDE_CONFIG_DIR when set, as install.sh links the skill.
    let config = std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("{}/.claude", std::env::var("HOME").unwrap_or_default()));
    let settings: Value = std::fs::read_to_string(format!("{config}/settings.json")).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    // Each hook is looked for under its own event. SessionEnd runs two of gopola's hooks; the line names the first one missing.
    for (event, needles) in [("SessionStart", &["gopola index --quiet && "][..]), ("UserPromptSubmit", &["gopola index --quiet"][..]),
                             ("PreToolUse", &["gopola file --hook"][..]), ("PostToolUse", &["gopola index --quiet"][..]),
                             ("Stop", &["gopola summarize --auto"][..]),
                             ("SessionEnd", &["gopola summarize --auto --final", "gopola embed --quiet"][..])] {
        let commands: Vec<&str> = settings["hooks"][event].as_array().into_iter().flatten()
            .flat_map(|g| g["hooks"].as_array().into_iter().flatten()).filter_map(|h| h["command"].as_str()).collect();
        findings.push(match needles.iter().find(|n| !commands.iter().any(|c| c.contains(*n))) {
            None => ("ok", format!("{event} hook wired")),
            Some(needle) => ("fix", format!("{event} hook missing: `{needle}` is not in {config}/settings.json")),
        });
    }

    let skill = format!("{config}/skills/gopola/SKILL.md");
    findings.push(if std::path::Path::new(&skill).exists() { ("ok", format!("gopola skill installed ({config}/skills/gopola)")) }
        else { ("fix", format!("gopola skill missing: run install.sh (it symlinks skills/gopola into {config}/skills)")) });

    if crate::index::copilot_root().is_dir() {
        // `<COPILOT_HOME>/hooks`, beside `session-state`.
        let hooks = crate::index::copilot_root().with_file_name("hooks");
        let wired = std::fs::read_dir(&hooks).into_iter().flatten().flatten()
            .filter(|f| f.path().extension().is_some_and(|x| x == "json"))
            .any(|f| std::fs::read_to_string(f.path()).is_ok_and(|t| t.contains("gopola copilot-hook")));
        findings.push(if wired { ("ok", "Copilot sessionStart hook wired".into()) }
            else { ("fix", format!("Copilot sessionStart hook missing: no *.json in {} holds `gopola copilot-hook`", hooks.display())) });
        let files: Vec<(std::path::PathBuf, String)> = std::fs::read_dir(crate::index::copilot_root()).into_iter().flatten().flatten()
            .map(|d| (d.path().join("events.jsonl"), d.file_name().to_string_lossy().into_owned())).collect();
        findings.extend(harness_findings(&c, "Copilot", crate::copilot::KNOWN_TYPES, crate::copilot::classify, &files)?);
    }
    let codex = crate::index::codex_home();
    if codex.is_dir() {
        // Codex takes hooks from `hooks.json` beside `config.toml`, or from `[hooks]` in `config.toml`.
        let wired = codex_hook_wired(&codex);
        findings.push(if wired { ("ok", "Codex SessionStart hook wired".into()) }
            else { ("fix", format!("Codex SessionStart hook missing: neither hooks.json nor config.toml in {} holds `gopola codex-hook`", codex.display())) });
        // Rollouts Codex still keeps, plain or as its own `.zst`; one Codex deleted, kept only in gopola's archive, is left out.
        let files: Vec<_> = crate::index::codex_rollouts().into_iter().filter(|(p, _)| crate::archive::live(p)).collect();
        findings.extend(harness_findings(&c, "Codex", crate::codex::KNOWN_TYPES, crate::codex::classify, &files)?);
    }

    let e = crate::embed::Embedder::from_env();
    let (total, have): (i64, i64) = c.query_row(
        "select (select count(*) from turns),
                (select count(*) from vectors v join turns t on t.session_id = v.session_id and t.n = v.n where v.model = ?1)",
        [&e.model], |r| Ok((r.get(0)?, r.get(1)?)))?;
    // A session still open (its last event under 30 minutes ago) is embedded by its SessionEnd hook, not by hand.
    let open_cutoff = crate::cmds::iso_from_secs(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0).saturating_sub(1800));
    let open_missing: i64 = c.query_row(
        "select count(*) from turns t join sessions s on s.id = t.session_id
         where s.ended_at >= ?2 and not exists
           (select 1 from vectors v where v.session_id = t.session_id and v.n = t.n and v.model = ?1)",
        params![e.model, open_cutoff], |r| r.get(0))?;
    let missing = total - have - open_missing;
    let open_note = if open_missing > 0 { format!(" ({open_missing} in a session still open; its SessionEnd hook embeds them)") } else { String::new() };
    findings.push(if missing <= 0 { ("ok", format!("meaning search: {have} of {total} turns embedded with {}{open_note}", e.model)) }
        else { ("look", format!("meaning search: {missing} turn(s) have no vector for {}: `gopola embed`{open_note}", e.model)) });
    findings.push(match e.embed_quick(&["ok".to_string()]) {
        Ok(_) => ("ok", format!("ollama answers at {} with {}", e.base, e.model)),
        Err(err) => ("fix", err.to_string()),
    });
    // What the session-start block costs for this project, in lines and characters.
    let project = crate::cmds::default_project();
    if let Ok(out) = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .args(["recent", "--project", &project]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let (l, ch) = (s.lines().count(), s.chars().count());
        findings.push(("info", format!("session-start block for {project}: {l} lines, {ch} chars (~{} tokens)", ch / 4)));
    }
    let cutoff = days_ago(30);
    let empty: Vec<String> = c.prepare(
        "select distinct arg from usage where cmd = 'search' and hits = 0 and ts >= ?1 order by ts desc limit 10")?
        .query_map([&cutoff], |r| r.get(0))?.flatten().collect();
    findings.push(if empty.is_empty() { ("ok", "no empty searches in 30 days".into()) }
        else { ("look", format!("{} search(es) found nothing in 30 days: {}", empty.len(),
            empty.iter().map(|q| format!("'{q}'")).collect::<Vec<_>>().join(", "))) });

    let long: i64 = c.query_row(
        "select count(*) from sessions where prompt_count >= ?1 and summary is null and started_at >= ?2",
        params![crate::cmds::LONG_SESSION_TURNS, cutoff], |r| r.get(0))?;
    findings.push(if long == 0 { ("ok", "no long session without a summary in 30 days".into()) }
        else { ("look", format!("{long} long session(s) in 30 days have no summary: `gopola recent` tags them; `gopola summarize <id>`")) });

    let used: Vec<(String, i64)> = c.prepare(
        "select cmd, count(*) from usage where ts >= ?1 group by cmd order by 2 desc")?
        .query_map([&cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    findings.push(("info", if used.is_empty() { "no usage recorded yet".into() } else {
        format!("used in 30 days: {}", used.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")) }));

    let (sessions, turns, commits, obs): (i64, i64, i64, i64) = c.query_row(
        "select (select count(*) from sessions where prompt_count > 0), (select count(*) from turns),
                (select count(*) from commits), (select count(*) from observations)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let size = std::fs::metadata(crate::db::db_path()).map(|m| m.len() / 1_000_000).unwrap_or(0);
    findings.push(("info", format!("{sessions} sessions, {turns} turns, {commits} commits, {obs} imported observations, {size} MB")));

    if o.json {
        let v: Vec<Value> = findings.iter().map(|(k, m)| json!({"level": k, "finding": m})).collect();
        println!("{}", json!(v));
    } else {
        for (k, m) in &findings { println!("{k:<5} {m}"); }
    }
    Ok(())
}

/// What `doctor` says of a harness gopola reads beside Claude Code (`name` is how it is shown; its `sessions.harness`
/// is the lower-case form): the record types it could not place, the lines it could not read, and the versions its
/// sessions ran. `files` are (transcript, session id).
fn harness_findings(c: &Connection, name: &str, known: &[&str], classify: fn(&str) -> crate::transcript::Parsed,
                    files: &[(std::path::PathBuf, String)]) -> rusqlite::Result<Vec<(&'static str, String)>> {
    let harness = name.to_lowercase();
    let mut findings = Vec::new();
    // `index` keeps each record type it could not place, with the first session that held it; one gopola has since
    // learned is left out.
    let mut unknown: serde_json::Map<String, Value> = c.query_row("select v from meta where k = ?1", [format!("{harness}_unknown_types")], |r| r.get::<_, String>(0)).ok()
        .and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
    unknown.retain(|t, _| !known.contains(&t.as_str()));
    findings.push(if unknown.is_empty() { ("ok", format!("{name} record types gopola does not know: none")) }
        else { ("look", format!("{name} record types gopola does not know: {}", unknown.iter()
            .map(|(t, id)| format!("{t} (in {})", short_id(id.as_str().unwrap_or_default()))).collect::<Vec<_>>().join(", "))) });
    // A field renamed inside a known type makes its lines unreadable, and `index` skips them silently. A last line
    // with no newline yet is still being written.
    let mut unreadable: Vec<(String, usize)> = files.iter().filter_map(|(p, id)| {
        let bytes = crate::archive::read(p).ok()?;
        let n = String::from_utf8_lossy(&bytes).split_inclusive('\n').filter(|l| l.ends_with('\n') && !l.trim().is_empty())
            .filter(|l| classify(l.trim_end()) == crate::transcript::Parsed::Unreadable).count();
        (n > 0).then(|| (id.clone(), n))
    }).collect();
    unreadable.sort();
    findings.push(if unreadable.is_empty() { ("ok", format!("{name} lines gopola cannot read: none")) }
        else { ("look", format!("{name} lines gopola cannot read: {} (a field gopola reads may have been renamed)",
            unreadable.iter().map(|(id, n)| format!("{n} in {}", short_id(id))).collect::<Vec<_>>().join(", "))) });
    // A session keeps the version it started on: a resumed one is counted there.
    let versions: Vec<(String, i64)> = c.prepare(
        "select coalesce(harness_version, 'unknown'), count(*) from sessions where harness = ?1 group by 1 order by 2 desc, 1")?
        .query_map([&harness], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    let list = versions.iter().map(|(v, k)| format!("{v} ×{k}")).collect::<Vec<_>>().join(", ");
    findings.push(("info", format!("{name} sessions: {}{}", versions.iter().map(|(_, k)| k).sum::<i64>(),
        if versions.is_empty() { String::new() } else { format!(" (versions {list})") })));
    Ok(findings)
}

/// Transcripts whose size differs from what the index recorded (cheap: one stat per file).
fn stale_transcripts(c: &Connection) -> usize {
    let known: std::collections::HashMap<String, i64> = c.prepare("select path, bytes_indexed from sessions")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    // A session that was imported is indexed from the import, never from Claude Code's own copy of it.
    let imports = crate::index::imports_root();
    let imported: std::collections::HashSet<String> = known.keys().map(std::path::Path::new)
        .filter(|p| p.starts_with(&imports))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).collect();
    let mut stale = std::fs::read_dir(crate::index::copilot_root()).into_iter().flatten().flatten()
        .map(|d| d.path().join("events.jsonl")).filter_map(|p| Some((p.metadata().ok()?.len() as i64, p)))
        .filter(|(size, p)| known.get(&p.to_string_lossy().to_string()) != Some(size))
        // A folder the index skips (no row, and no `session.start` to place it) is not waiting for `gopola index`.
        .filter(|(_, p)| p.parent().and_then(|d| d.file_name())
            .is_some_and(|id| crate::index::launch_dir(c, &id.to_string_lossy(), p, &[], crate::copilot::classify).is_some()))
        .count();
    // Codex compresses quiet rollouts; compare their decompressed size and leave gopola's deleted-session copies out.
    stale += crate::index::codex_rollouts().into_iter()
        .filter(|(p, _)| crate::archive::live(p))
        .filter_map(|(p, id)| Some((crate::archive::size(&p)? as i64, p, id)))
        .filter(|(size, p, _)| known.get(&p.to_string_lossy().to_string()) != Some(size))
        .filter(|(_, p, id)| crate::index::launch_dir(c, id, p, &[], crate::codex::classify).is_some())
        .count();
    let Ok(dirs) = std::fs::read_dir(crate::index::projects_root()) else { return stale };
    for d in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            if p.file_stem().is_some_and(|s| imported.contains(&*s.to_string_lossy())) { continue; }
            let size = f.metadata().map(|m| m.len() as i64).unwrap_or(0);
            if known.get(&p.to_string_lossy().to_string()).copied() != Some(size) { stale += 1; }
        }
    }
    stale
}

fn days_ago(n: u64) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    crate::cmds::iso_from_secs(secs.saturating_sub(n * 86_400))
}
