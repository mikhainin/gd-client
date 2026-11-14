//! Local SQLite-backed state store: tracks the mapping between local files
//! and Drive file IDs, plus the Drive Changes API page token, per sync folder.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive_client::DriveFile;
use crate::error::SyncError;

// `Connection` is `Send` but not `Sync` (it caches prepared statements in a
// `RefCell`), so a plain `Connection` field would make `Arc<StateDb>` not
// `Send`, which `tokio::spawn` requires for the sync engine's per-folder
// tasks. Wrapping it in a `Mutex` makes `StateDb` `Sync` (all access is
// already effectively serialised through `&self` methods that need a lock).
pub struct StateDb {
    conn: Mutex<Connection>,
}

impl StateDb {
    pub fn open(path: &Path) -> Result<Self, SyncError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), SyncError> {
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn.transaction()?;
        transaction.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sync_folder_state (
                sync_folder_id     TEXT PRIMARY KEY,
                changes_page_token TEXT
            );

            CREATE TABLE IF NOT EXISTS file_state (
                sync_folder_id      TEXT NOT NULL,
                local_rel_path       TEXT NOT NULL,
                drive_file_id     TEXT,
                is_folder         INTEGER NOT NULL DEFAULT 0,
                local_mtime_secs  INTEGER,
                drive_modified_time TEXT,
                md5_checksum      TEXT,
                PRIMARY KEY (sync_folder_id, local_rel_path)
            );

            CREATE INDEX IF NOT EXISTS idx_file_state_drive_id
                ON file_state (sync_folder_id, drive_file_id);

            CREATE TABLE IF NOT EXISTS pending_local_changes (
                sync_folder_id TEXT NOT NULL,
                local_rel_path TEXT NOT NULL,
                upload_json TEXT,
                revision INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (sync_folder_id, local_rel_path)
            );
            "#,
        )?;
        let has_revision: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('pending_local_changes') WHERE name = 'revision')",
            [],
            |row| row.get(0),
        )?;
        if !has_revision {
            transaction.execute(
                "ALTER TABLE pending_local_changes ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn get_page_token(&self, sync_folder_id: Uuid) -> Result<Option<String>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT changes_page_token FROM sync_folder_state WHERE sync_folder_id = ?1",
        )?;
        let result = stmt
            .query_row(params![sync_folder_id.to_string()], |row| row.get(0))
            .ok();
        Ok(result)
    }

    pub fn set_page_token(&self, sync_folder_id: Uuid, token: &str) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO sync_folder_state (sync_folder_id, changes_page_token) VALUES (?1, ?2)
             ON CONFLICT(sync_folder_id) DO UPDATE SET changes_page_token = excluded.changes_page_token",
            params![sync_folder_id.to_string(), token],
        )?;
        Ok(())
    }

    pub fn upsert_file_state(&self, entry: &FileStateEntry) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO file_state
                (sync_folder_id, local_rel_path, drive_file_id, is_folder, local_mtime_secs, drive_modified_time, md5_checksum)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sync_folder_id, local_rel_path) DO UPDATE SET
                drive_file_id = excluded.drive_file_id,
                is_folder = excluded.is_folder,
                local_mtime_secs = excluded.local_mtime_secs,
                drive_modified_time = excluded.drive_modified_time,
                md5_checksum = excluded.md5_checksum",
            params![
                entry.sync_folder_id.to_string(),
                entry.local_rel_path,
                entry.drive_file_id,
                entry.is_folder as i64,
                entry.local_mtime_secs,
                entry.drive_modified_time,
                entry.md5_checksum,
            ],
        )?;
        Ok(())
    }

    pub fn remove_file_state(
        &self,
        sync_folder_id: Uuid,
        local_rel_path: &str,
    ) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM file_state WHERE sync_folder_id = ?1 AND
             (local_rel_path = ?2 OR substr(local_rel_path, 1, length(?2) + 1) = ?2 || '/')",
            params![sync_folder_id.to_string(), local_rel_path],
        )?;
        Ok(())
    }

    pub fn relocate_file_state(
        &self,
        folder_id: Uuid,
        old: &str,
        new: &str,
    ) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "UPDATE file_state SET local_rel_path = ?3 || substr(local_rel_path, length(?2) + 1)
             WHERE sync_folder_id = ?1 AND
             (local_rel_path = ?2 OR substr(local_rel_path, 1, length(?2) + 1) = ?2 || '/')",
            params![folder_id.to_string(), old, new],
        )?;
        Ok(())
    }

    pub fn find_by_drive_id(
        &self,
        sync_folder_id: Uuid,
        drive_file_id: &str,
    ) -> Result<Option<FileStateEntry>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sync_folder_id, local_rel_path, drive_file_id, is_folder, local_mtime_secs, drive_modified_time, md5_checksum
             FROM file_state WHERE sync_folder_id = ?1 AND drive_file_id = ?2",
        )?;
        let mut rows = stmt.query(params![sync_folder_id.to_string(), drive_file_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_entry(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn find_by_local_path(
        &self,
        sync_folder_id: Uuid,
        local_rel_path: &str,
    ) -> Result<Option<FileStateEntry>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sync_folder_id, local_rel_path, drive_file_id, is_folder, local_mtime_secs, drive_modified_time, md5_checksum
             FROM file_state WHERE sync_folder_id = ?1 AND local_rel_path = ?2",
        )?;
        let mut rows = stmt.query(params![sync_folder_id.to_string(), local_rel_path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_entry(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn file_states(&self, folder_id: Uuid) -> Result<Vec<FileStateEntry>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sync_folder_id, local_rel_path, drive_file_id, is_folder, local_mtime_secs, drive_modified_time, md5_checksum
             FROM file_state WHERE sync_folder_id = ?1 ORDER BY local_rel_path",
        )?;
        let mut rows = stmt.query([folder_id.to_string()])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(row_to_entry(row)?);
        }
        Ok(entries)
    }

    pub fn queue_local_change(&self, folder_id: Uuid, rel_path: &str) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO pending_local_changes (sync_folder_id, local_rel_path) VALUES (?1, ?2)
             ON CONFLICT(sync_folder_id, local_rel_path) DO UPDATE SET revision = revision + 1",
            params![folder_id.to_string(), rel_path],
        )?;
        Ok(())
    }

    pub fn pending_local_paths(&self, folder_id: Uuid) -> Result<Vec<String>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_rel_path FROM pending_local_changes WHERE sync_folder_id = ?1 ORDER BY local_rel_path",
        )?;
        let paths = stmt
            .query_map([folder_id.to_string()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(paths)
    }

    pub fn pending_upload(
        &self,
        folder_id: Uuid,
        rel_path: &str,
    ) -> Result<Option<PendingUpload>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT upload_json FROM pending_local_changes WHERE sync_folder_id = ?1 AND local_rel_path = ?2",
        )?;
        let mut rows = stmt.query(params![folder_id.to_string(), rel_path])?;
        let json: Option<String> = match rows.next()? {
            Some(row) => row.get(0)?,
            None => None,
        };
        json.map(|json| serde_json::from_str(&json).map_err(SyncError::from))
            .transpose()
    }

    pub fn set_pending_upload(
        &self,
        folder_id: Uuid,
        rel_path: &str,
        upload: Option<&PendingUpload>,
    ) -> Result<(), SyncError> {
        let json = upload.map(serde_json::to_string).transpose()?;
        self.conn.lock().unwrap().execute(
            "INSERT INTO pending_local_changes (sync_folder_id, local_rel_path, upload_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(sync_folder_id, local_rel_path) DO UPDATE SET upload_json = excluded.upload_json",
            params![folder_id.to_string(), rel_path, json],
        )?;
        Ok(())
    }

    pub fn complete_local_change(&self, folder_id: Uuid, rel_path: &str) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM pending_local_changes WHERE sync_folder_id = ?1 AND local_rel_path = ?2",
            params![folder_id.to_string(), rel_path],
        )?;
        Ok(())
    }

    pub fn pending_local_revision(
        &self,
        folder_id: Uuid,
        rel_path: &str,
    ) -> Result<Option<i64>, SyncError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT revision FROM pending_local_changes WHERE sync_folder_id = ?1 AND local_rel_path = ?2",
        )?;
        let mut rows = stmt.query(params![folder_id.to_string(), rel_path])?;
        rows.next()?
            .map(|row| row.get(0).map_err(SyncError::from))
            .transpose()
    }

    pub fn complete_local_revision(
        &self,
        folder_id: Uuid,
        rel_path: &str,
        revision: i64,
    ) -> Result<(), SyncError> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM pending_local_changes WHERE sync_folder_id = ?1 AND local_rel_path = ?2 AND revision = ?3",
            params![folder_id.to_string(), rel_path, revision],
        )?;
        Ok(())
    }

    pub fn has_pending_local_change(
        &self,
        folder_id: Uuid,
        rel_path: &str,
    ) -> Result<bool, SyncError> {
        Ok(self.pending_local_paths(folder_id)?.iter().any(|pending| {
            pending == rel_path
                || Path::new(pending).starts_with(rel_path)
                || Path::new(rel_path).starts_with(pending)
        }))
    }

    pub fn has_pending_local_write(
        &self,
        folder_id: Uuid,
        rel_path: &str,
    ) -> Result<bool, SyncError> {
        Ok(self
            .pending_local_paths(folder_id)?
            .iter()
            .any(|pending| Path::new(rel_path).starts_with(pending)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpload {
    pub file_id: String,
    pub previous_drive_id: Option<String>,
    pub is_folder: bool,
    pub uploaded: Option<DriveFile>,
}

#[derive(Debug, Clone)]
pub struct FileStateEntry {
    pub sync_folder_id: Uuid,
    pub local_rel_path: String,
    pub drive_file_id: Option<String>,
    pub is_folder: bool,
    pub local_mtime_secs: Option<i64>,
    pub drive_modified_time: Option<String>,
    pub md5_checksum: Option<String>,
}

fn row_to_entry(row: &rusqlite::Row) -> Result<FileStateEntry, SyncError> {
    let sync_folder_id: String = row.get(0)?;
    Ok(FileStateEntry {
        sync_folder_id: Uuid::parse_str(&sync_folder_id)
            .map_err(|e| SyncError::Api(format!("corrupt sync_folder_id in state db: {e}")))?,
        local_rel_path: row.get(1)?,
        drive_file_id: row.get(2)?,
        is_folder: row.get::<_, i64>(3)? != 0,
        local_mtime_secs: row.get(4)?,
        drive_modified_time: row.get(5)?,
        md5_checksum: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completing_an_older_attempt_does_not_drop_a_newer_watcher_event() {
        let root = std::env::temp_dir().join(format!("gdrive-queue-revision-{}", Uuid::new_v4()));
        let db = StateDb::open(&root.join("state.sqlite")).unwrap();
        let id = Uuid::new_v4();
        db.queue_local_change(id, "file.txt").unwrap();
        let revision = db.pending_local_revision(id, "file.txt").unwrap().unwrap();
        db.set_pending_upload(id, "file.txt", None).unwrap();
        assert_eq!(
            db.pending_local_revision(id, "file.txt").unwrap(),
            Some(revision)
        );
        db.queue_local_change(id, "file.txt").unwrap();
        db.complete_local_revision(id, "file.txt", revision)
            .unwrap();
        assert_eq!(db.pending_local_paths(id).unwrap(), vec!["file.txt"]);
        let latest = db.pending_local_revision(id, "file.txt").unwrap().unwrap();
        db.complete_local_revision(id, "file.txt", latest).unwrap();
        assert!(db.pending_local_paths(id).unwrap().is_empty());
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_pending_queue_is_migrated_without_losing_upload_stages() {
        let root = std::env::temp_dir().join(format!("gdrive-queue-migration-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE pending_local_changes (
                sync_folder_id TEXT NOT NULL, local_rel_path TEXT NOT NULL,
                upload_json TEXT, PRIMARY KEY (sync_folder_id, local_rel_path)
             );",
        )
        .unwrap();
        let id = Uuid::new_v4();
        let upload = PendingUpload {
            file_id: "recoverable-id".into(),
            previous_drive_id: Some("old-id".into()),
            is_folder: false,
            uploaded: None,
        };
        conn.execute(
            "INSERT INTO pending_local_changes VALUES (?1, ?2, ?3)",
            params![
                id.to_string(),
                r"report\2026.txt",
                serde_json::to_string(&upload).unwrap()
            ],
        )
        .unwrap();
        drop(conn);
        let db = StateDb::open(&path).unwrap();
        assert_eq!(
            db.pending_local_revision(id, r"report\2026.txt").unwrap(),
            Some(1)
        );
        assert_eq!(
            db.pending_upload(id, r"report\2026.txt")
                .unwrap()
                .unwrap()
                .file_id,
            "recoverable-id"
        );
        db.queue_local_change(id, r"report\2026.txt").unwrap();
        assert_eq!(
            db.pending_local_revision(id, r"report\2026.txt").unwrap(),
            Some(2)
        );
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stores_and_retrieves_page_token() {
        let dir = std::env::temp_dir().join(format!("gdrive-sync-test-{}", Uuid::new_v4()));
        let db = StateDb::open(&dir.join("state.sqlite3")).unwrap();
        let folder_id = Uuid::new_v4();
        assert_eq!(db.get_page_token(folder_id).unwrap(), None);
        db.set_page_token(folder_id, "token-1").unwrap();
        assert_eq!(
            db.get_page_token(folder_id).unwrap(),
            Some("token-1".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upserts_and_finds_file_state() {
        let dir = std::env::temp_dir().join(format!("gdrive-sync-test-{}", Uuid::new_v4()));
        let db = StateDb::open(&dir.join("state.sqlite3")).unwrap();
        let folder_id = Uuid::new_v4();
        let entry = FileStateEntry {
            sync_folder_id: folder_id,
            local_rel_path: "notes.txt".into(),
            drive_file_id: Some("abc123".into()),
            is_folder: false,
            local_mtime_secs: Some(1_700_000_000),
            drive_modified_time: Some("2024-01-01T00:00:00Z".into()),
            md5_checksum: Some("deadbeef".into()),
        };
        db.upsert_file_state(&entry).unwrap();
        let found = db.find_by_drive_id(folder_id, "abc123").unwrap().unwrap();
        assert_eq!(found.local_rel_path, "notes.txt");
        let _ = std::fs::remove_dir_all(dir);
    }
}
