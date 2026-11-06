import { invoke } from "@tauri-apps/api/core";
import type {
  AppSettings,
  CleanupReport,
  CompareResult,
  FileInfo,
  FolderInfo,
  RestoreOutcome,
  RetentionPreview,
  StorageStats,
  VersionInfo,
  VersionPreview,
} from "./types";

export const api = {
  listFolders: () => invoke<FolderInfo[]>("list_folders"),
  addFolder: (path: string, retentionDays?: number, thinning?: boolean, excludeGlobs?: string, maxFileMb?: number) =>
    invoke<FolderInfo>("add_folder", { path, retentionDays, thinning, excludeGlobs, maxFileMb }),
  updateFolder: (
    id: number,
    enabled: boolean,
    retentionDays: number,
    thinning: boolean,
    excludeGlobs: string,
    maxFileMb: number,
    paused: boolean,
  ) => invoke<FolderInfo[]>("update_folder", { id, enabled, retentionDays, thinning, excludeGlobs, maxFileMb, paused }),
  removeFolder: (id: number, deleteHistory: boolean) => invoke<void>("remove_folder", { id, deleteHistory }),
  resumeFolder: (id: number) => invoke<void>("resume_folder", { id }),
  listFiles: (folderId: number, includeDeleted: boolean, query: string) =>
    invoke<FileInfo[]>("list_files", { folderId, includeDeleted, query }),
  getHistory: (fileId: number) => invoke<VersionInfo[]>("get_history", { fileId }),
  getPreview: (versionId: number) => invoke<VersionPreview>("get_preview", { versionId }),
  compare: (oldId: number, newId: number) => invoke<CompareResult>("compare", { oldId, newId }),
  restoreAs: (versionId: number, targetPath: string) =>
    invoke<RestoreOutcome>("restore_version_as", { versionId, targetPath }),
  restoreVersionInPlace: (versionId: number) => invoke<RestoreOutcome>("restore_version_in_place", { versionId }),
  restoreFolderAt: (folderId: number, ts: number, targetDir: string) =>
    invoke<string[]>("restore_folder_to_point_in_time", { folderId, ts, targetDir }),
  suggestRestorePath: (versionId: number) => invoke<string>("suggest_restore_path", { versionId }),
  setPinned: (versionId: number, pinned: boolean) => invoke<void>("set_version_pinned", { versionId, pinned }),
  setLabel: (versionId: number, label: string | null) => invoke<void>("set_version_label", { versionId, label }),
  snapshotAll: () => invoke<void>("snapshot_all"),
  rescanAll: () => invoke<void>("rescan_all"),
  runCleanup: () => invoke<CleanupReport>("run_cleanup_now"),
  previewCleanup: () => invoke<RetentionPreview>("preview_cleanup"),
  applyRetentionToAll: (retentionDays: number, thinning: boolean) =>
    invoke<FolderInfo[]>("apply_retention_to_all_folders", { retentionDays, thinning }),
  getStats: () => invoke<StorageStats>("get_stats"),
  checkIntegrity: () => invoke<string[]>("check_integrity"),
  getSettings: () => invoke<AppSettings>("get_settings"),
  saveSettings: (settings: AppSettings) => invoke<AppSettings>("save_settings", { settings }),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  defaultExcludes: () => invoke<string[]>("default_excludes"),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  openVersion: (versionId: number) => invoke<void>("open_version", { versionId }),
  openInDiffTool: (oldId: number, newId: number) => invoke<void>("open_in_diff_tool", { oldId, newId }),
  setAutostart: (enabled: boolean) => invoke<boolean>("set_autostart", { enabled }),
};
