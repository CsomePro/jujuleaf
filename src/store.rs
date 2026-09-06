use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Prepared,
    Inflight,
    Unknown,
    Confirmed,
    Failed,
}

impl OperationStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Inflight => "inflight",
            Self::Unknown => "unknown",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "inflight" => Ok(Self::Inflight),
            "unknown" => Ok(Self::Unknown),
            "confirmed" => Ok(Self::Confirmed),
            "failed" => Ok(Self::Failed),
            _ => Err(anyhow!("invalid operation status in database: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSyncState {
    pub project_id: String,
    pub doc_id: String,
    pub path: String,
    pub remote_version: i64,
    pub remote_hash: String,
    pub jj_operation_id: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub receipt_id: String,
    pub project_id: String,
    pub doc_id: String,
    pub base_version: i64,
    pub operation_json: String,
    pub expected_hash: String,
    pub source_ids_json: String,
    pub status: OperationStatus,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub struct SyncStore {
    connection: Connection,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn content_hash(content: &str) -> String {
    hex::encode(Sha1::digest(content.as_bytes()))
}

impl SyncStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open sync database {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let store = Self {
            connection: Connection::open_in_memory()?,
        };
        store.connection.pragma_update(None, "foreign_keys", true)?;
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS documents (
                project_id TEXT NOT NULL,
                doc_id TEXT NOT NULL,
                path TEXT NOT NULL,
                remote_version INTEGER NOT NULL CHECK (remote_version >= 0),
                remote_hash TEXT NOT NULL,
                jj_operation_id TEXT,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (project_id, doc_id)
            );

            CREATE TABLE IF NOT EXISTS sync_operations (
                receipt_id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                doc_id TEXT NOT NULL,
                base_version INTEGER NOT NULL CHECK (base_version >= 0),
                operation_json TEXT NOT NULL,
                expected_hash TEXT NOT NULL,
                source_ids_json TEXT NOT NULL DEFAULT '[]',
                status TEXT NOT NULL CHECK (
                    status IN ('prepared', 'inflight', 'unknown', 'confirmed', 'failed')
                ),
                error TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS sync_operations_pending
            ON sync_operations(project_id, doc_id, status, created_at_ms);

            INSERT INTO schema_meta(key, value)
            VALUES ('schema_version', '1')
            ON CONFLICT(key) DO UPDATE SET value = excluded.value;
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_document(
        &self,
        project_id: &str,
        doc_id: &str,
        path: &str,
        remote_version: i64,
        remote_hash: &str,
        jj_operation_id: Option<&str>,
    ) -> Result<()> {
        ensure!(remote_version >= 0, "remote version cannot be negative");
        self.connection.execute(
            r#"
            INSERT INTO documents(
                project_id, doc_id, path, remote_version, remote_hash,
                jj_operation_id, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(project_id, doc_id) DO UPDATE SET
                path = excluded.path,
                remote_version = excluded.remote_version,
                remote_hash = excluded.remote_hash,
                jj_operation_id = excluded.jj_operation_id,
                updated_at_ms = excluded.updated_at_ms
            "#,
            params![
                project_id,
                doc_id,
                path,
                remote_version,
                remote_hash,
                jj_operation_id,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn document(&self, project_id: &str, doc_id: &str) -> Result<Option<DocumentSyncState>> {
        self.connection
            .query_row(
                r#"
                SELECT project_id, doc_id, path, remote_version, remote_hash,
                       jj_operation_id, updated_at_ms
                FROM documents
                WHERE project_id = ?1 AND doc_id = ?2
                "#,
                params![project_id, doc_id],
                |row| {
                    Ok(DocumentSyncState {
                        project_id: row.get(0)?,
                        doc_id: row.get(1)?,
                        path: row.get(2)?,
                        remote_version: row.get(3)?,
                        remote_hash: row.get(4)?,
                        jj_operation_id: row.get(5)?,
                        updated_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn documents(&self, project_id: &str) -> Result<Vec<DocumentSyncState>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT project_id, doc_id, path, remote_version, remote_hash,
                   jj_operation_id, updated_at_ms
            FROM documents
            WHERE project_id = ?1
            ORDER BY path
            "#,
        )?;
        statement
            .query_map([project_id], |row| {
                Ok(DocumentSyncState {
                    project_id: row.get(0)?,
                    doc_id: row.get(1)?,
                    path: row.get(2)?,
                    remote_version: row.get(3)?,
                    remote_hash: row.get(4)?,
                    jj_operation_id: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn prepare_operation(
        &self,
        project_id: &str,
        doc_id: &str,
        base_version: i64,
        operation_json: &str,
        expected_hash: &str,
    ) -> Result<String> {
        ensure!(base_version >= 0, "base version cannot be negative");
        serde_json::from_str::<serde_json::Value>(operation_json)
            .context("operation_json must be valid JSON")?;
        let receipt_id = Uuid::new_v4().to_string();
        let timestamp = now_ms();
        self.connection.execute(
            r#"
            INSERT INTO sync_operations(
                receipt_id, project_id, doc_id, base_version, operation_json,
                expected_hash, source_ids_json, status, created_at_ms, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', 'prepared', ?7, ?7)
            "#,
            params![
                receipt_id,
                project_id,
                doc_id,
                base_version,
                operation_json,
                expected_hash,
                timestamp
            ],
        )?;
        Ok(receipt_id)
    }

    pub fn mark_inflight(&self, receipt_id: &str) -> Result<()> {
        self.transition(
            receipt_id,
            &[OperationStatus::Prepared, OperationStatus::Unknown],
            OperationStatus::Inflight,
            None,
        )
    }

    pub fn record_source_id(&self, receipt_id: &str, source_id: &str) -> Result<()> {
        let existing: String = self.connection.query_row(
            "SELECT source_ids_json FROM sync_operations WHERE receipt_id = ?1",
            [receipt_id],
            |row| row.get(0),
        )?;
        let mut source_ids: Vec<String> = serde_json::from_str(&existing)?;
        if !source_ids.iter().any(|id| id == source_id) {
            source_ids.push(source_id.to_owned());
        }
        self.connection.execute(
            "UPDATE sync_operations SET source_ids_json = ?2, updated_at_ms = ?3 WHERE receipt_id = ?1",
            params![receipt_id, serde_json::to_string(&source_ids)?, now_ms()],
        )?;
        Ok(())
    }

    pub fn mark_unknown(&self, receipt_id: &str, reason: &str) -> Result<()> {
        self.transition(
            receipt_id,
            &[OperationStatus::Prepared, OperationStatus::Inflight],
            OperationStatus::Unknown,
            Some(reason),
        )
    }

    pub fn mark_confirmed(&self, receipt_id: &str) -> Result<()> {
        self.transition(
            receipt_id,
            &[
                OperationStatus::Prepared,
                OperationStatus::Inflight,
                OperationStatus::Unknown,
            ],
            OperationStatus::Confirmed,
            None,
        )
    }

    pub fn mark_failed(&self, receipt_id: &str, reason: &str) -> Result<()> {
        self.transition(
            receipt_id,
            &[
                OperationStatus::Prepared,
                OperationStatus::Inflight,
                OperationStatus::Unknown,
            ],
            OperationStatus::Failed,
            Some(reason),
        )
    }

    fn transition(
        &self,
        receipt_id: &str,
        allowed: &[OperationStatus],
        next: OperationStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let current = self
            .operation(receipt_id)?
            .ok_or_else(|| anyhow!("operation receipt not found: {receipt_id}"))?;
        ensure!(
            allowed.contains(&current.status),
            "invalid sync transition {} -> {} for {receipt_id}",
            current.status.as_str(),
            next.as_str()
        );
        self.connection.execute(
            r#"
            UPDATE sync_operations
            SET status = ?2, error = ?3, updated_at_ms = ?4
            WHERE receipt_id = ?1
            "#,
            params![receipt_id, next.as_str(), error, now_ms()],
        )?;
        Ok(())
    }

    pub fn operation(&self, receipt_id: &str) -> Result<Option<OperationRecord>> {
        self.connection
            .query_row(
                r#"
                SELECT receipt_id, project_id, doc_id, base_version, operation_json,
                       expected_hash, source_ids_json, status, error,
                       created_at_ms, updated_at_ms
                FROM sync_operations WHERE receipt_id = ?1
                "#,
                [receipt_id],
                row_to_operation,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn unresolved_operations(
        &self,
        project_id: &str,
        doc_id: Option<&str>,
    ) -> Result<Vec<OperationRecord>> {
        let mut records = Vec::new();
        if let Some(doc_id) = doc_id {
            let mut statement = self.connection.prepare(
                r#"
                SELECT receipt_id, project_id, doc_id, base_version, operation_json,
                       expected_hash, source_ids_json, status, error,
                       created_at_ms, updated_at_ms
                FROM sync_operations
                WHERE project_id = ?1 AND doc_id = ?2
                  AND status IN ('prepared', 'inflight', 'unknown')
                ORDER BY created_at_ms
                "#,
            )?;
            records.extend(
                statement
                    .query_map(params![project_id, doc_id], row_to_operation)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        } else {
            let mut statement = self.connection.prepare(
                r#"
                SELECT receipt_id, project_id, doc_id, base_version, operation_json,
                       expected_hash, source_ids_json, status, error,
                       created_at_ms, updated_at_ms
                FROM sync_operations
                WHERE project_id = ?1
                  AND status IN ('prepared', 'inflight', 'unknown')
                ORDER BY created_at_ms
                "#,
            )?;
            records.extend(
                statement
                    .query_map([project_id], row_to_operation)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(records)
    }

    /// Mark uncertain receipts as confirmed when a fresh remote snapshot has
    /// exactly the content hash the operation expected.
    pub fn reconcile_confirmed_hash(
        &self,
        project_id: &str,
        doc_id: &str,
        remote_hash: &str,
    ) -> Result<usize> {
        let changed = self.connection.execute(
            r#"
            UPDATE sync_operations
            SET status = 'confirmed', error = NULL, updated_at_ms = ?4
            WHERE project_id = ?1 AND doc_id = ?2 AND expected_hash = ?3
              AND status IN ('prepared', 'inflight', 'unknown')
            "#,
            params![project_id, doc_id, remote_hash, now_ms()],
        )?;
        Ok(changed)
    }
}

fn row_to_operation(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRecord> {
    let status: String = row.get(7)?;
    let status = OperationStatus::parse(&status).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, error.into())
    })?;
    Ok(OperationRecord {
        receipt_id: row.get(0)?,
        project_id: row.get(1)?,
        doc_id: row.get(2)?,
        base_version: row.get(3)?,
        operation_json: row.get(4)?,
        expected_hash: row.get(5)?,
        source_ids_json: row.get(6)?,
        status,
        error: row.get(8)?,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_state_is_a_checkpoint_not_a_history_copy() {
        let store = SyncStore::open_memory().unwrap();
        store
            .upsert_document(
                "p1",
                "d1",
                "main.tex",
                12,
                &content_hash("hello"),
                Some("op1"),
            )
            .unwrap();
        let state = store.document("p1", "d1").unwrap().unwrap();
        assert_eq!(state.remote_version, 12);
        assert_eq!(state.remote_hash, content_hash("hello"));
        assert_eq!(state.jj_operation_id.as_deref(), Some("op1"));

        store
            .upsert_document(
                "p1",
                "d1",
                "main.tex",
                13,
                &content_hash("world"),
                Some("op2"),
            )
            .unwrap();
        assert_eq!(
            store.document("p1", "d1").unwrap().unwrap().remote_version,
            13
        );
    }

    #[test]
    fn operation_receipts_survive_uncertain_network_results() {
        let store = SyncStore::open_memory().unwrap();
        let receipt = store
            .prepare_operation("p1", "d1", 7, r#"[{"i":"x","p":0}]"#, "expected")
            .unwrap();
        store.mark_inflight(&receipt).unwrap();
        store.record_source_id(&receipt, "client-op-1").unwrap();
        store
            .mark_unknown(&receipt, "connection closed before confirmation")
            .unwrap();

        let pending = store.unresolved_operations("p1", Some("d1")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, OperationStatus::Unknown);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&pending[0].source_ids_json).unwrap(),
            vec!["client-op-1"]
        );

        store.mark_inflight(&receipt).unwrap();
        store.mark_confirmed(&receipt).unwrap();
        assert!(
            store
                .unresolved_operations("p1", Some("d1"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn terminal_receipts_cannot_be_reopened() {
        let store = SyncStore::open_memory().unwrap();
        let receipt = store
            .prepare_operation("p1", "d1", 0, "[]", "hash")
            .unwrap();
        store.mark_confirmed(&receipt).unwrap();
        assert!(store.mark_inflight(&receipt).is_err());
    }
}
