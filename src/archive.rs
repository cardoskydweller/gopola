//! gopola's own copy of every transcript. Claude Code deletes transcripts after `cleanupPeriodDays` (default 30), so
//! each file under the projects folder is copied, zstd-compressed, to the same relative path under `archive/` beside
//! the database once it has gone an hour without changes, and again whenever it changes. Readers keep the original
//! path and go through `open`, `read`, `exists`, `size`, `modified` and `list`, which use the copy once the original
//! is gone. Copilot CLI's session folders are kept the same way, but only each `events.jsonl` (gopola never reads
//! `workspace.yaml`), under `archive-copilot/` beside `archive/`, and Codex CLI's plain rollouts under `archive-codex/`.
//! Codex compresses a quiet rollout itself (`<rollout>.zst`, the plain file removed): readers find that before the copy.

use crate::cmds::{Opts, Res};
use serde_json::json;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const QUIET: Duration = Duration::from_secs(3600);
const TMP: &str = ".zst.tmp-";

pub fn root() -> PathBuf {
    crate::db::db_path().with_file_name("archive")
}

pub fn copilot_copies() -> PathBuf {
    root().with_file_name("archive-copilot")
}

pub fn codex_copies() -> PathBuf {
    root().with_file_name("archive-codex")
}

/// The archive folder or file that mirrors a path under the projects folder, or under Copilot's or Codex's in
/// `archive-copilot` or `archive-codex` beside it: inside the Claude mirror, their folders would be listed as Claude projects.
fn mirror(p: &Path) -> Option<PathBuf> {
    if let Ok(rel) = p.strip_prefix(crate::index::copilot_root()) { return Some(copilot_copies().join(rel)) }
    if let Ok(rel) = p.strip_prefix(crate::index::codex_home()) { return Some(codex_copies().join(rel)) }
    Some(root().join(p.strip_prefix(crate::index::projects_root()).ok()?))
}

fn zst(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".zst");
    s.into()
}

pub fn copy_of(original: &Path) -> Option<PathBuf> {
    Some(zst(&mirror(original)?))
}

/// Codex's own `<rollout>.zst` beside a rollout, when Codex has compressed it.
fn codex_zst(path: &Path) -> Option<PathBuf> {
    Some(zst(path)).filter(|z| path.starts_with(crate::index::codex_home()) && z.is_file())
}

/// Where a gone file's bytes are kept compressed: Codex's own `<rollout>.zst` beside it, else gopola's copy.
fn compressed(path: &Path) -> Option<PathBuf> {
    codex_zst(path).or_else(|| copy_of(path))
}

/// The file is still where its tool keeps it: plain, or compressed there by Codex (gopola's copy does not count).
pub fn live(path: &Path) -> bool {
    path.is_file() || codex_zst(path).is_some()
}

#[derive(Default)]
pub struct Synced {
    pub copied: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// From `index`, which hooks run on every prompt and edit: at most every 10 minutes, since a copy waits an hour
/// anyway (a pass with nothing to copy took 0.09 s over 4,387 files on 2026-11-09, against 10-50 ms for `index`).
pub fn sync_if_due() {
    let stamp = root().join(".synced");
    let fresh = std::fs::metadata(&stamp).and_then(|m| m.modified()).ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|d| d < Duration::from_secs(600));
    // Stamped first, so hooks starting together do not all walk the folder.
    if fresh || std::fs::create_dir_all(root()).and_then(|_| std::fs::File::create(&stamp)).is_err() { return }
    sync();
}

/// Copy every quiet file whose copy is missing or carries another modification time. A file that fails is
/// reported and skipped, so one bad file never stops the rest.
pub fn sync() -> Synced {
    let projects = crate::index::projects_root();
    let mut files = Vec::new();
    walk(&projects, &mut files);
    let copilot = std::fs::read_dir(crate::index::copilot_root()).into_iter().flatten().flatten()
        .map(|d| d.path().join("events.jsonl")).filter(|p| p.is_file());
    // Plain rollouts only: Codex's own `.zst` is already compressed and stays where Codex put it.
    let codex = crate::index::codex_rollouts().into_iter().map(|(p, _)| p).filter(|p| p.is_file());
    let now = SystemTime::now();
    let mut s = Synced::default();
    for f in files.into_iter().chain(copilot).chain(codex) {
        // A project's memory folder holds notes, not transcripts.
        if f.strip_prefix(&projects).is_ok_and(|r| r.components().nth(1).is_some_and(|c| c.as_os_str() == "memory")) {
            continue;
        }
        match copy_one(&f, now) {
            Ok(Some((i, o))) => { s.copied += 1; s.bytes_in += i; s.bytes_out += o; }
            Ok(None) => {}
            Err(e) => eprintln!("gopola: archive {}: {e}", f.display()),
        }
    }
    s
}

pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

/// (bytes read, bytes written) when a copy was made.
fn copy_one(f: &Path, now: SystemTime) -> std::io::Result<Option<(u64, u64)>> {
    // The time is read before the bytes: a file that grows while it is read keeps an older time on its copy and is copied again.
    let meta = std::fs::metadata(f)?;
    let (m, len) = (meta.modified()?, meta.len());
    if now.duration_since(m).unwrap_or_default() < QUIET { return Ok(None) }
    let Some(dst) = copy_of(f) else { return Ok(None) };
    if std::fs::metadata(&dst).and_then(|d| d.modified()).ok() == Some(m) { return Ok(None) }
    std::fs::create_dir_all(dst.parent().unwrap_or(&root()))?;
    // Written under a name of this process and renamed, so a reader or another hook never sees half a copy.
    let mut tmp = dst.clone().into_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    // Streamed, so a hook never holds a whole transcript; the pledged length goes into the frame header for `size`.
    let written = (|| -> std::io::Result<u64> {
        let mut enc = zstd::stream::write::Encoder::new(std::fs::File::create(&tmp)?, 3)?;
        enc.set_pledged_src_size(Some(len))?;
        std::io::copy(&mut std::fs::File::open(f)?.take(len), &mut enc)?;
        let out = enc.finish()?;
        out.set_modified(m)?;
        Ok(out.metadata()?.len())
    })();
    match written {
        Ok(n) => { std::fs::rename(&tmp, &dst)?; Ok(Some((len, n))) }
        Err(e) => { let _ = std::fs::remove_file(&tmp); Err(e) }
    }
}

/// The original when it is there, else Codex's `.zst` of it or gopola's copy, decompressed.
pub fn open(path: &Path) -> std::io::Result<Box<dyn BufRead + Send>> {
    match std::fs::File::open(path) {
        Ok(f) => Ok(Box::new(BufReader::with_capacity(1 << 16, f))),
        Err(e) => match compressed(path).map(std::fs::File::open) {
            Some(Ok(c)) => Ok(Box::new(BufReader::with_capacity(1 << 16, zstd::stream::read::Decoder::new(c)?))),
            _ => Err(e),
        },
    }
}

pub fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    open(path)?.read_to_end(&mut v)?;
    Ok(v)
}

pub fn is_dir(path: &Path) -> bool {
    path.is_dir() || mirror(path).is_some_and(|m| m.is_dir())
}

pub fn exists(path: &Path) -> bool {
    path.is_file() || compressed(path).is_some_and(|c| c.is_file())
}

pub fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).or_else(|e| compressed(path).map(std::fs::metadata).unwrap_or(Err(e))).and_then(|m| m.modified()).ok()
}

/// The original's length, or the length its compressed form decompresses to: from the frame header, where gopola's
/// copies always record it, else by decompressing (Codex's own `.zst` may leave it out).
pub fn size(path: &Path) -> Option<u64> {
    if let Ok(m) = std::fs::metadata(path) { return Some(m.len()) }
    let mut head = [0u8; 18];
    let n = std::fs::File::open(compressed(path)?).and_then(|mut f| f.read(&mut head)).ok()?;
    match zstd::zstd_safe::get_frame_content_size(&head[..n]).ok()? {
        Some(len) => Some(len),
        None => std::io::copy(&mut open(path).ok()?, &mut std::io::sink()).ok(),
    }
}

/// What `dir` holds, with what was there before Claude Code deleted it: the paths of its entries and of the copies
/// in its mirror, as original paths, sorted, each once.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let names = |d: &Path| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
    let mut out: Vec<PathBuf> = names(dir).into_iter().map(|n| dir.join(n)).collect();
    if let Some(m) = mirror(dir) {
        out.extend(names(&m).into_iter().filter(|n| !n.contains(TMP)).map(|n| dir.join(n.strip_suffix(".zst").unwrap_or(&n))));
    }
    out.sort();
    out.dedup();
    out
}

/// `gopola archive`: copy what is due now (every `index` does the same).
pub fn archive_cmd(o: &Opts) -> Res {
    let s = sync();
    let mb = |b: u64| b as f64 / 1e6;
    if o.json {
        println!("{}", json!({"copied": s.copied, "bytes_in": s.bytes_in, "bytes_out": s.bytes_out, "archive": root(),
            "archive_copilot": copilot_copies(), "archive_codex": codex_copies()}));
    } else {
        println!("archived {} file{}: {:.1} MB -> {:.1} MB, in {}, {} and {}", s.copied, if s.copied == 1 { "" } else { "s" },
            mb(s.bytes_in), mb(s.bytes_out), root().display(), copilot_copies().display(), codex_copies().display());
    }
    Ok(())
}
