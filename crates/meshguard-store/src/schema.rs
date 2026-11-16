//! Schema and forward-only migrations (ADR-005 §4–5).

use rusqlite::Connection;

use crate::{Result, StoreError};

/// Migration `n` (1-based) brings the schema from version `n - 1` to `n`.
const MIGRATIONS: &[&str] = &[
    // 1: conversations, messages, outbox.
    "CREATE TABLE conversations (
       conversation_id TEXT PRIMARY KEY,
       my_node         INTEGER NOT NULL,
       peer_node       INTEGER NOT NULL,
       title_enc       BLOB,
       last_message_at INTEGER NOT NULL DEFAULT 0,
       unread_count    INTEGER NOT NULL DEFAULT 0,
       UNIQUE (my_node, peer_node)
     );
     CREATE TABLE messages (
       conversation_id TEXT NOT NULL REFERENCES conversations ON DELETE CASCADE,
       message_id      BLOB NOT NULL CHECK (length(message_id) = 16),
       direction       INTEGER NOT NULL CHECK (direction IN (0, 1)),
       kind            INTEGER NOT NULL,
       body_enc        BLOB NOT NULL,
       created_at      INTEGER NOT NULL,
       stored_at       INTEGER NOT NULL,
       state           TEXT NOT NULL,
       PRIMARY KEY (conversation_id, message_id, direction)
     );
     CREATE INDEX messages_by_time ON messages (conversation_id, stored_at);
     CREATE TABLE outbox (
       conversation_id TEXT NOT NULL,
       message_id      BLOB NOT NULL,
       direction       INTEGER NOT NULL DEFAULT 1 CHECK (direction = 1),
       attempts        INTEGER NOT NULL DEFAULT 0,
       next_deadline   INTEGER NOT NULL,
       expires_at      INTEGER NOT NULL,
       last_transport  TEXT,
       PRIMARY KEY (conversation_id, message_id),
       FOREIGN KEY (conversation_id, message_id, direction)
         REFERENCES messages (conversation_id, message_id, direction) ON DELETE CASCADE
     );
     CREATE INDEX outbox_by_deadline ON outbox (next_deadline);",
    // 2: key check, so a different vault key is noticed on open.
    "CREATE TABLE meta (
       name  TEXT PRIMARY KEY,
       value BLOB NOT NULL
     );",
    // 3: the peer's send generation when the message was queued; rows of an
    // older generation (contact blocked or removed since) are never sent.
    "ALTER TABLE outbox ADD COLUMN send_generation INTEGER NOT NULL DEFAULT 0;",
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    migrate_to(conn, SCHEMA_VERSION)
}

/// Refuse a database written by a newer build. Reads only.
pub(crate) fn check_not_newer(conn: &Connection) -> Result<i64> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::TooNew {
            found: current,
            known: SCHEMA_VERSION,
        });
    }
    Ok(current)
}

/// Apply migrations up to `target`; separate for tests of older versions.
pub(crate) fn migrate_to(conn: &mut Connection, target: i64) -> Result<()> {
    let current = check_not_newer(conn)?;
    for version in (current + 1)..=target {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATIONS[(version - 1) as usize])?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}
