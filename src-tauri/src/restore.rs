use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use crate::app::AppState;
use crate::ingest::{load_version_bytes, record_restored, snapshot_for_restore};
use crate::model::RestoreOutcome;

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("fht-restore-{}.tmp", std::process::id()));
    fs::write(&tmp, data).with_context(|| format!("cannot write {}", tmp.display()))?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(anyhow!("cannot replace {}: {err}", path.display()))
        }
    }
}

fn is_link_or_junction(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(anyhow!("unsafe relative path {rel}"));
    }
    let mut current = base.to_path_buf();
    for component in rel_path.components() {
        current.push(component);
        if is_link_or_junction(&current) {
            return Err(anyhow!(
                "refusing to restore through the link or junction {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn ensure_still_inside(base: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().unwrap_or(target);
    let resolved_parent = crate::ingest::canonical(parent);
    let resolved_base = crate::ingest::canonical(base);
    if !resolved_parent.starts_with(&resolved_base) {
        return Err(anyhow!(
            "refusing to restore {}: it resolves outside {}",
            target.display(),
            base.display()
        ));
    }
    Ok(())
}

fn write_contained(base: &Path, target: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    ensure_still_inside(base, target)?;
    write_atomic(target, data)
}

fn is_tracked(state: &Arc<AppState>, path: &Path) -> bool {
    state.folder_for_path(path).is_some()
}

pub fn restore_as(state: &Arc<AppState>, version_id: i64, target: &Path) -> Result<RestoreOutcome> {
    let data = load_version_bytes(state, version_id)?;
    write_atomic(target, &data)?;
    Ok(RestoreOutcome {
        path: target.to_string_lossy().to_string(),
        tracked: is_tracked(state, target),
        bytes_written: data.len() as i64,
    })
}

pub fn restore_in_place(state: &Arc<AppState>, version_id: i64) -> Result<RestoreOutcome> {
    let version = state
        .db
        .version_row(version_id)?
        .ok_or_else(|| anyhow!("version {version_id} does not exist"))?;
    let file = state
        .db
        .file_row(version.file_id)?
        .ok_or_else(|| anyhow!("the file of this version no longer exists"))?;
    let folder = state
        .db
        .folder_row(file.folder_id)?
        .ok_or_else(|| anyhow!("the watched folder no longer exists"))?;
    let target = safe_join(Path::new(&folder.path), &file.rel_path)?;

    let data = load_version_bytes(state, version_id)?;

    if target.exists() {
        let current = fs::read(&target).with_context(|| format!("cannot read {}", target.display()))?;
        if blake3::hash(&current).to_hex().as_str() != version.content_hash {
            snapshot_for_restore(state, folder.id, &target)
                .context("restore cancelled: the current content could not be saved first")?;
        }
    }

    write_contained(Path::new(&folder.path), &target, &data)?;
    if let Err(err) = record_restored(state, folder.id, &target) {
        log::warn!("restored {} but could not record the restore: {err:#}", target.display());
    }
    Ok(RestoreOutcome {
        path: target.to_string_lossy().to_string(),
        tracked: true,
        bytes_written: data.len() as i64,
    })
}

pub fn restore_folder_at(
    state: &Arc<AppState>,
    folder_id: i64,
    ts: i64,
    target_dir: &Path,
) -> Result<Vec<String>> {
    state
        .db
        .folder_row(folder_id)?
        .ok_or_else(|| anyhow!("the watched folder no longer exists"))?;
    let mut written = Vec::new();
    for file in state.db.list_file_rows(folder_id)? {
        let Some(version) = state.db.version_at(file.id, ts)? else {
            continue;
        };
        if version.event == "deleted" {
            continue;
        }
        let name_at_time = state
            .db
            .path_at(file.id, ts)?
            .unwrap_or_else(|| file.rel_path.clone());
        let data = load_version_bytes(state, version.id)?;
        let target = safe_join(target_dir, &name_at_time)?;
        write_contained(target_dir, &target, &data)?;
        written.push(target.to_string_lossy().to_string());
    }
    Ok(written)
}

pub fn default_restore_name(rel_path: &str, ts: i64) -> String {
    let path = Path::new(rel_path);
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let extension = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let stamp = chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H-%M")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string());
    let name = format!("{stem} ({stamp}){extension}");
    if parent.as_os_str().is_empty() {
        name
    } else {
        parent.join(name).to_string_lossy().to_string()
    }
}
