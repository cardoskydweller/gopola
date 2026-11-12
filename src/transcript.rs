//! Parsing of Claude Code transcript lines into the few events gopola cares about.
//! Record shapes were quoted from real files on 2026-09-22 (Claude Code 2.1.278).

use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum Event {
    /// A human prompt: `type == "user"`, string content, not meta.
    Prompt { ts: String, cwd: Option<String>, branch: Option<String>, text: String, entrypoint: Option<String> },
    /// An assistant record: its last text block (if any), the files it edited or wrote, and what it ran
    /// (Bash commands, and skills as `Skill <name> <args>`).
    Reply { ts: String, text: Option<String>, files: Vec<String>, tools: Vec<String>, reads: Vec<String>, commands: Vec<String> },
    /// Claude Code's own title for the session (`{"type":"ai-title","aiTitle":"…"}`), written per turn.
    Title(String),
    /// A Copilot or Codex session's first record: where it launched, which CLI wrote it, and whether the record itself
    /// says the run was scripted (Codex's `exec`; Copilot's start record does not say, so `None`). Claude's `classify`
    /// never returns it.
    Start { cwd: Option<String>, branch: Option<String>, version: Option<String>, scripted: Option<bool> },
}

/// Titles Claude Code emits when it has nothing to say about the session.
pub fn is_generic_title(t: &str) -> bool {
    matches!(t.trim(), "" | "Continue session" | "New session" | "Untitled")
}

/// Tags whose whole block is removed before storage. `private` leaves a marker; the others are harness
/// plumbing (the same list claude-mem strips) and leave nothing.
const STRIPPED_TAGS: &[(&str, &str)] = &[
    ("private", "[private]"),
    ("system-reminder", ""),
    ("task-notification", ""),
    ("persisted-output", ""),
    ("system_instruction", ""),
    ("system-instruction", ""),
    ("claude-mem-context", ""),
];

fn strip_tag(text: &str, tag: &str, marker: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open) {
        let s = pos + start;
        out.push_str(&text[pos..s]);
        out.push_str(marker);
        match lower[s..].find(&close) {
            Some(end) => pos = s + end + close.len(),
            None => pos = text.len(),
        }
    }
    out.push_str(&text[pos..]);
    out
}

/// Remove `<private>…</private>` (leaving `[private]`) and harness tag blocks, then redact secrets.
pub fn strip_private(text: &str) -> String {
    let mut t = text.to_string();
    for (tag, marker) in STRIPPED_TAGS {
        if t.to_ascii_lowercase().contains(&format!("<{tag}>")) {
            t = strip_tag(&t, tag, marker);
        }
    }
    redact_secrets(&t)
}

/// Secret-shaped strings never reach the database. The transcript on disk still has them; this keeps
/// the searchable copy and any model digest clean. Conservative patterns, each with a kind marker.
pub fn redact_secrets(text: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "private-key"),
            (r"\bAKIA[0-9A-Z]{16}\b", "aws-key"),
            (r"\bghp_[A-Za-z0-9]{30,}\b|\bgithub_pat_[A-Za-z0-9_]{30,}\b", "github-token"),
            (r"\bxox[abpr]-[A-Za-z0-9-]{10,}\b", "slack-token"),
            (r"\bsk-[A-Za-z0-9_-]{20,}\b", "api-key"),
            (r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b", "jwt"),
            (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}", "bearer"),
        ]
        .iter()
        .map(|(p, k)| (regex::Regex::new(p).expect("redaction regex"), *k))
        .collect()
    });
    let mut t = std::borrow::Cow::Borrowed(text);
    for (re, kind) in rules.iter() {
        if re.is_match(&t) {
            t = std::borrow::Cow::Owned(re.replace_all(&t, format!("[redacted:{kind}]")).into_owned());
        }
    }
    t.into_owned()
}

#[derive(Debug, PartialEq)]
pub enum Parsed {
    Event(Event),
    /// A record kind gopola ignores by design (attachments, tool results, meta, …).
    Ignored,
    /// A user or assistant record gopola expected to read and could not.
    Unreadable,
    /// A record type the reader does not know (its `type`); Claude's `classify` never returns it.
    Unknown(String),
}

/// Parse one transcript line. `None` for lines that are not JSON or not an event we keep.
#[cfg(test)]
pub fn parse_line(line: &str) -> Option<Event> {
    match classify(line) {
        Parsed::Event(e) => Some(e),
        _ => None,
    }
}

/// The words of a human prompt. The CLI writes a string. The VS Code extension writes a list of text blocks
/// marked `turnOrigin: "human"` (all 283 such prompts since 2026-10-01; its two "[Request interrupted by user]"
/// records are unmarked), sometimes with an `<ide_opened_file>` block, which is the editor's context, not the
/// user's words. Tool results are lists too, but never marked human.
fn prompt_raw(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if v.get("turnOrigin").and_then(Value::as_str) != Some("human") {
        return None;
    }
    let blocks = content.as_array()?;
    let kind = |b: &Value| b.get("type").and_then(Value::as_str).map(str::to_string);
    if blocks.iter().any(|b| kind(b).as_deref() == Some("tool_result")) {
        return None;
    }
    let words: Vec<&str> = blocks
        .iter()
        .filter(|b| kind(b).as_deref() == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter(|t| !t.trim_start().starts_with("<ide_"))
        .collect();
    if words.is_empty() { None } else { Some(words.join("\n")) }
}

pub fn classify(line: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Parsed::Unreadable };
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "ai-title" {
        return match v.get("aiTitle").and_then(Value::as_str) {
            Some(t) if !is_generic_title(t) => Parsed::Event(Event::Title(t.trim().to_string())),
            _ => Parsed::Ignored,
        };
    }
    if kind != "user" && kind != "assistant" {
        return Parsed::Ignored;
    }
    if kind == "user" {
        let meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
        let Some(raw) = prompt_raw(&v) else { return Parsed::Ignored };
        if meta {
            return Parsed::Ignored;
        }
        // Harness-generated prompts (task notifications) and prompts that are entirely private are not turns.
        let head = raw.trim_start();
        if head.starts_with("<task-notification>") || head.starts_with("[SYSTEM NOTIFICATION") || strip_private(&raw).replace("[private]", "").trim().is_empty() {
            return Parsed::Ignored;
        }
    }
    match parse_event(&v, kind) {
        Some(e) => Parsed::Event(e),
        None => Parsed::Unreadable,
    }
}

fn parse_event(v: &Value, kind: &str) -> Option<Event> {
    let ts = v.get("timestamp")?.as_str()?.to_string();
    match kind {
        "user" => {
            let raw = prompt_raw(v)?;
            // Harness-generated prompts (task notifications) are not the user's words: not a turn at all.
            if raw.trim_start().starts_with("<task-notification>") || raw.trim_start().starts_with("[SYSTEM NOTIFICATION") {
                return None;
            }
            let text = strip_private(&raw);
            if text.trim().is_empty() {
                return None;
            }
            Some(Event::Prompt {
                ts,
                cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
                branch: v.get("gitBranch").and_then(Value::as_str).map(str::to_string),
                text,
                entrypoint: v.get("entrypoint").and_then(Value::as_str).map(str::to_string),
            })
        }
        "assistant" => {
            let blocks = v.get("message")?.get("content")?.as_array()?;
            let mut text = None;
            let mut files = Vec::new();
            let mut tools = Vec::new();
            let mut reads = Vec::new();
            let mut commands = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.trim().is_empty() {
                                text = Some(strip_private(t));
                            }
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                        tools.push(name.to_string());
                        if name == "Read" {
                            if let Some(p) = b.get("input").and_then(|i| i.get("file_path")).and_then(Value::as_str) {
                                reads.push(p.to_string());
                            }
                        }
                        if name == "Edit" || name == "Write" {
                            if let Some(p) = b.get("input").and_then(|i| i.get("file_path")).and_then(Value::as_str) {
                                files.push(p.to_string());
                            }
                        }
                        let input = b.get("input");
                        let arg = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
                        if name == "Bash" && let Some(cmd) = arg("command") {
                            commands.push(strip_private(cmd));
                        }
                        if name == "Skill" && let Some(s) = arg("skill") {
                            commands.push(strip_private(format!("Skill {s} {}", arg("args").unwrap_or("")).trim()));
                        }
                    }
                    _ => {}
                }
            }
            Some(Event::Reply { ts, text, files, tools, reads, commands })
        }
        _ => None,
    }
}

/// A prompt that carries no information about what the session was for.
pub fn is_throwaway_prompt(text: &str) -> bool {
    let t = text.trim();
    t.len() < 3 || t.starts_with("--") || t.starts_with('/') || t.starts_with("<command-name>")
}

pub fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

/// Truncate to at most `n` characters, on a char boundary, with an ellipsis.
pub fn clip(text: &str, n: usize) -> String {
    let mut out: String = text.chars().take(n).collect();
    if text.chars().count() > n {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture lines copied from a real transcript on 2026-09-22 and redacted.
    const PROMPT: &str = r#"{"parentUuid":"3ad6d2f9","isSidechain":false,"promptId":"dfe16434","type":"user","message":{"role":"user","content":"the flutter sdk"},"uuid":"0a544b77","timestamp":"2026-09-21T12:09:44.110Z","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","entrypoint":"cli","cwd":"/Users/x/repos/demo-app","sessionId":"2860f747","version":"2.1.278","gitBranch":"main"}"#;
    const META: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: /x"}]},"timestamp":"2026-09-21T12:09:45.000Z","cwd":"/Users/x/repos/demo-app"}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01","type":"tool_result","content":"ok"}]},"timestamp":"2026-09-21T12:09:46.000Z","cwd":"/Users/x/repos/demo-app"}"#;
    const REPLY: &str = r#"{"type":"assistant","timestamp":"2026-09-21T12:10:48.388Z","uuid":"4a8f2e8e","message":{"model":"claude-opus-5","role":"assistant","content":[{"type":"text","text":"I've read the ground.\nSecond line."},{"type":"tool_use","id":"toolu_02","name":"Edit","input":{"file_path":"/Users/x/repos/demo-app/docs/board.md","old_string":"a","new_string":"b"}},{"type":"tool_use","id":"toolu_03","name":"Bash","input":{"command":"ls"}}]}}"#;
    const ATTACHMENT: &str = r#"{"type":"attachment","attachment":{"type":"hook_success"},"timestamp":"2026-09-21T12:10:49.000Z"}"#;

    #[test]
    fn prompt_is_parsed_with_cwd_and_branch() {
        assert_eq!(
            parse_line(PROMPT),
            Some(Event::Prompt {
                ts: "2026-09-21T12:09:44.110Z".into(),
                cwd: Some("/Users/x/repos/demo-app".into()),
                branch: Some("main".into()),
                text: "the flutter sdk".into(),
                entrypoint: Some("cli".into()),
            })
        );
    }

    #[test]
    fn meta_and_tool_result_user_records_are_not_prompts() {
        assert_eq!(parse_line(META), None);
        assert_eq!(parse_line(TOOL_RESULT), None);
    }

    #[test]
    fn reply_keeps_last_text_edited_files_and_commands() {
        assert_eq!(
            parse_line(REPLY),
            Some(Event::Reply {
                ts: "2026-09-21T12:10:48.388Z".into(),
                text: Some("I've read the ground.\nSecond line.".into()),
                files: vec!["/Users/x/repos/demo-app/docs/board.md".into()],
                tools: vec!["Edit".into(), "Bash".into()],
                reads: vec![],
                commands: vec!["ls".into()],
            })
        );
    }

    #[test]
    fn vscode_prompts_are_text_block_lists_marked_human() {
        // Shapes from a3bc4bcd (a client Flutter app, VS Code extension, 2026-10-13).
        let prompt = r#"{"type":"user","entrypoint":"claude-vscode","promptSource":"sdk","turnOrigin":"human","timestamp":"2026-10-13T14:20:00.000Z","cwd":"/x","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>The user opened the file /x/lib/a.dart</ide_opened_file>"},{"type":"text","text":"check the PROJ board for tickets assigned to me"}]}}"#;
        match classify(prompt) {
            Parsed::Event(Event::Prompt { text, entrypoint, .. }) => {
                assert_eq!(text, "check the PROJ board for tickets assigned to me");
                assert_eq!(entrypoint.as_deref(), Some("claude-vscode"));
            }
            other => panic!("{other:?}"),
        }
        let interrupted = r#"{"type":"user","entrypoint":"claude-vscode","timestamp":"2026-10-13T14:21:00.000Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
        assert_eq!(classify(interrupted), Parsed::Ignored);
        let result = r#"{"type":"user","turnOrigin":"human","timestamp":"2026-10-13T14:22:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"},{"type":"text","text":"x"}]}}"#;
        assert_eq!(classify(result), Parsed::Ignored);
    }

    #[test]
    fn commands_are_bash_and_skills_with_secrets_redacted() {
        let r = r#"{"type":"assistant","timestamp":"2026-10-13T14:30:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"TWG_CONFIG_DIR=~/.config/twg-acme twg login --site acme","description":"log in"}},{"type":"tool_use","id":"b","name":"Skill","input":{"skill":"building-cli-tools"}},{"type":"tool_use","id":"c","name":"Skill","input":{"skill":"gopola","args":"search twg"}},{"type":"tool_use","id":"d","name":"Bash","input":{"command":"curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345' x"}},{"type":"tool_use","id":"e","name":"Grep","input":{"pattern":"twg"}}]}}"#;
        match classify(r) {
            Parsed::Event(Event::Reply { commands, .. }) => assert_eq!(commands, vec![
                "TWG_CONFIG_DIR=~/.config/twg-acme twg login --site acme",
                "Skill building-cli-tools",
                "Skill gopola search twg",
                "curl -H 'Authorization: [redacted:bearer]' x",
            ]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_records_and_garbage_are_skipped() {
        assert_eq!(classify(ATTACHMENT), Parsed::Ignored);
        assert_eq!(classify(META), Parsed::Ignored);
        assert_eq!(classify(TOOL_RESULT), Parsed::Ignored);
        assert_eq!(classify("not json"), Parsed::Unreadable);
        assert_eq!(classify(r#"{"type":"assistant","message":{"content":"legacy string"}}"#), Parsed::Unreadable);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn titles_and_private_spans() {
        assert_eq!(classify(r#"{"type":"ai-title","aiTitle":"Reverse engineering platform version","sessionId":"x"}"#),
            Parsed::Event(Event::Title("Reverse engineering platform version".into())));
        assert_eq!(classify(r#"{"type":"ai-title","aiTitle":"Continue session","sessionId":"x"}"#), Parsed::Ignored);
        assert_eq!(strip_private("key <private>hunter2</private> done"), "key [private] done");
        assert_eq!(strip_private("a <PRIVATE>x\ny</PRIVATE> b <private>unterminated"), "a [private] b [private]");
        assert_eq!(strip_private("nothing here"), "nothing here");
        let p = r#"{"type":"user","message":{"role":"user","content":"token <private>abc</private>"},"timestamp":"2026-09-21T12:09:44.110Z"}"#;
        match classify(p) { Parsed::Event(Event::Prompt { text, .. }) => assert_eq!(text, "token [private]"), other => panic!("{other:?}") }
    }

    #[test]
    fn harness_tags_and_secrets() {
        assert_eq!(strip_private("do x <system-reminder>\nnoise\n</system-reminder> now"), "do x  now");
        assert_eq!(classify(r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>x</task-id>\n</task-notification>"},"timestamp":"2026-09-21T12:09:44.110Z"}"#), Parsed::Ignored);
        assert_eq!(classify(r#"{"type":"user","message":{"role":"user","content":"<private>all of it</private>"},"timestamp":"2026-09-21T12:09:44.110Z"}"#), Parsed::Ignored);
        assert_eq!(redact_secrets("key AKIAIOSFODNN7EXAMPLE and ghp_abcdefghijklmnopqrstuvwxyz0123456789 ok"), "key [redacted:aws-key] and [redacted:github-token] ok");
        assert_eq!(redact_secrets("Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345"), "Authorization: [redacted:bearer]");
        assert_eq!(redact_secrets("demo-app-dev-key stays; sk-short stays"), "demo-app-dev-key stays; sk-short stays");
        let r = r#"{"type":"assistant","timestamp":"2026-09-21T12:10:48.388Z","message":{"role":"assistant","content":[{"type":"text","text":"read it"},{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/x/a.rs"}}]}}"#;
        match classify(r) { Parsed::Event(Event::Reply { reads, files, .. }) => { assert_eq!(reads, vec!["/x/a.rs"]); assert!(files.is_empty()); } other => panic!("{other:?}") }
    }

    #[test]
    fn throwaway_prompts() {
        assert!(is_throwaway_prompt("--continue"));
        assert!(is_throwaway_prompt("/clear"));
        assert!(is_throwaway_prompt("ok"));
        assert!(!is_throwaway_prompt("the flutter sdk"));
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("héllo wörld", 5), "héllo…");
        assert_eq!(clip("hi", 5), "hi");
    }
}
