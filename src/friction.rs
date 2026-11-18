//! `gopola friction`: the labels that mark which typed prompts were corrections, and the scoring run (d3 and a
//! classifier trained on those labels, under a memory-pressure guard) that finds the likeliest ones. A label is one human verdict on one turn, kept in `labels`; `confirmed` says that the
//! owner has seen it. Nothing the retro writes can replace a confirmed label.

use crate::cmds::{Opts, Res, now_iso, short_id, short_ts};
use crate::embed::{BATCH, EmbedError, Embedder, from_blob, text_hash, to_blob};
use crate::insight::secs_ago;
use crate::transcript::{MAX_INPUT_BYTES, clip, first_line, printable, read_capped};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

pub const LABELS: [&str; 3] = ["correction", "borderline", "not_friction"];

/// The decision model that scores a prompt, and the memory-pressure level (macOS: 1 normal, 2 warning, 4 critical)
/// at which a run stops. The owner works at level 2 most of the day, so only 4 defers.
pub const D3_MODEL: &str = "d3:4b-nvfp4";
pub const DEFER_AT_LEVEL: u8 = 4;
/// sklearn's default regularisation, the setting that the measurements used.
const CLASSIFIER_C: f64 = 1.0;
const CLASSIFY_PREFIX: &str = "task: classification | query: ";

pub fn friction(o: &Opts, arg: Option<&str>) -> Res {
    let forms = [o.session.is_some(), o.since.is_some(), o.import_labels.is_some(), o.label.is_some(), o.pending, o.confirm.is_some(), o.auto];
    if forms.iter().filter(|f| **f).count() > 1 {
        return Err("friction takes one of --session, --since, --import-labels, --label, --pending, --confirm, --auto".into());
    }
    let mut c = crate::db::open()?;
    if o.auto {
        auto(&c);
        Ok(())
    } else if o.session.is_some() || o.since.is_some() {
        scoring(&mut c, o)
    } else if let Some(file) = &o.import_labels {
        import_labels(&mut c, o, file)
    } else if let Some(r) = &o.label {
        let label = arg.ok_or("--label needs <ref> <label>")?;
        label_one(&mut c, o, r, label)
    } else if o.pending {
        pending(&c, o)
    } else if let Some(r) = &o.confirm {
        confirm(&c, o, r, arg)
    } else {
        Err("friction needs --session <prefix> or --since <date>, or one of --import-labels, --label, --pending, --confirm".into())
    }
}

/// Text from a file or a command line, made safe to print in an error: clipped, control characters written out.
fn shown(s: &str) -> String {
    printable(&clip(s, 80))
}

fn check_label(label: &str) -> Result<(), String> {
    if LABELS.contains(&label) {
        Ok(())
    } else {
        Err(format!("'{}' is not a label: use {}", shown(label), LABELS.join(", ")))
    }
}

/// `<session prefix>:<turn>` to the one session it names and a turn that session holds. The prefix is limited to
/// the characters of a session id, so it cannot carry a `like` wildcard.
fn resolve_ref(c: &Connection, r: &str) -> Result<(String, i64), String> {
    let bad = || format!("'{}' is not a ref: write <session prefix>:<turn>", shown(r));
    let (prefix, n) = r.rsplit_once(':').ok_or_else(bad)?;
    let n: i64 = n.parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
    if !is_session_id(prefix) {
        return Err(bad());
    }
    let db = |e: rusqlite::Error| e.to_string();
    let (k, id): (i64, Option<String>) = c
        .query_row("select count(*), min(id) from sessions where id like ?1 || '%'", [prefix], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db)?;
    let id = match id {
        Some(id) if k == 1 => id,
        _ if k == 0 => return Err(format!("no session starts with '{prefix}'")),
        _ => return Err(format!("{k} sessions start with '{prefix}'; give more characters (--json has the full id)")),
    };
    let has_turn: bool = c
        .query_row("select exists (select 1 from turns where session_id = ?1 and n = ?2)", params![id, n], |r| r.get(0))
        .map_err(db)?;
    if has_turn { Ok((id, n)) } else { Err(format!("session {} has no turn {n}", short_id(&id))) }
}

fn ref_text(id: &str, n: i64) -> String {
    format!("{}:{n}", short_id(id))
}

/// A labels file (`{"<prefix>:<n>": {"label": …, …}, …}`) checked whole: the first bad row refuses the file. So does a
/// second key for a turn that an earlier key labelled differently (two keys can name one turn: a prefix and the full id).
fn read_labels(c: &Connection, path: &str) -> Result<Vec<(String, i64, String)>, String> {
    let text = read_capped(std::path::Path::new(path), MAX_INPUT_BYTES)?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let Value::Object(rows) = v else { return Err(format!("{path}: a labels file is a JSON object")) };
    let mut out = Vec::new();
    let mut first_key: HashMap<(String, i64), (&str, &str)> = HashMap::new();
    for (key, row) in &rows {
        let row_error = |why: String| {
            let quoted = json!(key).to_string();
            let line = text.lines().position(|l| l.contains(&quoted)).map(|i| format!(" on line {}", i + 1)).unwrap_or_default();
            format!("{path}: {}{line}: {why}", shown(&quoted))
        };
        let label = row.get("label").and_then(Value::as_str).ok_or_else(|| row_error("the value must be an object with a \"label\"".into()))?;
        check_label(label).map_err(row_error)?;
        let (id, n) = resolve_ref(c, key).map_err(row_error)?;
        match first_key.insert((id.clone(), n), (key, label)) {
            Some((first, first_label)) if first_label != label => {
                return Err(row_error(format!("the same turn as {} with a different label", shown(&json!(first).to_string()))));
            }
            Some(_) => {}
            None => out.push((id, n, label.to_string())),
        }
    }
    Ok(out)
}

fn import_labels(c: &mut Connection, o: &Opts, path: &str) -> Res {
    let rows = read_labels(c, path)?;
    let now = now_iso();
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (id, n, label) in &rows {
        tx.execute(
            "insert into labels (session_id, n, label, source, ts, confirmed) values (?1, ?2, ?3, 'import', ?4, 1)
             on conflict(session_id, n) do update set label = excluded.label, source = 'import', ts = excluded.ts, confirmed = 1",
            params![id, n, label, now],
        )?;
    }
    tx.commit()?;
    if o.json {
        println!("{}", json!({"imported": rows.len()}));
    } else {
        println!("imported {} labels", rows.len());
    }
    Ok(())
}

/// The retro's verdict. A confirmed row is never touched: the upsert only updates a row with `confirmed = 0`, and
/// what is stored afterwards says which case it was.
fn label_one(c: &mut Connection, o: &Opts, r: &str, label: &str) -> Res {
    check_label(label)?;
    let (id, n) = resolve_ref(c, r)?;
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "insert into labels (session_id, n, label, source, ts, confirmed) values (?1, ?2, ?3, 'retro', ?4, 0)
         on conflict(session_id, n) do update set label = excluded.label, source = 'retro', ts = excluded.ts
         where labels.confirmed = 0",
        params![id, n, label, now_iso()],
    )?;
    let (stored, confirmed): (String, bool) =
        tx.query_row("select label, confirmed from labels where session_id = ?1 and n = ?2", params![id, n], |r| Ok((r.get(0)?, r.get(1)?)))?;
    tx.commit()?;
    let r = ref_text(&id, n);
    if o.json {
        println!("{}", json!({"ref": r, "session_id": id, "n": n, "label": stored, "confirmed": confirmed}));
    } else if stored != label {
        println!("kept the confirmed label {stored} for {r}");
    } else {
        println!("labelled {r} {label}");
    }
    Ok(())
}

/// The owner's yes on an existing label, optionally with a different label.
fn confirm(c: &Connection, o: &Opts, r: &str, label: Option<&str>) -> Res {
    if let Some(l) = label {
        check_label(l)?;
    }
    let (id, n) = resolve_ref(c, r)?;
    let changed = c.execute(
        "update labels set confirmed = 1, label = coalesce(?3, label) where session_id = ?1 and n = ?2",
        params![id, n, label],
    )?;
    let r = ref_text(&id, n);
    if changed == 0 {
        return Err(format!("no label for {r}: record one with --label first").into());
    }
    if o.json {
        println!("{}", json!({"ref": r, "session_id": id, "n": n, "confirmed": true}));
    } else {
        println!("confirmed {r}");
    }
    Ok(())
}

/// The labels that wait for a yes, oldest first, each with its turn's first line.
fn pending(c: &Connection, o: &Opts) -> Res {
    let mut st = c.prepare(
        "select l.session_id, l.n, l.label, l.source, l.ts, t.prompt from labels l
         left join turns t on t.session_id = l.session_id and t.n = l.n
         where l.confirmed = 0 order by l.ts, l.session_id, l.n",
    )?;
    let rows: Vec<(String, i64, String, String, String, Option<String>)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<Result<_, _>>()?;
    let line = |p: &Option<String>| clip(first_line(p.as_deref().unwrap_or("")), 100);
    if o.json {
        let v: Vec<Value> = rows.iter().map(|(id, n, label, source, ts, p)| {
            json!({"ref": ref_text(id, *n), "session_id": id, "n": n, "label": label, "source": source, "ts": ts, "prompt": line(p)})
        }).collect();
        println!("{}", json!({"pending": v}));
    } else {
        for (id, n, label, _, _, p) in &rows {
            println!("{}  {label}  {}", ref_text(id, *n), printable(&line(p)));
        }
    }
    Ok(())
}

/// What a scoring run covers: one session by id prefix (as given), or a window of a project's turns.
pub enum Scope {
    Session(String),
    Since { since: String, project: String },
}

impl Scope {
    /// The `<scope>` that outcome rows start with. A prefix or a date from a command line is clipped and made printable.
    fn label(&self) -> String {
        match self {
            Scope::Session(prefix) => shown(prefix),
            Scope::Since { since, project } => shown(&format!("since {since} {project}")),
        }
    }
}

pub enum Outcome {
    Scored { scored: usize, marked: usize },
    Deferred(usize, String),
    Failed(usize, String),
    Skipped,
}

impl Outcome {
    /// The word that names it (the end of its `usage` command and `--json`'s `outcome`), the prompts newly scored, and its detail.
    fn parts(&self) -> (&'static str, usize, String) {
        match self {
            Outcome::Scored { scored, marked } => ("scored", *scored, format!("{scored} scored, {marked} rejected by d3")),
            Outcome::Deferred(n, why) => ("deferred", *n, why.clone()),
            Outcome::Failed(n, why) => ("failed", *n, why.clone()),
            Outcome::Skipped => ("skipped", 0, "friction scoring runs on macOS only".into()),
        }
    }
}

/// The memory-pressure level, read by running `$GOPOLA_SYSCTL` (default `/usr/sbin/sysctl`; no shell). `None` when it
/// cannot be run or does not print a number.
pub fn pressure_level() -> Option<u8> {
    let sysctl = std::env::var_os("GOPOLA_SYSCTL").unwrap_or_else(|| "/usr/sbin/sysctl".into());
    let out = std::process::Command::new(sysctl).args(["-n", "kern.memorystatus_vm_pressure_level"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether this machine can say how much memory is free: macOS, or any machine given a `GOPOLA_SYSCTL` script (the tests').
pub fn has_pressure_source() -> bool {
    cfg!(target_os = "macos") || std::env::var_os("GOPOLA_SYSCTL").is_some()
}

/// Why a scoring step stopped. Anything that is not a pressure reading is a failed run, its text safe to print.
enum Stop {
    Critical(u8),
    Unreadable,
    Fail(String),
}

impl<E: std::fmt::Display> From<E> for Stop {
    fn from(e: E) -> Stop {
        Stop::Fail(printable(&e.to_string()))
    }
}

impl Stop {
    fn why(&self) -> String {
        match self {
            Stop::Critical(level) => format!("memory pressure is critical (level {level})"),
            Stop::Unreadable => "memory pressure unreadable".into(),
            Stop::Fail(why) => why.clone(),
        }
    }
}

fn guard() -> Result<(), Stop> {
    match pressure_level() {
        Some(l) if l >= DEFER_AT_LEVEL => Err(Stop::Critical(l)),
        Some(_) => Ok(()),
        None => Err(Stop::Unreadable),
    }
}

fn lock_path() -> PathBuf {
    let mut p = crate::db::db_path().into_os_string();
    p.push(".friction.lock");
    p.into()
}

/// `Some` while this process holds the lock on `path` (made, with its folder, if missing; never deleted), `None` when
/// another holder has it. It never waits, and a killed holder leaves nothing behind: the lock goes with the process.
fn take_lock(path: &Path) -> Result<Option<std::fs::File>, String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path).map_err(io)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(io(e)),
    }
}

/// The outcome row, before anything else happens: a run that is killed leaves it as it is, and the catch-up reads
/// its scope like any other row's. Rows with no `<scope>: ` prefix (`invalid session id …`, a hook's no-JSON row)
/// are on purpose: the catch-up leaves them out by that text.
/// `None` when the insert failed: the connection's last rowid would then be an earlier run's row.
fn begin(c: &Connection, scope: &str) -> Option<i64> {
    c.execute(
        "insert into usage (ts, cmd, arg, hits) values (?1, 'friction-failed', ?2, 0)",
        params![now_iso(), format!("{scope}: did not finish")],
    ).ok()?;
    Some(c.last_insert_rowid())
}

/// Turns that row into the run's outcome, by rowid; with no row of its own (or one that cannot be updated) the outcome is a new row.
fn record(c: &Connection, row: Option<i64>, scope: &str, out: &Outcome) {
    let (kind, hits, detail) = out.parts();
    let (cmd, arg) = (format!("friction-{kind}"), format!("{scope}: {}", clip(&detail, 200)));
    let updated = row.is_some_and(|row| c.execute("update usage set cmd = ?2, arg = ?3, hits = ?4 where rowid = ?1", params![row, cmd, arg, hits as i64]).is_ok());
    if !updated {
        crate::db::log_usage(c, &cmd, &arg, hits as i64);
    }
}

/// The whole run: an outcome row, the index, d3 over the scope's new prompts, the classifier's vectors. One at a time.
/// A prefix that names one session is recorded as that session's whole id, so that a later run by id and one by
/// prefix are the same scope to the catch-up and to `doctor`.
pub fn run_scope(c: &mut Connection, e: &Embedder, scope: &Scope) -> Outcome {
    let label = scope.label();
    let row = begin(c, &label);
    let out = scoped(c, e, scope);
    let label = match scope {
        Scope::Session(_) => resolve(c, scope).ok().and_then(|ids| ids.into_iter().next()).map_or(label, |id| printable(&id)),
        Scope::Since { .. } => label,
    };
    record(c, row, &label, &out);
    out
}

fn scoped(c: &mut Connection, e: &Embedder, scope: &Scope) -> Outcome {
    if !has_pressure_source() {
        return Outcome::Skipped;
    }
    let _lock = match take_lock(&lock_path()) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Outcome::Deferred(0, "another friction run is in progress".into()),
        Err(m) => return Outcome::Failed(0, printable(&m)),
    };
    if let Err(err) = crate::index::run(c, false) {
        return Outcome::Failed(0, printable(&err.to_string()));
    }
    match resolve(c, scope) {
        Ok(ids) => score(c, e, &ids),
        Err(m) => Outcome::Failed(0, m),
    }
}

/// The ids of the sessions a scope names. A prefix is compared as text (a `%` or `_` in it is not a wildcard).
fn resolve(c: &Connection, scope: &Scope) -> Result<Vec<String>, String> {
    let db = |e: rusqlite::Error| e.to_string();
    match scope {
        Scope::Session(prefix) => {
            let (k, id): (i64, Option<String>) = c
                .query_row("select count(*), min(id) from sessions where substr(id, 1, length(?1)) = ?1", [prefix], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?;
            match id {
                Some(id) if k == 1 => Ok(vec![id]),
                _ if k == 0 => Err("no such session".into()),
                _ => Err(format!("{k} sessions start with '{}'", shown(prefix))),
            }
        }
        Scope::Since { since, project } => c
            .prepare(
                "select s.id from sessions s where (s.project = ?1 or ?1 = 'all')
                   and exists (select 1 from turns t where t.session_id = s.id and t.ts >= ?2)",
            )
            .map_err(db)?
            .query_map(params![project, since], |r| r.get(0))
            .map_err(db)?
            .collect::<Result<_, _>>()
            .map_err(db),
    }
}

struct Prompt {
    session_id: String,
    n: i64,
    prompt: String,
    project: String,
    ts: String,
}

/// Every typed prompt that is worth asking d3 about: not from a scripted session, under 2,000 characters and 3,900
/// bytes (so that it fits d3's 4,096-token window), not a command or tag (`<…`), not a continued-session note.
/// These bounds, the 120 s timeout and the pressure check are all that limit what reaches d3.
fn eligible(c: &Connection) -> rusqlite::Result<Vec<Prompt>> {
    c.prepare(
        "select t.session_id, t.n, t.prompt, s.project, t.ts from turns t join sessions s on s.id = t.session_id
         where s.scripted = 0 and length(t.prompt) < 2000 and length(cast(t.prompt as blob)) <= 3900
           and t.prompt not like '<%' and t.prompt not like 'This session is being continued%'
         order by t.session_id, t.n",
    )?
    .query_map([], |r| Ok(Prompt { session_id: r.get(0)?, n: r.get(1)?, prompt: r.get(2)?, project: r.get(3)?, ts: r.get(4)? }))?
    .collect()
}

/// The hash of the text that each stored d3 score was made from, by (session, turn).
fn stored_scores(c: &Connection) -> rusqlite::Result<HashMap<(String, i64), String>> {
    c.prepare("select session_id, n, hash from scores where model = ?1")?
        .query_map([D3_MODEL], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect()
}

/// d3's question, in the wording that was measured.
fn question() -> Value {
    json!({"type": "noul",
        "instructions": "Is the developer correcting the AI assistant, pointing out its mistake or missed step, repeating an earlier instruction, or showing frustration with how it worked?",
        "criteria": {"false": "A new request, an answer, an approval or a question, with no correction",
                     "true": "The developer corrects or pushes back on the assistant"}})
}

fn classify_text(prompt: &str) -> String {
    format!("{CLASSIFY_PREFIX}{prompt}")
}

/// What a run did before it stopped.
#[derive(Default)]
struct Tally {
    scored: usize,
    marked: usize,
    called: bool,
}

/// d3 over the eligible prompts of `ids` whose stored score is missing or is of other text, then the classification
/// vectors that are missing or stale. At critical pressure the run stops and asks Ollama to free d3's memory, if it
/// made a d3 call (an unload sent before one would load d3 first); the answer to that request changes nothing.
fn score(c: &mut Connection, e: &Embedder, ids: &[String]) -> Outcome {
    let mut t = Tally::default();
    match score_prompts(c, e, ids, &mut t) {
        Ok(()) => Outcome::Scored { scored: t.scored, marked: t.marked },
        Err(stop @ Stop::Critical(_)) => {
            if t.called {
                let _ = e.unload(D3_MODEL);
            }
            Outcome::Deferred(t.scored, stop.why())
        }
        Err(stop @ Stop::Unreadable) => Outcome::Deferred(t.scored, stop.why()),
        Err(Stop::Fail(why)) => Outcome::Failed(t.scored, why),
    }
}

fn score_prompts(c: &mut Connection, e: &Embedder, ids: &[String], t: &mut Tally) -> Result<(), Stop> {
    guard()?;
    let prompts: Vec<Prompt> = eligible(c)?.into_iter().filter(|p| ids.contains(&p.session_id)).collect();
    let have = stored_scores(c)?;
    let question = question();
    for p in &prompts {
        let hash = text_hash(&p.prompt);
        if have.get(&(p.session_id.clone(), p.n)) == Some(&hash) {
            continue;
        }
        guard()?;
        t.called = true;
        // A refused prompt is marked, so that it is not asked again until its text changes; any other error stops the run.
        let (score, error) = match e.noul(D3_MODEL, &p.prompt, &question) {
            Ok(v) => (Some(v), None),
            Err(EmbedError::Rejected(m)) => (None, Some(clip(&m, 200))),
            Err(err) => return Err(err.into()),
        };
        c.execute(
            "insert or replace into scores (session_id, n, model, hash, score, error, ts) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![p.session_id, p.n, D3_MODEL, hash, score, error, now_iso()],
        )?;
        if score.is_some() { t.scored += 1 } else { t.marked += 1 }
    }
    embed_classification(c, e, &prompts)
}

/// The classification model's key in `vectors`: the embedding model's own vectors stay apart from these.
fn classification_model(e: &Embedder) -> String {
    format!("{}+classification", e.model)
}

/// Classification vectors for the scope's prompts and for every confirmed-labelled turn (the classifier's training
/// rows), where one is missing or was made from other text, 16 to a request, each batch kept as it lands.
fn embed_classification(c: &mut Connection, e: &Embedder, prompts: &[Prompt]) -> Result<(), Stop> {
    let model = classification_model(e);
    let mut want: BTreeMap<(String, i64), String> = prompts.iter().map(|p| ((p.session_id.clone(), p.n), p.prompt.clone())).collect();
    let labelled: Vec<((String, i64), String)> = c
        .prepare("select t.session_id, t.n, t.prompt from labels l join turns t on t.session_id = l.session_id and t.n = l.n where l.confirmed = 1")?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    want.extend(labelled);
    let have: HashMap<(String, i64), String> = c
        .prepare("select session_id, n, hash from vectors where model = ?1")?
        .query_map([&model], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let todo: Vec<((String, i64), String, String)> = want
        .into_iter()
        .map(|(k, prompt)| {
            let text = classify_text(&prompt);
            let hash = text_hash(&text);
            (k, text, hash)
        })
        .filter(|(k, _, hash)| have.get(k) != Some(hash))
        .collect();
    for chunk in todo.chunks(BATCH) {
        let texts: Vec<String> = chunk.iter().map(|(_, text, _)| text.clone()).collect();
        let vecs = e.embed(&texts)?;
        let tx = c.transaction()?;
        for (((id, n), _, hash), v) in chunk.iter().zip(&vecs) {
            tx.execute(
                "insert or replace into vectors (session_id, n, model, hash, vec) values (?1, ?2, ?3, ?4, ?5)",
                params![id, n, model, hash, to_blob(v)],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// The classifier's fit: a correction is the positive class, `borderline` and `not_friction` the negative.
fn fit(x: &[Vec<f32>], labels: &[String]) -> Option<Model> {
    let y: Vec<bool> = labels.iter().map(|l| l == "correction").collect();
    train(x, &y, CLASSIFIER_C)
}

/// FR-3: each method's scores as fractional ranks over the rows that it scored, then the mean of the two with equal
/// weights. A row that the classifier has not scored keeps its d3 rank alone.
fn fr3(d3: &[f64], clf: &[Option<f64>]) -> Vec<f64> {
    let mut out = ranks(d3);
    let scored: Vec<(usize, f64)> = clf.iter().enumerate().filter_map(|(i, s)| s.map(|s| (i, s))).collect();
    let clf_ranks = ranks(&scored.iter().map(|s| s.1).collect::<Vec<_>>());
    for ((i, _), r) in scored.iter().zip(clf_ranks) {
        out[*i] = (out[*i] + r) / 2.0;
    }
    out
}

/// `friction --session <prefix> | --since D [--project P|all] [-n 10]`: the scoring run, then the likeliest corrections.
fn scoring(c: &mut Connection, o: &Opts) -> Res {
    let scope = match &o.session {
        Some(prefix) => Scope::Session(prefix.clone()),
        None => Scope::Since { since: o.since.clone().unwrap_or_default(), project: crate::cmds::project_filter(o) },
    };
    let e = Embedder::from_env();
    let out = run_scope(c, &e, &scope);
    if o.quiet {
        // The detached run also scores what earlier ones left behind. A deferral says the Mac is still busy.
        if let Scope::Session(id) = &scope
            && !matches!(out, Outcome::Deferred(..))
        {
            catch_up(c, &e, id);
        }
        return Ok(());
    }
    list(c, o, &e, &scope, &out)?;
    match out {
        Outcome::Scored { .. } => Ok(()),
        _ => {
            let (kind, _, detail) = out.parts();
            Err(format!("friction {kind}: {detail}").into())
        }
    }
}

const DAY: u64 = 86_400;
/// Other sessions that one detached run catches up.
const CATCH_UP_SESSIONS: usize = 5;
/// d3's measured time per prompt with the model held in memory.
const SECONDS_PER_PROMPT: f64 = 0.43;

/// A session id as Claude Code writes it: letters, digits and hyphens, at most 64. Nothing else reaches a command line or a lookup.
fn is_session_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `friction --auto`, the SessionEnd hook: starts the session's scoring as its own detached run and returns at once,
/// so that a slow or deferred run never holds a session's end. It says nothing and fails nothing: what goes wrong is
/// an outcome row, which is what `doctor` and the next run read.
fn auto(c: &Connection) {
    let input = crate::insight::read_stdin_json();
    let failed = |why: String| crate::db::log_usage(c, "friction-failed", &why, 0);
    match input["session_id"].as_str() {
        None => failed("the hook sent no session_id".into()),
        Some(sid) if !is_session_id(sid) => failed(format!("invalid session id {}", shown(sid))),
        Some(sid) => {
            if let Err(e) = spawn_detached(sid) {
                failed(format!("could not start: {}", shown(&e.to_string())));
            }
        }
    }
}

/// `friction --session <sid> --quiet` in a process group of its own (a hook that is killed does not take it along), with
/// no stdin, stdout or stderr (the hook's caller waits for its pipes to close), and never waited for.
fn spawn_detached(sid: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    std::process::Command::new(std::env::current_exe()?)
        .args(["friction", "--session", sid, "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// Other sessions' scoring, newest first, for the sessions that `stalled` names; a deferral ends it. When the rows
/// cannot be read, a failed row says so (`doctor` shows it): the run is detached, and nobody reads its stderr.
fn catch_up(c: &mut Connection, e: &Embedder, current: &str) {
    match stalled(c, current) {
        Ok(ids) => {
            for id in ids {
                if matches!(run_scope(c, e, &Scope::Session(id)), Outcome::Deferred(..)) {
                    break;
                }
            }
        }
        Err(err) => crate::db::log_usage(c, "friction-failed", &format!("catch-up could not read the outcome rows: {}", shown(&err.to_string())), 0),
    }
}

/// The one session that `text` names, an id or a prefix that starts one session; `None` for none or several.
fn one_session(c: &Connection, text: &str) -> Option<String> {
    resolve(c, &Scope::Session(text.to_string())).ok()?.pop()
}

/// Up to five sessions other than `current` (an id or the prefix that the run was given), newest first, whose latest
/// outcome row in the last 7 days is a deferral or a failure. Each row's scope is resolved to its one session, so that
/// a prefix and the id are the same session. A row that is not about one session (`since …`, a refusal, a hook with no
/// JSON, a prefix of none or several) is no session; neither is `no such session`. A `did not finish` row under an hour
/// old is a run in progress; an older one is a killed run.
fn stalled(c: &Connection, current: &str) -> rusqlite::Result<Vec<String>> {
    let hour = secs_ago(3600);
    let rows: Vec<(String, String, String)> = c
        .prepare("select cmd, arg, ts from usage where cmd like 'friction-%' and ts >= ?1 order by rowid desc")?
        .query_map([secs_ago(7 * DAY)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let current = one_session(c, current);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (cmd, arg, ts) in rows {
        let Some((scope, detail)) = arg.split_once(": ") else { continue };
        if !is_session_id(scope) {
            continue;
        }
        let Some(id) = one_session(c, scope) else { continue };
        if !seen.insert(id.clone()) {
            continue;
        }
        let stalled = cmd == "friction-deferred" || cmd == "friction-failed";
        if stalled && current.as_ref() != Some(&id) && detail != "no such session" && !(detail == "did not finish" && ts >= hour) {
            out.push(id);
            if out.len() == CATCH_UP_SESSIONS {
                break;
            }
        }
    }
    Ok(out)
}

/// doctor's friction line: what the recent runs did, and the ended sessions whose prompts are still unscored. A run in
/// progress (a `did not finish` row under an hour old) is neither a failure nor a deferral. Without a pressure source
/// nothing is scored, so the line says so.
pub fn doctor_line(c: &Connection, has_source: bool) -> rusqlite::Result<(&'static str, String)> {
    let month = secs_ago(30 * DAY);
    if !has_source {
        let skipped: i64 = c.query_row(
            "select count(distinct arg) from usage where cmd = 'friction-skipped' and arg not like 'since %' and ts >= ?1", [&month], |r| r.get(0))?;
        return Ok(("info", format!("friction: scoring runs on macOS only ({skipped} sessions skipped in 30 days)")));
    }
    let runs: Vec<(String, String, String)> = c
        .prepare(
            "select ts, cmd, arg from usage where cmd in ('friction-deferred', 'friction-failed') and ts >= ?1
               and not (arg like '%: did not finish' and ts >= ?2) order by rowid desc",
        )?
        .query_map([&month, &secs_ago(3600)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let deferred = runs.iter().filter(|r| r.1 == "friction-deferred").count();
    let counts = format!("{deferred} deferred and {} failed runs", runs.len() - deferred);
    let ended: std::collections::HashSet<String> = c
        .prepare("select id from sessions where ended_at < ?1 and ended_at >= ?2")?
        .query_map([secs_ago(1800), month], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let have = stored_scores(c)?;
    let unscored: Vec<Prompt> = eligible(c)?
        .into_iter()
        .filter(|p| ended.contains(&p.session_id) && have.get(&(p.session_id.clone(), p.n)) != Some(&text_hash(&p.prompt)))
        .collect();
    let Some(since) = unscored.iter().map(|p| p.ts.chars().take(10).collect::<String>()).min() else {
        return Ok(("ok", format!("friction: every ended session in 30 days scored ({counts}, since caught up)")));
    };
    let sessions: std::collections::HashSet<&str> = unscored.iter().map(|p| p.session_id.as_str()).collect();
    let minutes = (unscored.len() as f64 * SECONDS_PER_PROMPT / 6.0).ceil() / 10.0;
    let latest = runs.first().map(|(ts, _, arg)| format!("; latest: {} {}", short_ts(ts), shown(arg))).unwrap_or_default();
    Ok(("look", format!(
        "friction: {} session(s) in 30 days have unscored prompts ({counts}{latest}): gopola friction --since {since} --project all scores them \
         (about {SECONDS_PER_PROMPT} s a prompt with d3 held, so {} prompts take about {minutes:.1} min)",
        sessions.len(), unscored.len())))
}

/// The scope's scored prompts, likeliest first. Ranks are over every scored eligible prompt in the database; the scope
/// only picks what is printed. The classifier trains here, on the confirmed labels that have a current vector.
fn list(c: &Connection, o: &Opts, e: &Embedder, scope: &Scope, out: &Outcome) -> Res {
    let d3: HashMap<(String, i64), (String, f64)> = c
        .prepare("select session_id, n, hash, score from scores where model = ?1 and score is not null")?
        .query_map([D3_MODEL], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))?
        .collect::<Result<_, _>>()?;
    let mut scored: Vec<(Prompt, f64)> = Vec::new();
    for p in eligible(c)? {
        if let Some((hash, score)) = d3.get(&(p.session_id.clone(), p.n))
            && *hash == text_hash(&p.prompt)
        {
            scored.push((p, *score));
        }
    }
    let model_key = classification_model(e);
    let vectors: HashMap<(String, i64), (String, Vec<u8>)> = c
        .prepare("select session_id, n, hash, vec from vectors where model = ?1")?
        .query_map([&model_key], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))?
        .collect::<Result<_, _>>()?;
    let current = |sid: &str, n: i64, prompt: &str| -> Option<Vec<f32>> {
        let (hash, blob) = vectors.get(&(sid.to_string(), n))?;
        (*hash == text_hash(&classify_text(prompt))).then(|| from_blob(blob))
    };
    let mut x = Vec::new();
    let mut labels = Vec::new();
    for (sid, n, label, prompt) in c
        .prepare("select l.session_id, l.n, l.label, t.prompt from labels l join turns t on t.session_id = l.session_id and t.n = l.n where l.confirmed = 1")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        .collect::<Result<Vec<_>, _>>()?
    {
        if let Some(v) = current(&sid, n, &prompt) {
            x.push(v);
            labels.push(label);
        }
    }
    let model = fit(&x, &labels);
    let clf: Vec<Option<f64>> = scored
        .iter()
        .map(|(p, _)| model.as_ref().and_then(|m| current(&p.session_id, p.n, &p.prompt).map(|v| m.predict(&v))))
        .collect();
    let rank = fr3(&scored.iter().map(|s| s.1).collect::<Vec<_>>(), &clf);

    let ids = resolve(c, scope).unwrap_or_default();
    let in_scope = |p: &Prompt| {
        ids.contains(&p.session_id) && match scope {
            Scope::Since { since, .. } => p.ts.as_str() >= since.as_str(),
            Scope::Session(_) => true,
        }
    };
    let mut rows: Vec<(f64, f64, Option<f64>, &Prompt)> = scored
        .iter()
        .zip(&clf)
        .zip(&rank)
        .filter(|(((p, _), _), _)| in_scope(p))
        .map(|(((p, d), cl), r)| (*r, *d, *cl, p))
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.3.session_id.cmp(&b.3.session_id)).then(a.3.n.cmp(&b.3.n)));
    rows.truncate(if o.n == 0 { 10 } else { o.n });

    let marks: HashMap<(String, i64), (String, bool)> = c
        .prepare("select session_id, n, label, confirmed from labels")?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))?
        .collect::<Result<_, _>>()?;
    let (kind, count, detail) = out.parts();
    let trained = model.as_ref().map_or(0, |_| labels.len());
    if o.json {
        let prompts: Vec<Value> = rows.iter().map(|(rank, d3, clf, p)| {
            let mark = marks.get(&(p.session_id.clone(), p.n));
            json!({"ref": ref_text(&p.session_id, p.n), "session_id": p.session_id, "n": p.n, "project": p.project, "ts": p.ts,
                   "prompt": clip(first_line(&p.prompt), 100), "d3": d3, "classifier": clf, "rank": rank,
                   "label": mark.map(|m| &m.0), "confirmed": mark.map(|m| m.1)})
        }).collect();
        println!("{}", json!({"outcome": kind, "detail": detail, "scored": count, "classifier_labels": trained, "prompts": prompts}));
    } else {
        for (rank, d3, clf, p) in &rows {
            let clf = clf.map_or("  - ".to_string(), |v| format!("{v:.2}"));
            let tag = marks.get(&(p.session_id.clone(), p.n)).map_or(String::new(), |(label, confirmed)| format!("  [{label} {}]", if *confirmed { "✓" } else { "?" }));
            println!("{rank:.2}  d3 {d3:.2}  clf {clf}  {:<12} {}  {}  {}{tag}", ref_text(&p.session_id, p.n), printable(&p.project), short_ts(&p.ts),
                printable(&clip(first_line(&p.prompt), 100)));
        }
        println!("{}", if model.is_some() { format!("classifier: trained on {trained} confirmed labels") } else { "classifier: off (no confirmed labels of both kinds; d3 alone)".to_string() });
    }
    Ok(())
}

/// A score at or above this is a flag; the research counted flags this way.
const FLAG_AT: f64 = 0.5;
/// The eval's done line: FR-3's average precision over every prompt.
const DONE_AP: f64 = 0.6;
/// The share of prompts that the retro reads from the top of the list, per 100 prompts.
const TOP_PER_100: f64 = 20.0;

/// One prompt of an eval set. Its label is the confirmed file's: a `label` that the row itself carries is not read.
struct EvalRow {
    id: String,
    session_id: String,
    prompt: String,
    label: String,
    committed: bool,
}

impl EvalRow {
    fn correction(&self) -> bool {
        self.label == "correction"
    }
}

/// `{id: {label, …}}` from `local/friction-labels-confirmed.json` when it is there (it holds every row's label on the
/// owner's Mac), else from `friction-labels-confirmed.json`.
fn eval_labels(dir: &Path) -> Result<HashMap<String, String>, String> {
    let local = dir.join("local/friction-labels-confirmed.json");
    let path = if local.exists() { local } else { dir.join("friction-labels-confirmed.json") };
    let at = path.display();
    let file: Value = serde_json::from_str(&read_capped(&path, MAX_INPUT_BYTES)?).map_err(|e| format!("{at}: not JSON: {e}"))?;
    file.as_object().ok_or_else(|| format!("{at}: not an object of ids"))?.iter().map(|(id, v)| {
        let label = v.get("label").and_then(Value::as_str).ok_or_else(|| format!("{at}: {}: not an object with a `label`", shown(id)))?;
        check_label(label).map_err(|m| format!("{at}: {}: {m}", shown(id)))?;
        Ok((id.clone(), label.to_string()))
    }).collect()
}

/// `friction.jsonl`, then `local/friction-client.jsonl` when it is there. Each row is an object with string `id`,
/// `session_id` and `prompt`; other keys are ignored. A row with no confirmed label is an error, not a row left out.
fn eval_rows(dir: &Path) -> Result<Vec<EvalRow>, String> {
    let labels = eval_labels(dir)?;
    let mut rows = Vec::new();
    for (file, committed) in [("friction.jsonl", true), ("local/friction-client.jsonl", false)] {
        let path = dir.join(file);
        if !committed && !path.exists() {
            continue;
        }
        for (i, line) in read_capped(&path, MAX_INPUT_BYTES)?.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let at = format!("{file}:{}", i + 1);
            let row: Value = serde_json::from_str(line).map_err(|e| format!("{at}: not JSON: {e}"))?;
            let field = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("{at}: `{k}` is not a string"));
            let (id, session_id, prompt) = (field("id")?, field("session_id")?, field("prompt")?);
            let label = labels.get(&id).ok_or_else(|| format!("{at}: {} has no confirmed label", shown(&id)))?.clone();
            rows.push(EvalRow { id, session_id, prompt, label, committed });
        }
    }
    Ok(rows)
}

/// d3's score and the classifier's for every row. d3 is asked once per prompt, with the pressure read before each call
/// and d3 unloaded if it turns critical after a call. Then the classification vectors, 16 to a request, and the
/// classifier scored leave-one-session-out: each session's rows by a model trained on all the others.
fn eval_scores(e: &Embedder, rows: &[EvalRow]) -> Result<(Vec<f64>, Vec<f64>), Stop> {
    let question = question();
    let mut d3 = Vec::with_capacity(rows.len());
    for r in rows {
        if let Err(stop) = guard() {
            if !d3.is_empty() && matches!(stop, Stop::Critical(_)) {
                let _ = e.unload(D3_MODEL);
            }
            return Err(stop);
        }
        d3.push(match e.noul(D3_MODEL, &r.prompt, &question) {
            Ok(score) => score,
            Err(EmbedError::Rejected(m)) => return Err(Stop::Fail(format!("{}: d3 refused the prompt: {}", shown(&r.id), shown(&m)))),
            Err(err) => return Err(err.into()),
        });
    }
    let mut x: Vec<Vec<f32>> = Vec::with_capacity(rows.len());
    for chunk in rows.chunks(BATCH) {
        x.extend(e.embed(&chunk.iter().map(|r| classify_text(&r.prompt)).collect::<Vec<_>>())?);
    }
    let mut clf = vec![0.0; rows.len()];
    for held_out in rows.iter().map(|r| r.session_id.as_str()).collect::<BTreeSet<_>>() {
        let (test, train): (Vec<usize>, Vec<usize>) = (0..rows.len()).partition(|&i| rows[i].session_id == held_out);
        let model = fit(&train.iter().map(|&i| x[i].clone()).collect::<Vec<_>>(), &train.iter().map(|&i| rows[i].label.clone()).collect::<Vec<_>>())
            .ok_or_else(|| Stop::Fail(format!("without session {}, the other rows lack a correction or a row that is not one: the classifier cannot be trained", shown(held_out))))?;
        for i in test {
            clf[i] = model.predict(&x[i]);
        }
    }
    Ok((d3, clf))
}

/// One method's measures: borderline counts as not a correction.
struct Measure {
    ap: f64,
    /// The corrections expected in the top `TOP_PER_100`, and that many rows.
    found: f64,
    k: usize,
    /// Rows flagged, and how many of them are corrections; `None` for a rank, which is not a probability.
    flags: Option<(usize, usize)>,
    /// The average precision over the committed rows alone; `None` when none of them is a correction.
    committed_ap: Option<f64>,
}

fn measure(rows: &[EvalRow], scores: &[f64], flag_at: Option<f64>) -> Measure {
    let pairs: Vec<(f64, bool)> = scores.iter().zip(rows).map(|(s, r)| (*s, r.correction())).collect();
    let committed: Vec<(f64, bool)> = scores.iter().zip(rows).filter(|(_, r)| r.committed).map(|(s, r)| (*s, r.correction())).collect();
    let (found, k) = top_found(&pairs, TOP_PER_100);
    Measure {
        ap: average_precision(&pairs),
        found,
        k,
        flags: flag_at.map(|t| (pairs.iter().filter(|p| p.0 >= t).count(), pairs.iter().filter(|p| p.0 >= t && p.1).count())),
        committed_ap: committed.iter().any(|p| p.1).then(|| average_precision(&committed)),
    }
}

fn method_json(m: &Measure, corrections: usize) -> Value {
    let mut v = json!({"ap": m.ap, "top20_found": m.found, "top20_k": m.k});
    if let Some((flagged, hit)) = m.flags {
        v["flags"] = json!(flagged);
        v["precision"] = json!((flagged > 0).then(|| hit as f64 / flagged as f64));
        v["recall"] = json!(hit as f64 / corrections as f64);
    }
    v
}

/// `eval friction --dir <folder>`: d3, the classifier (leave-one-session-out) and FR-3 scored on an eval set, with
/// `correction` as the positive class. It opens no database, and takes the scoring runs' lock so that it never scores
/// beside one. It prints ids and numbers, never a prompt's words.
pub fn eval(o: &Opts, dir: &Path) -> Res {
    if !has_pressure_source() {
        return Err("friction scoring runs on macOS only".into());
    }
    let rows = eval_rows(dir)?;
    let corrections = rows.iter().filter(|r| r.correction()).count();
    if corrections == 0 {
        return Err("no row is labelled a correction: there is nothing to measure".into());
    }
    let _lock = take_lock(&lock_path())?.ok_or("another friction run is in progress")?;
    let (d3, clf) = eval_scores(&Embedder::from_env(), &rows).map_err(|stop| stop.why())?;
    let blend = fr3(&d3, &clf.iter().map(|s| Some(*s)).collect::<Vec<_>>());
    let methods = [
        ("d3", "d3", measure(&rows, &d3, Some(FLAG_AT))),
        ("classifier", "classifier", measure(&rows, &clf, Some(FLAG_AT))),
        ("FR-3", "fr3", measure(&rows, &blend, None)),
    ];
    let committed = rows.iter().filter(|r| r.committed).count();
    let pass = methods[2].2.ap >= DONE_AP;
    let committed_ap = methods.each_ref().map(|m| m.2.committed_ap);
    if o.json {
        let committed_ap = match committed_ap {
            [Some(a), Some(b), Some(c)] => json!({"d3": a, "classifier": b, "fr3": c}),
            _ => Value::Null,
        };
        let methods: serde_json::Map<String, Value> = methods.iter().map(|(_, key, m)| (key.to_string(), method_json(m, corrections))).collect();
        println!("{}", json!({"prompts": rows.len(), "committed": committed, "corrections": corrections, "borderline": "counted negative",
            "methods": methods, "committed_ap": committed_ap, "done_line": {"fr3_ap_at_least": DONE_AP, "pass": pass}}));
        return Ok(());
    }
    println!("friction eval: {} prompts ({committed} committed), {corrections} corrections; borderline counted negative", rows.len());
    println!("{:<12}{:<7}{:<20}flags at {FLAG_AT} (P / R)", "method", "AP", "top-20 per 100");
    for (name, _, m) in &methods {
        let flags = m.flags.map_or("-".to_string(), |(flagged, hit)| {
            let precision = if flagged > 0 { format!("{:.2}", hit as f64 / flagged as f64) } else { "-".to_string() };
            format!("{flagged} ({precision} / {:.2})", hit as f64 / corrections as f64)
        });
        println!("{name:<12}{:<7}{:<20}{flags}", format!("{:.3}", m.ap), format!("{:.1}/{corrections} (k={})", m.found, m.k));
    }
    match committed_ap {
        [Some(a), Some(b), Some(c)] => println!("{:<12}d3 {a:.3} · classifier {b:.3} · FR-3 {c:.3}", "committed"),
        _ => println!("{:<12}no correction among the committed prompts", "committed"),
    }
    println!("{}", done_line(methods[2].2.ap, pass));
    Ok(())
}

/// The done line, with the AP that it judges: the table rounds to 3 decimals, so 0.5996 reads 0.600 there above a fail.
fn done_line(ap: f64, pass: bool) -> String {
    format!("done line (FR-3 AP {ap:.4} >= {DONE_AP} on all prompts): {}", if pass { "pass" } else { "fail" })
}

/// A logistic regression over embeddings: `predict` is the chance that a row is a correction.
pub struct Model {
    w: Vec<f64>,
    b: f64,
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

impl Model {
    pub fn predict(&self, x: &[f32]) -> f64 {
        sigmoid(self.w.iter().zip(x).map(|(w, v)| w * f64::from(*v)).sum::<f64>() + self.b)
    }
}

/// Fits `0.5·|w|² + C·Σ sᵢ·logloss(yᵢ, w·xᵢ + b)` (sklearn's objective with balanced class weights
/// `sᵢ = n / (2·n_class(yᵢ))`; the intercept is not penalised) by Nesterov-accelerated gradient descent.
/// `None` when `y` lacks either class: a model of one class says nothing.
pub fn train(x: &[Vec<f32>], y: &[bool], c: f64) -> Option<Model> {
    let n = y.len() as f64;
    let npos = y.iter().filter(|&&v| v).count() as f64;
    if npos == 0.0 || npos == n {
        return None;
    }
    let s: Vec<f64> = y.iter().map(|&v| n / (2.0 * if v { npos } else { n - npos })).collect();
    let rows: Vec<Vec<f64>> = x.iter().map(|r| r.iter().map(|&v| f64::from(v)).collect()).collect();
    let max_sq = rows.iter().map(|r| r.iter().map(|v| v * v).sum::<f64>()).fold(0.0, f64::max);
    // A bound on the gradient's Lipschitz constant, so that a step of 1/l always descends.
    let l = 1.0 + c * s.iter().sum::<f64>() * (max_sq + 1.0) / 4.0;
    let mut w = vec![0.0; rows.first().map_or(0, Vec::len)];
    let mut b = 0.0;
    let (mut w_prev, mut b_prev) = (w.clone(), b);
    for it in 0..20_000 {
        let mom = it as f64 / (it as f64 + 3.0);
        let yw: Vec<f64> = w.iter().zip(&w_prev).map(|(a, p)| a + mom * (a - p)).collect();
        let yb = b + mom * (b - b_prev);
        let mut g = yw.clone();
        let mut gb = 0.0;
        for ((row, &yi), si) in rows.iter().zip(y).zip(&s) {
            let z = row.iter().zip(&yw).map(|(a, w)| a * w).sum::<f64>() + yb;
            let r = c * si * (sigmoid(z) - f64::from(u8::from(yi)));
            for (gj, a) in g.iter_mut().zip(row) {
                *gj += r * a;
            }
            gb += r;
        }
        if (g.iter().map(|v| v * v).sum::<f64>() + gb * gb).sqrt() < 1e-5 {
            (w, b) = (yw, yb);
            break;
        }
        w_prev = w;
        b_prev = b;
        w = yw.iter().zip(&g).map(|(a, g)| a - g / l).collect();
        b = yb - gb / l;
    }
    Some(Model { w, b })
}

/// Each score's rank among `scores`, from 0 up to just under 1 (position over n); equal scores share the
/// mean of their positions, so a tie does not fake an order.
pub fn ranks(scores: &[f64]) -> Vec<f64> {
    let n = scores.len() as f64;
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[a].total_cmp(&scores[b]));
    let mut out = vec![0.0; scores.len()];
    let mut start = 0;
    for tie in order.chunk_by(|&a, &b| scores[a].total_cmp(&scores[b]).is_eq()) {
        let rank = (2 * start + tie.len() - 1) as f64 / 2.0 / n;
        for &i in tie {
            out[i] = rank;
        }
        start += tie.len();
    }
    out
}

/// (rows, positives) for each distinct score, highest score first.
fn tie_groups(pairs: &[(f64, bool)]) -> Vec<(usize, usize)> {
    let mut sorted = pairs.to_vec();
    sorted.sort_by(|a, b| b.0.total_cmp(&a.0));
    sorted
        .chunk_by(|a, b| a.0.total_cmp(&b.0).is_eq())
        .map(|g| (g.len(), g.iter().filter(|p| p.1).count()))
        .collect()
}

/// Average precision as sklearn computes it: one step per distinct score, so a tie is one threshold.
/// 0 when there is no positive.
pub fn average_precision(pairs: &[(f64, bool)]) -> f64 {
    let npos = pairs.iter().filter(|p| p.1).count() as f64;
    if npos == 0.0 {
        return 0.0;
    }
    let (mut seen, mut tp, mut prev_recall, mut ap) = (0.0, 0.0, 0.0, 0.0);
    for (rows, pos) in tie_groups(pairs) {
        seen += rows as f64;
        tp += pos as f64;
        let recall = tp / npos;
        ap += (recall - prev_recall) * tp / seen;
        prev_recall = recall;
    }
    ap
}

/// The corrections expected among the top k rows, and k = max(1, round(n × per100 / 100)) (a half rounds to
/// even, as the measured Python round does). Inside a tie at the cut, the expected count if the tied rows came in
/// random order.
pub fn top_found(pairs: &[(f64, bool)], per100: f64) -> (f64, usize) {
    let k = ((pairs.len() as f64 * per100 / 100.0).round_ties_even() as usize).max(1);
    let (mut found, mut left) = (0.0, k);
    for (rows, pos) in tie_groups(pairs) {
        let take = rows.min(left);
        found += (pos * take) as f64 / rows as f64;
        left -= take;
        if left == 0 {
            break;
        }
    }
    (found, k)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed pseudo-random stream in [-1, 1), so the points never change between runs.
    fn seeded(n: usize, dims: usize) -> Vec<Vec<f32>> {
        let mut s: u64 = 12345;
        let mut next = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as f32 / (1u64 << 30) as f32) - 1.0
        };
        (0..n).map(|_| (0..dims).map(|_| next()).collect()).collect()
    }

    /// 40 overlapping points in 4 dimensions, 10 of them positive.
    fn overlapping() -> (Vec<Vec<f32>>, Vec<bool>) {
        let mut x = seeded(40, 4);
        let y: Vec<bool> = (0..40).map(|i| i < 10).collect();
        for (row, &pos) in x.iter_mut().zip(&y) {
            if pos {
                row[0] += 0.7;
            }
        }
        (x, y)
    }

    /// The norm of the objective's gradient at `m`, with class weights written out from the 10 and 30.
    fn gradient_norm(m: &Model, x: &[Vec<f32>], y: &[bool], c: f64) -> f64 {
        let (npos, n) = (10.0, 40.0);
        let s = |yi: bool| if yi { n / (2.0 * npos) } else { n / (2.0 * (n - npos)) };
        let mut g: Vec<f64> = m.w.clone();
        let mut gb = 0.0;
        for (row, &yi) in x.iter().zip(y) {
            let z: f64 = row.iter().zip(&m.w).map(|(a, b)| f64::from(*a) * b).sum::<f64>() + m.b;
            let r = c * s(yi) * (sigmoid(z) - if yi { 1.0 } else { 0.0 });
            for (gj, a) in g.iter_mut().zip(row) {
                *gj += r * f64::from(*a);
            }
            gb += r;
        }
        (g.iter().map(|v| v * v).sum::<f64>() + gb * gb).sqrt()
    }

    #[test]
    fn train_reaches_the_optimum() {
        let (x, y) = overlapping();
        let m = train(&x, &y, 1.0).unwrap();
        let norm = gradient_norm(&m, &x, &y, 1.0);
        assert!(norm < 1e-4, "gradient norm {norm}");
    }

    #[test]
    fn train_honours_c() {
        let (x, y) = overlapping();
        let m = train(&x, &y, 0.05).unwrap();
        let norm = gradient_norm(&m, &x, &y, 0.05);
        assert!(norm < 1e-4, "gradient norm {norm}");
    }

    #[test]
    fn train_separates_separable_data() {
        let x: Vec<Vec<f32>> = vec![
            vec![1.0, 0.0], vec![0.8, 0.1], vec![0.9, 0.0], vec![0.0, 1.0], vec![0.1, 0.7], vec![0.0, 0.9],
        ];
        let y = [true, true, true, false, false, false];
        let m = train(&x, &y, 1.0).unwrap();
        let pairs: Vec<(f64, bool)> = x.iter().map(|r| m.predict(r)).zip(y).collect();
        assert!((average_precision(&pairs) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn train_needs_both_classes() {
        let x = seeded(6, 3);
        assert!(train(&x, &[false; 6], 1.0).is_none());
        assert!(train(&x, &[true; 6], 1.0).is_none());
    }

    #[test]
    fn ranks_share_ties() {
        assert_eq!(ranks(&[0.2, 0.9, 0.2, 0.5]), vec![0.125, 0.75, 0.125, 0.5]);
    }

    #[test]
    fn average_precision_matches_sklearn_steps() {
        // the 0.8 tie is one threshold: recall 0.5 at precision 1, then recall 1.0 at precision 2/3
        let ap = average_precision(&[(0.9, true), (0.8, false), (0.8, true), (0.1, false)]);
        assert!((ap - (0.5 + 0.5 * 2.0 / 3.0)).abs() < 1e-4, "ap {ap}");
    }

    #[test]
    fn top_found_counts_a_tie_at_the_cut() {
        let pairs = [(0.9, true), (0.5, true), (0.5, false), (0.5, false), (0.1, true)];
        let (found, k) = top_found(&pairs, 40.0);
        assert_eq!(k, 2);
        assert!((found - (1.0 + 1.0 / 3.0)).abs() < 1e-9, "found {found}");
    }

    #[test]
    fn the_done_line_shows_the_ap_it_judges_to_four_decimals() {
        assert_eq!(done_line(0.5996, false), "done line (FR-3 AP 0.5996 >= 0.6 on all prompts): fail");
        assert_eq!(done_line(1.0, true), "done line (FR-3 AP 1.0000 >= 0.6 on all prompts): pass");
    }

    #[test]
    fn average_precision_is_zero_without_a_positive() {
        assert_eq!(average_precision(&[(0.9, false), (0.1, false)]), 0.0);
    }

    #[test]
    fn top_found_takes_at_least_one_row() {
        // 3 rows at 10 per 100 round to 0, and the top row is still looked at
        assert_eq!(top_found(&[(0.1, false), (0.9, true), (0.2, false)], 10.0), (1.0, 1));
    }

    #[test]
    fn fr3_averages_two_rank_lists_with_equal_weights() {
        // d3 ranks over all three rows: [2/3, 0, 1/3]. The classifier scored two rows: ranks [0, 1/2] over those two.
        // The row it has not scored keeps its d3 rank alone.
        let r = fr3(&[0.9, 0.1, 0.5], &[Some(0.2), Some(0.8), None]);
        let want = [(2.0 / 3.0 + 0.0) / 2.0, (0.0 + 0.5) / 2.0, 1.0 / 3.0];
        for (got, want) in r.iter().zip(want) {
            assert!((got - want).abs() < 1e-12, "{r:?}");
        }
        assert_eq!(fr3(&[0.9, 0.1], &[None, None]), ranks(&[0.9, 0.1]), "classifier off: d3 alone");
    }

    #[test]
    fn fit_is_c_one_with_only_corrections_positive() {
        let (x, y) = overlapping();
        let labels: Vec<String> = y.iter().enumerate().map(|(i, &pos)| match (pos, i % 2) {
            (true, _) => "correction", (false, 0) => "borderline", (false, _) => "not_friction",
        }.to_string()).collect();
        let (m, want) = (fit(&x, &labels).unwrap(), train(&x, &y, 1.0).unwrap());
        for row in x.iter().take(6) {
            assert_eq!(m.predict(row), want.predict(row));
        }
        let no_correction: Vec<String> = labels.iter().map(|l| if l == "correction" { "borderline".to_string() } else { l.clone() }).collect();
        assert!(fit(&x, &no_correction).is_none(), "borderline is not a positive");
    }

    #[test]
    fn the_lock_creates_its_folder_and_excludes_a_second_holder() {
        let path = std::env::temp_dir().join(format!("gopola-lock-{}/nested/db.friction.lock", std::process::id()));
        let first = take_lock(&path).unwrap().expect("a free lock is taken");
        assert!(take_lock(&path).unwrap().is_none(), "a held lock is refused, not waited for");
        drop(first);
        assert!(take_lock(&path).unwrap().is_some(), "and free again once its holder is gone");
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn doctor_without_a_pressure_source_counts_the_sessions_skipped() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("create table usage (ts text not null, cmd text not null, arg text not null, hits integer not null)").unwrap();
        for (ago_secs, arg) in [(0, "aaaaaaaa: friction scoring runs on macOS only"), (60, "aaaaaaaa: friction scoring runs on macOS only"),
                                (60, "bbbbbbbb: friction scoring runs on macOS only"), (31 * DAY, "cccccccc: friction scoring runs on macOS only"),
                                // a window is not a session
                                (60, "since 2026-11-01 demo-app: friction scoring runs on macOS only")] {
            c.execute("insert into usage values (?1, 'friction-skipped', ?2, 0)", params![secs_ago(ago_secs), arg]).unwrap();
        }
        assert_eq!(doctor_line(&c, false).unwrap(), ("info", "friction: scoring runs on macOS only (2 sessions skipped in 30 days)".to_string()));
    }

    #[test]
    fn top_found_rounds_a_half_to_even() {
        // 25 rows at 10 per 100 is k = 2.5: the measured Python round rounds it to 2, not 3
        let pairs: Vec<(f64, bool)> = (0..25).map(|i| (1.0 - i as f64 / 100.0, i == 0 || i == 2)).collect();
        assert_eq!(top_found(&pairs, 10.0), (1.0, 2));
    }
}
