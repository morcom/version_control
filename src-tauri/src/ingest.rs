use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::app::{AppState, FolderContext};
use crate::db::{now_millis, BlobRef, FileRow, KeyframeUpdate, NewVersion};
use crate::diff;
use crate::extract::{self, ContentKind};
use crate::model::CompareResult;
use crate::store::BlobStore;
use crate::watcher::Job;

const DEBOUNCE: Duration = Duration::from_millis(1200);
const STABLE_WINDOW: Duration = Duration::from_millis(700);
const STABLE_POLL: Duration = Duration::from_millis(120);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DEFERRALS: u32 = 50;
const MAX_DELTA_BASE: u64 = 32 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const RENAME_WINDOW_MS: i64 = 15_000;

pub const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/node_modules/**",
    "**/~$*",
    "**/*.tmp",
    "**/*.temp",
    "**/*.swp",
    "**/*.crdownload",
    "**/Thumbs.db",
    "**/desktop.ini",
    "**/.DS_Store",
    "**/*.lock",
];

const SKIP_DIRS: &[&str] = &[".git", ".svn", ".hg", "node_modules"];

#[derive(Debug)]
enum PendingKind {
    Touch(PathBuf),
    Removed(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

struct Pending {
    kind: PendingKind,
    last: Instant,
    seq: u64,
    retries: u32,
}

pub fn worker_loop(state: Arc<AppState>, rx: Receiver<Job>) {
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    let mut seq = 0u64;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Job::Reconcile { folder_id }) => {
                flush_all(&state, &mut pending);
                guarded("reconcile", || reconcile_folder(&state, folder_id));
            }
            Ok(Job::Snapshot { folder_id }) => {
                flush_all(&state, &mut pending);
                guarded("snapshot", || snapshot_folder(&state, folder_id).map(|_| ()));
            }
            Ok(Job::Touch { path, .. }) => {
                seq += 1;
                merge(&mut pending, path.clone(), PendingKind::Touch(path), seq);
            }
            Ok(Job::Removed { path, .. }) => {
                seq += 1;
                merge(&mut pending, path.clone(), PendingKind::Removed(path), seq);
            }
            Ok(Job::Renamed { from, to, .. }) => {
                seq += 1;
                pending.remove(&from);
                merge(&mut pending, to.clone(), PendingKind::Renamed { from, to }, seq);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        let mut ready: Vec<(PathBuf, u64)> = pending
            .iter()
            .filter(|(_, item)| now.duration_since(item.last) >= DEBOUNCE)
            .map(|(key, item)| (key.clone(), item.seq))
            .collect();
        ready.sort_by_key(|(_, s)| *s);
        let mut items: Vec<Pending> = Vec::new();
        for (key, _) in ready {
            let Some(mut item) = pending.remove(&key) else {
                continue;
            };
            let busy = matches!(&item.kind, PendingKind::Touch(path) if still_changing(path));
            if busy && item.retries < MAX_DEFERRALS {
                item.retries += 1;
                item.last = Instant::now();
                pending.insert(key, item);
            } else {
                items.push(item);
            }
        }
        apply_batch(&state, items);
        state.flush_notify();
    }
}

fn still_changing(path: &Path) -> bool {
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|age| age < STABLE_WINDOW)
        .unwrap_or(false)
}

fn guarded<F: FnOnce() -> Result<()>>(what: &str, f: F) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log::error!("{what} failed: {err:#}"),
        Err(_) => log::error!("{what} panicked"),
    }
}

fn merge(pending: &mut HashMap<PathBuf, Pending>, key: PathBuf, kind: PendingKind, seq: u64) {
    match pending.get_mut(&key) {
        Some(entry) => {
            entry.kind = match (std::mem::replace(&mut entry.kind, PendingKind::Touch(PathBuf::new())), kind) {
                (PendingKind::Renamed { from, to }, PendingKind::Touch(_)) => PendingKind::Renamed { from, to },
                (_, other) => other,
            };
            entry.last = Instant::now();
            entry.seq = seq;
        }
        None => {
            pending.insert(
                key,
                Pending {
                    kind,
                    last: Instant::now(),
                    seq,
                    retries: 0,
                },
            );
        }
    }
}

fn apply_batch(state: &Arc<AppState>, items: Vec<Pending>) {
    let (removals, others): (Vec<_>, Vec<_>) = items
        .into_iter()
        .partition(|p| matches!(p.kind, PendingKind::Removed(_)));
    for item in removals.into_iter().chain(others) {
        let label = format!("{:?}", item.kind);
        guarded(&label, || apply_pending(state, item.kind));
    }
}

fn flush_all(state: &Arc<AppState>, pending: &mut HashMap<PathBuf, Pending>) {
    let mut items: Vec<Pending> = pending.drain().map(|(_, item)| item).collect();
    items.sort_by_key(|p| p.seq);
    apply_batch(state, items);
}

fn apply_pending(state: &Arc<AppState>, kind: PendingKind) -> Result<()> {
    match kind {
        PendingKind::Touch(path) => {
            if let Some(folder_id) = state.folder_for_path(&path) {
                if path.is_dir() {
                    reconcile_subtree(state, folder_id, &path)?;
                } else {
                    ingest_path(state, folder_id, &path, "modified")?;
                }
            }
        }
        PendingKind::Removed(path) => {
            if let Some(folder_id) = state.folder_for_path(&path) {
                mark_removed(state, folder_id, &path)?;
            }
        }
        PendingKind::Renamed { from, to } => {
            if let Some(folder_id) = state.folder_for_path(&to) {
                apply_rename(state, folder_id, &from, &to)?;
                if to.is_dir() {
                    reconcile_subtree(state, folder_id, &to)?;
                } else {
                    ingest_path(state, folder_id, &to, "modified")?;
                }
            } else if let Some(folder_id) = state.folder_for_path(&from) {
                mark_removed(state, folder_id, &from)?;
            }
        }
    }
    Ok(())
}

fn try_build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        builder.add(Glob::new(pattern).map_err(|e| anyhow!("invalid ignore pattern {pattern:?}: {e}"))?);
    }
    builder
        .build()
        .map_err(|e| anyhow!("the ignore patterns cannot be combined: {e}"))
}

pub fn build_glob_set(patterns: &[String]) -> GlobSet {
    try_build_glob_set(patterns).unwrap_or_else(|err| {
        log::warn!("{err:#}; skipping the invalid patterns");
        let valid: Vec<String> = patterns
            .iter()
            .filter(|p| p.trim().is_empty() || Glob::new(p.trim()).is_ok())
            .cloned()
            .collect();
        try_build_glob_set(&valid).unwrap_or_else(|_| {
            try_build_glob_set(&default_exclude_patterns()).expect("built-in ignore patterns are valid")
        })
    })
}

pub fn default_exclude_patterns() -> Vec<String> {
    DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
}

fn user_patterns(exclude_globs: &str) -> Vec<String> {
    exclude_globs
        .split(['\n', ','])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn validate_patterns(exclude_globs: &str) -> Result<()> {
    let mut patterns = default_exclude_patterns();
    patterns.extend(user_patterns(exclude_globs));
    try_build_glob_set(&patterns).map(|_| ())
}

pub fn folder_patterns(exclude_globs: &str) -> GlobSet {
    let mut patterns = default_exclude_patterns();
    patterns.extend(user_patterns(exclude_globs));
    build_glob_set(&patterns)
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn invalid_user_pattern_keeps_builtin_exclusions() {
        let set = folder_patterns("**/*.log\n[unclosed");
        assert!(is_excluded("logs/app.log", &set));
        assert!(is_excluded("a/.git/config", &set));
        assert!(is_excluded("x/~$draft.docx", &set));
        assert!(is_excluded("cache/file.tmp", &set));
    }

    #[test]
    fn validation_reports_bad_patterns() {
        assert!(validate_patterns("**/*.log").is_ok());
        assert!(validate_patterns("[unclosed").is_err());
    }
}

pub fn is_excluded(rel_path: &str, globs: &GlobSet) -> bool {
    if globs.is_match(rel_path) {
        return true;
    }
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    name.starts_with("~$")
}

fn mtime_millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_cloud_placeholder(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        if let Ok(meta) = fs::metadata(path) {
            return meta.file_attributes() & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0;
        }
    }
    let _ = path;
    false
}

pub fn read_stable(path: &Path) -> Option<(Vec<u8>, u64, i64)> {
    if is_cloud_placeholder(path) {
        return None;
    }
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut last: Option<(u64, SystemTime)> = None;
    let mut stable_since = Instant::now();
    loop {
        let meta = fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let size = meta.len();
        let mtime = meta.modified().ok()?;
        let idle = SystemTime::now()
            .duration_since(mtime)
            .map(|age| age >= STABLE_WINDOW)
            .unwrap_or(false);
        let stable = idle
            || matches!(last, Some((s, m)) if s == size && m == mtime && stable_since.elapsed() >= STABLE_WINDOW);
        if stable {
            if let Ok(data) = fs::read(path) {
                let after = fs::metadata(path).ok()?;
                if after.len() == size && after.modified().ok() == Some(mtime) && data.len() as u64 == size {
                    return Some((data, size, mtime_millis(mtime)));
                }
            }
            last = None;
        } else if last != Some((size, mtime)) {
            last = Some((size, mtime));
            stable_since = Instant::now();
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(STABLE_POLL);
    }
}

pub fn ingest_path(state: &Arc<AppState>, folder_id: i64, path: &Path, event_hint: &str) -> Result<()> {
    ingest_inner(state, folder_id, path, event_hint, Mode::Normal).map(|_| ())
}

pub fn snapshot_for_restore(state: &Arc<AppState>, folder_id: i64, path: &Path) -> Result<i64> {
    ingest_inner(state, folder_id, path, "pre-restore", Mode::Always)?
        .ok_or_else(|| anyhow!("the current content of {} could not be saved", path.display()))
}

pub fn record_restored(state: &Arc<AppState>, folder_id: i64, path: &Path) -> Result<Option<i64>> {
    ingest_inner(state, folder_id, path, "restored", Mode::IgnoreRules)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    IgnoreRules,
    Always,
}

fn ingest_inner(
    state: &Arc<AppState>,
    folder_id: i64,
    path: &Path,
    event_hint: &str,
    mode: Mode,
) -> Result<Option<i64>> {
    let force = mode != Mode::Normal;
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(None);
    };
    if !force && !state.folder_active(&ctx.row) {
        return Ok(None);
    }
    let Ok(rel) = crate::db::rel_path_of(&ctx.row.path, path) else {
        return Ok(None);
    };
    if rel.is_empty() || (!force && is_excluded(&rel, &ctx.patterns)) {
        return Ok(None);
    }
    let Ok(meta) = fs::metadata(path) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let max_bytes = (ctx.row.max_file_mb.max(1) as u64) * 1024 * 1024;
    if !force && meta.len() > max_bytes {
        log::debug!("skipping {rel}: larger than the configured limit");
        return Ok(None);
    }

    let Some((data, size, mtime)) = read_stable(path) else {
        return Ok(None);
    };
    let hash = blake3::hash(&data).to_hex().to_string();
    let now = now_millis();

    let mut renamed_from: Option<String> = None;
    let file_row = match state.db.find_file(folder_id, &rel)? {
        Some(row) => row,
        None => match state
            .db
            .find_rename_candidate(folder_id, &hash, now - RENAME_WINDOW_MS)?
        {
            Some(old) => {
                renamed_from = Some(old.rel_path.clone());
                state.db.rename_file(old.id, &rel, now)?;
                log::info!("followed rename {} -> {}", old.rel_path, rel);
                state
                    .db
                    .file_row(old.id)?
                    .ok_or_else(|| anyhow!("file row vanished after rename"))?
            }
            None => {
                let id = state.db.upsert_file(folder_id, &rel, now)?;
                state
                    .db
                    .file_row(id)?
                    .ok_or_else(|| anyhow!("file row vanished after insert"))?
            }
        },
    };

    let latest = state.db.latest_version_row(file_row.id)?;
    let came_back = latest.as_ref().is_some_and(|l| l.event == "deleted");
    let unchanged = latest.as_ref().is_some_and(|l| l.content_hash == hash);
    if mode != Mode::Always && unchanged && !came_back && renamed_from.is_none() {
        state.db.update_file_state(
            file_row.id,
            size as i64,
            mtime,
            &hash,
            file_row.keyframe_blob_id,
            now,
        )?;
        return Ok(None);
    }

    let prev_path = renamed_from.filter(|old| *old != rel);
    let event = match event_hint {
        "renamed" | "pre-restore" | "snapshot" | "restored" => event_hint,
        _ if prev_path.is_some() => "renamed",
        _ if came_back => "created",
        _ if latest.is_some() => "modified",
        _ => "created",
    };

    state.db.touch_file_stat(file_row.id, size as i64, mtime)?;
    let version_id = store_content(
        state,
        &file_row,
        &data,
        &hash,
        event,
        prev_path.as_deref(),
        size as i64,
        mtime,
        now,
    )?;
    state.notify();
    log::info!("stored version {version_id} for {rel} ({size} bytes, {event})");
    Ok(Some(version_id))
}

#[allow(clippy::too_many_arguments)]
pub fn store_content(
    state: &Arc<AppState>,
    file_row: &FileRow,
    data: &[u8],
    hash: &str,
    event: &str,
    prev_path: Option<&str>,
    size: i64,
    mtime: i64,
    now: i64,
) -> Result<i64> {
    let kind = extract::kind_for_path(Path::new(&file_row.rel_path), data);
    let (added, removed, snippet, is_text) = compute_stats(state, file_row, data, hash, kind);

    let _storage = state.storage_lock.read().unwrap_or_else(|e| e.into_inner());

    let storage_hash: String;
    let blob_ref: BlobRef;
    let keyframe: KeyframeUpdate;
    if let Some(existing) = state.db.find_full_blob(hash)? {
        if !state.store.object_path(&existing.storage_hash).exists() {
            let full_bytes = BlobStore::compress_full(data)?;
            let full_hash = BlobStore::storage_hash(&full_bytes);
            state.store.write_blob(&full_hash, &full_bytes)?;
            state
                .db
                .update_blob_storage(existing.id, &full_hash, full_bytes.len() as i64)?;
        }
        blob_ref = BlobRef::Existing(existing.id);
        keyframe = KeyframeUpdate::UseBlob;
    } else {
        let full_bytes = BlobStore::compress_full(data)?;
        let full_hash = BlobStore::storage_hash(&full_bytes);
        let delta = try_delta(state, file_row, data, full_bytes.len());
        let (kind, base_id, stored_size) = match delta {
            Some((bytes, base_id)) => {
                storage_hash = BlobStore::storage_hash(&bytes);
                state.store.write_blob(&storage_hash, &bytes)?;
                ("delta", base_id, bytes.len() as i64)
            }
            None => {
                state.store.write_blob(&full_hash, &full_bytes)?;
                storage_hash = full_hash;
                ("full", 0, full_bytes.len() as i64)
            }
        };
        blob_ref = BlobRef::New {
            kind,
            base_id,
            stored_size,
            storage_hash: &storage_hash,
        };
        keyframe = KeyframeUpdate::Keep(file_row.keyframe_blob_id);
    }

    let version_id = state.db.commit_version(&NewVersion {
        file_id: file_row.id,
        ts: now,
        event,
        size,
        mtime,
        content_hash: hash,
        blob: blob_ref,
        keyframe,
        prev_path,
        added,
        removed,
        snippet: snippet.as_deref(),
        is_text,
    })?;
    Ok(version_id)
}

fn try_delta(state: &Arc<AppState>, file_row: &FileRow, data: &[u8], full_len: usize) -> Option<(Vec<u8>, i64)> {
    let base_id = file_row.keyframe_blob_id?;
    let base_blob = state.db.blob_row(base_id).ok().flatten()?;
    if base_blob.kind != "full" || base_blob.raw_size as u64 > MAX_DELTA_BASE {
        return None;
    }
    let base_bytes = load_blob_bytes(state, &base_blob).ok()?;
    let delta_bytes = BlobStore::compress_delta(&base_bytes, data).ok()?;
    if delta_bytes.len() >= full_len {
        return None;
    }
    let roundtrip = BlobStore::decompress_delta(&base_bytes, &delta_bytes, data.len()).ok()?;
    (roundtrip == data).then_some((delta_bytes, base_id))
}

fn compute_stats(
    state: &Arc<AppState>,
    file_row: &FileRow,
    data: &[u8],
    hash: &str,
    kind: ContentKind,
) -> (i64, i64, Option<String>, bool) {
    let new_text = if data.len() <= MAX_TEXT_BYTES {
        extract::extract_text(kind, data)
    } else {
        None
    };
    let previous_hash = file_row.content_hash.clone();
    let previous_size = file_row.size;
    let previous_text = previous_hash
        .as_ref()
        .and_then(|h| state.db.get_text(h).ok().flatten());
    if let Some(text) = &new_text {
        let _ = state.db.set_text_if_absent(hash, text);
    }
    match (previous_text, new_text) {
        (Some(old_text), Some(new_text)) => {
            let stats = diff::diff_stats(&old_text, &new_text, 6);
            (stats.added, stats.removed, stats.snippet, true)
        }
        (None, Some(new_text)) => (new_text.lines().count() as i64, 0, None, true),
        _ => (0, 0, diff::binary_snippet(previous_size, data.len() as i64), false),
    }
}

pub fn load_blob_bytes(state: &Arc<AppState>, blob: &crate::db::BlobRow) -> Result<Vec<u8>> {
    let stored = state.store.read_stored(&blob.storage_hash)?;
    if blob.kind == "full" {
        BlobStore::decompress_full(&stored, blob.raw_size as usize)
    } else {
        let base = state
            .db
            .blob_row(blob.base_id)?
            .ok_or_else(|| anyhow!("delta base blob is missing"))?;
        if base.kind != "full" {
            return Err(anyhow!("delta base is not a full blob"));
        }
        let base_bytes = load_blob_bytes(state, &base)?;
        BlobStore::decompress_delta(&base_bytes, &stored, blob.raw_size as usize)
    }
}

pub fn load_version_bytes(state: &Arc<AppState>, version_id: i64) -> Result<Vec<u8>> {
    let version = state
        .db
        .version_row(version_id)?
        .ok_or_else(|| anyhow!("version {version_id} does not exist"))?;
    let blob = state
        .db
        .blob_row(version.blob_id)?
        .ok_or_else(|| anyhow!("blob for version {version_id} is missing"))?;
    load_blob_bytes(state, &blob)
}

pub fn version_text(state: &Arc<AppState>, version_id: i64) -> Option<String> {
    let version = state.db.version_row(version_id).ok().flatten()?;
    if let Some(text) = state.db.get_text(&version.content_hash).ok().flatten() {
        return Some(text);
    }
    if version.size as usize > MAX_TEXT_BYTES {
        return None;
    }
    let bytes = load_version_bytes(state, version_id).ok()?;
    let kind = extract::kind_for_path(Path::new(&version.rel_path), &bytes);
    extract::extract_text(kind, &bytes)
}

pub fn compare_versions(state: &Arc<AppState>, old_id: i64, new_id: i64) -> Result<CompareResult> {
    let old_version = state
        .db
        .version_row(old_id)?
        .ok_or_else(|| anyhow!("version {old_id} does not exist"))?;
    let new_version = state
        .db
        .version_row(new_id)?
        .ok_or_else(|| anyhow!("version {new_id} does not exist"))?;
    let kind = extract::kind_for_path(Path::new(&new_version.rel_path), &[]);
    let old_label = old_version.prev_path.clone().unwrap_or(old_version.rel_path.clone());
    let new_label = new_version.rel_path.clone();

    let texts = if old_version.is_text || new_version.is_text || kind.is_textual() {
        version_text(state, old_id).zip(version_text(state, new_id))
    } else {
        None
    };

    match texts {
        Some((old_text, new_text)) => {
            let outcome = diff::diff_lines(&old_text, &new_text, &old_label, &new_label);
            Ok(CompareResult {
                is_text: true,
                kind: kind.as_str().to_string(),
                added: outcome.added,
                removed: outcome.removed,
                old_label,
                new_label,
                old_size: old_version.size,
                new_size: new_version.size,
                old_hash: old_version.content_hash,
                new_hash: new_version.content_hash,
                rows: outcome.rows,
                unified: outcome.unified,
                truncated: outcome.truncated,
            })
        }
        None => Ok(CompareResult {
            is_text: false,
            kind: if kind.is_textual() { "binary".into() } else { kind.as_str().to_string() },
            added: 0,
            removed: 0,
            old_label,
            new_label,
            old_size: old_version.size,
            new_size: new_version.size,
            old_hash: old_version.content_hash,
            new_hash: new_version.content_hash,
            rows: Vec::new(),
            unified: diff::binary_snippet(old_version.size, new_version.size).unwrap_or_default(),
            truncated: false,
        }),
    }
}

fn record_event(state: &Arc<AppState>, file: &FileRow, event: &str, prev_path: Option<&str>) -> Result<()> {
    if let Some(latest) = state.db.latest_version_row(file.id)? {
        state.db.insert_version(
            file.id,
            now_millis(),
            event,
            latest.size,
            &latest.content_hash,
            latest.blob_id,
            prev_path,
            0,
            0,
            None,
            latest.is_text,
        )?;
    }
    Ok(())
}

fn rename_one(state: &Arc<AppState>, folder_id: i64, row: &FileRow, new_rel: &str) -> Result<()> {
    let now = now_millis();
    match state.db.find_file(folder_id, new_rel)? {
        Some(target) if target.id != row.id => {
            state.db.merge_file_into(row.id, target.id, now)?;
            if let Some(merged) = state.db.file_row(target.id)? {
                record_event(state, &merged, "renamed", Some(&row.rel_path))?;
            }
        }
        Some(_) => {}
        None => {
            state.db.rename_file(row.id, new_rel, now)?;
            if let Some(renamed) = state.db.file_row(row.id)? {
                record_event(state, &renamed, "renamed", Some(&row.rel_path))?;
            }
        }
    }
    log::info!("renamed {} -> {}", row.rel_path, new_rel);
    Ok(())
}

pub fn apply_rename(state: &Arc<AppState>, folder_id: i64, from: &Path, to: &Path) -> Result<()> {
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(());
    };
    if !state.folder_active(&ctx.row) {
        return Ok(());
    }
    let (Ok(old_rel), Ok(new_rel)) = (
        crate::db::rel_path_of(&ctx.row.path, from),
        crate::db::rel_path_of(&ctx.row.path, to),
    ) else {
        return Ok(());
    };
    if let Some(row) = state.db.find_file(folder_id, &old_rel)? {
        rename_one(state, folder_id, &row, &new_rel)?;
    }
    for row in state.db.files_under(folder_id, &old_rel)? {
        let suffix = &row.rel_path[old_rel.len()..];
        let target = format!("{new_rel}{suffix}");
        rename_one(state, folder_id, &row, &target)?;
    }
    state.notify();
    Ok(())
}

fn mark_row_deleted(state: &Arc<AppState>, row: &FileRow) -> Result<()> {
    if row.status == "active" {
        record_event(state, row, "deleted", None)?;
        state.db.mark_file_deleted(row.id, now_millis())?;
        log::info!("file deleted: {}", row.rel_path);
    }
    Ok(())
}

pub fn mark_removed(state: &Arc<AppState>, folder_id: i64, path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(());
    };
    if !state.folder_active(&ctx.row) {
        return Ok(());
    }
    let Ok(rel) = crate::db::rel_path_of(&ctx.row.path, path) else {
        return Ok(());
    };
    if let Some(row) = state.db.find_file(folder_id, &rel)? {
        mark_row_deleted(state, &row)?;
    }
    for row in state.db.files_under(folder_id, &rel)? {
        mark_row_deleted(state, &row)?;
    }
    state.notify();
    Ok(())
}

struct WalkResult {
    files: Vec<(PathBuf, String, i64, i64)>,
    errors: usize,
}

fn walk(ctx: &FolderContext, start: &Path) -> WalkResult {
    let mut files = Vec::new();
    let mut errors = 0usize;
    let walker = walkdir::WalkDir::new(start)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&ctx.root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || is_excluded(&rel, &ctx.patterns) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            errors += 1;
            continue;
        };
        let mtime = meta.modified().ok().map(mtime_millis).unwrap_or(0);
        files.push((entry.path().to_path_buf(), rel, meta.len() as i64, mtime));
    }
    WalkResult { files, errors }
}

fn reconcile_scope(state: &Arc<AppState>, folder_id: i64, start: &Path, scope_rel: Option<&str>) -> Result<()> {
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(());
    };
    if !state.folder_active(&ctx.row) {
        return Ok(());
    }
    if !ctx.root.is_dir() || !start.is_dir() {
        log::warn!("folder {} is not available, skipping re-scan", start.display());
        return Ok(());
    }
    let result = walk(&ctx, start);
    let rows = match scope_rel {
        None => state.db.list_file_rows(folder_id)?,
        Some(prefix) => state.db.files_under(folder_id, prefix)?,
    };
    let tracked: HashMap<String, FileRow> = rows.into_iter().map(|r| (r.rel_path.clone(), r)).collect();
    let seen: HashSet<&str> = result.files.iter().map(|(_, rel, _, _)| rel.as_str()).collect();

    let safe_to_delete = result.errors == 0 && !(result.files.is_empty() && tracked.len() > 5);
    if safe_to_delete {
        for row in tracked.values() {
            if row.status == "active" && !seen.contains(row.rel_path.as_str()) {
                mark_row_deleted(state, row)?;
            }
        }
    } else if result.errors > 0 {
        log::warn!(
            "re-scan of {} had {} errors; missing files are not marked deleted",
            start.display(),
            result.errors
        );
    }

    for (path, rel, size, mtime) in &result.files {
        let needs = match tracked.get(rel) {
            None => true,
            Some(row) => row.status != "active" || row.size != *size || row.mtime != *mtime,
        };
        if needs
            && let Err(err) = ingest_path(state, folder_id, path, "modified") {
                log::warn!("re-scan could not ingest {rel}: {err:#}");
            }
    }
    state.notify();
    Ok(())
}

pub fn reconcile_folder(state: &Arc<AppState>, folder_id: i64) -> Result<()> {
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(());
    };
    reconcile_scope(state, folder_id, &ctx.root, None)
}

fn reconcile_subtree(state: &Arc<AppState>, folder_id: i64, dir: &Path) -> Result<()> {
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(());
    };
    let rel = crate::db::rel_path_of(&ctx.row.path, dir)?;
    reconcile_scope(state, folder_id, dir, Some(&rel))
}

pub fn snapshot_folder(state: &Arc<AppState>, folder_id: i64) -> Result<usize> {
    let Some(ctx) = state.folder(folder_id) else {
        return Ok(0);
    };
    if !state.folder_active(&ctx.row) || !ctx.root.is_dir() {
        return Ok(0);
    }
    let result = walk(&ctx, &ctx.root);
    let mut count = 0usize;
    for (path, rel, _, _) in &result.files {
        match ingest_inner(state, folder_id, path, "snapshot", Mode::Normal) {
            Ok(Some(_)) => count += 1,
            Ok(None) => {}
            Err(err) => log::warn!("snapshot could not ingest {rel}: {err:#}"),
        }
    }
    Ok(count)
}

pub fn canonical(path: &Path) -> PathBuf {
    let Ok(resolved) = fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    let text = resolved.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        resolved
    }
}

fn path_key(path: &Path) -> Vec<String> {
    canonical(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .filter(|c| c != "\\" && !c.is_empty())
        .collect()
}

pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let (a, b) = (path_key(a), path_key(b));
    let shorter = a.len().min(b.len());
    shorter > 0 && a[..shorter] == b[..shorter]
}

pub fn ensure_folder_inside_data_dir(path: &Path, data_dir: &Path) -> Result<()> {
    if paths_overlap(path, data_dir) {
        return Err(anyhow!("the watched folder cannot be the history folder or contain it"));
    }
    Ok(())
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn sibling_folders_with_a_shared_prefix_do_not_overlap() {
        let base = tempfile::tempdir().unwrap();
        let data = base.path().join("History");
        let sibling = base.path().join("HistoryBackup");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        assert!(!paths_overlap(&sibling, &data));
        assert!(ensure_folder_inside_data_dir(&sibling, &data).is_ok());
    }

    #[test]
    fn nested_and_case_variant_paths_overlap() {
        let base = tempfile::tempdir().unwrap();
        let data = base.path().join("History");
        let inner = data.join("objects");
        fs::create_dir_all(&inner).unwrap();
        assert!(paths_overlap(&inner, &data));
        assert!(paths_overlap(base.path(), &data));
        let upper = PathBuf::from(data.to_string_lossy().to_uppercase());
        assert!(paths_overlap(&upper, &data));
    }
}
