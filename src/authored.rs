//! `authored` and `blame`: which commits on a git ref came from Claude Code sessions, and which of their
//! lines survive. git cannot tell: the owner commits by hand and from sessions under one identity.
//!
//! A session's commits are the ones gopola already knows: `git log --all` in the launch directory
//! between the session's first and last timestamp (the `commits` table, written at index time). The
//! same rule runs again now for sessions in this repository, because the table can be stale. A commit
//! counts only when its author is the identity the sessions commit as (teammates' fetched commits fall
//! inside session windows too). Each recorded commit is then followed onto the ref through rewritten
//! history: the exact sha, else an equal `git patch-id --stable`, else the same subject (subject-only).

use crate::cmds::{Opts, Res, short_id};
use crate::transcript::clip;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How a recorded commit was found on the ref, weakest first, so `max` keeps the strongest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Match {
    Subject,
    PatchId,
    Sha,
}

impl Match {
    pub fn label(self) -> &'static str {
        match self {
            Match::Sha => "sha",
            Match::PatchId => "patch-id",
            Match::Subject => "subject-only",
        }
    }
}

struct Session {
    id: String,
    start: i64,
    end: i64,
    same_repo: bool,
}

struct Reachable {
    ct: i64,
    date: String,
    email: String,
    name: String,
    merge: bool,
    subject: String,
}

pub struct Hit {
    pub sessions: BTreeSet<String>,
    pub kind: Match,
}

#[derive(Default)]
pub struct Excluded {
    pub merges: usize,
    pub bumps: usize,
}

pub struct Attribution {
    pub toplevel: PathBuf,
    pub project: String,
    pub refname: String,
    pub identity: String,
    pub sessions: usize,
    /// Reachable sha -> the sessions whose recorded commit it is.
    pub hits: HashMap<String, Hit>,
    reach: HashMap<String, Reachable>,
    /// Recorded commits left out because another person wrote them, by author name.
    pub other_authors: BTreeMap<String, usize>,
    /// Those commits: sha, author, subject.
    pub others: Vec<(String, String, String)>,
    /// Recorded commits by the identity that are not on the ref by sha, patch-id or subject.
    pub not_on_ref: usize,
    /// Those commits: sha, subject, sessions (for --json, to audit the match).
    pub missed: Vec<(String, String, BTreeSet<String>)>,
    pub excluded: Excluded,
}

type Error = Box<dyn std::error::Error>;

fn git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git <args>` with `input` on stdin. The input is written from a thread, so a child that fills its
/// stdout pipe before it has read all of stdin cannot deadlock against us.
fn git_stdin(dir: &Path, args: &[&str], input: String) -> Result<String, Error> {
    let mut child = Command::new("git").arg("-C").arg(dir).args(args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().ok_or("git: no stdin")?;
    let writer = std::thread::spawn(move || { let _ = stdin.write_all(input.as_bytes()); });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git patch-id --stable` of each listed commit: patch-id -> shas. A merge has no patch and no id.
fn patch_ids(dir: &Path, shas: &[String]) -> Result<HashMap<String, Vec<String>>, Error> {
    let mut by_pid: HashMap<String, Vec<String>> = HashMap::new();
    if shas.is_empty() {
        return Ok(by_pid);
    }
    let mut log = Command::new("git").arg("-C").arg(dir)
        .args(["log", "--no-walk=unsorted", "--stdin", "-p", "--no-color", "--no-ext-diff", "--no-textconv",
               "--diff-merges=off", "--format=commit %H"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let log_out = log.stdout.take().ok_or("git log: no stdout")?;
    let mut pid = Command::new("git").arg("-C").arg(dir).args(["patch-id", "--stable"])
        .stdin(Stdio::from(log_out)).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdin = log.stdin.take().ok_or("git log: no stdin")?;
    let input = shas.join("\n") + "\n";
    let writer = std::thread::spawn(move || { let _ = stdin.write_all(input.as_bytes()); });
    let mut text = String::new();
    pid.stdout.take().ok_or("git patch-id: no stdout")?.read_to_string(&mut text)?;
    let _ = writer.join();
    let _ = log.wait();
    let _ = pid.wait();
    for line in text.lines() {
        if let Some((p, sha)) = line.split_once(' ') {
            by_pid.entry(p.to_string()).or_default().push(sha.to_string());
        }
    }
    Ok(by_pid)
}

/// Seconds since the epoch of an ISO-8601 timestamp: `2026-10-09T15:03:44.326Z`, or with `+02:00`.
pub fn epoch(iso: &str) -> Option<i64> {
    let b = iso.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &iso[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let t = &rest[1..];
            let oh: i64 = t.get(0..2)?.parse().ok()?;
            let om: i64 = t.get(t.len().saturating_sub(2)..)?.parse().ok()?;
            let o = oh * 3600 + om * 60;
            if *sign == b'+' { o } else { -o }
        }
        _ => return None,
    };
    // days-from-civil (Howard Hinnant), the inverse of cmds::iso_from_secs.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - offset)
}

/// A version bump, for `--no-bumps`: "chore: update version to 2.1.0", "Bump version to 1.2+3".
pub fn is_bump(subject: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\b(bump|bumps|bumped|update|updates|updated|set)\b.*\bversion\b|\bversion bump\b").unwrap())
        .is_match(subject)
}

fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The ref to read when none is given: the remote's default branch, else HEAD.
pub fn default_ref(dir: &Path) -> String {
    git(dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "origin/HEAD")
        .unwrap_or_else(|| "HEAD".into())
}

/// Is a session's launch directory this repository (its toplevel, a subdirectory, a `wt` task worktree,
/// or any worktree that shares its git directory)? Only those sessions get the live re-run of the rule;
/// a session in another clone keeps what gopola recorded from that clone.
fn same_repo(launch: &Path, top: &Path, common: &Path) -> bool {
    let launch = real(launch);
    if launch.starts_with(top) {
        return true;
    }
    if launch.is_dir() {
        return git(&launch, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(|c| real(Path::new(c.trim())) == common).unwrap_or(false);
    }
    // A task worktree that is gone: <parent>/<repo>-wt/<task>.
    let wt = top.file_name().map(|n| format!("{}-wt", n.to_string_lossy()));
    launch.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()) == wt
        && launch.parent().and_then(|p| p.parent()) == top.parent()
}

pub fn attribute(c: &Connection, start: &Path, refname: Option<&str>, since: Option<&str>, author: Option<&str>,
                 skip_merges: bool, skip_bumps: bool) -> Result<Attribution, Error> {
    let top = real(Path::new(git(start, &["rev-parse", "--show-toplevel"])?.trim()));
    let common = real(Path::new(git(&top, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?.trim()));
    let project = crate::index::project_for_dir(c, &top);
    let refname = refname.map(str::to_string).unwrap_or_else(|| default_ref(&top));
    git(&top, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")])
        .map_err(|_| format!("not a commit in {}: {refname}", top.display()))?;
    let identity = match author {
        Some(a) => a.to_string(),
        None => git(&top, &["config", "user.email"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
            .ok_or("this repo has no user.email: pass --author <email>")?,
    };

    // The project's sessions, as gopola filed them.
    let since = since.unwrap_or("");
    let mut sessions: Vec<Session> = Vec::new();
    let mut launch_ok: HashMap<String, bool> = HashMap::new();
    let mut st = c.prepare(
        "select id, launch_dir, started_at, ended_at from sessions
         where project = ?1 and started_at >= ?2 and ended_at is not null order by started_at")?;
    let rows: Vec<(String, String, String, String)> =
        st.query_map(params![project, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.flatten().collect();
    let mut window: Option<(String, String)> = None;
    for (id, launch, s, e) in rows {
        let (Some(start), Some(end)) = (epoch(&s), epoch(&e)) else { continue };
        let same = *launch_ok.entry(launch.clone()).or_insert_with(|| same_repo(Path::new(&launch), &top, &common));
        if same {
            window = Some(match window {
                None => (s.clone(), e.clone()),
                Some((a, b)) => (if s < a { s.clone() } else { a }, if e > b { e.clone() } else { b }),
            });
        }
        sessions.push(Session { id, start, end, same_repo: same });
    }
    let by_id: HashMap<&str, &Session> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();

    // Recorded commits: candidate sha -> (sessions, subject).
    let mut cand: BTreeMap<String, (BTreeSet<String>, String)> = BTreeMap::new();
    let mut st = c.prepare(
        "select k.session_id, k.sha, k.subject from commits k join sessions s on s.id = k.session_id
         where s.project = ?1 and s.started_at >= ?2")?;
    for row in st.query_map(params![project, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
        let (sid, sha, subject) = row?;
        if by_id.contains_key(sid.as_str()) {
            let e = cand.entry(sha).or_insert_with(|| (BTreeSet::new(), subject));
            e.0.insert(sid);
        }
    }
    // The same rule, now, for sessions in this repository.
    if let Some((s, e)) = &window {
        let out = git(&top, &["log", "--all", "--format=%H%x09%ct%x09%s", &format!("--since={s}"), &format!("--until={e}")])?;
        for line in out.lines() {
            let mut it = line.splitn(3, '\t');
            let (Some(sha), Some(ct), Some(subject)) = (it.next(), it.next(), it.next()) else { continue };
            let Ok(ct) = ct.parse::<i64>() else { continue };
            for s in sessions.iter().filter(|s| s.same_repo && s.start <= ct && ct <= s.end) {
                let e = cand.entry(sha.to_string()).or_insert_with(|| (BTreeSet::new(), subject.to_string()));
                e.0.insert(s.id.clone());
            }
        }
    }

    // Everything reachable from the ref.
    let mut reach: HashMap<String, Reachable> = HashMap::new();
    let mut by_subject: HashMap<String, Vec<String>> = HashMap::new();
    let out = git(&top, &["log", &refname, "--format=%H%x09%ct%x09%as%x09%ae%x09%an%x09%P%x09%s"])?;
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(7, '\t').collect();
        if f.len() < 7 { continue }
        let Ok(ct) = f[1].parse::<i64>() else { continue };
        by_subject.entry(f[6].to_string()).or_default().push(f[0].to_string());
        reach.insert(f[0].to_string(), Reachable { ct, date: f[2].into(), email: f[3].into(), name: f[4].into(),
            merge: f[5].split_whitespace().count() > 1, subject: f[6].into() });
    }

    // Authors of the recorded commits whose objects are here.
    let missing: Vec<String> = cand.keys().filter(|s| !reach.contains_key(*s)).cloned().collect();
    let mut cand_author: HashMap<String, (String, String)> = HashMap::new();
    if !missing.is_empty() {
        let present = git_stdin(&top, &["cat-file", "--batch-check=%(objectname) %(objecttype)"], missing.join("\n") + "\n")?;
        let present: Vec<String> = present.lines().filter_map(|l| l.strip_suffix(" commit")).map(str::to_string).collect();
        if !present.is_empty() {
            let out = git_stdin(&top, &["log", "--no-walk=unsorted", "--stdin", "--format=%H%x09%ae%x09%an"], present.join("\n") + "\n")?;
            for l in out.lines() {
                let f: Vec<&str> = l.splitn(3, '\t').collect();
                if f.len() == 3 { cand_author.insert(f[0].into(), (f[1].into(), f[2].into())); }
            }
        }
    }

    // Patch-ids: the recorded commits not on the ref, and the ref's commits since the first session.
    let first = sessions.iter().map(|s| s.start).min().unwrap_or(i64::MAX);
    let pid_cands: Vec<String> = cand_author.keys().cloned().collect();
    let cand_pid: HashMap<String, String> = patch_ids(&top, &pid_cands)?.into_iter()
        .flat_map(|(p, shas)| shas.into_iter().map(move |s| (s, p.clone()))).collect();
    let recent: Vec<String> = reach.iter().filter(|(_, r)| r.ct >= first && !r.merge).map(|(s, _)| s.clone()).collect();
    let reach_pid = if cand_pid.is_empty() { HashMap::new() } else { patch_ids(&top, &recent)? };

    let mut a = Attribution {
        toplevel: top.clone(), project, refname, identity: identity.clone(), sessions: 0, hits: HashMap::new(),
        reach: HashMap::new(), other_authors: BTreeMap::new(), others: Vec::new(), not_on_ref: 0, missed: Vec::new(), excluded: Excluded::default(),
    };
    let mut seen_excluded: BTreeSet<String> = BTreeSet::new();
    for (sha, (sids, subject)) in &cand {
        let since_ct = sids.iter().filter_map(|s| by_id.get(s.as_str())).map(|s| s.start).min().unwrap_or(first);
        let after = |s: &String| reach.get(s).map(|r| r.ct >= since_ct).unwrap_or(false);
        let found: Vec<(String, Match)> = if reach.contains_key(sha) {
            vec![(sha.clone(), Match::Sha)]
        } else if let Some(v) = cand_pid.get(sha).and_then(|p| reach_pid.get(p)).filter(|v| v.iter().any(after)) {
            v.iter().filter(|s| after(s)).map(|s| (s.clone(), Match::PatchId)).collect()
        } else if let Some(s) = by_subject.get(subject)
            .and_then(|v| v.iter().filter(|s| after(s)).min_by_key(|s| reach[*s].ct)) {
            vec![(s.clone(), Match::Subject)]
        } else {
            Vec::new()
        };
        // Another person's commit is not a session commit, whoever's window it fell in.
        let other = cand_author.get(sha).filter(|(e, _)| *e != identity).map(|(_, n)| n.clone())
            .or_else(|| found.iter().filter_map(|(s, _)| reach.get(s)).find(|r| r.email != identity).map(|r| r.name.clone()));
        if let Some(name) = other {
            *a.other_authors.entry(name.clone()).or_default() += 1;
            a.others.push((sha.clone(), name, subject.clone()));
            continue;
        }
        if found.is_empty() {
            a.not_on_ref += 1;
            a.missed.push((sha.clone(), subject.clone(), sids.clone()));
            continue;
        }
        for (rs, kind) in found {
            let r = &reach[&rs];
            if (skip_merges && r.merge) || (skip_bumps && is_bump(&r.subject)) {
                if seen_excluded.insert(rs.clone()) {
                    if skip_merges && r.merge { a.excluded.merges += 1 } else { a.excluded.bumps += 1 }
                }
                continue;
            }
            let h = a.hits.entry(rs).or_insert_with(|| Hit { sessions: BTreeSet::new(), kind });
            h.kind = h.kind.max(kind);
            h.sessions.extend(sids.iter().cloned());
        }
    }
    a.sessions = a.hits.values().flat_map(|h| h.sessions.iter()).collect::<BTreeSet<_>>().len();
    for sha in a.hits.keys() {
        if let Some(r) = reach.remove(sha) {
            a.reach.insert(sha.clone(), r);
        }
    }
    Ok(a)
}

fn attribution_line(a: &Attribution) -> String {
    let mut parts = Vec::new();
    if !a.other_authors.is_empty() {
        let names: Vec<String> = a.other_authors.iter().map(|(n, k)| format!("{n} {k}")).collect();
        parts.push(format!("{} recorded by other authors ({})", a.other_authors.values().sum::<usize>(), names.join(", ")));
    }
    if a.not_on_ref > 0 {
        parts.push(format!("{} recorded not on {}", a.not_on_ref, a.refname));
    }
    if a.excluded.merges > 0 { parts.push(format!("{} merges (--no-merges)", a.excluded.merges)) }
    if a.excluded.bumps > 0 { parts.push(format!("{} version bumps (--no-bumps)", a.excluded.bumps)) }
    if parts.is_empty() { String::new() } else { format!("not counted: {}", parts.join(" · ")) }
}

fn attribution_json(a: &Attribution) -> Value {
    let missed: Vec<Value> = a.missed.iter().map(|(s, sub, ids)| json!({"sha": s, "subject": sub, "sessions": ids})).collect();
    let others: Vec<Value> = a.others.iter().map(|(s, n, sub)| json!({"sha": s, "author": n, "subject": sub})).collect();
    json!({"other_authors": a.other_authors, "other_author_commits": others, "not_on_ref": a.not_on_ref, "not_on_ref_commits": missed,
           "excluded": {"merges": a.excluded.merges, "bumps": a.excluded.bumps}})
}

fn short_sessions(s: &BTreeSet<String>) -> String {
    s.iter().map(|x| short_id(x).to_string()).collect::<Vec<_>>().join(",")
}

fn parent_dir(file: &str) -> String {
    file.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into())
}

/// `authored <repo>`: the ref's commits that came from Claude Code sessions, with their lines under --path.
pub fn authored(o: &Opts, repo: Option<&str>) -> Res {
    let c = crate::db::open()?;
    let dir = PathBuf::from(repo.unwrap_or("."));
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()).into());
    }
    let a = attribute(&c, &dir, o.git_ref.as_deref(), o.since.as_deref(), o.author.as_deref(), o.no_merges, o.no_bumps)?;
    let path = o.path.as_deref().map(|p| p.trim_end_matches('/').to_string());

    // Lines and files under --path, per commit; a commit that adds or deletes no line there (a merge has
    // no lines of its own) is left out and counted.
    let shas: Vec<String> = a.hits.keys().cloned().collect();
    let mut stats: HashMap<String, Vec<(i64, i64, String)>> = HashMap::new();
    if !shas.is_empty() {
        let mut args = vec!["log", "--no-walk=unsorted", "--stdin", "--numstat", "--no-renames", "--format=@%H"];
        if let Some(p) = &path { args.extend(["--", p.as_str()]); }
        let out = git_stdin(&a.toplevel, &args, shas.join("\n") + "\n")?;
        let mut cur = String::new();
        for l in out.lines() {
            if let Some(s) = l.strip_prefix('@') {
                cur = s.to_string();
                stats.entry(cur.clone()).or_default();
            } else if !l.is_empty() {
                let f: Vec<&str> = l.splitn(3, '\t').collect();
                if f.len() == 3 {
                    stats.entry(cur.clone()).or_default().push((f[0].parse().unwrap_or(0), f[1].parse().unwrap_or(0), f[2].to_string()));
                }
            }
        }
    }
    let mut rows: Vec<(&String, &Hit, &Reachable, i64, i64)> = Vec::new();
    let mut outside = 0usize;
    let (mut files, mut dirs): (BTreeSet<String>, HashMap<String, (i64, i64)>) = (BTreeSet::new(), HashMap::new());
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut sessions: BTreeSet<&String> = BTreeSet::new();
    for (sha, hit) in &a.hits {
        let r = &a.reach[sha];
        let st = stats.get(sha);
        if path.is_some() && st.is_none_or(|v| v.is_empty()) {
            outside += 1;
            continue;
        }
        let (mut add, mut del) = (0, 0);
        for (ad, de, f) in st.into_iter().flatten() {
            add += ad;
            del += de;
            files.insert(f.clone());
            let d = dirs.entry(parent_dir(f)).or_default();
            d.0 += ad;
            d.1 += de;
        }
        *kinds.entry(hit.kind.label()).or_default() += 1;
        sessions.extend(hit.sessions.iter());
        rows.push((sha, hit, r, add, del));
    }
    rows.sort_by(|x, y| y.2.date.cmp(&x.2.date).then(y.2.ct.cmp(&x.2.ct)).then(x.0.cmp(y.0)));
    let (added, deleted) = rows.iter().fold((0, 0), |(a, d), r| (a + r.3, d + r.4));
    let mut top: Vec<(String, (i64, i64))> = dirs.into_iter().collect();
    top.sort_by(|x, y| y.1.0.cmp(&x.1.0).then(x.0.cmp(&y.0)));
    crate::db::log_usage(&c, "authored", &format!("{} {}", a.project, a.refname), rows.len() as i64);

    if o.json {
        let commits: Vec<Value> = rows.iter().map(|(sha, h, r, add, del)| json!({
            "sha": sha, "date": r.date, "sessions": h.sessions, "match": h.kind.label(), "subject": r.subject,
            "added": add, "deleted": del, "merge": r.merge})).collect();
        let top_dirs: Vec<Value> = top.iter().take(10).map(|(d, (ad, de))| json!({"dir": d, "added": ad, "deleted": de})).collect();
        println!("{}", json!({"repo": a.toplevel, "project": a.project, "ref": a.refname, "identity": a.identity,
            "since": o.since, "path": path, "commits": commits,
            "summary": {"commits": rows.len(), "sessions": sessions.len(), "added": added, "deleted": deleted,
                        "files": files.len(), "top_dirs": top_dirs, "match": kinds, "outside_path": outside},
            "not_counted": attribution_json(&a)}));
        return Ok(());
    }
    let under = path.as_deref().map(|p| format!("  under {p}/")).unwrap_or_default();
    let since = o.since.as_deref().map(|s| format!("  since {s}")).unwrap_or_default();
    println!("# authored  {}  {}{since}{under}  (sessions commit as {})", a.project, a.refname, a.identity);
    for (sha, h, r, add, del) in &rows {
        println!("{}  {}  {:<17}  {:<12} {:>6} {:>6}  {}", r.date, &sha[..9], clip(&short_sessions(&h.sessions), 17),
                 h.kind.label(), format!("+{add}"), format!("-{del}"), clip(&r.subject, 80));
    }
    let kinds_s: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
    println!("{} commits from {} sessions · +{added} -{deleted} lines in {} files{} · match: {}",
             rows.len(), sessions.len(), files.len(), path.as_deref().map(|p| format!(" under {p}/")).unwrap_or_default(),
             if kinds_s.is_empty() { "none".into() } else { kinds_s.join(", ") });
    if !top.is_empty() {
        let t: Vec<String> = top.iter().take(5).map(|(d, (ad, _))| format!("{d} +{ad}")).collect();
        println!("top directories (lines added): {}", t.join(" · "));
    }
    if outside > 0 {
        println!("{outside} more session commits change no line under {}/", path.as_deref().unwrap_or(""));
    }
    let n = attribution_line(&a);
    if !n.is_empty() { println!("{n}"); }
    Ok(())
}

/// One surviving line: the commit that last changed it, and its text.
pub struct Line {
    pub sha: String,
    pub text: String,
}

/// Parse `git blame --line-porcelain`: every line has a header (`<sha> <orig> <final> [<n>]`), its
/// key-value fields, then the content after a tab.
pub fn parse_porcelain(out: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut sha = String::new();
    for l in out.split('\n') {
        if let Some(text) = l.strip_prefix('\t') {
            lines.push(Line { sha: sha.clone(), text: text.to_string() });
        } else if l.len() > 41 && l.as_bytes()[40] == b' ' && l[..40].bytes().all(|b| b.is_ascii_hexdigit()) {
            sha = l[..40].to_string();
        }
    }
    lines
}

fn blame_file(top: &Path, refname: &str, file: &str, raw: bool) -> Result<Vec<Line>, String> {
    let w: &[&str] = if raw { &[] } else { &["-w"] };
    let out = Command::new("git").arg("-C").arg(top).args(["blame", "--line-porcelain"]).args(w).args([refname, "--", file])
        .output().map_err(|e| format!("git blame: {e}"))?;
    if !out.status.success() {
        return Err(format!("git blame {file}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// `blame <file|dir>`: each surviving line on the ref, marked with the Claude Code session whose commit
/// wrote it; `--summary` prints per file how many lines that is.
pub fn blame(o: &Opts, target: Option<&str>) -> Res {
    let target = target.ok_or("blame needs a file or directory")?;
    let c = crate::db::open()?;
    let cwd = std::env::current_dir()?;
    let abs = if Path::new(target).is_absolute() { PathBuf::from(target) } else { cwd.join(target) };
    // The path may exist on the ref and not on disk: start git from the nearest directory that exists.
    let mut base = abs.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !base.is_dir() {
        if let Some(n) = base.file_name() { tail.push(n.to_os_string()) }
        if !base.pop() { return Err(format!("no such path: {target}").into()) }
    }
    let a = attribute(&c, &base, o.git_ref.as_deref(), o.since.as_deref(), o.author.as_deref(), o.no_merges, o.no_bumps)?;
    let mut rel = real(&base).strip_prefix(&a.toplevel).map(Path::to_path_buf)
        .map_err(|_| format!("{} is not inside {}", abs.display(), a.toplevel.display()))?;
    for t in tail.iter().rev() { rel.push(t) }
    let rel_s = rel.to_string_lossy().to_string();
    let spec = if rel_s.is_empty() { ".".to_string() } else { rel_s.clone() };
    let listed = git(&a.toplevel, &["ls-tree", "-r", "--name-only", "-z", &a.refname, "--", &spec])?;
    let files: Vec<String> = listed.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();
    if files.is_empty() {
        return Err(format!("not on {}: {spec}", a.refname).into());
    }

    // One git blame per file, on as many threads as the machine has.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(files.len());
    let mut results: Vec<(usize, Result<Vec<Line>, String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| {
            let mut mine = Vec::new();
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= files.len() { break }
                mine.push((i, blame_file(&a.toplevel, &a.refname, &files[i], o.no_ignore_whitespace)));
            }
            mine
        })).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    results.sort_by_key(|r| r.0);

    let mut per_file: Vec<(String, usize, usize, Vec<Line>)> = Vec::new();
    for (i, r) in results {
        let lines = r?;
        let claude = lines.iter().filter(|l| a.hits.contains_key(&l.sha)).count();
        per_file.push((files[i].clone(), lines.len(), claude, lines));
    }
    let total: usize = per_file.iter().map(|f| f.1).sum();
    let claude: usize = per_file.iter().map(|f| f.2).sum();
    let pct = |c: usize, t: usize| if t == 0 { 0.0 } else { 100.0 * c as f64 / t as f64 };
    crate::db::log_usage(&c, "blame", &format!("{} {spec}", a.refname), claude as i64);

    if o.summary {
        per_file.sort_by(|x, y| y.2.cmp(&x.2).then(y.1.cmp(&x.1)).then(x.0.cmp(&y.0)));
        let with = per_file.iter().filter(|f| f.2 > 0).count();
        if o.json {
            let fs: Vec<Value> = per_file.iter().map(|(f, t, c, _)| json!({"file": f, "total": t, "claude": c,
                "pct": (pct(*c, *t) * 10.0).round() / 10.0})).collect();
            println!("{}", json!({"repo": a.toplevel, "ref": a.refname, "identity": a.identity, "path": spec,
                "files": fs, "total": total, "claude": claude, "session_commits": a.hits.len(),
                "not_counted": attribution_json(&a)}));
            return Ok(());
        }
        println!("# blame {spec} @ {}: {claude} of {total} lines from Claude sessions ({:.1}%) in {with} of {} files",
                 a.refname, pct(claude, total), per_file.len());
        println!("{:>7} {:>7} {:>6}  file", "claude", "total", "%");
        for (f, t, c, _) in per_file.iter().filter(|f| f.2 > 0) {
            println!("{c:>7} {t:>7} {:>6.1}  {f}", pct(*c, *t));
        }
        let rest: Vec<&(String, usize, usize, Vec<Line>)> = per_file.iter().filter(|f| f.2 == 0).collect();
        if !rest.is_empty() {
            println!("{} files with no lines from Claude sessions ({} lines)", rest.len(), rest.iter().map(|f| f.1).sum::<usize>());
        }
        println!("{} session commits on {} (sessions commit as {})", a.hits.len(), a.refname, a.identity);
        let n = attribution_line(&a);
        if !n.is_empty() { println!("{n}"); }
        return Ok(());
    }
    if o.json {
        let fs: Vec<Value> = per_file.iter().map(|(f, t, c, lines)| {
            let ls: Vec<Value> = lines.iter().enumerate().map(|(i, l)| json!({"n": i + 1, "sha": l.sha,
                "sessions": a.hits.get(&l.sha).map(|h| h.sessions.iter().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                "text": l.text})).collect();
            json!({"file": f, "total": t, "claude": c, "lines": ls})
        }).collect();
        println!("{}", json!({"repo": a.toplevel, "ref": a.refname, "identity": a.identity, "files": fs,
            "total": total, "claude": claude, "not_counted": attribution_json(&a)}));
        return Ok(());
    }
    for (f, t, c, lines) in &per_file {
        println!("# {f} @ {}: {c} of {t} lines from Claude sessions ({:.1}%)", a.refname, pct(*c, *t));
        let who = |l: &Line| a.hits.get(&l.sha).map(|h| {
            let first = short_id(h.sessions.iter().next().map(String::as_str).unwrap_or("")).to_string();
            if h.sessions.len() > 1 { format!("{first}+{}", h.sessions.len() - 1) } else { first }
        }).unwrap_or_else(|| "-".into());
        if o.ranges {
            let mut by_session: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for (i, l) in lines.iter().enumerate().filter(|(_, l)| a.hits.contains_key(&l.sha)) {
                by_session.entry(who(l)).or_default().push(i + 1);
            }
            for (s, ns) in by_session { println!("{s}  {}", fold_ranges(&ns)) }
            continue;
        }
        for (i, l) in lines.iter().enumerate() {
            println!("{:>5} {} {:<10} {}", i + 1, &l.sha[..9.min(l.sha.len())], who(l), l.text);
        }
    }
    Ok(())
}

/// `[11, 12, 14, 15, 16]` as `11-12 14-16`.
fn fold_ranges(ns: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ns.len() {
        let mut j = i;
        while j + 1 < ns.len() && ns[j + 1] == ns[j] + 1 { j += 1 }
        out.push(if j == i { ns[i].to_string() } else { format!("{}-{}", ns[i], ns[j]) });
        i = j + 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_reads_transcript_and_git_timestamps() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-10-09T15:03:44.326Z"), Some(1_791_558_224));
        assert_eq!(epoch("2026-10-09T17:03:44+02:00"), epoch("2026-10-09T15:03:44Z"));
        assert_eq!(epoch("2026-03-01T00:00:00Z").unwrap() - epoch("2026-02-28T00:00:00Z").unwrap(), 86_400);
        assert_eq!(epoch("2024-03-01T00:00:00Z").unwrap() - epoch("2024-02-28T00:00:00Z").unwrap(), 2 * 86_400);
        assert_eq!(epoch("not a date"), None);
        assert_eq!(crate::cmds::iso_from_secs(epoch("2026-10-14T16:00:14Z").unwrap() as u64), "2026-10-14T16:00:14Z");
    }

    #[test]
    fn bumps_are_version_subjects_only() {
        assert!(is_bump("chore: update version to 2.1.0+5 in pubspec.yaml"));
        assert!(is_bump("Bump version to 1.2.3"));
        assert!(is_bump("chore: version bump"));
        assert!(!is_bump("feat: show the app version on the about page"));
        assert!(!is_bump("fix: update the store list"));
    }

    #[test]
    fn porcelain_gives_every_line_its_commit() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = format!(
            "{a} 1 1 2\nauthor x\nsummary s\nfilename f\n\tfirst\n{a} 2 2\nauthor x\nfilename f\n\t\tindented\n\
             {b} 1 3 1\nauthor y\nsummary {a} looks like a sha\nboundary\nfilename f\n\t\n");
        let lines = parse_porcelain(&out);
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].sha.as_str(), lines[0].text.as_str()), (a.as_str(), "first"));
        assert_eq!((lines[1].sha.as_str(), lines[1].text.as_str()), (a.as_str(), "\tindented"));
        assert_eq!((lines[2].sha.as_str(), lines[2].text.as_str()), (b.as_str(), ""));
    }

    #[test]
    fn strongest_match_wins() {
        assert_eq!(Match::Subject.max(Match::Sha), Match::Sha);
        assert_eq!(Match::PatchId.max(Match::Subject), Match::PatchId);
    }
}
