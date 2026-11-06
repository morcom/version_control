use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::app::AppState;
use crate::db::{normalize_path, now_millis};
use crate::ingest::{compare_versions, default_exclude_patterns, load_version_bytes, version_text};
use crate::model::{
    AppSettings, CleanupReport, CompareResult, FileInfo, FolderInfo, RestoreOutcome, RetentionPreview,
    StorageStats, VersionInfo, VersionPreview,
};
use crate::restore::{default_restore_name, restore_as, restore_folder_at, restore_in_place};
use crate::watcher::Job;

type CmdResult<T> = Result<T, String>;

fn fail(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn state_ref(state: &State<'_, Arc<AppState>>) -> Arc<AppState> {
    Arc::clone(state.inner())
}

async fn blocking<T, F>(state: &State<'_, Arc<AppState>>, f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<AppState>) -> anyhow::Result<T> + Send + 'static,
{
    let app_state = state_ref(state);
    tauri::async_runtime::spawn_blocking(move || f(app_state))
        .await
        .map_err(|e| format!("background task failed: {e}"))?
        .map_err(fail)
}

#[tauri::command]
pub async fn list_folders(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<FolderInfo>> {
    blocking(&state, |s| s.db.list_folders()).await
}

#[tauri::command]
pub async fn add_folder(
    state: State<'_, Arc<AppState>>,
    path: String,
    retention_days: Option<i64>,
    thinning: Option<bool>,
    exclude_globs: Option<String>,
    max_file_mb: Option<i64>,
) -> CmdResult<FolderInfo> {
    blocking(&state, move |s| {
        let folder_path = normalize_path(Path::new(&path));
        let folder = PathBuf::from(&folder_path);
        if !folder.is_dir() {
            anyhow::bail!("{folder_path} is not a folder");
        }
        let settings = s.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        crate::ingest::ensure_folder_inside_data_dir(&folder, Path::new(&settings.data_dir))?;
        let existing_same = s.db.folder_row_by_path(&folder_path)?;
        for existing in s.db.list_folder_rows()? {
            if existing_same.as_ref().map(|e| e.id) == Some(existing.id) {
                continue;
            }
            if crate::ingest::paths_overlap(&folder, Path::new(&existing.path)) {
                anyhow::bail!("{folder_path} overlaps with the watched folder {}", existing.path);
            }
        }
        let globs = exclude_globs.unwrap_or_else(|| settings.default_exclude_globs.clone());
        crate::ingest::validate_patterns(&globs)?;
        let id = match existing_same {
            Some(row) if row.archived => {
                s.db.unarchive_folder(row.id)?;
                row.id
            }
            Some(_) => anyhow::bail!("{folder_path} is already watched"),
            None => s.db.insert_folder(
                &folder_path,
                retention_days.unwrap_or(settings.default_retention_days).max(1),
                thinning.unwrap_or(settings.default_thinning),
                &globs,
                max_file_mb.unwrap_or(settings.default_max_file_mb).max(1),
                now_millis(),
            )?,
        };
        s.refresh_watchers()?;
        s.send(Job::Reconcile { folder_id: id });
        s.db.list_folders()?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| anyhow::anyhow!("folder was not stored"))
    })
    .await
}

#[tauri::command]
pub async fn resume_folder(state: State<'_, Arc<AppState>>, id: i64) -> CmdResult<()> {
    blocking(&state, move |s| {
        let row = s
            .db
            .folder_row(id)?
            .ok_or_else(|| anyhow::anyhow!("folder {id} does not exist"))?;
        if !row.archived {
            return Ok(());
        }
        if !Path::new(&row.path).is_dir() {
            anyhow::bail!("{} is not available", row.path);
        }
        for existing in s.db.list_folder_rows()? {
            if crate::ingest::paths_overlap(Path::new(&row.path), Path::new(&existing.path)) {
                anyhow::bail!("{} overlaps with the watched folder {}", row.path, existing.path);
            }
        }
        s.db.unarchive_folder(id)?;
        s.refresh_watchers()?;
        s.send(Job::Reconcile { folder_id: id });
        s.notify();
        Ok(())
    })
    .await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn update_folder(
    state: State<'_, Arc<AppState>>,
    id: i64,
    enabled: bool,
    retention_days: i64,
    thinning: bool,
    exclude_globs: String,
    max_file_mb: i64,
    paused: bool,
) -> CmdResult<Vec<FolderInfo>> {
    blocking(&state, move |s| {
        if retention_days < 1 || max_file_mb < 1 {
            anyhow::bail!("retention days and maximum file size must be at least 1");
        }
        crate::ingest::validate_patterns(&exclude_globs)?;
        let before = s.db.folder_row(id)?;
        s.db.update_folder(id, enabled, retention_days, thinning, &exclude_globs, max_file_mb, paused)?;
        if let Err(err) = s.refresh_watchers() {
            log::warn!("cannot refresh watchers: {err:#}");
        }
        let resumed = before.map(|b| (!b.enabled || b.paused) && enabled && !paused).unwrap_or(false);
        if resumed {
            s.send(Job::Reconcile { folder_id: id });
        }
        s.notify();
        s.db.list_folders()
    })
    .await
}

#[tauri::command]
pub async fn remove_folder(state: State<'_, Arc<AppState>>, id: i64, delete_history: bool) -> CmdResult<()> {
    blocking(&state, move |s| {
        s.db.remove_folder(id, delete_history)?;
        if let Err(err) = s.refresh_watchers() {
            log::warn!("cannot refresh watchers: {err:#}");
        }
        s.notify();
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn list_files(
    state: State<'_, Arc<AppState>>,
    folder_id: i64,
    include_deleted: bool,
    query: String,
) -> CmdResult<Vec<FileInfo>> {
    blocking(&state, move |s| s.db.list_files(folder_id, include_deleted, &query)).await
}

#[tauri::command]
pub async fn get_history(state: State<'_, Arc<AppState>>, file_id: i64) -> CmdResult<Vec<VersionInfo>> {
    blocking(&state, move |s| s.db.list_versions(file_id)).await
}

#[tauri::command]
pub async fn get_preview(state: State<'_, Arc<AppState>>, version_id: i64) -> CmdResult<VersionPreview> {
    blocking(&state, move |s| {
        let version = s
            .db
            .version_info(version_id)?
            .ok_or_else(|| anyhow::anyhow!("version {version_id} does not exist"))?;
        let kind = crate::extract::kind_for_path(Path::new(&version.rel_path), &[]);
        if version.event == "deleted" {
            return Ok(VersionPreview {
                is_text: version.is_text,
                kind: "deleted".to_string(),
                text: None,
                diff: None,
                version,
            });
        }
        let previous = s.db.previous_version_id(version.file_id, version.ts, version.id)?;
        let diff = match previous {
            Some(older) => compare_versions(&s, older, version_id).ok(),
            None => None,
        };
        let text = if diff.is_none() && version.is_text {
            version_text(&s, version_id).map(|t| {
                if t.len() > 512 * 1024 {
                    let mut cut = 512 * 1024;
                    while !t.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}\n... preview truncated ...", &t[..cut])
                } else {
                    t
                }
            })
        } else {
            None
        };
        let is_text = version.is_text || diff.as_ref().map(|d| d.is_text).unwrap_or(false);
        Ok(VersionPreview {
            is_text,
            kind: if is_text {
                kind.as_str().to_string()
            } else if kind == crate::extract::ContentKind::Text {
                "binary".to_string()
            } else {
                kind.as_str().to_string()
            },
            text,
            diff,
            version,
        })
    })
    .await
}

#[tauri::command]
pub async fn compare(state: State<'_, Arc<AppState>>, old_id: i64, new_id: i64) -> CmdResult<CompareResult> {
    blocking(&state, move |s| compare_versions(&s, old_id, new_id)).await
}

#[tauri::command]
pub async fn restore_version_as(
    state: State<'_, Arc<AppState>>,
    version_id: i64,
    target_path: String,
) -> CmdResult<RestoreOutcome> {
    blocking(&state, move |s| restore_as(&s, version_id, Path::new(&target_path))).await
}

#[tauri::command]
pub async fn restore_version_in_place(
    state: State<'_, Arc<AppState>>,
    version_id: i64,
) -> CmdResult<RestoreOutcome> {
    blocking(&state, move |s| {
        let outcome = restore_in_place(&s, version_id)?;
        s.notify();
        Ok(outcome)
    })
    .await
}

#[tauri::command]
pub async fn restore_folder_to_point_in_time(
    state: State<'_, Arc<AppState>>,
    folder_id: i64,
    ts: i64,
    target_dir: String,
) -> CmdResult<Vec<String>> {
    blocking(&state, move |s| restore_folder_at(&s, folder_id, ts, Path::new(&target_dir))).await
}

#[tauri::command]
pub async fn suggest_restore_path(state: State<'_, Arc<AppState>>, version_id: i64) -> CmdResult<String> {
    blocking(&state, move |s| {
        let version = s
            .db
            .version_row(version_id)?
            .ok_or_else(|| anyhow::anyhow!("version does not exist"))?;
        let file = s
            .db
            .file_row(version.file_id)?
            .ok_or_else(|| anyhow::anyhow!("file does not exist"))?;
        let folder = s
            .db
            .folder_row(file.folder_id)?
            .ok_or_else(|| anyhow::anyhow!("folder does not exist"))?;
        let name = default_restore_name(&file.rel_path, version.ts);
        Ok(PathBuf::from(&folder.path).join(name).to_string_lossy().to_string())
    })
    .await
}

#[tauri::command]
pub async fn set_version_pinned(state: State<'_, Arc<AppState>>, version_id: i64, pinned: bool) -> CmdResult<()> {
    blocking(&state, move |s| s.db.set_version_pin(version_id, pinned)).await
}

#[tauri::command]
pub async fn set_version_label(
    state: State<'_, Arc<AppState>>,
    version_id: i64,
    label: Option<String>,
) -> CmdResult<()> {
    blocking(&state, move |s| s.db.set_version_label(version_id, label.as_deref())).await
}

#[tauri::command]
pub async fn snapshot_all(state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    blocking(&state, |s| {
        for folder in s.db.list_folder_rows()? {
            s.send(Job::Snapshot { folder_id: folder.id });
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn rescan_all(state: State<'_, Arc<AppState>>) -> CmdResult<()> {
    blocking(&state, |s| {
        for folder in s.db.list_folder_rows()? {
            s.send(Job::Reconcile { folder_id: folder.id });
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn run_cleanup_now(state: State<'_, Arc<AppState>>) -> CmdResult<CleanupReport> {
    blocking(&state, |s| {
        let report = crate::retention::run_cleanup(&s, now_millis())?;
        s.notify();
        Ok(report)
    })
    .await
}

#[tauri::command]
pub async fn preview_cleanup(state: State<'_, Arc<AppState>>) -> CmdResult<RetentionPreview> {
    blocking(&state, |s| crate::retention::preview_cleanup(&s, now_millis())).await
}

#[tauri::command]
pub async fn apply_retention_to_all_folders(
    state: State<'_, Arc<AppState>>,
    retention_days: i64,
    thinning: bool,
) -> CmdResult<Vec<FolderInfo>> {
    blocking(&state, move |s| {
        s.db.set_retention_for_all(retention_days.max(1), thinning)?;
        s.reload_folders()?;
        s.notify();
        s.db.list_folders()
    })
    .await
}

#[tauri::command]
pub async fn get_stats(state: State<'_, Arc<AppState>>) -> CmdResult<StorageStats> {
    blocking(&state, |s| {
        let (folders, files, versions, blobs, raw_bytes, stored_bytes) = s.db.storage_stats()?;
        Ok(StorageStats {
            folders,
            files,
            versions,
            blobs,
            raw_bytes,
            stored_bytes,
            db_bytes: s.db.db_file_size()?,
            data_dir: s.settings.read().unwrap_or_else(|e| e.into_inner()).data_dir.clone(),
        })
    })
    .await
}

#[tauri::command]
pub async fn check_integrity(state: State<'_, Arc<AppState>>) -> CmdResult<Vec<String>> {
    blocking(&state, |s| {
        let _storage = s.storage_lock.read().unwrap_or_else(|e| e.into_inner());
        let mut problems = s.db.check_integrity()?;
        let blobs: Vec<(String, i64)> = s
            .db
            .all_blobs()?
            .into_iter()
            .map(|b| (b.storage_hash, b.stored_size))
            .collect();
        problems.extend(s.store.check_files(&blobs)?);
        Ok(problems)
    })
    .await
}

#[tauri::command]
pub fn get_settings(app: AppHandle, state: State<'_, Arc<AppState>>) -> CmdResult<AppSettings> {
    use tauri_plugin_autostart::ManagerExt;
    let mut settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Ok(enabled) = app.autolaunch().is_enabled() {
        settings.autostart = enabled;
    }
    Ok(settings)
}

#[tauri::command]
pub async fn save_settings(state: State<'_, Arc<AppState>>, settings: AppSettings) -> CmdResult<AppSettings> {
    blocking(&state, move |s| {
        crate::ingest::validate_patterns(&settings.default_exclude_globs)?;
        let merged = {
            let mut guard = s.settings.write().unwrap_or_else(|e| e.into_inner());
            let mut next = settings;
            next.data_dir = guard.data_dir.clone();
            next.paused = guard.paused;
            *guard = next.clone();
            next
        };
        s.db.set_setting("app", &merged.to_stored_json()?)?;
        s.db.delete_setting("app.bak")?;
        if let Err(err) = s.refresh_watchers() {
            log::warn!("cannot refresh watchers: {err:#}");
        }
        s.emit("settings-changed");
        Ok(merged)
    })
    .await
}

#[tauri::command]
pub async fn set_paused(state: State<'_, Arc<AppState>>, paused: bool) -> CmdResult<()> {
    blocking(&state, move |s| {
        let was = s.paused();
        s.set_paused(paused)?;
        if was && !paused {
            for folder in s.db.list_folder_rows()? {
                s.send(Job::Reconcile { folder_id: folder.id });
            }
        }
        if let Some(handle) = s.handle.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            crate::update_tray_pause_label(handle, paused);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub fn default_excludes() -> Vec<String> {
    default_exclude_patterns()
}

#[tauri::command]
pub fn reveal_path(path: String) -> CmdResult<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(format!("/select,{path}"))
            .spawn()
            .map_err(|e| format!("cannot reveal {path}: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("revealing files is only supported on Windows".to_string())
    }
}

#[tauri::command]
pub async fn open_version(app: AppHandle, state: State<'_, Arc<AppState>>, version_id: i64) -> CmdResult<()> {
    let path = blocking(&state, move |s| {
        let version = s
            .db
            .version_row(version_id)?
            .ok_or_else(|| anyhow::anyhow!("version does not exist"))?;
        let bytes = load_version_bytes(&s, version_id)?;
        crate::tempfiles::write_copy(version_id, &version.rel_path, &bytes)
    })
    .await?;
    if !crate::tempfiles::can_open_directly(&path) {
        return reveal_path(path.to_string_lossy().to_string());
    }
    app.opener()
        .open_path(path.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

#[tauri::command]
pub async fn open_in_diff_tool(state: State<'_, Arc<AppState>>, old_id: i64, new_id: i64) -> CmdResult<()> {
    blocking(&state, move |s| {
        let tool = s
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .external_diff_tool
            .clone();
        if tool.trim().is_empty() {
            anyhow::bail!("no external diff tool is configured");
        }
        let mut paths = Vec::new();
        for id in [old_id, new_id] {
            let version = s
                .db
                .version_row(id)?
                .ok_or_else(|| anyhow::anyhow!("version {id} does not exist"))?;
            paths.push(crate::tempfiles::write_copy(id, &version.rel_path, &load_version_bytes(&s, id)?)?);
        }
        std::process::Command::new(&tool)
            .args(&paths)
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start {tool}: {e}"))?;
        Ok(())
    })
    .await
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> CmdResult<bool> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled { manager.enable() } else { manager.disable() };
    result.map_err(|e| format!("cannot change the autostart setting: {e}"))?;
    manager
        .is_enabled()
        .map_err(|e| format!("cannot read the autostart setting: {e}"))
}
