//! `weight <session>`: how big the context was on each model call, and what put it there.
//!
//! Measured from the transcript's own records, never estimated from file size: the context of a
//! call is the `usage` Claude Code logged for it (input + cache creation + cache read), and one API
//! message spans several assistant records, so calls are counted once per `message.id`. Byte sizes
//! appear only to attribute that context to its sources (tool results, incoming prompts), at about
//! four bytes a token; images are counted, not sized, at the per-image cap.

use crate::authored::epoch;
use crate::cmds::{Opts, Res, short_id, short_ts};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::process::Command;

/// The most one image costs on current models (2576 px long edge), from the API's vision docs.
const IMAGE_TOKENS_MAX: u64 = 4784;

#[derive(Default)]
struct Origin { turns: u64, calls: u64, processed: u64, prompt_bytes: u64 }

#[derive(Default)]
struct Tool { calls: u64, bytes: u64, images: u64 }

/// No record for this long before a compaction means the session sat idle. Across the 29 main-transcript
/// compactions of 2026-10-07..11-04 with a parent record, the quiet before one was 1-9 s or 50m 26s-53m 54s.
const IDLE_SECS: i64 = 300;

/// `quiet`: seconds from the record before it (`logicalParentUuid`) to the start of the compaction.
struct Compaction { ts: String, trigger: String, pre: u64, post: u64, duration_ms: u64, quiet: Option<i64> }

impl Compaction {
    fn idle(&self) -> bool { self.quiet.is_some_and(|q| q >= IDLE_SECS) }

    fn line(&self) -> String {
        let quiet = match self.quiet {
            Some(q) => format!("{} quiet before it", dur(q)),
            None => "quiet before it unknown (its parent record is not in this transcript)".into(),
        };
        format!("{}  {}  {} → {}  {quiet}, {} to compact{}", short_ts(&self.ts), self.trigger, tok(self.pre), tok(self.post),
            dur((self.duration_ms / 1000) as i64), if self.idle() { "  idle" } else { "" })
    }
}

fn dur(s: i64) -> String {
    match s {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

#[derive(Default)]
struct Weight {
    first_ts: String,
    last_ts: String,
    contexts: Vec<u64>,
    output: u64,
    origins: BTreeMap<String, Origin>,
    tools: HashMap<String, Tool>,
    compactions: Vec<Compaction>,
    hook_bytes: u64,
    prompt_images: u64,
}

fn text_len(content: &Value) -> (u64, u64) {
    match content {
        Value::String(s) => (s.len() as u64, 0),
        Value::Array(blocks) => blocks.iter().fold((0, 0), |(b, i), x| match x.get("type").and_then(Value::as_str) {
            Some("image") => (b, i + 1),
            Some("text") => (b + x.get("text").and_then(Value::as_str).map_or(0, |t| t.len() as u64), i),
            Some("tool_result") => { let (tb, ti) = text_len(x.get("content").unwrap_or(&Value::Null)); (b + tb, i + ti) }
            _ => (b, i),
        }),
        _ => (0, 0),
    }
}

/// What started this prompt: Claude Code's own `turnOrigin`, else (older transcripts) a guess
/// from the text, else none (meta records, tool results).
fn origin_of(v: &Value) -> Option<String> {
    if let Some(o) = v.get("turnOrigin").and_then(Value::as_str) { return Some(o.to_string()) }
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) { return None }
    match v.pointer("/message/content") {
        Some(Value::String(s)) if s.starts_with("<task-notification>") => Some("task_notification".into()),
        Some(Value::String(_)) => Some("human".into()),
        _ => None,
    }
}

fn measure(r: impl BufRead) -> Weight {
    let mut w = Weight::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut stamped: HashMap<String, i64> = HashMap::new();
    let mut current = "before the first prompt".to_string();
    for line in r.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) { continue }
        // Earliest and latest, not first and last: a teleported copy opens with records stamped when it was copied.
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            if w.first_ts.is_empty() || ts < w.first_ts.as_str() { w.first_ts = ts.to_string() }
            if ts > w.last_ts.as_str() { w.last_ts = ts.to_string() }
            if let (Some(u), Some(t)) = (v.get("uuid").and_then(Value::as_str), epoch(ts)) { stamped.insert(u.to_string(), t); }
        }
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let (Some(id), Some(n)) = (b.get("id").and_then(Value::as_str), b.get("name").and_then(Value::as_str)) {
                            names.insert(id.to_string(), n.to_string());
                        }
                    }
                }
                let (Some(u), Some(id)) = (v.pointer("/message/usage"), v.pointer("/message/id").and_then(Value::as_str)) else { continue };
                if !seen.insert(id.to_string()) { continue }
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let ctx = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                w.contexts.push(ctx);
                w.output += n("output_tokens");
                let o = w.origins.entry(current.clone()).or_default();
                o.calls += 1;
                o.processed += ctx;
            }
            Some("user") => {
                let content = v.pointer("/message/content").unwrap_or(&Value::Null);
                if let Some(blocks) = content.as_array() {
                    for b in blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")) {
                        let name = b.get("tool_use_id").and_then(Value::as_str).and_then(|i| names.get(i)).cloned().unwrap_or_else(|| "?".into());
                        let (bytes, images) = text_len(b.get("content").unwrap_or(&Value::Null));
                        let t = w.tools.entry(name).or_default();
                        t.calls += 1;
                        t.bytes += bytes;
                        t.images += images;
                    }
                }
                if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) { continue }
                let Some(o) = origin_of(&v) else { continue };
                if blocks_are_tool_results(content) { continue }
                let (bytes, images) = text_len(content);
                w.prompt_images += images;
                current = o;
                let e = w.origins.entry(current.clone()).or_default();
                e.turns += 1;
                e.prompt_bytes += bytes;
            }
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                let m = v.get("compactMetadata").unwrap_or(&Value::Null);
                let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
                let duration_ms = m.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
                let parent = v.get("logicalParentUuid").and_then(Value::as_str).and_then(|u| stamped.get(u));
                let quiet = parent.zip(epoch(&ts)).map(|(p, t)| t - p - (duration_ms / 1000) as i64);
                w.compactions.push(Compaction {
                    ts,
                    trigger: m.get("trigger").and_then(Value::as_str).unwrap_or("?").to_string(),
                    pre: m.get("preTokens").and_then(Value::as_u64).unwrap_or(0),
                    post: m.get("postTokens").and_then(Value::as_u64).unwrap_or(0),
                    duration_ms,
                    quiet,
                });
            }
            Some("attachment") => {
                // Hook stdout is recorded in full but only `content` (and a SessionStart hook's
                // `additionalContext`) reaches the model; counting stdout made one session's hooks
                // look like 15 MB of context.
                let a = v.get("attachment").unwrap_or(&Value::Null);
                match a.get("type").and_then(Value::as_str) {
                    Some("hook_success") | Some("hook_system_message") => {
                        w.hook_bytes += a.get("content").and_then(Value::as_str).map_or(0, |s| s.len() as u64);
                        let extra = a.get("stdout").and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .and_then(|j| j.pointer("/hookSpecificOutput/additionalContext").and_then(Value::as_str).map(|s| s.len() as u64));
                        w.hook_bytes += extra.unwrap_or(0);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    w
}

/// The chain a transcript belongs to: the `session_id` on its last record that has one. A `/clear` starts a new
/// transcript, and its records keep the first transcript's id there (a real job, 2026-10-28: 11 transcripts). The
/// first transcript of a forked job opens with records of the session it came from, so the last record decides.
fn chain_of(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const KEY: &[u8] = br#""session_id":""#;
    let find = |buf: &[u8]| buf.windows(KEY.len()).rposition(|w| w == KEY).and_then(|i| {
        let rest = &buf[i + KEY.len()..];
        rest.iter().position(|&b| b == b'"').and_then(|e| String::from_utf8(rest[..e].to_vec()).ok())
    });
    // A copy in the archive cannot seek: it is read whole.
    let Ok(mut f) = std::fs::File::open(path) else { return find(&crate::archive::read(path).ok()?) };
    let len = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    let mut back = 1u64 << 16;
    loop {
        let start = len.saturating_sub(back);
        f.seek(SeekFrom::Start(start)).ok()?;
        buf.clear();
        (&mut f).take(len - start).read_to_end(&mut buf).ok()?;
        if let Some(c) = find(&buf) { return Some(c) }
        if start == 0 { return None }
        back *= 4;
    }
}

/// The transcripts in this transcript's folder that share its chain, oldest first, each measured. A later part is
/// written after the first transcript stopped, so a file last written before that is not read at all.
fn chain_parts(path: &str) -> Option<(String, Vec<(String, Weight)>)> {
    let path = std::path::Path::new(path);
    let chain = chain_of(path)?;
    let dir = path.parent()?;
    let modified = |p: &std::path::Path| crate::archive::modified(p);
    let since = modified(&dir.join(format!("{chain}.jsonl")));
    let mut parts: Vec<(String, Weight)> = crate::archive::list(dir).into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && modified(p) >= since)
        .filter(|p| chain_of(p).as_deref() == Some(chain.as_str()))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), measure(crate::archive::open(&p).ok()?))))
        .collect();
    parts.sort_by(|a, b| a.1.first_ts.cmp(&b.1.first_ts));
    (parts.len() > 1).then_some((chain, parts))
}

fn blocks_are_tool_results(content: &Value) -> bool {
    content.as_array().is_some_and(|b| !b.is_empty() && b.iter().all(|x| x.get("type").and_then(Value::as_str) == Some("tool_result")))
}

fn tok(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", (n + 500) / 1000),
        n => n.to_string(),
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0 }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[derive(Default)]
struct CopilotSegment {
    start_time: String,
    stop_time: String,
    context: Value,
    selected_model: Option<String>,
    reasoning_effort: Option<String>,
    max_ai_credits: Option<u64>,
    copilot_version: Option<String>,
    limits_final: bool,
    shutdown_type: Option<String>,
    shutdown: Option<Value>,
    calls: Vec<usize>,
}

struct CopilotCall {
    segment: usize,
    model: String,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: Option<u64>,
    credits_nano: u64,
    duration_ms: u64,
    finish_reason: Option<String>,
}

struct CopilotRefusal {
    source: String,
    command: String,
    rules: Vec<String>,
}

fn string_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number_at(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn duration_ms(start: &str, stop: &str) -> u64 {
    epoch(stop).zip(epoch(start)).map_or(0, |(b, a)| b.saturating_sub(a).max(0) as u64 * 1000)
}

fn shell_segments(s: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut arithmetic = 0;
    let mut heredocs = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(q) = quote {
            if ch == q { quote = None; }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if arithmetic > 0 {
            if ch == '(' { arithmetic += 1; }
            if ch == ')' { arithmetic -= 1; }
        } else if ch == '$' && s[i..].starts_with("$((") {
            chars.next();
            chars.next();
            arithmetic = 2;
        } else if ch == '<' && chars.peek().is_some_and(|(_, next)| *next == '<') {
            chars.next();
            if chars.peek().is_some_and(|(_, next)| *next == '<') {
                chars.next();
                continue;
            }
            let strip_tabs = chars.peek().is_some_and(|(_, next)| *next == '-');
            if strip_tabs { chars.next(); }
            while chars.peek().is_some_and(|(_, next)| *next == ' ' || *next == '\t') { chars.next(); }
            let mut delimiter = String::new();
            let mut quoted = None;
            let mut word = false;
            while let Some(&(_, c)) = chars.peek() {
                if quoted.is_none() && (c.is_whitespace() || ";|&<>()".contains(c)) { break }
                chars.next();
                word = true;
                if c == '\\' && quoted != Some('\'') {
                    if let Some((_, next)) = chars.next() { delimiter.push(next); }
                } else if let Some(q) = quoted {
                    if c == q { quoted = None; } else { delimiter.push(c); }
                } else if c == '\'' || c == '"' {
                    quoted = Some(c);
                } else {
                    delimiter.push(c);
                }
            }
            if word { heredocs.push((delimiter, strip_tabs)); }
        } else if ch == '\n' && !heredocs.is_empty() {
            segments.push(&s[start..i]);
            let mut body_start = i + 1;
            for (delimiter, strip_tabs) in heredocs.drain(..) {
                while body_start < s.len() {
                    let rest = &s[body_start..];
                    let (line, length) = rest.find('\n').map_or((rest, rest.len()), |end| (&rest[..end], end + 1));
                    body_start += length;
                    let line = line.trim_end_matches('\r');
                    if (if strip_tabs { line.trim_start_matches('\t') } else { line }) == delimiter { break }
                }
            }
            while chars.peek().is_some_and(|(j, _)| *j < body_start) { chars.next(); }
            start = body_start;
        } else if ch == ';' || ch == '\n'
            || ((ch == '&' || ch == '|') && chars.peek().is_some_and(|(_, next)| *next == ch))
        {
            segments.push(&s[start..i]);
            let mut end = i + ch.len_utf8();
            if (ch == '&' || ch == '|') && let Some((j, next)) = chars.next() {
                end = j + next.len_utf8();
            }
            start = end;
        }
    }
    segments.push(&s[start..]);
    segments
}

fn shell_first_word(s: &str) -> String {
    static WORDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?:'[^']*'|"(?:\\.|[^"\\])*"|\\.|[^\s'"\\])+"#).unwrap()
    });
    let word = shell_segments(s).into_iter().find_map(|segment| {
        let word = WORDS.find_iter(segment).map(|m| m.as_str()).find(|word| {
            !word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })?;
        (word.trim_matches(['\'', '"']) != "cd").then_some(word)
    }).unwrap_or("");
    let name = word.trim_matches(['\'', '"']);
    if name.contains('/') {
        "<path>".into()
    } else if (1..=40).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)) {
        name.to_string()
    } else {
        "?".into()
    }
}

fn zshguard_rules(s: &str) -> Vec<String> {
    static RULE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*[0-9]+\. \[([^\s\[\]]+)\]").unwrap()
    });
    RULE.captures_iter(s).map(|c| {
        match &c[1] {
            rule @ ("modifier" | "equals" | "nomatch" | "for-scalar" | "tied-var" | "bfs-time"
                | "pipestatus" | "list-arg" | "split-arg" | "adb-stdin" | "pipe-filter"
                | "bg-filter" | "pgrep-wait" | "clean-guard" | "inplace" | "inplace-redirect"
                | "alt-arg" | "tools-filter" | "switch-track" | "gh-auth-switch" | "gh-wrong-account") => rule,
            _ => "?",
        }.to_string()
    }).collect()
}

fn copilot_git(root: &str, args: &[String]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn copilot_segment_git(segment: &CopilotSegment) -> (String, Option<Vec<String>>) {
    let root = segment.context.get("gitRoot").and_then(Value::as_str).unwrap_or("");
    let branch = segment.context.get("branch").and_then(Value::as_str).unwrap_or("");
    if root.is_empty() || branch.is_empty() || segment.stop_time.is_empty() {
        return ("unknown".into(), None);
    }
    let (Some(start), Some(stop)) = (epoch(&segment.start_time), epoch(&segment.stop_time)) else {
        return ("unknown".into(), None);
    };
    let Some(log) = copilot_git(root, &["log".into(), "--format=%H %h %at".into(), "--end-of-options".into(), branch.into()]) else {
        return ("unknown".into(), None);
    };
    let mut head = "unknown".to_string();
    let mut commits = Vec::new();
    for line in log.lines() {
        let mut fields = line.split_whitespace();
        let (Some(full), Some(short), Some(time)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let Ok(time) = time.parse::<i64>() else { continue };
        if time <= stop {
            if head == "unknown" { head = full.to_string(); }
            if time >= start { commits.push(short.to_string()); }
        }
    }
    (head, Some(commits))
}

fn copilot_weight(o: &Opts, id: &str, path: &str) -> Res {
    let reader = crate::archive::open(std::path::Path::new(path)).map_err(|e| format!("{path}: {e}"))?;
    let mut segments: Vec<CopilotSegment> = Vec::new();
    let mut calls: Vec<CopilotCall> = Vec::new();
    let mut tool_calls: BTreeMap<String, u64> = BTreeMap::new();
    let mut shell_chains = 0u64;
    let mut starts: HashMap<String, (String, String)> = HashMap::new();
    let mut refusals = Vec::new();
    let (mut system_prompt_bytes, mut brief_bytes) = (None, None);
    let mut current: Option<usize> = None;
    let mut last_version: Option<String> = None;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = &v["data"];
        let ts = string_at(&v, "timestamp").unwrap_or_default();
        if kind != "session.start" && kind != "session.resume"
            && let Some(i) = current
            && segments[i].shutdown.is_none() && ts > segments[i].stop_time
        {
            segments[i].stop_time = ts.clone();
        }
        match kind {
            "session.start" | "session.resume" => {
                let start_time = if kind == "session.start" {
                    string_at(data, "startTime")
                } else {
                    string_at(data, "resumeTime")
                }.unwrap_or_else(|| ts.clone());
                let version = string_at(data, "copilotVersion").or_else(|| last_version.clone());
                if version.is_some() { last_version = version.clone(); }
                segments.push(CopilotSegment {
                    start_time: start_time.clone(),
                    stop_time: start_time,
                    context: data.get("context").cloned().unwrap_or(Value::Null),
                    selected_model: string_at(data, "selectedModel"),
                    reasoning_effort: string_at(data, "reasoningEffort"),
                    max_ai_credits: if kind == "session.start" {
                        number_at(&data["sessionLimits"], "maxAiCredits")
                    } else {
                        segments.first().and_then(|s| s.max_ai_credits)
                    },
                    copilot_version: version,
                    limits_final: false,
                    shutdown_type: None,
                    shutdown: None,
                    calls: Vec::new(),
                });
                current = Some(segments.len() - 1);
            }
            "session.usage_record" => {
                if let Some(segment) = current {
                    let u = &data["usage"];
                    let call = CopilotCall {
                        segment,
                        model: string_at(u, "model").unwrap_or_default(),
                        input: number_at(u, "inputTokens").unwrap_or(0),
                        cache_read: number_at(u, "cacheReadTokens").unwrap_or(0),
                        cache_write: number_at(u, "cacheWriteTokens").unwrap_or(0),
                        output: number_at(u, "outputTokens"),
                        credits_nano: number_at(&u["copilotUsage"], "totalNanoAiu").unwrap_or(0),
                        duration_ms: number_at(u, "duration").unwrap_or(0),
                        finish_reason: string_at(u, "finishReason"),
                    };
                    segments[segment].calls.push(calls.len());
                    calls.push(call);
                }
            }
            "session.info" => {
                if let Some(segment) = current {
                    let message = data.get("message").and_then(Value::as_str).unwrap_or("");
                    if message.starts_with("Session limits:") {
                        segments[segment].limits_final = message.to_ascii_lowercase().contains("the next model call is final");
                    }
                }
            }
            "session.shutdown" => {
                if let Some(segment) = current {
                    segments[segment].shutdown_type = string_at(data, "shutdownType");
                    segments[segment].stop_time = ts;
                    segments[segment].shutdown = Some(data.clone());
                }
            }
            "system.message" if system_prompt_bytes.is_none() => {
                system_prompt_bytes = data.get("content").and_then(Value::as_str).map(str::len);
            }
            "user.message" if brief_bytes.is_none() => {
                brief_bytes = data.get("content").and_then(Value::as_str).map(str::len);
            }
            "tool.execution_start" => {
                let name = string_at(data, "toolName").unwrap_or_default();
                *tool_calls.entry(name.clone()).or_default() += 1;
                let command = data.pointer("/arguments/command").and_then(Value::as_str).unwrap_or("");
                if name == "bash" && shell_segments(command).iter().filter(|s| !s.trim().is_empty()).count() > 1 {
                    shell_chains += 1;
                }
                if let Some(id) = string_at(data, "toolCallId") {
                    starts.insert(id, (name, shell_first_word(command)));
                }
            }
            "tool.execution_complete" if data.get("success").and_then(Value::as_bool) == Some(false) => {
                let error = data.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
                let (source, rules) = if error.starts_with("Permission denied because no interactive user response") {
                    ("copilot-permission".to_string(), Vec::new())
                } else if error.starts_with("Denied by preToolUse hook: zshguard refused") {
                    ("hook:zshguard".to_string(), zshguard_rules(error))
                } else {
                    ("other".to_string(), Vec::new())
                };
                let command = data.get("toolCallId").and_then(Value::as_str).and_then(|id| starts.get(id))
                    .filter(|(name, _)| name == "bash").map_or(String::new(), |(_, word)| word.clone());
                refusals.push(CopilotRefusal { source, command, rules });
            }
            _ => {}
        }
    }
    let credits_nano: u128 = calls.iter().map(|c| c.credits_nano as u128).sum();
    let total_credits = credits_nano as f64 / 1_000_000_000.0;
    let calls_json: Vec<Value> = calls.iter().enumerate().map(|(i, c)| json!({
        "number": i + 1, "segment": c.segment + 1, "model": c.model, "input": c.input,
        "fresh_input": c.input.saturating_sub(c.cache_read), "cache_read": c.cache_read,
        "cache_write": c.cache_write, "output": c.output, "credits": c.credits_nano as f64 / 1_000_000_000.0,
        "wall_ms": c.duration_ms, "finish_reason": c.finish_reason
    })).collect();
    let mut segment_json = Vec::new();
    let mut all_wall_ms = 0u64;
    for (i, segment) in segments.iter().enumerate() {
        let segment_nano: u128 = segment.calls.iter().map(|n| calls[*n].credits_nano as u128).sum();
        let segment_credits = segment_nano as f64 / 1_000_000_000.0;
        let cap = segment.max_ai_credits;
        let cap_hit = cap.is_some_and(|c| segment_nano >= c as u128 * 1_000_000_000);
        let stop_reason = if segment.shutdown.is_none() {
            "killed"
        } else if segment.limits_final || cap_hit {
            "cap"
        } else if segment.shutdown_type.as_deref().is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("error") || s.contains("fail")
        }) {
            "error"
        } else {
            "done"
        };
        let wall = duration_ms(&segment.start_time, &segment.stop_time);
        all_wall_ms += wall;
        let (head_at_stop, commits) = copilot_segment_git(segment);
        let call_input: u64 = segment.calls.iter().map(|n| calls[*n].input).sum();
        let call_fresh: u64 = segment.calls.iter().map(|n| calls[*n].input.saturating_sub(calls[*n].cache_read)).sum();
        let call_cache_read: u64 = segment.calls.iter().map(|n| calls[*n].cache_read).sum();
        let call_cache_write: u64 = segment.calls.iter().map(|n| calls[*n].cache_write).sum();
        let call_output: u64 = segment.calls.iter().map(|n| calls[*n].output.unwrap_or(0)).sum();
        let call_wall: u64 = segment.calls.iter().map(|n| calls[*n].duration_ms).sum();
        let floor_split = segment.shutdown.as_ref().map(|s| json!({
            "system_tokens": number_at(s, "systemTokens"),
            "tool_definitions_tokens": number_at(s, "toolDefinitionsTokens"),
            "conversation_tokens": number_at(s, "conversationTokens")
        }));
        let shutdown_json = segment.shutdown.as_ref().map(|s| json!({
            "shutdown_type": s.get("shutdownType"),
            "total_nano_aiu": number_at(s, "totalNanoAiu"),
            "total_api_duration_ms": number_at(s, "totalApiDurationMs"),
            "code_changes": {
                "files_modified": s["codeChanges"]["filesModified"].as_array().map(|files| files.len() as u64)
                    .or_else(|| number_at(&s["codeChanges"], "filesModified")),
                "lines_added": number_at(&s["codeChanges"], "linesAdded"),
                "lines_removed": number_at(&s["codeChanges"], "linesRemoved")
            }
        }));
        segment_json.push(json!({
            "number": i + 1, "cwd": segment.context.get("cwd"), "git_root": segment.context.get("gitRoot"),
            "branch": segment.context.get("branch"), "head_commit": segment.context.get("headCommit"),
            "head_at_stop": head_at_stop, "commits": commits, "selected_model": segment.selected_model,
            "reasoning_effort": segment.reasoning_effort, "max_ai_credits": cap,
            "copilot_version": segment.copilot_version, "start_time": segment.start_time, "stop_time": segment.stop_time,
            "stop_reason": stop_reason, "wall_ms": wall,
            "calls": segment.calls.len(), "input": call_input, "fresh_input": call_fresh,
            "cache_read": call_cache_read, "cache_write": call_cache_write, "output": call_output,
            "credits": segment_credits, "credits_per_call": segment_credits / segment.calls.len().max(1) as f64,
            "api_duration_ms": call_wall,
            "first_call_input_floor": segment.calls.first().map_or(0, |n| calls[*n].input),
            "floor_split": floor_split, "shutdown": shutdown_json
        }));
    }
    let calls_len = calls.len();
    let totals = json!({
        "calls": calls_len,
        "input": calls.iter().map(|c| c.input).sum::<u64>(),
        "fresh_input": calls.iter().map(|c| c.input.saturating_sub(c.cache_read)).sum::<u64>(),
        "cache_read": calls.iter().map(|c| c.cache_read).sum::<u64>(),
        "cache_write": calls.iter().map(|c| c.cache_write).sum::<u64>(),
        "output": calls.iter().map(|c| c.output.unwrap_or(0)).sum::<u64>(),
        "credits": total_credits,
        "credits_per_call": total_credits / calls_len.max(1) as f64,
        "api_duration_ms": calls.iter().map(|c| c.duration_ms).sum::<u64>()
    });
    let stop = segment_json.last().and_then(|s| s["stop_reason"].as_str()).unwrap_or("killed");
    let stop_credits = segment_json.last().and_then(|s| s["credits"].as_f64()).unwrap_or(0.0);
    let first_call_input_floor = calls.first().map_or(0, |c| c.input);
    let system_prompt_bytes = system_prompt_bytes.unwrap_or(0);
    let brief_bytes = brief_bytes.unwrap_or(0);

    if o.json {
        println!("{}", json!({
            "id": id, "stop_reason": stop, "stop_credits": stop_credits, "wall_ms": all_wall_ms,
            "totals": totals, "segments": segment_json, "calls": calls_json,
            "first_call_input_floor": first_call_input_floor,
            "system_prompt_bytes": system_prompt_bytes, "brief_bytes": brief_bytes,
            "tool_calls": {
                "by_name": tool_calls, "chained_shell_calls": shell_chains,
                "refusals": refusals.iter().map(|r| json!({"source": r.source, "command": r.command, "rules": r.rules})).collect::<Vec<_>>()
            }
        }));
        return Ok(());
    }

    let start = segments.first().map_or("", |s| s.start_time.as_str());
    let end = segments.last().map_or("", |s| s.stop_time.as_str());
    println!("# weight {}  Copilot  {} → {}", short_id(id), short_ts(start), short_ts(end));
    println!("session  {} calls  {:.1} credits  credits/call={:.4}  {} wall  {} stop ({:.1}/{})",
        calls_len, total_credits, totals["credits_per_call"].as_f64().unwrap_or(0.0), dur((all_wall_ms / 1000) as i64), stop, stop_credits,
        segments.last().and_then(|s| s.max_ai_credits).map_or_else(|| "?".into(), |c| c.to_string()));
    for (i, segment) in segments.iter().enumerate() {
        let row = &segment_json[i];
        let u = |k: &str| row[k].as_u64().unwrap_or(0);
        let commits = row["commits"].as_array().map_or_else(|| "unknown".into(), |commits| {
            format!("[{}]", commits.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
        });
        println!("segment {}  cwd={}  root={}  branch={}  head={}  stop-head={}  commits={}",
            i + 1, segment.context.get("cwd").and_then(Value::as_str).unwrap_or("?"),
            segment.context.get("gitRoot").and_then(Value::as_str).unwrap_or("?"),
            segment.context.get("branch").and_then(Value::as_str).unwrap_or("?"),
            segment.context.get("headCommit").and_then(Value::as_str).unwrap_or("?"), row["head_at_stop"].as_str().unwrap_or("unknown"), commits);
        println!("  model={} effort={} cap={} version={} stop={} ({:.1}/{}) wall={}",
            segment.selected_model.as_deref().unwrap_or("?"), segment.reasoning_effort.as_deref().unwrap_or("?"),
            segment.max_ai_credits.map_or_else(|| "?".into(), |c| c.to_string()),
            segment.copilot_version.as_deref().unwrap_or("?"), row["stop_reason"].as_str().unwrap_or("killed"),
            row["credits"].as_f64().unwrap_or(0.0), segment.max_ai_credits.map_or_else(|| "?".into(), |c| c.to_string()),
            dur((u("wall_ms") / 1000) as i64));
        println!("  calls={} credits={:.1} credits/call={:.4} input={} fresh={} cache-read={} cache-write={} output={} api-ms={}",
            u("calls"), row["credits"].as_f64().unwrap_or(0.0), row["credits_per_call"].as_f64().unwrap_or(0.0), u("input"), u("fresh_input"), u("cache_read"),
            u("cache_write"), u("output"), u("api_duration_ms"));
        let split = &row["floor_split"];
        let token = |k: &str| split[k].as_u64().map_or_else(|| "?".into(), |n| n.to_string());
        println!("  first call input floor={}  shutdown floor split: system={} tool-definitions={} conversation={}",
            u("first_call_input_floor"), token("system_tokens"), token("tool_definitions_tokens"), token("conversation_tokens"));
    }
    println!("system prompt={} bytes  first brief={} bytes", system_prompt_bytes, brief_bytes);
    if o.n > 0 {
        println!("\ncall  model    input fresh cache-read cache-write output credits wall-ms");
        for c in calls_json.iter().take(o.n) {
            println!("{:>4}  {:<18} {:>5} {:>5} {:>10} {:>11} {:>6} {:>7.1} {:>7}",
                c["number"].as_u64().unwrap_or(0), c["model"].as_str().unwrap_or(""),
                c["input"].as_u64().unwrap_or(0), c["fresh_input"].as_u64().unwrap_or(0),
                c["cache_read"].as_u64().unwrap_or(0), c["cache_write"].as_u64().unwrap_or(0),
                c["output"].as_u64().unwrap_or(0), c["credits"].as_f64().unwrap_or(0.0),
                c["wall_ms"].as_u64().unwrap_or(0));
        }
    }
    println!("\ntool calls");
    for (name, count) in &tool_calls { println!("  {name:<20} {count}"); }
    println!("  chained shell calls  {shell_chains}");
    for refusal in &refusals {
        println!("  refusal {}  command={}  rules={}", refusal.source, refusal.command, refusal.rules.join(","));
    }
    Ok(())
}

/// `arg` is a session prefix, or a path to a `.jsonl` transcript (for a session not yet indexed).
pub fn weight(o: &Opts, arg: &str) -> Res {
    // A Codex rollout, plain or compressed by Codex: its id is the last 36 characters before `.jsonl`.
    if let Some(stem) = arg.strip_suffix(".zst").unwrap_or(arg).strip_suffix(".jsonl")
        && std::path::Path::new(arg).is_file()
        && let (Ok(real), Ok(home)) = (std::path::Path::new(arg).canonicalize(), crate::index::codex_home().canonicalize())
        && real.starts_with(&home) {
        return Err(crate::cmds::refusal(stem.get(stem.len().saturating_sub(36)..).unwrap_or(stem), "weight", "codex").into());
    }
    let (id, project, path) = if arg.ends_with(".jsonl") && std::path::Path::new(arg).is_file() {
        if let (Ok(real), Ok(root)) = (std::path::Path::new(arg).canonicalize(), crate::index::copilot_root().canonicalize())
            && real.starts_with(&root) {
            let session = real.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()).unwrap_or(arg);
            return copilot_weight(o, session, &real.to_string_lossy());
        }
        let stem = std::path::Path::new(arg).file_stem().and_then(|s| s.to_str()).unwrap_or(arg).to_string();
        (stem, String::new(), arg.to_string())
    } else {
        let c = crate::db::open()?;
        let id = crate::insight::resolve_session(&c, arg)?;
        let (project, path, harness): (String, String, String) = c.query_row(
            "select project, path, harness from sessions where id = ?1", [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if harness == "copilot" {
            crate::db::log_usage(&c, "weight", &id, 1);
            return copilot_weight(o, &id, &path);
        }
        crate::cmds::claude_only(&c, &id, "weight")?;
        crate::db::log_usage(&c, "weight", &id, 1);
        (id, project, path)
    };
    let f = crate::archive::open(std::path::Path::new(&path)).map_err(|e| format!("{path}: {e}"))?;
    let w = measure(f);
    let mut sorted = w.contexts.clone();
    sorted.sort_unstable();
    let mut tools: Vec<(&String, &Tool)> = w.tools.iter().collect();
    tools.sort_by_key(|(_, t)| std::cmp::Reverse(t.bytes / 4 + t.images * IMAGE_TOKENS_MAX));
    let chain = chain_parts(&path);
    let part = |(pid, p): &(String, Weight)| {
        let mut s = p.contexts.clone();
        s.sort_unstable();
        json!({"id": pid, "first": p.first_ts, "last": p.last_ts, "calls": s.len(), "first_context": p.contexts.first().copied().unwrap_or(0),
            "median": pct(&s, 0.5), "max": s.last().copied().unwrap_or(0), "compactions": p.compactions.len(),
            "idle_compactions": p.compactions.iter().filter(|c| c.idle()).count()})
    };

    if o.json {
        println!("{}", json!({
            "id": id, "project": project, "first": w.first_ts, "last": w.last_ts,
            "calls": sorted.len(), "context": {"first": w.contexts.first().copied().unwrap_or(0), "median": pct(&sorted, 0.5), "p90": pct(&sorted, 0.9), "max": sorted.last().copied().unwrap_or(0),
                "processed": sorted.iter().sum::<u64>()},
            "output_tokens": w.output,
            "compactions": w.compactions.iter().map(|c| json!({"ts": c.ts, "trigger": c.trigger, "pre": c.pre, "post": c.post,
                "duration_ms": c.duration_ms, "quiet_s": c.quiet, "idle": c.idle()})).collect::<Vec<_>>(),
            "origins": w.origins.iter().map(|(k, v)| (k.clone(), json!({"turns": v.turns, "calls": v.calls, "processed": v.processed, "prompt_bytes": v.prompt_bytes}))).collect::<serde_json::Map<_, _>>(),
            "tools": tools.iter().map(|(k, t)| json!({"tool": k, "calls": t.calls, "bytes": t.bytes, "images": t.images})).collect::<Vec<_>>(),
            "hook_bytes": w.hook_bytes, "prompt_images": w.prompt_images,
            "chain": chain.as_ref().map(|(c, parts)| json!({"id": c, "parts": parts.iter().map(part).collect::<Vec<_>>()})),
        }));
        return Ok(());
    }

    let span = format!("{} → {}", short_ts(&w.first_ts), short_ts(&w.last_ts));
    println!("# weight {}  {}{span}", short_id(&id), if project.is_empty() { String::new() } else { format!("{project}  ") });
    if let Some((c, parts)) = &chain {
        let at = parts.iter().position(|(pid, _)| *pid == id).map_or(String::new(), |i| format!("; the figures after this table are part {}'s", i + 1));
        println!("chain {}: {} transcripts, a new one at each /clear{at}", short_id(c), parts.len());
        println!("  part  session   from              to                 calls  first  median     max  compactions");
        for (i, p) in parts.iter().enumerate() {
            let v = part(p);
            let u = |k: &str| v[k].as_u64().unwrap_or(0);
            let idle = match u("idle_compactions") { 0 => String::new(), k => format!(" ({k} idle)") };
            println!("  {:>4}  {}  {}  {}  {:>5}  {:>5}  {:>6}  {:>6}  {}{idle}", i + 1, short_id(&p.0), short_ts(&p.1.first_ts),
                short_ts(&p.1.last_ts), u("calls"), tok(u("first_context")), tok(u("median")), tok(u("max")), u("compactions"));
        }
        println!();
    }
    if sorted.is_empty() {
        println!("no model calls with usage recorded");
        return Ok(());
    }
    println!("model calls  {}   context per call: first {}  median {}  p90 {}  max {}   output {}",
        sorted.len(), tok(w.contexts[0]), tok(pct(&sorted, 0.5)), tok(pct(&sorted, 0.9)), tok(*sorted.last().unwrap()), tok(w.output));
    if w.compactions.is_empty() {
        println!("compactions  none");
    } else {
        println!("compactions  {} (idle: no record for {} min or more before it)", w.compactions.len(), IDLE_SECS / 60);
        for c in &w.compactions { println!("  {}", c.line()) }
    }
    println!("\nwhat started each turn      turns   calls   context processed   its text");
    let mut origins: Vec<(&String, &Origin)> = w.origins.iter().collect();
    origins.sort_by_key(|(_, v)| std::cmp::Reverse(v.processed));
    let total: u64 = sorted.iter().sum();
    for (k, v) in origins {
        println!("  {k:<24} {:>6} {:>7}   {:>8} ({:>2}%)   ~{}", v.turns, v.calls, tok(v.processed),
            (v.processed * 100).checked_div(total).unwrap_or(0), tok(v.prompt_bytes / 4));
    }
    println!("\ntool results (≈ bytes/4; images at most {IMAGE_TOKENS_MAX} tokens each)");
    let top = if o.n == 0 { 8 } else { o.n };
    for (k, t) in tools.iter().take(top) {
        let img = if t.images > 0 { format!("  + {} image(s)", t.images) } else { String::new() };
        println!("  {k:<24} {:>5} calls   ~{}{img}", t.calls, tok(t.bytes / 4));
    }
    if tools.len() > top {
        let rest: u64 = tools.iter().skip(top).map(|(_, t)| t.bytes / 4).sum();
        println!("  {} more tools             ~{}", tools.len() - top, tok(rest));
    }
    let img = if w.prompt_images > 0 { format!("   images in prompts: {}", w.prompt_images) } else { String::new() };
    println!("\ninjected by hooks  ~{}{img}", tok(w.hook_bytes / 4));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_call_per_message_id_and_attributed_to_its_turn() {
        let lines = [
            r#"{"type":"user","turnOrigin":"human","message":{"content":"look at the board"},"timestamp":"2026-10-10T10:00:00Z"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":10}},"timestamp":"2026-10-10T10:00:01Z"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"same call, second block"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":10}},"timestamp":"2026-10-10T10:00:01Z"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"12345678"}]},"timestamp":"2026-10-10T10:00:02Z"}"#,
            r#"{"type":"user","turnOrigin":"scheduled","isMeta":true,"message":{"content":"tick"},"timestamp":"2026-10-10T10:30:00Z"}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[],"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":1995,"output_tokens":3}},"timestamp":"2026-10-10T10:30:01Z"}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"m3","content":[],"usage":{"input_tokens":99999}},"timestamp":"2026-10-10T10:30:02Z"}"#,
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":967278,"postTokens":32338},"timestamp":"2026-10-10T11:00:00Z"}"#,
            r#"{"type":"attachment","attachment":{"type":"hook_success","content":"","stdout":"this is recorded, never sent"},"timestamp":"2026-10-10T11:00:01Z"}"#,
            r#"{"type":"attachment","attachment":{"type":"hook_success","content":"abcd","stdout":"{\"hookSpecificOutput\":{\"additionalContext\":\"12345678\"}}"},"timestamp":"2026-10-10T11:00:02Z"}"#,
        ].join("\n");
        let w = measure(std::io::Cursor::new(lines));
        assert_eq!(w.contexts, vec![1002, 2000], "m1 once, sidechain skipped");
        assert_eq!(w.output, 13);
        assert_eq!(w.origins["human"].calls, 1);
        assert_eq!(w.origins["human"].processed, 1002);
        assert_eq!(w.origins["scheduled"].turns, 1, "a meta prompt with a turnOrigin is still a turn");
        assert_eq!(w.origins["scheduled"].processed, 2000);
        assert!(!w.origins.contains_key("?"), "a tool result is not a prompt");
        assert_eq!(w.tools["Bash"].bytes, 8);
        assert_eq!(w.compactions.len(), 1);
        assert_eq!(w.compactions[0].pre, 967278);
        assert_eq!(w.hook_bytes, 12, "content plus additionalContext, not raw stdout");
    }

    #[test]
    fn older_transcripts_without_turn_origin() {
        let lines = [
            r#"{"type":"user","message":{"content":"<task-notification>done</task-notification>"}}"#,
            r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":10}}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"skill text"}}"#,
            r#"{"type":"user","message":{"content":"hello"}}"#,
            r#"{"type":"assistant","message":{"id":"b","usage":{"input_tokens":20}}}"#,
        ].join("\n");
        let w = measure(std::io::Cursor::new(lines));
        assert_eq!(w.origins["task_notification"].processed, 10);
        assert_eq!(w.origins["human"].processed, 20);
        assert_eq!(w.origins["human"].turns, 1, "the meta record is not a turn");
    }

    #[test]
    fn the_span_is_the_earliest_and_latest_timestamp() {
        let lines = [
            r#"{"type":"queue-operation","timestamp":"2026-10-26T10:50:42Z"}"#,
            r#"{"type":"user","message":{"content":"plan it"},"timestamp":"2026-10-16T14:22:30Z"}"#,
            r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":1}},"timestamp":"2026-10-16T22:04:39Z"}"#,
        ].join("\n");
        let w = measure(std::io::Cursor::new(lines));
        assert_eq!((w.first_ts.as_str(), w.last_ts.as_str()), ("2026-10-16T14:22:30Z", "2026-10-26T10:50:42Z"));
    }

    #[test]
    fn a_compaction_after_a_long_quiet_is_idle_and_one_right_after_a_record_is_not() {
        // Shapes from two real sessions (2026-10-14, 2026-10-27): the boundary is
        // written when the compaction ends, so the quiet before it is the gap to its logical parent minus
        // durationMs. The four records before a boundary carry no timestamp.
        let lines = [
            r#"{"type":"system","subtype":"away_summary","uuid":"p1","timestamp":"2026-10-14T13:06:08.751Z"}"#,
            r#"{"type":"ai-title","aiTitle":"t"}"#,
            r#"{"type":"mode","mode":"x"}"#,
            r#"{"type":"system","subtype":"compact_boundary","logicalParentUuid":"p1","compactMetadata":{"trigger":"auto","preTokens":366735,"postTokens":28621,"durationMs":44198},"timestamp":"2026-10-14T13:57:34.369Z"}"#,
            r#"{"type":"attachment","uuid":"p2","attachment":{"type":"x"},"timestamp":"2026-10-27T14:02:24.616Z"}"#,
            r#"{"type":"system","subtype":"compact_boundary","logicalParentUuid":"p2","compactMetadata":{"trigger":"auto","preTokens":477187,"postTokens":28945,"durationMs":86},"timestamp":"2026-10-27T14:02:33.157Z"}"#,
            r#"{"type":"system","subtype":"compact_boundary","logicalParentUuid":"elsewhere","compactMetadata":{"trigger":"auto","preTokens":510815,"postTokens":30000,"durationMs":84224},"timestamp":"2026-10-12T14:33:36.000Z"}"#,
        ].join("\n");
        let w = measure(std::io::Cursor::new(lines));
        let c = &w.compactions;
        assert_eq!((c[0].quiet, c[0].duration_ms, c[0].idle()), (Some(3086 - 44), 44198, true), "51m26s gap, 44s of it compacting");
        assert_eq!((c[1].quiet, c[1].idle()), (Some(9), false));
        assert_eq!((c[2].quiet, c[2].idle()), (None, false), "a parent outside this transcript has no time");
        assert_eq!(c[0].line(), "2026-10-14 13:57  auto  367k → 29k  50m 42s quiet before it, 44s to compact  idle");
        assert_eq!(c[1].line(), "2026-10-27 14:02  auto  477k → 29k  9s quiet before it, 0s to compact");
        assert_eq!(c[2].line(), "2026-10-12 14:33  auto  511k → 30k  quiet before it unknown (its parent record is not in this transcript), 1m 24s to compact");
    }

    #[test]
    fn percentiles_and_token_format() {
        let s = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(pct(&s, 0.5), 6);
        assert_eq!(pct(&s, 0.9), 9);
        assert_eq!(tok(476_400), "476k");
        assert_eq!(tok(656_400_000), "656.4M");
        assert_eq!(tok(999), "999");
    }
}
