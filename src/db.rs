//! SQLite storage. The tables are described in docs/reference.md, "The tables".

use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

pub const SCHEMA_VERSION: &str = "9";

pub fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("GOPOLA_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/gopola/gopola.db")
}

pub fn open() -> rusqlite::Result<Connection> {
    let path = db_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = Connection::open(&path)?;
    // Hooks in several sessions (and after each edit) index at once: a second writer waits rather than failing.
    c.execute_batch("pragma journal_mode = wal; pragma synchronous = normal; pragma busy_timeout = 5000;")?;
    migrate(&c)?;
    Ok(c)
}

pub fn open_read_only(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "create table if not exists sessions (
           id text primary key, project text not null, launch_dir text not null,
           branch text, started_at text, ended_at text, first_prompt text,
           prompt_count integer not null default 0, bytes_indexed integer not null default 0,
           resume_offset integer not null default 0, resume_turn integer not null default 1,
           path text not null unique);
         create table if not exists turns (
           session_id text not null, n integer not null, ts text not null,
           prompt text not null, reply text, files text,
           primary key (session_id, n));
         create table if not exists commits (
           session_id text not null, sha text not null, ts text not null, subject text not null,
           primary key (session_id, sha));
         create table if not exists observations (
           id integer primary key, project text not null, ts text not null, type text,
           title text, subtitle text, facts text, narrative text);
         create virtual table if not exists fts using fts5(
           kind unindexed, ref unindexed, project unindexed, ts unindexed, text,
           tokenize = 'porter unicode61');
         create table if not exists meta (k text primary key, v text);
         create table if not exists moves (old text primary key, new text not null);
         create table if not exists turn_files (
           session_id text not null, n integer not null, path text not null,
           primary key (session_id, n, path));
         create index if not exists turn_files_path on turn_files(path);
         create table if not exists injected (
           session_id text not null, path text not null, ts text not null,
           primary key (session_id, path));
         create table if not exists usage (
           ts text not null, cmd text not null, arg text, hits integer not null default 0);
         create table if not exists vectors (
           session_id text not null, n integer not null, model text not null,
           hash text not null, vec blob not null,
           primary key (session_id, n, model));
         create table if not exists fts_ref (
           kind text not null, ref text not null, rid integer not null primary key);
         create index if not exists fts_ref_kind_ref on fts_ref(kind, ref);
         create index if not exists sessions_project_started on sessions(project, started_at);",
    )?;
    let cols: Vec<String> = c
        .prepare("pragma table_info(sessions)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .collect();
    if !cols.iter().any(|n| n == "summary") {
        c.execute_batch("alter table sessions add column summary text; alter table sessions add column summarized_at text;")?;
    }
    if !cols.iter().any(|n| n == "summarized_turns") {
        c.execute_batch("alter table sessions add column summarized_turns integer not null default 0;")?;
    }
    if !cols.iter().any(|n| n == "title") {
        c.execute_batch("alter table sessions add column title text;")?;
    }
    if !cols.iter().any(|n| n == "worktree") {
        c.execute_batch("alter table sessions add column worktree text;")?;
    }
    if !cols.iter().any(|n| n == "harness") {
        c.execute_batch("alter table sessions add column harness text not null default 'claude';")?;
    }
    if !cols.iter().any(|n| n == "harness_version") {
        c.execute_batch("alter table sessions add column harness_version text;")?;
    }
    if !cols.iter().any(|n| n == "scripted") {
        c.execute_batch("alter table sessions add column scripted integer not null default 0;")?;
    }
    let fcols: Vec<String> = c
        .prepare("pragma table_info(turn_files)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .collect();
    if !fcols.iter().any(|n| n == "kind") {
        c.execute_batch("alter table turn_files add column kind text not null default 'edit';")?;
    }
    let tcols: Vec<String> = c
        .prepare("pragma table_info(turns)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .collect();
    if !tcols.iter().any(|n| n == "tools") {
        c.execute_batch("alter table turns add column tools text;")?;
    }
    if !tcols.iter().any(|n| n == "commands") {
        // Commands were not kept before schema 7. Rewind every session so the next `index` reads each
        // transcript once from the start and past sessions become searchable by what they ran; summaries,
        // commits and imported observations are untouched by a re-read.
        c.execute_batch(
            "alter table turns add column commands text;
             update sessions set bytes_indexed = 0, resume_offset = 0, resume_turn = 1;",
        )?;
    }
    // Older databases have FTS rows without the rowid map: build it once (an unindexed scan, one time).
    let (nfts, nref): (i64, i64) = c.query_row(
        "select (select count(*) from fts), (select count(*) from fts_ref)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if nfts > 0 && nref == 0 {
        c.execute_batch("insert or ignore into fts_ref (kind, ref, rid) select kind, ref, rowid from fts;")?;
    }
    // Older databases have turns but no turn_files rows: derive them once from the JSON column.
    let (nturns, nfiles): (i64, i64) = c.query_row(
        "select (select count(*) from turns where files != '[]'), (select count(*) from turn_files)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if nturns > 0 && nfiles == 0 {
        c.execute_batch(
            "insert or ignore into turn_files (session_id, n, path)
             select t.session_id, t.n, j.value from turns t, json_each(t.files) j;")?;
    }
    c.execute(
        "insert into meta (k, v) values ('schema_version', ?1) on conflict(k) do update set v = excluded.v",
        [SCHEMA_VERSION],
    )?;
    Ok(())
}

/// Insert one searchable row and remember its rowid so it can be removed without scanning the index.
pub fn fts_insert(c: &Connection, kind: &str, r#ref: &str, project: &str, ts: &str, text: &str) -> rusqlite::Result<()> {
    c.execute(
        "insert into fts (kind, ref, project, ts, text) values (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![kind, r#ref, project, ts, text],
    )?;
    let rid = c.last_insert_rowid();
    c.execute("insert or replace into fts_ref (kind, ref, rid) values (?1, ?2, ?3)", rusqlite::params![kind, r#ref, rid])?;
    Ok(())
}

/// Remove every searchable row for (kind, ref).
pub fn fts_delete(c: &Connection, kind: &str, r#ref: &str) -> rusqlite::Result<()> {
    c.execute(
        "delete from fts where rowid in (select rid from fts_ref where kind = ?1 and ref = ?2)",
        rusqlite::params![kind, r#ref],
    )?;
    c.execute("delete from fts_ref where kind = ?1 and ref = ?2", rusqlite::params![kind, r#ref])?;
    Ok(())
}

/// Remove rows of the given kinds whose ref matches `ref_like` and is at least `ref_min`.
pub fn fts_delete_like(c: &Connection, kinds: &[&str], ref_like: &str, ref_min: &str) -> rusqlite::Result<()> {
    for kind in kinds {
        c.execute(
            "delete from fts where rowid in (select rid from fts_ref where kind = ?1 and ref like ?2 and ref >= ?3)",
            rusqlite::params![kind, ref_like, ref_min],
        )?;
        c.execute("delete from fts_ref where kind = ?1 and ref like ?2 and ref >= ?3", rusqlite::params![kind, ref_like, ref_min])?;
    }
    Ok(())
}

/// Record what was asked of gopola and how much came back; `doctor` reads it. Never fails a command.
pub fn log_usage(c: &Connection, cmd: &str, arg: &str, hits: i64) {
    let _ = c.execute(
        "insert into usage (ts, cmd, arg, hits) values (?1, ?2, ?3, ?4)",
        rusqlite::params![crate::cmds::now_iso(), cmd, arg, hits],
    );
}

/// Drop everything derived from transcripts. Imported observations and summaries survive:
/// both cost something to make again. Summaries are parked and restored by `restore_summaries`.
/// Vectors survive too: they are keyed by a hash of their text, so a rebuilt turn with the same text keeps its vector.
/// The sessions in `keep` (no transcript and no copy left to read them again from) keep everything.
pub fn reset_transcript_tables(c: &Connection, keep: &[String]) -> rusqlite::Result<()> {
    c.execute_batch("create temp table if not exists kept_sessions (id text primary key); delete from kept_sessions;")?;
    for id in keep {
        c.execute("insert or ignore into kept_sessions (id) values (?1)", [id])?;
    }
    c.execute_batch(
        "create temp table if not exists kept_summaries as
           select id, summary, summarized_at from sessions where summary is not null;
         create temp table dropped_rids as select rid from fts_ref where kind not in ('observation', 'summary')
           and not (kind = 'title' and ref in (select id from kept_sessions))
           and not (kind in ('prompt', 'reply', 'command') and substr(ref, 1, instr(ref, ':') - 1) in (select id from kept_sessions))
           and not (kind = 'commit' and ref in (select sha from commits where session_id in (select id from kept_sessions)));
         delete from fts where rowid in (select rid from dropped_rids);
         delete from fts_ref where rid in (select rid from dropped_rids);
         drop table dropped_rids;
         delete from sessions where id not in (select id from kept_sessions);
         delete from turns where session_id not in (select id from kept_sessions);
         delete from commits where session_id not in (select id from kept_sessions);
         delete from turn_files where session_id not in (select id from kept_sessions);",
    )
}

/// After a full rebuild, put parked summaries back on their sessions.
pub fn restore_summaries(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "update sessions set summary = (select summary from kept_summaries k where k.id = sessions.id),
           summarized_at = (select summarized_at from kept_summaries k where k.id = sessions.id)
         where id in (select id from kept_summaries);
         drop table if exists kept_summaries;",
    )
}
