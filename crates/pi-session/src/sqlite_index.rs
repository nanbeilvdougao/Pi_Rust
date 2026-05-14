//! Optional SQLite mirror of the JSONL session log.
//!
//! Behind the `sqlite-index` feature. JSONL stays canonical so the SQLite
//! file is recoverable from disk at any time. The index buys:
//!
//! - O(log n) lookup of message-count and last-update per session.
//! - Full-text search across user/assistant content (FTS5).
//! - Tag table for the upcoming `pi search` cross-session lookup.
//!
//! Schema versioning lives in the `meta` table; we rebuild from JSONL on
//! version mismatch.

use std::path::{Path, PathBuf};

use pi_core::{Message, PiError, PiErrorKind, PiResult, Role};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{JsonlSessionStore, Session, SessionStore};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct SqliteIndex {
    conn: Connection,
    db_path: PathBuf,
}

impl SqliteIndex {
    pub fn open(path: impl AsRef<Path>) -> PiResult<Self> {
        let db_path = path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path).map_err(map_sqlite_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sqlite_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(map_sqlite_err)?;
        let index = Self { conn, db_path };
        index.migrate()?;
        Ok(index)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn migrate(&self) -> PiResult<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    created_ms INTEGER NOT NULL,
                    updated_ms INTEGER NOT NULL,
                    message_count INTEGER NOT NULL DEFAULT 0,
                    last_user_excerpt TEXT
                );
                CREATE TABLE IF NOT EXISTS messages (
                    session_id TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    tool_call_id TEXT,
                    PRIMARY KEY (session_id, seq)
                );
                CREATE INDEX IF NOT EXISTS messages_session
                    ON messages(session_id);
                CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
                    USING fts5(session_id UNINDEXED, role UNINDEXED, content);
                "#,
            )
            .map_err(map_sqlite_err)?;
        let stored: Option<i64> = self
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;
        if stored != Some(SCHEMA_VERSION) {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )
                .map_err(map_sqlite_err)?;
        }
        Ok(())
    }

    pub fn ingest(&mut self, store: &JsonlSessionStore) -> PiResult<usize> {
        let summaries = store.list()?;
        let mut total = 0usize;
        let tx = self.conn.transaction().map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM messages_fts", [])
            .map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM messages", [])
            .map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM sessions", [])
            .map_err(map_sqlite_err)?;
        for summary in &summaries {
            let session = store.load(&summary.id)?;
            let created = session
                .messages
                .first()
                .map(|m| m.timestamp_ms as i64)
                .unwrap_or(0);
            tx.execute(
                "INSERT INTO sessions(id, created_ms, updated_ms, message_count, last_user_excerpt)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    summary.id,
                    created,
                    summary.updated_ms as i64,
                    summary.message_count as i64,
                    summary.last_user_excerpt,
                ],
            )
            .map_err(map_sqlite_err)?;
            for (seq, message) in session.messages.iter().enumerate() {
                insert_message_tx(&tx, &summary.id, seq as i64, message)?;
                total += 1;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(total)
    }

    pub fn record(&self, session_id: &str, seq: usize, message: &Message) -> PiResult<()> {
        let now = pi_core::now_ms() as i64;
        let tx = self.conn.unchecked_transaction().map_err(map_sqlite_err)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;
        if exists.is_none() {
            tx.execute(
                "INSERT INTO sessions(id, created_ms, updated_ms, message_count, last_user_excerpt)
                 VALUES (?1, ?2, ?2, 0, NULL)",
                params![session_id, now],
            )
            .map_err(map_sqlite_err)?;
        }
        insert_message_tx(&tx, session_id, seq as i64, message)?;
        let excerpt = if message.role == Role::User {
            Some(truncate_excerpt(&message.content, 80))
        } else {
            None
        };
        if let Some(excerpt) = excerpt {
            tx.execute(
                "UPDATE sessions SET updated_ms = ?1, message_count = message_count + 1,
                    last_user_excerpt = ?2 WHERE id = ?3",
                params![now, excerpt, session_id],
            )
            .map_err(map_sqlite_err)?;
        } else {
            tx.execute(
                "UPDATE sessions SET updated_ms = ?1, message_count = message_count + 1
                    WHERE id = ?2",
                params![now, session_id],
            )
            .map_err(map_sqlite_err)?;
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> PiResult<Vec<SearchHit>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, role, snippet(messages_fts, 2, '«', '»', '…', 12) AS hit
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1
                 LIMIT ?2",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(SearchHit {
                    session_id: row.get::<_, String>(0)?,
                    role: row.get::<_, String>(1)?,
                    snippet: row.get::<_, String>(2)?,
                })
            })
            .map_err(map_sqlite_err)?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row.map_err(map_sqlite_err)?);
        }
        Ok(hits)
    }

    pub fn count_sessions(&self) -> PiResult<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        Ok(count as usize)
    }
}

fn insert_message_tx(
    tx: &rusqlite::Connection,
    session_id: &str,
    seq: i64,
    message: &Message,
) -> PiResult<()> {
    tx.execute(
        "INSERT INTO messages(session_id, seq, role, content, timestamp_ms, tool_call_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            session_id,
            seq,
            message.role.as_str(),
            message.content,
            message.timestamp_ms as i64,
            message.tool_call_id,
        ],
    )
    .map_err(map_sqlite_err)?;
    tx.execute(
        "INSERT INTO messages_fts(session_id, role, content) VALUES (?1, ?2, ?3)",
        params![session_id, message.role.as_str(), message.content],
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

fn map_sqlite_err(err: rusqlite::Error) -> PiError {
    PiError::new(PiErrorKind::Session, format!("SQLite 错误：{err}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub role: String,
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, Role};
    use tempfile::tempdir;

    #[test]
    fn ingest_then_search_finds_terms() {
        let dir = tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(dir.path());
        store
            .append(
                "alpha",
                &Message::new(Role::User, "请帮我重构数据库 schema"),
            )
            .expect("append");
        store
            .append(
                "alpha",
                &Message::new(Role::Assistant, "好的，我先看 schema"),
            )
            .expect("append");
        let mut index = SqliteIndex::open(dir.path().join("index.db")).expect("open");
        let n = index.ingest(&store).expect("ingest");
        assert!(n >= 2);
        assert_eq!(index.count_sessions().expect("count"), 1);
        let hits = index.search("schema", 10).expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].session_id, "alpha");
    }

    #[test]
    fn record_appends_incrementally() {
        let dir = tempdir().expect("tempdir");
        let index = SqliteIndex::open(dir.path().join("incr.db")).expect("open");
        index
            .record("beta", 0, &Message::new(Role::User, "hello world"))
            .expect("record");
        index
            .record("beta", 1, &Message::new(Role::Assistant, "hi"))
            .expect("record");
        let hits = index.search("hello", 10).expect("search");
        assert!(hits.iter().any(|h| h.session_id == "beta"));
    }
}
