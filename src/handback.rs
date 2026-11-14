//! `handback <agent-id-prefix>`: the last report a subagent returned to the session that launched it,
//! as that session received it; with `--since <time>`, every report from that time on. The subagent's own transcript does not hold the hand-back reliably
//! (its last text can be "I'll wait for the notification"), so this reads the parent transcript:
//! `<projects>/<project>/<session>/subagents/agent-<id>.jsonl` names the parent `<project>/<session>.jsonl`.
//!
//! Four record shapes carry a hand-back, counted in the README:
//! - `peer_message`: a `user` record whose text holds `<agent-message from="<id>">`, a
//!   `[Subagent hand-back]` line, and the report with every line indented by two spaces.
//! - `queued_command`: an `attachment` record (`attachment.type` = `queued_command`) whose `prompt` is that
//!   same frame, or a `<task-notification>` as below; the harness uses it when the message arrived during a turn.
//!
//! One agent file can sit under two sessions: every parent is read, and the latest record by `timestamp` wins.
//! - `task_notification`: a `user` record whose text is `<task-notification>` with `<task-id>` = the agent
//!   and the report inline in `<result>` (older harness, or an agent that did not call SubagentHandback).
//! - `tool_result`: the result of a foreground `Agent` call, `toolUseResult.agentId` with the text in `content`.
//! A notification or tool result that only says the report "was delivered ... as a message" is skipped.

use crate::cmds::Opts;
use crate::index::projects_root;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

const DELIVERED: &str = "was delivered to you as a message from";

/// Agent id -> every session transcript that holds a file of it (a resumed or copied agent can sit under
/// two sessions), for every subagent file whose id starts with `prefix`.
fn find_agents(prefix: &str) -> BTreeMap<String, Vec<PathBuf>> {
    let mut found = BTreeMap::new();
    let dirs = |p: &PathBuf| crate::archive::list(p);
    for project in dirs(&projects_root()) {
        for session in dirs(&project) {
            for f in dirs(&session.join("subagents")) {
                let name = f.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let id = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl"));
                let sid = session.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if let Some(id) = id.filter(|id| id.starts_with(prefix)) {
                    found.entry(id.to_string()).or_insert_with(Vec::new).push(project.join(format!("{sid}.jsonl")));
                }
            }
        }
    }
    found
}

/// The report inside the last `<agent-message from="id">` frame of `s`, with the harness's two-space indent removed.
fn frame(s: &str, id: &str) -> Option<String> {
    let open = format!("<agent-message from=\"{id}\">\n");
    let rest = &s[s.rfind(&open)? + open.len()..];
    let mut lines = rest.lines();
    if !lines.next()?.starts_with("[Subagent hand-back]") { return None }
    let body: Vec<&str> = lines.take_while(|l| *l != "</agent-message>").map(|l| l.strip_prefix("  ").unwrap_or(l)).collect();
    Some(body.join("\n"))
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let from = s.find(open)? + open.len();
    Some(&s[from..from + s[from..].find(close)?])
}

/// The report in the `<result>` of a `<task-notification>` for agent `id`, unless it only points to a message.
fn notification(s: &str, id: &str) -> Option<String> {
    if between(s, "<task-id>", "</task-id>") != Some(id) { return None }
    let r = &s[s.find("<result>")? + 8..s.rfind("</result>")?];
    (!r.contains(DELIVERED)).then(|| r.to_string())
}

pub(crate) fn hand_back(v: &Value, id: &str) -> Option<(&'static str, String)> {
    let text = match v.get("type").and_then(Value::as_str)? {
        "attachment" if v.pointer("/attachment/type").and_then(Value::as_str) == Some("queued_command") => {
            let s = v.pointer("/attachment/prompt")?.as_str()?;
            let t = if s.starts_with("<task-notification>") { notification(s, id) } else { frame(s, id) };
            return t.map(|t| ("queued_command", t));
        }
        "user" => v,
        _ => return None,
    };
    if text.pointer("/toolUseResult/agentId").and_then(Value::as_str) == Some(id) {
        let t = text.pointer("/toolUseResult/content")?.as_array()?.iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
        return (!t.contains(DELIVERED)).then_some(("tool_result", t));
    }
    let s = text.pointer("/message/content")?.as_str()?;
    if s.starts_with("<task-notification>") {
        return notification(s, id).map(|t| ("task_notification", t));
    }
    frame(s, id).map(|t| ("peer_message", t))
}

/// Exit code: 0 printed one, 1 found none, 2 usage.
pub fn run(o: &Opts, prefix: Option<&str>) -> i32 {
    let Some(prefix) = prefix.map(|p| p.strip_prefix("agent-").unwrap_or(p)).filter(|p| !p.is_empty()) else {
        eprintln!("gopola: handback needs an agent id prefix");
        return 2;
    };
    let agents = find_agents(prefix);
    if agents.len() > 1 {
        let names: Vec<_> = agents.keys().take(5).cloned().collect();
        let more = if agents.len() > 5 { ", ..." } else { "" };
        eprintln!("gopola: '{prefix}' matches {} agents: {}{more}", agents.len(), names.join(", "));
        return 2;
    }
    let Some((id, parents)) = agents.into_iter().next() else {
        eprintln!("gopola: no subagent file agent-{prefix}*.jsonl under {}/*/*/subagents", projects_root().display());
        return 1;
    };
    // Every hand-back in every parent, ordered by the record's timestamp; in one file, the later line last.
    let mut all: Vec<(String, PathBuf, &'static str, String)> = Vec::new();
    for parent in &parents {
        let raw = crate::archive::read(parent).unwrap_or_default();
        for v in String::from_utf8_lossy(&raw).lines().filter(|l| l.contains(&id)).filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
            let Some((shape, text)) = hand_back(&v, &id) else { continue };
            let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
            all.push((ts, parent.clone(), shape, text));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let read = || parents.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    if let Some(since) = o.since.as_deref() {
        // a hand-back copied with its agent into a second session is one report, not two
        all.dedup_by(|a, b| a.0 == b.0 && a.3 == b.3);
        // UTC ISO times compare as text to the second: `06:19:22.400Z` is at or after `06:19:22Z`
        let second = |t: &str| t.split(['.', 'Z']).next().unwrap_or(t).to_string();
        let after: Vec<_> = all.into_iter().filter(|h| second(&h.0) >= second(since)).collect();
        if after.is_empty() {
            eprintln!("gopola: agent {id} has no hand-back at or after {since} in {}", read());
            return 1;
        }
        if o.json {
            let list: Vec<Value> = after.iter().map(|(ts, p, shape, text)| json!({"ts": ts, "transcript": p, "shape": shape, "text": text.trim_end()})).collect();
            println!("{}", json!({"agent": id, "handbacks": list}));
        } else {
            let parts: Vec<String> = after.iter().map(|(ts, _, _, text)| format!("--- hand-back {ts} ---\n{}", text.trim_end())).collect();
            println!("{}", parts.join("\n\n"));
        }
        return 0;
    }
    let Some((_, parent, shape, text)) = all.pop() else {
        eprintln!("gopola: agent {id} has no hand-back in {} (the agent may still be running)", read());
        return 1;
    };
    let text = text.trim_end();
    if o.json {
        println!("{}", json!({"agent": id, "transcript": parent, "shape": shape, "text": text}));
    } else {
        println!("{text}");
    }
    0
}
