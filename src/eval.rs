//! `gopola eval ask`: scores `ask` on an eval folder (`ask.jsonl`, and on the owner's Mac `local/ask-client.jsonl`) with the
//! scorers the research used, in gopola. It prints refs, ids and numbers, never a turn's words. `eval friction` is
//! `friction::eval`.

use crate::cmds::{Opts, Res};

pub fn eval(o: &Opts, what: Option<&str>) -> Res {
    match what {
        Some("ask") => ask_eval(o),
        Some("friction") => match &o.dir {
            Some(d) => crate::friction::eval(o, std::path::Path::new(d)),
            None => Err("eval friction needs --dir <eval folder>".into()),
        },
        _ => Err("eval needs `ask` or `friction`".into()),
    }
}

/// The scoring itself: reading the eval files, and what a run's scores say once `ask::rank` has made them. Nothing
/// here needs the reranker, so it is tested on every platform with hand-built runs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod ask_score {
    use crate::ask::{self, NonTurn, Ranked};
    use crate::transcript::{MAX_INPUT_BYTES, read_capped};
    use serde_json::{Map, Value, json};
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    pub struct Question {
        pub id: String,
        pub exact: String,
        pub reworded: String,
        pub gold: Vec<String>,
        pub gold_sessions: Vec<String>,
        pub expects_answer: bool,
        pub until: String,
        pub exclude: Vec<String>,
        pub client: bool,
    }

    /// `ask.jsonl`, then `local/ask-client.jsonl` when it is there, whose rows are marked `client`.
    pub fn read_questions(dir: &Path) -> Result<Vec<Question>, String> {
        let mut questions = Vec::new();
        for (file, client) in [("ask.jsonl", false), ("local/ask-client.jsonl", true)] {
            let path = dir.join(file);
            if client && !path.exists() {
                continue;
            }
            let text = read_capped(&path, MAX_INPUT_BYTES)?;
            for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                questions.push(question(line, client).map_err(|e| format!("{file}:{}: {e}", i + 1))?);
            }
        }
        Ok(questions)
    }

    fn question(line: &str, client: bool) -> Result<Question, String> {
        let row: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
        let field = |k: &str| row.get(k).ok_or_else(|| format!("missing field `{k}`"));
        let text = |k: &str| field(k)?.as_str().map(str::to_string).ok_or_else(|| format!("`{k}` is not a string"));
        let list = |k: &str| -> Result<Vec<String>, String> {
            field(k)?.as_array().and_then(|a| a.iter().map(|s| s.as_str().map(str::to_string)).collect())
                .ok_or_else(|| format!("`{k}` is not a list of strings"))
        };
        Ok(Question {
            id: text("id")?, exact: text("exact")?, reworded: text("reworded")?, until: text("until")?,
            gold: list("gold")?, gold_sessions: list("gold_sessions")?,
            expects_answer: match text("expect")?.as_str() {
                "answer" => true,
                "refusal" => false,
                _ => return Err("`expect` is not \"answer\" or \"refusal\"".into()),
            },
            exclude: if row.get("exclude").is_none_or(Value::is_null) { Vec::new() } else { list("exclude")? },
            client,
        })
    }

    /// Question id, then turn label, then the sentences that count as the answer in that turn.
    pub type Gold = HashMap<String, HashMap<String, Vec<String>>>;

    /// `local/ask-sentences.json` (every question) when it is there, else `ask-sentences.json`. A key that holds a list is a
    /// turn label; any other key is a note.
    pub fn read_gold_sentences(dir: &Path) -> Result<Gold, String> {
        let local = dir.join("local/ask-sentences.json");
        let path = if local.exists() { local } else { dir.join("ask-sentences.json") };
        let shown = path.display();
        let text = read_capped(&path, MAX_INPUT_BYTES)?;
        let file: Value = serde_json::from_str(&text).map_err(|e| format!("{shown}: not JSON: {e}"))?;
        let bad = || format!("{shown}: not an object of question ids, each an object of turn labels and lists of sentences");
        file.as_object().ok_or_else(bad)?.iter().map(|(id, labels)| {
            let turns = labels.as_object().ok_or_else(bad)?.iter().filter_map(|(label, s)| Some((label, s.as_array()?)))
                .map(|(label, s)| Ok((label.clone(), s.iter().map(|s| s.as_str().map(str::to_string)).collect::<Option<Vec<_>>>().ok_or_else(bad)?)))
                .collect::<Result<_, String>>()?;
            Ok((id.clone(), turns))
        }).collect()
    }

    /// One phrasing of one question, searched and ranked.
    pub struct Run {
        pub id: String,
        pub phrasing: &'static str,
        pub client: bool,
        pub expects_answer: bool,
        pub gold: Vec<String>,
        pub gold_sessions: Vec<String>,
        /// The label of each hit, in hit order (`ranked.scores` is in the same order).
        pub labels: Vec<String>,
        pub ranked: Ranked,
        /// The best sentence of the best turn under each of `RULES`.
        pub sentence: [Option<String>; 3],
        pub search_secs: f64,
    }

    fn is_gold(r: &Run, label: &str) -> bool {
        r.gold.iter().any(|g| g == label)
    }

    /// The turn `ask` would cite for this run under the non-turn rule and a threshold.
    fn cited(r: &Run, rule: NonTurn, t: f32) -> Option<&str> {
        ask::answer_index(&r.ranked, rule, t).map(|i| r.labels[i].as_str())
    }

    pub fn refuses(r: &Run, rule: NonTurn, t: f32) -> bool {
        cited(r, rule, t).is_none()
    }

    /// A good outcome: a refusal when the question has no answer or its gold turn is not among the hits, else the gold turn.
    pub fn good(r: &Run, rule: NonTurn, t: f32) -> bool {
        if !r.expects_answer {
            return refuses(r, rule, t);
        }
        match cited(r, rule, t) {
            None => !r.labels.iter().any(|l| is_gold(r, l)),
            Some(label) => is_gold(r, label),
        }
    }

    /// A first cite that is not a gold turn (an absent question has none, so any cite is wrong).
    pub fn wrong(r: &Run, rule: NonTurn, t: f32) -> bool {
        cited(r, rule, t).is_some_and(|label| !is_gold(r, label))
    }

    /// Every candidate score of every phrasing, with a threshold that answers all and one that answers none.
    pub fn candidates(runs: &[Run], rule: NonTurn) -> Vec<f32> {
        let mut c: Vec<f32> = runs.iter().filter_map(|r| ask::candidate(&r.ranked, rule).and_then(|i| r.ranked.scores[i])).collect();
        c.extend([f32::NEG_INFINITY, f32::INFINITY]);
        c.sort_by(f32::total_cmp);
        c.dedup();
        c
    }

    /// The candidate that maximises good outcomes minus wrong cites on `train`; the lowest of equals.
    pub fn fit(candidates: &[f32], train: &[&Run], rule: NonTurn) -> f32 {
        let net = |t: f32| train.iter().map(|r| good(r, rule, t) as i64 - wrong(r, rule, t) as i64).sum::<i64>();
        candidates.iter().copied().max_by(|&a, &b| net(a).cmp(&net(b)).then(b.total_cmp(&a))).unwrap_or(f32::NEG_INFINITY)
    }

    /// Each question's threshold, fit on every other question (both phrasings of the one scored are left out), from the
    /// candidates of all phrasings.
    pub fn thresholds_loo(runs: &[Run], rule: NonTurn) -> HashMap<String, f32> {
        let candidates = candidates(runs, rule);
        let ids: BTreeSet<&String> = runs.iter().map(|r| &r.id).collect();
        ids.into_iter().map(|id| {
            let train: Vec<&Run> = runs.iter().filter(|r| &r.id != id).collect();
            (id.clone(), fit(&candidates, &train, rule))
        }).collect()
    }

    /// (hits, total) of the sentence shown under `RULES[k]`: over phrasings of answer questions with gold sentences whose candidate turn,
    /// whatever its score, is a gold turn with a listed sentence.
    pub fn sentence_hits(runs: &[&Run], rule: NonTurn, k: usize, gold: &Gold) -> (usize, usize) {
        let (mut hit, mut total) = (0, 0);
        for r in runs.iter().filter(|r| r.expects_answer) {
            let Some(label) = ask::candidate(&r.ranked, rule).map(|i| &r.labels[i]) else { continue };
            let listed = gold.get(&r.id).and_then(|turns| turns.get(label)).filter(|s| !s.is_empty());
            if let Some(listed) = listed.filter(|_| is_gold(r, label)) {
                total += 1;
                hit += r.sentence[k].as_ref().is_some_and(|s| listed.contains(s)) as usize;
            }
        }
        (hit, total)
    }

    const SENTENCE_RULES: [&str; 3] = ["quote_filter", "length", "all"];

    /// The measures of one set of runs under one non-turn rule, each run judged at its own question's threshold.
    pub fn summary(runs: &[&Run], rule: NonTurn, thr: &HashMap<String, f32>, gold: &Gold) -> Value {
        let at = |r: &Run| thr[&r.id];
        let (answers, absent): (Vec<&Run>, Vec<&Run>) = runs.iter().copied().partition(|r| r.expects_answer);
        let count = |of: &[&Run], keep: &dyn Fn(&Run) -> bool| of.iter().filter(|r| keep(r)).count();
        let right = count(&answers, &|r| cited(r, rule, at(r)).is_some_and(|l| is_gold(r, l)));
        let session = count(&answers, &|r| cited(r, rule, at(r))
            .is_some_and(|l| r.gold_sessions.iter().any(|s| l.split(':').next() == Some(s.as_str()))));
        let wrong_cites = count(runs, &|r| wrong(r, rule, at(r)));
        let good_outcomes = count(runs, &|r| good(r, rule, at(r)));
        let invented = count(runs, &|r| cited(r, rule, at(r)).is_some_and(|l| !r.labels.iter().any(|x| x == l)));
        let sentence: Map<String, Value> = SENTENCE_RULES.iter().enumerate().map(|(k, name)| {
            let (hit, total) = sentence_hits(runs, rule, k, gold);
            (name.to_string(), json!(format!("{hit}/{total}")))
        }).collect();
        json!({
            "right_first_turn": format!("{right}/{}", answers.len()),
            "top1_session": format!("{session}/{}", answers.len()),
            "wrong_first_cites": wrong_cites,
            "absent_cited": format!("{}/{}", count(&absent, &|r| cited(r, rule, at(r)).is_some()), absent.len()),
            "good": format!("{good_outcomes}/{}", runs.len()),
            "good_minus_wrong": good_outcomes as i64 - wrong_cites as i64,
            "invented_refs": invented,
            "sentence": sentence,
        })
    }

    /// A threshold as JSON: the exact number (rounded, it could land above the score it was fitted on), or "inf" and "-inf"
    /// for the two that answer none and all.
    fn num(t: f32) -> Value {
        if t.is_finite() { json!(f64::from(t)) } else { json!(t.to_string()) }
    }

    fn rule_report(runs: &[Run], rule: NonTurn, gold: &Gold) -> Value {
        let all: Vec<&Run> = runs.iter().collect();
        let committed: Vec<&Run> = runs.iter().filter(|r| !r.client).collect();
        let loo = thresholds_loo(runs, rule);
        let loo_json: Map<String, Value> = loo.iter().map(|(id, t)| (id.clone(), num(*t))).collect();
        json!({
            "threshold_fit_on_all": num(fit(&candidates(runs, rule), &all, rule)),
            "thresholds_loo": loo_json,
            "sets": {"all": summary(&all, rule, &loo, gold), "committed": summary(&committed, rule, &loo, gold)},
        })
    }

    /// The report under both non-turn rules, and the rule kept: the one with the higher good outcomes minus wrong cites on all
    /// runs, leave-one-question-out; a tie keeps `not_found`.
    pub fn scored(runs: &[Run], gold: &Gold) -> Value {
        let not_found = rule_report(runs, NonTurn::NotFound, gold);
        let best_turn = rule_report(runs, NonTurn::BestTurn, gold);
        let net = |r: &Value| r["sets"]["all"]["good_minus_wrong"].as_i64();
        let kept = if net(&best_turn) > net(&not_found) { "best_turn" } else { "not_found" };
        json!({"non_turn": {"not_found": not_found, "best_turn": best_turn}, "non_turn_kept": kept})
    }

    fn secs(x: f64) -> Value {
        json!((x * 100.0).round() / 100.0)
    }

    /// Search plus reranking seconds, and reranking alone: the median and 90th percentile of every phrasing but the first
    /// (the first pays for warming the model), and the slowest phrasing of all.
    pub fn time_stats(runs: &[Run]) -> Value {
        let warm = |of: &dyn Fn(&Run) -> f64| -> (Value, Value) {
            let mut xs: Vec<f64> = runs.iter().skip(1).map(of).collect();
            xs.sort_by(f64::total_cmp);
            if xs.is_empty() {
                return (Value::Null, Value::Null);
            }
            let mid = xs.len() / 2;
            let median = if xs.len() % 2 == 1 { xs[mid] } else { (xs[mid - 1] + xs[mid]) / 2.0 };
            (secs(median), secs(xs[(xs.len() as f64 * 0.9) as usize]))
        };
        let total = |r: &Run| r.search_secs + r.ranked.rerank_secs;
        let ((median, p90), (rerank_median, rerank_p90)) = (warm(&total), warm(&|r| r.ranked.rerank_secs));
        let slowest = runs.iter().max_by(|a, b| total(a).total_cmp(&total(b)));
        json!({
            "warm_phrasings": runs.len().saturating_sub(1),
            "search_rerank_median_s": median, "search_rerank_p90_s": p90,
            "rerank_median_s": rerank_median, "rerank_p90_s": rerank_p90,
            "slowest_search_rerank_s": slowest.map(|r| secs(total(r))),
            "slowest_id": slowest.map(|r| r.id.clone()), "slowest_phrasing": slowest.map(|r| r.phrasing),
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn ask_eval(_: &Opts) -> Res {
    eprintln!("gopola ask needs macOS (the reranker runs through Metal)");
    std::process::exit(1)
}

/// The most memory this process held, in MB: `ri_lifetime_max_phys_footprint`, which Activity Monitor's memory column reads.
#[cfg(target_os = "macos")]
fn peak_footprint_mb() -> Option<f64> {
    // SAFETY: an all-zero `rusage_info_v4` is valid (plain integers), and RUSAGE_INFO_V4 is the flavor that fills that struct.
    let (rc, info) = unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(std::process::id() as libc::c_int, libc::RUSAGE_INFO_V4, (&raw mut info).cast());
        (rc, info)
    };
    (rc == 0).then(|| (info.ri_lifetime_max_phys_footprint as f64 / 1e5).round() / 10.0)
}

/// One text line per measure, the `all` set beside the `committed` one.
#[cfg(target_os = "macos")]
const MEASURES: [(&str, &[&str]); 10] = [
    ("right first turn", &["right_first_turn"]),
    ("top-1 session", &["top1_session"]),
    ("wrong first cites", &["wrong_first_cites"]),
    ("absent cited", &["absent_cited"]),
    ("good outcomes", &["good"]),
    ("good minus wrong", &["good_minus_wrong"]),
    ("invented refs", &["invented_refs"]),
    ("sentence score, quote filter", &["sentence", "quote_filter"]),
    ("sentence score, length", &["sentence", "length"]),
    ("sentence score, all sentences", &["sentence", "all"]),
];

#[cfg(target_os = "macos")]
fn print_report(report: &serde_json::Value, phrasings: usize, questions: usize, load_s: f64, time: &serde_json::Value, peak: Option<f64>) {
    use crate::transcript::printable;
    use serde_json::Value;
    let cell = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
    let secs = |v: &Value| v.as_f64().map_or("-".to_string(), |s| format!("{s:.2} s"));
    println!("eval ask: {phrasings} phrasings of {questions} questions; threshold candidates: all phrasings; model loaded in {load_s:.1} s");
    for (name, key) in [("A, not_found", "not_found"), ("D, best_turn", "best_turn")] {
        let rule = &report["non_turn"][key];
        println!("\nnon-turn rule {name}\n  {:<32} {:>9} {:>9}", "", "all", "committed");
        for (label, path) in MEASURES {
            let at = |set: &str| cell(path.iter().fold(&rule["sets"][set], |v, k| &v[*k]));
            println!("  {label:<32} {:>9} {:>9}", at("all"), at("committed"));
        }
        let t = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok()).unwrap_or(f64::NAN);
        let loo: Vec<f64> = rule["thresholds_loo"].as_object().map_or_else(Vec::new, |m| m.values().map(t).collect());
        let (low, high) = loo.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
        println!("  threshold fit on all: {}; leave-one-out thresholds {low} to {high}", cell(&rule["threshold_fit_on_all"]));
    }
    println!("\ntime, every phrasing but the first ({} of them): search + rerank median {}, p90 {}; rerank alone median {}, p90 {}",
        time["warm_phrasings"], secs(&time["search_rerank_median_s"]), secs(&time["search_rerank_p90_s"]),
        secs(&time["rerank_median_s"]), secs(&time["rerank_p90_s"]));
    println!("slowest: {} {}, {} for search + rerank", printable(time["slowest_id"].as_str().unwrap_or("-")),
        time["slowest_phrasing"].as_str().unwrap_or("-"), secs(&time["slowest_search_rerank_s"]));
    println!("peak memory: {}", peak.map_or("unknown".to_string(), |mb| format!("{mb} MB")));
    println!("non-turn rule kept: {}", if report["non_turn_kept"] == "best_turn" { "D" } else { "A" });
}

/// `eval ask --dir <folder>`: every phrasing is searched as `ask` searches (8 hits, all projects, before the row's `until`),
/// the hits are ranked by the reranker, and both non-turn rules are scored.
#[cfg(target_os = "macos")]
fn ask_eval(o: &Opts) -> Res {
    use crate::{ask, cmds, embed, transcript::printable};
    let dir = std::path::Path::new(o.dir.as_deref().ok_or("eval ask needs --dir <eval folder>")?);
    let questions = ask_score::read_questions(dir)?;
    let gold = ask_score::read_gold_sentences(dir)?;
    let c = crate::db::open()?;
    let embed_model = embed::Embedder::from_env().model;
    if !embed::has_vectors(&c, &embed_model)? {
        return Err(format!("no turn has a vector for {embed_model}: run `gopola embed`; the eval measures words and meaning together").into());
    }
    // Every search comes before the model loads: a search without meaning would measure something else.
    let mut searched = Vec::new();
    for q in &questions {
        for (phrasing, text) in [("exact", &q.exact), ("reworded", &q.reworded)] {
            let start = std::time::Instant::now();
            let mut found = cmds::search_rows(&c, &Opts { n: ask::HITS, ..Default::default() }, text, "all", "", &q.until)?;
            let search_secs = start.elapsed().as_secs_f64();
            if let Some(note) = &found.note {
                return Err(format!("meaning search is off ({note}); the eval measures words and meaning together").into());
            }
            found.rows.retain(|((_, r, ..), _)| !q.exclude.iter().any(|x| r.starts_with(x.as_str())));
            searched.push((q, phrasing, text, ask::docs(&c, &found.rows)?, search_secs));
        }
    }
    let model = ask::model_path();
    ask::check_model_file(&model)?;
    let start = std::time::Instant::now();
    let reranker = crate::rerank::load(&model)?;
    let load_s = start.elapsed().as_secs_f64();
    let mut scorer = reranker.scorer()?;
    let mut runs = Vec::new();
    for (q, phrasing, text, docs, search_secs) in searched {
        if !scorer.fits(text) {
            return Err(format!("{} {phrasing}: the question is too long to score", printable(&q.id)).into());
        }
        let ranked = ask::rank(&mut scorer, text, &docs)?;
        runs.push(ask_score::Run {
            id: q.id.clone(), phrasing, client: q.client, expects_answer: q.expects_answer,
            gold: q.gold.clone(), gold_sessions: q.gold_sessions.clone(),
            labels: docs.iter().map(|d| d.label.clone()).collect(),
            sentence: ask::RULES.map(|rule| ask::best_sentence(&ranked, rule).map(str::to_string)),
            ranked, search_secs,
        });
    }
    let scored = ask_score::scored(&runs, &gold);
    let time = ask_score::time_stats(&runs);
    let peak = peak_footprint_mb();
    if o.json {
        let label = |r: &ask_score::Run, i: Option<usize>| i.map(|i| printable(&r.labels[i]));
        let rows: Vec<serde_json::Value> = runs.iter().map(|r| serde_json::json!({"id": r.id, "phrasing": r.phrasing,
            "top": label(r, r.ranked.top), "best_turn": label(r, r.ranked.best_turn), "score": r.ranked.top.and_then(|i| r.ranked.scores[i])})).collect();
        println!("{}", serde_json::json!({"phrasings": runs.len(), "questions": questions.len(), "load_s": (load_s * 100.0).round() / 100.0,
            "time": time, "peak_footprint_mb": peak, "non_turn": scored["non_turn"], "non_turn_kept": scored["non_turn_kept"], "rows": rows}));
    } else {
        print_report(&scored, runs.len(), questions.len(), load_s, &time, peak);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ask_score::*;
    use crate::ask::{NonTurn, Ranked};
    use std::collections::HashMap;

    /// One phrasing's run from hand-written hits `(label, score)`; the top hit and the best turn are found as `ask::rank` finds them.
    fn run(id: &str, phrasing: &'static str, expects_answer: bool, gold: &[&str], hits: &[(&str, f32)]) -> Run {
        let first_max = |keep: &dyn Fn(&str) -> bool| hits.iter().enumerate().filter(|(_, h)| keep(h.0))
            .fold(None::<(usize, f32)>, |best, (i, h)| match best { Some((_, s)) if h.1 <= s => best, _ => Some((i, h.1)) }).map(|(i, _)| i);
        Run {
            id: id.into(), phrasing, client: false, expects_answer,
            gold: gold.iter().map(|g| g.to_string()).collect(), gold_sessions: gold.iter().map(|g| g[..8].to_string()).collect(),
            labels: hits.iter().map(|h| h.0.to_string()).collect(),
            ranked: Ranked { scores: hits.iter().map(|h| Some(h.1)).collect(), top: first_max(&|_| true),
                best_turn: first_max(&|l| l.contains(':')), sentences: Vec::new(), rerank_secs: 0.0 },
            sentence: [None, None, None], search_secs: 0.0,
        }
    }

    const NF: NonTurn = NonTurn::NotFound;

    #[test]
    fn good_and_wrong_follow_the_research_scorer() {
        let outcome = |r: &Run, t: f32| (good(r, NF, t), wrong(r, NF, t), refuses(r, NF, t));
        // An absent question: a cite is wrong and not good, a refusal is good.
        let absent = run("x01", "exact", false, &[], &[("aaaaaaaa:1", 0.9)]);
        assert_eq!(outcome(&absent, 0.5), (false, true, false));
        assert_eq!(outcome(&absent, 0.95), (true, false, true));
        // A question whose gold turn is in the hits: the gold turn is good, another turn is wrong, a refusal is neither.
        let hits = [("aaaaaaaa:1", 0.9), ("bbbbbbbb:2", 0.5)];
        let missed = run("q01", "exact", true, &["bbbbbbbb:2"], &hits);
        assert_eq!(outcome(&missed, 0.5), (false, true, false));
        assert_eq!(outcome(&missed, 0.95), (false, false, true));
        let found = run("q02", "exact", true, &["bbbbbbbb:2"], &[("bbbbbbbb:2", 0.9)]);
        assert_eq!(outcome(&found, 0.5), (true, false, false));
        // A refusal when no gold turn is among the hits is good; a cite there is wrong.
        let out_of_reach = run("q03", "exact", true, &["cccccccc:3"], &[("aaaaaaaa:1", 0.9)]);
        assert_eq!(outcome(&out_of_reach, 0.95), (true, false, true));
        assert_eq!(outcome(&out_of_reach, 0.5), (false, true, false));
        // A score equal to the threshold answers.
        assert_eq!(outcome(&found, 0.9), (true, false, false));
    }

    #[test]
    fn the_loo_threshold_never_sees_its_own_question() {
        // q01 is answered by a gold turn at 0.9; x01 has no answer but a turn scores 0.8.
        let runs = [
            run("q01", "exact", true, &["aaaaaaaa:1"], &[("aaaaaaaa:1", 0.9)]),
            run("q01", "reworded", true, &["aaaaaaaa:1"], &[("aaaaaaaa:1", 0.9)]),
            run("x01", "exact", false, &[], &[("bbbbbbbb:2", 0.8)]),
            run("x01", "reworded", false, &[], &[("bbbbbbbb:2", 0.8)]),
        ];
        let all: Vec<&Run> = runs.iter().collect();
        assert_eq!(fit(&candidates(&runs, NF), &all, NF), 0.9);
        let loo = thresholds_loo(&runs, NF);
        // q01 gets what x01 alone fits (refuse its 0.8: the candidate 0.9 is q01's own score, kept as a candidate),
        // x01 gets what q01 alone fits (anything answers it).
        assert_eq!((loo["q01"], loo["x01"]), (0.9, f32::NEG_INFINITY));
    }

    #[test]
    fn a_tie_takes_the_lowest_threshold() {
        // Any threshold up to 0.8 answers the one question well; the lowest of them is taken.
        let runs = [run("q01", "exact", true, &["aaaaaaaa:1"], &[("aaaaaaaa:1", 0.8)])];
        let all: Vec<&Run> = runs.iter().collect();
        assert_eq!(candidates(&runs, NF), [f32::NEG_INFINITY, 0.8, f32::INFINITY]);
        assert_eq!(fit(&candidates(&runs, NF), &all, NF), f32::NEG_INFINITY);
    }

    fn gold_sentences(rows: &[(&str, &str, &[&str])]) -> HashMap<String, HashMap<String, Vec<String>>> {
        let mut gold: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for (id, label, sentences) in rows {
            gold.entry(id.to_string()).or_default().insert(label.to_string(), sentences.iter().map(|s| s.to_string()).collect());
        }
        gold
    }

    #[test]
    fn the_sentence_score_counts_only_a_gold_top_with_gold_sentences() {
        let with = |id: &str, expects_answer: bool, gold: &[&str], top: (&str, f32), shown: &str| {
            let mut r = run(id, "exact", expects_answer, gold, &[top]);
            r.sentence = [None, None, Some(shown.to_string())];
            r
        };
        let runs = [
            with("q01", true, &["aaaaaaaa:1"], ("aaaaaaaa:1", 0.9), "Port 8095."),    // counted, shown
            with("q02", true, &["aaaaaaaa:1"], ("aaaaaaaa:1", 0.9), "Something else."), // counted, not shown
            with("q03", true, &["aaaaaaaa:1"], ("aaaaaaaa:1", 0.9), "Port 8095."),    // no gold sentence listed
            with("q04", true, &["aaaaaaaa:1"], ("bbbbbbbb:2", 0.9), "Port 8095."),    // top is not the gold turn
            with("q05", false, &[], ("aaaaaaaa:1", 0.9), "Port 8095."),               // absent question
            with("q06", true, &["aaaaaaaa:1"], ("aaaaaaaa:1", 0.9), "Port 8095."),    // question has no entry at all
            with("q07", true, &["aaaaaaaa:1"], ("aaaaaaaa:1", 0.01), "Port 8095."),   // counted whatever its score
        ];
        let gold = gold_sentences(&[
            ("q01", "aaaaaaaa:1", &["Port 8095.", "Another."]), ("q02", "aaaaaaaa:1", &["Port 8095."]), ("q03", "aaaaaaaa:1", &[]),
            ("q04", "bbbbbbbb:2", &["Port 8095."]), ("q05", "aaaaaaaa:1", &["Port 8095."]), ("q07", "aaaaaaaa:1", &["Port 8095."]),
        ]);
        let all: Vec<&Run> = runs.iter().collect();
        assert_eq!(sentence_hits(&all, NF, 2, &gold), (2, 3));
        // The same runs under the other rules' sentences: nothing was shown there.
        assert_eq!(sentence_hits(&all, NF, 0, &gold), (0, 3));
    }

    #[test]
    fn warm_median_and_p90_skip_the_first() {
        // The first run is cold: 100 s. The ten warm ones take 1.5, 3, ... 15 s (a third of each is reranking).
        let mut runs = vec![run("q00", "exact", true, &[], &[])];
        runs[0].search_secs = 50.0;
        runs[0].ranked.rerank_secs = 50.0;
        for i in 1..=10 {
            let mut r = run(&format!("q{i:02}"), "exact", true, &[], &[]);
            r.search_secs = i as f64;
            r.ranked.rerank_secs = i as f64 * 0.5;
            runs.push(r);
        }
        let t = time_stats(&runs);
        assert_eq!((t["search_rerank_median_s"].clone(), t["search_rerank_p90_s"].clone()), (8.25.into(), 15.0.into()), "{t}");
        assert_eq!((t["rerank_median_s"].clone(), t["rerank_p90_s"].clone()), (2.75.into(), 5.0.into()), "{t}");
        assert_eq!((t["slowest_search_rerank_s"].clone(), t["slowest_id"].clone()), (100.0.into(), "q00".into()), "{t}");
        // One run only: nothing is warm.
        assert!(time_stats(&runs[..1])["search_rerank_median_s"].is_null());
    }

    #[test]
    fn both_non_turn_rules_are_scored_and_a_tie_keeps_not_found() {
        let on_top = |id: &str| [
            run(id, "exact", true, &["aaaaaaaa:1"], &[("commit-0123456789ab", 0.99), ("aaaaaaaa:1", 0.97)]),
            run(id, "reworded", true, &["aaaaaaaa:1"], &[("commit-0123456789ab", 0.99), ("aaaaaaaa:1", 0.97)]),
        ];
        let plain = |id: &str| [
            run(id, "exact", true, &["bbbbbbbb:2"], &[("bbbbbbbb:2", 0.9)]),
            run(id, "reworded", true, &["bbbbbbbb:2"], &[("bbbbbbbb:2", 0.9)]),
        ];
        let net = |v: &serde_json::Value, rule: &str| v["non_turn"][rule]["sets"]["all"]["good_minus_wrong"].as_i64().unwrap();
        let gold = HashMap::new();

        // q01's top is a commit and its best turn is gold: best-turn answers it, not-found refuses.
        let runs: Vec<Run> = on_top("q01").into_iter().chain(plain("q02")).collect();
        let v = scored(&runs, &gold);
        assert_eq!((net(&v, "not_found"), net(&v, "best_turn")), (2, 4), "{v}");
        assert_eq!(v["non_turn_kept"], "best_turn");
        let at = |rule: &str, key: &str| v["non_turn"][rule]["sets"]["all"][key].clone();
        assert_eq!((at("not_found", "right_first_turn"), at("best_turn", "right_first_turn")), ("2/4".into(), "4/4".into()), "{v}");
        assert_eq!((at("not_found", "top1_session"), at("best_turn", "top1_session")), ("2/4".into(), "4/4".into()), "{v}");

        // An absent question that a turn answers at 0.8: it is cited, so wrong, under both rules.
        let absent = [run("x01", "exact", false, &[], &[("cccccccc:3", 0.8)]), run("x01", "reworded", false, &[], &[("cccccccc:3", 0.8)])];
        let runs: Vec<Run> = plain("q02").into_iter().chain(absent).collect();
        let v = scored(&runs, &gold);
        let at = |rule: &str, key: &str| v["non_turn"][rule]["sets"]["all"][key].clone();
        assert_eq!((at("not_found", "absent_cited"), at("not_found", "wrong_first_cites")), ("2/2".into(), 2.into()), "{v}");

        // Only plain questions: both rules agree, and a tie keeps not-found. q03 is a client question, so the committed set is q02 alone.
        let mut runs: Vec<Run> = plain("q02").into_iter().chain(plain("q03")).collect();
        runs[2].client = true;
        runs[3].client = true;
        let v = scored(&runs, &gold);
        assert_eq!((net(&v, "not_found"), net(&v, "best_turn")), (4, 4), "{v}");
        assert_eq!(v["non_turn_kept"], "not_found");

        // The whole report is there, each rule with its own thresholds and both sets.
        for rule in ["not_found", "best_turn"] {
            let r = &v["non_turn"][rule];
            assert!(!r["threshold_fit_on_all"].is_null() && !r["thresholds_loo"]["q02"].is_null(), "{r}");
            assert_eq!((r["sets"]["all"]["right_first_turn"].clone(), r["sets"]["committed"]["right_first_turn"].clone()), ("4/4".into(), "2/2".into()), "{r}");
            assert_eq!(r["sets"]["all"]["invented_refs"], 0, "{r}");
        }
    }

    #[test]
    fn the_fitted_threshold_is_written_exactly() {
        // `ask` compares a score with the threshold as f32s, so a threshold rounded above the phrasing that it was fitted on
        // would refuse that phrasing. 0.9512346 has seven significant digits.
        let score = 0.9512346_f32;
        let runs = [run("q01", "exact", true, &["aaaaaaaa:1"], &[("aaaaaaaa:1", score)]), run("x01", "exact", false, &[], &[("bbbbbbbb:2", 0.5)])];
        let v = scored(&runs, &HashMap::new());
        assert_eq!(v["non_turn"]["not_found"]["threshold_fit_on_all"].as_f64().map(|t| t as f32), Some(score), "{v}");
    }

    #[test]
    fn the_summary_counts_turns_and_sessions_apart() {
        let hit = |id: &str, top: &str| run(id, "exact", true, &["aaaaaaaa:1"], &[(top, 0.9)]);
        let runs = [hit("q01", "aaaaaaaa:1"), hit("q02", "aaaaaaaa:5"), hit("q03", "bbbbbbbb:2")];
        let all: Vec<&Run> = runs.iter().collect();
        let thr: HashMap<String, f32> = runs.iter().map(|r| (r.id.clone(), f32::NEG_INFINITY)).collect();
        let s = summary(&all, NF, &thr, &HashMap::new());
        // q02 cites another turn of the right session, q03 another session.
        assert_eq!((s["right_first_turn"].clone(), s["top1_session"].clone(), s["wrong_first_cites"].clone()), ("1/3".into(), "2/3".into(), 2.into()), "{s}");
    }

    /// A fresh folder for one test.
    fn eval_folder(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gopola-eval-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, text) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    const ROW: &str = r#"{"id":"q01","exact":"a b","reworded":"c d","gold":["aaaaaaaa:1"],"gold_sessions":["aaaaaaaa"],"expect":"answer","answer":"x","until":"2026-11-16"}"#;

    #[test]
    fn a_malformed_line_names_its_file_and_line() {
        let read = |name: &str, file: &str, text: &str| read_questions(&eval_folder(name, &[(file, text)])).err().unwrap();
        assert!(read("not-json", "ask.jsonl", &format!("{ROW}\n{{\n")).starts_with("ask.jsonl:2: "));
        assert_eq!(read("no-gold", "ask.jsonl", &format!("{ROW}\n{ROW}\n{}\n", ROW.replace("\"gold\":[\"aaaaaaaa:1\"],", ""))),
            "ask.jsonl:3: missing field `gold`");
        assert_eq!(read("bad-expect", "ask.jsonl", &ROW.replace("\"answer\",", "\"maybe\",")), "ask.jsonl:1: `expect` is not \"answer\" or \"refusal\"");
        assert_eq!(read("gold-type", "ask.jsonl", &ROW.replace("[\"aaaaaaaa:1\"]", "\"aaaaaaaa:1\"")), "ask.jsonl:1: `gold` is not a list of strings");
        // The client file is named by its path under the folder.
        let both = eval_folder("client-line", &[("ask.jsonl", ROW), ("local/ask-client.jsonl", "[]\n")]);
        assert_eq!(read_questions(&both).err().unwrap(), "local/ask-client.jsonl:1: missing field `id`");
    }

    #[test]
    fn without_local_only_the_committed_set_is_read() {
        let dir = eval_folder("committed", &[("ask.jsonl", &format!("{ROW}\n")), ("ask-sentences.json", r#"{"q01":{"aaaaaaaa:1":["Port 8095."],"note":"x"}}"#)]);
        let q = read_questions(&dir).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!((q[0].id.as_str(), q[0].exact.as_str(), q[0].reworded.as_str(), q[0].until.as_str()), ("q01", "a b", "c d", "2026-11-16"));
        assert_eq!((q[0].gold.clone(), q[0].gold_sessions.clone(), q[0].expects_answer, q[0].client, q[0].exclude.len()),
            (vec!["aaaaaaaa:1".to_string()], vec!["aaaaaaaa".to_string()], true, false, 0));
        // Keys that hold a list are turn labels; a note beside them is not one.
        assert_eq!(read_gold_sentences(&dir).unwrap()["q01"], HashMap::from([("aaaaaaaa:1".to_string(), vec!["Port 8095.".to_string()])]));
    }

    #[test]
    fn the_local_files_add_the_client_questions_and_win_the_sentences() {
        let client = ROW.replace("q01", "q02").replace("\"until\"", "\"exclude\":[\"ac0e31c8\"],\"until\"");
        let dir = eval_folder("local", &[("ask.jsonl", ROW), ("local/ask-client.jsonl", &client),
            ("ask-sentences.json", r#"{"q01":{"aaaaaaaa:1":["Committed."]}}"#), ("local/ask-sentences.json", r#"{"q02":{"aaaaaaaa:1":["Local."]}}"#)]);
        let q = read_questions(&dir).unwrap();
        assert_eq!(q.iter().map(|q| (q.id.as_str(), q.client, q.exclude.clone())).collect::<Vec<_>>(),
            [("q01", false, vec![]), ("q02", true, vec!["ac0e31c8".to_string()])]);
        assert_eq!(read_gold_sentences(&dir).unwrap().keys().collect::<Vec<_>>(), ["q02"]);
        // The committed sentences file is required when there is no local one.
        let bare = eval_folder("bare", &[("ask.jsonl", ROW)]);
        assert!(read_gold_sentences(&bare).unwrap_err().contains("ask-sentences.json"));
    }
}
