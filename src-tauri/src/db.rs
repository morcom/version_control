use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{FileInfo, FolderInfo, VersionInfo};

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct FolderRow {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    pub retention_days: i64,
    pub thinning: bool,
    pub exclude_globs: String,
    pub max_file_mb: i64,
    pub paused: bool,
    pub created_at: i64,
    pub archived: bool,
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub folder_id: i64,
    pub rel_path: String,
    pub status: String,
    pub size: i64,
    pub mtime: i64,
    pub content_hash: Option<String>,
    pub keyframe_blob_id: Option<i64>,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone)]
pub struct BlobRow {
    pub id: i64,
    pub kind: String,
    pub content_hash: String,
    pub base_id: i64,
    pub stored_size: i64,
    pub raw_size: i64,
    pub storage_hash: String,
}

#[derive(Debug, Clone)]
pub struct VersionRow {
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

pub const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Copy)]
pub enum BlobRef<'a> {
    Existing(i64),
    New {
        kind: &'a str,
        base_id: i64,
        stored_size: i64,
        storage_hash: &'a str,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum KeyframeUpdate {
    UseBlob,
    Keep(Option<i64>),
}

pub struct NewVersion<'a> {
    pub file_id: i64,
    pub ts: i64,
    pub event: &'a str,
    pub size: i64,
    pub mtime: i64,
    pub content_hash: &'a str,
    pub blob: BlobRef<'a>,
    pub keyframe: KeyframeUpdate,
    pub prev_path: Option<&'a str>,
    pub added: i64,
    pub removed: i64,
    pub snippet: Option<&'a str>,
    pub is_text: bool,
}

#[derive(Debug, Clone)]
pub struct VersionBrief {
    pub id: i64,
    pub file_id: i64,
    pub ts: i64,
    pub pinned: bool,
}

const FILE_COLUMNS: &str =
    "id, folder_id, rel_path, status, size, mtime, content_hash, keyframe_blob_id, first_seen, last_seen";

fn map_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        rel_path: row.get(2)?,
        status: row.get(3)?,
        size: row.get(4)?,
        mtime: row.get(5)?,
        content_hash: row.get(6)?,
        keyframe_blob_id: row.get(7)?,
        first_seen: row.get(8)?,
        last_seen: row.get(9)?,
    })
}

const FOLDER_COLUMNS: &str =
    "id, path, enabled, retention_days, thinning, exclude_globs, max_file_mb, paused, created_at, archived";

fn map_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<FolderRow> {
    Ok(FolderRow {
        id: row.get(0)?,
        path: row.get(1)?,
        enabled: row.get(2)?,
        retention_days: row.get(3)?,
        thinning: row.get(4)?,
        exclude_globs: row.get(5)?,
        max_file_mb: row.get(6)?,
        paused: row.get(7)?,
        created_at: row.get(8)?,
        archived: row.get(9)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("cannot open database")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate(path)?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn migrate(&self, path: &Path) -> Result<()> {
        let conn = self.conn();
        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current > SCHEMA_VERSION {
            return Err(anyhow!(
                "the history database was created by a newer version of the app (schema {current}, supported {SCHEMA_VERSION})"
            ));
        }
        if current == SCHEMA_VERSION {
            return Ok(());
        }
        let has_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'folders'",
            [],
            |r| r.get(0),
        )?;
        if has_tables > 0 {
            let backup = path.with_extension(format!("db.bak-v{current}"));
            if !backup.exists() {
                let target = backup.to_string_lossy().replace('\'', "''");
                conn.execute_batch(&format!("VACUUM INTO '{target}'"))
                    .context("cannot back up the database before migrating")?;
            }
        }
        if current < 1 {
            Self::migrate_v1(&conn)?;
        }
        if current < 2 {
            let has_archived: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('folders') WHERE name = 'archived'",
                [],
                |r| r.get(0),
            )?;
            let mut sql = String::from("BEGIN;");
            if has_archived == 0 {
                sql.push_str("ALTER TABLE folders ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;");
            }
            sql.push_str(
                "CREATE INDEX IF NOT EXISTS idx_versions_blob ON versions(blob_id);
                 CREATE INDEX IF NOT EXISTS idx_blobs_base ON blobs(base_id);
                 PRAGMA user_version = 2;
                 COMMIT;",
            );
            conn.execute_batch(&sql)?;
        }
        Ok(())
    }

    fn migrate_v1(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                enabled INTEGER NOT NULL DEFAULT 1,
                retention_days INTEGER NOT NULL DEFAULT 90,
                thinning INTEGER NOT NULL DEFAULT 1,
                exclude_globs TEXT NOT NULL DEFAULT '',
                max_file_mb INTEGER NOT NULL DEFAULT 128,
                paused INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
                rel_path TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                size INTEGER NOT NULL DEFAULT 0,
                mtime INTEGER NOT NULL DEFAULT 0,
                content_hash TEXT,
                keyframe_blob_id INTEGER,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                UNIQUE(folder_id, rel_path)
            );

            CREATE TABLE IF NOT EXISTS versions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                ts INTEGER NOT NULL,
                event TEXT NOT NULL,
                size INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                blob_id INTEGER NOT NULL,
                prev_path TEXT,
                label TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                added INTEGER NOT NULL DEFAULT 0,
                removed INTEGER NOT NULL DEFAULT 0,
                snippet TEXT,
                is_text INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                base_id INTEGER NOT NULL DEFAULT 0,
                stored_size INTEGER NOT NULL,
                raw_size INTEGER NOT NULL,
                storage_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(kind, content_hash, base_id)
            );

            CREATE TABLE IF NOT EXISTS text_cache (
                content_hash TEXT PRIMARY KEY,
                text TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_versions_file ON versions(file_id, ts);
            CREATE INDEX IF NOT EXISTS idx_versions_ts ON versions(ts);
            CREATE INDEX IF NOT EXISTS idx_files_folder ON files(folder_id);
            CREATE INDEX IF NOT EXISTS idx_files_hash ON files(content_hash);
            PRAGMA user_version = 1;
            COMMIT;
            "#,
        )?;
        Ok(())
    }

    pub fn list_folder_rows(&self) -> Result<Vec<FolderRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FOLDER_COLUMNS} FROM folders WHERE archived = 0 ORDER BY path COLLATE NOCASE"
        ))?;
        let rows = stmt
            .query_map([], map_folder)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_all_folder_rows(&self) -> Result<Vec<FolderRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FOLDER_COLUMNS} FROM folders ORDER BY path COLLATE NOCASE"
        ))?;
        let rows = stmt
            .query_map([], map_folder)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn folder_row(&self, id: i64) -> Result<Option<FolderRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE id = ?1"))?;
        Ok(stmt.query_row(params![id], map_folder).optional()?)
    }

    pub fn folder_row_by_path(&self, path: &str) -> Result<Option<FolderRow>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE path = ?1 COLLATE NOCASE"))?;
        Ok(stmt.query_row(params![path], map_folder).optional()?)
    }

    pub fn unarchive_folder(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE folders SET archived = 0, enabled = 1, paused = 0 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn insert_folder(
        &self,
        path: &str,
        retention_days: i64,
        thinning: bool,
        exclude_globs: &str,
        max_file_mb: i64,
        now: i64,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO folders (path, enabled, retention_days, thinning, exclude_globs, max_file_mb, paused, created_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![
                path,
                retention_days,
                thinning as i64,
                exclude_globs,
                max_file_mb,
                now
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_folder(
        &self,
        id: i64,
        enabled: bool,
        retention_days: i64,
        thinning: bool,
        exclude_globs: &str,
        max_file_mb: i64,
        paused: bool,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE folders SET enabled = ?2, retention_days = ?3, thinning = ?4, exclude_globs = ?5,
             max_file_mb = ?6, paused = ?7 WHERE id = ?1",
            params![
                id,
                enabled as i64,
                retention_days,
                thinning as i64,
                exclude_globs,
                max_file_mb,
                paused as i64
            ],
        )?;
        Ok(())
    }

    pub fn remove_folder(&self, id: i64, delete_history: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if delete_history {
            tx.execute(
                "DELETE FROM versions WHERE file_id IN (SELECT id FROM files WHERE folder_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM files WHERE folder_id = ?1", params![id])?;
            tx.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        } else {
            tx.execute(
                "UPDATE folders SET archived = 1, enabled = 0 WHERE id = ?1",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<FolderInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "WITH vf AS (
                SELECT f.folder_id AS folder_id, v.blob_id AS blob_id
                FROM versions v JOIN files f ON f.id = v.file_id
             ),
             vc AS (SELECT folder_id, COUNT(*) AS n FROM vf GROUP BY folder_id),
             bs AS (
                SELECT d.folder_id AS folder_id, SUM(b.stored_size) AS stored, SUM(b.raw_size) AS raw
                FROM (SELECT DISTINCT folder_id, blob_id FROM vf) d JOIN blobs b ON b.id = d.blob_id
                GROUP BY d.folder_id
             ),
             fc AS (SELECT folder_id, COUNT(*) AS n FROM files GROUP BY folder_id)
             SELECT f.id, f.path, f.enabled, f.retention_days, f.thinning, f.exclude_globs, f.max_file_mb,
                    f.paused, f.created_at,
                    COALESCE(fc.n, 0), COALESCE(vc.n, 0), COALESCE(bs.stored, 0), COALESCE(bs.raw, 0),
                    f.archived
             FROM folders f
             LEFT JOIN fc ON fc.folder_id = f.id
             LEFT JOIN vc ON vc.folder_id = f.id
             LEFT JOIN bs ON bs.folder_id = f.id
             ORDER BY f.archived, f.path COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(FolderInfo {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    enabled: row.get(2)?,
                    retention_days: row.get(3)?,
                    thinning: row.get(4)?,
                    exclude_globs: row.get(5)?,
                    max_file_mb: row.get(6)?,
                    paused: row.get(7)?,
                    created_at: row.get(8)?,
                    file_count: row.get(9)?,
                    version_count: row.get(10)?,
                    stored_bytes: row.get(11)?,
                    raw_bytes: row.get(12)?,
                    archived: row.get(13)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn file_row(&self, id: i64) -> Result<Option<FileRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {FILE_COLUMNS} FROM files WHERE id = ?1"))?;
        Ok(stmt.query_row(params![id], map_file).optional()?)
    }

    pub fn find_file(&self, folder_id: i64, rel_path: &str) -> Result<Option<FileRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE folder_id = ?1 AND rel_path = ?2"
        ))?;
        Ok(stmt.query_row(params![folder_id, rel_path], map_file).optional()?)
    }

    pub fn list_file_rows(&self, folder_id: i64) -> Result<Vec<FileRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE folder_id = ?1 ORDER BY rel_path COLLATE NOCASE"
        ))?;
        let rows = stmt
            .query_map(params![folder_id], map_file)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn files_under(&self, folder_id: i64, dir_rel: &str) -> Result<Vec<FileRow>> {
        let conn = self.conn();
        let prefix = format!("{}/", dir_rel.trim_end_matches('/'));
        let mut stmt = conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE folder_id = ?1 AND substr(rel_path, 1, length(?2)) = ?2"
        ))?;
        let rows = stmt
            .query_map(params![folder_id, prefix], map_file)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_file(&self, folder_id: i64, rel_path: &str, now: i64) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO files (folder_id, rel_path, status, size, mtime, content_hash, keyframe_blob_id, first_seen, last_seen)
             VALUES (?1, ?2, 'active', 0, 0, NULL, NULL, ?3, ?3)
             ON CONFLICT(folder_id, rel_path) DO UPDATE SET status = 'active', last_seen = ?3",
            params![folder_id, rel_path, now],
        )?;
        let mut stmt = conn.prepare("SELECT id FROM files WHERE folder_id = ?1 AND rel_path = ?2")?;
        let id: i64 = stmt.query_row(params![folder_id, rel_path], |row| row.get(0))?;
        Ok(id)
    }

    pub fn update_file_state(
        &self,
        file_id: i64,
        size: i64,
        mtime: i64,
        content_hash: &str,
        keyframe_blob_id: Option<i64>,
        now: i64,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE files SET size = ?2, mtime = ?3, content_hash = ?4, keyframe_blob_id = ?5,
             status = 'active', last_seen = ?6 WHERE id = ?1",
            params![file_id, size, mtime, content_hash, keyframe_blob_id, now],
        )?;
        Ok(())
    }

    pub fn touch_file_stat(&self, file_id: i64, size: i64, mtime: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE files SET size = ?2, mtime = ?3 WHERE id = ?1",
            params![file_id, size, mtime],
        )?;
        Ok(())
    }

    pub fn mark_file_deleted(&self, file_id: i64, now: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE files SET status = 'deleted', last_seen = ?2 WHERE id = ?1",
            params![file_id, now],
        )?;
        Ok(())
    }

    pub fn rename_file(&self, file_id: i64, new_rel_path: &str, now: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE files SET rel_path = ?2, status = 'active', last_seen = ?3 WHERE id = ?1",
            params![file_id, new_rel_path, now],
        )?;
        Ok(())
    }

    pub fn merge_file_into(&self, source_id: i64, target_id: i64, now: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE versions SET file_id = ?2 WHERE file_id = ?1",
            params![source_id, target_id],
        )?;
        tx.execute(
            "UPDATE files SET (size, mtime, content_hash, keyframe_blob_id) =
                (SELECT size, mtime, content_hash, keyframe_blob_id FROM files WHERE id = ?1),
                status = 'active', last_seen = ?3
             WHERE id = ?2",
            params![source_id, target_id, now],
        )?;
        tx.execute("DELETE FROM files WHERE id = ?1", params![source_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn find_rename_candidate(
        &self,
        folder_id: i64,
        content_hash: &str,
        since: i64,
    ) -> Result<Option<FileRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files
             WHERE folder_id = ?1 AND content_hash = ?2 AND last_seen >= ?3 AND status != 'active'
             ORDER BY last_seen DESC LIMIT 1"
        ))?;
        Ok(stmt
            .query_row(params![folder_id, content_hash, since], map_file)
            .optional()?)
    }

    pub fn list_files(
        &self,
        folder_id: i64,
        include_deleted: bool,
        query: &str,
    ) -> Result<Vec<FileInfo>> {
        let conn = self.conn();
        let pattern = format!("%{}%", query.trim().to_lowercase().replace('\\', "/"));
        let mut stmt = conn.prepare(
            "WITH stats AS (
                SELECT v.file_id AS file_id, COUNT(*) AS n, MAX(v.ts) AS latest_ts
                FROM versions v JOIN files f ON f.id = v.file_id
                WHERE f.folder_id = ?1
                GROUP BY v.file_id
             )
             SELECT f.id, f.folder_id, f.rel_path, f.status, f.size, f.mtime, f.content_hash, f.first_seen, f.last_seen,
                    COALESCE(s.n, 0),
                    (SELECT v.id FROM versions v WHERE v.file_id = f.id ORDER BY v.ts DESC, v.id DESC LIMIT 1),
                    s.latest_ts
             FROM files f LEFT JOIN stats s ON s.file_id = f.id
             WHERE f.folder_id = ?1
               AND (?2 = 1 OR f.status = 'active')
               AND (?3 = '%%' OR LOWER(f.rel_path) LIKE ?3)
             ORDER BY f.status = 'deleted', f.last_seen DESC, f.rel_path COLLATE NOCASE
             LIMIT 5000",
        )?;
        let rows = stmt
            .query_map(params![folder_id, include_deleted as i64, pattern], |row| {
                Ok(FileInfo {
                    id: row.get(0)?,
                    folder_id: row.get(1)?,
                    rel_path: row.get(2)?,
                    status: row.get(3)?,
                    size: row.get(4)?,
                    mtime: row.get(5)?,
                    content_hash: row.get(6)?,
                    first_seen: row.get(7)?,
                    last_seen: row.get(8)?,
                    version_count: row.get(9)?,
                    latest_version_id: row.get(10)?,
                    latest_ts: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn version_briefs_for_folder(&self, folder_id: i64) -> Result<Vec<VersionBrief>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.file_id, v.ts, v.pinned FROM versions v JOIN files f ON f.id = v.file_id
             WHERE f.folder_id = ?1 ORDER BY v.file_id, v.ts DESC, v.id DESC",
        )?;
        let rows = stmt
            .query_map(params![folder_id], |row| {
                Ok(VersionBrief {
                    id: row.get(0)?,
                    file_id: row.get(1)?,
                    ts: row.get(2)?,
                    pinned: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn path_at(&self, file_id: i64, ts: i64) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT prev_path FROM versions WHERE file_id = ?1 AND ts > ?2 AND prev_path IS NOT NULL
                 ORDER BY ts ASC, id ASC LIMIT 1",
                params![file_id, ts],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn previous_version_id(&self, file_id: i64, ts: i64, id: i64) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT id FROM versions WHERE file_id = ?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3))
                 ORDER BY ts DESC, id DESC LIMIT 1",
                params![file_id, ts, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_version(
        &self,
        file_id: i64,
        ts: i64,
        event: &str,
        size: i64,
        content_hash: &str,
        blob_id: i64,
        prev_path: Option<&str>,
        added: i64,
        removed: i64,
        snippet: Option<&str>,
        is_text: bool,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO versions (file_id, ts, event, size, content_hash, blob_id, prev_path, label, pinned,
                                   added, removed, snippet, is_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 0, ?8, ?9, ?10, ?11)",
            params![
                file_id,
                ts,
                event,
                size,
                content_hash,
                blob_id,
                prev_path,
                added,
                removed,
                snippet,
                is_text as i64
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn map_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<VersionRow> {
        Ok(VersionRow {
            id: row.get(0)?,
            file_id: row.get(1)?,
            ts: row.get(2)?,
            event: row.get(3)?,
            size: row.get(4)?,
            content_hash: row.get(5)?,
            blob_id: row.get(6)?,
            prev_path: row.get(7)?,
            label: row.get(8)?,
            pinned: row.get(9)?,
            added: row.get(10)?,
            removed: row.get(11)?,
            snippet: row.get(12)?,
            is_text: row.get(13)?,
            rel_path: row.get(14)?,
        })
    }

    const VERSION_SELECT: &'static str = "SELECT v.id, v.file_id, v.ts, v.event, v.size, v.content_hash, v.blob_id,
            v.prev_path, v.label, v.pinned, v.added, v.removed, v.snippet, v.is_text, f.rel_path
        FROM versions v JOIN files f ON f.id = v.file_id";

    pub fn list_versions(&self, file_id: i64) -> Result<Vec<VersionInfo>> {
        let conn = self.conn();
        let sql = format!("{} WHERE v.file_id = ?1 ORDER BY v.ts DESC, v.id DESC", Self::VERSION_SELECT);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![file_id], |row| {
                let v = Self::map_version(row)?;
                Ok(VersionInfo {
                    id: v.id,
                    file_id: v.file_id,
                    ts: v.ts,
                    event: v.event,
                    size: v.size,
                    content_hash: v.content_hash,
                    blob_id: v.blob_id,
                    prev_path: v.prev_path,
                    label: v.label,
                    pinned: v.pinned,
                    added: v.added,
                    removed: v.removed,
                    snippet: v.snippet,
                    is_text: v.is_text,
                    rel_path: v.rel_path,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn version_row(&self, id: i64) -> Result<Option<VersionRow>> {
        let conn = self.conn();
        let sql = format!("{} WHERE v.id = ?1", Self::VERSION_SELECT);
        let mut stmt = conn.prepare(&sql)?;
        let row = stmt.query_row(params![id], Self::map_version).optional()?;
        Ok(row)
    }

    pub fn version_info(&self, id: i64) -> Result<Option<VersionInfo>> {
        Ok(self.version_row(id)?.map(|v| VersionInfo {
            id: v.id,
            file_id: v.file_id,
            ts: v.ts,
            event: v.event,
            size: v.size,
            content_hash: v.content_hash,
            blob_id: v.blob_id,
            prev_path: v.prev_path,
            label: v.label,
            pinned: v.pinned,
            added: v.added,
            removed: v.removed,
            snippet: v.snippet,
            is_text: v.is_text,
            rel_path: v.rel_path,
        }))
    }

    pub fn latest_version_row(&self, file_id: i64) -> Result<Option<VersionRow>> {
        let conn = self.conn();
        let sql = format!(
            "{} WHERE v.file_id = ?1 ORDER BY v.ts DESC, v.id DESC LIMIT 1",
            Self::VERSION_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let row = stmt.query_row(params![file_id], Self::map_version).optional()?;
        Ok(row)
    }

    pub fn version_at(&self, file_id: i64, ts: i64) -> Result<Option<VersionRow>> {
        let conn = self.conn();
        let sql = format!(
            "{} WHERE v.file_id = ?1 AND v.ts <= ?2 ORDER BY v.ts DESC, v.id DESC LIMIT 1",
            Self::VERSION_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let row = stmt.query_row(params![file_id, ts], Self::map_version).optional()?;
        Ok(row)
    }

    pub fn set_version_pin(&self, id: i64, pinned: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE versions SET pinned = ?2 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    pub fn set_version_label(&self, id: i64, label: Option<&str>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE versions SET label = ?2 WHERE id = ?1",
            params![id, label],
        )?;
        Ok(())
    }

    pub fn commit_version(&self, new: &NewVersion<'_>) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let blob_id = match new.blob {
            BlobRef::Existing(id) => id,
            BlobRef::New {
                kind,
                base_id,
                stored_size,
                storage_hash,
            } => {
                tx.execute(
                    "INSERT OR IGNORE INTO blobs (kind, content_hash, base_id, stored_size, raw_size, storage_hash, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![kind, new.content_hash, base_id, stored_size, new.size, storage_hash, new.ts],
                )?;
                tx.query_row(
                    "SELECT id FROM blobs WHERE kind = ?1 AND content_hash = ?2 AND base_id = ?3",
                    params![kind, new.content_hash, base_id],
                    |r| r.get(0),
                )?
            }
        };
        tx.execute(
            "INSERT INTO versions (file_id, ts, event, size, content_hash, blob_id, prev_path, label, pinned,
                                   added, removed, snippet, is_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 0, ?8, ?9, ?10, ?11)",
            params![
                new.file_id,
                new.ts,
                new.event,
                new.size,
                new.content_hash,
                blob_id,
                new.prev_path,
                new.added,
                new.removed,
                new.snippet,
                new.is_text as i64
            ],
        )?;
        let version_id = tx.last_insert_rowid();
        let keyframe = match (new.blob, new.keyframe) {
            (BlobRef::New { kind: "full", .. }, _) => Some(blob_id),
            (_, KeyframeUpdate::UseBlob) => Some(blob_id),
            (_, KeyframeUpdate::Keep(k)) => k,
        };
        tx.execute(
            "UPDATE files SET size = ?2, mtime = ?3, content_hash = ?4, keyframe_blob_id = ?5,
             status = 'active', last_seen = ?6 WHERE id = ?1",
            params![new.file_id, new.size, new.mtime, new.content_hash, keyframe, new.ts],
        )?;
        tx.commit()?;
        Ok(version_id)
    }

    pub fn update_blob_storage(&self, id: i64, storage_hash: &str, stored_size: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE blobs SET storage_hash = ?2, stored_size = ?3 WHERE id = ?1",
            params![id, storage_hash, stored_size],
        )?;
        Ok(())
    }

    pub fn delete_versions(&self, ids: &[i64]) -> Result<i64> {
        let conn = self.conn();
        let mut removed = 0i64;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM versions WHERE id = ?1")?;
            for id in ids {
                removed += stmt.execute(params![id])? as i64;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn blob_row(&self, id: i64) -> Result<Option<BlobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content_hash, base_id, stored_size, raw_size, storage_hash FROM blobs WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(BlobRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content_hash: row.get(2)?,
                    base_id: row.get(3)?,
                    stored_size: row.get(4)?,
                    raw_size: row.get(5)?,
                    storage_hash: row.get(6)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn find_full_blob(&self, content_hash: &str) -> Result<Option<BlobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content_hash, base_id, stored_size, raw_size, storage_hash FROM blobs
             WHERE kind = 'full' AND content_hash = ?1 LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![content_hash], |row| {
                Ok(BlobRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content_hash: row.get(2)?,
                    base_id: row.get(3)?,
                    stored_size: row.get(4)?,
                    raw_size: row.get(5)?,
                    storage_hash: row.get(6)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn all_blobs(&self) -> Result<Vec<BlobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content_hash, base_id, stored_size, raw_size, storage_hash FROM blobs",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(BlobRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content_hash: row.get(2)?,
                    base_id: row.get(3)?,
                    stored_size: row.get(4)?,
                    raw_size: row.get(5)?,
                    storage_hash: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_blob(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM blobs WHERE id = ?1", params![id])?;
        Ok(())
    }



    pub fn set_retention_for_all(&self, retention_days: i64, thinning: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE folders SET retention_days = ?1, thinning = ?2 WHERE archived = 0",
            params![retention_days, thinning as i64],
        )?;
        Ok(())
    }

    pub fn set_text_if_absent(&self, content_hash: &str, text: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO text_cache (content_hash, text) VALUES (?1, ?2)",
            params![content_hash, text],
        )?;
        Ok(())
    }

    pub fn clear_stale_keyframes(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE files SET keyframe_blob_id = NULL WHERE keyframe_blob_id IS NOT NULL
             AND keyframe_blob_id NOT IN (SELECT id FROM blobs)",
            [],
        )?;
        Ok(())
    }


    pub fn get_text(&self, content_hash: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT text FROM text_cache WHERE content_hash = ?1")?;
        let text = stmt
            .query_row(params![content_hash], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(text)
    }

    pub fn prune_text_cache(&self) -> Result<i64> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM text_cache WHERE content_hash NOT IN
                (SELECT content_hash FROM files WHERE content_hash IS NOT NULL)",
            [],
        )?;
        Ok(removed as i64)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
        let value = stmt
            .query_row(params![key], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(value)
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn storage_stats(&self) -> Result<(i64, i64, i64, i64, i64, i64)> {
        let conn = self.conn();
        let folders: i64 = conn.query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0))?;
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let versions: i64 = conn.query_row("SELECT COUNT(*) FROM versions", [], |r| r.get(0))?;
        let blobs: i64 = conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        let raw: i64 =
            conn.query_row("SELECT COALESCE(SUM(raw_size),0) FROM blobs", [], |r| r.get(0))?;
        let stored: i64 =
            conn.query_row("SELECT COALESCE(SUM(stored_size),0) FROM blobs", [], |r| r.get(0))?;
        Ok((folders, files, versions, blobs, raw, stored))
    }

    pub fn check_integrity(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut problems = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.content_hash, b.id, b.content_hash, b.kind, base.id, base.kind
             FROM versions v
             LEFT JOIN blobs b ON b.id = v.blob_id
             LEFT JOIN blobs base ON base.id = b.base_id AND b.kind = 'delta'
             ORDER BY v.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (version_id, hash, blob_id, blob_hash, kind, base_id, base_kind) = row?;
            match (blob_id, blob_hash) {
                (None, _) => problems.push(format!("version {version_id} has a missing blob")),
                (Some(_), Some(h)) if h != hash => problems.push(format!(
                    "version {version_id} points at a blob with different content"
                )),
                _ => {}
            }
            if kind.as_deref() == Some("delta") && (base_id.is_none() || base_kind.as_deref() != Some("full")) {
                problems.push(format!("version {version_id} is a delta without a valid base"));
            }
        }
        Ok(problems)
    }

    pub fn blobs_created_before(&self, ts: i64) -> Result<Vec<BlobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content_hash, base_id, stored_size, raw_size, storage_hash FROM blobs WHERE created_at < ?1",
        )?;
        let rows = stmt
            .query_map(params![ts], |row| {
                Ok(BlobRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content_hash: row.get(2)?,
                    base_id: row.get(3)?,
                    stored_size: row.get(4)?,
                    raw_size: row.get(5)?,
                    storage_hash: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn version_blob_ids(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, blob_id FROM versions")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn keyframe_blob_ids(&self) -> Result<std::collections::HashSet<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT keyframe_blob_id FROM files WHERE keyframe_blob_id IS NOT NULL")?;
        let mut set = std::collections::HashSet::new();
        for id in stmt.query_map([], |row| row.get::<_, i64>(0))? {
            set.insert(id?);
        }
        Ok(set)
    }

    pub fn db_file_size(&self) -> Result<i64> {
        let conn = self.conn();
        let path: String = conn.query_row("PRAGMA database_list", [], |r| r.get(2))?;
        let mut size = 0;
        if !path.is_empty()
            && let Ok(meta) = std::fs::metadata(&path) {
                size = meta.len() as i64;
            }
        Ok(size)
    }
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn normalize_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    let trimmed = text.trim_end_matches(['\\', '/']).to_string();
    if trimmed.len() > 3 {
        trimmed
    } else {
        text
    }
}

pub fn rel_path_of(folder: &str, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(Path::new(folder))
        .map_err(|_| {
            anyhow!(
                "path {} is not inside {}",
                path.to_string_lossy(),
                folder
            )
        })?
        .to_string_lossy()
        .trim_start_matches(['\\', '/'])
        .replace('\\', "/");
    Ok(rel)
}
