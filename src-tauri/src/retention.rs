use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;

use crate::app::AppState;
use crate::db::{BlobRow, VersionBrief};
use crate::model::{AppSettings, CleanupReport, FolderRetentionPreview, RetentionPreview};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;
const WEEK: i64 = 7 * DAY;
const GC_GRACE_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub keep_all_ms: i64,
    pub hourly_until_ms: i64,
    pub daily_until_ms: i64,
    pub max_versions: usize,
}

impl Policy {
    pub fn from_settings(settings: &AppSettings) -> Self {
        let keep_all_ms = settings.keep_all_hours.max(0) * HOUR;
        let hourly_until_ms = (settings.hourly_until_days.max(0) * DAY).max(keep_all_ms);
        let daily_until_ms = (settings.daily_until_days.max(0) * DAY).max(hourly_until_ms);
        Self {
            keep_all_ms,
            hourly_until_ms,
            daily_until_ms,
            max_versions: settings.max_versions_per_file.max(0) as usize,
        }
    }

    fn bucket_of(&self, ts: i64, now: i64) -> Option<(u8, i64)> {
        let age = now - ts;
        if age < self.keep_all_ms {
            None
        } else if age < self.hourly_until_ms {
            Some((1, ts.div_euclid(HOUR)))
        } else if age < self.daily_until_ms {
            Some((2, ts.div_euclid(DAY)))
        } else {
            Some((3, ts.div_euclid(WEEK)))
        }
    }
}

fn plan_file(versions: &[VersionBrief], policy: &Policy, cutoff: i64, thinning: bool, now: i64, out: &mut Vec<i64>) {
    if versions.is_empty() {
        return;
    }
    let latest_id = versions[0].id;
    let mut keep: HashSet<i64> = HashSet::new();
    keep.insert(latest_id);
    let mut seen_buckets: HashSet<(u8, i64)> = HashSet::new();
    for v in versions {
        if v.pinned {
            keep.insert(v.id);
            continue;
        }
        if v.ts < cutoff {
            continue;
        }
        if !thinning {
            keep.insert(v.id);
            continue;
        }
        match policy.bucket_of(v.ts, now) {
            Some(bucket) => {
                if seen_buckets.insert(bucket) {
                    keep.insert(v.id);
                }
            }
            None => {
                keep.insert(v.id);
            }
        }
    }
    if policy.max_versions > 0 {
        let mut kept: Vec<i64> = versions
            .iter()
            .filter(|v| keep.contains(&v.id) && !v.pinned)
            .map(|v| v.id)
            .collect();
        while kept.len() > policy.max_versions {
            if let Some(oldest) = kept.pop()
                && oldest != latest_id {
                    keep.remove(&oldest);
                }
        }
    }
    out.extend(versions.iter().filter(|v| !keep.contains(&v.id)).map(|v| v.id));
}

struct Plan {
    remove_versions: Vec<i64>,
    per_folder: Vec<FolderRetentionPreview>,
    total_versions: i64,
}

fn plan(state: &Arc<AppState>, now: i64) -> Result<Plan> {
    let (policy, include_removed) = {
        let settings = state.settings.read().unwrap_or_else(|e| e.into_inner());
        (Policy::from_settings(&settings), settings.clean_removed_folders)
    };
    let mut remove_versions = Vec::new();
    let mut per_folder = Vec::new();
    let mut total_versions = 0i64;

    let folders = if include_removed {
        state.db.list_all_folder_rows()?
    } else {
        state.db.list_folder_rows()?
    };
    for folder in folders {
        let cutoff = now - folder.retention_days.max(1) * DAY;
        let briefs = state.db.version_briefs_for_folder(folder.id)?;
        let before = remove_versions.len();
        for group in briefs.chunk_by(|a, b| a.file_id == b.file_id) {
            plan_file(group, &policy, cutoff, folder.thinning, now, &mut remove_versions);
        }
        total_versions += briefs.len() as i64;
        per_folder.push(FolderRetentionPreview {
            folder_id: folder.id,
            path: folder.path.clone(),
            total_versions: briefs.len() as i64,
            removable_versions: (remove_versions.len() - before) as i64,
        });
    }
    Ok(Plan {
        remove_versions,
        per_folder,
        total_versions,
    })
}

fn deletable_blobs(state: &Arc<AppState>, now: i64, removed_versions: &HashSet<i64>) -> Result<Vec<BlobRow>> {
    let all = state.db.all_blobs()?;
    let base_of: HashMap<i64, i64> = all
        .iter()
        .filter(|b| b.kind == "delta")
        .map(|b| (b.id, b.base_id))
        .collect();
    let mut roots = state.db.keyframe_blob_ids()?;
    for (version_id, blob_id) in state.db.version_blob_ids()? {
        if removed_versions.contains(&version_id) {
            continue;
        }
        roots.insert(blob_id);
        if let Some(base) = base_of.get(&blob_id) {
            roots.insert(*base);
        }
    }
    let old_enough: HashSet<i64> = state
        .db
        .blobs_created_before(now - GC_GRACE_MS)?
        .into_iter()
        .map(|b| b.id)
        .collect();
    Ok(all
        .into_iter()
        .filter(|b| old_enough.contains(&b.id) && !roots.contains(&b.id))
        .collect())
}

pub fn preview_cleanup(state: &Arc<AppState>, now: i64) -> Result<RetentionPreview> {
    let plan = plan(state, now)?;
    let removed: HashSet<i64> = plan.remove_versions.iter().copied().collect();
    let removable_bytes = deletable_blobs(state, now, &removed)?
        .iter()
        .map(|b| b.stored_size)
        .sum();
    Ok(RetentionPreview {
        total_versions: plan.total_versions,
        removable_versions: plan.remove_versions.len() as i64,
        removable_bytes,
        folders: plan.per_folder,
    })
}

pub fn run_cleanup(state: &Arc<AppState>, now: i64) -> Result<CleanupReport> {
    let _storage = state.storage_lock.write().unwrap_or_else(|e| e.into_inner());
    let plan = plan(state, now)?;
    let scanned_folders = plan.per_folder.len() as i64;
    let removed_versions = if plan.remove_versions.is_empty() {
        0
    } else {
        state.db.delete_versions(&plan.remove_versions)?
    };

    let mut removed_blobs = 0i64;
    let mut freed_bytes = 0i64;
    for blob in deletable_blobs(state, now, &HashSet::new())? {
        state.db.delete_blob(blob.id)?;
        state.store.remove_stored(&blob.storage_hash);
        freed_bytes += blob.stored_size;
        removed_blobs += 1;
    }

    state.db.clear_stale_keyframes()?;
    state.db.prune_text_cache()?;

    Ok(CleanupReport {
        removed_versions,
        removed_blobs,
        freed_bytes,
        scanned_folders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::from_settings(&AppSettings::default())
    }

    #[test]
    fn recent_versions_are_never_bucketed() {
        let now = 100 * DAY;
        assert_eq!(policy().bucket_of(now - HOUR, now), None);
    }

    #[test]
    fn tiers_follow_the_configured_windows() {
        let now = 400 * DAY;
        let p = policy();
        assert_eq!(p.bucket_of(now - 2 * DAY, now).unwrap().0, 1);
        assert_eq!(p.bucket_of(now - 10 * DAY, now).unwrap().0, 2);
        assert_eq!(p.bucket_of(now - 60 * DAY, now).unwrap().0, 3);
    }

    #[test]
    fn windows_are_monotonic_even_with_inconsistent_input() {
        let settings = AppSettings {
            keep_all_hours: 72,
            hourly_until_days: 1,
            daily_until_days: 0,
            ..AppSettings::default()
        };
        let p = Policy::from_settings(&settings);
        assert!(p.hourly_until_ms >= p.keep_all_ms);
        assert!(p.daily_until_ms >= p.hourly_until_ms);
    }

    #[test]
    fn plan_keeps_latest_and_pinned_and_thins() {
        let now = 100 * DAY;
        let v = |id, age_days: i64, pinned| VersionBrief {
            id,
            file_id: 1,
            ts: now - age_days * DAY,
            pinned,
        };
        let versions = vec![v(5, 0, false), v(4, 3, false), v(3, 3, false), v(2, 200, true), v(1, 200, false)];
        let mut out = Vec::new();
        plan_file(&versions, &policy(), now - 90 * DAY, true, now, &mut out);
        out.sort();
        assert_eq!(out, vec![1, 3]);
    }
}
