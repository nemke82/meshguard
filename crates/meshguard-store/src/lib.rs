//! MeshGuard message store (ADR-005): conversation history, inbox and outbox
//! in one SQLite file owned by Rust.
//!
//! Message bodies and display names are encrypted per field with a key
//! derived from the device vault key (ADR-002); ids, node numbers, times and
//! delivery states stay readable so the store can sort and schedule.
//!
//! The crate has no Tauri, tokio or Android dependency, so the Tauri
//! commands and the native Android service (ADR-001) share it. All times
//! are Unix seconds passed in by the caller. Nothing in here logs.

mod cipher;
mod schema;

use std::path::Path;

use meshguard_core::crypto::SessionKey;
use meshguard_core::delivery::{DeliveryState, OutboundMessage};
use meshguard_core::ids::{ConversationId, MessageId, TransportId};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use cipher::StoreCipher;
pub use schema::SCHEMA_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("stored data could not be decrypted")]
    Decrypt,
    /// The store was written with another vault key (for example the
    /// keyring entry was lost). Its content cannot be read with this key.
    #[error("the store was created with a different key")]
    WrongKey,
    #[error("encryption failed")]
    Encrypt,
    #[error(
        "database was written by a newer MeshGuard (schema {found}, this build knows {known})"
    )]
    TooNew { found: i64, known: i64 },
    #[error("invalid stored value: {0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Which way a message went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    fn from_i64(v: i64) -> Result<Self> {
        match v {
            0 => Ok(Self::In),
            1 => Ok(Self::Out),
            _ => Err(StoreError::Invalid("direction")),
        }
    }
}

/// Wire message type of a stored message (`meshguard_core` type codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Text,
    /// Check-in / SOS (body is the encoded status).
    Status,
}

impl MessageKind {
    fn as_i64(self) -> i64 {
        match self {
            Self::Text => 1,
            Self::Status => 4,
        }
    }

    fn from_i64(v: i64) -> Result<Self> {
        match v {
            1 => Ok(Self::Text),
            4 => Ok(Self::Status),
            _ => Err(StoreError::Invalid("kind")),
        }
    }
}

/// State of an incoming message.
pub const IN_RECEIVED: &str = "received";
pub const IN_READ: &str = "read";

/// One row of the chat list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: ConversationId,
    pub my_node: u32,
    pub peer_node: u32,
    pub title: Option<String>,
    pub last_message_at: u64,
    pub unread: u32,
}

/// Position in a conversation's history, for loading older messages.
/// Unique per message, so messages stored in the same second are not
/// skipped between pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub stored_at: u64,
    pub seq: i64,
}

/// One stored message, decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Pass to [`Store::messages`] to load the messages before this one.
    pub cursor: Cursor,
    pub conversation: ConversationId,
    pub message_id: MessageId,
    pub direction: Direction,
    pub kind: MessageKind,
    pub body: Vec<u8>,
    pub created_at: u64,
    pub stored_at: u64,
    /// Outgoing: `DeliveryState::as_str()`; incoming: [`IN_RECEIVED`] / [`IN_READ`].
    pub state: String,
}

/// An outgoing message that is due for work, with its body for re-sealing.
#[derive(Debug, Clone, PartialEq)]
pub struct DueOutbound {
    pub conversation: ConversationId,
    pub kind: MessageKind,
    /// Time of the first attempt; a retry carries it unchanged (ADR-003).
    pub created_at: u64,
    pub body: Vec<u8>,
    pub outbound: OutboundMessage,
}

/// Stable conversation id for a chat between our radio and a peer.
pub fn conversation_id(my_node: u32, peer_node: u32) -> ConversationId {
    ConversationId(format!("c:{my_node:08x}:{peer_node:08x}"))
}

/// `(my_node, peer_node)` of an id made by [`conversation_id`].
pub fn parse_conversation_id(id: &ConversationId) -> Option<(u32, u32)> {
    let rest = id.0.strip_prefix("c:")?;
    let (my, peer) = rest.split_once(':')?;
    if my.len() != 8 || peer.len() != 8 {
        return None;
    }
    Some((
        u32::from_str_radix(my, 16).ok()?,
        u32::from_str_radix(peer, 16).ok()?,
    ))
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

pub struct Store {
    conn: Connection,
    cipher: StoreCipher,
}

impl Store {
    /// Open (or create) the store at `path`, migrating it to the current
    /// schema. A database from a newer build is left untouched.
    pub fn open(path: &Path, vault: &SessionKey) -> Result<Self> {
        Self::init(Connection::open(path)?, vault)
    }

    /// A throw-away store, for tests.
    pub fn open_in_memory(vault: &SessionKey) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, vault)
    }

    fn init(mut conn: Connection, vault: &SessionKey) -> Result<Self> {
        // Before anything that writes to the file (journal mode is stored
        // in the header): a newer database must stay untouched.
        schema::check_not_newer(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        // In-memory databases report "memory"; both are fine.
        let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        schema::migrate(&mut conn)?;
        let store = Self {
            conn,
            cipher: StoreCipher::new(vault),
        };
        store.check_key()?;
        Ok(store)
    }

    /// Record a key check on first use; refuse a store sealed with
    /// another key, instead of failing later on every read.
    fn check_key(&self) -> Result<()> {
        let aad = cipher::key_check_aad();
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE name = 'key_check'", [], |r| {
                r.get(0)
            })
            .optional()?;
        match stored {
            Some(blob) => match self.cipher.open(&blob, &aad) {
                Ok(v) if v == cipher::KEY_CHECK => Ok(()),
                _ => Err(StoreError::WrongKey),
            },
            None => {
                // A store from schema 1 may hold rows sealed with another
                // key; only claim it for this key when it is still empty.
                let rows: i64 = self
                    .conn
                    .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?;
                if rows > 0 && !self.first_body_opens()? {
                    return Err(StoreError::WrongKey);
                }
                let blob = self.cipher.seal(cipher::KEY_CHECK, &aad)?;
                self.conn.execute(
                    "INSERT INTO meta (name, value) VALUES ('key_check', ?1)",
                    params![blob],
                )?;
                Ok(())
            }
        }
    }

    fn first_body_opens(&self) -> Result<bool> {
        let (conv, id, dir, enc): (String, Vec<u8>, i64, Vec<u8>) = self.conn.query_row(
            "SELECT conversation_id, message_id, direction, body_enc FROM messages LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let conv = ConversationId(conv);
        let id = MessageId::from_slice(&id).ok_or(StoreError::Invalid("message id"))?;
        let dir = Direction::from_i64(dir)?;
        Ok(self
            .cipher
            .open(&enc, &cipher::body_aad(&conv, &id, dir))
            .is_ok())
    }

    // ── Conversations ─────────────────────────────────────────

    /// Create the conversation if needed; update its title when given.
    pub fn upsert_conversation(
        &self,
        my_node: u32,
        peer_node: u32,
        title: Option<&str>,
    ) -> Result<ConversationId> {
        let id = conversation_id(my_node, peer_node);
        self.conn.execute(
            "INSERT OR IGNORE INTO conversations (conversation_id, my_node, peer_node)
             VALUES (?1, ?2, ?3)",
            params![id.0, my_node, peer_node],
        )?;
        if let Some(title) = title {
            let enc = self
                .cipher
                .seal(title.as_bytes(), &cipher::title_aad(&id))?;
            self.conn.execute(
                "UPDATE conversations SET title_enc = ?2 WHERE conversation_id = ?1",
                params![id.0, enc],
            )?;
        }
        Ok(id)
    }

    /// The chat list, most recent activity first.
    pub fn conversations(&self) -> Result<Vec<ConversationSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT conversation_id, my_node, peer_node, title_enc, last_message_at, unread_count
             FROM conversations ORDER BY last_message_at DESC, conversation_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, u32>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, my_node, peer_node, title_enc, last, unread) = row?;
            let id = ConversationId(id);
            let title = match title_enc {
                Some(enc) => Some(
                    String::from_utf8(self.cipher.open(&enc, &cipher::title_aad(&id))?)
                        .map_err(|_| StoreError::Invalid("title"))?,
                ),
                None => None,
            };
            out.push(ConversationSummary {
                id,
                my_node,
                peer_node,
                title,
                last_message_at: to_u64(last),
                unread,
            });
        }
        Ok(out)
    }

    /// Remove a conversation with all its messages and queued sends.
    ///
    /// Best effort against recovery: `secure_delete` zeroes the freed
    /// pages, and a truncating checkpoint moves them into the main file and
    /// empties the WAL, which otherwise keeps older copies of the rows.
    /// Flash storage may still hold old blocks; the encryption is what
    /// protects those (ADR-005 §6).
    pub fn delete_conversation(&mut self, id: &ConversationId) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM conversations WHERE conversation_id = ?1",
            params![id.0],
        )?;
        if n > 0 {
            // Busy readers can block a full checkpoint; deletion has still
            // happened, the WAL is cleared on a later checkpoint.
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(n > 0)
    }

    /// Mark every incoming message of the conversation as read.
    pub fn mark_read(&mut self, id: &ConversationId) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE messages SET state = ?2
             WHERE conversation_id = ?1 AND direction = 0 AND state = ?3",
            params![id.0, IN_READ, IN_RECEIVED],
        )?;
        tx.execute(
            "UPDATE conversations SET unread_count = 0 WHERE conversation_id = ?1",
            params![id.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── Messages ──────────────────────────────────────────────

    /// Store an incoming message. Returns false for a duplicate (same id
    /// already stored), which is then left unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_incoming(
        &mut self,
        conversation: &ConversationId,
        message_id: MessageId,
        kind: MessageKind,
        body: &[u8],
        created_at: u64,
        now: u64,
    ) -> Result<bool> {
        let enc = self.cipher.seal(
            body,
            &cipher::body_aad(conversation, &message_id, Direction::In),
        )?;
        let tx = self.conn.transaction()?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO messages
               (conversation_id, message_id, direction, kind, body_enc, created_at, stored_at, state)
             VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7)",
            params![
                conversation.0,
                message_id.as_bytes().as_slice(),
                kind.as_i64(),
                enc,
                to_i64(created_at),
                to_i64(now),
                IN_RECEIVED
            ],
        )? > 0;
        if added {
            tx.execute(
                "UPDATE conversations
                 SET unread_count = unread_count + 1, last_message_at = MAX(last_message_at, ?2)
                 WHERE conversation_id = ?1",
                params![conversation.0, to_i64(now)],
            )?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// Store an outgoing message and its outbox entry in one transaction.
    /// Call this before handing the frame to any radio.
    pub fn insert_outgoing(
        &mut self,
        conversation: &ConversationId,
        kind: MessageKind,
        body: &[u8],
        created_at: u64,
        outbound: &OutboundMessage,
    ) -> Result<()> {
        let id = outbound.message_id;
        let enc = self
            .cipher
            .seal(body, &cipher::body_aad(conversation, &id, Direction::Out))?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO messages
               (conversation_id, message_id, direction, kind, body_enc, created_at, stored_at, state)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?5, ?6)",
            params![
                conversation.0,
                id.as_bytes().as_slice(),
                kind.as_i64(),
                enc,
                to_i64(created_at),
                outbound.state.as_str()
            ],
        )?;
        write_outbox(&tx, conversation, outbound)?;
        tx.execute(
            "UPDATE conversations SET last_message_at = MAX(last_message_at, ?2)
             WHERE conversation_id = ?1",
            params![conversation.0, to_i64(created_at)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist new delivery bookkeeping for an outgoing message. A settled
    /// message leaves the outbox; its final state stays on the message.
    pub fn update_outbound(
        &mut self,
        conversation: &ConversationId,
        outbound: &OutboundMessage,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE messages SET state = ?3
             WHERE conversation_id = ?1 AND message_id = ?2 AND direction = 1",
            params![
                conversation.0,
                outbound.message_id.as_bytes().as_slice(),
                outbound.state.as_str()
            ],
        )?;
        if n == 0 {
            // Deleted meanwhile (conversation removed): nothing to track.
            return Ok(());
        }
        write_outbox(&tx, conversation, outbound)?;
        tx.commit()?;
        Ok(())
    }

    /// Take a message out of the outbox without waiting for an ACK, keeping
    /// `outbound.state` on the message. For peers that cannot confirm (v1):
    /// once the radio accepted the frame, resending could only duplicate it.
    pub fn settle_unconfirmed(
        &mut self,
        conversation: &ConversationId,
        outbound: &OutboundMessage,
    ) -> Result<()> {
        let id = outbound.message_id.as_bytes().as_slice();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE messages SET state = ?3
             WHERE conversation_id = ?1 AND message_id = ?2 AND direction = 1",
            params![conversation.0, id, outbound.state.as_str()],
        )?;
        tx.execute(
            "DELETE FROM outbox WHERE conversation_id = ?1 AND message_id = ?2",
            params![conversation.0, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Bookkeeping for an outgoing message whether or not it is still in
    /// the outbox (a late ACK may confirm a message already marked failed
    /// or expired). Without an outbox row the deadlines and attempts are 0.
    pub fn outgoing(
        &self,
        conversation: &ConversationId,
        message_id: &MessageId,
    ) -> Result<Option<OutboundMessage>> {
        if let Some(out) = self.outbound(conversation, message_id)? {
            return Ok(Some(out));
        }
        self.conn
            .query_row(
                "SELECT state FROM messages
                 WHERE conversation_id = ?1 AND message_id = ?2 AND direction = 1",
                params![conversation.0, message_id.as_bytes().as_slice()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|state| {
                Ok(OutboundMessage {
                    message_id: *message_id,
                    state: DeliveryState::parse(&state).ok_or(StoreError::Invalid("state"))?,
                    attempts: 0,
                    expires_at: 0,
                    next_deadline: 0,
                    last_transport: None,
                })
            })
            .transpose()
    }

    /// Outbox bookkeeping for one outgoing message, if it is still queued.
    pub fn outbound(
        &self,
        conversation: &ConversationId,
        message_id: &MessageId,
    ) -> Result<Option<OutboundMessage>> {
        self.conn
            .query_row(
                "SELECT o.attempts, o.next_deadline, o.expires_at, o.last_transport, m.state
                 FROM outbox o JOIN messages m USING (conversation_id, message_id, direction)
                 WHERE o.conversation_id = ?1 AND o.message_id = ?2",
                params![conversation.0, message_id.as_bytes().as_slice()],
                |r| {
                    Ok((
                        r.get::<_, u32>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(attempts, next, expires, transport, state)| {
                Ok(OutboundMessage {
                    message_id: *message_id,
                    state: DeliveryState::parse(&state).ok_or(StoreError::Invalid("state"))?,
                    attempts,
                    expires_at: to_u64(expires),
                    next_deadline: to_u64(next),
                    last_transport: transport.map(TransportId),
                })
            })
            .transpose()
    }

    /// Queued messages whose deadline has passed at `now` (send, ACK
    /// timeout or expiry), oldest deadline first.
    pub fn due_outbound(&self, now: u64) -> Result<Vec<DueOutbound>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.conversation_id, o.message_id, o.attempts, o.next_deadline, o.expires_at,
                    o.last_transport, m.state, m.kind, m.body_enc, m.created_at
             FROM outbox o JOIN messages m USING (conversation_id, message_id, direction)
             WHERE o.next_deadline <= ?1 OR o.expires_at <= ?1
             ORDER BY o.next_deadline",
        )?;
        let rows = stmt.query_map(params![to_i64(now)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Vec<u8>>(8)?,
                r.get::<_, i64>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (conv, id, attempts, next, expires, transport, state, kind, enc, created) = row?;
            let conversation = ConversationId(conv);
            let message_id = MessageId::from_slice(&id).ok_or(StoreError::Invalid("message id"))?;
            let body = self.cipher.open(
                &enc,
                &cipher::body_aad(&conversation, &message_id, Direction::Out),
            )?;
            out.push(DueOutbound {
                conversation,
                kind: MessageKind::from_i64(kind)?,
                created_at: to_u64(created),
                body,
                outbound: OutboundMessage {
                    message_id,
                    state: DeliveryState::parse(&state).ok_or(StoreError::Invalid("state"))?,
                    attempts,
                    expires_at: to_u64(expires),
                    next_deadline: to_u64(next),
                    last_transport: transport.map(TransportId),
                },
            });
        }
        Ok(out)
    }

    /// Up to `limit` messages of a conversation before `before` (the newest
    /// when `None`), returned oldest first for display.
    pub fn messages(
        &self,
        conversation: &ConversationId,
        before: Option<Cursor>,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, direction, kind, body_enc, created_at, stored_at, state, rowid
             FROM messages
             WHERE conversation_id = ?1
               AND (stored_at < ?2 OR (stored_at = ?2 AND rowid < ?3))
             ORDER BY stored_at DESC, rowid DESC
             LIMIT ?4",
        )?;
        let (before_at, before_seq) = match before {
            Some(c) => (to_i64(c.stored_at), c.seq),
            None => (i64::MAX, i64::MAX),
        };
        let rows = stmt.query_map(params![conversation.0, before_at, before_seq, limit], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, dir, kind, enc, created, stored, state, seq) = row?;
            let message_id = MessageId::from_slice(&id).ok_or(StoreError::Invalid("message id"))?;
            let direction = Direction::from_i64(dir)?;
            let body = self.cipher.open(
                &enc,
                &cipher::body_aad(conversation, &message_id, direction),
            )?;
            out.push(StoredMessage {
                cursor: Cursor {
                    stored_at: to_u64(stored),
                    seq,
                },
                conversation: conversation.clone(),
                message_id,
                direction,
                kind: MessageKind::from_i64(kind)?,
                body,
                created_at: to_u64(created),
                stored_at: to_u64(stored),
                state,
            });
        }
        out.reverse();
        Ok(out)
    }
}

/// Insert, update or (when settled) remove the outbox row.
fn write_outbox(
    tx: &Transaction<'_>,
    conversation: &ConversationId,
    outbound: &OutboundMessage,
) -> Result<()> {
    let id = outbound.message_id.as_bytes().as_slice();
    if outbound.state.is_settled() {
        tx.execute(
            "DELETE FROM outbox WHERE conversation_id = ?1 AND message_id = ?2",
            params![conversation.0, id],
        )?;
    } else {
        tx.execute(
            "INSERT INTO outbox
               (conversation_id, message_id, attempts, next_deadline, expires_at, last_transport)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (conversation_id, message_id) DO UPDATE SET
               attempts = excluded.attempts,
               next_deadline = excluded.next_deadline,
               expires_at = excluded.expires_at,
               last_transport = excluded.last_transport",
            params![
                conversation.0,
                id,
                outbound.attempts,
                to_i64(outbound.next_deadline),
                to_i64(outbound.expires_at),
                outbound.last_transport.as_ref().map(|t| t.0.as_str())
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
