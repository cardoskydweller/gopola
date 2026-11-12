//! `dispatches <session>`: each Agent and SendMessage call a session made, with its time, its description or
//! summary, the agent it reached, whether a hand-back came, and the whole prompt — to re-dispatch or resume a
//! session's stopped agents without a hand-written `jq` over the transcript.
//!
//! Shapes counted over 222 main transcripts on 2026-11-06: `Agent` (1,781 calls) takes `description`, `prompt`,
//! and often `model`, `subagent_type`, `run_in_background`; its result's `toolUseResult.agentId` names the agent
//! (1,778), else (38 older launches) an `agentId: <id>` line in the content; a string result is a refusal (a
//! permission denial, a call stopped before it ran). `SendMessage` (945) takes `to` (= `recipient`), `summary`,
//! `recipient_kind` (agent 747, none 97, name 65, session 36) and `message`, the whole text (`content` is a cut
//! copy); its result may name `resumedAgentId` or `revivedAgentId`.

use crate::cmds::{Opts, Res, short_id};
use crate::transcript::redact_secrets;
use serde_json::{Value, json};
use std::collections::HashMap;

struct Dispatch {
    ts: String,
    tool: &'static str,
    label: String,
    to: Option<String>,
    kind: Option<String>,
    model: Option<String>,
    agent_type: Option<String>,
    background: bool,
    agent: Option<String>,
    outcome: String,
    prompt: String,
    /// (time, `returned` or `stopped`) of the first hand-back after this call, before the next call to the agent.
    handback: Option<(String, &'static str)>,
}

fn s(v: &Value, p: &str) -> Option<String> {
    v.pointer(p).and_then(Value::as_str).map(str::to_string)
}

/// The text of a tool result's content, a string or a list of text blocks.
fn content_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// `2026-10-27T18:30:27.799Z` -> `2026-10-27 18:30:27Z`.
fn when(ts: &str) -> String {
    format!("{}Z", ts.chars().take(19).collect::<String>().replacen('T', " ", 1))
}

pub fn dispatches(o: &Opts, session: Option<&str>) -> Res {
    let Some(prefix) = session else { return Err("dispatches needs a session prefix".into()) };
    let c = crate::db::open()?;
    let id = crate::insight::resolve_session(&c, prefix)?;
    crate::cmds::claude_only(&c, &id, "dispatches")?;
    let path: String = c.query_row("select path from sessions where id = ?1", [&id], |r| r.get(0))?;
    let raw = crate::archive::read(std::path::Path::new(&path)).map_err(|e| format!("{path}: {e}"))?;
    let text = String::from_utf8_lossy(&raw);

    let mut list: Vec<Dispatch> = Vec::new();
    let mut open: HashMap<String, usize> = HashMap::new();
    for line in text.lines().filter(|l| l.contains("\"tool_use\"") || l.contains("tool_use_id")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let ts = s(&v, "/timestamp").unwrap_or_default();
        let kind = v.get("type").and_then(Value::as_str);
        for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            match (b.get("type").and_then(Value::as_str), kind) {
                (Some("tool_use"), Some("assistant")) => {
                    let i = b.get("input").unwrap_or(&Value::Null);
                    let d = match b.get("name").and_then(Value::as_str) {
                        Some("Agent") => Dispatch {
                            ts: ts.clone(), tool: "Agent", label: s(i, "/description").unwrap_or_default(), to: None, kind: None,
                            model: s(i, "/model"), agent_type: s(i, "/subagent_type"),
                            background: i.get("run_in_background").and_then(Value::as_bool) == Some(true),
                            agent: None, outcome: "no result".into(), prompt: s(i, "/prompt").unwrap_or_default(), handback: None,
                        },
                        Some("SendMessage") => {
                            let to = s(i, "/to").or_else(|| s(i, "/recipient"));
                            let kind = s(i, "/recipient_kind");
                            // an agent id is 17 characters starting with `a`; a name or a session is not one
                            let agent = to.clone().filter(|t| kind.as_deref().is_none_or(|k| k == "agent") && t.len() == 17 && t.starts_with('a'));
                            let prompt = s(i, "/message").or_else(|| s(i, "/content")).unwrap_or_default();
                            // with no summary (older calls), the message's first line, cut to 60 characters
                            let label = s(i, "/summary").filter(|x| !x.is_empty())
                                .unwrap_or_else(|| prompt.lines().next().unwrap_or("").chars().take(60).collect());
                            Dispatch {
                                ts: ts.clone(), tool: "SendMessage", label, to, kind,
                                model: None, agent_type: None, background: false, agent, outcome: "no result".into(),
                                prompt, handback: None,
                            }
                        }
                        _ => continue,
                    };
                    if let Some(u) = b.get("id").and_then(Value::as_str) {
                        open.insert(u.to_string(), list.len());
                    }
                    list.push(d);
                }
                (Some("tool_result"), Some("user")) => {
                    let Some(n) = b.get("tool_use_id").and_then(Value::as_str).and_then(|u| open.remove(u)) else { continue };
                    let d = &mut list[n];
                    let r = v.get("toolUseResult").unwrap_or(&Value::Null);
                    let body = content_text(b);
                    if let Value::String(t) = r {
                        d.outcome = format!("refused: {}", t.lines().next().unwrap_or(""));
                        continue;
                    }
                    if d.tool == "Agent" {
                        d.agent = s(r, "/agentId").or_else(|| {
                            let at = body.find("agentId: ")? + 9;
                            Some(body[at..].chars().take_while(char::is_ascii_alphanumeric).collect())
                        });
                        d.outcome = match (&d.agent, s(r, "/status")) {
                            (Some(_), Some(st)) => st.replace('_', " "),
                            (Some(_), None) => "async launched".into(),
                            (None, _) => format!("no agent: {}", body.lines().next().unwrap_or("")),
                        };
                    } else {
                        if let Some(a) = s(r, "/resumedAgentId").or_else(|| s(r, "/revivedAgentId")) {
                            d.agent = Some(a);
                        }
                        d.outcome = match r.get("success").and_then(Value::as_bool) {
                            Some(false) => format!("not sent: {}", s(r, "/message").unwrap_or_default()),
                            _ => "sent".into(),
                        };
                    }
                }
                _ => {}
            }
        }
    }

    // Every hand-back of each agent in this transcript, read as `gopola handback` reads them.
    let agents: Vec<String> = { let mut a: Vec<String> = list.iter().filter_map(|d| d.agent.clone()).collect(); a.sort(); a.dedup(); a };
    let mut backs: HashMap<&str, Vec<(String, &'static str)>> = HashMap::new();
    for line in text.lines() {
        let hits: Vec<&String> = agents.iter().filter(|a| line.contains(a.as_str())).collect();
        if hits.is_empty() { continue }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        for a in hits {
            if crate::handback::hand_back(&v, a).is_some() {
                let state = if line.contains("<status>killed</status>") { "stopped" } else { "returned" };
                backs.entry(a.as_str()).or_default().push((s(&v, "/timestamp").unwrap_or_default(), state));
            }
        }
    }
    for i in 0..list.len() {
        let Some(a) = list[i].agent.clone() else { continue };
        let from = list[i].ts.clone();
        let until = list[i + 1..].iter().find(|d| d.agent.as_deref() == Some(a.as_str())).map(|d| d.ts.clone());
        list[i].handback = backs.get(a.as_str()).and_then(|v| {
            v.iter().find(|(t, _)| *t >= from && until.as_ref().is_none_or(|u| t < u)).cloned()
        });
    }

    let needle = o.grep.as_deref();
    let shown: Vec<&Dispatch> = list.iter().filter(|d| needle.is_none_or(|n| d.prompt.contains(n) || d.label.contains(n))).collect();
    crate::db::log_usage(&c, "dispatches", &format!("{}{}", short_id(&id), needle.map(|g| format!(" --grep {g}")).unwrap_or_default()), shown.len() as i64);

    if o.json {
        let rows: Vec<Value> = shown.iter().map(|d| json!({
            "ts": d.ts, "tool": d.tool, "label": d.label, "to": d.to, "recipient_kind": d.kind, "model": d.model,
            "subagent_type": d.agent_type, "background": d.background, "agent": d.agent, "outcome": d.outcome,
            "handback": d.handback.as_ref().map(|(t, st)| json!({"ts": t, "state": st})), "prompt": redact_secrets(&d.prompt)})).collect();
        println!("{}", json!({"session": id, "transcript": path, "grep": needle, "dispatches": rows}));
        return Ok(());
    }
    let count = |t: &str| shown.iter().filter(|d| d.tool == t).count();
    let to_agents = shown.iter().filter(|d| d.agent.is_some()).count();
    let back = shown.iter().filter(|d| d.handback.is_some()).count();
    println!("# dispatches {}{}: {} Agent call(s), {} SendMessage call(s); a hand-back after {back} of the {to_agents} that reached an agent",
        short_id(&id), needle.map(|g| format!(" matching {g:?}")).unwrap_or_default(), count("Agent"), count("SendMessage"));
    for d in shown {
        let mut what: Vec<String> = Vec::new();
        what.extend(d.model.clone());
        what.extend(d.agent_type.clone());
        if d.background { what.push("background".into()) }
        if let (Some(k), "SendMessage") = (&d.kind, d.tool) { what.push(k.clone()) }
        let target = match (&d.agent, &d.to) {
            (Some(a), _) => format!("agent {a}"),
            (None, Some(t)) => t.clone(),
            (None, None) => "no agent".into(),
        };
        let extra = if what.is_empty() { String::new() } else { format!(" ({})", what.join(", ")) };
        let hb = match (&d.agent, &d.handback) {
            (_, Some((t, st))) => format!("hand-back: {st} {}", when(t).get(11..).unwrap_or("")),
            (Some(_), None) => "hand-back: none".into(),
            (None, None) => String::new(),
        };
        println!("\n{}  {}  {:?}  → {target}{extra}  {}  {hb}", when(&d.ts), d.tool, d.label, d.outcome);
        let prompt = redact_secrets(&d.prompt);
        let lines: Vec<&str> = prompt.lines().collect();
        let keep = o.head.unwrap_or(lines.len()).min(lines.len());
        for l in &lines[..keep] { println!("{l}") }
        if keep < lines.len() { println!("… {} more lines", lines.len() - keep) }
    }
    Ok(())
}
