//! Parsing of GitHub Copilot CLI `events.jsonl` lines into the events gopola keeps.
//! Record shapes were read from real sessions on 2026-11-11 (CLI 1.0.81 to 1.0.107); the fixtures are invented.

use crate::transcript::{Event, Parsed, strip_private};
use serde_json::Value;

/// Every record type seen so far. A type outside this list is `Parsed::Unknown`, so a format change shows up in `doctor`.
pub const KNOWN_TYPES: &[&str] = &[
    "session.start", "session.shutdown", "session.model_change", "session.info", "session.context_changed",
    "session.usage_record", "session.usage_checkpoint", "session.auto_mode_resolved",
    "user.message", "system.message", "assistant.turn_start", "assistant.turn_end", "assistant.message",
    "tool.execution_start", "tool.execution_complete", "permission.requested", "permission.completed",
    "hook.start", "hook.end", "skill.invoked", "instruction.update", "subagent.deselected",
    "model.turn_started", "model.turn_ended", "model.model_call_started", "model.model_call_success",
    "model.message", "model.response", "model.messages_snapshot", "model.captured_assignment_context",
];

pub fn classify(line: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Parsed::Unreadable };
    let Some(kind) = v.get("type").and_then(Value::as_str) else { return Parsed::Unreadable };
    if !KNOWN_TYPES.contains(&kind) {
        return Parsed::Unknown(kind.to_string());
    }
    let data = &v["data"];
    let str_at = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).map(str::to_string);
    // Only a turn's records need their time; a type gopola ignores is ignored with or without one.
    let ts = || str_at(&v, "timestamp");
    match kind {
        "session.start" => Parsed::Event(Event::Start {
            cwd: str_at(&data["context"], "cwd"),
            branch: str_at(&data["context"], "branch"),
            version: str_at(data, "copilotVersion"),
            scripted: None,
        }),
        "user.message" => {
            // A `source` marks text a skill injected, not the user's words.
            if data.get("source").is_some_and(|s| !s.is_null()) {
                return Parsed::Ignored;
            }
            let Some(content) = data.get("content").and_then(Value::as_str) else { return Parsed::Unreadable };
            let text = strip_private(content);
            if text.replace("[private]", "").trim().is_empty() {
                return Parsed::Ignored;
            }
            let Some(ts) = ts() else { return Parsed::Unreadable };
            Parsed::Event(Event::Prompt { ts, cwd: None, branch: None, text, entrypoint: None })
        }
        "assistant.message" => {
            let Some(ts) = ts() else { return Parsed::Unreadable };
            let Some(content) = data.get("content").and_then(Value::as_str) else { return Parsed::Unreadable };
            let text = Some(content).filter(|t| !t.trim().is_empty()).map(strip_private);
            let (mut files, mut tools, mut reads, mut commands) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for r in data.get("toolRequests").and_then(Value::as_array).into_iter().flatten() {
                let name = r.get("name").and_then(Value::as_str).unwrap_or("");
                tools.push(name.to_string());
                let arg = |k: &str| str_at(&r["arguments"], k);
                match name {
                    "view" => reads.extend(arg("path")),
                    "edit" | "create" => files.extend(arg("path")),
                    "bash" => commands.extend(arg("command").map(|c| strip_private(&c))),
                    // As Claude's: `Skill <name> <args>`, the args being any other argument text.
                    "skill" => if let Some(s) = arg("skill") {
                        let args: Vec<&str> = r["arguments"].as_object().into_iter().flatten()
                            .filter(|(k, _)| *k != "skill").filter_map(|(_, v)| v.as_str()).collect();
                        commands.push(strip_private(format!("Skill {s} {}", args.join(" ")).trim()));
                    },
                    _ => {}
                }
            }
            Parsed::Event(Event::Reply { ts, text, files, tools, reads, commands })
        }
        "tool.execution_complete" => {
            let files: Vec<String> =
                data.get("fileEdits").and_then(Value::as_array).into_iter().flatten().filter_map(|e| str_at(e, "path")).collect();
            if files.is_empty() {
                return Parsed::Ignored;
            }
            let Some(ts) = ts() else { return Parsed::Unreadable };
            Parsed::Event(Event::Reply { ts, text: None, files, tools: vec![], reads: vec![], commands: vec![] })
        }
        "model.message" => {
            // The title call has no `phase`; an answer (`phase` set) may quote the tag.
            let m = &data["message"];
            let content = m["content"].as_str().unwrap_or("").trim();
            match content.strip_prefix("<session-title>").and_then(|rest| rest.split_once("</session-title>")) {
                Some((title, _)) if m["phase"].is_null() && !title.trim().is_empty() => Parsed::Event(Event::Title(title.trim().to_string())),
                _ => Parsed::Ignored,
            }
        }
        _ => Parsed::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPED: &str = include_str!("../tests/fixtures/copilot/typed-1.0.107.jsonl");
    const SCRIPTED: &str = include_str!("../tests/fixtures/copilot/scripted-1.0.107.jsonl");
    const VSCODE: &str = include_str!("../tests/fixtures/copilot/vscode-1.0.81.jsonl");

    fn line<'a>(fixture: &'a str, needle: &str) -> &'a str {
        fixture.lines().find(|l| l.contains(needle)).expect("fixture line")
    }

    #[test]
    fn start_gives_cwd_branch_version() {
        assert_eq!(
            classify(line(TYPED, r#""type":"session.start""#)),
            Parsed::Event(Event::Start { cwd: Some("/work/demo-app".into()), branch: Some("main".into()), version: Some("1.0.107".into()), scripted: None })
        );
    }

    #[test]
    fn typed_prompt_is_a_prompt() {
        assert_eq!(
            classify(line(TYPED, r#""content":"is the demo app"#)),
            Parsed::Event(Event::Prompt {
                ts: "2026-03-02T09:00:10.000Z".into(),
                cwd: None,
                branch: None,
                text: "is the demo app in sync with origin?".into(),
                entrypoint: None,
            })
        );
    }

    #[test]
    fn skill_injected_message_is_ignored() {
        assert_eq!(classify(line(VSCODE, r#""source":"skill-brainstorming""#)), Parsed::Ignored);
    }

    #[test]
    fn text_goes_through_strip_private() {
        let prompt = r#"{"type":"user.message","data":{"content":"key <private>hunter2</private> done","source":null},"id":"a","timestamp":"2026-03-02T09:00:10.000Z","parentId":null}"#;
        match classify(prompt) {
            Parsed::Event(Event::Prompt { text, .. }) => assert_eq!(text, "key [private] done"),
            other => panic!("{other:?}"),
        }
        let all_private = r#"{"type":"user.message","data":{"content":"<private>all of it</private>","source":null},"id":"a","timestamp":"2026-03-02T09:00:10.000Z","parentId":null}"#;
        assert_eq!(classify(all_private), Parsed::Ignored);
        let reply = r#"{"type":"assistant.message","data":{"content":"AKIAIOSFODNN7EXAMPLE","toolRequests":[{"name":"bash","arguments":{"command":"echo AKIAIOSFODNN7EXAMPLE"}}]},"id":"a","timestamp":"2026-03-02T09:00:10.000Z","parentId":null}"#;
        match classify(reply) {
            Parsed::Event(Event::Reply { text, commands, .. }) => {
                assert_eq!(text.as_deref(), Some("[redacted:aws-key]"));
                assert_eq!(commands, vec!["echo [redacted:aws-key]"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_only_message_has_no_text() {
        assert_eq!(
            classify(line(TYPED, r#""name":"bash""#)),
            Parsed::Event(Event::Reply {
                ts: "2026-03-02T09:00:12.000Z".into(),
                text: None,
                files: vec![],
                tools: vec!["bash".into()],
                reads: vec![],
                commands: vec!["git status --short --branch".into()],
            })
        );
    }

    #[test]
    fn file_requests_and_edits() {
        match classify(line(TYPED, r#""name":"create""#)) {
            Parsed::Event(Event::Reply { files, tools, .. }) => {
                assert_eq!(files, vec!["/work/demo-app/README.md"]);
                assert_eq!(tools, vec!["create"]);
            }
            other => panic!("{other:?}"),
        }
        match classify(line(SCRIPTED, r#""name":"view""#)) {
            Parsed::Event(Event::Reply { reads, files, .. }) => {
                assert_eq!(reads, vec!["/work/demo-app/plan.md"]);
                assert!(files.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            classify(line(TYPED, r#""fileEdits""#)),
            Parsed::Event(Event::Reply {
                ts: "2026-03-02T09:01:02.600Z".into(),
                text: None,
                files: vec!["/work/demo-app/README.md".into()],
                tools: vec![],
                reads: vec![],
                commands: vec![],
            })
        );
    }

    #[test]
    fn title_call_gives_title() {
        assert_eq!(classify(line(TYPED, "session-title")), Parsed::Event(Event::Title("Demo Sync Check".into())));
        let empty = r#"{"type":"model.message","data":{"message":{"content":"<session-title> </session-title>"}},"id":"a","timestamp":"2026-03-02T09:00:11.000Z","parentId":null}"#;
        assert_eq!(classify(empty), Parsed::Ignored);
    }

    #[test]
    fn only_a_phaseless_message_opening_with_the_tag_is_a_title() {
        assert_eq!(classify(line(TYPED, "final_answer")), Parsed::Ignored);
        let answer = r#"{"type":"model.message","data":{"kind":"message","message":{"content":"<session-title>Other Title</session-title>","phase":"final_answer"}},"id":"a","timestamp":"2026-03-02T09:00:11.000Z","parentId":null}"#;
        assert_eq!(classify(answer), Parsed::Ignored);
        let quoted = r#"{"type":"model.message","data":{"kind":"message","message":{"content":"it reads <session-title>Other Title</session-title>"}},"id":"a","timestamp":"2026-03-02T09:00:11.000Z","parentId":null}"#;
        assert_eq!(classify(quoted), Parsed::Ignored);
    }

    #[test]
    fn skill_request_is_a_command() {
        let reply = r#"{"type":"assistant.message","data":{"content":"","toolRequests":[{"name":"skill","arguments":{"skill":"brainstorming"}},{"name":"skill","arguments":{"skill":"gopola","args":"search twg"}}]},"id":"a","timestamp":"2026-03-02T09:00:10.000Z","parentId":null}"#;
        match classify(reply) {
            Parsed::Event(Event::Reply { commands, tools, .. }) => {
                assert_eq!(commands, vec!["Skill brainstorming", "Skill gopola search twg"]);
                assert_eq!(tools, vec!["skill", "skill"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn known_noise_is_ignored() {
        // the first tool.execution_complete in TYPED is a bash call with no fileEdits
        for t in ["system.message", "hook.start", "permission.requested", "tool.execution_start", "tool.execution_complete"] {
            assert_eq!(classify(line(TYPED, &format!(r#""type":"{t}""#))), Parsed::Ignored, "{t}");
        }
    }

    #[test]
    fn known_noise_without_a_timestamp_is_ignored() {
        assert_eq!(classify(r#"{"type":"hook.start","data":{},"id":"x","parentId":null}"#), Parsed::Ignored);
    }

    #[test]
    fn unknown_type_is_reported() {
        let l = r#"{"type":"subagent.started","data":{},"id":"x","timestamp":"2026-03-02T09:00:00.000Z","parentId":null}"#;
        assert_eq!(classify(l), Parsed::Unknown("subagent.started".into()));
    }

    #[test]
    fn bad_json_is_unreadable() {
        assert_eq!(classify("{not json"), Parsed::Unreadable);
    }

    #[test]
    fn every_fixture_line_classifies() {
        for (name, fixture) in [("typed", TYPED), ("scripted", SCRIPTED), ("vscode", VSCODE)] {
            for l in fixture.lines() {
                assert!(matches!(classify(l), Parsed::Event(_) | Parsed::Ignored), "{name}: {l}");
            }
        }
    }
}
