use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderInfo {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    pub retention_days: i64,
    pub thinning: bool,
    pub exclude_globs: String,
    pub max_file_mb: i64,
    pub paused: bool,
    pub created_at: i64,
    pub file_count: i64,
    pub version_count: i64,
    pub stored_bytes: i64,
    pub raw_bytes: i64,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub id: i64,
    pub folder_id: i64,
    pub rel_path: String,
    pub status: String,
    pub size: i64,
    pub mtime: i64,
    pub content_hash: Option<String>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub version_count: i64,
    pub latest_version_id: Option<i64>,
    pub latest_ts: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub id: i64,
    pub file_id: i64,
    pub ts: i64,
    pub event: String,
    pub size: i64,
    pub content_hash: String,
    pub blob_id: i64,
    pub prev_path: Option<String>,
    pub label: Option<String>,
    pub pinned: bool,
    pub added: i64,
    pub removed: i64,
    pub snippet: Option<String>,
    pub is_text: bool,
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    pub tag: String,
    pub old_line: Option<i64>,
    pub new_line: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    pub is_text: bool,
    pub kind: String,
    pub added: i64,
    pub removed: i64,
    pub old_label: String,
    pub new_label: String,
    pub old_size: i64,
    pub new_size: i64,
    pub old_hash: String,
    pub new_hash: String,
    pub rows: Vec<DiffRow>,
    pub unified: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionPreview {
    pub version: VersionInfo,
    pub is_text: bool,
    pub kind: String,
    pub text: Option<String>,
    pub diff: Option<CompareResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStats {
    pub folders: i64,
    pub files: i64,
    pub versions: i64,
    pub blobs: i64,
    pub raw_bytes: i64,
    pub stored_bytes: i64,
    pub db_bytes: i64,
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub data_dir: String,
    pub default_retention_days: i64,
    pub default_thinning: bool,
    pub default_max_file_mb: i64,
    pub default_exclude_globs: String,
    pub autostart: bool,
    pub external_diff_tool: String,
    pub reconcile_interval_minutes: i64,
    pub paused: bool,
    pub start_minimized: bool,
    pub show_deleted: bool,
    pub keep_all_hours: i64,
    pub hourly_until_days: i64,
    pub daily_until_days: i64,
    pub max_versions_per_file: i64,
    pub cleanup_interval_hours: i64,
    pub clean_removed_folders: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            data_dir: String::new(),
            default_retention_days: 90,
            default_thinning: true,
            default_max_file_mb: 128,
            default_exclude_globs: String::new(),
            autostart: false,
            external_diff_tool: String::new(),
            reconcile_interval_minutes: 10,
            paused: false,
            start_minimized: true,
            show_deleted: true,
            keep_all_hours: 24,
            hourly_until_days: 7,
            daily_until_days: 30,
            max_versions_per_file: 0,
            cleanup_interval_hours: 1,
            clean_removed_folders: false,
        }
    }
}

impl AppSettings {
    pub fn to_stored_json(&self) -> serde_json::Result<String> {
        let mut stored = self.clone();
        stored.autostart = false;
        serde_json::to_string(&stored)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionPreview {
    pub total_versions: i64,
    pub removable_versions: i64,
    pub removable_bytes: i64,
    pub folders: Vec<FolderRetentionPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRetentionPreview {
    pub folder_id: i64,
    pub path: String,
    pub total_versions: i64,
    pub removable_versions: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub removed_versions: i64,
    pub removed_blobs: i64,
    pub freed_bytes: i64,
    pub scanned_folders: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub path: String,
    pub tracked: bool,
    pub bytes_written: i64,
}
