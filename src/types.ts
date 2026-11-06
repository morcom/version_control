export type FolderInfo = {
  id: number;
  path: string;
  enabled: boolean;
  retentionDays: number;
  thinning: boolean;
  excludeGlobs: string;
  maxFileMb: number;
  paused: boolean;
  createdAt: number;
  fileCount: number;
  versionCount: number;
  storedBytes: number;
  rawBytes: number;
  archived: boolean;
};

export type FileInfo = {
  id: number;
  folderId: number;
  relPath: string;
  status: string;
  size: number;
  mtime: number;
  contentHash: string | null;
  firstSeen: number;
  lastSeen: number;
  versionCount: number;
  latestVersionId: number | null;
  latestTs: number | null;
};

export type VersionInfo = {
  id: number;
  fileId: number;
  ts: number;
  event: string;
  size: number;
  contentHash: string;
  blobId: number;
  prevPath: string | null;
  label: string | null;
  pinned: boolean;
  added: number;
  removed: number;
  snippet: string | null;
  isText: boolean;
  relPath: string;
};

export type DiffRow = {
  tag: "same" | "ins" | "del" | "skip";
  oldLine: number | null;
  newLine: number | null;
  text: string;
};

export type CompareResult = {
  isText: boolean;
  kind: string;
  added: number;
  removed: number;
  oldLabel: string;
  newLabel: string;
  oldSize: number;
  newSize: number;
  oldHash: string;
  newHash: string;
  rows: DiffRow[];
  unified: string;
  truncated: boolean;
};

export type VersionPreview = {
  version: VersionInfo;
  isText: boolean;
  kind: string;
  text: string | null;
  diff: CompareResult | null;
};

export type StorageStats = {
  folders: number;
  files: number;
  versions: number;
  blobs: number;
  rawBytes: number;
  storedBytes: number;
  dbBytes: number;
  dataDir: string;
};

export type AppSettings = {
  dataDir: string;
  defaultRetentionDays: number;
  defaultThinning: boolean;
  defaultMaxFileMb: number;
  defaultExcludeGlobs: string;
  autostart: boolean;
  externalDiffTool: string;
  reconcileIntervalMinutes: number;
  paused: boolean;
  startMinimized: boolean;
  showDeleted: boolean;
  keepAllHours: number;
  hourlyUntilDays: number;
  dailyUntilDays: number;
  maxVersionsPerFile: number;
  cleanupIntervalHours: number;
  cleanRemovedFolders: boolean;
};

export type FolderRetentionPreview = {
  folderId: number;
  path: string;
  totalVersions: number;
  removableVersions: number;
};

export type RetentionPreview = {
  totalVersions: number;
  removableVersions: number;
  removableBytes: number;
  folders: FolderRetentionPreview[];
};

export type CleanupReport = {
  removedVersions: number;
  removedBlobs: number;
  freedBytes: number;
  scannedFolders: number;
};

export type RestoreOutcome = {
  path: string;
  tracked: boolean;
  bytesWritten: number;
};
