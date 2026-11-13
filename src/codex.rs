//! Parsing of OpenAI Codex CLI rollout lines into the events gopola keeps.
//! Record shapes were read from the probe rollouts on 2026-11-11 (CLI 0.167.0); the fixtures are invented.

use crate::transcript::{Event, Parsed, strip_private};
use serde_json::Value;
use std::path::Path;

/// Every record kind seen so far: `type`, or `type/payload.type` for `event_msg` and `response_item`, or
/// `item/<item.type>` for a completed item. A kind outside this list is `Parsed::Unknown`, so a format change shows
/// up in `doctor`.
pub const KNOWN_TYPES: &[&str] = &[
    "session_meta", "world_state", "turn_context", "token_usage_record",
    "event_msg/task_started", "event_msg/thread_settings_applied", "event_msg/token_count", "event_msg/task_complete",
    "response_item/additional_tools", "response_item/message", "response_item/custom_tool_call",
    "response_item/custom_tool_call_output", "response_item/reasoning",
    "item/UserMessage", "item/AgentMessage", "item/Reasoning", "item/CommandExecution", "item/FileChange",
];

pub fn classify(line: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Parsed::Unreadable };
    let Some(ty) = v.get("type").and_then(Value::as_str) else { return Parsed::Unreadable };
    let p = &v["payload"];
    let item = &p["item"];
    let kind = match (ty, p["type"].as_str()) {
        ("event_msg", Some("item_completed")) => match item["type"].as_str() {
            Some(t) => format!("item/{t}"),
            None => return Parsed::Unreadable,
        },
        ("event_msg" | "response_item", Some(sub)) => format!("{ty}/{sub}"),
        ("event_msg" | "response_item", None) => return Parsed::Unreadable,
        _ => ty.to_string(),
    };
    // A subagent's rollout is recorded but not read as the owner's turns. None has been seen yet, so both a plain
    // `"subagent"` source and an object keyed `subagent` count as one.
    if kind == "session_meta" && (p["source"] == "subagent" || p["source"].get("subagent").is_some()) {
        return Parsed::Unknown("subagent.session_meta".into());
    }
    if !KNOWN_TYPES.contains(&kind.as_str()) {
        return Parsed::Unknown(kind);
    }
    let str_at = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).map(str::to_string);
    // Only a turn's records need their time; a kind gopola ignores is ignored with or without one.
    let ts = || str_at(&v, "timestamp");
    match kind.as_str() {
        "session_meta" => Parsed::Event(Event::Start {
            cwd: str_at(p, "cwd"),
            branch: str_at(&p["git"], "branch"),
            version: str_at(p, "cli_version"),
            scripted: Some(p["source"] == "exec" || p["originator"] == "codex_exec"),
        }),
        "item/UserMessage" => {
            let Some(content) = joined(item, "text") else { return Parsed::Unreadable };
            let text = strip_private(&content);
            if text.replace("[private]", "").trim().is_empty() {
                return Parsed::Ignored;
            }
            let Some(ts) = ts() else { return Parsed::Unreadable };
            Parsed::Event(Event::Prompt { ts, cwd: None, branch: None, text, entrypoint: None })
        }
        "item/AgentMessage" => {
            let Some(ts) = ts() else { return Parsed::Unreadable };
            let Some(content) = joined(item, "Text") else { return Parsed::Unreadable };
            let text = Some(content).filter(|t| !t.trim().is_empty()).map(|t| strip_private(&t));
            Parsed::Event(Event::Reply { ts, text, files: vec![], tools: vec![], reads: vec![], commands: vec![] })
        }
        "item/CommandExecution" => {
            let Some(ts) = ts() else { return Parsed::Unreadable };
            let Some(argv) = item["command"].as_array() else { return Parsed::Unreadable };
            let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
            let command = match argv[..] {
                [_, "-lc" | "-c", script] => script.to_string(),
                _ => argv.join(" "),
            };
            // `parsed_cmd` is Codex's own reading of the command; a read's path is relative to the command's `cwd`,
            // and with no `cwd` a relative path names no file.
            let dir = item["cwd"].as_str().filter(|c| !c.is_empty()).map(|c| url_path(c).unwrap_or_else(|| c.into()));
            let reads = item["parsed_cmd"].as_array().into_iter().flatten()
                .filter(|c| c["type"] == "read")
                .filter_map(|c| c["path"].as_str())
                .filter(|path| dir.is_some() || Path::new(path).is_absolute())
                .map(|path| Path::new(dir.as_deref().unwrap_or("")).join(path).to_string_lossy().into_owned())
                .collect();
            let (tools, commands) = (vec!["exec_command".to_string()], vec![strip_private(&command)]);
            Parsed::Event(Event::Reply { ts, text: None, files: vec![], tools, reads, commands })
        }
        "item/FileChange" => {
            let Some(ts) = ts() else { return Parsed::Unreadable };
            if item["status"] != "completed" { return Parsed::Ignored }
            let Some(changes) = item["changes"].as_object() else { return Parsed::Unreadable };
            let files = changes.iter().flat_map(|(path, c)| std::iter::once(path.clone()).chain(str_at(c, "move_path"))).collect();
            Parsed::Event(Event::Reply { ts, text: None, files, tools: vec!["apply_patch".into()], reads: vec![], commands: vec![] })
        }
        _ => Parsed::Ignored,
    }
}

/// The `text` of an item's content blocks of one type, joined with newlines; `None` when it has no content list.
fn joined(item: &Value, block_type: &str) -> Option<String> {
    let blocks = item["content"].as_array()?;
    Some(blocks.iter().filter(|b| b["type"] == block_type).filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
}

/// The path of a `file://` URL, `%XX` escapes decoded (a space, a non-ASCII name); `None` for anything else.
fn url_path(url: &str) -> Option<String> {
    let s = url.strip_prefix("file://")?.as_bytes();
    let hex = |b: Option<&u8>| b.and_then(|&b| (b as char).to_digit(16));
    let (mut out, mut i) = (Vec::with_capacity(s.len()), 0);
    while i < s.len() {
        match (s[i], hex(s.get(i + 1)), hex(s.get(i + 2))) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Event, Parsed};

    const EXEC: &str = include_str!("../tests/fixtures/codex/exec-0.167.0.jsonl");
    const TUI: &str = include_str!("../tests/fixtures/codex/tui-0.167.0.jsonl");
    const FORK: &str = include_str!("../tests/fixtures/codex/fork-0.167.0.jsonl");

    fn line<'a>(fixture: &'a str, needle: &str) -> &'a str {
        fixture.lines().find(|l| l.contains(needle)).expect("fixture line")
    }

    /// An `item_completed` record around `item` (JSON text).
    fn item(item: &str) -> String {
        format!(r#"{{"timestamp":"2026-03-02T09:00:10.000Z","ordinal":1,"type":"event_msg","payload":{{"type":"item_completed","thread_id":"t","turn_id":"u","item":{item}}}}}"#)
    }

    /// A `session_meta` record; `source` is JSON text.
    fn meta(source: &str, originator: &str) -> String {
        format!(r#"{{"timestamp":"2026-03-02T09:00:00.000Z","ordinal":0,"type":"session_meta","payload":{{"id":"s","cwd":"/work/demo-app","originator":"{originator}","cli_version":"0.167.0","source":{source},"git":{{"branch":"main"}}}}}}"#)
    }

    fn scripted(l: &str) -> Option<bool> {
        match classify(l) {
            Parsed::Event(Event::Start { scripted, .. }) => scripted,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn meta_gives_cwd_branch_version_scripted() {
        assert_eq!(
            classify(EXEC.lines().next().unwrap()),
            Parsed::Event(Event::Start {
                cwd: Some("/work/demo-app".into()),
                branch: Some("main".into()),
                version: Some("0.167.0".into()),
                scripted: Some(true),
            })
        );
        assert_eq!(scripted(TUI.lines().next().unwrap()), Some(false));
        assert_eq!(scripted(FORK.lines().next().unwrap()), Some(true));
        // Either half of the rule marks a run scripted.
        assert_eq!(scripted(&meta(r#""exec""#, "codex-other")), Some(true));
        assert_eq!(scripted(&meta(r#""cli""#, "codex_exec")), Some(true));
        assert_eq!(scripted(&meta(r#""cli""#, "codex-tui")), Some(false));
    }

    #[test]
    fn user_message_is_a_prompt() {
        assert_eq!(
            classify(line(EXEC, r#""type":"UserMessage""#)),
            Parsed::Event(Event::Prompt {
                ts: "2026-03-02T09:00:10.010Z".into(),
                cwd: None,
                branch: None,
                text: "is the demo app in sync with origin?".into(),
                entrypoint: None,
            })
        );
        // Only `text` blocks are the user's words, even when another block carries a `text` key.
        let blocks = item(r#"{"type":"UserMessage","id":"m","content":[{"type":"text","text":"first"},{"type":"image","url":"x","text":"alt"},{"type":"text","text":"second"}]}"#);
        match classify(&blocks) {
            Parsed::Event(Event::Prompt { text, .. }) => assert_eq!(text, "first\nsecond"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn agent_message_is_a_reply() {
        assert_eq!(
            classify(line(EXEC, r#""phase":"commentary""#)),
            Parsed::Event(Event::Reply {
                ts: "2026-03-02T09:00:12.000Z".into(),
                text: Some("Checking the branch against origin first.".into()),
                files: vec![],
                tools: vec![],
                reads: vec![],
                commands: vec![],
            })
        );
    }

    #[test]
    fn agent_message_without_text_blocks_has_no_reply_text() {
        let empty = item(r#"{"type":"AgentMessage","id":"a","content":[],"phase":"final_answer"}"#);
        match classify(&empty) {
            Parsed::Event(Event::Reply { text, .. }) => assert_eq!(text, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn command_execution_gives_command_and_reads() {
        assert_eq!(
            classify(line(EXEC, r#""type":"read""#)),
            Parsed::Event(Event::Reply {
                ts: "2026-03-02T09:00:15.000Z".into(),
                text: None,
                files: vec![],
                tools: vec!["exec_command".into()],
                reads: vec!["/work/demo-app/README.md".into()],
                commands: vec!["sed -n 1p README.md".into()],
            })
        );
        match classify(line(EXEC, r#""parsed_cmd":[{"type":"unknown""#)) {
            Parsed::Event(Event::Reply { reads, commands, .. }) => {
                assert!(reads.is_empty());
                assert_eq!(commands, vec!["git status --short --branch"]);
            }
            other => panic!("{other:?}"),
        }
        // A listing or a search names a path too, but only a `read` is a read.
        let listing = item(r#"{"type":"CommandExecution","id":"c","command":["/bin/zsh","-lc","ls src && rg demo src"],"cwd":"file:///work/demo-app","parsed_cmd":[{"type":"list_files","cmd":"ls src","path":"src"},{"type":"search","cmd":"rg demo src","query":"demo","path":"src"}]}"#);
        match classify(&listing) {
            Parsed::Event(Event::Reply { reads, .. }) => assert!(reads.is_empty(), "{reads:?}"),
            other => panic!("{other:?}"),
        }
        // `-c` is a shell's script too; any other argv is joined with spaces.
        for (argv, want) in [(r#"["/bin/bash","-c","ls -la"]"#, "ls -la"), (r#"["git","log","-1"]"#, "git log -1")] {
            let l = item(&format!(r#"{{"type":"CommandExecution","id":"c","command":{argv},"cwd":"file:///work/demo-app","parsed_cmd":[]}}"#));
            match classify(&l) {
                Parsed::Event(Event::Reply { commands, .. }) => assert_eq!(commands, vec![want]),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn file_change_gives_edited_files() {
        assert_eq!(
            classify(line(EXEC, r#""type":"FileChange""#)),
            Parsed::Event(Event::Reply {
                ts: "2026-03-02T09:01:07.000Z".into(),
                text: None,
                files: vec!["/work/demo-app/NOTES.md".into()],
                tools: vec!["apply_patch".into()],
                reads: vec![],
                commands: vec![],
            })
        );
        let moved = item(r#"{"type":"FileChange","id":"p","changes":{"/work/demo-app/old.md":{"type":"update","unified_diff":"","move_path":"/work/demo-app/new.md"}},"status":"completed"}"#);
        match classify(&moved) {
            Parsed::Event(Event::Reply { files, .. }) => assert_eq!(files, vec!["/work/demo-app/old.md", "/work/demo-app/new.md"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn declined_file_change_is_not_an_edit() {
        let declined = item(r#"{"type":"FileChange","id":"p","changes":{"/work/demo-app/README.md":{"type":"update"}},"status":"declined"}"#);
        assert_eq!(classify(&declined), Parsed::Ignored);
    }

    #[test]
    fn read_path_is_resolved_against_a_percent_encoded_cwd() {
        for (cwd, want) in [("file:///work/demo%20app", "/work/demo app/README.md"), ("file:///work/caf%C3%A9", "/work/café/README.md")] {
            let l = item(&format!(r#"{{"type":"CommandExecution","id":"c","command":["/bin/zsh","-lc","cat README.md"],"cwd":"{cwd}","parsed_cmd":[{{"type":"read","cmd":"cat README.md","name":"README.md","path":"README.md"}}]}}"#));
            match classify(&l) {
                Parsed::Event(Event::Reply { reads, .. }) => assert_eq!(reads, vec![want]),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn read_path_without_a_file_url_cwd() {
        for (cwd, path, want) in [
            (r#","cwd":"/work/demo%20app""#, "README.md", vec!["/work/demo%20app/README.md"]),
            ("", "README.md", vec![]),
            (r#","cwd":"""#, "README.md", vec![]),
            ("", "/work/demo/README.md", vec!["/work/demo/README.md"]),
        ] {
            let l = item(&format!(r#"{{"type":"CommandExecution","id":"c","command":["/bin/zsh","-lc","cat x"]{cwd},"parsed_cmd":[{{"type":"read","cmd":"cat x","name":"x","path":"{path}"}}]}}"#));
            match classify(&l) {
                Parsed::Event(Event::Reply { reads, .. }) => assert_eq!(reads, want, "{cwd} {path}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn text_goes_through_strip_private() {
        let prompt = item(r#"{"type":"UserMessage","id":"m","content":[{"type":"text","text":"key <private>hunter2</private> AKIAIOSFODNN7EXAMPLE"}]}"#);
        match classify(&prompt) {
            Parsed::Event(Event::Prompt { text, .. }) => assert_eq!(text, "key [private] [redacted:aws-key]"),
            other => panic!("{other:?}"),
        }
        let all_private = item(r#"{"type":"UserMessage","id":"m","content":[{"type":"text","text":"<private>all of it</private>"}]}"#);
        assert_eq!(classify(&all_private), Parsed::Ignored);
        let reply = item(r#"{"type":"AgentMessage","id":"a","content":[{"type":"Text","text":"AKIAIOSFODNN7EXAMPLE"}],"phase":"final_answer"}"#);
        match classify(&reply) {
            Parsed::Event(Event::Reply { text, .. }) => assert_eq!(text.as_deref(), Some("[redacted:aws-key]")),
            other => panic!("{other:?}"),
        }
        let command = item(r#"{"type":"CommandExecution","id":"c","command":["/bin/zsh","-lc","echo AKIAIOSFODNN7EXAMPLE"],"cwd":"file:///work/demo-app","parsed_cmd":[]}"#);
        match classify(&command) {
            Parsed::Event(Event::Reply { commands, .. }) => assert_eq!(commands, vec!["echo [redacted:aws-key]"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn noise_is_ignored() {
        for needle in [r#""type":"token_count""#, r#""type":"task_started""#, r#""type":"message""#, r#""type":"custom_tool_call","#, r#""type":"Reasoning""#] {
            assert_eq!(classify(line(EXEC, needle)), Parsed::Ignored, "{needle}");
        }
        // A kind gopola ignores is ignored with or without a time.
        assert_eq!(classify(r#"{"type":"event_msg","payload":{"type":"token_count"}}"#), Parsed::Ignored);
    }

    #[test]
    fn unknown_record_and_item_are_reported() {
        let compacted = r#"{"timestamp":"2026-03-02T09:00:00.000Z","ordinal":9,"type":"compacted","payload":{"message":"summary"}}"#;
        assert_eq!(classify(compacted), Parsed::Unknown("compacted".into()));
        let event = r#"{"timestamp":"2026-03-02T09:00:00.000Z","ordinal":9,"type":"event_msg","payload":{"type":"exec_command_begin"}}"#;
        assert_eq!(classify(event), Parsed::Unknown("event_msg/exec_command_begin".into()));
        let mcp = item(r#"{"type":"McpToolCall","id":"x","server":"demo","tool":"lookup"}"#);
        assert_eq!(classify(&mcp), Parsed::Unknown("item/McpToolCall".into()));
    }

    #[test]
    fn subagent_meta_is_reported_as_subagent() {
        assert_eq!(classify(&meta(r#""subagent""#, "codex_exec")), Parsed::Unknown("subagent.session_meta".into()));
        assert_eq!(classify(&meta(r#"{"subagent":"review"}"#, "codex_exec")), Parsed::Unknown("subagent.session_meta".into()));
    }

    #[test]
    fn bad_json_and_untimed_turn_records_are_unreadable() {
        assert_eq!(classify("{not json"), Parsed::Unreadable);
        let untimed = line(EXEC, r#""type":"UserMessage""#).replace(r#""timestamp":"2026-03-02T09:00:10.010Z","#, "");
        assert_eq!(classify(&untimed), Parsed::Unreadable);
    }

    #[test]
    fn every_fixture_line_classifies() {
        for (name, fixture) in [("exec", EXEC), ("tui", TUI), ("fork", FORK)] {
            for l in fixture.lines() {
                assert!(matches!(classify(l), Parsed::Event(_) | Parsed::Ignored), "{name}: {l}");
            }
        }
    }
}
