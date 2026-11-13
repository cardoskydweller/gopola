//! End-to-end: a fake ~/.claude/projects tree plus a real git repo; index twice, then query.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod fake_ollama;
use fake_ollama::{fixture, FakeOllama, Rude};

struct World {
    base: PathBuf,
    projects: PathBuf,
    repo: PathBuf,
    db: PathBuf,
}

fn setup() -> World {
    let base = std::env::temp_dir().join(format!("gopola-it-{}-{}", std::process::id(), rand_suffix()));
    let repo = base.join("repos").join("demo-app");
    fs::create_dir_all(&repo).unwrap();
    let projects = base.join("projects");
    fs::create_dir_all(&projects).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    let db = base.join("gopola.db");
    World { base, projects, repo, db }
}

/// Unique per test: the clock alone is not (macOS ticks in microseconds, so two parallel tests got
/// the same directory and raced `git init` in it, failing at random).
fn rand_suffix() -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn git(repo: &Path, args: &[&str]) {
    let st = Command::new("git").arg("-C").arg(repo).args(args).status().unwrap();
    assert!(st.success(), "git {args:?}");
}

fn commit_at(repo: &Path, msg: &str, iso: &str) {
    fs::write(repo.join("f.txt"), msg).unwrap();
    git(repo, &["add", "f.txt"]);
    let st = Command::new("git").arg("-C").arg(repo)
        .env("GIT_AUTHOR_DATE", iso).env("GIT_COMMITTER_DATE", iso)
        .args(["commit", "-q", "-m", msg]).status().unwrap();
    assert!(st.success());
}

/// Nothing listens here. Every run that names no fake Ollama gets this one, so `cargo test` never reaches a
/// real Ollama on the machine (plan Global Constraints).
const NO_OLLAMA: &str = "http://127.0.0.1:9";
/// The model the fakes know and the fixtures were captured with.
const MODEL: &str = "embeddinggemma-2:270m";

fn gopola(w: &World, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .current_dir(&w.repo).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

/// `gopola` against a given Ollama and embedding model, with the exit code (`embed` tests need it).
fn gopola_with(w: &World, ollama: &str, model: &str, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects)
        .env("GOPOLA_OLLAMA", ollama).env("GOPOLA_EMBED_MODEL", model)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .current_dir(&w.repo).args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

fn transcript_dir(w: &World) -> PathBuf {
    let encoded = w.repo.to_string_lossy().replace('/', "-");
    let d = w.projects.join(encoded);
    fs::create_dir_all(&d).unwrap();
    d
}

fn prompt(ts: &str, text: &str, cwd: &str) -> String {
    format!(r#"{{"type":"user","message":{{"role":"user","content":{}}},"timestamp":"{ts}","cwd":"{cwd}","gitBranch":"main","sessionId":"s1"}}"#,
        serde_json::to_string(text).unwrap())
}
fn reply(ts: &str, text: &str, file: Option<&str>) -> String {
    let mut blocks = vec![format!(r#"{{"type":"text","text":{}}}"#, serde_json::to_string(text).unwrap())];
    if let Some(f) = file {
        blocks.push(format!(r#"{{"type":"tool_use","id":"t1","name":"Edit","input":{{"file_path":"{f}","old_string":"a","new_string":"b"}}}}"#));
    }
    format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{}]}}}}"#, blocks.join(","))
}

#[test]
fn index_recent_search_show_and_incremental_reindex() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let file = w.repo.join("src/lib.rs").to_string_lossy().to_string();
    let path = transcript_dir(&w).join("0123abcd-0000-4000-8000-000000000001.jsonl");
    let lines = [
        r#"{"type":"attachment","attachment":{"type":"hook"},"timestamp":"2026-09-01T10:00:00.000Z"}"#.to_string(),
        prompt("2026-09-01T10:00:01.000Z", "--continue", &cwd),
        prompt("2026-09-01T10:00:02.000Z", "add retry to the uploader", &cwd),
        reply("2026-09-01T10:00:05.000Z", "Reading the uploader first.", None),
        reply("2026-09-01T10:00:09.000Z", "Retry added with backoff.\nTests pass.", Some(&file)),
        r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"skill text"}]},"timestamp":"2026-09-01T10:00:10.000Z"}"#.to_string(),
        prompt("2026-09-01T10:05:00.000Z", "now the tests for it", &cwd),
        reply("2026-09-01T10:06:00.000Z", "Three tests cover the retry path.", None),
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    commit_at(&w.repo, "uploader: retry with backoff", "2026-09-01T10:00:08+00:00");
    commit_at(&w.repo, "unrelated, outside the window", "2026-09-02T09:00:00+00:00");

    let (ok, out, err) = gopola(&w, &["index", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files_indexed"], 1);
    assert_eq!(v["turns_written"], 3, "{out}");
    assert_eq!(v["lines_skipped"], 0);

    let (ok, out, _) = gopola(&w, &["recent"]);
    assert!(ok);
    assert!(out.starts_with("# gopola: demo-app, last 1 sessions (of 1)"), "{out}");
    assert!(out.contains("0123abcd  main  3 turns  1 commit"), "{out}"); assert!(!out.contains("1 commits"), "{out}");
    assert!(out.contains("  > add retry to the uploader"), "first prompt skips --continue: {out}");
    assert!(out.contains("  ✓ uploader: retry with backoff"), "{out}");
    assert!(!out.contains("outside the window"), "{out}");
    assert!(out.contains("  < Three tests cover the retry path."), "{out}");

    let (ok, out, _) = gopola(&w, &["search", "backoff"]);
    assert!(ok);
    assert!(out.contains("reply") && out.contains("commit"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "backoff", "--kind", "commit", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "{out}");
    let (_, out, _) = gopola(&w, &["search", "nothing-like-this"]);
    assert!(out.starts_with("no matches"), "{out}");

    let (ok, out, _) = gopola(&w, &["show", "0123"]);
    assert!(ok, "{out}");
    assert!(out.contains("  2  10:00  > add retry to the uploader"), "{out}");
    assert!(out.contains("     files: lib.rs"), "{out}");
    let (_, out, _) = gopola(&w, &["show", "0123", "--turn", "2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns"][0]["reply"], "Retry added with backoff.\nTests pass.");
    assert_eq!(v["turns"][0]["files"][0], file);

    // Append to the open turn and add a new one; the last turn must be rewritten, not duplicated.
    let more = [
        reply("2026-09-01T10:07:00.000Z", "Four tests now.", None),
        prompt("2026-09-01T10:10:00.000Z", "commit it", &cwd),
        reply("2026-09-01T10:11:00.000Z", "Committed.", None),
    ];
    fs::OpenOptions::new().append(true).open(&path).unwrap();
    let mut content = fs::read_to_string(&path).unwrap();
    content.push_str(&(more.join("\n") + "\n"));
    fs::write(&path, content).unwrap();
    commit_at(&w.repo, "uploader: tests", "2026-09-01T10:10:30+00:00");

    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns_written"], 2, "rewrote turn 3 and wrote turn 4: {out}");
    let (_, out, _) = gopola(&w, &["show", "0123", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let turns = v["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 4, "{out}");
    assert_eq!(turns[2]["reply"], "Four tests now.");
    let (_, out, _) = gopola(&w, &["search", "tests", "--kind", "reply", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2, "turn 3 indexed once: {out}");
    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("4 turns  2 commits"), "{out}");

    // Unchanged files are skipped.
    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files_indexed"], 0);

    // A project with no sessions prints nothing and exits 0 (hook must be silent).
    let (ok, out, _) = gopola(&w, &["recent", "--project", "nope"]);
    assert!(ok && out.is_empty());

    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn summarize_calls_the_model_once_with_a_digest_and_search_falls_back_to_any_term() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let path = transcript_dir(&w).join("abcd1234-0000-4000-8000-000000000002.jsonl");
    let mut lines = Vec::new();
    for i in 0..21 {
        let ts = format!("2026-09-03T10:{:02}:00.000Z", i);
        lines.push(prompt(&ts, &format!("step {i}: tighten the uploader retry"), &cwd));
        lines.push(reply(&format!("2026-09-03T10:{:02}:30.000Z", i), &format!("Step {i} done."), None));
    }
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    gopola(&w, &["index", "--quiet"]);

    // Long and unsummarised: the block says so, and search for two terms that never co-occur falls back.
    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("abcd1234  main  21 turns  0 commits  [long, unsummarised]"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "uploader zebra"]);
    assert!(out.starts_with("# no result has every term"), "{out}");
    assert!(out.contains("uploader"), "{out}");

    // A stand-in `claude` records its arguments and answers three lines.
    let fake = w.base.join("fake-claude.sh");
    let log = w.base.join("fake-claude.log");
    fs::write(&fake, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'asked: tighten the uploader retry\\ndone: 21 steps\\nnext: nothing\\n'\n", log.display())).unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("GOPOLA_CLAUDE", &fake)
        .env("COPILOT_HOME", w.base.join("copilot")).current_dir(&w.repo).args(["summarize", "abcd", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["summary"], "asked: tighten the uploader retry\ndone: 21 steps\nnext: nothing");
    let args = fs::read_to_string(&log).unwrap();
    assert!(args.contains("--bare") && args.contains("--tools\n\n") && args.contains("claude-sonnet-5"), "{args}");
    assert!(args.contains("1> step 0: tighten the uploader retry") && args.contains("21< Step 20 done."), "digest missing turns: {args}");
    assert!(args.len() < 16_000, "digest not capped: {}", args.len());

    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("abcd1234  main  21 turns  0 commits\n  asked: tighten the uploader retry\n  done: 21 steps\n  next: nothing"), "{out}");
    assert!(!out.contains("unsummarised"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "steps", "--kind", "summary"]);
    assert!(out.contains("summary") && out.contains("abcd1234"), "{out}");

    // A full rebuild keeps the summary: it cost a model call.
    gopola(&w, &["index", "--full", "--quiet"]);
    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("  done: 21 steps"), "summary lost on --full: {out}");

    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn timeline_file_tools_doctor_and_auto_summary() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let lib = w.repo.join("src/lib.rs").to_string_lossy().to_string();
    let dir = transcript_dir(&w);
    let a = dir.join("aaaa0001-0000-4000-8000-000000000001.jsonl");
    fs::write(&a, [
        prompt("2026-09-01T09:00:00.000Z", "wire the uploader into lib", &cwd),
        reply("2026-09-01T09:00:10.000Z", "Wired.", Some(&lib)),
        prompt("2026-09-01T09:05:00.000Z", "and log failures", &cwd),
        reply("2026-09-01T09:05:10.000Z", "Logging added.", None),
    ].join("\n") + "\n").unwrap();
    commit_at(&w.repo, "lib: wire the uploader", "2026-09-01T09:00:09+00:00");
    let b = dir.join("bbbb0002-0000-4000-8000-000000000002.jsonl");
    let mut lines = Vec::new();
    for i in 0..21 {
        lines.push(prompt(&format!("2026-09-02T10:{i:02}:00.000Z"), &format!("step {i}"), &cwd));
        lines.push(reply(&format!("2026-09-02T10:{i:02}:30.000Z"), &format!("Step {i} done."), None));
    }
    fs::write(&b, lines.join("\n") + "\n").unwrap();
    gopola(&w, &["index", "--quiet"]);

    let (ok, out, err) = gopola(&w, &["timeline", "bbbb:10", "--before", "1", "--after", "1"]);
    assert!(ok, "{err}");
    assert!(out.contains("  9  ") && out.contains("> 10  ") && out.contains(" 11  "), "{out}");
    assert!(out.contains("> step 8") && out.contains("> step 10"), "{out}");
    let (_, out, _) = gopola(&w, &["timeline", "aaaa"]);
    assert!(out.contains("> 2026-09-01 09:00  aaaa0001") && out.contains("  2026-09-02 10:00  bbbb0002"), "{out}");
    let sha = String::from_utf8(Command::new("git").arg("-C").arg(&w.repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    let (_, out, _) = gopola(&w, &["timeline", &sha.trim()[..10]]);
    assert!(out.contains("# aaaa0001: turns around 1"), "{out}");

    let (_, out, _) = gopola(&w, &["file", &lib]);
    assert!(out.contains("aaaa0001:1    > wire the uploader into lib"), "{out}");
    let (_, out, _) = gopola(&w, &["file", "src/nothing.rs"]);
    assert!(out.starts_with("no past session edited"), "{out}");
    let hook_input = format!(r#"{{"session_id":"bbbb0002-0000-4000-8000-000000000002","tool_name":"Read","tool_input":{{"file_path":"{lib}"}}}}"#);
    let run_hook = |input: &str| -> String {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
            .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).current_dir(&w.repo)
            .args(["file", "--hook"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into()
    };
    let first = run_hook(&hook_input);
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.starts_with("gopola: lib.rs was edited in 1 past session(s):"), "{ctx}");
    assert!(ctx.contains("2026-09-01 aaaa0001  > wire the uploader into lib"), "{ctx}");
    assert_eq!(run_hook(&hook_input), "", "second Read of the same file in the same session must stay silent");
    let own = hook_input.replace("bbbb0002-0000-4000-8000-000000000002", "aaaa0001-0000-4000-8000-000000000001");
    assert_eq!(run_hook(&own), "", "a session's own edits are not news to it");

    let (_, out, _) = gopola(&w, &["tools", "--project", "all"]);
    assert!(out.contains("      1     1  Edit"), "{out}");

    let (_, out, _) = gopola(&w, &["recent", "--since", "2026-09-02", "--until", "2026-09-03"]);
    assert!(out.contains("last 1 sessions (of 1)") && out.contains("bbbb0002"), "{out}");

    let fake = w.base.join("fake-claude.sh");
    let counter = w.base.join("calls");
    fs::write(&fake, format!("#!/bin/sh\necho x >> '{}'\nprintf 'asked: steps\\ndone: 21\\nnext: nothing\\n'\n", counter.display())).unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let auto = |input: &str| {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
            .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("GOPOLA_CLAUDE", &fake).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex")).current_dir(&w.repo)
            .args(["summarize", "--auto"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "auto form prints nothing");
    };
    auto(r#"{"session_id":"aaaa0001-0000-4000-8000-000000000001","stop_hook_active":false}"#);
    assert!(!counter.exists(), "a 2-turn session must not be summarised");
    auto(r#"{"session_id":"bbbb0002-0000-4000-8000-000000000002","stop_hook_active":false}"#);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1, "one call for the 21-turn session");
    auto(r#"{"session_id":"bbbb0002-0000-4000-8000-000000000002","stop_hook_active":false}"#);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1, "not again until 20 more turns");
    auto(r#"{"session_id":"bbbb0002-0000-4000-8000-000000000002","stop_hook_active":true}"#);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1, "never inside a stop-hook continuation");
    let (_, out, _) = gopola(&w, &["recent", "--since", "2026-09-02"]);
    assert!(out.contains("  done: 21"), "{out}");

    gopola(&w, &["search", "zebra giraffe"]);
    let (ok, out, _) = gopola(&w, &["doctor"]);
    assert!(ok);
    assert!(out.contains("ok    index is current"), "{out}");
    assert!(out.contains("'zebra giraffe'"), "{out}");
    assert!(out.contains("hook"), "{out}");
    let (_, out, _) = gopola(&w, &["doctor", "--json"]);
    assert!(serde_json::from_str::<serde_json::Value>(&out).unwrap().is_array());

    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn titles_private_spans_digest_footer_and_final_summary() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let dir = transcript_dir(&w);
    let a = dir.join("cccc0003-0000-4000-8000-000000000003.jsonl");
    let mut lines = vec![
        r#"{"type":"ai-title","aiTitle":"Continue session","sessionId":"cccc0003"}"#.to_string(),
        prompt("2026-09-04T09:00:00.000Z", "rotate the api key <private>sk-live-123</private> today", &cwd),
        reply("2026-09-04T09:00:10.000Z", "Rotated. Old key was <private>sk-live-123</private>.", None),
        r#"{"type":"ai-title","aiTitle":"API key rotation","sessionId":"cccc0003"}"#.to_string(),
    ];
    for i in 1..21 {
        lines.push(prompt(&format!("2026-09-04T09:{i:02}:00.000Z"), &format!("step {i}"), &cwd));
        lines.push(reply(&format!("2026-09-04T09:{i:02}:30.000Z"), &format!("Step {i} done."), None));
    }
    fs::write(&a, lines.join("\n") + "\n").unwrap();
    commit_at(&w.repo, "keys: rotate", "2026-09-04T09:00:09+00:00");
    gopola(&w, &["index", "--quiet"]);

    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("cccc0003  main  21 turns  1 commit  API key rotation"), "{out}");
    assert!(out.lines().last().unwrap().starts_with("# gopola search"), "{out}");
    assert!(!out.contains("sk-live"), "{out}");
    let (_, out, _) = gopola(&w, &["recent", "--no-footer"]);
    assert!(!out.contains("# gopola search"), "{out}");
    let (_, out, _) = gopola(&w, &["show", "cccc", "--turn", "1", "--json"]);
    assert!(out.contains("[private]") && !out.contains("sk-live"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "sk-live-123"]);
    assert!(out.starts_with("no matches"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "rotation", "--kind", "title"]);
    assert!(out.contains("title") && out.contains("cccc0003"), "{out}");

    let (ok, out, err) = gopola(&w, &["digest", "--since", "2026-09-01", "--until", "2026-09-30", "--project", "all"]);
    assert!(ok, "{err}");
    assert!(out.contains("2026-09-04") && out.contains("demo-app") && out.contains(" 1 sessions   21 turns   1 commits  API key rotation"), "{out}");
    assert!(out.contains("      ✓ keys: rotate"), "{out}");
    let (_, out, _) = gopola(&w, &["digest", "--since", "2026-09-01", "--until", "2026-09-30", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["days"][0]["titles"][0], "API key rotation");

    let fake = w.base.join("fake-claude.sh");
    let counter = w.base.join("calls");
    fs::write(&fake, format!("#!/bin/sh\necho x >> '{}'\nprintf 'asked: keys\\ndone: rotated\\nnext: nothing\\n'\n", counter.display())).unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let run = |args: &[&str], input: &str| {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
            .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("GOPOLA_CLAUDE", &fake).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex")).current_dir(&w.repo)
            .args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let sid = r#"{"session_id":"cccc0003-0000-4000-8000-000000000003","stop_hook_active":false}"#;
    run(&["summarize", "--auto"], sid);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1, "Stop: first threshold");
    let mut more = fs::read_to_string(&a).unwrap();
    more.push_str(&(vec![prompt("2026-09-04T10:00:00.000Z", "one more", &cwd), reply("2026-09-04T10:00:30.000Z", "Done.", None)].join("\n") + "\n"));
    fs::write(&a, more).unwrap();
    run(&["summarize", "--auto"], sid);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1, "Stop: not yet 20 more");
    run(&["summarize", "--auto", "--final"], sid);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 2, "SessionEnd: once more because it grew");
    run(&["summarize", "--auto", "--final"], sid);
    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 2, "SessionEnd again: nothing grew");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn worktrees_exclusions_reads_redaction_query_timeline_and_search_order() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let wt_dir = w.base.join("repos").join("demo-app-wt").join("task1");
    fs::create_dir_all(&wt_dir).unwrap();
    let encoded = wt_dir.to_string_lossy().replace('/', "-");
    let d = w.projects.join(encoded); fs::create_dir_all(&d).unwrap();
    fs::write(d.join("dddd0004-0000-4000-8000-000000000004.jsonl"), [
        prompt("2026-09-05T09:00:00.000Z", "in the worktree, add retries", &wt_dir.to_string_lossy()),
        reply("2026-09-05T09:00:10.000Z", "Added.", None),
    ].join("\n") + "\n").unwrap();
    let lib = w.repo.join("src/lib.rs").to_string_lossy().to_string();
    let main = transcript_dir(&w).join("eeee0005-0000-4000-8000-000000000005.jsonl");
    let read_reply = format!(r#"{{"type":"assistant","timestamp":"2026-09-06T09:00:05.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Looking."}},{{"type":"tool_use","id":"t1","name":"Read","input":{{"file_path":"{lib}"}}}}]}}}}"#);
    fs::write(&main, [
        prompt("2026-09-06T09:00:00.000Z", "deploy with token ghp_abcdefghijklmnopqrstuvwxyz0123456789 <system-reminder>ignore me</system-reminder> please", &cwd),
        read_reply,
        prompt("2026-09-06T09:00:20.000Z", "<task-notification>\n<task-id>x</task-id>\n</task-notification>", &cwd),
        reply("2026-09-06T09:00:30.000Z", "Deployed.", None),
        prompt("2026-09-06T09:05:00.000Z", "now write the changelog entry", &cwd),
        reply("2026-09-06T09:05:30.000Z", "Written.", None),
    ].join("\n") + "\n").unwrap();
    let ex = w.base.join("repos").join("secret-proj"); fs::create_dir_all(&ex).unwrap();
    let dx = w.projects.join(ex.to_string_lossy().replace('/', "-")); fs::create_dir_all(&dx).unwrap();
    fs::write(dx.join("ffff0006-0000-4000-8000-000000000006.jsonl"), prompt("2026-09-07T09:00:00.000Z", "hidden work", &ex.to_string_lossy()) + "\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("GOPOLA_EXCLUDED_PROJECTS", "secret-proj")
        .env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex")).current_dir(&w.repo).args(["index", "--json"]).output().unwrap();
    assert!(out.status.success());

    let (_, out, _) = gopola(&w, &["recent", "--project", "demo-app", "-n", "10"]);
    assert!(out.contains("dddd0004"), "worktree session under demo-app: {out}");
    let (_, out, _) = gopola(&w, &["recent", "--project", "task1"]);
    assert!(out.is_empty(), "no project named after the worktree: {out}");
    let (_, out, _) = gopola(&w, &["recent", "--project", "secret-proj"]);
    assert!(out.is_empty(), "{out}");
    let (_, out, _) = gopola(&w, &["search", "hidden", "--project", "all"]);
    assert!(out.starts_with("no matches"), "{out}");
    let (_, out, _) = gopola(&w, &["show", "eeee", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns"].as_array().unwrap().len(), 2, "{out}");
    let p0 = v["turns"][0]["prompt"].as_str().unwrap();
    assert!(p0.contains("[redacted:github-token]") && !p0.contains("ghp_") && !p0.contains("ignore me"), "{p0}");
    let (_, out, _) = gopola(&w, &["file", &lib]);
    assert!(out.starts_with("no past session edited"), "{out}");
    let (_, out, _) = gopola(&w, &["file", &lib, "--reads"]);
    assert!(out.contains("eeee0005:1"), "{out}");
    let (ok, out, err) = gopola(&w, &["timeline", "--query", "changelog entry", "--before", "1", "--after", "0"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("# anchor: eeee0005:2") && out.contains("> now write the changelog entry"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "retries changelog", "--project", "all", "--order", "date"]);
    let first = out.lines().find(|l| l.starts_with("2026")).unwrap();
    assert!(first.starts_with("2026-09-06"), "date order puts the newest first: {out}");
    let (_, out, _) = gopola(&w, &["search", "changelog", "--project", "all", "--until", "2026-09-06"]);
    assert!(out.starts_with("no matches"), "--until excludes the 06th: {out}");
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("CLAUDE_PROJECT_DIR", &ex)
        .current_dir(&w.repo).args(["recent"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "", "excluded project via CLAUDE_PROJECT_DIR prints nothing");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn moved_checkout_keeps_its_history_under_the_new_name() {
    let mut w = setup();
    let wt = w.base.join("repos").join("demo-app-wt").join("task1");
    fs::create_dir_all(&wt).unwrap();
    for (dir, id, ts, text) in [(&w.repo, "aaaa0001", "2026-09-01T10:00:00.000Z", "add the uploader"),
                                (&wt, "bbbb0002", "2026-09-02T10:00:00.000Z", "in the worktree, add retries")] {
        let d = w.projects.join(dir.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(format!("{id}-0000-4000-8000-000000000001.jsonl")),
            [prompt(ts, text, &dir.to_string_lossy()), reply(&ts.replace(":00.000Z", ":30.000Z"), "Done.", None)].join("\n") + "\n").unwrap();
    }
    commit_at(&w.repo, "uploader", "2026-09-01T10:00:10+00:00");
    assert!(gopola(&w, &["index"]).0);

    let new = w.base.join("repos").join("renamed-app");
    fs::rename(&w.repo, &new).unwrap();
    fs::rename(w.base.join("repos").join("demo-app-wt"), w.base.join("repos").join("renamed-app-wt")).unwrap();
    let old = w.repo.to_string_lossy().to_string();
    w.repo = new.clone();
    let (ok, out, err) = gopola(&w, &["moved", &old, &new.to_string_lossy()]);
    assert!(ok && out.contains("2 session(s) now under renamed-app (were demo-app)"), "{out}{err}");
    // Claude Code's folder renamed too, so `claude --resume` finds the session from the new path: the row follows it.
    let real = fs::canonicalize(&new).unwrap().to_string_lossy().replace('/', "-");
    fs::rename(w.projects.join(old.replace('/', "-")), w.projects.join(real)).unwrap();

    for args in [&["recent", "-n", "10"][..], &["index", "--full"][..], &["recent", "-n", "10"][..]] {
        let (ok, out, err) = gopola(&w, args);
        assert!(ok, "{err}");
        if args[0] == "recent" {
            assert!(out.starts_with("# gopola: renamed-app,") && out.contains("aaaa0001") && out.contains("bbbb0002"), "{out}");
        }
    }
    let (_, out, _) = gopola(&w, &["search", "uploader"]);
    assert!(out.contains("aaaa0001"), "searched from the new folder: {out}");
    let (ok, out, err) = gopola(&w, &["authored"]);
    assert!(ok && out.contains("1 commits from 1 sessions"), "{out}{err}");
    let (ok, out, err) = gopola(&w, &["weight", "aaaa0001"]);
    assert!(ok && out.starts_with("# weight aaaa0001"), "read from the renamed folder: {out}{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn a_reader_that_closed_its_end_of_the_pipe_stops_gopola_quietly() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_gopola")).arg("version").stdout(writer).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.is_empty(), "as `gopola … | head` once printed: {err}");
}

#[test]
fn doctor_checks_each_hook_under_its_own_event_in_claude_config_dir() {
    let w = setup();
    let claude = w.base.join("claude");
    fs::create_dir_all(claude.join("skills/gopola")).unwrap();
    fs::write(claude.join("skills/gopola/SKILL.md"), "x").unwrap();
    let hook = |cmd: &str| format!(r#"[{{"hooks":[{{"type":"command","command":"$HOME/.local/bin/gopola {cmd}"}}]}}]"#);
    let settings = |post: &str| format!(r#"{{"hooks":{{"SessionStart":{},"UserPromptSubmit":{},"PreToolUse":{},{post}"Stop":{},"SessionEnd":[{{"hooks":[{{"type":"command","command":"gopola summarize --auto --final"}},{{"type":"command","command":"gopola embed --quiet"}}]}}]}}}}"#,
        hook("index --quiet && gopola recent"), hook("index --quiet"), hook("file --hook"), hook("summarize --auto"));

    fs::write(claude.join("settings.json"), settings("")).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("fix   PostToolUse hook missing: `gopola index --quiet`")
        && out.contains("ok    UserPromptSubmit hook wired") && out.contains("ok    gopola skill installed"), "{out}");

    fs::write(claude.join("settings.json"), settings(&format!(r#""PostToolUse":{},"#, hook("index --quiet")))).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionEnd"] {
        assert!(out.contains(&format!("ok    {event} hook wired")), "{event}: {out}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

/// An assistant record that ran tools: each (name, input) becomes a tool_use block.
fn reply_running(ts: &str, text: &str, tools: &[(&str, serde_json::Value)]) -> String {
    let mut blocks = vec![serde_json::json!({"type": "text", "text": text})];
    for (i, (name, input)) in tools.iter().enumerate() {
        blocks.push(serde_json::json!({"type": "tool_use", "id": format!("t{i}"), "name": name, "input": input}));
    }
    serde_json::json!({"type": "assistant", "timestamp": ts, "message": {"role": "assistant", "content": blocks}}).to_string()
}

/// 2026-10-13: "why did you need a twg login in that session?" needed what a session ran, not what it said.
#[test]
fn commands_are_searchable_show_grep_filters_and_an_old_database_backfills() {
    use serde_json::json;
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let path = transcript_dir(&w).join("4567abcd-0000-4000-8000-000000000002.jsonl");
    let heredoc = "python3 - <<'EOF'\nimport re\nh = '## 26. twg across orgs'\nEOF";
    let lines = [
        prompt("2026-10-12T20:00:00.000Z", "build the wrapper from the wishlist", &cwd),
        reply_running("2026-10-12T20:00:05.000Z", "Probing first.", &[
            ("Bash", json!({"command": "TWG_CONFIG_DIR=/tmp/x twg login --help", "description": "probe"})),
            ("Bash", json!({"command": "ls ~/.config"})),
            ("Skill", json!({"skill": "building-cli-tools"})),
        ]),
        prompt("2026-10-12T20:10:00.000Z", "mark the entry", &cwd),
        reply_running("2026-10-12T20:10:05.000Z", "Marked.", &[("Bash", json!({"command": heredoc}))]),
        prompt("2026-10-12T20:20:00.000Z", "and run the tests", &cwd),
        reply_running("2026-10-12T20:20:05.000Z", "Green.", &[("Bash", json!({"command": "cargo test"}))]),
        prompt("2026-10-12T20:30:00.000Z", "is twg still logged in?", &cwd),
        reply("2026-10-12T20:30:05.000Z", "Yes.", None),
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok, "{err}");

    let search = |w: &World| {
        let (_, out, _) = gopola(w, &["search", "twg", "--kind", "command", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut refs: Vec<String> = v.as_array().unwrap().iter().map(|h| h["ref"].as_str().unwrap().to_string()).collect();
        refs.sort();
        refs
    };
    assert_eq!(search(&w), vec!["4567abcd-0000-4000-8000-000000000002:00000001", "4567abcd-0000-4000-8000-000000000002:00000002"],
        "the probe and the heredoc, not the prompt that only says twg");
    let (_, out, _) = gopola(&w, &["search", "building-cli-tools", "--kind", "command"]);
    assert!(out.contains("command") && out.contains("4567abcd:1"), "a skill is a command: {out}");

    let (ok, out, _) = gopola(&w, &["show", "4567", "--grep", "twg"]);
    assert!(ok, "{out}");
    assert!(out.contains("  1  20:00  > build the wrapper") && out.contains("     $ TWG_CONFIG_DIR=/tmp/x twg login --help"), "{out}");
    assert!(!out.contains("$ ls"), "only the commands that mention it: {out}");
    assert!(out.contains("     $ python3 - <<'EOF' … h = '## 26. twg across orgs'"), "a heredoc shows its matching line: {out}");
    assert!(!out.contains("and run the tests"), "a turn that never mentions it is left out: {out}");
    assert!(out.contains("  4  20:30  > is twg still logged in?"), "a prompt that mentions it counts: {out}");
    let (_, out, _) = gopola(&w, &["show", "4567", "--grep", "kubernetes"]);
    assert!(out.contains("no turn mentions 'kubernetes'"), "{out}");
    let (_, out, _) = gopola(&w, &["show", "4567", "--turn", "3", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns"][0]["commands"], json!(["cargo test"]));

    // A database from before commands were kept: no column, no command rows, a summary that cost a model call.
    {
        let c = rusqlite::Connection::open(&w.db).unwrap();
        c.execute_batch(
            "alter table turns drop column commands;
             delete from fts where rowid in (select rid from fts_ref where kind = 'command');
             delete from fts_ref where kind = 'command';
             update sessions set summary = 'kept summary';").unwrap();
    }
    assert!(search(&w).is_empty(), "the old database has no commands");
    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files_indexed"], 1, "the upgrade re-reads the transcript once: {out}");
    assert_eq!(search(&w).len(), 2, "past sessions become searchable by what they ran");
    let c = rusqlite::Connection::open(&w.db).unwrap();
    let summary: String = c.query_row("select summary from sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(summary, "kept summary");
    let turns: i64 = c.query_row("select count(*) from turns", [], |r| r.get(0)).unwrap();
    assert_eq!(turns, 4, "re-read, not duplicated");
    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files_indexed"], 0, "and only once: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// One transcript file, one prompt and one reply per turn, written with the existing `prompt`/`reply`
/// helpers. The file stem is the session id gopola indexes.
fn write_session(w: &World, id: &str, ts: &str, turns: &[(&str, &str)]) {
    let cwd = w.repo.to_string_lossy().to_string();
    let lines: Vec<String> = turns.iter().flat_map(|(p, r)| [prompt(ts, p, &cwd), reply(ts, r, None)]).collect();
    fs::write(transcript_dir(w).join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
}

fn rusqlite_open(p: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(p).unwrap()
}

/// Two sessions: one about running models on Ollama, one about Safari cookies.
fn two_topic_world() -> World {
    let w = setup();
    write_session(&w, "aaaaaaaa-0000-0000-0000-000000000001", "2026-10-12T10:00:00.000Z",
        &[("could we use ollama models here?", "Yes: Ollama 0.40.3 is installed and serving.")]);
    write_session(&w, "bbbbbbbb-0000-0000-0000-000000000002", "2026-10-10T10:00:00.000Z",
        &[("sign-in fails on safari", "The Secure cookie is dropped on http://localhost.")]);
    w
}

/// Replays every fixture in tests/fixtures/ollama against `base`, by the strictness its README sets, and
/// returns how many ran. `exact_row_len`: only the real server is held to the captured row length.
fn replay_ollama_fixtures(base: &str, exact_row_len: bool) -> usize {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ollama");
    let readme = fs::read_to_string(dir.join("README.md")).unwrap();
    let version = readme.lines().find_map(|l| l.strip_prefix("Contract version: ")).expect("the README states its version");
    println!("ollama contract version {version}, against {base}");
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).proxy(None).max_redirects(0).build().into();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(readme.contains(&format!("`{name}`")), "{name} is not described in the README");
        let f: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let mut r = agent.post(format!("{base}/api/embed")).send_json(&f["request"]).unwrap();
        let status = u64::from(r.status().as_u16());
        let body: serde_json::Value = r.body_mut().read_json().unwrap();
        assert_eq!(status, f["status"].as_u64().unwrap(), "{name}: {body}");
        for k in f["keys_include"].as_array().unwrap() {
            assert!(body.get(k.as_str().unwrap()).is_some(), "{name}: no key {k} in {body}");
        }
        if let Some(rows) = f["rows"].as_u64() {
            let got = body["embeddings"].as_array().unwrap();
            assert_eq!(got.len() as u64, rows, "{name}: rows");
            for row in got {
                let len = row.as_array().unwrap().len() as u64;
                match f["row_len"].as_u64() {
                    Some(want) if exact_row_len => assert_eq!(len, want, "{name}: row length"),
                    _ => assert!(len > 0, "{name}: an empty row"),
                }
            }
        }
        if let Some(e) = f["error_contains"].as_str() {
            assert!(body["error"].as_str().unwrap_or("").contains(e), "{name}: {body}");
        }
        println!("  ok  {name}");
    }
    files.len()
}

#[test]
fn embed_fills_vectors_once_per_turn() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    assert!(gopola(&w, &["index", "--quiet"]).0);
    let (code, out, err) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("embedded 2 turns"), "{out}");
    assert!(out.contains("0 left"), "{out}");
    let (_, again, _) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert!(again.contains("embedded 0 turns"), "{again}");
}

/// ureq reads HTTP_PROXY and friends with no loopback exemption; gopola sends turn text to GOPOLA_OLLAMA only.
/// The proxy variables point at the closed port, so a client that honoured them could not embed.
#[test]
fn embed_ignores_proxy_variables() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects)
        .env("GOPOLA_OLLAMA", &fake.url).env("GOPOLA_EMBED_MODEL", MODEL)
        .env("HTTP_PROXY", NO_OLLAMA).env("HTTPS_PROXY", NO_OLLAMA).env("ALL_PROXY", NO_OLLAMA)
        .env("http_proxy", NO_OLLAMA).env("https_proxy", NO_OLLAMA).env("all_proxy", NO_OLLAMA)
        .env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex")).current_dir(&w.repo).args(["embed"]).output().unwrap();
    let (out_s, err) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(out_s.contains("embedded 2 turns"), "{out_s}");
}

#[test]
fn embed_skips_unchanged_turns_after_reindex_and_full() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let after_first = fake.requests.load(std::sync::atomic::Ordering::SeqCst);
    gopola(&w, &["index", "--full", "--quiet"]);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert!(out.contains("embedded 0 turns"), "{out}");
    assert_eq!(fake.requests.load(std::sync::atomic::Ordering::SeqCst), after_first, "no embed call for unchanged text");
}

/// Review Focus 3's other half: an incremental index rewrites a session's last turn whenever its transcript
/// grows, here by a line that is not a turn. The text is the same, so nothing is embedded again.
#[test]
fn embed_skips_the_last_turn_rewritten_with_the_same_text() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let after_first = fake.requests.load(std::sync::atomic::Ordering::SeqCst);
    let path = transcript_dir(&w).join("aaaaaaaa-0000-0000-0000-000000000001.jsonl");
    let mut content = fs::read_to_string(&path).unwrap();
    content.push_str(r#"{"type":"attachment","attachment":{"type":"hook"},"timestamp":"2026-10-12T10:00:09.000Z"}"#);
    content.push('\n');
    fs::write(&path, content).unwrap();
    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns_written"], 1, "the last turn was rewritten: {out}");
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert!(out.contains("embedded 0 turns"), "{out}");
    assert_eq!(fake.requests.load(std::sync::atomic::Ordering::SeqCst), after_first, "no embed call for the same text");
}

/// 16 texts per request: 17 turns take two requests on one kept-alive connection, 16 then 1.
#[test]
fn embed_sends_a_17_turn_session_in_two_batches() {
    let w = setup();
    let turns: Vec<(String, String)> = (1..=17).map(|i| (format!("step {i} of the uploader"), format!("Step {i} done."))).collect();
    let pairs: Vec<(&str, &str)> = turns.iter().map(|(p, r)| (p.as_str(), r.as_str())).collect();
    write_session(&w, "cccccccc-0000-0000-0000-000000000003", "2026-10-11T10:00:00.000Z", &pairs);
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let (code, out, err) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("embedded 17 turns"), "{out}");
    assert_eq!(*fake.batches.lock().unwrap(), vec![16, 1]);
    let n: i64 = rusqlite_open(&w.db).query_row("select count(*) from vectors", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 17);
}

/// Ollama took 1.87 s to load the model cold (`load_duration` in the captured fixture): embed waits it out.
#[test]
fn embed_waits_out_a_cold_model_load() {
    let w = two_topic_world();
    let cold_ms = fixture("embed-array-2")["response"]["load_duration"].as_u64().unwrap() / 1_000_000;
    let fake = FakeOllama::start_rude(8, MODEL, Rude::DelayMs(cold_ms));
    gopola(&w, &["index", "--quiet"]);
    let (code, out, err) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("embedded 2 turns"), "{out}");
}

#[test]
fn prune_keeps_vectors_of_unindexed_sessions() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    // What `index --full` leaves between its delete and its rebuild: no sessions, no turns.
    let c = rusqlite_open(&w.db);
    c.execute_batch("delete from sessions; delete from turns;").unwrap();
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let n: i64 = c.query_row("select count(*) from vectors", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "vectors of sessions not (yet) indexed are kept");
}

/// The prune that does delete: an indexed session whose turn is gone loses that turn's vector, and only it.
#[test]
fn prune_deletes_the_vector_of_a_turn_gone_from_an_indexed_session() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let c = rusqlite_open(&w.db);
    let before: i64 = c.query_row("select count(*) from vectors", [], |r| r.get(0)).unwrap();
    assert_eq!(before, 2);
    c.execute("delete from turns where session_id like 'aaaaaaaa%'", []).unwrap();
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let kept: Vec<String> = c.prepare("select session_id from vectors").unwrap()
        .query_map([], |r| r.get(0)).unwrap().flatten().collect();
    assert_eq!(kept, vec!["bbbbbbbb-0000-0000-0000-000000000002"], "the gone turn's vector, and only it, is pruned");
    let sessions: i64 = c.query_row("select count(*) from sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(sessions, 2, "the session row stays");
}

#[test]
fn embed_reports_missing_model() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, "some-other-model");
    gopola(&w, &["index", "--quiet"]);
    let (code, _, err) = gopola_with(&w, &fake.url, MODEL, &["embed"]);
    assert_eq!(code, 1);
    assert!(err.contains("ollama pull embeddinggemma-2:270m"), "{err}");
    let (code_q, out_q, _) = gopola_with(&w, &fake.url, MODEL, &["embed", "--quiet"]);
    assert_eq!((code_q, out_q.as_str()), (0, ""), "the hook form never fails a session");
}

#[test]
fn embed_reports_unreachable_ollama() {
    let w = two_topic_world();
    gopola(&w, &["index", "--quiet"]);
    let (code, _, err) = gopola_with(&w, NO_OLLAMA, MODEL, &["embed"]);
    assert_eq!(code, 1);
    assert!(err.contains("not reachable at http://127.0.0.1:9"), "{err}");
}

/// Structural (cross-platform-contract-fixtures): every variant of `pub enum EmbedError`, read from
/// src/embed.rs, is produced by a captured fixture or a rude mode of the fake, and gopola names it.
/// A new variant fails here until a case below produces it.
#[test]
fn every_embed_error_is_produced_by_a_fixture_or_a_rude_mode() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/embed.rs")).unwrap();
    let body = src.split("pub enum EmbedError {").nth(1).expect("pub enum EmbedError in src/embed.rs");
    let variants: Vec<&str> = body[..body.find("\n}").unwrap()].lines().map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(|l| l.split(|ch: char| !ch.is_alphanumeric()).next().unwrap()).collect();
    let w = two_topic_world();
    gopola(&w, &["index", "--quiet"]);
    // (variant, what produces it, the Ollama gopola talks to, what its error says)
    let cases: Vec<(&str, &str, String, &[&str])> = vec![
        ("Unreachable", "nothing listening", NO_OLLAMA.to_string(), &["ollama not reachable at http://127.0.0.1:9"]),
        ("ModelMissing", "model-missing.json", FakeOllama::start(8, "some-other-model").url,
            &["model embeddinggemma-2:270m is not pulled (ollama pull embeddinggemma-2:270m)"]),
        ("Bad", "Rude::Status500", FakeOllama::start_rude(8, MODEL, Rude::Status500).url,
            &["ollama answered unexpectedly", "status 500"]),
        ("Bad", "Rude::WrongRowCount", FakeOllama::start_rude(8, MODEL, Rude::WrongRowCount).url,
            &["ollama answered unexpectedly", "1 embeddings for 2 inputs"]),
        ("Bad", "Rude::HangUp", FakeOllama::start_rude(8, MODEL, Rude::HangUp).url,
            &["ollama answered unexpectedly", "Peer disconnected"]),
    ];
    for v in &variants {
        assert!(cases.iter().any(|c| c.0 == *v), "EmbedError::{v} is produced by no fixture or rude mode");
    }
    for (variant, how, url, says) in &cases {
        let (code, _, err) = gopola_with(&w, url, MODEL, &["embed"]);
        assert_eq!(code, 1, "{variant} via {how}: {err}");
        for s in says.iter() {
            assert!(err.contains(s), "{variant} via {how}: {err}");
        }
    }
}

#[test]
fn ollama_fixtures_hold_for_the_fake() {
    let fake = FakeOllama::start(8, MODEL);
    assert_eq!(replay_ollama_fixtures(&fake.url, false), 4);
}

#[test]
#[ignore = "needs a real Ollama with embeddinggemma-2:270m; run by hand (plan Task 4)"]
fn ollama_contract_real_server() {
    // The same fixtures, against the real server, with the captured row length held exactly.
    let base = std::env::var("GOPOLA_OLLAMA").unwrap_or_else(|_| "http://localhost:11434".into());
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).proxy(None).max_redirects(0).build().into();
    let version = agent.get(format!("{base}/api/version")).call().unwrap().body_mut().read_to_string().unwrap();
    println!("ollama at {base}: {version}");
    assert_eq!(replay_ollama_fixtures(&base, true), 4);
    // And gopola end to end against it.
    let w = two_topic_world();
    gopola(&w, &["index", "--quiet"]);
    let (code, out, err) = gopola_with(&w, &base, MODEL, &["embed"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("embedded 2 turns"), "{out}");
    let len: i64 = rusqlite_open(&w.db).query_row("select length(vec) from vectors limit 1", [], |r| r.get(0)).unwrap();
    assert_eq!(len, 768 * 4);
    let (code404, _, err404) = gopola_with(&w, &base, "nope:1b", &["embed"]);
    assert_eq!(code404, 1);
    assert!(err404.contains("ollama pull nope:1b"), "{err404}");
}

/// What 0.3.0 (81f1665) printed for `gopola search safari cookie` on `two_topic_world()`, captured on
/// 2026-10-13 from a build of that commit. `--words` must print exactly this (plan Global Constraints).
const SAFARI_COOKIE_030: &str = "# no result has every term; these match some of 'safari cookie'\n\
2026-10-10  prompt      bbbbbbbb:1     sign-in fails on [safari]\n\
2026-10-10  reply       bbbbbbbb:1     The Secure [cookie] is dropped on http://localhost.\n";

#[test]
fn search_finds_by_meaning_what_words_miss() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, words, _) = gopola_with(&w, &fake.url, MODEL, &["search", "running", "on", "my", "laptop", "--words", "-n", "1"]);
    assert!(!words.contains("aaaaaaaa"), "words alone cannot find it: {words}");
    let (code, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "running", "on", "my", "laptop", "-n", "1"]);
    assert_eq!(code, 0);
    assert!(out.contains("aaaaaaaa:1"), "meaning finds the ollama turn: {out}");
    assert!(out.contains("turn"), "a meaning-only hit is kind `turn`: {out}");
}

#[test]
fn search_words_flag_is_exactly_the_old_search() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari", "cookie", "--words"]);
    assert_eq!(out, SAFARI_COOKIE_030);
    assert_eq!(fake.requests.load(std::sync::atomic::Ordering::SeqCst), 1, "only embed's request: --words asks Ollama nothing");
}

#[test]
fn search_exact_words_still_rank_first() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "Secure", "cookie", "--json"]);
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(hits[0]["ref"].as_str().unwrap().starts_with("bbbbbbbb"), "{out}");
    assert!(["words", "both"].contains(&hits[0]["via"].as_str().unwrap()), "{out}");
}

#[test]
fn search_falls_back_to_words_when_ollama_is_down() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    // Vectors exist and Ollama is gone: one line says why, then the word results.
    let (code, out, _) = gopola_with(&w, NO_OLLAMA, MODEL, &["search", "safari"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("# meaning search off: ollama not reachable at http://127.0.0.1:9"), "{out}");
    assert!(out.contains("bbbbbbbb"), "{out}");
}

#[test]
fn search_falls_back_to_words_when_model_missing() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let other = FakeOllama::start(8, "some-other-model");
    let (_, out, _) = gopola_with(&w, &other.url, MODEL, &["search", "safari"]);
    assert!(out.starts_with("# meaning search off: model embeddinggemma-2:270m is not pulled (ollama pull embeddinggemma-2:270m); words only\n"), "{out}");
    assert!(out.contains("bbbbbbbb"), "{out}");
}

#[test]
fn search_kind_filter_uses_words_only() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let before = fake.requests.load(std::sync::atomic::Ordering::SeqCst);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "laptop", "--kind", "prompt"]);
    assert!(!out.contains("aaaaaaaa"), "{out}");
    assert_eq!(fake.requests.load(std::sync::atomic::Ordering::SeqCst), before, "no query embedding under --kind");
}

/// Nothing embedded for the model: meaning has nothing to add, so search asks Ollama nothing and prints no
/// note; `doctor` and the SessionEnd hook are where a missing embed shows.
#[test]
fn search_without_vectors_makes_no_ollama_call_and_no_note() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari"]);
    assert!(out.contains("bbbbbbbb") && !out.starts_with('#'), "{out}");
    assert_eq!(fake.requests.load(std::sync::atomic::Ordering::SeqCst), 0, "no query embedding without vectors");
    let (_, down, _) = gopola_with(&w, NO_OLLAMA, MODEL, &["search", "safari"]);
    assert!(!down.contains("meaning search off"), "{down}");
}

/// `--order date` on the fused ranking: relevance picks the n hits, date orders them, newest first.
#[test]
fn search_order_date_sorts_the_fused_top_n_by_date() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let refs = |n: &str| -> Vec<String> {
        let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "cookie", "dropped", "--order", "date", "-n", n, "--json"]);
        serde_json::from_str::<serde_json::Value>(&out).unwrap().as_array().unwrap().iter()
            .map(|h| h["ref"].as_str().unwrap()[..8].to_string()).collect()
    };
    assert_eq!(refs("1"), ["bbbbbbbb"], "the word hit is the most relevant, though aaaaaaaa is newer");
    assert_eq!(refs("2"), ["aaaaaaaa", "bbbbbbbb"], "both, newest first");
}

#[test]
fn usage_logs_word_hits_not_fused_hits() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    gopola_with(&w, &fake.url, MODEL, &["search", "zebra", "crossing"]);
    let c = rusqlite_open(&w.db);
    let hits: i64 = c.query_row("select hits from usage where cmd = 'search' order by rowid desc limit 1", [], |r| r.get(0)).unwrap();
    assert_eq!(hits, 0, "doctor must still see that no words matched");
}

/// With --json the note goes to stderr, so stdout stays one valid JSON document (R18).
#[test]
fn search_json_keeps_stdout_valid_when_meaning_is_off() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (code, out, err) = gopola_with(&w, NO_OLLAMA, MODEL, &["search", "safari", "--json"]);
    assert_eq!(code, 0);
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {out}"));
    assert!(!hits.as_array().unwrap().is_empty(), "{out}");
    assert!(err.contains("# meaning search off: ollama not reachable"), "note on stderr: {err}");
}

#[test]
fn doctor_reports_meaning_search_coverage() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let (_, before, _) = gopola_with(&w, &fake.url, MODEL, &["doctor"]);
    assert!(before.contains("2 turn(s) have no vector for embeddinggemma-2:270m"), "{before}");
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, after, _) = gopola_with(&w, &fake.url, MODEL, &["doctor"]);
    assert!(after.contains("meaning search: 2 of 2 turns embedded with embeddinggemma-2:270m"), "{after}");
    assert!(after.contains(&format!("ollama answers at {} with embeddinggemma-2:270m", fake.url)), "{after}");
    assert_eq!(after.matches("SessionEnd hook").count(), 1, "one SessionEnd line for both of its hooks: {after}");
    let (_, down, _) = gopola_with(&w, NO_OLLAMA, MODEL, &["doctor"]);
    assert!(down.contains("ollama not reachable at http://127.0.0.1:9"), "{down}");
}

/// A hung Ollama must not stall search: the query embedding gives up after 10 s (embed batches keep 120 s),
/// says so in the usual note, and the word results still print.
#[test]
fn search_gives_up_on_a_hung_ollama_and_prints_word_results() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let slow = FakeOllama::start_rude(8, MODEL, Rude::DelayMs(14_000));
    let t = std::time::Instant::now();
    let (code, out, _) = gopola_with(&w, &slow.url, MODEL, &["search", "safari"]);
    assert!(t.elapsed().as_secs() < 13, "gave up at 10 s, took {:?}", t.elapsed());
    assert_eq!(code, 0);
    assert!(out.starts_with("# meaning search off: "), "{out}");
    assert!(out.contains("answered too slowly") && out.contains("; words only\n"), "{out}");
    assert!(out.contains("bbbbbbbb"), "{out}");
}

/// The same 10 s for doctor's Ollama probe.
#[test]
fn doctor_probe_gives_up_on_a_hung_ollama() {
    let w = two_topic_world();
    gopola(&w, &["index", "--quiet"]);
    let slow = FakeOllama::start_rude(8, MODEL, Rude::DelayMs(14_000));
    let t = std::time::Instant::now();
    let (_, out, _) = gopola_with(&w, &slow.url, MODEL, &["doctor"]);
    assert!(t.elapsed().as_secs() < 13, "gave up at 10 s, took {:?}", t.elapsed());
    assert!(out.contains("answered too slowly"), "{out}");
}

/// Words found nothing and meaning found something: say so, as 0.3.0 said "no matches" (stderr with --json).
#[test]
fn search_marks_a_meaning_only_answer() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "laptop"]);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("# no result has every word; nearest by meaning"), "{out}");
    assert!(lines.next().unwrap().contains("aaaaaaaa:1"), "{out}");
    let (_, jout, jerr) = gopola_with(&w, &fake.url, MODEL, &["search", "laptop", "--json"]);
    assert!(serde_json::from_str::<serde_json::Value>(&jout).is_ok(), "{jout}");
    assert!(jerr.contains("# no result has every word; nearest by meaning"), "{jerr}");
    let (_, with_words, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari"]);
    assert!(!with_words.contains("nearest by meaning"), "{with_words}");
}

/// Sessions still open are embedded by their SessionEnd hook, so doctor must not tell you to run `gopola embed`.
#[test]
fn doctor_does_not_count_open_sessions_as_unembedded() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let c = rusqlite_open(&w.db);
    c.execute("update sessions set ended_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') where id like 'aaaaaaaa%'", []).unwrap();
    let (_, one, _) = gopola_with(&w, &fake.url, MODEL, &["doctor"]);
    assert!(one.contains("1 turn(s) have no vector for embeddinggemma-2:270m") && one.contains("1 in a session still open"), "{one}");
    c.execute("update sessions set ended_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')", []).unwrap();
    let (_, both, _) = gopola_with(&w, &fake.url, MODEL, &["doctor"]);
    assert!(!both.contains("have no vector"), "{both}");
    assert!(both.contains("meaning search: 0 of 2 turns embedded") && both.contains("2 in a session still open"), "{both}");
}

/// `--meaning` that cannot be honoured says so, once.
#[test]
fn search_meaning_flag_says_why_it_was_not_used() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    let (_, none, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari", "--meaning"]);
    assert!(none.starts_with("# meaning not used: no turn has a vector for embeddinggemma-2:270m"), "{none}");
    assert!(none.contains("bbbbbbbb"), "{none}");
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    let (_, kind, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari", "--meaning", "--kind", "prompt"]);
    assert!(kind.starts_with("# meaning not used: --kind searches words only"), "{kind}");
    let (_, plain, _) = gopola_with(&w, &fake.url, MODEL, &["search", "safari", "--meaning"]);
    assert!(!plain.contains("meaning not used"), "{plain}");
}

/// A vector of another length (a re-pulled model tag) is skipped, never compared on its first dimensions.
#[test]
fn search_skips_vectors_of_another_length() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    gopola(&w, &["index", "--quiet"]);
    gopola_with(&w, &fake.url, MODEL, &["embed"]);
    rusqlite_open(&w.db).execute(
        "update vectors set vec = x'0000803f000000000000000000000000' where session_id like 'aaaaaaaa%'", []).unwrap();
    let (_, out, _) = gopola_with(&w, &fake.url, MODEL, &["search", "laptop", "--meaning", "--json"]);
    assert!(!out.contains("aaaaaaaa"), "the 4-float vector must not be ranked: {out}");
    assert!(out.contains("bbbbbbbb"), "{out}");
}

/// The SessionEnd hook indexes first, so the session's last turn is embedded by the hook that follows it.
#[test]
fn embed_indexes_first() {
    let w = two_topic_world();
    let fake = FakeOllama::start(8, MODEL);
    let (code, out, err) = gopola_with(&w, &fake.url, MODEL, &["embed", "--quiet"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");
    let n: i64 = rusqlite_open(&w.db).query_row("select count(*) from vectors", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "embed --quiet embeds turns no earlier `index` had seen");
}

// handback: a subagent's last report, read from the parent transcript. Fixtures keep the real records'
// keys and carry synthetic text.

/// A parent transcript beside empty subagent files, in the layout Claude Code writes.
fn handback_world(w: &World, agents: &[&str], parent: &[serde_json::Value]) {
    handback_session(w, "5e55-0000", agents, parent)
}

fn handback_session(w: &World, session: &str, agents: &[&str], parent: &[serde_json::Value]) {
    let dir = transcript_dir(w);
    let subs = dir.join(session).join("subagents");
    fs::create_dir_all(&subs).unwrap();
    for a in agents {
        fs::write(subs.join(format!("agent-{a}.jsonl")), "{}\n").unwrap();
        fs::write(subs.join(format!("agent-{a}.meta.json")), r#"{"description":"synthetic","toolUseId":"toolu_x"}"#).unwrap();
    }
    let lines: Vec<String> = parent.iter().map(|v| v.to_string()).collect();
    fs::write(dir.join(format!("{session}.jsonl")), lines.join("\n") + "\n").unwrap();
}

/// What the harness sends: a frame, and the report with every line indented by two spaces.
fn peer_frame(id: &str, report: &str) -> String {
    let body: Vec<String> = report.lines().map(|l| format!("  {l}")).collect();
    format!("<agent-message from=\"{id}\">\n[Subagent hand-back] Synthetic preamble. The report follows:\n{}\n</agent-message>\n", body.join("\n"))
}

fn handback(w: &World, prefix: &str) -> (i32, String, String) {
    gopola_with(w, NO_OLLAMA, MODEL, &["handback", prefix])
}

#[test]
fn handback_prints_a_peer_message_without_its_frame_and_the_last_one_of_a_resumed_agent() {
    let w = setup();
    let user = |text: String| serde_json::json!({"type":"user","isMeta":true,"turnOrigin":"peer","message":{"role":"user","content":text}});
    handback_world(&w, &["a1peer0001"], &[
        user(format!("Another Claude session sent a message:\n{}", peer_frame("a1peer0001", "DONE\nfirst round"))),
        user(format!("Another Claude session sent a message:\n{}", peer_frame("a1peer0001", "DONE\n\n  indented line\nsecond round"))),
    ]);
    let (code, out, err) = handback(&w, "a1peer");
    assert_eq!((code, out.as_str()), (0, "DONE\n\n  indented line\nsecond round\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_reads_a_queued_command_attachment() {
    let w = setup();
    handback_world(&w, &["a2queued01"], &[serde_json::json!({"type":"attachment",
        "attachment":{"type":"queued_command","prompt":peer_frame("a2queued01", "DONE\nreached during a turn"),"commandMode":"prompt"}})]);
    let (code, out, err) = handback(&w, "a2que");
    assert_eq!((code, out.as_str()), (0, "DONE\nreached during a turn\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_reads_the_result_of_a_task_notification_and_ignores_a_later_pointer() {
    let w = setup();
    let note = |result: &str| serde_json::json!({"type":"user","turnOrigin":"task_notification","message":{"role":"user","content":
        format!("<task-notification>\n<task-id>a3notice01</task-id>\n<status>completed</status>\n<summary>Agent \"x\" finished</summary>\n<result>{result}</result>\n<usage></usage>\n</task-notification>")}});
    handback_world(&w, &["a3notice01"], &[
        note("Findings: none.\nSecond line."),
        note("This agent's report was delivered to you as a message from \"a3notice01\" (its SubagentHandback call). Read it there.\n"),
    ]);
    let (code, out, err) = handback(&w, "a3notice");
    assert_eq!((code, out.as_str()), (0, "Findings: none.\nSecond line.\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_reads_the_result_of_a_foreground_agent_call() {
    let w = setup();
    handback_world(&w, &["a4fore0001"], &[serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[{"type":"text","text":"Verdict: fine."}]}]},
        "toolUseResult":{"status":"completed","agentId":"a4fore0001","agentType":"general-purpose","content":[{"type":"text","text":"Verdict: fine."}]}})]);
    let (code, out, err) = handback(&w, "a4fore");
    assert_eq!((code, out.as_str()), (0, "Verdict: fine.\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// 2026-11-05 (a real session): resuming a session's stopped builders needed their briefs and which of them had
/// handed back; a `jq` over the transcript was written for it twice.
#[test]
fn dispatches_lists_each_agent_and_message_with_its_agent_and_whether_a_hand_back_came() {
    use serde_json::json;
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let path = transcript_dir(&w).join("d15b0000-0000-4000-8000-000000000003.jsonl");
    let (a1, a2) = ("a1aaaaaaaaaaaaaaa", "a2bbbbbbbbbbbbbbb");
    let call = |ts: &str, id: &str, name: &str, input: serde_json::Value| json!({"type":"assistant","timestamp":ts,
        "message":{"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}}).to_string();
    let result = |ts: &str, id: &str, text: &str, r: serde_json::Value| json!({"type":"user","timestamp":ts,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":[{"type":"text","text":text}]}]},"toolUseResult":r}).to_string();
    let user = |ts: &str, text: String| json!({"type":"user","timestamp":ts,"message":{"role":"user","content":text}}).to_string();
    let lines = [
        prompt("2026-11-05T06:00:00.000Z", "resume the builders", &cwd),
        call("2026-11-05T06:01:00.000Z", "u1", "Agent", json!({"description":"Build x","prompt":"Brief one.\nSecond line.","model":"sonnet","run_in_background":true})),
        result("2026-11-05T06:01:01.000Z", "u1", "Async agent launched", json!({"status":"async_launched","agentId":a1})),
        call("2026-11-05T06:02:00.000Z", "u2", "Agent", json!({"description":"Build y","prompt":"Brief two."})),
        result("2026-11-05T06:02:01.000Z", "u2", &format!("Async agent launched successfully.\nagentId: {a2} (internal ID)"), serde_json::Value::Null),
        call("2026-11-05T06:03:00.000Z", "u3", "Agent", json!({"description":"Build z","prompt":"Brief three."})),
        result("2026-11-05T06:03:01.000Z", "u3", "denied", json!("Permission for this action was denied by the auto mode classifier.")),
        user("2026-11-05T06:10:00.000Z", format!("Another Claude session sent a message:\n{}", peer_frame(a1, "DONE\nbuilt x"))),
        user("2026-11-05T06:11:00.000Z", format!("<task-notification>\n<task-id>{a2}</task-id>\n<status>killed</status>\n<result>I'll start by reading.</result>\n</task-notification>")),
        call("2026-11-05T06:20:00.000Z", "u4", "SendMessage", json!({"to":a2,"recipient":a2,"recipient_kind":"agent","type":"message","message":"Fix round 1: carry on.\nMore.","content":"Fix round 1: carry on.…"})),
        result("2026-11-05T06:20:01.000Z", "u4", "sent", json!({"success":true,"resumedAgentId":a2})),
        call("2026-11-05T06:21:00.000Z", "u5", "SendMessage", json!({"to":"demo-app-ce","recipient":"demo-app-ce","recipient_kind":"name","summary":"Ask demo-app","type":"message","message":"Which repos?","content":"Which repos?"})),
        result("2026-11-05T06:21:01.000Z", "u5", "sent", json!({"success":true})),
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok, "{err}");

    let (ok, out, err) = gopola(&w, &["dispatches", "d15b"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("# dispatches d15b0000: 3 Agent call(s), 2 SendMessage call(s); a hand-back after 2 of the 3 that reached an agent\n"), "{out}");
    assert!(out.contains(&format!("2026-11-05 06:01:00Z  Agent  \"Build x\"  → agent {a1} (sonnet, background)  async launched  hand-back: returned 06:10:00Z\nBrief one.\nSecond line.\n")), "{out}");
    assert!(out.contains(&format!("Agent  \"Build y\"  → agent {a2}  async launched  hand-back: stopped 06:11:00Z\n")), "the id from the content line: {out}");
    assert!(out.contains("Agent  \"Build z\"  → no agent  refused: Permission for this action was denied"), "{out}");
    assert!(out.contains(&format!("SendMessage  \"Fix round 1: carry on.\"  → agent {a2} (agent)  sent  hand-back: none\nFix round 1: carry on.\nMore.\n")), "the whole message, not the cut copy: {out}");
    assert!(out.contains("SendMessage  \"Ask demo-app\"  → demo-app-ce (name)  sent  \n"), "{out}");

    let (_, out, _) = gopola(&w, &["dispatches", "d15b", "--grep", "Brief two", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let d = v["dispatches"].as_array().unwrap();
    assert_eq!(d.len(), 1, "{out}");
    assert_eq!((d[0]["agent"].as_str(), d[0]["handback"]["state"].as_str()), (Some(a2), Some("stopped")), "{out}");
    let (_, out, _) = gopola(&w, &["dispatches", "d15b", "--head", "1"]);
    assert!(out.contains("Brief one.\n… 1 more lines"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_exits_1_while_the_agent_has_returned_nothing() {
    let w = setup();
    handback_world(&w, &["a5wait0001"], &[serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[{"type":"text","text":"Async agent launched"}]}]},
        "toolUseResult":{"status":"async_launched","agentId":"a5wait0001","description":"synthetic"}})]);
    let (code, out, err) = handback(&w, "a5wait");
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("a5wait0001 has no hand-back in") && err.contains("5e55-0000.jsonl"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_exits_2_for_a_prefix_that_matches_two_agents_or_none_given() {
    let w = setup();
    handback_world(&w, &["a6twin0001", "a6twin0002"], &[]);
    let (code, _, err) = handback(&w, "a6");
    assert_eq!(code, 2);
    assert!(err.contains("a6twin0001") && err.contains("a6twin0002"), "{err}");
    let (code, _, _) = gopola_with(&w, NO_OLLAMA, MODEL, &["handback"]);
    assert_eq!(code, 2);
    fs::remove_dir_all(&w.base).unwrap();
}

fn queued_note(ts: &str, id: &str, result: &str) -> serde_json::Value {
    serde_json::json!({"type":"attachment","timestamp":ts,"attachment":{"type":"queued_command","commandMode":"task-notification",
        "prompt":format!("<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n<summary>Agent \"x\" finished</summary>\n<result>{result}</result>\n</task-notification>")}})
}

#[test]
fn handback_reads_a_task_notification_queued_during_a_turn() {
    let w = setup();
    handback_world(&w, &["a7qnote001"], &[queued_note("2026-09-01T10:00:00.000Z", "a7qnote001", "Queued note report.\nLine two.")]);
    let (code, out, err) = handback(&w, "a7qnote");
    assert_eq!((code, out.as_str()), (0, "Queued note report.\nLine two.\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_prefers_a_later_queued_notification_to_an_earlier_report() {
    let w = setup();
    let early = serde_json::json!({"type":"user","timestamp":"2026-09-01T10:00:00.000Z","message":{"role":"user","content":
        format!("Another Claude session sent a message:\n{}", peer_frame("a8resume01", "first report"))}});
    handback_world(&w, &["a8resume01"], &[early, queued_note("2026-09-01T10:05:00.000Z", "a8resume01", "second report")]);
    let (code, out, err) = handback(&w, "a8resume");
    assert_eq!((code, out.as_str()), (0, "second report\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn handback_takes_the_latest_report_across_two_parent_sessions_of_one_agent() {
    let w = setup();
    handback_session(&w, "5e55-0001", &["a9twice001"], &[serde_json::json!({"type":"user","timestamp":"2026-09-01T10:00:00.000Z","message":{"role":"user","content":"nothing yet"}})]);
    handback_session(&w, "5e55-0002", &["a9twice001"], &[queued_note("2026-09-01T11:00:00.000Z", "a9twice001", "only in the second session")]);
    let (code, out, err) = handback(&w, "a9twice");
    assert_eq!((code, out.as_str()), (0, "only in the second session\n"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

// ---- authored / blame -------------------------------------------------------------------------------

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// git with both dates pinned (and another author when given), so a commit lands inside or outside the window.
fn git_at(repo: &Path, iso: &str, author: Option<(&str, &str)>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).env("GIT_AUTHOR_DATE", iso).env("GIT_COMMITTER_DATE", iso);
    if let Some((name, email)) = author {
        cmd.env("GIT_AUTHOR_NAME", name).env("GIT_AUTHOR_EMAIL", email);
    }
    let st = cmd.args(args).status().unwrap();
    assert!(st.success(), "git {args:?}");
    git_out(repo, &["rev-parse", "HEAD"])
}

fn commit_files(repo: &Path, files: &[(&str, &str)], msg: &str, iso: &str, author: Option<(&str, &str)>) -> String {
    for (f, text) in files {
        let p = repo.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    git(repo, &["add", "-A"]);
    git_at(repo, iso, author, &["commit", "-q", "-m", msg])
}

/// One session, 10:00–10:10, and a history around it: commits on the ref by sha, a cherry-pick after the
/// session (patch-id), a rewrite with new content (subject), a pruned branch whose copy can only match by
/// subject, a teammate's commit inside the window, a branch that never lands, a merge and a version bump.
fn authored_world() -> (World, std::collections::HashMap<&'static str, String>) {
    let w = setup();
    let r = &w.repo;
    let mut sha = std::collections::HashMap::new();
    git(r, &["checkout", "-q", "-b", "dev"]);
    let base = commit_files(r, &[("src/app.txt", "one\ntwo\nthree\n"), ("VERSION", "1.0.0\n")], "base", "2026-09-01T09:00:00+00:00", None);
    sha.insert("beta", commit_files(r, &[("src/app.txt", "ONE\ntwo\nthree\n")], "feat: beta", "2026-09-01T10:01:00+00:00", None));
    sha.insert("gamma", commit_files(r, &[("src/app.txt", "ONE\ntwo\nTHREE\n")], "teammate: gamma", "2026-09-01T10:02:00+00:00",
        Some(("Other Person", "other@example.com"))));
    for (branch, file, text, msg, iso) in [
        ("work", "src/alpha.txt", "alpha\n", "feat: alpha", "2026-09-01T10:03:00+00:00"),
        ("side", "src/delta.txt", "delta draft\n", "feat: delta", "2026-09-01T10:04:00+00:00"),
        ("tmp", "src/eps.txt", "eps\n", "feat: epsilon", "2026-09-01T10:05:00+00:00"),
        ("wip", "src/wip.txt", "wip\n", "wip: never merged", "2026-09-01T10:06:00+00:00"),
        ("feature", "src/zeta.txt", "zeta\n", "feat: zeta", "2026-09-01T10:07:00+00:00"),
    ] {
        git(r, &["checkout", "-q", "-b", branch, &base]);
        sha.insert(msg, commit_files(r, &[(file, text)], msg, iso, None));
    }
    git(r, &["checkout", "-q", "dev"]);
    sha.insert("merge", git_at(r, "2026-09-01T10:08:00+00:00", None, &["merge", "-q", "--no-ff", "-m", "Merge branch 'feature'", "feature"]));
    sha.insert("bump", commit_files(r, &[("VERSION", "1.0.1\n")], "chore: bump version to 1.0.1", "2026-09-01T10:09:00+00:00", None));
    // After the session: the side branches reach dev rewritten, and someone edits by hand.
    sha.insert("alpha2", git_at(r, "2026-09-01T11:00:00+00:00", None, &["cherry-pick", "--allow-empty", &sha["feat: alpha"]]));
    sha.insert("delta2", commit_files(r, &[("src/delta.txt", "delta final\nsecond line\n")], "feat: delta", "2026-09-01T11:10:00+00:00", None));
    sha.insert("eps2", git_at(r, "2026-09-01T11:20:00+00:00", None, &["cherry-pick", &sha["feat: epsilon"]]));
    sha.insert("hand", commit_files(r, &[("src/app.txt", "ONE\nTWO\nTHREE\n")], "hand: outside the window", "2026-09-01T12:00:00+00:00", None));

    let cwd = w.repo.to_string_lossy().to_string();
    let lines = [prompt("2026-09-01T10:00:00.000Z", "build the features", &cwd), reply("2026-09-01T10:10:00.000Z", "Done.", None)];
    fs::write(transcript_dir(&w).join("0a0a0a0a-0000-4000-8000-000000000001.jsonl"), lines.join("\n") + "\n").unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok, "{err}");
    // After indexing: a commit inside the window that the commits table never saw (only the live re-run
    // finds it), and the epsilon branch pruned, so its recorded sha has no object left.
    sha.insert("eta", commit_files(r, &[("src/eta.txt", "eta\n")], "feat: eta", "2026-09-01T10:09:30+00:00", None));
    git(r, &["branch", "-q", "-D", "tmp"]);
    git(r, &["reflog", "expire", "--expire=now", "--all"]);
    git(r, &["gc", "-q", "--prune=now"]);
    assert!(!Command::new("git").arg("-C").arg(r).args(["cat-file", "-e", &sha["feat: epsilon"]]).status().unwrap().success());
    (w, sha)
}

fn by_subject(v: &serde_json::Value) -> std::collections::BTreeMap<String, (String, String)> {
    v["commits"].as_array().unwrap().iter()
        .map(|c| (c["subject"].as_str().unwrap().to_string(), (c["sha"].as_str().unwrap().to_string(), c["match"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn authored_follows_session_commits_onto_the_ref() {
    let (w, sha) = authored_world();
    let repo = w.repo.to_string_lossy().to_string();
    let (ok, out, err) = gopola(&w, &["authored", &repo, "--ref", "dev", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let got = by_subject(&v);
    let want = [
        ("feat: beta", "beta", "sha"), ("feat: alpha", "alpha2", "patch-id"), ("feat: delta", "delta2", "subject-only"),
        ("feat: epsilon", "eps2", "subject-only"), ("feat: zeta", "feat: zeta", "sha"), ("Merge branch 'feature'", "merge", "sha"),
        ("chore: bump version to 1.0.1", "bump", "sha"), ("feat: eta", "eta", "sha"),
    ];
    assert_eq!(got.len(), want.len(), "{out}");
    for (subject, key, kind) in want {
        assert_eq!(got.get(subject), Some(&(sha[key].clone(), kind.to_string())), "{subject}: {out}");
    }
    assert!(!got.contains_key("teammate: gamma") && !got.contains_key("hand: outside the window") && !got.contains_key("base"), "{out}");
    assert_eq!(v["not_counted"]["other_authors"]["Other Person"], 1, "{out}");
    assert_eq!(v["not_counted"]["other_author_commits"][0]["sha"], sha["gamma"].as_str(), "{out}");
    assert_eq!(v["not_counted"]["not_on_ref"], 1, "{out}");
    assert_eq!(v["not_counted"]["not_on_ref_commits"][0]["subject"], "wip: never merged", "{out}");
    assert_eq!(v["identity"], "t@example.com");
    assert_eq!(v["commits"][0]["sessions"][0], "0a0a0a0a-0000-4000-8000-000000000001", "{out}");

    // Merges and bumps go only when asked, and the output says how many.
    let (ok, out, _) = gopola(&w, &["authored", &repo, "--ref", "dev", "--no-merges", "--no-bumps"]);
    assert!(ok);
    assert!(out.contains("6 commits from 1 sessions"), "{out}");
    assert!(out.contains("1 merges (--no-merges) · 1 version bumps (--no-bumps)"), "{out}");
    assert!(out.contains("1 recorded by other authors (Other Person 1)"), "{out}");
    assert!(out.contains(&format!("{}  0a0a0a0a           patch-id", &sha["alpha2"][..9])), "{out}");

    // --path keeps the lines under it and counts the commits that change nothing there.
    let (ok, out, _) = gopola(&w, &["authored", &repo, "--ref", "dev", "--path", "src", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["summary"]["commits"], 6, "{out}");
    assert_eq!((v["summary"]["added"].as_i64(), v["summary"]["deleted"].as_i64()), (Some(7), Some(1)), "{out}");
    assert_eq!(v["summary"]["files"], 6, "{out}");
    assert_eq!(v["summary"]["outside_path"], 2, "{out}");
    assert_eq!(v["summary"]["top_dirs"][0]["dir"], "src", "{out}");

    // A session that starts after --since has nothing.
    let (ok, out, _) = gopola(&w, &["authored", &repo, "--ref", "dev", "--since", "2026-09-02"]);
    assert!(ok);
    assert!(out.contains("0 commits from 0 sessions"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn blame_marks_the_lines_session_commits_wrote() {
    let (w, sha) = authored_world();
    let (ok, out, err) = gopola(&w, &["blame", "src/app.txt", "--ref", "dev"]);
    assert!(ok, "{err}");
    assert!(out.contains("# src/app.txt @ dev: 1 of 3 lines from Claude sessions (33.3%)"), "{out}");
    assert!(out.contains(&format!("    1 {} 0a0a0a0a   ONE", &sha["beta"][..9])), "{out}");
    assert!(out.contains(&format!("    2 {} -          TWO", &sha["hand"][..9])), "{out}");
    assert!(out.contains(&format!("    3 {} -          THREE", &sha["gamma"][..9])), "{out}");

    let (ok, out, _) = gopola(&w, &["blame", "src", "--ref", "dev", "--summary", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["claude"].as_u64(), v["total"].as_u64()), (Some(7), Some(9)), "{out}");
    assert_eq!(v["files"][0]["file"], "src/delta.txt", "most Claude lines first: {out}");
    let app = v["files"].as_array().unwrap().iter().find(|f| f["file"] == "src/app.txt").unwrap();
    assert_eq!((app["claude"].as_u64(), app["total"].as_u64()), (Some(1), Some(3)), "{out}");

    // A file on the ref that is not on disk (another branch is checked out).
    git(&w.repo, &["checkout", "-q", "work"]);
    assert!(!w.repo.join("src/zeta.txt").exists());
    let (ok, out, err) = gopola(&w, &["blame", "src/zeta.txt", "--ref", "dev", "--summary"]);
    assert!(ok, "{err}");
    assert!(out.contains("# blame src/zeta.txt @ dev: 1 of 1 lines from Claude sessions (100.0%) in 1 of 1 files"), "{out}");
    let (ok, _, err) = gopola(&w, &["blame", "src/nowhere.txt", "--ref", "dev"]);
    assert!(!ok && err.contains("not on dev: src/nowhere.txt"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn blame_ignores_whitespace_by_default_and_prints_ranges() {
    let w = setup();
    let r = &w.repo;
    commit_files(r, &[("f.txt", "w\nx\ny\nz\nq\n")], "feat: session work", "2026-09-01T10:05:00+00:00", None);
    commit_files(r, &[("f.txt", "w\n  x\ny\n    z\nq\n")], "style: reindent", "2026-09-01T12:00:00+00:00", None);
    commit_files(r, &[("f.txt", "w\n  x\ny\n    z\nhand\n")], "hand: last line", "2026-09-01T12:30:00+00:00", None);
    let cwd = w.repo.to_string_lossy().to_string();
    let lines = [prompt("2026-09-01T10:00:00.000Z", "write it", &cwd), reply("2026-09-01T10:10:00.000Z", "Done.", None)];
    fs::write(transcript_dir(&w).join("0a0a0a0a-0000-4000-8000-000000000001.jsonl"), lines.join("\n") + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);

    let (ok, out, err) = gopola(&w, &["blame", "f.txt", "--summary"]);
    assert!(ok, "{err}");
    assert!(out.contains("4 of 5 lines from Claude sessions (80.0%)"), "{out}");
    let (_, out, _) = gopola(&w, &["blame", "f.txt", "--summary", "--no-ignore-whitespace"]);
    assert!(out.contains("2 of 5 lines from Claude sessions (40.0%)"), "{out}");

    let (_, out, _) = gopola(&w, &["blame", "f.txt", "--ranges"]);
    assert!(out.contains("0a0a0a0a  1-4\n"), "{out}");
    let (_, out, _) = gopola(&w, &["blame", "f.txt", "--ranges", "--no-ignore-whitespace"]);
    assert!(out.contains("0a0a0a0a  1 3\n"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn two_clones_of_one_repo_are_two_projects() {
    let w = setup();
    let other = w.base.join("repos").join("Other").join("demo-app");
    fs::create_dir_all(&other).unwrap();
    for (dir, id, text) in [(&w.repo, "aaaa0001", "work in the first clone"), (&other, "bbbb0002", "work in the second clone")] {
        let d = w.projects.join(dir.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&d).unwrap();
        let cwd = dir.to_string_lossy().to_string();
        fs::write(d.join(format!("{id}-0000-4000-8000-000000000001.jsonl")),
            [prompt("2026-09-01T10:00:00.000Z", text, &cwd), reply("2026-09-01T10:00:10.000Z", "Done.", None)].join("\n") + "\n").unwrap();
    }
    assert!(gopola(&w, &["index"]).0);
    let from = |dir: &Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_gopola")).env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects)
            .env("GOPOLA_OLLAMA", NO_OLLAMA).current_dir(dir).args(["recent"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let (first, second) = (from(&w.repo), from(&other));
    assert!(first.contains("# gopola: demo-app,") && first.contains("first clone") && !first.contains("second clone"), "{first}");
    assert!(second.contains("# gopola: Other/demo-app,") && second.contains("second clone") && !second.contains("first clone"), "{second}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// A transcript line with its `sessionId` (and `entrypoint`, when given) set.
fn as_session(line: String, id: &str, entrypoint: Option<&str>) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&line).unwrap();
    v["sessionId"] = id.into();
    if let Some(e) = entrypoint { v["entrypoint"] = e.into(); }
    v.to_string()
}

#[test]
fn import_files_a_cloud_transcript_from_a_branch_or_a_file_under_this_repo() {
    let w = setup();
    // git names the checkout by its real path (/private/var/… on macOS); Claude Code's copy must name the same one.
    let repo = fs::canonicalize(&w.repo).unwrap();
    commit_at(&repo, "base", "2026-10-16T09:00:00+00:00");
    let cloud = [
        as_session(prompt("2026-10-16T14:22:30.000Z", "write the track 5 plan", "/home/user/demo-app"), "cloud-0001", Some("remote")),
        as_session(reply("2026-10-16T14:30:00.000Z", "The plan has 11 tasks.", None), "cloud-0001", None),
        as_session(prompt("2026-10-16T22:04:00.000Z", "shut the VM down", "/home/user/demo-app"), "cloud-0001", Some("remote")),
    ].join("\n") + "\n";
    // The export step in the README: one commit on its own branch, no checkout.
    fs::write(w.base.join("t.jsonl"), &cloud).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let blob = run(&["hash-object", "-w", &w.base.join("t.jsonl").to_string_lossy()]);
    let tree = {
        let mut mk = Command::new("git").arg("-C").arg(&repo).arg("mktree")
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        use std::io::Write;
        mk.stdin.take().unwrap().write_all(format!("100644 blob {blob}\ttranscript.jsonl\n").as_bytes()).unwrap();
        String::from_utf8(mk.wait_with_output().unwrap().stdout).unwrap().trim().to_string()
    };
    let commit = run(&["commit-tree", &tree, "-m", "transcript"]);
    run(&["update-ref", "refs/heads/transcript/plan", &commit]);

    let (ok, out, err) = gopola(&w, &["import", "transcript/plan:transcript.jsonl"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("imported cloud-00 into demo-app: 2 turns, 2026-10-16T14:22:30.000Z → 2026-10-16T22:04:00.000Z"), "{out}");
    let (ok, out, _) = gopola(&w, &["show", "cloud-0001"]);
    assert!(ok && out.contains("> write the track 5 plan") && out.contains("> shut the VM down"), "{out}");
    let (_, out, _) = gopola(&w, &["search", "VM", "--kind", "prompt", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap().as_array().unwrap().len(), 1, "{out}");
    let (ok, out, err) = gopola(&w, &["weight", "cloud-0001"]);
    assert!(ok && out.starts_with("# weight cloud-00"), "{out}{err}");

    // `claude -p --teleport` writes its copy under ~/.claude/projects marked sdk-cli, which index reads as nothing.
    let tp_dir = w.projects.join(repo.to_string_lossy().replace('/', "-"));
    fs::create_dir_all(&tp_dir).unwrap();
    let cwd = repo.to_string_lossy().to_string();
    let copy = [
        as_session(prompt("2026-10-17T08:00:00.000Z", "review the cookie probe", &cwd), "tp-0002", Some("sdk-cli")),
        as_session(reply("2026-10-17T08:05:00.000Z", "Safari keeps it.", None), "tp-0002", None),
    ].join("\n") + "\n";
    fs::write(tp_dir.join("tp-0002.jsonl"), &copy).unwrap();
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["show", "tp-0002", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["turns"].as_array().map(Vec::len), Some(0), "{out}");
    let (ok, out, err) = gopola(&w, &["import", &tp_dir.join("tp-0002.jsonl").to_string_lossy()]);
    assert!(ok && out.starts_with("imported tp-0002 into demo-app: 1 turns"), "{out}{err}");
    // The import keeps the session through later runs that see Claude Code's copy again (grown, too), and through --full.
    let grown = copy + &as_session(prompt("2026-10-17T09:00:00.000Z", "a later turn in the local copy", &cwd), "tp-0002", Some("sdk-cli")) + "\n";
    fs::write(tp_dir.join("tp-0002.jsonl"), grown).unwrap();
    for args in [&["index"][..], &["index", "--full"][..]] {
        assert!(gopola(&w, args).0);
        let (_, out, _) = gopola(&w, &["show", "tp-0002"]);
        assert!(out.contains("> review the cookie probe") && !out.contains("later turn"), "{args:?}: {out}");
        let (_, out, _) = gopola(&w, &["show", "cloud-0001"]);
        assert!(out.contains("> write the track 5 plan"), "{args:?}: {out}");
    }
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("index is current"), "Claude Code's copy is not stale: {out}");

    // Imported again from another repository: one copy remains, so later runs index nothing.
    let other = w.base.join("repos").join("other-app");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    let out = Command::new(env!("CARGO_BIN_EXE_gopola")).env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects)
        .env("GOPOLA_OLLAMA", NO_OLLAMA).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex")).current_dir(&other).args(["import", &w.base.join("t.jsonl").to_string_lossy()]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("imported cloud-00 into other-app"), "{out:?}");
    for _ in 0..2 {
        let (_, out, _) = gopola(&w, &["index", "--json"]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["files_indexed"], 0, "{out}");
    }

    // Refusals.
    fs::write(w.base.join("not.jsonl"), "{\"type\":\"user\"}\n").unwrap();
    for (arg, says) in [
        ("no-such-file", "no such file, and not a <ref>:<path>"),
        ("transcript/plan:missing.jsonl", "git show transcript/plan:missing.jsonl:"),
        (&*w.base.join("not.jsonl").to_string_lossy(), "not a Claude Code transcript"),
    ] {
        let (ok, _, err) = gopola(&w, &["import", arg]);
        assert!(!ok && err.contains(says), "{arg}: {err}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

/// One transcript's records: a prompt and `calls` model calls of `ctx` tokens each. `chain` is the record's
/// `session_id`: after a `/clear`, Claude Code writes a new transcript whose records keep the first one's id there.
fn chain_part(id: &str, chain: &str, day: &str, calls: u64, ctx: u64) -> String {
    use serde_json::json;
    let mut lines = vec![json!({"type": "user", "sessionId": id, "session_id": chain, "turnOrigin": "human",
        "message": {"content": "go on"}, "timestamp": format!("{day}T10:00:00Z")}).to_string()];
    for i in 0..calls {
        lines.push(json!({"type": "assistant", "sessionId": id, "session_id": chain, "timestamp": format!("{day}T10:0{}:00Z", i + 1),
            "message": {"id": format!("{id}-m{i}"), "content": [], "usage": {"input_tokens": ctx}}}).to_string());
    }
    // The last records of a transcript carry no session_id.
    lines.push(json!({"type": "ai-title", "aiTitle": "a job", "sessionId": id}).to_string());
    lines.join("\n") + "\n"
}

/// 2026-10-28, a real job: `weight` showed only the transcript before the first `/clear`.
#[test]
fn weight_lists_every_transcript_of_a_clear_chain() {
    let w = setup();
    let (root, part, other) = ("aaaa1111-0000-4000-8000-000000000001", "bbbb2222-0000-4000-8000-000000000002", "cccc3333-0000-4000-8000-000000000003");
    let dir = transcript_dir(&w);
    // A job forked from an interactive session opens with that session's records.
    let forked = r#"{"type":"attachment","attachment":{"type":"x"},"session_id":"dddd4444-0000-4000-8000-000000000004","timestamp":"2026-10-24T09:00:00Z"}"#;
    fs::write(dir.join(format!("{root}.jsonl")), format!("{forked}\n{}", chain_part(root, root, "2026-10-24", 2, 1000))).unwrap();
    fs::write(dir.join(format!("{part}.jsonl")), chain_part(part, root, "2026-10-27", 3, 5000)).unwrap();
    fs::write(dir.join(format!("{other}.jsonl")), chain_part(other, other, "2026-10-25", 4, 9000)).unwrap();
    assert!(gopola(&w, &["index"]).0);
    for asked in ["aaaa1111", "bbbb2222"] {
        let (ok, out, err) = gopola(&w, &["weight", asked]);
        assert!(ok, "{err}");
        assert!(out.contains("chain aaaa1111: 2 transcripts"), "{asked}: {out}");
        let rows: Vec<&str> = out.lines().filter(|l| l.contains("2026-10-2")).filter(|l| l.trim_start().starts_with(['1', '2'])).collect();
        assert_eq!(rows.len(), 2, "{asked}: {out}");
        assert!(rows[0].contains("aaaa1111") && rows[0].contains("2026-10-24 09:00"), "{out}");
        assert!(rows[1].contains("bbbb2222") && rows[1].split_whitespace().any(|x| x == "3"), "the part's 3 calls: {out}");
        assert!(!out.contains("cccc3333"), "another session is not in the chain: {out}");
    }
    let (_, out, _) = gopola(&w, &["weight", "bbbb2222", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["chain"]["id"], root);
    assert_eq!(v["chain"]["parts"][1]["calls"], 3);
    assert_eq!(v["chain"]["parts"][1]["median"], 5000);
    let (_, out, _) = gopola(&w, &["weight", "cccc3333"]);
    assert!(!out.contains("chain"), "a session with no /clear prints no chain: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// 2026-11-13, software-factory add684fe: skill calls were counted with `search -n 1000 | rg -c`, which stops at 10 rows
/// without -n and also counts rows that only name the skill's path.
#[test]
fn skills_counts_skill_calls_by_name() {
    use serde_json::json;
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let dir = transcript_dir(&w);
    let skill = |ts: &str, calls: &[(&str, Option<&str>)]| reply_running(ts, "ok",
        &calls.iter().map(|(s, a)| ("Skill", json!({"skill": s, "args": a}))).collect::<Vec<_>>());
    let one = [
        prompt("2026-11-01T10:00:00Z", "wrap up", &cwd),
        skill("2026-11-01T10:00:01Z", &[("session-retro", None), ("session-retro", None)]),
        prompt("2026-11-02T10:00:00Z", "write a skill; read ~/.claude/skills/session-retro/SKILL.md", &cwd),
        skill("2026-11-02T10:00:01Z", &[("superpowers:writing-skills", None), ("session-retro", Some("live"))]),
    ];
    fs::write(dir.join("aaaa0001-0000-4000-8000-000000000001.jsonl"), one.join("\n") + "\n").unwrap();
    let two = [prompt("2026-11-03T10:00:00Z", "<command-message>session-retro</command-message>\n<command-name>/session-retro</command-name>", &cwd),
        reply("2026-11-03T10:00:01Z", "Retro done.", None)];
    fs::write(dir.join("bbbb0002-0000-4000-8000-000000000002.jsonl"), two.join("\n") + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);

    let (ok, out, err) = gopola(&w, &["skills", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["project"], "all", "across projects unless --project names one");
    let row = |name: &str| v["skills"].as_array().unwrap().iter().find(|s| s["skill"] == name).cloned().unwrap_or_default();
    let r = row("session-retro");
    assert_eq!((r["calls"].as_u64(), r["sessions"].as_u64(), r["slash"].as_u64()), (Some(2), Some(1), Some(1)),
        "two turns call it (a repeated call in a turn is one); the path in a prompt is not a call; one /session-retro: {out}");
    assert_eq!((r["first"].as_str(), r["last"].as_str(), r["projects"].clone()), (Some("2026-11-01"), Some("2026-11-02"), json!(["demo-app"])));
    assert_eq!(row("superpowers:writing-skills")["calls"], 1);
    assert!(row("writing-skills").is_null(), "a plugin's name is its own: {out}");

    let (_, out, _) = gopola(&w, &["skills", "writing-skills", "session-retro"]);
    let lines: Vec<&str> = out.lines().filter(|l| l.contains("writing-skills") || l.contains("session-retro")).collect();
    assert_eq!(lines.len(), 2, "exactly the names asked: {out}");
    assert!(lines.iter().any(|l| l.starts_with("writing-skills") && l.split_whitespace().nth(1) == Some("0")), "a name asked for shows zeros: {out}");
    // 0.19.0's Opus review, S2: a row of zeros was logged as a hit.
    let hits = || rusqlite_open(&w.db).query_row("select hits from usage where cmd = 'skills' order by rowid desc limit 1", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(hits(), 1, "only session-retro was called");
    gopola(&w, &["skills", "writing-skills"]);
    assert_eq!(hits(), 0, "a name with no call is no hit");
    fs::remove_dir_all(&w.base).unwrap();
}

/// 2026-11-12, copilot-orchestration 9c783c76: each agent's first call was read with a hand `jq` over
/// `subagents/agent-*.jsonl`, joined to `agent-*.meta.json`, because `weight` on a subagent file printed nothing.
#[test]
fn weight_agents_measures_each_subagent_transcript() {
    let w = setup();
    let id = "eeee7777-0000-4000-8000-000000000007";
    write_session(&w, id, "2026-11-12T10:00:00Z", &[("dispatch two", "Dispatched.")]);
    let sub = transcript_dir(&w).join(id).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    let call = |ts: &str, mid: &str, ctx: u64| serde_json::json!({"type": "assistant", "isSidechain": true, "timestamp": ts,
        "message": {"id": mid, "content": [], "usage": {"input_tokens": 3, "cache_creation_input_tokens": ctx - 3, "cache_read_input_tokens": 0, "output_tokens": 7}}}).to_string();
    // The name order (a1, b2) is not the start order (b2, a1).
    for (agent, ty, desc, start, ctxs) in [("a1", "general-purpose", "Build the thing", "2026-11-12T10:05:00Z", [59862, 70000]),
        ("b2", "worker", "Review the thing", "2026-11-12T10:01:00Z", [48283, 52000])] {
        let lines = [call(start, &format!("{agent}m1"), ctxs[0]), call(start, &format!("{agent}m1"), ctxs[0]), call(start, &format!("{agent}m2"), ctxs[1])];
        fs::write(sub.join(format!("agent-{agent}.jsonl")), lines.join("\n") + "\n").unwrap();
        fs::write(sub.join(format!("agent-{agent}.meta.json")), serde_json::json!({"agentType": ty, "description": desc}).to_string()).unwrap();
    }
    assert!(gopola(&w, &["index"]).0);

    let (ok, out, err) = gopola(&w, &["weight", "eeee7777", "--agents", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let a = v["agents"].as_array().unwrap();
    let got: Vec<(&str, &str, u64, u64, u64)> = a.iter().map(|x| (x["agent"].as_str().unwrap(), x["type"].as_str().unwrap(),
        x["calls"].as_u64().unwrap(), x["first_context"].as_u64().unwrap(), x["max"].as_u64().unwrap())).collect();
    assert_eq!(got, [("b2", "worker", 2, 48283, 52000), ("a1", "general-purpose", 2, 59862, 70000)], "start order, one call per message id: {out}");
    assert_eq!((a[0]["description"].as_str(), a[0]["output_tokens"].as_u64()), (Some("Review the thing"), Some(14)));
    let (_, out, _) = gopola(&w, &["weight", "eeee7777", "--agents"]);
    assert!(out.lines().any(|l| l.contains("b2") && l.contains("worker") && l.contains("48283") && l.contains("Review the thing")), "{out}");
    let (_, plain, _) = gopola(&w, &["weight", "eeee7777", "--json"]);
    assert!(serde_json::from_str::<serde_json::Value>(&plain).unwrap().get("agents").is_none(), "only on request: {plain}");

    // A Workflow agent's transcript sits a folder deeper, and is all sidechain records too.
    let deeper = sub.join("workflows/wf_1");
    fs::create_dir_all(&deeper).unwrap();
    fs::copy(sub.join("agent-b2.jsonl"), deeper.join("agent-c3.jsonl")).unwrap();
    let (ok, out, err) = gopola(&w, &["weight", &deeper.join("agent-c3.jsonl").to_string_lossy()]);
    assert!(ok && out.contains("model calls  2") && out.contains("first 48k"), "a subagent file measures its own calls: {out}{err}");
    // 0.19.0's Opus review, S1 (owner, 2026-11-13: list them): --agents lists a Workflow's agents too.
    fs::write(deeper.join("agent-c3.meta.json"), serde_json::json!({"agentType": "workflow-subagent", "description": "verify:r1"}).to_string()).unwrap();
    let (_, out, _) = gopola(&w, &["weight", "eeee7777", "--agents", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let got: Vec<(&str, &str)> = v["agents"].as_array().unwrap().iter().map(|x| (x["agent"].as_str().unwrap(), x["type"].as_str().unwrap())).collect();
    assert_eq!(got, [("b2", "worker"), ("c3", "workflow-subagent"), ("a1", "general-purpose")], "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

fn bash_use(ts: &str, id: &str, name: &str, cmd: &str) -> String {
    serde_json::json!({"type": "assistant", "timestamp": ts, "message": {"role": "assistant",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": {"command": cmd}}]}}).to_string()
}

/// Shapes from real Bash results (2026-11-05): content is a string; `is_error` with `Exit code N` first; the
/// varying keys sit in `toolUseResult`.
fn bash_result(ts: &str, id: &str, content: &str, is_error: bool, extra: serde_json::Value) -> String {
    serde_json::json!({"type": "user", "timestamp": ts, "toolUseResult": extra, "message": {"role": "user",
        "content": [{"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}]}}).to_string()
}

/// One session with four Bash calls and a Read, and one subagent with one Bash call.
fn commands_world() -> (World, String) {
    let w = setup();
    let id = "eeee5555-0000-4000-8000-000000000005";
    let cwd = w.repo.to_string_lossy().to_string();
    let dir = transcript_dir(&w);
    // Where Claude Code saves an output too large for the transcript.
    let full = dir.join(id).join("tool-results/b1.txt");
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    let forty: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
    fs::write(&full, forty.join("\n")).unwrap();
    let preview = "<persisted-output>\nOutput too large (1KB). Full output saved to: x\n\nPreview (first 2KB):\nline 1\n</persisted-output>";
    let lines = [
        prompt("2026-09-01T10:00:00Z", "start docker", &cwd),
        // "Docker" in the text passes the line filter, so only the exact match on the command leaves this call out.
        serde_json::json!({"type": "assistant", "timestamp": "2026-09-01T10:00:01Z", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "Is Docker up?"}, {"type": "tool_use", "id": "b1", "name": "Bash", "input": {"command": "docker info"}}]}}).to_string(),
        bash_result("2026-09-01T10:00:04Z", "b1", "failed to connect to the docker API", false,
            serde_json::json!({"stdout": "failed to connect to the docker API", "stderr": "", "interrupted": false})),
        bash_use("2026-09-01T10:00:05Z", "b2", "Bash", "false; echo boom"),
        bash_result("2026-09-01T10:00:06Z", "b2", "Exit code 2\nboom", true, serde_json::json!("Error: Exit code 2\nboom")),
        bash_use("2026-09-01T10:00:07Z", "b3", "Bash", "open -a Docker; say \"hi\""),
        bash_result("2026-09-01T10:00:09Z", "b3", preview, false,
            serde_json::json!({"stdout": "line 1", "persistedOutputPath": full.to_string_lossy(), "interrupted": false})),
        serde_json::json!({"type": "assistant", "timestamp": "2026-09-01T10:00:10Z", "message": {"role": "assistant",
            "content": [{"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": "/x"}}]}}).to_string(),
        bash_use("2026-09-01T10:00:11Z", "b4", "Bash", "sleep 999"),
        // A copy of a request, with no timestamp: 17,153 of these on 2026-11-05 hold earlier tool_use blocks again.
        serde_json::json!({"type": "api-request-blob", "hash": "h", "message": {"content": [
            {"type": "tool_use", "id": "b9", "name": "Bash", "input": {"command": "cd /x && docker info"}}]}}).to_string(),
    ];
    fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    let sub = dir.join(id).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    let agent = [
        bash_use("2026-09-01T10:00:02Z", "a1", "bash", "cargo test"),
        bash_result("2026-09-01T10:00:03Z", "a1", "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 ok", false, serde_json::Value::Null),
    ];
    fs::write(sub.join("agent-abc123.jsonl"), agent.join("\n") + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    (w, id.to_string())
}

/// 2026-11-04, a real session: "how did session 8a089f5e start Docker" took `show --grep` (commands cut at about
/// 150 characters), `show --turn` (no commands) and two hand-written jq programs over the raw transcript.
#[test]
fn commands_lists_bash_calls_with_status_output_and_subagents() {
    let (w, _) = commands_world();
    let (ok, out, err) = gopola(&w, &["commands", "eeee5555", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let c = v["commands"].as_array().unwrap();
    let got: Vec<(&str, &str, &str)> = c.iter().map(|x| (x["command"].as_str().unwrap(), x["source"].as_str().unwrap(), x["status"].as_str().unwrap())).collect();
    assert_eq!(got, [("docker info", "session", "ok"), ("cargo test", "agent", "ok"), ("false; echo boom", "session", "exit 2"),
        ("open -a Docker; say \"hi\"", "session", "ok"), ("sleep 999", "session", "no result")], "time order, subagent included, no Read");
    assert_eq!(c[1]["agent"], "abc123");
    assert_eq!((c[0]["ts"].as_str(), c[0]["result_ts"].as_str()), (Some("2026-09-01T10:00:01Z"), Some("2026-09-01T10:00:04Z")));
    assert_eq!(c[2]["exit"], 2);
    assert_eq!(c[2]["output"], "boom", "the Exit code line is the status, not output");
    assert_eq!(c[3]["output"].as_str().unwrap().lines().count(), 40, "a persisted output is printed whole");
    assert_eq!(c[1]["output"], "token [redacted:github-token] ok");
    assert!(c[4]["result_ts"].is_null());

    let (_, out, _) = gopola(&w, &["commands", "eeee5555"]);
    assert!(out.contains("agent abc123") && out.contains("exit 2") && out.contains("$ open -a Docker; say \"hi\"") && out.contains("line 40"), "{out}");
    let (_, out, _) = gopola(&w, &["commands", "eeee5555", "--head", "1"]);
    assert!(out.contains("line 1\n") && !out.contains("line 2\n") && out.contains("… 39 more lines"), "cut only on request: {out}");
    // 2026-11-12, a client session: a command's output held storage keys, so only the command text could be kept.
    let (_, out, _) = gopola(&w, &["commands", "eeee5555", "--no-output"]);
    assert!(out.contains("$ open -a Docker") && out.contains("exit 2") && !out.contains("line 1") && !out.contains("more lines")
        && !out.contains("\nboom") && !out.contains("[redacted"), "{out}");
    let (_, out, _) = gopola(&w, &["commands", "eeee5555", "--no-output", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["commands"].as_array().unwrap().iter().all(|x| x.get("output").is_none() && x["command"].is_string()), "{out}");

    let exact = |g: &str| -> Vec<String> {
        let (_, out, _) = gopola(&w, &["commands", "eeee5555", "--grep", g, "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["commands"].as_array().unwrap().iter().map(|x| x["command"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(exact("Docker"), ["open -a Docker; say \"hi\""], "exact and case-sensitive");
    assert_eq!(exact("say \"hi\""), ["open -a Docker; say \"hi\""], "a quote in the needle");
    assert_eq!(exact("cargo"), ["cargo test"], "a subagent's command is searched too");
    // A Workflow's agent sits under subagents/workflows/wf_<run>/ (0.19.0's Opus review, S1; owner, 2026-11-13).
    let run = transcript_dir(&w).join("eeee5555-0000-4000-8000-000000000005/subagents/workflows/wf_1");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("agent-wf1.jsonl"), bash_use("2026-09-01T10:00:12Z", "w1", "Bash", "just test") + "\n").unwrap();
    let (_, out, _) = gopola(&w, &["commands", "eeee5555", "--grep", "just test", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["commands"][0]["command"].as_str(), v["commands"][0]["agent"].as_str()), (Some("just test"), Some("wf1")), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn commands_searches_sessions_by_exact_substring() {
    let (w, id) = commands_world();
    write_session(&w, "ffff6666-0000-4000-8000-000000000006", "2026-09-02T10:00:00Z", &[("other work", "done")]);
    assert!(gopola(&w, &["index"]).0);
    let (ok, out, err) = gopola(&w, &["commands", "--grep", "open -a Docker", "--project", "all", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let c = v["commands"].as_array().unwrap();
    assert_eq!(c.len(), 1, "{out}");
    assert_eq!(c[0]["session"], id.as_str());
    assert_eq!(c[0]["result_ts"], "2026-09-01T10:00:09Z");
    assert_eq!(v["sessions_read"], 2);
    let (_, out, _) = gopola(&w, &["commands", "--grep", "open -a Docker"]);
    assert!(out.contains("eeee5555") && out.contains("$ open -a Docker"), "the default project is this repo's: {out}");
    let (ok, _, err) = gopola(&w, &["commands"]);
    assert!(!ok && err.contains("commands needs a session or --grep"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

fn hook(ts: &str, a: serde_json::Value) -> String {
    serde_json::json!({"type": "attachment", "timestamp": ts, "attachment": a}).to_string()
}

/// 2026-11-09, session 49a0f556: the loop checks after the rename pulled a transcript's hook records out with
/// `rg -o … | sort | uniq -c`, twice. Shapes as counted over every transcript on 2026-11-10.
#[test]
fn hooks_lists_each_run_with_status_blocks_and_subagents() {
    let w = setup();
    let id = "abab7777-0000-4000-8000-000000000007";
    let cwd = w.repo.to_string_lossy().to_string();
    let dir = transcript_dir(&w);
    let lines = [
        prompt("2026-09-01T10:00:00Z", "PreToolUse:Bash hook error: [x]: a prompt that quotes a block", &cwd),
        hook("2026-09-01T10:00:01Z", serde_json::json!({"type": "hook_success", "hookName": "SessionStart:startup", "hookEvent": "SessionStart",
            "command": "gopola index --quiet", "exitCode": 0, "durationMs": 79, "stdout": "# gopola: one\ntwo", "stderr": "", "content": "# gopola: one\ntwo"})),
        hook("2026-09-01T10:00:02.000Z", serde_json::json!({"type": "hook_additional_context", "hookName": "SessionStart", "content": ["You have superpowers."]})),
        bash_use("2026-09-01T10:00:03Z", "b1", "Bash", "for t in $f; do :; done"),
        bash_result("2026-09-01T10:00:04Z", "b1", "PreToolUse:Bash hook error: [$HOME/.local/bin/zshguard hook]: zshguard refused this command", true,
            serde_json::json!("Error: PreToolUse:Bash hook error: …")),
        // A copy of a request repeats the block: not a run of its own.
        serde_json::json!({"type": "api-request-blob", "message": {"content": [{"type": "tool_result", "tool_use_id": "b1",
            "content": "PreToolUse:Bash hook error: [$HOME/.local/bin/zshguard hook]: zshguard refused this command", "is_error": true}]}}).to_string(),
        hook("2026-09-01T10:00:05Z", serde_json::json!({"type": "async_hook_response", "hookName": "PreToolUse:Bash", "hookEvent": "PreToolUse",
            "toolName": "Bash", "response": {}, "stdout": "", "stderr": "[Hook] bootstrap\n", "exitCode": 0})),
        hook("2026-09-01T10:00:06Z", serde_json::json!({"type": "hook_non_blocking_error", "hookName": "Stop", "hookEvent": "Stop",
            "stderr": "Failed with non-blocking status code: /bin/sh: planlint-docs-hook: not found", "stdout": "", "exitCode": 127, "command": "Checking the docs' claims", "durationMs": 12})),
        hook("2026-09-01T10:00:07Z", serde_json::json!({"type": "hook_cancelled", "hookName": "Stop", "hookEvent": "Stop", "command": "Checking the docs' claims",
            "durationMs": 60242, "timedOut": true, "timeoutMs": 60000})),
        hook("2026-09-01T10:00:08Z", serde_json::json!({"type": "hook_system_message", "hookName": "Stop", "hookEvent": "Stop", "content": "planlint: 85 stale claim(s)"})),
        serde_json::json!({"type": "user", "isMeta": true, "timestamp": "2026-09-01T10:00:09Z", "message": {"role": "user",
            "content": "Stop hook feedback:\n[~/.claude/stop-hook-git-check.sh]: There are untracked files"}}).to_string(),
    ];
    fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    let sub = dir.join(id).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("agent-abc123.jsonl"), hook("2026-09-01T10:00:02.500Z",serde_json::json!({"type": "hook_success", "hookName": "PreToolUse:Read",
        "command": "gopola file --hook", "exitCode": 0, "durationMs": 1500, "stdout": "", "stderr": ""})) + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);

    let (ok, out, err) = gopola(&w, &["hooks", "abab7777", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let h = v["hooks"].as_array().unwrap();
    let got: Vec<(&str, &str, &str)> = h.iter().map(|x| (x["hook"].as_str().unwrap(), x["status"].as_str().unwrap(), x["command"].as_str().unwrap())).collect();
    assert_eq!(got, [
        ("SessionStart:startup", "ok", "gopola index --quiet"),
        ("SessionStart", "context", ""),
        ("PreToolUse:Read", "ok", "gopola file --hook"),
        ("PreToolUse:Bash", "blocked", "$HOME/.local/bin/zshguard hook"),
        ("PreToolUse:Bash", "ok", ""),
        ("Stop", "exit 127", "Checking the docs' claims"),
        ("Stop", "timed out", "Checking the docs' claims"),
        ("Stop", "message", ""),
        ("Stop", "blocked", "~/.claude/stop-hook-git-check.sh"),
    ], "time order; the quoted prompt and the request copy are not runs");
    assert_eq!(h[2]["agent"], "abc123");
    assert_eq!(h[3]["message"], "zshguard refused this command");
    assert_eq!(h[6]["duration_ms"], 60242);

    let (_, out, _) = gopola(&w, &["hooks", "abab7777"]);
    assert!(out.contains("9 hook run(s), 1 by subagents, 4 not ok"), "{out}");
    assert!(out.contains("  ! zshguard refused this command") && out.contains("  ! There are untracked files") && out.contains("1.5s"), "{out}");
    assert!(!out.contains("# gopola: one"), "output only with --head: {out}");
    let (_, out, _) = gopola(&w, &["hooks", "abab7777", "--head", "1"]);
    assert!(out.contains("  > # gopola: one\n  … 1 more lines") && out.contains("  > You have superpowers."), "{out}");
    let (_, out, _) = gopola(&w, &["hooks", "abab7777", "--grep", "Stop", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["hooks"].as_array().unwrap().len(), 4, "--grep matches the hook name too");
    // A Workflow's agent (0.19.0's Opus review, S1; owner, 2026-11-13).
    let run = sub.join("workflows/wf_1");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("agent-wf1.jsonl"), hook("2026-09-01T10:00:10Z", serde_json::json!({"type": "hook_success", "hookName": "PreToolUse:Bash",
        "command": "zshguard hook", "exitCode": 0, "durationMs": 5, "stdout": "", "stderr": ""})) + "\n").unwrap();
    let (_, out, _) = gopola(&w, &["hooks", "abab7777"]);
    assert!(out.contains("10 hook run(s), 2 by subagents, 4 not ok"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// Set a file's modification time `secs` seconds into the past, so `archive` sees it as quiet.
fn age(p: &Path, secs: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn unzstd(p: &Path) -> Vec<u8> {
    zstd::decode_all(fs::File::open(p).unwrap()).unwrap()
}

/// Claude Code deletes transcripts after cleanupPeriodDays (default 30); on 2026-11-09 the index went back to
/// 2026-08-20 while the oldest transcript left was 2026-10-10.
#[test]
fn archive_copies_quiet_files_once_and_again_when_they_grow() {
    use std::io::Write;
    let w = setup();
    let id = "abab0000-0000-4000-8000-000000000001";
    write_session(&w, id, "2026-09-01T10:00:00Z", &[("keep this", "kept")]);
    write_session(&w, "f2e50000-0000-4000-8000-000000000002", "2026-09-01T10:00:00Z", &[("still going", "yes")]);
    let dir = transcript_dir(&w);
    let main = dir.join(format!("{id}.jsonl"));
    let sub = dir.join(id).join("subagents/agent-a1.jsonl");
    let out = dir.join(id).join("tool-results/b1.txt");
    let memory = dir.join("memory/MEMORY.md");
    for (p, text) in [(&sub, "{\"a\":1}\n"), (&out, "line 1\n"), (&memory, "- a note\n")] {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    for p in [&main, &sub, &out, &memory] { age(p, 7200) }

    let archive = w.base.join("archive");
    let copy = |p: &Path| PathBuf::from(format!("{}.zst", archive.join(p.strip_prefix(&w.projects).unwrap()).display()));
    let (ok, o, err) = gopola(&w, &["archive", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&o).unwrap();
    assert_eq!(v["copied"], 3, "{o}");
    for p in [&main, &sub, &out] { assert!(copy(p).is_file(), "{}", p.display()) }
    assert!(!copy(&memory).exists(), "memory folders are not transcripts");
    assert!(!copy(&dir.join("f2e50000-0000-4000-8000-000000000002.jsonl")).exists(), "a file written in the last hour waits");
    assert_eq!(unzstd(&copy(&main)), fs::read(&main).unwrap());

    let (_, o, _) = gopola(&w, &["archive", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&o).unwrap()["copied"], 0, "{o}");
    fs::OpenOptions::new().append(true).open(&main).unwrap().write_all(b"{\"type\":\"x\"}\n").unwrap();
    age(&main, 7200);
    let (_, o, _) = gopola(&w, &["archive"]);
    assert!(o.starts_with("archived 1 file"), "{o}");
    assert_eq!(unzstd(&copy(&main)), fs::read(&main).unwrap());
    fs::remove_dir_all(&w.base).unwrap();
}

fn files_under(d: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(d).unwrap().flatten() {
        if e.path().is_dir() { files_under(&e.path(), out) } else { out.push(e.path()) }
    }
}

#[test]
fn readers_answer_from_the_archive_once_claude_code_deleted_the_originals() {
    let (w, _) = commands_world();
    let dir = transcript_dir(&w);
    let (root, part) = ("aaaa1111-0000-4000-8000-000000000001", "bbbb2222-0000-4000-8000-000000000002");
    fs::write(dir.join(format!("{root}.jsonl")), chain_part(root, root, "2026-10-24", 2, 1000)).unwrap();
    fs::write(dir.join(format!("{part}.jsonl")), chain_part(part, root, "2026-10-27", 3, 5000)).unwrap();
    handback_world(&w, &["a4fore0001"], &[serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[{"type":"text","text":"Verdict: fine."}]}]},
        "toolUseResult":{"status":"completed","agentId":"a4fore0001","agentType":"general-purpose","content":[{"type":"text","text":"Verdict: fine."}]}})]);
    assert!(gopola(&w, &["index"]).0);
    let mut files = Vec::new();
    files_under(&dir, &mut files);
    files.sort(); // the chain's first transcript is aged first, so its part stays the later one
    for f in &files { age(f, 7200) }

    let asks: [&[&str]; 3] = [&["commands", "eeee5555", "--json"], &["weight", "bbbb2222", "--json"], &["dispatches", "eeee5555", "--json"]];
    let answers = || -> Vec<String> {
        let mut v: Vec<String> = asks.iter().map(|a| { let (ok, out, err) = gopola(&w, a); assert!(ok, "{a:?}: {err}"); out }).collect();
        v.push(handback(&w, "a4fore").1);
        v
    };
    let before = answers();
    assert!(before[0].contains("line 40") && before[1].contains("\"chain\"") && before[3] == "Verdict: fine.\n", "{before:?}");
    assert!(gopola(&w, &["archive"]).0);
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(answers(), before);
    fs::remove_dir_all(&w.base).unwrap();
}

/// Roadmap gap 2: one `index --full` used to wipe every session whose transcript Claude Code had deleted.
#[test]
fn index_full_rebuilds_from_the_archive_and_keeps_sessions_it_cannot_reread() {
    let w = setup();
    let (a, b) = ("a1c00000-0000-4000-8000-000000000001", "b1c00000-0000-4000-8000-000000000002");
    write_session(&w, a, "2026-09-01T10:00:00Z", &[("archived prompt zebra", "ok")]);
    write_session(&w, b, "2026-09-02T10:00:00Z", &[("lost prompt walrus", "ok"), ("second one", "ok")]);
    assert!(gopola(&w, &["index"]).0);
    let dir = transcript_dir(&w);
    age(&dir.join(format!("{a}.jsonl")), 7200);
    assert!(gopola(&w, &["archive"]).0);
    // Claude Code's cleanup: a has a copy in the archive, b has none anywhere.
    fs::remove_file(dir.join(format!("{a}.jsonl"))).unwrap();
    fs::remove_file(dir.join(format!("{b}.jsonl"))).unwrap();

    let (ok, out, err) = gopola(&w, &["index", "--full", "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["turns_written"], 1, "a is read again from its copy: {out}");
    let (_, out, _) = gopola(&w, &["recent", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let turns: Vec<(String, i64)> = v["sessions"].as_array().unwrap().iter()
        .map(|s| (s["id"].as_str().unwrap().to_string(), s["turns"].as_i64().unwrap())).collect();
    assert_eq!(turns, [(b.to_string(), 2), (a.to_string(), 1)], "{out}");
    for (word, id) in [("zebra", a), ("walrus", b)] {
        let (_, out, _) = gopola(&w, &["search", word]);
        assert!(out.contains(&id[..8]), "{word}: {out}");
    }
    let (_, out, _) = gopola(&w, &["index", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["files_indexed"].as_i64(), v["turns_written"].as_i64()), (Some(0), Some(0)), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

const CP: &str = "c1000000-0000-4000-8000-000000000001";

/// `<base>/copilot/session-state/<id>/` holding a Copilot fixture as `events.jsonl`, launched in the test repo.
fn copilot_dir(w: &World, id: &str, fixture: &str) -> PathBuf {
    copilot_dir_in(w, id, fixture, &w.repo)
}

fn copilot_dir_in(w: &World, id: &str, fixture: &str, cwd: &Path) -> PathBuf {
    let d = w.base.join("copilot/session-state").join(id);
    fs::create_dir_all(&d).unwrap();
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/copilot").join(fixture)).unwrap();
    fs::write(d.join("events.jsonl"), text.replace("/work/demo-app", &cwd.to_string_lossy())).unwrap();
    d
}

#[test]
fn copilot_session_is_indexed_in_the_same_project() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok && err.is_empty(), "{err}");
    let (_, out, _) = gopola(&w, &["recent", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["project"].as_str(), v["total"].as_i64()), (Some("demo-app"), Some(2)), "{out}");
    let s = &v["sessions"][0];
    assert_eq!((s["id"].as_str(), s["harness"].as_str(), s["title"].as_str(), s["turns"].as_i64(), s["scripted"].as_bool(), s["branch"].as_str()),
        (Some(CP), Some("copilot"), Some("Demo Sync Check"), Some(2), Some(false), Some("main")), "{out}");
    assert_eq!(v["sessions"][1]["harness"], "claude", "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_scripted_run_is_searchable() {
    let w = setup();
    // Two passes: the second starts at the prompt, past `session.start`, and keeps the version the first read.
    let f = copilot_dir(&w, CP, "scripted-1.0.107.jsonl").join("events.jsonl");
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, &full[..full.find("verdict: clean").unwrap()]).unwrap();
    assert!(gopola(&w, &["index"]).0);
    fs::write(&f, &full).unwrap();
    assert!(gopola(&w, &["index"]).0);
    for args in [&["search", "verdict clean", "--json"][..], &["search", "cargo test", "--kind", "command", "--json"]] {
        let (_, out, _) = gopola(&w, args);
        assert!(out.contains(&format!("\"ref\":\"{CP}:")), "{args:?}: {out}");
    }
    let (_, out, _) = gopola(&w, &["show", CP, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["harness"].as_str(), v["scripted"].as_bool()), (Some("copilot"), Some(true)), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_file_finds_edits() {
    let w = setup();
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["file", &w.repo.join("README.md").to_string_lossy()]);
    assert!(out.contains(&CP[..8]), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_folder_without_events_is_skipped() {
    let w = setup();
    let d = w.base.join("copilot/session-state/f5000000-0000-4000-8000-000000000009");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("workspace.yaml"), "id: f5000000-0000-4000-8000-000000000009\n").unwrap();
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    let (ok, out, err) = gopola(&w, &["index", "--json"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["files_indexed"], 1, "{out}");
    fs::remove_file(w.base.join("copilot/session-state").join(CP).join("events.jsonl")).unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok && err.is_empty(), "an indexed session's file gone: {err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_partial_last_line_completes_next_pass() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, &full[..full.find("added README.md").unwrap()]).unwrap();
    let show = || serde_json::from_str::<serde_json::Value>(&gopola(&w, &["show", CP, "--json"]).1).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(show()["turns"][1]["reply"], serde_json::Value::Null);
    fs::write(&f, &full).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(show()["turns"][1]["reply"], "added README.md");
    assert_eq!(show()["scripted"], false, "the title the first pass stored still counts");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_title_on_later_pass_clears_scripted() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, &full[..full.find(r#"{"type":"model.message""#).unwrap()]).unwrap();
    let scripted = || serde_json::from_str::<serde_json::Value>(&gopola(&w, &["show", CP, "--json"]).1).unwrap()["scripted"].as_bool();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(scripted(), Some(true));
    fs::write(&f, &full).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(scripted(), Some(false));
    fs::remove_dir_all(&w.base).unwrap();
}

/// A full index deletes the rows, so the launch dir comes from the file again, and must go through `moved` as Claude's do.
#[test]
fn copilot_session_of_a_moved_checkout_keeps_the_new_name_after_a_full_index() {
    let mut w = setup();
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    assert!(gopola(&w, &["index"]).0);
    let new = w.base.join("repos").join("renamed-app");
    fs::rename(&w.repo, &new).unwrap();
    let old = w.repo.to_string_lossy().to_string();
    w.repo = new.clone();
    assert!(gopola(&w, &["moved", &old, &new.to_string_lossy()]).0);
    assert!(gopola(&w, &["index", "--full"]).0);
    let (_, out, _) = gopola(&w, &["recent", "--project", "renamed-app", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["sessions"][0]["id"], CP, "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_old_version_is_not_scripted() {
    let w = setup();
    copilot_dir(&w, CP, "vscode-1.0.81.jsonl");
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["show", CP, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["turns"].as_array().map(Vec::len), v["scripted"].as_bool()), (Some(2), Some(false)), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_subagent_session_writes_no_turns() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    assert!(gopola(&w, &["index"]).0);
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, full + r#"{"type":"subagent.started","data":{},"id":"e0000099","timestamp":"2026-03-02T09:06:00.000Z","parentId":"e0000020"}"# + "\n").unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok, "{err}");
    let (_, out, _) = gopola(&w, &["recent", "--all", "--json"]);
    assert!(!out.contains(CP), "{out}");
    let (_, out, _) = gopola(&w, &["search", "readme", "--json"]);
    assert!(!out.contains(CP), "the first pass's turns are gone: {out}");
    let (_, out, _) = gopola(&w, &["show", CP, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["harness"].as_str(), v["turns"].as_array().map(Vec::len)), (Some("copilot"), Some(0)), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn tests_never_read_the_real_copilot_home() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent", "--project", "all", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["total"], 1, "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// Copilot's archive copies live beside the Claude mirror, not in it: a Claude checkout named `copilot` keeps its name.
#[test]
fn copilot_archive_copy_is_read_and_a_claude_project_named_copilot_keeps_its_name() {
    let w = setup();
    let checkout = w.base.join("repos").join("copilot");
    fs::create_dir_all(&checkout).unwrap();
    let cwd = checkout.to_string_lossy().to_string();
    let d = w.projects.join(cwd.replace('/', "-"));
    fs::create_dir_all(&d).unwrap();
    let id = "0123abcd-0000-4000-8000-000000000001";
    let ts = "2026-03-01T10:00:00.000Z";
    fs::write(d.join(format!("{id}.jsonl")), [prompt(ts, "fix the build", &cwd), reply(ts, "Fixed.", None)].join("\n") + "\n").unwrap();
    // The Copilot folder is gone; only gopola's copy is left, where the archive keeps Copilot sessions.
    let original = copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    let copy = w.base.join("archive-copilot").join(CP);
    fs::create_dir_all(&copy).unwrap();
    fs::write(copy.join("events.jsonl.zst"), zstd::bulk::compress(&fs::read(original.join("events.jsonl")).unwrap(), 3).unwrap()).unwrap();
    fs::remove_dir_all(&original).unwrap();
    let (ok, _, err) = gopola(&w, &["index", "--full"]);
    assert!(ok && err.is_empty(), "{err}");
    let project = |id: &str| serde_json::from_str::<serde_json::Value>(&gopola(&w, &["show", id, "--json"]).1).unwrap()["project"].clone();
    assert_eq!(project(id), "copilot");
    assert_eq!(project(CP), "demo-app", "the Copilot session is read from its copy");
    fs::remove_dir_all(&w.base).unwrap();
}

/// The ids in the Codex fixtures: the exec run (with its resume), the interactive session, and the exec run's fork.
const CX: &str = "019cadc0-0000-7000-8000-00000000e001";
const CX_TUI: &str = "019cadf7-0000-7000-8000-00000000e002";
const CX_FORK: &str = "019cadc4-0000-7000-8000-00000000e003";

/// `<base>/codex/sessions/2026/03/02/rollout-…-<id>.jsonl` holding a Codex fixture, launched in `cwd`.
fn codex_rollout(w: &World, id: &str, fixture: &str, cwd: &Path) -> PathBuf {
    let d = w.base.join("codex/sessions/2026/03/02");
    fs::create_dir_all(&d).unwrap();
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex").join(fixture)).unwrap();
    let f = d.join(format!("rollout-2026-03-02T09-00-00-{id}.jsonl"));
    fs::write(&f, text.replace("/work/demo-app", &cwd.to_string_lossy())).unwrap();
    f
}

fn show_json(w: &World, id: &str) -> serde_json::Value {
    serde_json::from_str(&gopola(w, &["show", id, "--json"]).1).unwrap()
}

#[test]
fn codex_session_is_indexed_in_the_same_project() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok && err.is_empty(), "{err}");
    let (_, out, _) = gopola(&w, &["recent", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["project"].as_str(), v["total"].as_i64()), (Some("demo-app"), Some(2)), "{out}");
    let s = &v["sessions"][0];
    assert_eq!((s["id"].as_str(), s["harness"].as_str(), s["turns"].as_i64(), s["branch"].as_str()),
        (Some(CX), Some("codex"), Some(2), Some("main")), "{out}");
    assert_eq!(v["sessions"][1]["harness"], "claude", "{out}");
    assert_eq!(show_json(&w, CX)["turns"].as_array().map(Vec::len), Some(2));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_exec_run_is_scripted_and_searchable() {
    let w = setup();
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let scripted: bool = rusqlite_open(&w.db).query_row("select scripted from sessions where id = ?1", [CX], |r| r.get(0)).unwrap();
    assert!(scripted);
    let (_, out, _) = gopola(&w, &["search", "git status", "--json"]);
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(hits.as_array().unwrap().iter().any(|h| h["kind"] == "command" && h["ref"] == format!("{CX}:00000001")), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_file_finds_edits_and_reads() {
    let w = setup();
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let sessions = |name: &str, reads: bool| {
        let path = w.repo.join(name).to_string_lossy().to_string();
        let args = [&["file", path.as_str(), "--json"][..], if reads { &["--reads"] } else { &[] }].concat();
        let v: serde_json::Value = serde_json::from_str(&gopola(&w, &args).1).unwrap();
        v["edits"].as_array().unwrap().iter().map(|e| (e["session"].as_str().unwrap().to_string(), e["turn"].as_i64().unwrap())).collect::<Vec<_>>()
    };
    assert_eq!(sessions("NOTES.md", false), vec![(CX.to_string(), 2)], "an edit");
    assert_eq!(sessions("README.md", false), vec![], "a read is not an edit");
    assert_eq!(sessions("README.md", true), vec![(CX.to_string(), 1)], "a read");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_resume_appends_a_turn() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, full.lines().take(39).map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(show_json(&w, CX)["turns"].as_array().map(Vec::len), Some(1));
    // `codex exec resume` appends to the same file, with no second `session_meta`.
    fs::write(&f, &full).unwrap();
    assert!(gopola(&w, &["index"]).0);
    let v = show_json(&w, CX);
    assert_eq!((v["turns"].as_array().map(Vec::len), v["turns"][1]["prompt"].as_str()),
        (Some(2), Some("add a line to NOTES.md saying the check passed")), "{v}");
    let row: (bool, Option<String>, String) = rusqlite_open(&w.db)
        .query_row("select scripted, harness_version, launch_dir from sessions where id = ?1", [CX], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(row, (true, Some("0.167.0".into()), w.repo.to_string_lossy().to_string()));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_partial_last_line_completes_next_pass() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, &full[..full.find("NOTES.md now ends with").unwrap()]).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(show_json(&w, CX)["turns"][1]["reply"], "Adding the line to NOTES.md.");
    fs::write(&f, &full).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(show_json(&w, CX)["turns"][1]["reply"], "NOTES.md now ends with: check passed.");
    let replies: i64 = rusqlite_open(&w.db)
        .query_row("select count(*) from fts where kind = 'reply' and ref = ?1", [format!("{CX}:00000002")], |r| r.get(0)).unwrap();
    assert_eq!(replies, 1, "the turn's reply is stored once");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_title_comes_from_the_session_index() {
    let w = setup();
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    let names = w.base.join("codex/session_index.jsonl");
    let entry = |id: &str, name: &str, at: &str| serde_json::json!({"id": id, "thread_name": name, "updated_at": at}).to_string() + "\n";
    fs::write(&names, entry(CX_TUI, "Demo Sync Check", "2026-03-02T10:00:30Z") + &entry(CX, "Another Thread", "2026-03-02T10:00:40Z")).unwrap();
    let title = || serde_json::from_str::<serde_json::Value>(&gopola(&w, &["recent", "--json"]).1).unwrap()["sessions"][0]["title"].clone();
    let titled = |words: &str| gopola(&w, &["search", words, "--kind", "title", "--json"]).1.contains(&format!(r#""ref":"{CX_TUI}""#));
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(title(), "Demo Sync Check");
    assert!(titled("sync check"));
    // A rename is a later line for the same id; the rollout itself is unchanged.
    let mut text = fs::read_to_string(&names).unwrap();
    text += &entry(CX_TUI, "Demo Readme Question", "2026-03-02T10:05:00Z");
    fs::write(&names, text).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert_eq!(title(), "Demo Readme Question");
    assert!(titled("readme question") && !titled("sync check"));
    fs::remove_dir_all(&w.base).unwrap();
}

/// A session the index stores as nothing (an excluded project, a subagent) gets no title from `session_index.jsonl`.
#[test]
fn codex_title_skips_sessions_indexed_as_nothing() {
    let w = setup();
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    let other = w.base.join("repos/other-app");
    fs::create_dir_all(&other).unwrap();
    let f = codex_rollout(&w, CX_FORK, "fork-0.167.0.jsonl", &other);
    fs::write(&f, fs::read_to_string(&f).unwrap().replacen(r#""source":"exec""#, r#""source":"subagent""#, 1)).unwrap();
    let entry = |id: &str, name: &str| serde_json::json!({"id": id, "thread_name": name, "updated_at": "2026-03-02T10:00:30Z"}).to_string() + "\n";
    fs::write(w.base.join("codex/session_index.jsonl"), entry(CX_TUI, "Hidden Zebra Plan") + &entry(CX_FORK, "Subagent Walrus Work")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA).env("GOPOLA_EXCLUDED_PROJECTS", "demo-app")
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .current_dir(&w.repo).args(["index"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for (id, words) in [(CX_TUI, "zebra"), (CX_FORK, "walrus")] {
        let (_, hits, _) = gopola(&w, &["search", words, "--kind", "title", "--project", "all", "--json"]);
        assert!(!hits.contains(id), "{words}: {hits}");
        let title: Option<String> = rusqlite_open(&w.db).query_row("select title from sessions where id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(title, None, "{words}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_fork_is_its_own_session() {
    let w = setup();
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    codex_rollout(&w, CX_FORK, "fork-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent", "--all", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["total"], 2, "{out}");
    let prompts = |id: &str| show_json(&w, id)["turns"].as_array().unwrap().iter().map(|t| t["prompt"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(prompts(CX_FORK), vec!["say done"]);
    assert_eq!(prompts(CX).len(), 2);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_subagent_session_writes_no_turns() {
    let w = setup();
    let f = codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    let text = fs::read_to_string(&f).unwrap();
    fs::write(&f, text.replacen(r#""source":"vscode""#, r#""source":"subagent""#, 1)).unwrap();
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok && err.is_empty(), "{err}");
    let v = show_json(&w, CX_TUI);
    assert_eq!((v["harness"].as_str(), v["project"].as_str(), v["turns"].as_array().map(Vec::len)), (Some("codex"), Some("demo-app"), Some(0)), "{v}");
    let unknown = |k: &str| rusqlite_open(&w.db).query_row("select v from meta where k = ?1", [k], |r| r.get::<_, String>(0)).ok();
    let seen: serde_json::Value = serde_json::from_str(&unknown("codex_unknown_types").unwrap()).unwrap();
    assert_eq!(seen["subagent.session_meta"], CX_TUI, "{seen}");
    assert_eq!(unknown("copilot_unknown_types"), None, "each harness keeps its own list");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn tests_never_read_the_real_codex_home() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    // Each helper finds the rollout under `<base>/codex`, and nothing from anywhere else.
    let total = || serde_json::from_str::<serde_json::Value>(&gopola(&w, &["recent", "--project", "all", "--all", "--json"]).1).unwrap()["total"].clone();
    assert_eq!(gopola_with(&w, NO_OLLAMA, MODEL, &["index"]).0, 0);
    assert_eq!(total(), 2);
    assert!(gopola(&w, &["index", "--full"]).0);
    assert_eq!(total(), 2);
    fs::remove_dir_all(&w.base).unwrap();
}

/// The Codex rows: (id, path).
fn codex_rows(w: &World) -> Vec<(String, String)> {
    rusqlite_open(&w.db).prepare("select id, path from sessions where harness = 'codex'").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect()
}

/// `index --json`'s files_seen, with stderr: one rollout under two names is one file seen.
fn files_seen(w: &World) -> (serde_json::Value, String) {
    let (_, out, err) = gopola(w, &["index", "--json"]);
    (serde_json::from_str::<serde_json::Value>(&out).unwrap()["files_seen"].clone(), err)
}

/// Like the Copilot archive test, for Codex: copies in `archive-codex`, read back once Codex's file is gone.
#[test]
fn archive_keeps_codex_rollouts() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let before = show_json(&w, CX)["turns"].clone();
    age(&f, 7200);
    let (ok, o, err) = gopola(&w, &["archive", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&o).unwrap();
    let copies = w.base.join("archive-codex");
    assert_eq!((v["copied"].as_i64(), v["archive_codex"].as_str()), (Some(1), Some(copies.to_string_lossy().as_ref())), "{o}");
    let copy = PathBuf::from(format!("{}.zst", copies.join(f.strip_prefix(w.base.join("codex")).unwrap()).display()));
    assert_eq!(unzstd(&copy), fs::read(&f).unwrap());
    assert_eq!(files_seen(&w), (1.into(), String::new()), "the rollout and its copy are one session");
    fs::remove_file(&f).unwrap();
    let (ok, out, err) = gopola(&w, &["index", "--full", "--json"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["turns_written"], 2, "{out}");
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), f.to_string_lossy().to_string())]);
    assert_eq!(show_json(&w, CX)["turns"], before);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn index_propagates_codex_title_row_conversion_errors() {
    let w = setup();
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    rusqlite::Connection::open(&w.db)
        .unwrap()
        .execute("update sessions set project = cast(x'01' as blob) where id = ?1", [CX_TUI])
        .unwrap();
    // Only the title query reads `session_index.jsonl`: without it the same row passes.
    let (ok, _, err) = gopola(&w, &["index"]);
    assert!(ok, "{err}");
    fs::write(
        w.base.join("codex/session_index.jsonl"),
        format!(r#"{{"id":"{CX_TUI}","thread_name":"demo title"}}"#),
    )
    .unwrap();

    // `index` exits 0 so a hook never blocks; the error is on stderr.
    let (_, _, err) = gopola(&w, &["index"]);
    assert!(err.contains("Invalid column type Blob at index: 1, name: project"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn doctor_detects_a_codex_rollout_grown_before_compression() {
    use std::io::Write;
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    fs::OpenOptions::new().append(true).open(&f).unwrap().write_all(b"\n").unwrap();
    codex_compress(&f);
    fs::remove_file(&f).unwrap();

    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("fix   1 transcript(s) changed since the last index"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// gopola copies a rollout, Codex resumes it and archives it, gopola copies it again under `archived_sessions/`, then
/// Codex deletes it: `index --full` reads the newer copy, with the resumed turn.
#[test]
fn archive_codex_reads_the_newer_archived_copy() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, full.lines().take(39).map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    age(&f, 7200);
    assert!(gopola(&w, &["archive"]).0);
    let moved = w.base.join("codex/archived_sessions").join(f.file_name().unwrap());
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::write(&moved, &full).unwrap();
    fs::remove_file(&f).unwrap();
    age(&moved, 3700);
    let (ok, o, err) = gopola(&w, &["archive", "--json"]);
    assert!(ok && serde_json::from_str::<serde_json::Value>(&o).unwrap()["copied"] == 1, "{o}{err}");
    fs::remove_file(&moved).unwrap();
    let (ok, _, err) = gopola(&w, &["index", "--full"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), moved.to_string_lossy().to_string())]);
    assert_eq!(show_json(&w, CX)["turns"].as_array().map(Vec::len), Some(2));
    fs::remove_dir_all(&w.base).unwrap();
}

/// The mirror: Codex unarchives a session before resuming it, so the newer copy can be the one under `sessions/`.
#[test]
fn archive_codex_reads_the_newer_unarchived_copy() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let full = fs::read_to_string(&f).unwrap();
    let archived = w.base.join("codex/archived_sessions").join(f.file_name().unwrap());
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::write(&archived, full.lines().take(39).map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    fs::remove_file(&f).unwrap();
    age(&archived, 7200);
    assert!(gopola(&w, &["archive"]).0);
    fs::write(&f, &full).unwrap();
    fs::remove_file(&archived).unwrap();
    age(&f, 3700);
    let (ok, o, err) = gopola(&w, &["archive", "--json"]);
    assert!(ok && serde_json::from_str::<serde_json::Value>(&o).unwrap()["copied"] == 1, "{o}{err}");
    fs::remove_file(&f).unwrap();
    let (ok, _, err) = gopola(&w, &["index", "--full"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), f.to_string_lossy().to_string())]);
    assert_eq!(show_json(&w, CX)["turns"].as_array().map(Vec::len), Some(2));
    fs::remove_dir_all(&w.base).unwrap();
}

/// Codex's `<rollout>.zst` beside the rollout, streamed with no pledged size, so the frame does not say how long it is.
fn codex_compress(f: &Path) -> PathBuf {
    let zst = PathBuf::from(format!("{}.zst", f.display()));
    let mut enc = zstd::stream::write::Encoder::new(fs::File::create(&zst).unwrap(), 3).unwrap();
    std::io::copy(&mut fs::File::open(f).unwrap(), &mut enc).unwrap();
    enc.finish().unwrap();
    zst
}

/// Codex replaces a quiet rollout by `<name>.jsonl.zst` (written first, then the plain file removed).
#[test]
fn codex_compressed_rollout_keeps_one_row() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let before = show_json(&w, CX)["turns"].clone();
    let zst = codex_compress(&f);
    assert_eq!(zstd::zstd_safe::get_frame_content_size(&fs::read(&zst).unwrap()).unwrap(), None);
    assert_eq!(files_seen(&w), (1.into(), String::new()), "both names exist: one session");
    fs::remove_file(&f).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    let (ok, out, err) = gopola(&w, &["index", "--full", "--json"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["turns_written"], 2, "read again from Codex's .zst: {out}");
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), f.to_string_lossy().to_string())]);
    assert_eq!(show_json(&w, CX)["turns"], before);
    fs::remove_dir_all(&w.base).unwrap();
}

/// `codex archive` moves a rollout under `archived_sessions/`; the row follows it once the old file is gone.
#[test]
fn codex_archived_rollout_keeps_one_row() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let before = show_json(&w, CX)["turns"].clone();
    let moved = w.base.join("codex/archived_sessions").join(f.file_name().unwrap());
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::copy(&f, &moved).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()), "both places hold it: one session");
    assert!(gopola(&w, &["index", "--full"]).0);
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), f.to_string_lossy().to_string())], "sessions/ is read first");
    fs::remove_file(&f).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    assert_eq!(codex_rows(&w), vec![(CX.to_string(), moved.to_string_lossy().to_string())]);
    assert_eq!(show_json(&w, CX)["turns"], before);
    fs::remove_dir_all(&w.base).unwrap();
}

/// Compressed by Codex, then moved under `archived_sessions/`: the row follows the `.zst`, and `index --full` re-reads it.
#[test]
fn codex_compressed_then_archived_rollout_keeps_one_row() {
    let w = setup();
    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let before = show_json(&w, CX)["turns"].clone();
    let zst = codex_compress(&f);
    fs::remove_file(&f).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    let archived = w.base.join("codex/archived_sessions");
    fs::create_dir_all(&archived).unwrap();
    let moved = archived.join(zst.file_name().unwrap());
    let (in_sessions, in_archived) = (vec![(CX.to_string(), f.to_string_lossy().to_string())],
        vec![(CX.to_string(), archived.join(f.file_name().unwrap()).to_string_lossy().to_string())]);
    fs::copy(&zst, &moved).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    assert_eq!(codex_rows(&w), in_sessions, "the old .zst is still there");
    fs::remove_file(&zst).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    assert_eq!(codex_rows(&w), in_archived);
    let (ok, out, err) = gopola(&w, &["index", "--full", "--json"]);
    assert!(ok && err.is_empty(), "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["turns_written"], 2, "read again from the moved .zst: {out}");
    assert_eq!(show_json(&w, CX)["turns"], before);
    // Back under `sessions/`, which is listed first, while the archived `.zst` is still there: the row waits for it to go.
    fs::copy(&moved, &zst).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    assert_eq!(codex_rows(&w), in_archived, "the stored .zst is still there");
    fs::remove_file(&moved).unwrap();
    assert_eq!(files_seen(&w), (1.into(), String::new()));
    assert_eq!(codex_rows(&w), in_sessions);
    fs::remove_dir_all(&w.base).unwrap();
}

/// One Claude session (2026-03-01), one typed Copilot session (03-02 09:00) and three scripted runs (03-02 10:00), indexed.
fn scripted_world() -> World {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    for n in 2..=4 {
        copilot_dir(&w, &format!("c1000000-0000-4000-8000-00000000000{n}"), "scripted-1.0.107.jsonl");
    }
    assert!(gopola(&w, &["index"]).0);
    w
}

fn session_rows(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("2026-")).collect()
}

#[test]
fn recent_folds_scripted_runs() {
    let w = scripted_world();
    let (_, out, _) = gopola(&w, &["recent"]);
    let rows = session_rows(&out);
    assert_eq!(rows.len(), 2, "{out}");
    assert_eq!(rows.iter().filter(|l| l.contains("copilot")).count(), 1, "{out}");
    assert!(out.starts_with("# gopola: demo-app, last 2 sessions (of 2)"), "{out}");
    assert_eq!(out.lines().rev().nth(1), Some("  + 3 scripted Copilot runs, last 2026-03-02 10:00"), "before the footer: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_fold_line_counts_against_lines() {
    let w = scripted_world();
    let (_, out, _) = gopola(&w, &["recent", "--lines", "8"]);
    assert_eq!((session_rows(&out).len(), out.lines().count()), (1, 6), "{out}");
    assert!(out.contains("  + 3 scripted Copilot runs, last "), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_never_gives_up_the_only_session_for_the_fold_line() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    for n in 2..=4 {
        copilot_dir(&w, &format!("c1000000-0000-4000-8000-00000000000{n}"), "scripted-1.0.107.jsonl");
    }
    assert!(gopola(&w, &["index"]).0);
    // Header, a three-line block and the footer fill --lines 5: the fold line has no room.
    let (_, out, _) = gopola(&w, &["recent", "--lines", "5"]);
    assert_eq!((session_rows(&out).len(), out.lines().count()), (1, 5), "{out}");
    assert!(!out.contains("scripted Copilot run"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_without_a_fold_keeps_a_block_that_fits() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    write_session(&w, "4567ef01-0000-4000-8000-000000000002", "2026-03-05T10:00:00.000Z", &[("later prompt", "ok")]);
    for n in 2..=4 {
        copilot_dir(&w, &format!("c1000000-0000-4000-8000-00000000000{n}"), "scripted-1.0.107.jsonl");
    }
    assert!(gopola(&w, &["index"]).0);
    // The runs sit between the two sessions: giving up the older block would leave them out of the fold.
    let (_, out, _) = gopola(&w, &["recent", "--lines", "30"]);
    assert!(out.contains("\n  + 3 scripted Copilot runs, last "), "{out}");
    let (_, out, _) = gopola(&w, &["recent", "--lines", "8"]);
    assert_eq!((session_rows(&out).len(), out.lines().count()), (2, 8), "{out}");
    assert!(!out.contains("scripted Copilot run"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_lists_a_project_that_has_only_scripted_runs() {
    let w = setup();
    for n in 2..=4 {
        copilot_dir(&w, &format!("c1000000-0000-4000-8000-00000000000{n}"), "scripted-1.0.107.jsonl");
    }
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.starts_with("# gopola: demo-app, last 0 sessions (of 0)\n  + 3 scripted Copilot runs, last 2026-03-02 10:00\n# gopola search"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&gopola(&w, &["recent", "--json"]).1).unwrap();
    assert_eq!((v["total"].as_i64(), v["sessions"].as_array().map(Vec::len), v["scripted"]["count"].as_i64()), (Some(0), Some(0), Some(3)), "{v}");
    let (_, out, _) = gopola(&w, &["recent", "--project", "nope"]);
    assert!(out.is_empty(), "no session at all stays silent: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_all_lists_scripted_runs() {
    let w = scripted_world();
    let (_, out, _) = gopola(&w, &["recent", "--all"]);
    assert_eq!(session_rows(&out).len(), 5, "{out}");
    assert!(out.starts_with("# gopola: demo-app, last 5 sessions (of 5)") && !out.contains("scripted Copilot run"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_fold_starts_at_the_oldest_listed_session_or_since() {
    let w = scripted_world();
    write_session(&w, "4567ef01-0000-4000-8000-000000000002", "2026-03-05T10:00:00.000Z", &[("later prompt", "ok")]);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent", "-n", "1"]);
    assert!(!out.contains("scripted Copilot run"), "older than the oldest listed: {out}");
    let (_, out, _) = gopola(&w, &["recent", "-n", "1", "--since", "2026-03-02"]);
    assert!(out.contains("  + 3 scripted Copilot runs, last "), "inside --since: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_fold_names_each_project_with_all_projects() {
    let w = scripted_world();
    copilot_dir_in(&w, "c1000000-0000-4000-8000-000000000005", "scripted-1.0.107.jsonl", &w.base.join("repos/other-app"));
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent", "--project", "all"]);
    assert!(out.contains("\n  + 4 scripted Copilot runs, last 2026-03-02 10:00 (demo-app 3, other-app 1)\n"), "{out}");
    let (_, out, _) = gopola(&w, &["recent"]);
    assert!(out.contains("\n  + 3 scripted Copilot runs, last 2026-03-02 10:00\n"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_json_carries_the_folded_scripted_runs() {
    let w = scripted_world();
    let v: serde_json::Value = serde_json::from_str(&gopola(&w, &["recent", "--json"]).1).unwrap();
    assert_eq!((v["total"].as_i64(), v["scripted"]["count"].as_i64(), v["scripted"]["last"].as_str()),
        (Some(2), Some(3), Some("2026-03-02T10:00:05.000Z")), "{v}");
    let v: serde_json::Value = serde_json::from_str(&gopola(&w, &["recent", "--all", "--json"]).1).unwrap();
    assert_eq!((v["total"].as_i64(), v["scripted"]["count"].as_i64()), (Some(5), Some(0)), "{v}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_json_with_no_sessions_carries_scripted() {
    let w = setup();
    for args in [&["recent", "--json"][..], &["recent", "--all", "--json"]] {
        let v: serde_json::Value = serde_json::from_str(&gopola(&w, args).1).unwrap();
        assert_eq!((v["total"].as_i64(), v["scripted"]["count"].as_i64(), v["scripted"]["last"].is_null()), (Some(0), Some(0), true), "{args:?}: {v}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn search_and_show_mark_copilot() {
    let w = scripted_world();
    let (_, out, _) = gopola(&w, &["search", "readme", "--kind", "reply"]);
    assert!(out.lines().any(|l| l.contains(&CP[..8]) && l.contains("copilot")), "{out}");
    let (_, out, _) = gopola(&w, &["search", "build", "--kind", "prompt"]);
    assert!(out.contains("0123abcd") && !out.contains("copilot"), "{out}");
    let (_, out, _) = gopola(&w, &["show", CP]);
    assert!(out.lines().next().unwrap().contains("copilot"), "{out}");
    let (_, out, _) = gopola(&w, &["show", "0123"]);
    assert!(!out.contains("copilot"), "{out}");
    let (_, out, _) = gopola(&w, &["file", &w.repo.join("README.md").to_string_lossy()]);
    assert!(out.lines().any(|l| l.contains(&CP[..8]) && l.contains("copilot")), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn claude_only_recent_is_unchanged() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    let lines = [
        prompt("2026-09-01T10:00:02.000Z", "add retry to the uploader", &cwd),
        reply("2026-09-01T10:00:09.000Z", "Retry added with backoff.", None),
        prompt("2026-09-01T10:05:00.000Z", "now the tests for it", &cwd),
        reply("2026-09-01T10:06:00.000Z", "Three tests cover the retry path.", None),
    ];
    fs::write(transcript_dir(&w).join("0123abcd-0000-4000-8000-000000000001.jsonl"), lines.join("\n") + "\n").unwrap();
    write_session(&w, "4567ef01-0000-4000-8000-000000000002", "2026-09-02T09:00:00.000Z", &[("fix the build", "Fixed.")]);
    commit_at(&w.repo, "uploader: retry with backoff", "2026-09-01T10:00:08+00:00");
    assert!(gopola(&w, &["index"]).0);
    // Captured from gopola 0.15.3, built before any Copilot code, on this world.
    let before = "\
# gopola: demo-app, last 2 sessions (of 2)
2026-09-02 09:00  4567ef01  main  1 turn  0 commits
  > fix the build
  < Fixed.
2026-09-01 10:00  0123abcd  main  2 turns  1 commit
  > add retry to the uploader
  ✓ uploader: retry with backoff
  < Three tests cover the retry path.
# gopola search <words> | show <id> | timeline <id:turn> | file <path> | digest --since <date>
";
    assert_eq!(gopola(&w, &["recent"]).1, before);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn claude_only_commands_refuse_copilot() {
    let w = setup();
    copilot_dir(&w, CP, "scripted-1.0.107.jsonl");
    assert!(gopola(&w, &["index"]).0);
    for cmd in ["commands", "dispatches", "hooks"] {
        let (ok, out, err) = gopola(&w, &[cmd, "c1000000"]);
        assert!(!ok && out.is_empty(), "{cmd}: {out}");
        assert!(err.contains(&format!("{cmd} reads Claude Code sessions only for now; c1000000 is a Copilot session")), "{cmd}: {err}");
    }
    let (ok, out, err) = gopola(&w, &["weight", "c1000000", "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["id"], CP);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_by_id_reports_segments_without_content() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    git(&w.repo, &["switch", "-c", "Ex-Ample"]);
    fs::write(w.repo.join("f.txt"), "synthetic change").unwrap();
    git(&w.repo, &["add", "f.txt"]);
    let status = Command::new("git").arg("-C").arg(&w.repo)
        .env("GIT_AUTHOR_DATE", "2026-03-02T09:00:08+00:00")
        .env("GIT_COMMITTER_DATE", "2026-03-03T09:00:00+00:00")
        .args(["commit", "-q", "-m", "synthetic change"]).status().unwrap();
    assert!(status.success());
    let head = Command::new("git").arg("-C").arg(&w.repo).args(["rev-parse", "HEAD"]).output().unwrap();
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let short_head = Command::new("git").arg("-C").arg(&w.repo).args(["rev-parse", "--short", "HEAD"]).output().unwrap();
    let short_head = String::from_utf8_lossy(&short_head.stdout).trim().to_string();
    commit_at(&w.repo, "outside the seat window", "2026-03-03T09:00:01+00:00");
    assert!(gopola(&w, &["index"]).0);
    let (ok, out, err) = gopola(&w, &["weight", CP, "--json"]);
    assert!(ok, "{err}");
    assert!(!out.contains("gopola-weight-unique-secret-7391"), "{out}");
    assert!(!out.contains("src/lib.rs"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["id"], CP);
    assert_eq!(v["stop_reason"], "done");
    assert_eq!(v["wall_ms"], 15_000);
    assert_eq!(v["totals"]["credits"], 3.5);
    assert!((v["totals"]["credits_per_call"].as_f64().unwrap() - 3.5 / 3.0).abs() < 1e-12);
    assert_eq!(v["segments"][0]["credits_per_call"], 1.5);
    assert_eq!(v["segments"][1]["credits_per_call"], 0.5);
    assert_eq!(v["first_call_input_floor"], 125);
    assert_eq!(v["segments"][0]["floor_split"], serde_json::json!({
        "system_tokens": 2, "tool_definitions_tokens": 1, "conversation_tokens": 7
    }));
    assert_eq!(v["segments"][1]["floor_split"], serde_json::json!({
        "system_tokens": 5, "tool_definitions_tokens": 3, "conversation_tokens": 10
    }));
    assert_eq!(v["segments"][0]["shutdown"]["total_nano_aiu"], 3_000_000_000u64);
    assert_eq!(v["segments"][1]["shutdown"]["total_nano_aiu"], 500_000_000);
    assert_eq!(v["segments"][1]["shutdown"]["code_changes"]["files_modified"], 2);
    assert_eq!(v["segments"][0]["first_call_input_floor"], 125);
    assert_eq!(v["segments"][1]["first_call_input_floor"], 80);
    assert_eq!(v["system_prompt_bytes"], "gopola-weight-unique-secret-7391".len());
    assert_eq!(v["brief_bytes"], "gopola-weight-unique-secret-7391".len());
    assert_eq!(v["segments"].as_array().unwrap().len(), 2);
    assert_eq!(v["segments"][0]["max_ai_credits"], 4);
    assert_eq!(v["segments"][0]["stop_reason"], "cap");
    assert_eq!(v["segments"][0]["credits"], 3.0);
    assert_eq!(v["segments"][1]["max_ai_credits"], 4, "resume keeps the start cap");
    assert_eq!(v["segments"][1]["stop_reason"], "done");
    assert_eq!(v["segments"][1]["selected_model"], "auto");
    assert_eq!(v["segments"][0]["head_at_stop"], head);
    assert_eq!(v["segments"][0]["commits"][0], short_head);
    assert_eq!(v["calls"][0]["model"], "Demo-Model-A");
    assert_eq!(v["calls"][1]["model"], "Demo-Model-B");
    assert_eq!(v["calls"][2]["model"], "Demo-Model-C");
    assert_eq!(v["calls"][0]["fresh_input"], 100);
    assert_eq!(v["calls"][0]["cache_read"], 25);
    assert_eq!(v["calls"][0]["cache_write"], 10);
    assert_eq!(v["calls"][0]["output"], 5);
    assert_eq!(v["calls"][0]["wall_ms"], 1200);
    assert_eq!(v["tool_calls"]["by_name"]["bash"], 2);
    assert_eq!(v["tool_calls"]["by_name"]["view"], 1);
    assert_eq!(v["tool_calls"]["chained_shell_calls"], 1);
    assert_eq!(v["tool_calls"]["refusals"][0]["source"], "copilot-permission");
    assert_eq!(v["tool_calls"]["refusals"][0]["command"], "printf");
    assert_eq!(v["tool_calls"]["refusals"][1]["source"], "hook:zshguard");
    assert_eq!(v["tool_calls"]["refusals"][1]["rules"][0], "inplace");
    assert_eq!(v["tool_calls"]["refusals"][1]["rules"], serde_json::json!(["inplace", "?", "?", "?"]));
    assert_eq!(v["tool_calls"]["refusals"][2]["source"], "other");
    assert_eq!(v["tool_calls"]["refusals"][2]["command"], "");
    let fixture = fs::read_to_string(&f).unwrap();
    let mut failures = Vec::new();
    for (name, body) in [
        ("odd backtick", "it`s here\n1. [gopola-weight-unique-secret-7391](#setup) first"),
        ("code fence", "```text\n1. [gopola-weight-unique-secret-7391](#x)\n```"),
    ] {
        let mut records: Vec<serde_json::Value> = fixture.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        records[9]["data"]["error"]["message"] = format!(
            "Denied by preToolUse hook: zshguard refused this command before it ran.\n1. [inplace-redirect] `cat > docs/notes.md <<'EOF'\n{body}\nEOF`\n2. [unknown-rule] command refused"
        ).into();
        fs::write(&f, records.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
        for args in [vec!["weight", CP], vec!["weight", CP, "--json"]] {
            let (ok, out, err) = gopola(&w, &args);
            assert!(ok, "{err}");
            let rules_ok = if args.contains(&"--json") {
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                v["tool_calls"]["refusals"][1]["rules"] == serde_json::json!(["inplace-redirect", "?"])
            } else {
                out.contains("refusal hook:zshguard  command=printf  rules=inplace-redirect,?")
            };
            if out.contains("gopola-weight-unique-secret-7391") || !rules_ok {
                failures.push(format!("{name}: {args:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "unsafe or incorrect rule output: {failures:?}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_reports_only_known_zshguard_rules() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    let mut records: Vec<serde_json::Value> = fs::read_to_string(&f).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    let known = [
        "modifier", "equals", "nomatch", "for-scalar", "tied-var", "bfs-time", "pipestatus",
        "list-arg", "split-arg", "adb-stdin", "pipe-filter", "bg-filter", "pgrep-wait",
        "clean-guard", "inplace", "inplace-redirect", "alt-arg", "tools-filter", "switch-track",
        "gh-auth-switch", "gh-wrong-account", "git-grep-escape",
    ];
    let rules = known.iter().copied().chain(["future-rule", "Invalid_rule"]);
    let lines = rules.enumerate().map(|(i, rule)| format!("{}. [{rule}] refused\n", i + 1)).collect::<String>();
    records[9]["data"]["error"]["message"] =
        format!("Denied by preToolUse hook: zshguard refused this command before it ran.\n{lines}").into();
    fs::write(&f, records.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
    let expected: Vec<&str> = known.into_iter().chain(["?", "?"]).collect();
    for args in [vec!["weight", f.to_str().unwrap()], vec!["weight", f.to_str().unwrap(), "--json"]] {
        let (ok, out, err) = gopola(&w, &args);
        assert!(ok, "{err}");
        if args.contains(&"--json") {
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["tool_calls"]["refusals"][1]["rules"], serde_json::json!(expected));
        } else {
            assert!(out.contains(&format!("rules={}", expected.join(","))), "{out}");
        }
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_unavailable_git_is_unknown() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    git(&w.repo, &["switch", "-c", "Ex-Ample"]);
    commit_at(&w.repo, "before the seat", "2026-03-01T09:00:00+00:00");
    let records: Vec<serde_json::Value> = fs::read_to_string(&f).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    let v: serde_json::Value = serde_json::from_str(&gopola(&w, &["weight", f.to_str().unwrap(), "--json"]).1).unwrap();
    assert_eq!(v["segments"][0]["commits"], serde_json::json!([]));
    for (key, value) in [
        ("gitRoot", w.base.join("removed-root").to_string_lossy().to_string()),
        ("branch", "removed-branch".into()),
        ("branch", String::new()),
    ] {
        let mut changed = records.clone();
        for record in &mut changed {
            if matches!(record["type"].as_str(), Some("session.start" | "session.resume")) {
                record["data"]["context"][key] = value.clone().into();
            }
        }
        fs::write(&f, changed.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
        let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap(), "--json"]);
        assert!(ok, "{err}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["segments"][0]["commits"].is_null(), "{out}");
        assert_eq!(v["segments"][0]["head_at_stop"], "unknown");
        let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap()]);
        assert!(ok, "{err}");
        assert!(out.contains("stop-head=unknown  commits=unknown"), "{out}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_shell_chains_respect_quotes() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    let mut records: Vec<serde_json::Value> = fs::read_to_string(&f).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    for (command, chains) in [
        ("git log | head", 0),
        ("rg 'a;b&&c||d' src", 0),
        ("git log --format=\"%h;%s\" | head", 0),
        ("printf \"a\\\";b\" 'a\\;b' | cat", 0),
        ("printf a\\;b", 0),
        ("git status && cargo test", 1),
        ("git status || cargo test", 1),
        ("git status; cargo test", 1),
        ("git status\ncargo test", 1),
        ("git status\n", 0),
        ("python3 - <<'EOF'\nimport sys\nprint(1)\nEOF", 0),
        ("cat <<EOF\none; two && three || four\nEOF\n", 0),
        ("cat <<EOF && git status\none\nEOF\n", 1),
        ("cat <<EOF\none\nEOF\ngit status", 1),
        ("cat <<-EOF\n\tone\n\tEOF\n", 0),
        ("cat <<ONE <<'TWO'\nfirst\nONE\nsecond; third\nTWO\n", 0),
        ("printf '<<EOF\\n' && git status", 1),
        ("x=$((1<<2))\ngit status", 1),
        ("x=$((1<<2))", 0),
        ("x=$(((1<<2) + (3<<1))) && git status", 1),
        ("x=$((1 && 2 || 3))", 0),
        ("x=$((1<<2))\ncat <<EOF\none; two\nEOF", 1),
    ] {
        records[5]["data"]["arguments"]["command"] = command.into();
        fs::write(&f, records.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
        let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap(), "--json"]);
        assert!(ok, "{err}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tool_calls"]["chained_shell_calls"], chains, "{command}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_refusal_words_hide_paths() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    let fixture = fs::read_to_string(&f).unwrap();
    for (command, expected) in [
        ("./deploy-acmecorp.sh", "<path>"),
        ("/opt/x/acmecorp-tool", "<path>"),
        ("dir/tool", "<path>"),
        ("curl https://example.invalid", "curl"),
        ("TOKEN='invented secret with spaces' /work/demo/bin/curl", "<path>"),
        ("'unsafe[content]'", "?"),
        ("abcdefghijklmnopqrstuvwxyz0123456789-extra", "?"),
        ("cd '/work/a;b' && TOKEN='invented value' /work/demo/bin/cargo test", "<path>"),
        ("cd /work/demo || cd /work/other; TOKEN=value curl https://example.invalid", "curl"),
        ("TOKEN=value; cd /work/demo\nrg 'a&&b' src", "rg"),
    ] {
        let mut records: Vec<serde_json::Value> = fixture.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        records[5]["data"]["arguments"]["command"] = command.into();
        fs::write(&f, records.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
        for args in [vec!["weight", f.to_str().unwrap()], vec!["weight", f.to_str().unwrap(), "--json"]] {
            let (ok, out, err) = gopola(&w, &args);
            assert!(ok, "{err}");
            if args.contains(&"--json") {
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                assert_eq!(v["tool_calls"]["refusals"][0]["command"], expected);
            } else {
                assert!(out.contains(&format!("refusal copilot-permission  command={expected}  rules=")), "{out}");
            }
        }
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_by_path_uses_compact_text_and_optional_call_table() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-segments.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy()]);
    assert!(ok, "{err}");
    // 0.19.0's Opus review, S3: --agents on a Copilot session was dropped without a word.
    let (ok2, out2, err2) = gopola(&w, &["weight", &f.to_string_lossy(), "--agents"]);
    assert!(ok2 && out2 == out && err2.contains("--agents reads Claude Code subagents only"), "{err2}");
    assert!(out.starts_with("# weight c1000000"));
    assert!(out.contains("credits/call=1.1667"), "{out}");
    assert!(out.contains("credits/call=1.5000"), "{out}");
    assert!(out.contains("shutdown floor split: system=2 tool-definitions=1 conversation=7"), "{out}");
    assert!(out.contains("shutdown floor split: system=5 tool-definitions=3 conversation=10"), "{out}");
    assert_eq!(out.matches("first call input floor=").count(), 2, "{out}");
    assert!(!out.contains("gopola-weight-unique-secret-7391"), "{out}");
    assert!(!out.contains("model    input"), "{out}");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy(), "-n", "5"]);
    assert!(ok, "{err}");
    assert!(out.contains("model    input"), "{out}");
    assert!(!out.contains("gopola-weight-unique-secret-7391"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_single_segment_prints_floor_once() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-cap.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert_eq!(out.matches("first call input floor=").count(), 1, "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_calls_reaching_credit_cap_stop_as_cap() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-cap.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy(), "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stop_reason"], "cap");
    assert_eq!(v["stop_credits"], 1.0);
    assert_eq!(v["segments"][0]["max_ai_credits"], 1);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_error_shutdown_stops_as_error() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-error.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy(), "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["stop_reason"], "error");
    let mut records: Vec<serde_json::Value> = fs::read_to_string(&f).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    records[1]["data"]["usage"]["finishReason"] = "error".into();
    records[2]["data"]["shutdownType"] = "routine".into();
    fs::write(&f, records.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
    let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap(), "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["stop_reason"], "done");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_killed_segment_ends_before_resume() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-killed.jsonl").join("events.jsonl");
    let records = fs::read_to_string(&f).unwrap();
    let mut resume: serde_json::Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
    resume["type"] = "session.resume".into();
    resume["timestamp"] = "2026-03-02T09:10:00.000Z".into();
    resume["data"]["resumeTime"] = "2026-03-02T09:10:00.000Z".into();
    let shutdown = serde_json::json!({"type": "session.shutdown", "timestamp": "2026-03-02T09:10:02.000Z",
        "data": {"shutdownType": "routine"}});
    fs::write(&f, format!("{records}{resume}\n{shutdown}\n")).unwrap();
    let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap(), "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["segments"][0]["stop_reason"], "killed");
    assert_eq!(v["segments"][0]["stop_time"], "2026-03-02T09:00:03.000Z");
    assert_eq!(v["segments"][0]["wall_ms"], 3000);
    assert!(v["segments"][0]["floor_split"].is_null());
    assert!(v["segments"][0]["shutdown"].is_null());
    assert_eq!(v["segments"][1]["stop_reason"], "done");
    assert_eq!(v["segments"][1]["credits_per_call"], 0.0);
    assert_eq!(v["wall_ms"], 5000);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_shutdown_without_type_is_done() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-killed.jsonl").join("events.jsonl");
    let records = fs::read_to_string(&f).unwrap();
    let shutdown = serde_json::json!({"type": "session.shutdown", "timestamp": "2026-03-02T09:00:04.000Z",
        "data": {}});
    let later = serde_json::json!({"type": "session.info", "timestamp": "2026-03-02T09:00:10.000Z",
        "data": {"message": "Finished"}});
    fs::write(&f, format!("{records}{shutdown}\n{later}\n")).unwrap();
    let (ok, out, err) = gopola(&w, &["weight", f.to_str().unwrap(), "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stop_reason"], "done");
    assert_eq!(v["segments"][0]["stop_time"], "2026-03-02T09:00:04.000Z");
    assert_eq!(v["segments"][0]["wall_ms"], 4000);
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_weight_without_shutdown_stops_as_killed() {
    let w = setup();
    let f = copilot_dir(&w, CP, "weight-killed.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy(), "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["stop_reason"], "killed");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn recent_folds_scripted_runs_per_harness() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    for n in 2..=3 {
        copilot_dir(&w, &format!("c1000000-0000-4000-8000-00000000000{n}"), "scripted-1.0.107.jsonl");
    }
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["recent"]);
    assert_eq!(session_rows(&out).len(), 1, "{out}");
    assert_eq!(out.lines().rev().nth(1), Some("  + 2 scripted Copilot runs, 1 scripted Codex run, last 2026-03-02 10:00"), "before the footer: {out}");
    let (_, out, _) = gopola(&w, &["recent", "--project", "all"]);
    assert!(out.contains("\n  + 2 scripted Copilot runs, 1 scripted Codex run, last 2026-03-02 10:00 (demo-app 3)\n"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn search_and_show_mark_codex() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["file", &w.repo.join("NOTES.md").to_string_lossy()]);
    assert!(out.lines().any(|l| l.contains(&CX[..8]) && l.contains(" codex > ")), "{out}");
    let (_, out, _) = gopola(&w, &["search", "heading", "--kind", "reply"]);
    assert!(out.lines().any(|l| l.contains(&CX_TUI[..8]) && l.contains("codex")), "{out}");
    let (_, out, _) = gopola(&w, &["search", "build", "--kind", "prompt"]);
    assert!(out.contains("0123abcd") && !out.contains("codex"), "{out}");
    let (_, out, _) = gopola(&w, &["show", CX_TUI]);
    assert!(out.lines().next().unwrap().contains("codex"), "{out}");
    // A plain `recent` lists an interactive Codex session as its own row, beside the Claude one.
    let (_, out, _) = gopola(&w, &["recent"]);
    let rows = session_rows(&out);
    assert_eq!(rows.len(), 2, "{out}");
    assert!(rows[0].contains(&CX_TUI[..8]) && rows[0].contains("  codex  "), "{out}");
    assert!(rows[1].contains("0123abcd") && !rows[1].contains("codex"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn claude_only_commands_refuse_codex() {
    let w = setup();
    let rollout = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    let (ok, out, err) = gopola(&w, &["weight", &rollout.to_string_lossy()]);
    assert!(!ok && out.is_empty(), "weight <rollout path>: {out}");
    assert!(err.trim_end().ends_with(&format!("weight reads Claude Code sessions only for now; {} is a Codex session", &CX[..8])), "{err}");
    assert!(gopola(&w, &["index"]).0);
    for cmd in ["commands", "weight", "dispatches", "hooks"] {
        let (ok, out, err) = gopola(&w, &[cmd, &CX[..8]]);
        assert!(!ok && out.is_empty(), "{cmd}: {out}");
        assert!(err.trim_end().ends_with(&format!("{cmd} reads Claude Code sessions only for now; {} is a Codex session", &CX[..8])), "{cmd}: {err}");
    }
    let (ok, _, err) = gopola(&w, &["context", &CX[..8]]);
    assert!(!ok && err.trim_end().ends_with(&format!("context reads Copilot sessions only; {} is a Codex session", &CX[..8])), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// A rollout Codex compressed is refused by its own id too.
#[test]
fn weight_refuses_a_compressed_codex_rollout_by_its_id() {
    let w = setup();
    let zst = codex_compress(&codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo));
    let (ok, out, err) = gopola(&w, &["weight", &zst.to_string_lossy()]);
    assert!(!ok && out.is_empty(), "{out}");
    assert!(err.trim_end().ends_with(&format!("weight reads Claude Code sessions only for now; {} is a Codex session", &CX[..8])), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn commands_grep_reads_claude_sessions_only() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    copilot_dir(&w, CP, "scripted-1.0.107.jsonl");
    assert!(gopola(&w, &["index"]).0);
    let (ok, out, err) = gopola(&w, &["commands", "--grep", "cargo", "--project", "demo-app", "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["sessions_read"], 1, "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// Like the Claude archive test, for Copilot: copies in `archive-copilot`, `events.jsonl` only, read back once the folder is gone.
#[test]
fn archive_keeps_copilot_sessions() {
    let w = setup();
    let d = copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    fs::write(d.join("workspace.yaml"), "id: x\n").unwrap();
    for f in ["events.jsonl", "workspace.yaml"] { age(&d.join(f), 7200) }
    let (ok, o, err) = gopola(&w, &["archive", "--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&o).unwrap();
    assert_eq!(v["copied"], 1, "{o}");
    assert_eq!(v["archive_copilot"], w.base.join("archive-copilot").to_string_lossy().as_ref(), "{o}");
    let copy = w.base.join("archive-copilot").join(CP);
    assert_eq!(unzstd(&copy.join("events.jsonl.zst")), fs::read(d.join("events.jsonl")).unwrap());
    assert!(!copy.join("workspace.yaml.zst").exists(), "gopola never reads workspace.yaml");
    assert!(!w.base.join("archive").join(CP).exists(), "not inside the Claude mirror");
    assert!(gopola(&w, &["archive"]).1.contains("archive-copilot"));
    fs::remove_dir_all(&d).unwrap();
    assert!(gopola(&w, &["index", "--full"]).0);
    let (_, out, _) = gopola(&w, &["show", CP, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["turns"].as_array().map(Vec::len), Some(2), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn doctor_reports_copilot() {
    let w = setup();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(!out.contains("Copilot"), "no Copilot folder, no Copilot lines: {out}");

    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    // Only a `*.json` counts as a hook file.
    fs::create_dir_all(w.base.join("copilot/hooks")).unwrap();
    fs::write(w.base.join("copilot/hooks/notes.txt"), "gopola copilot-hook").unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("info  Copilot sessions: 0\n") && out.contains("fix   Copilot sessionStart hook missing"), "{out}");
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("ok    Copilot record types gopola does not know: none"), "{out}");
    assert!(out.contains("info  Copilot sessions: 1 (versions 1.0.107 ×1)"), "{out}");
    assert!(out.contains("fix   Copilot sessionStart hook missing"), "{out}");

    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"type":"widget.spun","data":{},"id":"e0000098","timestamp":"2026-03-02T09:06:00.000Z","parentId":null}"# + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    let hooks = w.base.join("copilot/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("other.json"), r#"{"version":1,"hooks":{}}"#).unwrap();
    fs::write(hooks.join("gopola.json"), r#"{"hooks":{"sessionStart":[{"bash":"$HOME/.local/bin/gopola copilot-hook"}]}}"#).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("look  Copilot record types gopola does not know: widget.spun (in c1000000)"), "{out}");
    assert!(out.contains("ok    Copilot sessionStart hook wired"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// A field renamed inside a known record type makes every such line unreadable, and `index` skips them without a word:
/// doctor counts them per session. A last line still being written (no newline yet) is not counted.
#[test]
fn doctor_counts_unreadable_copilot_lines() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("ok    Copilot lines gopola cannot read: none"), "{out}");
    let renamed = r#"{"type":"user.message","data":{"text":"renamed field","source":null},"id":"e0000096","timestamp":"2026-03-02T09:06:00.000Z","parentId":null}"#;
    fs::write(&f, fs::read_to_string(&f).unwrap() + renamed + "\n" + renamed + "\n" + r#"{"type":"user.mess"#).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("look  Copilot lines gopola cannot read: 2 in c1000000"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// The row keeps every type an older gopola could not place: one it now knows is no longer named.
#[test]
fn doctor_leaves_out_a_copilot_type_gopola_now_knows() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"type":"widget.spun","data":{},"id":"e0000098","timestamp":"2026-03-02T09:06:00.000Z","parentId":null}"# + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    rusqlite_open(&w.db).execute("update meta set v = replace(v, 'widget.spun', 'session.info') where k = 'copilot_unknown_types'", []).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("ok    Copilot record types gopola does not know: none"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn doctor_sees_a_copilot_transcript_that_changed() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    assert!(gopola(&w, &["index"]).0);
    // A machine with Copilot and no Claude projects folder still gets the check.
    fs::remove_dir_all(&w.projects).unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("ok    index is current"));
    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"type":"session.info","data":{},"id":"e0000097","timestamp":"2026-03-02T09:06:00.000Z","parentId":null}"# + "\n").unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   1 transcript(s) changed since the last index"));
    fs::remove_dir_all(&w.base).unwrap();
}

/// The index skips a folder whose first record is no `session.start`; `gopola index` cannot make it current.
#[test]
fn doctor_leaves_out_a_copilot_folder_the_index_skips() {
    let w = setup();
    let f = copilot_dir(&w, "c1000000-0000-4000-8000-000000000009", "typed-1.0.107.jsonl").join("events.jsonl");
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, full.split_once('\n').unwrap().1).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert!(gopola(&w, &["doctor"]).1.contains("ok    index is current"));
    copilot_dir(&w, CP, "typed-1.0.107.jsonl");
    assert!(gopola(&w, &["doctor"]).1.contains("fix   1 transcript(s) changed since the last index"), "a new session still counts");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn doctor_reports_codex() {
    let w = setup();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(!out.contains("Codex"), "no Codex folder, no Codex lines: {out}");

    let f = codex_rollout(&w, CX, "exec-0.167.0.jsonl", &w.repo);
    // A Claude session beside it is not counted among Codex's.
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    assert!(gopola(&w, &["index"]).0);
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("fix   Codex SessionStart hook missing"), "{out}");
    assert!(out.contains("ok    Codex record types gopola does not know: none"), "{out}");
    assert!(out.contains("ok    Codex lines gopola cannot read: none"), "{out}");
    assert!(out.contains("info  Codex sessions: 1 (versions 0.167.0 ×1)"), "{out}");
    assert!(out.contains("ok    index is current"), "{out}");

    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"timestamp":"2026-03-02T09:06:00.000Z","type":"compacted","payload":{"message":"demo summary"}}"# + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    fs::write(w.base.join("codex/hooks.json"), r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"gopola codex-hook"}]}]}}"#).unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   Codex SessionStart hook missing"));
    fs::write(w.base.join("codex/hooks.json"), r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"$HOME/.local/bin/gopola codex-hook"}]}]}}"#).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("look  Codex record types gopola does not know: compacted (in 019cadc0)"), "{out}");
    assert!(out.contains("ok    Codex SessionStart hook wired"), "{out}");
    // Codex also takes hooks from `[hooks]` in config.toml.
    fs::remove_file(w.base.join("codex/hooks.json")).unwrap();
    fs::write(w.base.join("codex/config.toml"), "# gopola codex-hook\n[hooks.OtherEvent]\ncommand = \"gopola codex-hook\"\n").unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   Codex SessionStart hook missing"));
    // SessionStart is a list of matcher groups: alone, `[[hooks.SessionStart.hooks]]` makes it a table.
    fs::write(w.base.join("codex/config.toml"), "[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"gopola codex-hook\"\n").unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   Codex SessionStart hook missing"));
    fs::write(w.base.join("codex/config.toml"), "[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ncommand = \"gopola codex-hook\"\nbroken = [\n").unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   Codex SessionStart hook missing"), "not TOML: not wired");
    for toml in [
        "[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"gopola codex-hook\"\n",
        "[hooks]\nSessionStart = [{ hooks = [{ type = \"command\", command = \"gopola codex-hook\" }] }]\n",
    ] {
        fs::write(w.base.join("codex/config.toml"), toml).unwrap();
        assert!(gopola(&w, &["doctor"]).1.contains("ok    Codex SessionStart hook wired"), "{toml}");
    }

    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"timestamp":"2026-03-02T09:07:00.000Z","type":"compacted","payload":{"message":"demo summary"}}"# + "\n").unwrap();
    assert!(gopola(&w, &["doctor"]).1.contains("fix   1 transcript(s) changed since the last index"));

    // A renamed field makes a line unreadable; a rollout Codex has compressed is still read.
    fs::write(&f, fs::read_to_string(&f).unwrap() + r#"{"timestamp":"2026-03-02T09:08:00.000Z","type":"event_msg","payload":{"kind":"task_started"}}"# + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    codex_compress(&f);
    fs::remove_file(&f).unwrap();
    let (_, out, _) = gopola(&w, &["doctor"]);
    assert!(out.contains("look  Codex lines gopola cannot read: 1 in 019cadc0"), "{out}");
    assert!(out.contains("ok    index is current"), "{out}");
    // A rollout the index skips (its first record no `session_meta`) is not waiting for `gopola index`.
    let skipped = codex_rollout(&w, CX_FORK, "fork-0.167.0.jsonl", &w.repo);
    fs::write(&skipped, fs::read_to_string(&skipped).unwrap().split_once('\n').unwrap().1).unwrap();
    assert!(gopola(&w, &["index"]).0);
    assert!(gopola(&w, &["doctor"]).1.contains("ok    index is current"));
    fs::remove_dir_all(&w.base).unwrap();
}

/// `gopola copilot-hook` as Copilot runs it: the session's cwd on stdin, `parent_args` standing in for the `ps` lookup.
fn copilot_hook(w: &World, db: &Path, parent_args: &str) -> (i32, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .env("GOPOLA_COPILOT_PARENT_ARGS", parent_args).current_dir(&w.repo).arg("copilot-hook")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(serde_json::json!({"cwd": w.repo, "sessionId": CP}).to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into())
}

/// The hook fires at the first prompt, before Copilot's title call, so its own session is indexed as scripted then: the
/// list it opens with must not count it (the owner's session 55055b3e said "2 scripted Copilot runs").
#[test]
fn copilot_hook_leaves_its_own_session_out() {
    let w = setup();
    let f = copilot_dir(&w, CP, "typed-1.0.107.jsonl").join("events.jsonl");
    let full = fs::read_to_string(&f).unwrap();
    fs::write(&f, &full[..full.find(r#"{"type":"model.message""#).unwrap()]).unwrap();
    copilot_dir(&w, "c2000000-0000-4000-8000-000000000002", "scripted-1.0.107.jsonl");
    let (code, out) = copilot_hook(&w, &w.db, "copilot --resume");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["additionalContext"].as_str().unwrap().contains("  + 1 scripted Copilot run, last "), "{out}");
    // `--all` lists scripted runs, so only `--exclude` keeps the session out of the total and the rows.
    let (_, out, _) = gopola(&w, &["recent", "--project", "demo-app", "--all", "--exclude", CP, "--json"]);
    assert!(!out.contains(CP) && out.contains(r#""total":1"#), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_hook_gives_recent_as_context() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    let (code, out) = copilot_hook(&w, &w.db, "copilot --resume");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["additionalContext"].as_str().unwrap().contains("# gopola: demo-app"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// A scripted run's stdin carries its whole prompt: a hook that exits without reading it breaks Copilot's write.
#[test]
fn copilot_hook_reads_all_of_its_stdin_in_prompt_runs() {
    use std::io::Write;
    let w = setup();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .env("GOPOLA_COPILOT_PARENT_ARGS", "copilot -p brief").current_dir(&w.repo).arg("copilot-hook")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let payload = serde_json::json!({"cwd": w.repo, "initialPrompt": "x".repeat(300_000)}).to_string();
    let written = child.stdin.take().unwrap().write_all(payload.as_bytes());
    let out = child.wait_with_output().unwrap();
    assert!(written.is_ok() && out.status.success() && out.stdout.is_empty(), "{written:?} {out:?}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_hook_is_silent_in_prompt_runs() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    for args in ["copilot -p review --no-ask-user", "/usr/bin/copilot --prompt=review", "copilot --prompt review"] {
        assert_eq!(copilot_hook(&w, &w.db, args), (0, String::new()), "{args}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

/// An empty `GOPOLA_COPILOT_PARENT_ARGS` stands for "no `copilot` ancestor found": the hook cannot tell, so it says nothing.
#[test]
fn copilot_hook_is_silent_without_a_copilot_parent() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    assert_eq!(copilot_hook(&w, &w.db, ""), (0, String::new()));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_hook_is_silent_for_a_project_with_no_sessions() {
    let w = setup();
    assert_eq!(copilot_hook(&w, &w.db, "copilot --resume"), (0, String::new()));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn copilot_hook_exits_zero_when_index_fails() {
    use std::os::unix::fs::PermissionsExt;
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    let ro = w.base.join("ro");
    fs::create_dir_all(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let r = copilot_hook(&w, &ro.join("gopola.db"), "copilot --resume");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r, (0, String::new()));
    fs::remove_dir_all(&w.base).unwrap();
}

/// The real lookup: a link to `sh` named `copilot` launches the hook, so `ps` shows the argument it was started with.
#[test]
fn copilot_hook_reads_its_parent_through_ps() {
    use std::io::Write;
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    let copilot = w.base.join("bin/copilot");
    fs::create_dir_all(copilot.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sh", &copilot).unwrap();
    let script = format!("{} copilot-hook; true", env!("CARGO_BIN_EXE_gopola"));
    let run = |flag: &str| {
        let mut child = Command::new(&copilot).args(["-c", &script, flag])
            .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
            .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
            .env_remove("GOPOLA_COPILOT_PARENT_ARGS").current_dir(&w.repo)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(serde_json::json!({"cwd": w.repo}).to_string().as_bytes()).unwrap();
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
    };
    assert!(run("--resume").contains("# gopola: demo-app"));
    assert_eq!(run("-p"), "");
    fs::remove_dir_all(&w.base).unwrap();
}

/// Codex's `SessionStart` stdin (keys as the 2026-11-12 probe logged them), for the session `CX_TUI` in the repo.
fn codex_start(w: &World, source: &str) -> serde_json::Value {
    serde_json::json!({"cwd": w.repo, "hook_event_name": "SessionStart", "model": "demo-model", "permission_mode": "default",
        "session_id": CX_TUI, "source": source, "transcript_path": null})
}

/// `gopola codex-hook` as Codex runs it: `input` on stdin, `parent_args` standing in for the `ps` lookup.
fn codex_hook(w: &World, db: &Path, parent_args: &str, input: &serde_json::Value) -> (i32, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .env("GOPOLA_CODEX_PARENT_ARGS", parent_args).current_dir(&w.repo).arg("codex-hook")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into())
}

#[test]
fn codex_hook_gives_recent_as_context() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    codex_rollout(&w, CX_TUI, "tui-0.167.0.jsonl", &w.repo);
    let (code, out) = codex_hook(&w, &w.db, "codex", &codex_start(&w, "startup"));
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart", "{out}");
    let context = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(context.contains("# gopola: demo-app") && context.contains("fix the build"), "{out}");
    assert!(!context.contains("019cadf7"), "its own session is left out: {out}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// The probe's exec run: the hook's parent was `codex exec say hi`.
#[test]
fn codex_hook_is_silent_in_exec_runs() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    for args in ["codex exec say hi", "/opt/homebrew/bin/codex e say hi"] {
        assert_eq!(codex_hook(&w, &w.db, args, &codex_start(&w, "startup")), (0, String::new()), "{args}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

/// `codex [OPTIONS] <COMMAND>`: root options and their values come before the subcommand.
#[test]
fn codex_hook_skips_root_options_before_the_subcommand() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    for args in ["codex -c k=v exec say hi", "codex -m X e say hi", "codex --model=X --search exec say hi"] {
        assert_eq!(codex_hook(&w, &w.db, args, &codex_start(&w, "startup")), (0, String::new()), "{args}");
    }
    // A model named `exec` is an option's value, not the subcommand.
    for args in ["codex -c k=v resume --last", "codex -m exec say hi", "codex --search"] {
        assert!(codex_hook(&w, &w.db, args, &codex_start(&w, "startup")).1.contains("# gopola: demo-app"), "{args}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_hook_answers_resume_not_compact() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    assert!(codex_hook(&w, &w.db, "codex resume", &codex_start(&w, "resume")).1.contains("# gopola: demo-app"));
    for source in ["compact", "clear"] {
        assert_eq!(codex_hook(&w, &w.db, "codex", &codex_start(&w, source)), (0, String::new()), "{source}");
    }
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_hook_exits_zero_when_index_fails() {
    use std::os::unix::fs::PermissionsExt;
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    let ro = w.base.join("ro");
    fs::create_dir_all(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let r = codex_hook(&w, &ro.join("gopola.db"), "codex", &codex_start(&w, "startup"));
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r, (0, String::new()));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_hook_install_merges_and_is_idempotent() {
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    let other = serde_json::json!({"matcher": "startup", "hooks": [{"type": "command", "command": "demo-tool start"}]});
    let pre = serde_json::json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "demo-guard"}]}]);
    fs::write(&hooks, serde_json::json!({"hooks": {"SessionStart": [other], "PreToolUse": pre}}).to_string()).unwrap();
    let bin = w.base.join("it's bin");
    let bin = bin.to_string_lossy();
    let (ok, out, err) = gopola(&w, &["codex-hook", "--install", &bin]);
    assert!(ok && out.contains(&format!("wrote {}", hooks.display())), "{out}{err}");
    let (ok, out, _) = gopola(&w, &["codex-hook", "--install", &bin]);
    assert!(ok && out.contains(&format!("left {} alone, unchanged", hooks.display())), "{out}");
    let installed = || -> (serde_json::Value, Vec<serde_json::Value>) {
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
        let ours = v["hooks"]["SessionStart"].as_array().unwrap().iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().clone())
            .filter(|h| h["command"].as_str().unwrap().contains("gopola codex-hook")).collect();
        (v, ours)
    };
    let (v, ours) = installed();
    assert_eq!(v["hooks"]["SessionStart"][0], other, "{v}");
    assert_eq!(v["hooks"]["PreToolUse"], pre, "{v}");
    let quoted = format!("'{}'/gopola codex-hook", bin.replace('\'', r"'\''"));
    assert_eq!(ours, [serde_json::json!({"type": "command", "command": quoted, "timeout": 10})], "{v}");
    // Installed from another bin, the one handler names the new one.
    assert!(gopola(&w, &["codex-hook", "--install", "/demo/bin"]).0);
    let (v, ours) = installed();
    assert_eq!(ours.len(), 1, "{v}");
    assert_eq!(ours[0]["command"], "'/demo/bin'/gopola codex-hook");
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2, "{v}");

    fs::write(&hooks, "{not json").unwrap();
    let (ok, _, err) = gopola(&w, &["codex-hook", "--install", &bin]);
    assert!(!ok && err.contains("hooks.json"), "{err}");
    assert_eq!(fs::read_to_string(&hooks).unwrap(), "{not json");
    fs::remove_dir_all(&w.base).unwrap();
}

/// No `codex` ancestor found: the hook cannot tell, and gives the list.
#[test]
fn codex_hook_gives_the_list_with_no_codex_ancestor() {
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    assert!(codex_hook(&w, &w.db, "", &codex_start(&w, "startup")).1.contains("# gopola:"));
    fs::remove_dir_all(&w.base).unwrap();
}

/// A `hooks` that is not an object, or a `SessionStart` that is not a list, is left alone (exit 1).
#[test]
fn codex_hook_install_refuses_a_hooks_json_of_another_shape() {
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    for text in [r#"{"hooks":{"SessionStart":{}}}"#, r#"{"hooks":[]}"#] {
        fs::write(&hooks, text).unwrap();
        let (code, _, err) = gopola_with(&w, NO_OLLAMA, MODEL, &["codex-hook", "--install", "/demo/bin"]);
        assert_eq!(code, 1, "{text}: {err}");
        assert!(err.contains("has no hooks.SessionStart list"), "{text}: {err}");
        assert_eq!(fs::read_to_string(&hooks).unwrap(), text);
    }
    fs::remove_dir_all(&w.base).unwrap();
}

/// Codex runs the handler from each session's cwd, so a relative bin is written as an absolute one.
#[test]
fn codex_hook_install_makes_the_bin_absolute() {
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    assert!(gopola(&w, &["codex-hook", "--install", "./bin"]).0);
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    let command = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    let bin = Path::new(command.strip_prefix('\'').and_then(|c| c.strip_suffix("'/gopola codex-hook")).unwrap());
    assert!(bin.is_absolute() && bin.ends_with("bin"), "{command}");
    assert_eq!(fs::canonicalize(bin.parent().unwrap()).unwrap(), fs::canonicalize(&w.repo).unwrap(), "{command}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_hook_install_keeps_a_hooks_json_symlink() {
    use std::os::unix::fs::symlink;
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    let target = w.base.join("codex/real-hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    fs::write(&target, r#"{"hooks":{"SessionStart":[]}}"#).unwrap();
    symlink(&target, &hooks).unwrap();

    let (ok, _, err) = gopola(&w, &["codex-hook", "--install", "/demo/bin"]);
    assert!(ok, "{err}");
    assert!(fs::symlink_metadata(&hooks).unwrap().file_type().is_symlink());
    assert!(fs::read_to_string(&target).unwrap().contains("gopola codex-hook"));
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn codex_hook_install_writes_through_a_link_to_a_missing_file() {
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    fs::create_dir_all(w.base.join("codex/dotfiles")).unwrap();
    std::os::unix::fs::symlink("dotfiles/hooks.json", &hooks).unwrap();

    let (ok, _, err) = gopola(&w, &["codex-hook", "--install", "/demo/bin"]);
    assert!(ok, "{err}");
    assert!(fs::symlink_metadata(&hooks).unwrap().file_type().is_symlink());
    assert!(fs::read_to_string(w.base.join("codex/dotfiles/hooks.json")).unwrap().contains("gopola codex-hook"));
    fs::remove_dir_all(&w.base).unwrap();
}

/// 0.18.3's Opus review: a link into a missing folder failed with only 'No such file or directory (os error 2)'.
#[test]
fn codex_hook_install_names_the_path_it_cannot_write() {
    let w = setup();
    let hooks = w.base.join("codex/hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("nodir/hooks.json", &hooks).unwrap();

    let (ok, _, err) = gopola(&w, &["codex-hook", "--install", "/demo/bin"]);
    assert!(!ok && err.contains("codex/nodir/hooks.json: No such file or directory"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// Read it all, as Copilot's hook does: a hook that quits before reading its stdin breaks the writer.
#[test]
fn codex_hook_reads_all_of_stdin() {
    use std::io::Write;
    let w = setup();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gopola"))
        .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
        .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
        .env("GOPOLA_CODEX_PARENT_ARGS", "codex exec say hi").current_dir(&w.repo).arg("codex-hook")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut input = codex_start(&w, "startup");
    input["model"] = "x".repeat(300_000).into();
    let written = child.stdin.take().unwrap().write_all(input.to_string().as_bytes());
    let out = child.wait_with_output().unwrap();
    assert!(written.is_ok() && out.status.success() && out.stdout.is_empty(), "{written:?} {out:?}");
    fs::remove_dir_all(&w.base).unwrap();
}

/// The real lookup: a link to `sh` named `codex` runs a script file named after its first argument, so `ps` shows
/// `…/codex exec` (or `…/codex resume`) above the hook.
#[test]
fn codex_hook_reads_its_parent_through_ps() {
    use std::io::Write;
    let w = setup();
    write_session(&w, "0123abcd-0000-4000-8000-000000000001", "2026-03-01T10:00:00.000Z", &[("fix the build", "Fixed.")]);
    let codex = w.base.join("bin/codex");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sh", &codex).unwrap();
    let scripts = w.base.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    for name in ["exec", "resume"] {
        fs::write(scripts.join(name), format!("{} codex-hook\ntrue\n", env!("CARGO_BIN_EXE_gopola"))).unwrap();
    }
    let run = |sub: &str| {
        let mut child = Command::new(&codex).arg(sub)
            .env("GOPOLA_DB", &w.db).env("GOPOLA_PROJECTS", &w.projects).env("GOPOLA_OLLAMA", NO_OLLAMA)
            .env("CLAUDE_CONFIG_DIR", w.base.join("claude")).env("COPILOT_HOME", w.base.join("copilot")).env("CODEX_HOME", w.base.join("codex"))
            .env_remove("GOPOLA_CODEX_PARENT_ARGS").current_dir(&scripts)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(codex_start(&w, "startup").to_string().as_bytes()).unwrap();
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
    };
    assert!(run("resume").contains("# gopola: demo-app"));
    assert_eq!(run("exec"), "");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn weight_reads_a_copilot_transcript_path() {
    let w = setup();
    let f = copilot_dir(&w, CP, "scripted-1.0.107.jsonl").join("events.jsonl");
    let (ok, out, err) = gopola(&w, &["weight", &f.to_string_lossy(), "--json"]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["id"], CP);
    fs::remove_dir_all(&w.base).unwrap();
}

/// The typed fixture plus one instruction update: the three things a Copilot session's model was told.
fn told_session(w: &World) {
    let d = copilot_dir(w, CP, "typed-1.0.107.jsonl");
    let f = d.join("events.jsonl");
    let mut text = fs::read_to_string(&f).unwrap();
    text.push_str(r#"{"type":"instruction.update","data":{"name":"house-rules","authority":"user","content":"Read docs/board.md first.\nThen run tools."},"id":"iu1","timestamp":"2026-11-07T09:00:30.000Z","parentId":null}"#);
    text.push('\n');
    fs::write(&f, text).unwrap();
    assert!(gopola(w, &["index"]).0);
}

#[test]
fn context_shows_what_a_copilot_session_was_told() {
    let w = setup();
    told_session(&w);
    let (ok, out, err) = gopola(&w, &["context", "c1000000"]);
    assert!(ok, "{err}");
    assert!(out.contains("hook sessionStart") && out.contains("demo context"), "{out}");
    assert!(out.contains("system system") && out.contains("You are a demo assistant."), "{out}");
    assert!(out.contains("instruction house-rules") && out.contains("Read docs/board.md first."), "{out}");
    let (_, json, _) = gopola(&w, &["context", "c1000000", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let kinds: Vec<&str> = v["parts"].as_array().unwrap().iter().map(|p| p["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["hook", "system", "instruction"], "session order: {json}");
    assert_eq!(v["parts"][1]["text"], "You are a demo assistant.", "without --grep each part carries its text");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn context_grep_counts_matches_per_part() {
    let w = setup();
    told_session(&w);
    let (ok, out, _) = gopola(&w, &["context", "c1000000", "--grep", "docs/board.md", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["matches"], 1, "{out}");
    let inst = v["parts"].as_array().unwrap().iter().find(|p| p["kind"] == "instruction").unwrap();
    assert_eq!(inst["matches"], 1);
    assert_eq!(inst["lines"], serde_json::json!(["Read docs/board.md first."]));
    assert!(inst.get("text").is_none(), "--grep shows matching lines, not the whole text: {out}");
    let (_, none, _) = gopola(&w, &["context", "c1000000", "--grep", "CLAUDE.md"]);
    assert!(none.contains("0 matches"), "{none}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn context_reads_past_a_line_that_is_not_utf8() {
    let w = setup();
    told_session(&w);
    let f = w.base.join("copilot/session-state").join(CP).join("events.jsonl");
    let text = fs::read(&f).unwrap();
    let cut = text.windows(22).position(|s| s == b"{\"type\":\"instruction.u").unwrap();
    let mut bytes = text[..cut].to_vec();
    bytes.extend_from_slice(b"\xff\xfe not json\n");
    bytes.extend_from_slice(&text[cut..]);
    fs::write(&f, bytes).unwrap();
    let (ok, out, _) = gopola(&w, &["context", "c1000000", "--grep", "docs/board.md"]);
    assert!(ok && out.contains("1 match for 'docs/board.md'"), "{out}");
    fs::remove_dir_all(&w.base).unwrap();
}

#[test]
fn context_refuses_a_claude_session() {
    let w = setup();
    let cwd = w.repo.to_string_lossy().to_string();
    fs::write(transcript_dir(&w).join("0123abcd-0000-4000-8000-000000000001.jsonl"),
        prompt("2026-11-07T09:00:00Z", "hello", &cwd) + "\n").unwrap();
    assert!(gopola(&w, &["index"]).0);
    let (ok, out, err) = gopola(&w, &["context", "0123abcd"]);
    assert!(!ok && out.is_empty(), "{out}");
    assert!(err.contains("context reads Copilot sessions only; 0123abcd is a Claude Code session, whose transcript holds no system prompt"), "{err}");
    fs::remove_dir_all(&w.base).unwrap();
}
