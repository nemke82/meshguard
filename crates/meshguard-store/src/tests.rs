use super::*;
use meshguard_core::delivery::{DueAction, RetryPolicy};

const ME: u32 = 0xa1b2_c3d4;
const BOB: u32 = 0x0000_0b0b;
const ANN: u32 = 0x0000_0a11;

fn vault() -> SessionKey {
    SessionKey::from_bytes([7u8; 32])
}

fn store() -> Store {
    Store::open_in_memory(&vault()).unwrap()
}

fn id(n: u8) -> MessageId {
    MessageId::from_bytes([n; 16])
}

fn tmp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "meshguard-store-test-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("store.db")
}

#[test]
fn incoming_roundtrip_and_duplicates() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, Some("Bob")).unwrap();
    assert!(s
        .insert_incoming(&c, id(1), MessageKind::Text, b"hi", 100, 105)
        .unwrap());
    // A relayed copy of the same message is ignored and not counted twice.
    assert!(!s
        .insert_incoming(&c, id(1), MessageKind::Text, b"hi", 100, 120)
        .unwrap());

    let msgs = s.messages(&c, None, 50).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, b"hi");
    assert_eq!(msgs[0].direction, Direction::In);
    assert_eq!(msgs[0].state, IN_RECEIVED);

    let list = s.conversations().unwrap();
    assert_eq!(list[0].title.as_deref(), Some("Bob"));
    assert_eq!(list[0].unread, 1);
    assert_eq!(list[0].last_message_at, 105);

    s.mark_read(&c).unwrap();
    assert_eq!(s.conversations().unwrap()[0].unread, 0);
    assert_eq!(s.messages(&c, None, 50).unwrap()[0].state, IN_READ);
}

#[test]
fn chat_list_is_sorted_by_last_activity() {
    let mut s = store();
    let bob = s.upsert_conversation(ME, BOB, Some("Bob")).unwrap();
    let ann = s.upsert_conversation(ME, ANN, Some("Ann")).unwrap();
    s.insert_incoming(&bob, id(1), MessageKind::Text, b"a", 10, 10)
        .unwrap();
    s.insert_incoming(&ann, id(2), MessageKind::Text, b"b", 20, 20)
        .unwrap();
    let order: Vec<_> = s
        .conversations()
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(order, vec![ann.clone(), bob.clone()]);

    let out = OutboundMessage::queued(id(3), 30, 3600);
    s.insert_outgoing(&bob, MessageKind::Text, b"c", 30, &out)
        .unwrap();
    assert_eq!(s.conversations().unwrap()[0].id, bob);
}

#[test]
fn outbox_follows_the_delivery_state_machine() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    let policy = RetryPolicy::default();
    let mut out = OutboundMessage::queued(id(9), 1000, 1000 + 3600);
    s.insert_outgoing(&c, MessageKind::Text, b"are you ok?", 1000, &out)
        .unwrap();

    // Queued: due now, with the body available for re-sealing.
    let due = s.due_outbound(1000).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].body, b"are you ok?");
    assert_eq!(due[0].outbound.due_action(1000), Some(DueAction::Send));

    // Submitted: not due until the ACK deadline.
    out.record_submitted(1001, TransportId::new("ble"), &policy)
        .unwrap();
    s.update_outbound(&c, &out).unwrap();
    assert!(s.due_outbound(1002).unwrap().is_empty());
    let reloaded = s.outbound(&c, &id(9)).unwrap().unwrap();
    assert_eq!(reloaded, out);
    let deadline = out.next_deadline;
    assert_eq!(s.due_outbound(deadline).unwrap().len(), 1);

    // Peer ACK settles it: out of the outbox, state kept on the message.
    out.record_peer_ack().unwrap();
    s.update_outbound(&c, &out).unwrap();
    assert!(s.outbound(&c, &id(9)).unwrap().is_none());
    assert!(s.due_outbound(u64::MAX / 2).unwrap().is_empty());
    assert_eq!(s.messages(&c, None, 10).unwrap()[0].state, "peer_ack");
}

#[test]
fn expiry_makes_a_message_due() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    let mut out = OutboundMessage::queued(id(4), 0, 100);
    out.next_deadline = 500; // e.g. waiting on a long backoff
    s.insert_outgoing(&c, MessageKind::Text, b"x", 0, &out)
        .unwrap();
    assert!(s.due_outbound(99).unwrap().is_empty());
    let due = s.due_outbound(100).unwrap();
    assert_eq!(due[0].outbound.due_action(100), Some(DueAction::Expire));
}

#[test]
fn deleting_a_conversation_removes_messages_and_outbox() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    s.insert_incoming(&c, id(1), MessageKind::Text, b"a", 1, 1)
        .unwrap();
    let out = OutboundMessage::queued(id(2), 1, 3600);
    s.insert_outgoing(&c, MessageKind::Status, b"ok", 1, &out)
        .unwrap();
    assert!(s.delete_conversation(&c).unwrap());
    assert!(!s.delete_conversation(&c).unwrap());
    assert!(s.conversations().unwrap().is_empty());
    assert!(s.messages(&c, None, 10).unwrap().is_empty());
    assert!(s.due_outbound(u64::MAX / 2).unwrap().is_empty());
    // Updating a message whose conversation is gone is a no-op.
    s.update_outbound(&c, &out).unwrap();
}

#[test]
fn paging_returns_older_messages_oldest_first() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    for n in 1..=5u8 {
        s.insert_incoming(&c, id(n), MessageKind::Text, &[n], n as u64, n as u64 * 10)
            .unwrap();
    }
    let last_two = s.messages(&c, None, 2).unwrap();
    assert_eq!(
        last_two.iter().map(|m| m.body[0]).collect::<Vec<_>>(),
        [4, 5]
    );
    let older = s.messages(&c, Some(last_two[0].cursor), 2).unwrap();
    assert_eq!(older.iter().map(|m| m.body[0]).collect::<Vec<_>>(), [2, 3]);
}

#[test]
fn paging_does_not_skip_messages_from_the_same_second() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    for n in 1..=3u8 {
        s.insert_incoming(&c, id(n), MessageKind::Text, &[n], 7, 7)
            .unwrap();
    }
    let page1 = s.messages(&c, None, 2).unwrap();
    let page2 = s.messages(&c, Some(page1[0].cursor), 2).unwrap();
    let all: Vec<u8> = page2.iter().chain(&page1).map(|m| m.body[0]).collect();
    assert_eq!(all, [1, 2, 3]);
}

#[test]
fn content_is_encrypted_and_bound_to_its_row() {
    let mut s = store();
    let c = s.upsert_conversation(ME, BOB, Some("Bob Secret")).unwrap();
    s.insert_incoming(&c, id(1), MessageKind::Text, b"meet at the bridge", 1, 1)
        .unwrap();
    s.insert_incoming(&c, id(2), MessageKind::Text, b"other", 2, 2)
        .unwrap();

    // No plaintext in the raw columns.
    let raw: Vec<u8> = s
        .conn
        .query_row(
            "SELECT body_enc FROM messages WHERE message_id = ?1",
            params![id(1).as_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!raw.windows(6).any(|w| w == b"bridge"));
    let title: Vec<u8> = s
        .conn
        .query_row("SELECT title_enc FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert!(!title.windows(6).any(|w| w == b"Secret"));

    // Moving a ciphertext to another row makes it unreadable.
    s.conn
        .execute(
            "UPDATE messages SET body_enc = ?1 WHERE message_id = ?2",
            params![raw, id(2).as_bytes().as_slice()],
        )
        .unwrap();
    assert!(matches!(s.messages(&c, None, 10), Err(StoreError::Decrypt)));
}

#[test]
fn survives_reopen_and_needs_the_same_vault() {
    let path = tmp_db("reopen");
    {
        let mut s = Store::open(&path, &vault()).unwrap();
        let c = s.upsert_conversation(ME, BOB, None).unwrap();
        s.insert_incoming(&c, id(1), MessageKind::Text, b"kept", 1, 1)
            .unwrap();
    }
    let s = Store::open(&path, &vault()).unwrap();
    let c = conversation_id(ME, BOB);
    assert_eq!(s.messages(&c, None, 10).unwrap()[0].body, b"kept");

    let other = Store::open(&path, &SessionKey::from_bytes([8u8; 32])).unwrap();
    assert!(matches!(
        other.messages(&c, None, 10),
        Err(StoreError::Decrypt)
    ));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn deleted_content_does_not_stay_in_the_wal() {
    let path = tmp_db("wal");
    let wal = path.with_file_name("store.db-wal");
    let mut s = Store::open(&path, &vault()).unwrap();
    let c = s.upsert_conversation(ME, BOB, None).unwrap();
    s.insert_incoming(&c, id(1), MessageKind::Text, b"x", 1, 1)
        .unwrap();
    assert!(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0);
    s.delete_conversation(&c).unwrap();
    assert_eq!(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0), 0);
    drop(s);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_newer_database_is_left_alone() {
    let path = tmp_db("too-new");
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
    }
    assert!(matches!(
        Store::open(&path, &vault()),
        Err(StoreError::TooNew { .. })
    ));
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(
        mode, "delete",
        "the journal mode must not be switched to WAL"
    );
    let v: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION + 1);
    let tables: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tables, 0, "no migration may run on a newer database");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn every_version_migrates_to_the_latest() {
    for start in 0..SCHEMA_VERSION {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::migrate_to(&mut conn, start).unwrap();
        schema::migrate(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}

#[test]
fn safety_pragmas_are_on() {
    let s = store();
    let fk: i64 = s
        .conn
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .unwrap();
    let sd: i64 = s
        .conn
        .pragma_query_value(None, "secure_delete", |r| r.get(0))
        .unwrap();
    assert_eq!((fk, sd), (1, 1));
}
