//! Meaning search: turn text into vectors through a local Ollama server, and rank by cosine.
//! The only network call gopola makes, and only to GOPOLA_OLLAMA (default http://localhost:11434).

use rusqlite::params;
use serde_json::{json, Value};

/// Small (346 MB loaded), 10 turns a second on an M1 Pro; measured 2026-10-13.
pub const DEFAULT_MODEL: &str = "embeddinggemma-2:270m";
const DOC_PREFIX: &str = "title: none | text: ";
const QUERY_PREFIX: &str = "task: search result | query: ";
/// What a turn is embedded from: the start of its prompt and of its reply. The probe used these cuts.
const PROMPT_CHARS: usize = 2000;
const REPLY_CHARS: usize = 4000;

/// The text a turn is embedded as, cut on character boundaries (a byte cut panics inside `é`).
pub fn doc_text(prompt: &str, reply: Option<&str>) -> String {
    let p: String = prompt.chars().take(PROMPT_CHARS).collect();
    let r: String = reply.unwrap_or("").chars().take(REPLY_CHARS).collect();
    format!("{DOC_PREFIX}{p}\n\n{r}")
}

pub fn query_text(q: &str) -> String {
    format!("{QUERY_PREFIX}{q}")
}

/// FNV-1a 64, hex. Stable across Rust releases, unlike `DefaultHasher`, so a stored hash still
/// matches the same text after a toolchain update.
pub fn text_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Scale to unit length so ranking is a dot product. A zero vector stays zero.
pub fn normalize(v: &mut [f32]) {
    let m = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if m > 0.0 {
        for x in v.iter_mut() {
            *x /= m;
        }
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dot of vectors of different lengths");
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Reciprocal-rank fusion: each list adds 1/(k + rank) to a key, rank counted from 1.
/// Ties keep the order in which keys were first seen, so the result is deterministic.
pub fn rrf(lists: &[Vec<String>], k: f64) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut score: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for list in lists {
        for (i, key) in list.iter().enumerate() {
            let s = score.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                0.0
            });
            *s += 1.0 / (k + i as f64 + 1.0);
        }
    }
    let mut keyed: Vec<(usize, String)> = order.into_iter().enumerate().collect();
    keyed.sort_by(|(ia, a), (ib, b)| score[b].partial_cmp(&score[a]).unwrap_or(std::cmp::Ordering::Equal).then(ia.cmp(ib)));
    keyed.into_iter().map(|(_, k)| k).collect()
}

#[derive(Debug)]
pub enum EmbedError {
    /// Nothing answered: the connection was refused, timed out, or the host does not resolve. Ollama is
    /// not running, or GOPOLA_OLLAMA names the wrong place.
    Unreachable(String),
    /// Ollama answered 404 naming the model: it has not been pulled.
    ModelMissing(String),
    /// Any other answer, or a peer that hung up mid-answer: another status, a body that is not JSON, or
    /// not one embedding per input. Carries ureq's or the server's own words.
    Bad(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Unreachable(m) => write!(f, "ollama not reachable at {m} (open Ollama.app or run `ollama serve`)"),
            EmbedError::ModelMissing(m) => write!(f, "model {m} is not pulled (ollama pull {m})"),
            EmbedError::Bad(m) => write!(f, "ollama answered unexpectedly: {m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

pub struct Embedder {
    pub base: String,
    pub model: String,
    agent: ureq::Agent,
}

impl Embedder {
    pub fn from_env() -> Embedder {
        let base = std::env::var("GOPOLA_OLLAMA").unwrap_or_else(|_| "http://localhost:11434".into());
        let model = std::env::var("GOPOLA_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(120)))
            .http_status_as_error(false)
            // Turn text goes to GOPOLA_OLLAMA and nowhere else: not through a proxy from the environment
            // (ureq reads ALL_PROXY/HTTPS_PROXY/HTTP_PROXY with no loopback exemption), not after a redirect.
            .proxy(None)
            .max_redirects(0)
            .build()
            .into();
        Embedder { base: base.trim_end_matches('/').to_string(), model, agent }
    }

    /// A failed request, by what failed: refused, a connect/resolve timeout or no such host means nothing answered;
    /// anything else (a peer that hung up mid-answer, a body that is not JSON) is a bad answer, in ureq's words.
    fn failed(&self, at: &str, e: ureq::Error) -> EmbedError {
        let refused = matches!(&e, ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused);
        // Only a connect or resolve timeout means nothing answered; the global timeout firing while the
        // server is answering is a slow server, which must not be told to run `ollama serve` (R7a).
        let no_answer = matches!(e, ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) | ureq::Error::HostNotFound);
        if refused || no_answer {
            EmbedError::Unreachable(format!("{}: {e}", self.base))
        } else if matches!(e, ureq::Error::Timeout(_)) {
            EmbedError::Bad(format!("{at}: ollama answered too slowly ({e})"))
        } else {
            EmbedError::Bad(format!("{at}: {e}"))
        }
    }

    /// One vector per text, in order, each scaled to unit length. Batches wait up to 120 s (a cold model load).
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed_within(texts, std::time::Duration::from_secs(120))
    }

    /// The same for something a person is waiting on (the search query, doctor's probe): gives up after 10 s.
    pub fn embed_quick(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed_within(texts, std::time::Duration::from_secs(10))
    }

    fn embed_within(&self, texts: &[String], timeout: std::time::Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/api/embed", self.base);
        let mut r = self.agent.post(&url).config().timeout_global(Some(timeout)).build()
            .send_json(json!({"model": self.model, "input": texts, "truncate": true}))
            .map_err(|e| self.failed(&url, e))?;
        let status = r.status().as_u16();
        let v: Value = r.body_mut().read_json().map_err(|e| self.failed(&format!("{url}: status {status}"), e))?;
        let err = v["error"].as_str().unwrap_or("");
        if status == 404 && err.contains("not found") {
            return Err(EmbedError::ModelMissing(self.model.clone()));
        }
        if status != 200 {
            return Err(EmbedError::Bad(format!("{url}: status {status}: {err}")));
        }
        let rows = v["embeddings"].as_array().ok_or_else(|| EmbedError::Bad(format!("{url}: no `embeddings`")))?;
        if rows.len() != texts.len() {
            return Err(EmbedError::Bad(format!("{url}: {} embeddings for {} inputs", rows.len(), texts.len())));
        }
        rows.iter().map(|row| {
            let mut x: Vec<f32> = row.as_array().ok_or_else(|| EmbedError::Bad(format!("{url}: a row is not a list")))?
                .iter().map(|n| n.as_f64().unwrap_or(0.0) as f32).collect();
            normalize(&mut x);
            Ok(x)
        }).collect()
    }
}

const BATCH: usize = 16;

/// `gopola embed [--limit N] [--quiet] [--json]`: embed every turn whose vector is missing or was made
/// from different text, newest first, 16 per request, each batch committed on its own so a killed hook
/// keeps what it finished. `--quiet` is the hook form: nothing on stdout, always exits 0; an error is one line on stderr (main.rs).
pub fn embed_cmd(o: &crate::cmds::Opts) -> crate::cmds::Res {
    let mut c = crate::db::open()?;
    crate::index::run(&mut c, false)?; // a session's last turn is embedded by the SessionEnd hook that follows its end
    let e = Embedder::from_env();
    // A vector whose turn is gone, in a session that is indexed, is stale. A session with no row is
    // between `index --full`'s delete and its rebuild: keep its vectors.
    c.execute(
        "delete from vectors where model = ?1
           and exists (select 1 from sessions s where s.id = vectors.session_id)
           and not exists (select 1 from turns t where t.session_id = vectors.session_id and t.n = vectors.n)",
        [&e.model],
    )?;
    let pending: Vec<(String, i64, String, String)> = {
        let mut st = c.prepare(
            "select t.session_id, t.n, t.prompt, t.reply, v.hash from turns t
             left join vectors v on v.session_id = t.session_id and v.n = t.n and v.model = ?1
             order by t.ts desc")?;
        let rows = st.query_map([&e.model], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?))
        })?;
        rows.flatten().filter_map(|(id, n, p, rep, have)| {
            let text = doc_text(&p, rep.as_deref());
            let hash = text_hash(&text);
            (have.as_deref() != Some(hash.as_str())).then_some((id, n, text, hash))
        }).collect()
    };
    let take = if o.n > 0 { o.n.min(pending.len()) } else { pending.len() };
    let started = std::time::Instant::now();
    let mut done = 0usize;
    for chunk in pending[..take].chunks(BATCH) {
        let texts: Vec<String> = chunk.iter().map(|(_, _, t, _)| t.clone()).collect();
        let vecs = e.embed(&texts)?; // the network call, outside any transaction
        let tx = c.transaction()?;
        for ((id, n, _, hash), v) in chunk.iter().zip(&vecs) {
            tx.execute(
                "insert or replace into vectors (session_id, n, model, hash, vec) values (?1, ?2, ?3, ?4, ?5)",
                params![id, n, e.model, hash, to_blob(v)],
            )?;
        }
        tx.commit()?;
        done += chunk.len();
    }
    let secs = started.elapsed().as_secs_f64();
    let left = pending.len() - done;
    crate::db::log_usage(&c, "embed", &e.model, done as i64);
    if o.quiet {
        return Ok(());
    }
    if o.json {
        println!("{}", json!({"embedded": done, "left": left, "model": e.model, "seconds": secs}));
    } else {
        println!("embedded {done} turns in {secs:.1} s with {}; {left} left", e.model);
    }
    Ok(())
}

/// Whether any turn has a vector for `model`.
pub fn has_vectors(c: &rusqlite::Connection, model: &str) -> rusqlite::Result<bool> {
    c.query_row("select exists (select 1 from vectors where model = ?1)", [model], |r| r.get(0))
}

/// One search hit: kind, ref, project, ts, snippet (`search` prints and fuses these).
pub type Hit = (String, String, String, String, String);

/// Turns ranked by cosine to the query, best first, limited to `project` ("all" for every project)
/// and to `since <= ts < until`. Kind `turn`, ref `<session>:<n:08>` like prompt and reply rows, and
/// the prompt's first line as the snippet. No vector for the model at all: an empty ranking, and no
/// call to Ollama (nothing embedded means meaning has nothing to add, not that it failed).
pub fn rank_turns(c: &rusqlite::Connection, e: &Embedder, query: &str, project: &str, since: &str, until: &str,
                  limit: usize) -> Result<Vec<Hit>, Box<dyn std::error::Error>> {
    let db = |x: rusqlite::Error| -> Box<dyn std::error::Error> { format!("database error: {x}").into() };
    if !has_vectors(c, &e.model).map_err(db)? {
        return Ok(Vec::new());
    }
    let q = e.embed_quick(&[query_text(query)])?.pop().unwrap_or_default();
    let mut st = c.prepare(
        "select v.session_id, v.n, s.project, t.ts, t.prompt, v.vec from vectors v
         join turns t on t.session_id = v.session_id and t.n = v.n
         join sessions s on s.id = v.session_id
         where v.model = ?1 and (s.project = ?2 or ?2 = 'all') and t.ts >= ?3 and t.ts < ?4").map_err(db)?;
    let mut scored: Vec<(f32, Hit)> = st
        .query_map(params![e.model, project, since, until], |r| {
            let (id, n, p, ts, prompt, blob): (String, i64, String, String, String, Vec<u8>) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
            let snippet: String = prompt.lines().next().unwrap_or("").chars().take(100).collect();
            let v = from_blob(&blob);
            if v.len() != q.len() {
                return Ok(None); // a vector of another length (a re-pulled model tag) cannot be compared
            }
            Ok(Some((dot(&q, &v), ("turn".to_string(), format!("{id}:{n:08}"), p, ts, snippet))))
        }).map_err(db)?.flatten().flatten().collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored.into_iter().take(limit).map(|(_, h)| h).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hash_is_fnv1a64() {
        // Published FNV-1a 64 vectors; the hash is stored, so it must never change between builds.
        assert_eq!(text_hash(""), "cbf29ce484222325");
        assert_eq!(text_hash("a"), "af63dc4c8601ec8c");
        assert_eq!(text_hash("foobar"), "85944171f73967e8");
    }

    #[test]
    fn doc_text_cuts_on_chars() {
        let prompt = "é".repeat(2001);
        let reply = "ü".repeat(4001);
        let t = doc_text(&prompt, Some(&reply));
        assert!(t.starts_with("title: none | text: "));
        assert_eq!(t.matches('é').count(), 2000);
        assert_eq!(t.matches('ü').count(), 4000);
        assert_eq!(doc_text("p", None), "title: none | text: p\n\n");
    }

    #[test]
    fn query_text_has_the_search_prefix() {
        assert_eq!(query_text("laptop models"), "task: search result | query: laptop models");
    }

    #[test]
    fn blob_round_trips() {
        let v = vec![0.0f32, -1.5, 3.25, f32::MIN_POSITIVE];
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert_eq!(to_blob(&v).len(), 16);
    }

    #[test]
    fn normalize_then_dot_is_cosine() {
        let mut a = vec![3.0f32, 4.0];
        let mut b = vec![6.0f32, 8.0];
        normalize(&mut a);
        normalize(&mut b);
        assert!((dot(&a, &b) - 1.0).abs() < 1e-6);
        let mut z = vec![0.0f32, 0.0];
        normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn rrf_rewards_agreement_and_keeps_first_seen_order_on_ties() {
        let words = vec!["a".to_string(), "b".into(), "c".into()];
        let meaning = vec!["c".to_string(), "d".into()];
        // c is 3rd and 1st: 1/63 + 1/61 beats a's 1/61 alone.
        assert_eq!(rrf(&[words, meaning], 60.0), vec!["c", "a", "b", "d"]);
        // a and x tie at 1/61: first seen wins.
        assert_eq!(rrf(&[vec!["a".into()], vec!["x".into()]], 60.0), vec!["a", "x"]);
    }

    #[test]
    fn dot_refuses_a_length_mismatch() {
        assert!(std::panic::catch_unwind(|| dot(&[1.0, 2.0], &[1.0])).is_err());
    }

    #[test]
    fn a_database_error_is_labelled_as_one() {
        let c = rusqlite::Connection::open_in_memory().unwrap(); // no vectors table
        let e = Embedder::from_env();
        let err = rank_turns(&c, &e, "q", "all", "", "9999", 10).unwrap_err().to_string();
        assert!(err.starts_with("database error: ") && !err.contains("ollama"), "{err}");
    }
}
