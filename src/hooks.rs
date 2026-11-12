//! `hooks <session>`: every hook run a session and its subagents recorded, one row each — time, hook, status,
//! duration, command — with what it said (stderr, a block's reason, a system message) and, on request, its output.
//!
//! Shapes counted over every transcript on disk on 2026-11-10: attachments `hook_success` (577,011),
//! `async_hook_response` (154,648, no command), `hook_additional_context` (3,376, content a list, no command),
//! `hook_system_message` (2,279), `hook_cancelled` (35, `timedOut`), `hook_non_blocking_error` (18). A block leaves no
//! attachment: the tool result's content is `<Event>:<Tool> hook error: [<command>]: <stderr>` (3,669), and a Stop
//! hook's is a user record `Stop hook feedback:\n[<command>]: <stderr>` (11). `stop_hook_summary` repeats the Stop
//! hooks' own `hook_success` rows and is not read.

use crate::cmds::{Opts, Res, short_id};
use crate::transcript::redact_secrets;
use serde_json::{Value, json};
use std::io::BufRead;
use std::path::Path;

struct Run {
    session: String,
    agent: Option<String>,
    ts: String,
    hook: String,
    status: String,
    ms: Option<i64>,
    command: String,
    /// Always shown: stderr, a block's reason, a system message.
    message: String,
    /// Shown with --head: stdout or the context a hook added.
    output: String,
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().or_else(|| x.get("text").and_then(Value::as_str))).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A command hook's block reads `[<command>]: <stderr>` -> (command, stderr). A hook that blocks with a JSON reason
/// has no command in the record, and its reason may itself start with a bracket (`[Fact-Forcing Gate]\n\n…`,
/// `[Fact-Forcing Gate] (denial #4 …`): (no command, the whole reason).
fn bracketed(rest: &str) -> (String, String) {
    match rest.strip_prefix('[').and_then(|r| r.split_once("]: ")).filter(|(command, _)| !command.contains('\n')) {
        Some((command, reason)) => (command.to_string(), reason.to_string()),
        None => (String::new(), rest.to_string()),
    }
}

fn runs_in(path: &Path, session: &str, agent: Option<&str>) -> Vec<Run> {
    let Ok(mut r) = crate::archive::open(path) else { return Vec::new() };
    let mut runs = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf).unwrap_or(0) == 0 { break }
        let line = String::from_utf8_lossy(&buf);
        if !line.contains("hook") { continue }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        let run = |hook: String, status: String, ms: Option<i64>, command: String, message: String, output: String| Run {
            session: session.to_string(), agent: agent.map(str::to_string), ts: ts.clone(), hook, status, ms, command, message, output };
        match v.get("type").and_then(Value::as_str) {
            Some("attachment") => {
                let a = &v["attachment"];
                let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let exit = a.get("exitCode").and_then(Value::as_i64);
                let ms = a.get("durationMs").and_then(Value::as_i64);
                let status = |x: Option<i64>| match x { Some(0) | None => "ok".to_string(), Some(n) => format!("exit {n}") };
                let stdout = if s("stdout").is_empty() { text(a.get("content")) } else { s("stdout") };
                let found = match a.get("type").and_then(Value::as_str) {
                    Some("hook_success" | "hook_non_blocking_error" | "async_hook_response") =>
                        run(s("hookName"), status(exit), ms, s("command"), s("stderr"), stdout),
                    Some("hook_cancelled") => {
                        let status = if a.get("timedOut").and_then(Value::as_bool) == Some(true) { "timed out" } else { "cancelled" };
                        run(s("hookName"), status.into(), ms, s("command"), String::new(), String::new())
                    }
                    Some("hook_system_message") => run(s("hookName"), "message".into(), None, String::new(), text(a.get("content")), String::new()),
                    Some("hook_additional_context") => run(s("hookName"), "context".into(), None, String::new(), String::new(), text(a.get("content"))),
                    _ => continue,
                };
                runs.push(found);
            }
            Some("user") => {
                // A block in an error tool result, or a Stop hook's feedback as a record of its own.
                let blocks: Vec<(String, String)> = match v.pointer("/message/content") {
                    Some(Value::Array(a)) => a.iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result") && b.get("is_error").and_then(Value::as_bool) == Some(true))
                        .filter_map(|b| text(b.get("content")).split_once(" hook error: ").filter(|(h, _)| !h.contains(char::is_whitespace)).map(|(h, r)| (h.to_string(), r.to_string())))
                        .collect(),
                    Some(Value::String(t)) => t.strip_prefix("Stop hook feedback:\n").map(|r| ("Stop".to_string(), r.to_string())).into_iter().collect(),
                    _ => Vec::new(),
                };
                for (hook, rest) in blocks {
                    let (command, reason) = bracketed(&rest);
                    runs.push(run(hook, "blocked".into(), None, command, reason, String::new()));
                }
            }
            _ => {}
        }
    }
    runs
}

fn when(ts: &str) -> String {
    format!("{}Z", ts.chars().take(19).collect::<String>().replacen('T', " ", 1))
}

fn duration(ms: Option<i64>) -> String {
    match ms {
        Some(n) if n >= 1000 => format!("{:.1}s", n as f64 / 1000.0),
        Some(n) => format!("{n}ms"),
        None => "-".into(),
    }
}

pub fn hooks(o: &Opts, session: Option<&str>) -> Res {
    let Some(prefix) = session else { return Err("hooks needs a session prefix".into()) };
    let c = crate::db::open()?;
    let id = crate::insight::resolve_session(&c, prefix)?;
    crate::cmds::claude_only(&c, &id, "hooks")?;
    let path: String = c.query_row("select path from sessions where id = ?1", [&id], |r| r.get(0))?;
    let mut runs: Vec<Run> = crate::commands::session_files(&id, &path).iter().flat_map(|(s, p, a)| runs_in(p, s, a.as_deref())).collect();
    if let Some(g) = o.grep.as_deref() { runs.retain(|r| r.command.contains(g) || r.hook.contains(g)) }
    runs.sort_by(|a, b| a.ts.cmp(&b.ts));
    crate::db::log_usage(&c, "hooks", &format!("{}{}", short_id(&id), o.grep.as_deref().map(|g| format!(" --grep {g}")).unwrap_or_default()), runs.len() as i64);

    if o.json {
        let list: Vec<Value> = runs.iter().map(|r| json!({"session": r.session, "source": if r.agent.is_some() { "agent" } else { "session" },
            "agent": r.agent, "ts": r.ts, "hook": r.hook, "status": r.status, "duration_ms": r.ms, "command": redact_secrets(&r.command),
            "message": redact_secrets(&r.message), "output": redact_secrets(&r.output)})).collect();
        println!("{}", json!({"session": id, "grep": o.grep, "hooks": list}));
        return Ok(());
    }
    let agents = runs.iter().filter(|r| r.agent.is_some()).count();
    let not_ok = runs.iter().filter(|r| !matches!(r.status.as_str(), "ok" | "context" | "message")).count();
    println!("# hooks {}{}: {} hook run(s), {agents} by subagents, {not_ok} not ok (exit, blocked, timed out, cancelled)",
        short_id(&id), o.grep.as_deref().map(|g| format!(" matching {g:?}")).unwrap_or_default(), runs.len());
    for r in &runs {
        let source = r.agent.as_deref().map_or("session".to_string(), |a| format!("agent {a}"));
        // The table shows a command's first line, cut at 160 characters; --json has it whole.
        let first = redact_secrets(r.command.lines().next().unwrap_or(""));
        let cmd = if first.is_empty() { "-".to_string() }
            else if first.chars().count() > 160 || r.command.contains('\n') { format!("{}…", first.chars().take(160).collect::<String>()) } else { first };
        println!("{}  {}  {source}  {}  {}  {}  {cmd}", when(&r.ts), short_id(&r.session), r.hook, r.status, duration(r.ms));
        for l in redact_secrets(&r.message).lines() { println!("  ! {l}") }
        if let Some(n) = o.head {
            let output = redact_secrets(&r.output);
            let lines: Vec<&str> = output.lines().collect();
            for l in &lines[..n.min(lines.len())] { println!("  > {l}") }
            if n < lines.len() { println!("  … {} more lines", lines.len() - n) }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bracketed;

    /// The forms seen in session 49973e08 (2026-11-10): a command hook's exit 2, and the two JSON reasons of one gate.
    #[test]
    fn a_bracket_names_a_command_only_before_a_colon() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(bracketed("[$HOME/.local/bin/zshguard hook]: refused: see [1]: the rule"), s("$HOME/.local/bin/zshguard hook", "refused: see [1]: the rule"));
        assert_eq!(bracketed("[Fact-Forcing Gate]\n\nBefore the first Bash command"), s("", "[Fact-Forcing Gate]\n\nBefore the first Bash command"));
        assert_eq!(bracketed("[Fact-Forcing Gate] (denial #4 this session) First edit"), s("", "[Fact-Forcing Gate] (denial #4 this session) First edit"));
        assert_eq!(bracketed("[Gate]\n\nstep [2]: done"), s("", "[Gate]\n\nstep [2]: done"), "a ]: after a newline is in the reason");
    }
}
