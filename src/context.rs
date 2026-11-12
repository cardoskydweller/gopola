//! `context <session>`: what a Copilot session's model was told — its system prompt, the instruction updates and the
//! context its hooks added — in session order, so a probe can check that a rule reached the prompt. Claude Code writes
//! no system prompt to its transcript; its hooks' context is in `gopola hooks`.

use crate::cmds::{short_id, Opts, Res};
use crate::transcript::redact_secrets;
use serde_json::{json, Value};
use std::io::BufRead;

struct Part {
    ts: String,
    kind: &'static str,
    name: String,
    text: String,
}

fn parts(r: impl BufRead) -> Vec<Part> {
    // split, not lines(): one line that is not UTF-8 must not end the read and turn later matches into a false 0
    r.split(b'\n').map_while(Result::ok).filter_map(|l| {
        let v: Value = serde_json::from_str(&String::from_utf8_lossy(&l)).ok()?;
        let s = |p: &str| v["data"].pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
        let (kind, name, text) = match v["type"].as_str()? {
            "system.message" => ("system", s("/role"), s("/content")),
            "instruction.update" => ("instruction", s("/name"), s("/content")),
            "hook.end" => ("hook", s("/hookType"), s("/output/additionalContext")),
            _ => return None,
        };
        if text.is_empty() { return None }
        Some(Part { ts: v["timestamp"].as_str().unwrap_or("").to_string(), kind, name, text: redact_secrets(&text) })
    }).collect()
}

pub fn context(o: &Opts, session: Option<&str>) -> Res {
    let Some(prefix) = session else { return Err("context needs a session prefix".into()) };
    let c = crate::db::open()?;
    let id = crate::insight::resolve_session(&c, prefix)?;
    let harness = crate::cmds::harness_of(&c, &id)?.unwrap_or_default();
    if harness != "copilot" {
        let hint = if harness == "claude" { ", whose transcript holds no system prompt (its hooks' context: `gopola hooks`)" } else { "" };
        return Err(format!("context reads Copilot sessions only; {} is a {} session{hint}", short_id(&id), crate::cmds::harness_name(&harness)).into());
    }
    let path: String = c.query_row("select path from sessions where id = ?1", [&id], |r| r.get(0))?;
    let parts = parts(crate::archive::open(std::path::Path::new(&path)).map_err(|e| format!("{path}: {e}"))?);
    let grep = o.grep.as_deref();
    let count = |p: &Part| grep.map_or(0, |g| p.text.matches(g).count());
    let total: usize = parts.iter().map(count).sum();
    crate::db::log_usage(&c, "context", &format!("{}{}", short_id(&id), grep.map(|g| format!(" --grep {g}")).unwrap_or_default()),
        if grep.is_some() { total } else { parts.len() } as i64);

    if o.json {
        let list: Vec<Value> = parts.iter().map(|p| {
            let mut v = json!({"ts": p.ts, "kind": p.kind, "name": p.name, "chars": p.text.chars().count()});
            match grep {
                Some(g) => { v["matches"] = json!(count(p)); v["lines"] = json!(p.text.lines().filter(|l| l.contains(g)).collect::<Vec<_>>()); }
                None => v["text"] = json!(p.text),
            }
            v
        }).collect();
        let mut out = json!({"session": id, "parts": list});
        if grep.is_some() { out["matches"] = json!(total) }
        println!("{out}");
        return Ok(());
    }
    let matches = |n: usize| format!("{n} match{}", if n == 1 { "" } else { "es" });
    for p in &parts {
        // `---`, not `#`: the system prompt's own headings start with `#`
        println!("--- {}  {} {}  {} chars", p.ts, p.kind, p.name, p.text.chars().count());
        match grep {
            Some(g) => {
                println!("  {}", matches(count(p)));
                for l in p.text.lines().filter(|l| l.contains(g)) { println!("  | {l}") }
            }
            None => println!("{}\n", p.text),
        }
    }
    if let Some(g) = grep { println!("{} for '{g}' in {} parts", matches(total), parts.len()) }
    Ok(())
}
