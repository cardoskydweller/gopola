//! What `ask` reads: the text behind each search hit, split into the sentences that can be offered as an answer.

use crate::cmds::short_id;
use crate::embed::Hit;
use rusqlite::{Connection, OptionalExtension, params};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use crate::cmds::{Opts, Res};
#[cfg(target_os = "macos")]
use crate::rerank::Scorer;

/// The reranker model, pinned to one Hugging Face commit: `ask --setup` downloads exactly these bytes.
pub const MODEL_FILE: &str = "qwen3-reranker-0.6b-q8_0.gguf";
pub const MODEL_SIZE: u64 = 639_153_184;
pub const MODEL_SHA256: &str = "22c9979ce4fbcdc5acdc310c6641c32797eff1aa980b8f7a2db8a8ea23429a48";
pub const MODEL_URL: &str = "https://huggingface.co/ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF/resolve/a02f48bb4f057028298c21fa033da2b30d7742d5/qwen3-reranker-0.6b-q8_0.gguf";

/// Beside the database, so `GOPOLA_DB` moves the model with it.
pub fn model_path() -> PathBuf {
    crate::db::db_path().with_file_name(MODEL_FILE)
}

/// Whether `path` can be the model: present, the pinned size, a GGUF version 3 header. llama.cpp trusts a file's
/// header counts and can abort the process on a bad one, so every caller checks here before it loads the file.
pub fn check_model_file(path: &Path) -> Result<(), String> {
    let shown = path.display();
    let again = "run `gopola ask --setup` again";
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound =>
            return Err("the reranker model is not set up: run `gopola ask --setup` (downloads 639 MB from Hugging Face)".into()),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    if size != MODEL_SIZE {
        return Err(format!("{shown} is {size} bytes, not {MODEL_SIZE}: {again}"));
    }
    let mut header = [0u8; 8];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut header)).map_err(|e| format!("{shown}: {e}"))?;
    if header != *b"GGUF\x03\0\0\0" {
        return Err(format!("{shown} is not a GGUF version 3 file: {again}"));
    }
    Ok(())
}

/// The sha256 of a file, as `/usr/bin/shasum` prints it.
#[cfg(target_os = "macos")]
fn sha256_of(path: &Path) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/shasum").args(["-a", "256"]).arg(path).output()
        .map_err(|e| format!("could not run /usr/bin/shasum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next() {
        Some(h) if out.status.success() && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(h.to_ascii_lowercase()),
        _ => Err(format!("/usr/bin/shasum gave no sha256 for {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim())),
    }
}

/// Moves a finished download to `dest` when it has the pinned size and hash; otherwise removes it and installs nothing.
#[cfg(target_os = "macos")]
fn verify_and_place(part: &Path, dest: &Path, size: u64, sha256: &str) -> Result<(), String> {
    let refuse = |why: String| { let _ = std::fs::remove_file(part); Err(format!("{why}; nothing was installed")) };
    let got = std::fs::metadata(part).map_err(|e| format!("{}: {e}", part.display()))?.len();
    if got != size {
        return refuse(format!("{} is {got} bytes, not {size}", part.display()));
    }
    let hash = match sha256_of(part) {
        Ok(hash) => hash,
        Err(e) => return refuse(e),
    };
    if hash != sha256 {
        return refuse(format!("{} has sha256 {hash}, not {sha256}", part.display()));
    }
    std::fs::rename(part, dest).or_else(|e| refuse(format!("could not move {} to {}: {e}", part.display(), dest.display())))
}

/// `ask --setup`: downloads the pinned model beside the database, and checks size and sha256 before it is placed.
#[cfg(target_os = "macos")]
pub fn setup(o: &Opts) -> Res {
    let path = model_path();
    let report = |downloaded: bool| if o.json {
        println!("{}", serde_json::json!({"model": path.display().to_string(), "bytes": MODEL_SIZE, "sha256": MODEL_SHA256, "downloaded": downloaded}));
    };
    if check_model_file(&path).is_ok() && sha256_of(&path)? == MODEL_SHA256 {
        report(false);
        if !o.json { println!("reranker already set up: {}", path.display()) }
        return Ok(());
    }
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)? }
    let mut part = path.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let status = std::process::Command::new("/usr/bin/curl")
        .args(["-q", "--fail", "--location", "--show-error", "--progress-bar", "--proto", "=https", "--proto-redir", "=https",
            "--max-redirs", "5", "--max-filesize", &MODEL_SIZE.to_string(), "--output"])
        .arg(&part).arg(MODEL_URL)
        .stdin(std::process::Stdio::null())
        .status().map_err(|e| format!("could not run /usr/bin/curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(format!("curl exited {}; nothing was installed", status.code().map_or("by a signal".into(), |c| c.to_string())).into());
    }
    verify_and_place(&part, &path, MODEL_SIZE, MODEL_SHA256)?;
    report(true);
    if !o.json { println!("reranker ready: {} (639 MB, sha256 {}…)", path.display(), &MODEL_SHA256[..8]) }
    Ok(())
}

/// How many search hits `ask` reads.
pub const HITS: usize = 8;
/// The most characters of a turn's prompt, and of its reply, that go to the reranker.
pub const CLIP: usize = 600;
/// The most sentences taken from one document.
pub const MAX_SENTENCES: usize = 300;

/// One hit's text. A turn hit carries its session and turn number and the prompt and reply behind it; any other hit
/// (a commit, a summary) is its snippet.
pub struct Doc {
    pub label: String,
    pub turn: Option<(String, i64)>,
    pub text: String,
    /// A turn's prompt, a space and its reply, whole; empty for any other hit.
    pub full: String,
}

pub fn docs(c: &Connection, rows: &[(Hit, &'static str)]) -> rusqlite::Result<Vec<Doc>> {
    rows.iter().map(|((kind, r, _, _, snippet), _)| {
        let turn = match (kind.as_str(), r.split_once(':')) {
            ("prompt" | "reply" | "command" | "turn", Some((id, n))) => n.parse::<i64>().ok().map(|n| (id.to_string(), n)),
            _ => None,
        };
        let row = match &turn {
            Some((id, n)) => c.query_row("select prompt, reply from turns where session_id = ?1 and n = ?2", params![id, n],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default()))).optional()?,
            None => None,
        };
        Ok(match (turn, row) {
            (Some((id, n)), Some((prompt, reply))) =>
                Doc { label: format!("{}:{n}", short_id(&id)), text: document(&prompt, &reply), full: format!("{prompt} {reply}"), turn: Some((id, n)) },
            _ => Doc { label: format!("{kind}-{}", r.chars().take(12).collect::<String>()), turn: None, text: snippet.clone(), full: String::new() },
        })
    }).collect()
}

/// A turn as the reranker reads it: the prompt and the reply, each cut to `CLIP` characters.
pub fn document(prompt: &str, reply: &str) -> String {
    let cut = |s: &str| s.chars().take(CLIP).collect::<String>();
    format!("User: {}\nAssistant: {}", cut(prompt), cut(reply))
}

/// Python's `str.isspace`: Unicode white space plus U+001C to U+001F.
fn space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `re.split(r"(?<=[.!?])\s+|\n+", text)` with the parts trimmed and the empty ones dropped, at most `MAX_SENTENCES`.
/// The research scripts split this way; the `regex` crate has no look-behind.
pub fn sentences(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut skip_to, mut prev) = (0, 0, ' ');
    for (i, c) in text.char_indices() {
        if i >= skip_to {
            let run: Option<fn(char) -> bool> =
                if space(c) && ".!?".contains(prev) { Some(space) } else if c == '\n' { Some(|c| c == '\n') } else { None };
            if let Some(in_run) = run {
                skip_to = text[i..].find(|c| !in_run(c)).map_or(text.len(), |k| i + k);
                parts.push(&text[start..i]);
                start = skip_to;
            }
        }
        prev = c;
    }
    parts.push(&text[start..]);
    parts.into_iter().map(|p| p.trim_matches(space)).filter(|p| !p.is_empty()).take(MAX_SENTENCES).collect()
}

/// Which sentences of a document are offered to the reranker as an answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rule {
    /// 12 to 300 characters, and no double quote.
    QuoteFilter,
    /// 12 to 300 characters.
    Length,
    /// Every sentence.
    All,
}

pub const RULES: [Rule; 3] = [Rule::QuoteFilter, Rule::Length, Rule::All];

pub fn offered(s: &str, rule: Rule) -> bool {
    let long_enough = (12..=300).contains(&s.chars().count());
    match rule {
        Rule::QuoteFilter => long_enough && !s.contains(['"', '“', '”']),
        Rule::Length => long_enough,
        Rule::All => true,
    }
}

/// What `ask` does when the best hit is not a turn (a commit, a summary).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NonTurn {
    /// The question has no answer in the history.
    NotFound,
    /// The best-scoring turn answers it.
    BestTurn,
}

/// The score a candidate turn must reach to answer: the cut that maximised right answers minus wrong citations on
/// the eval set, because a wrong citation costs more than not found.
pub const THRESHOLD: f32 = 0.99659204;
/// Offering every sentence scored 18 of 31 right, the same as the length filter and above the quote filter (13 of
/// 31); with a tie, the rule with no filter wins.
pub const RULE: Rule = Rule::All;
/// "Not found" when the best hit is not a turn: it left 42 good outcomes minus wrong first citations on the eval
/// set, against 39 for answering with the best turn.
pub const NON_TURN: NonTurn = NonTurn::NotFound;

/// What the reranker made of the hits. A `None` score is a text over one batch, which is not scored.
pub struct Ranked {
    /// One per doc, in doc order.
    pub scores: Vec<Option<f32>>,
    /// The best-scoring doc, and the best-scoring turn: the same doc whenever the top hit is a turn.
    pub top: Option<usize>,
    pub best_turn: Option<usize>,
    /// The sentences of `best_turn`'s whole text, each with its score.
    pub sentences: Vec<(String, Option<f32>)>,
    pub rerank_secs: f64,
}

/// The index of the highest score, the first of equals; unscored entries never win.
fn argmax(scores: impl Iterator<Item = Option<f32>>) -> Option<usize> {
    scores.enumerate().filter_map(|(i, s)| s.map(|s| (i, s)))
        .fold(None, |best, (i, s)| match best { Some((_, b)) if s <= b => best, _ => Some((i, s)) })
        .map(|(i, _)| i)
}

/// The best-scoring doc and the best-scoring turn.
fn tops(scores: &[Option<f32>], docs: &[Doc]) -> (Option<usize>, Option<usize>) {
    (argmax(scores.iter().copied()),
     argmax(docs.iter().zip(scores).map(|(d, s)| if d.turn.is_some() { *s } else { None })))
}

/// Scores each hit's text, then the sentences of the best turn.
#[cfg(target_os = "macos")]
pub fn rank(s: &mut Scorer, q: &str, docs: &[Doc]) -> Result<Ranked, String> {
    let start = std::time::Instant::now();
    let scores = docs.iter().map(|d| s.score(q, &d.text)).collect::<Result<Vec<_>, _>>()?;
    let (top, best_turn) = tops(&scores, docs);
    let sentences = match best_turn {
        Some(i) => sentences(&docs[i].full).into_iter().map(|t| Ok((t.to_string(), s.score(q, t)?))).collect::<Result<_, String>>()?,
        None => Vec::new(),
    };
    Ok(Ranked { scores, top, best_turn, sentences, rerank_secs: start.elapsed().as_secs_f64() })
}

/// The turn that may answer: the top hit when it is a turn, else what the non-turn rule says.
pub fn candidate(r: &Ranked, rule: NonTurn) -> Option<usize> {
    if r.top == r.best_turn { r.top } else if rule == NonTurn::BestTurn { r.best_turn } else { None }
}

/// The candidate, when its score reaches the threshold.
pub fn answer_index(r: &Ranked, rule: NonTurn, threshold: f32) -> Option<usize> {
    candidate(r, rule).filter(|&i| r.scores[i].is_some_and(|s| s >= threshold))
}

/// The best-scoring sentence the rule offers.
pub fn best_sentence(r: &Ranked, rule: Rule) -> Option<&str> {
    argmax(r.sentences.iter().map(|(t, s)| if offered(t, rule) { *s } else { None })).map(|i| r.sentences[i].0.as_str())
}

/// What `ask` prints: the cited turn as `(ref, session, turn)` with the key sentence, or "not found" when there is
/// none. Everything that came from a transcript goes through `printable`, so the output holds no control byte.
pub fn render(answer: Option<(&str, &str, i64)>, sentence: Option<&str>, score: Option<f32>, json: bool) -> String {
    use crate::transcript::printable;
    let sentence = sentence.map(printable);
    match (answer, json) {
        (Some((label, session, n)), true) => serde_json::json!({"found": true, "ref": printable(label), "session": printable(session),
            "turn": n, "sentence": sentence, "score": score}).to_string(),
        (None, true) => serde_json::json!({"found": false, "ref": null, "session": null, "turn": null, "sentence": null, "score": score}).to_string(),
        (Some((label, ..)), false) => match sentence {
            Some(s) => format!("{}  {s}", printable(label)),
            None => printable(label),
        },
        (None, false) => "not found".into(),
    }
}

/// `ask <question>`: the turn that answers it and its key sentence, or "not found".
#[cfg(target_os = "macos")]
pub fn ask(o: &Opts, q: &str) -> Res {
    let model = model_path();
    check_model_file(&model)?;
    let reranker = crate::rerank::load(&model)?;
    let mut scorer = reranker.scorer()?;
    if !scorer.fits(q) {
        return Err("the question is too long to score".into());
    }
    let c = crate::db::open()?;
    let found = crate::cmds::search_rows(&c, &Opts { n: HITS, ..Default::default() }, q, o.project.as_deref().unwrap_or("all"),
        o.since.as_deref().unwrap_or(""), o.until.as_deref().unwrap_or("9999"))?;
    if let Some(note) = &found.note {
        eprintln!("{note}");
    }
    let docs = docs(&c, &found.rows)?;
    let r = rank(&mut scorer, q, &docs)?;
    let answer = answer_index(&r, NON_TURN, THRESHOLD).and_then(|i| docs[i].turn.as_ref().map(|t| (i, t)));
    let score = answer.map(|(i, _)| i).or(r.top).and_then(|i| r.scores[i]);
    let cited = answer.map(|(i, (session, n))| (docs[i].label.as_str(), session.as_str(), *n));
    println!("{}", render(cited, best_sentence(&r, RULE), score, o.json));
    crate::db::log_usage(&c, "ask", q, answer.is_some() as i64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_split_as_the_research_scripts_split() {
        assert_eq!(sentences("Done. Next step?  Yes!\nok"), ["Done.", "Next step?", "Yes!", "ok"]);
        assert_eq!(sentences("a\n\n  b. c"), ["a", "b.", "c"]);
        assert_eq!(sentences("v1.2 is out.\u{a0}Then"), ["v1.2 is out.", "Then"]);
        assert_eq!(sentences("line one\nline two. \n\n three"), ["line one", "line two.", "three"]);
        assert_eq!(sentences("x.\u{1c}y"), ["x.", "y"]); // Python counts U+001C as whitespace
        assert_eq!(sentences("say \"hi\". e.g. this"), ["say \"hi\".", "e.g.", "this"]);
    }

    #[test]
    fn only_the_first_300_sentences_are_kept() {
        let text = "s.\n".repeat(400);
        let s = sentences(&text);
        assert_eq!(s.len(), MAX_SENTENCES);
        assert_eq!(s[0], "s.");
    }

    #[test]
    fn offered_follows_each_rule() {
        let rules = |s: &str| RULES.map(|r| offered(s, r));
        assert_eq!(rules("Yes."), [false, false, true]);
        assert_eq!(rules("say \"hi\" now"), [false, true, true]); // 12 chars, one quote
        assert_eq!(rules(&"a".repeat(301)), [false, false, true]);
        assert_eq!(rules("a plain sentence of exactly forty chars."), [true, true, true]);
    }

    #[test]
    fn document_cuts_on_chars() {
        let d = document(&"é".repeat(601), &"ü".repeat(601));
        assert_eq!(d, format!("User: {}\nAssistant: {}", "é".repeat(600), "ü".repeat(600)));
    }

    #[test]
    fn docs_label_turns_and_keep_other_hits_as_snippets() {
        let c = Connection::open_in_memory().unwrap();
        c.execute("create table turns (session_id text, n integer, prompt text, reply text)", []).unwrap();
        c.execute("insert into turns values ('aaaaaaaa-0000-0000-0000-000000000001', 3, 'the prompt', NULL)", []).unwrap();
        let hit = |kind: &str, r: &str, snippet: &str| ((kind.into(), r.into(), "p".into(), "2026-01-01".into(), snippet.into()), "words");
        let rows = [
            hit("reply", "aaaaaaaa-0000-0000-0000-000000000001:00000003", "the [prompt]"),
            hit("commit", "0123456789abcdef", "fix [cookie]…"),
            hit("turn", "bbbbbbbb-0000-0000-0000-000000000002:00000001", "gone"),
            hit("observation", "42", "a note"),
        ];
        let d = docs(&c, &rows).unwrap();
        assert_eq!(d.iter().map(|d| d.label.as_str()).collect::<Vec<_>>(),
            ["aaaaaaaa:3", "commit-0123456789ab", "turn-bbbbbbbb-000", "observation-42"]);
        assert_eq!(d[0].turn, Some(("aaaaaaaa-0000-0000-0000-000000000001".into(), 3)));
        assert_eq!(d[0].text, "User: the prompt\nAssistant: ");
        assert_eq!(d[0].full, "the prompt ");
        assert_eq!((d[1].turn.clone(), d[1].text.as_str(), d[1].full.as_str()), (None, "fix [cookie]…", ""));
        assert_eq!((d[2].turn.clone(), d[2].text.as_str()), (None, "gone"));
    }

    fn doc(turn: bool) -> Doc {
        Doc { label: String::new(), turn: turn.then(|| ("s".into(), 1)), text: String::new(), full: String::new() }
    }

    fn ranked(scores: &[Option<f32>], top: Option<usize>, best_turn: Option<usize>) -> Ranked {
        Ranked { scores: scores.to_vec(), top, best_turn, sentences: Vec::new(), rerank_secs: 0.0 }
    }

    #[test]
    fn a_non_turn_top_follows_the_non_turn_rule() {
        // A commit scores 0.99 and the best turn 0.97.
        let r = ranked(&[Some(0.99), Some(0.97)], Some(0), Some(1));
        assert_eq!(candidate(&r, NonTurn::NotFound), None);
        assert_eq!(candidate(&r, NonTurn::BestTurn), Some(1));
        assert_eq!(answer_index(&r, NonTurn::NotFound, 0.95), None);
        assert_eq!(answer_index(&r, NonTurn::BestTurn, 0.95), Some(1));
        assert_eq!(answer_index(&r, NonTurn::BestTurn, 0.98), None);
        // A turn on top is the candidate under both rules.
        let r = ranked(&[Some(0.97), Some(0.99)], Some(1), Some(1));
        assert_eq!([NonTurn::NotFound, NonTurn::BestTurn].map(|n| candidate(&r, n)), [Some(1), Some(1)]);
        // No hit scored: nothing to answer with.
        let r = ranked(&[None], None, None);
        assert_eq!([NonTurn::NotFound, NonTurn::BestTurn].map(|n| answer_index(&r, n, 0.0)), [None, None]);
    }

    #[test]
    fn a_top_score_under_the_threshold_is_not_found() {
        let r = ranked(&[Some(0.94)], Some(0), Some(0));
        assert_eq!(answer_index(&r, NonTurn::NotFound, 0.95), None);
        let r = ranked(&[Some(0.95)], Some(0), Some(0));
        assert_eq!(answer_index(&r, NonTurn::NotFound, 0.95), Some(0), "a score equal to the threshold answers");
    }

    #[test]
    fn ties_go_to_the_first() {
        let docs = [doc(false), doc(true), doc(true)];
        assert_eq!(tops(&[Some(0.5), Some(0.5), Some(0.5)], &docs), (Some(0), Some(1)));
        assert_eq!(tops(&[Some(0.4), Some(0.9), Some(0.9)], &docs), (Some(1), Some(1)));
        assert_eq!(tops(&[None, None, None], &docs), (None, None));
        let mut r = ranked(&[], None, None);
        r.sentences = vec![("First one here.".into(), Some(0.8)), ("Second one here.".into(), Some(0.8))];
        assert_eq!(best_sentence(&r, Rule::All), Some("First one here."));
    }

    #[test]
    fn best_sentence_skips_unscored_and_unoffered() {
        let mut r = ranked(&[], None, None);
        r.sentences = vec![
            ("Too long to score at all.".into(), None),
            ("ok".into(), Some(0.99)),
            ("A sentence with a \"quote\" in it.".into(), Some(0.98)),
            ("A plain sentence is offered.".into(), Some(0.5)),
        ];
        assert_eq!(best_sentence(&r, Rule::All), Some("ok"));
        assert_eq!(best_sentence(&r, Rule::Length), Some("A sentence with a \"quote\" in it."));
        assert_eq!(best_sentence(&r, Rule::QuoteFilter), Some("A plain sentence is offered."));
        r.sentences.truncate(1);
        assert_eq!(best_sentence(&r, Rule::All), None);
    }

    #[test]
    fn render_prints_each_form() {
        let cited = Some(("cccccccc:1", "cccccccc-0000-0000-0000-000000000003", 1));
        assert_eq!(render(cited, Some("Use port 8095."), Some(0.999), false), "cccccccc:1  Use port 8095.");
        assert_eq!(render(cited, None, Some(0.999), false), "cccccccc:1");
        assert_eq!(render(None, Some("Use port 8095."), Some(0.5), false), "not found");
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(v(render(cited, Some("Use port 8095."), Some(0.5), true)), serde_json::json!({"found": true, "ref": "cccccccc:1",
            "session": "cccccccc-0000-0000-0000-000000000003", "turn": 1, "sentence": "Use port 8095.", "score": 0.5}));
        assert_eq!(v(render(None, Some("Use port 8095."), None, true)), serde_json::json!({"found": false, "ref": null,
            "session": null, "turn": null, "sentence": null, "score": null}));
    }

    #[test]
    fn render_prints_an_escape_sequence_inert() {
        let cited = Some(("cc\u{1b}[31m:1", "cc\u{202e}cc", 1));
        let sentence = Some("Use port \u{1b}[2J8095\u{7} now.");
        for json in [false, true] {
            let out = render(cited, sentence, Some(0.9), json);
            assert!(!out.chars().any(|c| c.is_control() || c == '\u{202e}'), "json {json}: {out:?}");
            let text = if json { serde_json::from_str::<serde_json::Value>(&out).unwrap()["sentence"].as_str().unwrap().to_string() } else { out };
            assert!(text.contains("Use port \\u{1b}[2J8095\\u{7} now."), "json {json}: {text}");
        }
        assert!(render(cited, sentence, None, false).starts_with("cc\\u{1b}[31m:1  "));
        let json = serde_json::from_str::<serde_json::Value>(&render(cited, sentence, None, true)).unwrap();
        assert_eq!((json["ref"].as_str(), json["session"].as_str()), (Some("cc\\u{1b}[31m:1"), Some("cc\\u{202e}cc")));
    }

    /// A fresh, empty folder for one test.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gopola-ask-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn model_file_checks() {
        let dir = scratch("model-file-checks");
        let again = "run `gopola ask --setup` again";
        let path = dir.join(MODEL_FILE);
        let shown = path.display();

        assert_eq!(check_model_file(&path), Err("the reranker model is not set up: run `gopola ask --setup` (downloads 639 MB from Hugging Face)".into()));

        std::fs::write(&path, b"GGUF").unwrap();
        assert_eq!(check_model_file(&path), Err(format!("{shown} is 4 bytes, not 639153184: {again}")));

        let sparse = std::fs::File::create(&path).unwrap();
        sparse.set_len(MODEL_SIZE).unwrap();
        assert_eq!(check_model_file(&path), Err(format!("{shown} is not a GGUF version 3 file: {again}")));

        std::fs::write(&path, b"GGUF\x03\0\0\0").unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MODEL_SIZE).unwrap();
        assert_eq!(check_model_file(&path), Ok(()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    fn shasum(path: &Path) -> String {
        let out = std::process::Command::new("/usr/bin/shasum").args(["-a", "256"]).arg(path).output().unwrap();
        String::from_utf8(out.stdout).unwrap().split_whitespace().next().unwrap().to_string()
    }

    /// A `.part` file of invented bytes beside a destination, and the hash `shasum` gives the bytes.
    #[cfg(target_os = "macos")]
    fn placement(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, String) {
        let dir = scratch(name);
        let (part, dest) = (dir.join("model.gguf.part"), dir.join("model.gguf"));
        std::fs::write(&part, b"invented model bytes").unwrap();
        let hash = shasum(&part);
        (dir, part, dest, hash)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_good_file_is_placed() {
        let (dir, part, dest, hash) = placement("good-file");
        verify_and_place(&part, &dest, 20, &hash).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"invented model bytes");
        assert!(!part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_wrong_hash_places_nothing() {
        let (dir, part, dest, hash) = placement("wrong-hash");
        let other = "0".repeat(64);
        let e = verify_and_place(&part, &dest, 20, &other).unwrap_err();
        assert!(e.contains(&hash) && e.contains(&other), "{e}");
        assert!(!dest.exists() && !part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_wrong_size_places_nothing() {
        let (dir, part, dest, hash) = placement("wrong-size");
        let e = verify_and_place(&part, &dest, 21, &hash).unwrap_err();
        assert!(e.contains("is 20 bytes, not 21"), "{e}");
        assert!(!dest.exists() && !part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_shasum_places_nothing_and_leaves_no_part() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, part, dest, hash) = placement("shasum-fails");
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o000)).unwrap(); // shasum cannot read it
        let e = verify_and_place(&part, &dest, 20, &hash).unwrap_err();
        assert!(e.contains("gave no sha256") && e.ends_with("; nothing was installed"), "{e}");
        assert!(!dest.exists() && !part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_move_leaves_no_part() {
        let (dir, part, _, hash) = placement("move-fails");
        let dest = dir.join("no-such-folder").join("model.gguf");
        let e = verify_and_place(&part, &dest, 20, &hash).unwrap_err();
        assert!(e.contains("could not move") && e.ends_with("; nothing was installed"), "{e}");
        assert!(!dest.exists() && !part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
