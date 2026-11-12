//! `commands [<session>] [--grep <text>]`: the Bash calls a session and its subagents ran, each with its time, its
//! status and its whole output, read from the transcripts when asked (no table, no index pass).
//!
//! Shapes counted over 286 main and 1,927 subagent transcripts on 2026-11-05: a Bash result's content is a string;
//! with `is_error` it starts `Exit code N` (or a hook block, a permission refusal, a tool error); `toolUseResult`
//! may add `interrupted`, `returnCodeInterpretation`, `backgroundTaskId`, and `persistedOutputPath`, whose file holds
//! the whole output when the content is only a preview. The tool is `Bash`, or `bash` (15 calls, all in subagents).

use crate::cmds::{Opts, Res, short_id};
use crate::transcript::redact_secrets;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

struct Call {
    session: String,
    agent: Option<String>,
    ts: String,
    result_ts: Option<String>,
    status: String,
    exit: Option<i64>,
    command: String,
    output: String,
}

/// The Bash calls in one transcript. With a needle (an exact, case-sensitive substring of the command), a line is
/// parsed only when it holds the needle as JSON writes it, or the id of a call that matched and has no result yet.
fn calls_in(path: &Path, session: &str, agent: Option<&str>, needle: Option<&str>) -> Vec<Call> {
    let Ok(mut r) = crate::archive::open(path) else { return Vec::new() };
    let escaped = needle.map(|n| { let s = serde_json::to_string(n).unwrap_or_default(); s[1..s.len() - 1].to_string() });
    let mut calls: Vec<Call> = Vec::new();
    let mut open: HashMap<String, usize> = HashMap::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf).unwrap_or(0) == 0 { break }
        let line = String::from_utf8_lossy(&buf);
        let wanted = escaped.as_deref().is_none_or(|n| line.contains(n)) || open.keys().any(|id| line.contains(id.as_str()));
        if !wanted { continue }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        // Only the call as made and its result: `api-request-blob` records repeat earlier blocks.
        let kind = v.get("type").and_then(Value::as_str);
        for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") if kind == Some("assistant") && b.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case("bash")) => {
                    let (Some(id), Some(cmd)) = (b.get("id").and_then(Value::as_str), b.pointer("/input/command").and_then(Value::as_str)) else { continue };
                    if needle.is_some_and(|n| !cmd.contains(n)) { continue }
                    open.insert(id.to_string(), calls.len());
                    calls.push(Call { session: session.to_string(), agent: agent.map(str::to_string), ts: ts.to_string(), result_ts: None,
                        status: "no result".into(), exit: None, command: cmd.to_string(), output: String::new() });
                }
                Some("tool_result") if kind == Some("user") => {
                    let Some(i) = b.get("tool_use_id").and_then(Value::as_str).and_then(|id| open.remove(id)) else { continue };
                    let c = &mut calls[i];
                    c.result_ts = Some(ts.to_string());
                    (c.status, c.exit, c.output) = outcome(b, v.get("toolUseResult").unwrap_or(&Value::Null));
                }
                _ => {}
            }
        }
    }
    calls
}

/// (status, exit code, output) of one result. A persisted output is read from its file; when the file is gone,
/// the preview in the content is what there is.
fn outcome(b: &Value, r: &Value) -> (String, Option<i64>, String) {
    let s = |k: &str| r.get(k).and_then(Value::as_str);
    let content = b.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    let output = s("persistedOutputPath").and_then(|p| crate::archive::read(Path::new(p)).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or(content);
    if b.get("is_error").and_then(Value::as_bool) == Some(true) {
        let code = output.strip_prefix("Exit code ").map(|rest| rest.split_once('\n').unwrap_or((rest, "")));
        return match code.and_then(|(n, out)| Some((n.trim().parse::<i64>().ok()?, out))) {
            Some((n, out)) => (format!("exit {n}"), Some(n), out.to_string()),
            None => ("error".into(), None, output),
        };
    }
    let status = if r.get("interrupted").and_then(Value::as_bool) == Some(true) {
        "interrupted".into()
    } else if let Some(id) = s("backgroundTaskId") {
        format!("background {id}")
    } else if let Some(why) = s("returnCodeInterpretation") {
        format!("ok ({why})")
    } else {
        "ok".into()
    };
    (status, None, output)
}

/// `2026-10-27T18:30:27.799Z` -> `2026-10-27 18:30:27Z`.
fn when(ts: &str) -> String {
    format!("{}Z", ts.chars().take(19).collect::<String>().replacen('T', " ", 1))
}

/// A session's own transcript, and its subagents' (`<id>/subagents/agent-<agent>.jsonl`): (session, file, agent).
pub fn session_files(id: &str, path: &str) -> Vec<(String, PathBuf, Option<String>)> {
    let main = PathBuf::from(path);
    let agents: Vec<_> = crate::archive::list(&main.with_extension("").join("subagents")).into_iter()
        .filter_map(|p| {
            let agent = p.file_name()?.to_str()?.strip_prefix("agent-")?.strip_suffix(".jsonl")?.to_string();
            Some((id.to_string(), p, Some(agent)))
        }).collect();
    std::iter::once((id.to_string(), main, None)).chain(agents).collect()
}

pub fn commands(o: &Opts, session: Option<&str>) -> Res {
    let c = crate::db::open()?;
    let needle = o.grep.as_deref();
    let (scope, rows): (String, Vec<(String, String)>) = match (session, needle) {
        (Some(p), _) => {
            let id = crate::insight::resolve_session(&c, p)?;
            crate::cmds::claude_only(&c, &id, "commands")?;
            let path: String = c.query_row("select path from sessions where id = ?1", [&id], |r| r.get(0))?;
            (short_id(&id).to_string(), vec![(id, path)])
        }
        (None, Some(_)) => {
            let project = crate::cmds::project_filter(o);
            let rows = c.prepare(
                "select id, path from sessions where harness = 'claude' and (project = ?1 or ?1 = 'all') and started_at >= ?2 and started_at < ?3")?
                .query_map(rusqlite::params![project, o.since.clone().unwrap_or_default(), o.until.clone().unwrap_or_else(|| "9999".into())],
                    |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
            (project, rows)
        }
        (None, None) => return Err("commands needs a session or --grep <text>".into()),
    };
    let rows: Vec<(String, String)> = rows.into_iter().filter(|(_, p)| crate::archive::exists(Path::new(p))).collect();
    let files: Vec<(String, PathBuf, Option<String>)> = rows.iter().flat_map(|(id, path)| session_files(id, path)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut calls: Vec<Call> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads).map(|_| s.spawn(|| {
            let mut out = Vec::new();
            while let Some((id, p, a)) = files.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                out.extend(calls_in(p, id, a.as_deref(), needle));
            }
            out
        })).collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    calls.sort_by(|a, b| a.ts.cmp(&b.ts));
    crate::db::log_usage(&c, "commands", &format!("{scope}{}", needle.map(|g| format!(" --grep {g}")).unwrap_or_default()), calls.len() as i64);

    if o.json {
        let list: Vec<Value> = calls.iter().map(|k| json!({"session": k.session, "source": if k.agent.is_some() { "agent" } else { "session" },
            "agent": k.agent, "ts": k.ts, "result_ts": k.result_ts, "status": k.status, "exit": k.exit,
            "command": redact_secrets(&k.command), "output": redact_secrets(&k.output)})).collect();
        println!("{}", json!({"scope": scope, "grep": needle, "sessions_read": rows.len(), "commands": list}));
        return Ok(());
    }
    let agents = calls.iter().filter(|k| k.agent.is_some()).count();
    println!("# commands {scope}{}: {} Bash call(s), {agents} of them by subagents; {} session(s) read",
        needle.map(|g| format!(" matching {g:?}")).unwrap_or_default(), calls.len(), rows.len());
    for k in &calls {
        let source = k.agent.as_deref().map_or("session".to_string(), |a| format!("agent {a}"));
        let result = k.result_ts.as_deref().map(|t| format!("  → {}", when(t).get(11..).unwrap_or(""))).unwrap_or_default();
        println!("\n{}  {}  {source}  {}{result}\n$ {}", when(&k.ts), short_id(&k.session), k.status, redact_secrets(&k.command));
        let output = redact_secrets(&k.output);
        let lines: Vec<&str> = output.lines().collect();
        let keep = o.head.unwrap_or(lines.len()).min(lines.len());
        for l in &lines[..keep] { println!("{l}") }
        if keep < lines.len() { println!("… {} more lines", lines.len() - keep) }
    }
    Ok(())
}
