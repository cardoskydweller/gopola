//! Incremental indexing of transcripts into the database.

use crate::transcript::{Event, Parsed, classify, is_throwaway_prompt};
use rusqlite::{Connection, params};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Report {
    pub files_seen: usize,
    pub files_indexed: usize,
    pub turns_written: usize,
    pub lines_skipped: usize,
}

pub fn projects_root() -> PathBuf {
    if let Ok(p) = std::env::var("GOPOLA_PROJECTS") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude/projects")
}

/// Copilot CLI's session folders, by Copilot's own rule: `$COPILOT_HOME/session-state`, else `~/.copilot/session-state`.
pub fn copilot_root() -> PathBuf {
    match std::env::var("COPILOT_HOME") {
        Ok(p) if !p.is_empty() => PathBuf::from(p).join("session-state"),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".copilot/session-state"),
    }
}

/// Codex CLI's home, by Codex's own rule: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    match std::env::var("CODEX_HOME") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".codex"),
    }
}

/// Codex's rollouts as (path without `.zst`, id), one per id, `sessions/` before `archived_sessions/`, at any depth,
/// then those Codex deleted that gopola's archive still has (as their original paths). Among gopola's copies of one id
/// the newest comes first: each copy carries its source's modification time, and a rollout only grows, while a session
/// moves both ways (`codex archive`, `codex unarchive`).
/// The id is the last 36 characters of `rollout-<local time>-<id>.jsonl` before `.jsonl`.
pub fn codex_rollouts() -> Vec<(PathBuf, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (root, newest_first) in [(codex_home(), false), (crate::archive::codex_copies(), true)] {
        let mut files = Vec::new();
        for dir in ["sessions", "archived_sessions"] {
            let mut in_dir = Vec::new();
            crate::archive::walk(&root.join(dir), &mut in_dir);
            in_dir.sort();
            files.append(&mut in_dir);
        }
        if newest_first {
            files.sort_by_key(|f| std::cmp::Reverse(std::fs::metadata(f).and_then(|m| m.modified()).ok()));
        }
        for f in files {
            let name = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".zst").unwrap_or(&name).strip_suffix(".jsonl") else { continue };
            let Some(id) = stem.strip_prefix("rollout-").and_then(|s| s.get(s.len().checked_sub(36)?..)) else { continue };
            if seen.insert(id.to_string()) {
                let original = codex_home().join(f.strip_prefix(&root).unwrap_or(&f));
                out.push((original.with_file_name(format!("{stem}.jsonl")), id.to_string()));
            }
        }
    }
    out
}

/// Where a Copilot or Codex session started: its row's launch dir once indexed, else its first record's `cwd` read as
/// if its checkout were where `gopola moved` says it went (the old path, a path under it, or a `-wt` task worktree
/// beside it).
pub fn launch_dir(c: &Connection, id: &str, file: &Path, moves: &[(String, String)], classify: fn(&str) -> Parsed) -> Option<PathBuf> {
    if let Ok(dir) = c.query_row("select launch_dir from sessions where id = ?1", [id], |r| r.get::<_, String>(0)) {
        return Some(dir.into());
    }
    let mut first = String::new();
    crate::archive::open(file).ok()?.read_line(&mut first).ok()?;
    let cwd = match classify(&first) {
        Parsed::Event(Event::Start { cwd: Some(cwd), .. }) => cwd,
        // A Codex subagent's first record is reported, not read, but it still says where the subagent ran.
        Parsed::Unknown(t) if t == "subagent.session_meta" =>
            serde_json::from_str::<serde_json::Value>(&first).ok()?["payload"]["cwd"].as_str()?.to_string(),
        _ => return None,
    };
    Some(moves.iter().find_map(|(old, new)| {
        let rest = cwd.strip_prefix(old.as_str())?;
        (rest.is_empty() || rest.starts_with('/') || rest.starts_with("-wt/")).then(|| format!("{new}{rest}"))
    }).unwrap_or(cwd).into())
}

/// Transcripts that `gopola import` copied in (cloud sessions), next to the database, in the same
/// `<encoded launch dir>/<session>.jsonl` layout as `projects_root`.
pub fn imports_root() -> PathBuf {
    crate::db::db_path().with_file_name("imports")
}

/// Decode a transcript directory name (`-Users-x-repos-demo-app`) into the launch path.
/// Claude Code replaces every non-alphanumeric character of the path with `-`, so a segment
/// may stand for `_`, `.` or `-` in the real name. At each step the longest run of segments
/// that matches an existing child directory (compared after the same replacement) wins;
/// when none matches, one segment is taken as-is.
pub fn decode_dir_name(name: &str) -> PathBuf {
    let parts: Vec<&str> = name.trim_start_matches('-').split('-').collect();
    let mut path = PathBuf::from("/");
    let mut i = 0;
    while i < parts.len() {
        let children: Vec<String> = std::fs::read_dir(&path)
            .map(|d| d.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let mut taken = 1;
        let mut real: Option<String> = None;
        for k in (1..=parts.len() - i).rev() {
            let want = parts[i..i + k].join("-");
            if let Some(c) = children.iter().find(|c| encode(c) == want) {
                taken = k;
                real = Some(c.clone());
                break;
            }
        }
        path = path.join(real.unwrap_or_else(|| parts[i..i + taken].join("-")));
        i += taken;
    }
    path
}

pub fn encode(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// A transcript folder name read as if its checkout were where `gopola moved` says it went. The old
/// path no longer exists, so it is matched encoded, before decoding; a `-wt` task worktree beside it
/// moves with it.
fn moved_name(name: &str, moves: &[(String, String)]) -> String {
    moves.iter().find(|(old, _)| name == old || name.starts_with(&format!("{old}-")))
        .map(|(old, new)| format!("{new}{}", &name[old.len()..]))
        .unwrap_or_else(|| name.to_string())
}

pub fn project_name(launch_dir: &Path) -> String {
    project_and_worktree(launch_dir).0
}

/// A session launched inside a task worktree (`<parent>/<repo>-wt/<task>`, the layout `wt` creates)
/// belongs to `<repo>`; the task name is kept as the worktree. Otherwise the directory's own name.
pub fn project_and_worktree(launch_dir: &Path) -> (String, Option<String>) {
    match worktree_repo(launch_dir) {
        Some(repo) => (own_name(&repo), Some(own_name(launch_dir))),
        None => (own_name(launch_dir), None),
    }
}

fn own_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".into())
}

/// The checkout a `wt` task worktree (`<parent>/<repo>-wt/<task>`) belongs to.
fn worktree_repo(launch_dir: &Path) -> Option<PathBuf> {
    let parent = launch_dir.parent()?;
    let repo = parent.file_name()?.to_str()?.strip_suffix("-wt")?;
    let checkout = parent.parent()?.join(repo);
    checkout.is_dir().then_some(checkout)
}

fn checkout_of(launch_dir: &Path) -> PathBuf {
    worktree_repo(launch_dir).unwrap_or_else(|| launch_dir.to_path_buf())
}

/// `<parent>/<name>`: how a second checkout of the same name is told apart.
fn qualified_name(checkout: &Path) -> String {
    let parent = checkout.parent().map(own_name).unwrap_or_else(|| "root".into());
    format!("{parent}/{}", own_name(checkout))
}

/// The project a directory's sessions are filed under. Two checkouts with one name are two projects: the
/// shallowest keeps the plain name, the other is `<parent>/<name>` (see `run`). That name is used when the
/// index holds sessions under it.
pub fn project_for_dir(c: &Connection, dir: &Path) -> String {
    let q = qualified_name(&checkout_of(dir));
    let filed = c.query_row("select 1 from sessions where project = ?1 limit 1", [&q], |_| Ok(())).is_ok();
    if filed { q } else { project_name(dir) }
}

/// `GOPOLA_EXCLUDED_PROJECTS`: comma-separated names or path fragments; a session under one is
/// recorded as seen and indexed as nothing, like an SDK session.
fn excluded(project: &str, launch_dir: &Path) -> bool {
    let Ok(list) = std::env::var("GOPOLA_EXCLUDED_PROJECTS") else { return false };
    let dir = launch_dir.to_string_lossy();
    list.split(',').map(str::trim).filter(|s| !s.is_empty())
        .any(|pat| project == pat || dir.contains(pat))
}

pub fn run(c: &mut Connection, full: bool) -> Result<Report, Box<dyn std::error::Error>> {
    crate::archive::sync_if_due();
    if full {
        // A session with no transcript and no copy left cannot be read again: a rebuild keeps its rows.
        let keep: Vec<String> = c.prepare("select id, path from sessions")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.flatten()
            .filter(|(_, p)| !crate::archive::exists(Path::new(p))).map(|(id, _)| id).collect();
        crate::db::reset_transcript_tables(c, &keep)?;
    }
    let mut report = Report { files_seen: 0, files_indexed: 0, turns_written: 0, lines_skipped: 0 };
    let moves: Vec<(String, String)> = c.prepare("select old, new from moves")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    let encoded: Vec<(String, String)> = moves.iter().map(|(old, new)| (encode(old), encode(new))).collect();
    // Imports first: an imported session keeps its row when Claude Code's own copy of it is read later.
    // `archive::list` adds the folders and transcripts Claude Code deleted that the archive still has.
    let dirs: Vec<_> = [imports_root(), projects_root()].iter().flat_map(|root| crate::archive::list(root))
        .filter(|d| crate::archive::is_dir(d))
        .map(|d| { let launch = decode_dir_name(&moved_name(&d.file_name().unwrap_or_default().to_string_lossy(), &encoded)); (d, launch) }).collect();
    // Copilot: one folder per session, named by its id, holding `events.jsonl`; a folder without one is skipped.
    let copilot: Vec<(PathBuf, String, PathBuf)> = crate::archive::list(&copilot_root()).into_iter()
        .filter(|d| crate::archive::is_dir(d))
        .filter_map(|d| {
            let (events, id) = (d.join("events.jsonl"), d.file_name()?.to_string_lossy().into_owned());
            if !crate::archive::exists(&events) { return None }
            let launch = launch_dir(c, &id, &events, &moves, crate::copilot::classify)?;
            Some((events, id, launch))
        }).collect();
    // Codex: one rollout per session, placed as Copilot's are.
    let codex: Vec<(PathBuf, String, PathBuf)> = codex_rollouts().into_iter()
        .filter_map(|(file, id)| {
            let launch = launch_dir(c, &id, &file, &moves, crate::codex::classify)?;
            Some((file, id, launch))
        }).collect();
    // Checkouts that share a name: the shallowest (then the first by path) keeps it.
    let mut by_name: std::collections::BTreeMap<String, Vec<PathBuf>> = Default::default();
    for launch in dirs.iter().map(|(_, l)| l).chain(copilot.iter().map(|(.., l)| l)).chain(codex.iter().map(|(.., l)| l)) {
        let checkout = checkout_of(launch);
        let names = by_name.entry(own_name(&checkout)).or_default();
        if !names.contains(&checkout) { names.push(checkout) }
    }
    let place = |launch_dir: &Path| {
        let (mut project, worktree) = project_and_worktree(launch_dir);
        let checkout = checkout_of(launch_dir);
        let rivals = &by_name[&own_name(&checkout)];
        if rivals.iter().any(|r| (r.components().count(), r) < (checkout.components().count(), &checkout)) {
            project = qualified_name(&checkout);
        }
        (project, worktree)
    };
    let mut count = |path: &Path, r: Result<Option<(usize, usize)>, Box<dyn std::error::Error>>| {
        report.files_seen += 1;
        match r {
            Ok(Some((turns, skipped))) => {
                report.files_indexed += 1;
                report.turns_written += turns;
                report.lines_skipped += skipped;
            }
            Ok(None) => {}
            Err(e) => eprintln!("gopola: {}: {e}", path.display()),
        }
    };
    for (dir, launch_dir) in dirs {
        let (project, worktree) = place(&launch_dir);
        for path in crate::archive::list(&dir) {
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            count(&path, index_file(c, &path, &id, "claude", classify, &project, worktree.as_deref(), &launch_dir));
        }
    }
    for (events, id, launch_dir) in copilot {
        let (project, worktree) = place(&launch_dir);
        count(&events, index_file(c, &events, &id, "copilot", crate::copilot::classify, &project, worktree.as_deref(), &launch_dir));
    }
    for (file, id, launch_dir) in codex {
        let (project, worktree) = place(&launch_dir);
        count(&file, index_file(c, &file, &id, "codex", crate::codex::classify, &project, worktree.as_deref(), &launch_dir));
    }
    codex_titles(c)?;
    if full {
        crate::db::restore_summaries(c)?;
    }
    Ok(report)
}

/// Codex keeps a session's title out of its rollout, in `session_index.jsonl`: one line per rename, the last for an id wins.
fn codex_titles(c: &Connection) -> rusqlite::Result<()> {
    let Ok(text) = std::fs::read_to_string(codex_home().join("session_index.jsonl")) else { return Ok(()) };
    let mut titles = std::collections::HashMap::new();
    for v in text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
        if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
            titles.insert(id.to_string(), name.to_string());
        }
    }
    // A session stored as nothing (an excluded project, a subagent) has no `started_at`, and gets no title either.
    let rows: Vec<(String, String, String, Option<String>)> = c
        .prepare("select id, project, started_at, title from sessions where harness = 'codex' and started_at is not null")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.flatten().collect();
    for (id, project, started, old) in rows {
        if let Some(new) = titles.get(&id).filter(|t| old.as_ref() != Some(*t)) {
            c.execute("update sessions set title = ?2 where id = ?1", params![id, new])?;
            crate::db::fts_delete(c, "title", &id)?;
            crate::db::fts_insert(c, "title", &id, &project, &started, new)?;
        }
    }
    Ok(())
}

struct OpenTurn {
    n: i64,
    ts: String,
    prompt: String,
    reply: Option<String>,
    files: Vec<String>,
    reads: Vec<String>,
    tools: std::collections::BTreeMap<String, i64>,
    commands: Vec<String>,
}

/// Returns `None` when the file was unchanged, else (turns written, lines skipped).
fn index_file(
    c: &mut Connection,
    path: &Path,
    id: &str,
    harness: &str,
    classify: fn(&str) -> Parsed,
    project: &str,
    worktree: Option<&str>,
    launch_dir: &Path,
) -> Result<Option<(usize, usize)>, Box<dyn std::error::Error>> {
    let original = path.is_file();
    let size = crate::archive::size(path).ok_or("no transcript and no archive copy")? as i64;
    let prior: Option<(i64, i64, i64, Option<String>, String)> = c
        .query_row(
            "select bytes_indexed, resume_offset, resume_turn, ended_at, path from sessions where id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok();
    // One session, two copies (an import, and the copy `claude --teleport` wrote): the import wins.
    let imported = path.starts_with(imports_root());
    // Claude Code's folder was renamed with its checkout, or Codex moved a rollout (plain or its own `.zst`) under
    // `archived_sessions/`: the row follows the transcript, once the old file is gone.
    if let Some((.., p)) = &prior && crate::archive::live(path) && !imported && **p != *path.to_string_lossy()
        && !crate::archive::live(Path::new(p)) && !Path::new(p).starts_with(imports_root()) {
        c.execute("update sessions set path = ?2 where id = ?1", params![id, path.to_string_lossy()])?;
    }
    let (offset, mut n, prior_ended) = match prior {
        Some((.., p)) if p != path.to_string_lossy() && Path::new(&p).starts_with(imports_root()) && !imported => return Ok(None),
        Some((.., p)) if p != path.to_string_lossy() && imported => {
            c.execute("delete from sessions where id = ?1", [&id])?;
            crate::db::fts_delete(c, "title", &id)?;
            (0, 1, None)
        }
        Some((bytes, ..)) if bytes == size => return Ok(None),
        // A copy made before the file's last turns: what the index read from the original stays.
        Some((bytes, ..)) if !original && bytes > size => return Ok(None),
        Some((bytes, off, turn, ended, _)) if bytes < size && off <= size => (off, turn, ended),
        _ => (0, 1, None), // new, shrunk or inconsistent: (re)build this file
    };

    let mut reader: Box<dyn BufRead + Send> = if original {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset as u64))?;
        Box::new(BufReader::with_capacity(1 << 16, file))
    } else {
        // A compressed copy cannot seek: read past what is already indexed.
        let mut r = crate::archive::open(path)?;
        std::io::copy(&mut (&mut r).take(offset as u64), &mut std::io::sink())?;
        r
    };
    let mut buf = Vec::new();
    let mut pos = offset;
    let mut open: Option<OpenTurn> = None;
    let mut resume_offset = offset;
    let mut resume_turn = n;
    let mut skipped = 0usize;
    let mut written = 0usize;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut title: Option<String> = None;
    let mut version: Option<String> = None;
    let mut start_scripted: Option<bool> = None;
    let mut unknown: Vec<String> = Vec::new();
    let mut sdk_session = excluded(project, launch_dir);

    let tx = c.transaction()?;
    tx.execute("delete from turns where session_id = ?1 and n >= ?2", params![id, n])?;
    tx.execute("delete from turn_files where session_id = ?1 and n >= ?2", params![id, n])?;
    crate::db::fts_delete_like(&tx, &["prompt", "reply", "command"], &format!("{id}:%"), &format!("{id}:{n:08}"))?;

    if sdk_session {
        // excluded project: nothing is read; the row below marks the file as seen
        tx.execute(
            "insert or replace into sessions (id, project, launch_dir, prompt_count, bytes_indexed, path, harness)
             values (?1, ?2, ?3, 0, ?4, ?5, ?6)",
            params![id, project, launch_dir.to_string_lossy(), size, path.to_string_lossy(), harness],
        )?;
        tx.commit()?;
        return Ok(Some((0, 0)));
    }
    loop {
        buf.clear();
        let line_start = pos;
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        pos += read as i64;
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let ev = match classify(line) {
            Parsed::Event(e) => e,
            Parsed::Ignored => continue,
            Parsed::Unknown(t) => {
                // A subagent's records (Copilot's, or a Codex subagent's whole rollout) cannot be placed in the
                // session's turns yet (spec, "My calls").
                if t.starts_with("subagent.") {
                    sdk_session = true;
                }
                unknown.push(t);
                continue;
            }
            Parsed::Unreadable => { skipped += 1; continue }
        };
        match ev {
            Event::Prompt { ts, branch: b, text, entrypoint, .. } => {
                // Sessions driven through the SDK (claude-mem's observer, scripted agents) are not
                // the user's work; record the file as seen so it is not re-read, and index nothing.
                // An import was asked for by name: `claude -p --teleport` marks the copy it writes `sdk-cli`.
                if !imported && n == 1 && open.is_none() && entrypoint.as_deref() == Some("sdk-cli") {
                    sdk_session = true;
                    break;
                }
                if let Some(t) = open.take() {
                    write_turn(&tx, &id, project, &t)?;
                    written += 1;
                }
                if branch.is_none() {
                    branch = b;
                }
                if first_prompt.is_none() && !is_throwaway_prompt(&text) {
                    first_prompt = Some(text.clone());
                }
                first_ts.get_or_insert_with(|| ts.clone());
                last_ts = Some(ts.clone());
                resume_offset = line_start;
                resume_turn = n;
                open = Some(OpenTurn {
                    n, ts, prompt: text, reply: None, files: Vec::new(), reads: Vec::new(), tools: Default::default(),
                    commands: Vec::new(),
                });
                n += 1;
            }
            Event::Title(t) => { title = Some(t); }
            Event::Start { branch: b, version: v, scripted: s, .. } => {
                branch = b;
                version = v;
                start_scripted = s;
            }
            Event::Reply { ts, text, files, tools, reads, commands } => {
                last_ts = Some(ts);
                if let Some(t) = open.as_mut() {
                    if text.is_some() {
                        t.reply = text;
                    }
                    for f in reads {
                        if !t.reads.contains(&f) {
                            t.reads.push(f);
                        }
                    }
                    for name in tools {
                        *t.tools.entry(name).or_insert(0) += 1;
                    }
                    for f in files {
                        if !t.files.contains(&f) {
                            t.files.push(f);
                        }
                    }
                    for cmd in commands {
                        if !t.commands.contains(&cmd) {
                            t.commands.push(cmd);
                        }
                    }
                }
            }
        }
    }
    if let Some(t) = open.take() {
        write_turn(&tx, &id, project, &t)?;
        written += 1;
    }
    if !unknown.is_empty() {
        // `doctor` names each record type the reader does not know, with the first session that held it.
        let key = format!("{harness}_unknown_types");
        let mut seen: serde_json::Map<String, serde_json::Value> = tx
            .query_row("select v from meta where k = ?1", [&key], |r| r.get::<_, String>(0)).ok()
            .and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        for t in unknown {
            seen.entry(t).or_insert(id.into());
        }
        tx.execute(
            "insert into meta (k, v) values (?1, ?2) on conflict(k) do update set v = excluded.v",
            params![key, serde_json::Value::Object(seen).to_string()],
        )?;
    }
    if sdk_session {
        // Turns an earlier pass wrote go too: the session is indexed as nothing.
        tx.execute("delete from turns where session_id = ?1", [id])?;
        tx.execute("delete from turn_files where session_id = ?1", [id])?;
        crate::db::fts_delete_like(&tx, &["prompt", "reply", "command"], &format!("{id}:%"), "")?;
        crate::db::fts_delete(&tx, "title", id)?;
        tx.execute(
            "insert or replace into sessions (id, project, launch_dir, prompt_count, bytes_indexed, path, harness, harness_version)
             values (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7)",
            params![id, project, launch_dir.to_string_lossy(), size, path.to_string_lossy(), harness, version],
        )?;
        tx.commit()?;
        return Ok(Some((0, 0)));
    }

    // Session row: started_at, branch and first_prompt are kept from the first pass; the title is
    // the newest non-generic one Claude Code wrote.
    tx.execute(
        "insert into sessions (id, project, launch_dir, branch, started_at, ended_at, first_prompt,
                               prompt_count, bytes_indexed, resume_offset, resume_turn, path, title, worktree,
                               harness, harness_version)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         on conflict(id) do update set
           ended_at = coalesce(excluded.ended_at, sessions.ended_at),
           branch = coalesce(sessions.branch, excluded.branch),
           started_at = coalesce(sessions.started_at, excluded.started_at),
           first_prompt = coalesce(sessions.first_prompt, excluded.first_prompt),
           title = coalesce(excluded.title, sessions.title),
           harness_version = coalesce(sessions.harness_version, excluded.harness_version),
           prompt_count = excluded.prompt_count, bytes_indexed = excluded.bytes_indexed,
           resume_offset = excluded.resume_offset, resume_turn = excluded.resume_turn",
        params![
            id, project, launch_dir.to_string_lossy(), branch, first_ts, last_ts, first_prompt,
            n - 1, size, resume_offset, resume_turn, path.to_string_lossy(), title, worktree, harness, version
        ],
    )?;
    if let Some(t) = &title {
        crate::db::fts_delete(&tx, "title", &id)?;
        crate::db::fts_insert(&tx, "title", &id, project, first_ts.as_deref().unwrap_or(""), t)?;
    }

    let (started, ended, version, stored_title, stored_scripted): (Option<String>, Option<String>, Option<String>, Option<String>, bool) = tx.query_row(
        "select started_at, ended_at, harness_version, title, scripted from sessions where id = ?1",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    // Codex's first record says whether the run was scripted; a later pass starts past it and keeps the stored flag.
    // From Copilot CLI 1.0.105 every interactive session asks for a title; a `copilot -p` run never does.
    let scripted = start_scripted.or((harness == "codex").then_some(stored_scripted)).unwrap_or_else(|| stored_title.is_none() && version.is_some_and(|v| {
        let parts: Vec<u64> = v.split(|ch: char| !ch.is_ascii_digit()).take(3).map(|p| p.parse().unwrap_or(0)).collect();
        parts >= vec![1, 0, 105]
    }));
    tx.execute("update sessions set scripted = ?2 where id = ?1", params![id, scripted])?;
    if ended != prior_ended {
        if let (Some(s), Some(e)) = (started, ended) {
            refresh_commits(&tx, &id, project, launch_dir, &s, &e)?;
        }
    }
    tx.commit()?;
    Ok(Some((written, skipped)))
}

fn write_turn(tx: &Connection, id: &str, project: &str, t: &OpenTurn) -> rusqlite::Result<()> {
    let files = serde_json::to_string(&t.files).unwrap_or_else(|_| "[]".into());
    let tools = serde_json::to_string(&t.tools).unwrap_or_else(|_| "{}".into());
    let commands = serde_json::to_string(&t.commands).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "insert or replace into turns (session_id, n, ts, prompt, reply, files, tools, commands)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, t.n, t.ts, t.prompt, t.reply, files, tools, commands],
    )?;
    for f in &t.files {
        tx.execute("insert or replace into turn_files (session_id, n, path, kind) values (?1, ?2, ?3, 'edit')", params![id, t.n, f])?;
    }
    for f in &t.reads {
        if !t.files.contains(f) {
            tx.execute("insert or ignore into turn_files (session_id, n, path, kind) values (?1, ?2, ?3, 'read')", params![id, t.n, f])?;
        }
    }
    let r = format!("{id}:{:08}", t.n);
    crate::db::fts_delete(tx, "prompt", &r)?;
    crate::db::fts_delete(tx, "reply", &r)?;
    crate::db::fts_delete(tx, "command", &r)?;
    crate::db::fts_insert(tx, "prompt", &r, project, &t.ts, &t.prompt)?;
    if let Some(reply) = &t.reply {
        crate::db::fts_insert(tx, "reply", &r, project, &t.ts, reply)?;
    }
    if !t.commands.is_empty() {
        crate::db::fts_insert(tx, "command", &r, project, &t.ts, &t.commands.join("\n"))?;
    }
    Ok(())
}

fn refresh_commits(
    tx: &Connection,
    id: &str,
    project: &str,
    dir: &Path,
    since: &str,
    until: &str,
) -> rusqlite::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let out = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "log", "--all", "--format=%H%x09%aI%x09%s"])
        .arg(format!("--since={since}"))
        .arg(format!("--until={until}"))
        .output();
    let Ok(out) = out else { return Ok(()) };
    if !out.status.success() {
        return Ok(());
    }
    let shas: Vec<String> = tx
        .prepare("select sha from commits where session_id = ?1")?
        .query_map([id], |r| r.get(0))?
        .flatten()
        .collect();
    for sha in shas {
        crate::db::fts_delete(tx, "commit", &sha)?;
    }
    tx.execute("delete from commits where session_id = ?1", [id])?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.splitn(3, '\t');
        let (Some(sha), Some(ts), Some(subject)) = (it.next(), it.next(), it.next()) else { continue };
        tx.execute(
            "insert or replace into commits (session_id, sha, ts, subject) values (?1, ?2, ?3, ?4)",
            params![id, sha, ts, subject],
        )?;
        crate::db::fts_delete(tx, "commit", sha)?;
        crate::db::fts_insert(tx, "commit", sha, project, ts, subject)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_dir_name_preferring_existing_directories() {
        let base = std::env::temp_dir().join(format!("gopola-decode-{}", std::process::id()));
        let nested = base.join("Work_Repos").join("client-service");
        std::fs::create_dir_all(&nested).unwrap();
        let encoded = encode(&base.to_string_lossy()) + "-Work-Repos-client-service";
        assert_eq!(decode_dir_name(&encoded), nested);
        assert_eq!(project_name(&decode_dir_name(&encoded)), "client-service");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn decodes_unknown_dirs_segment_by_segment() {
        assert_eq!(decode_dir_name("-nope-such-dir"), PathBuf::from("/nope/such/dir"));
        assert_eq!(project_name(&PathBuf::from("/Users/x/.claude")), "claude");
        assert_eq!(project_name(&PathBuf::from("/Users/x/repos/demo-app")), "demo-app");
    }
}
