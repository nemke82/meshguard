//! Bounded duplicate / replay cache keyed by `(sender, message_id)`.
//!
//! Meshtastic relays can deliver the same packet more than once, and future
//! transports may deliver the same logical message twice. A message is shown
//! (and notified) only the first time it is observed. Later copies are
//! reported as [`Seen::Duplicate`] with a copy count, so the caller can re-send
//! a safe ACK a *bounded* number of times (see [`Seen::should_ack`]).
//!
//! Replay safety rules:
//! * The retention window must cover the largest accepted TTL plus clock
//!   skew; older frames are rejected by
//!   [`crate::envelope::EnvelopeV2::check_freshness`].
//! * An entry is **never evicted before its retention expires**. When the
//!   cache is full of unexpired entries, a new id is [`Seen::Refused`]: the
//!   caller drops the frame without showing or ACKing it, and the sender's
//!   bounded retry delivers it later. Forgetting an id early would let a
//!   replay of it be shown again.
//!
//! The cache is pure in-memory state. Callers persist it via
//! [`DedupCache::entries`] and [`DedupCache::restore`] so it survives restarts.

use std::collections::{HashMap, VecDeque};

use crate::ids::{MessageId, PeerIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// First observation: show, notify and ACK.
    New,
    /// Already seen; `copies` counts observations including this one (>= 2).
    Duplicate { copies: u32 },
    /// Cache is full of unexpired entries; drop without showing or ACKing.
    Refused,
}

impl Seen {
    /// Whether to (re-)send an application ACK for this observation.
    ///
    /// ACKing a duplicate lets a sender recover from a lost ACK, but every ACK
    /// costs shared LoRa airtime, so replays of one frame may trigger at most
    /// `max_acks_per_message` ACKs in total.
    pub fn should_ack(self, max_acks_per_message: u32) -> bool {
        match self {
            Self::New => max_acks_per_message > 0,
            Self::Duplicate { copies } => copies <= max_acks_per_message,
            Self::Refused => false,
        }
    }
}

type Key = (PeerIdentity, MessageId);

#[derive(Debug, Clone, Copy)]
struct Entry {
    seen_at: u64,
    copies: u32,
}

#[derive(Debug, Clone)]
pub struct DedupCache {
    capacity: usize,
    retention_secs: u64,
    seen: HashMap<Key, Entry>,
    /// Insertion order, oldest first.
    order: VecDeque<(Key, u64)>,
}

impl DedupCache {
    /// # Panics
    /// If `capacity` is zero.
    pub fn new(capacity: usize, retention_secs: u64) -> Self {
        assert!(capacity > 0, "dedup capacity must be non-zero");
        Self {
            capacity,
            retention_secs,
            seen: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Rebuild from persisted `(sender, id, seen_at, copies)` entries, oldest
    /// first. Persisting `copies` keeps the ACK budget across restarts.
    ///
    /// Unexpired entries are all kept, even beyond `capacity`; new ids are then
    /// refused until enough of them expire.
    pub fn restore(
        capacity: usize,
        retention_secs: u64,
        entries: impl IntoIterator<Item = (PeerIdentity, MessageId, u64, u32)>,
        now: u64,
    ) -> Self {
        let mut cache = Self::new(capacity, retention_secs);
        for (sender, id, seen_at, copies) in entries {
            cache.insert((sender, id), seen_at, copies.max(1));
        }
        cache.prune(now);
        cache
    }

    /// Record an observation and classify it.
    pub fn observe(&mut self, sender: &PeerIdentity, id: MessageId, now: u64) -> Seen {
        self.prune(now);
        let key = (sender.clone(), id);
        if let Some(e) = self.seen.get_mut(&key) {
            e.copies = e.copies.saturating_add(1);
            return Seen::Duplicate { copies: e.copies };
        }
        if self.seen.len() >= self.capacity {
            return Seen::Refused;
        }
        self.insert(key, now, 1);
        Seen::New
    }

    /// Undo the observation of `id` (e.g. it could not be persisted, so the
    /// frame is dropped unACKed and the sender's retry must be seen as new).
    pub fn forget(&mut self, sender: &PeerIdentity, id: &MessageId) {
        let key = (sender.clone(), *id);
        if self.seen.remove(&key).is_some() {
            self.order.retain(|(k, _)| k != &key);
        }
    }

    pub fn contains(&self, sender: &PeerIdentity, id: &MessageId) -> bool {
        self.seen.contains_key(&(sender.clone(), *id))
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// `(sender, id, seen_at, copies)` for persistence, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = (&PeerIdentity, &MessageId, u64, u32)> {
        self.order.iter().filter_map(|(key, t)| {
            let e = self.seen.get(key)?;
            (e.seen_at == *t).then_some((&key.0, &key.1, *t, e.copies))
        })
    }

    fn insert(&mut self, key: Key, at: u64, copies: u32) {
        if self.seen.contains_key(&key) {
            return;
        }
        self.seen.insert(
            key.clone(),
            Entry {
                seen_at: at,
                copies,
            },
        );
        self.order.push_back((key, at));
    }

    /// Drop entries whose retention has expired. Entries are only ever
    /// removed here, never for capacity.
    ///
    /// An entry is kept *through* `seen_at + retention` inclusive, matching
    /// the inclusive expiry boundary of `check_freshness`: with retention =
    /// max TTL + 2·skew, the last instant a frame can pass freshness is still
    /// covered.
    fn prune(&mut self, now: u64) {
        // `order` is insertion order; with a non-monotonic clock an older
        // entry may sit behind a newer one, which only delays its removal.
        while let Some((key, at)) = self.order.front() {
            if at.saturating_add(self.retention_secs) >= now {
                break;
            }
            if self.seen.get(key).map(|e| e.seen_at) == Some(*at) {
                self.seen.remove(key);
            }
            self.order.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u32) -> PeerIdentity {
        PeerIdentity::meshtastic_node(n)
    }

    fn id(b: u8) -> MessageId {
        MessageId::from_bytes([b; 16])
    }

    #[test]
    fn first_is_new_then_duplicate() {
        let mut c = DedupCache::new(10, 100);
        assert_eq!(c.observe(&peer(1), id(1), 0), Seen::New);
        assert_eq!(c.observe(&peer(1), id(1), 1), Seen::Duplicate { copies: 2 });
        assert_eq!(
            c.observe(&peer(1), id(1), 99),
            Seen::Duplicate { copies: 3 }
        );
    }

    #[test]
    fn same_id_from_different_senders_is_distinct() {
        let mut c = DedupCache::new(10, 100);
        assert_eq!(c.observe(&peer(1), id(1), 0), Seen::New);
        assert_eq!(c.observe(&peer(2), id(1), 0), Seen::New);
    }

    #[test]
    fn retention_expires_entries() {
        let mut c = DedupCache::new(10, 100);
        c.observe(&peer(1), id(1), 0);
        // Still remembered at exactly seen_at + retention (inclusive).
        assert_eq!(
            c.observe(&peer(1), id(1), 100),
            Seen::Duplicate { copies: 2 }
        );
        assert_eq!(c.observe(&peer(1), id(1), 101), Seen::New);
    }

    #[test]
    fn full_cache_refuses_instead_of_forgetting_unexpired_ids() {
        let mut c = DedupCache::new(2, 1000);
        assert_eq!(c.observe(&peer(1), id(1), 0), Seen::New);
        assert_eq!(c.observe(&peer(1), id(2), 1), Seen::New);
        assert_eq!(c.observe(&peer(1), id(3), 2), Seen::Refused);
        // The earlier ids are still remembered: replays stay duplicates.
        assert_eq!(c.observe(&peer(1), id(1), 3), Seen::Duplicate { copies: 2 });
        assert_eq!(c.len(), 2);
        // Once id(1) expires there is room again.
        assert_eq!(c.observe(&peer(1), id(3), 1001), Seen::New);
        assert!(!c.contains(&peer(1), &id(1)));
    }

    #[test]
    fn ack_budget_per_message() {
        assert!(Seen::New.should_ack(3));
        assert!(Seen::Duplicate { copies: 3 }.should_ack(3));
        assert!(!Seen::Duplicate { copies: 4 }.should_ack(3));
        assert!(!Seen::Refused.should_ack(3));
        assert!(!Seen::New.should_ack(0));
    }

    #[test]
    fn restore_keeps_unexpired_entries_beyond_capacity() {
        let saved = (0..4u8).map(|i| (peer(1), id(i), u64::from(i), 1));
        let mut c = DedupCache::restore(2, 1000, saved, 10);
        assert_eq!(c.len(), 4);
        assert_eq!(
            c.observe(&peer(1), id(0), 10),
            Seen::Duplicate { copies: 2 }
        );
        assert_eq!(c.observe(&peer(1), id(9), 10), Seen::Refused);
    }

    #[test]
    fn survives_persist_and_restore() {
        let mut c = DedupCache::new(10, 100);
        c.observe(&peer(1), id(1), 10);
        c.observe(&peer(1), id(2), 50);
        let saved: Vec<_> = c
            .entries()
            .map(|(p, i, t, n)| (p.clone(), *i, t, n))
            .collect();
        let mut r = DedupCache::restore(10, 100, saved, 120);
        // id(1) seen at 10 is past retention at 120; id(2) is not.
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.observe(&peer(1), id(2), 120),
            Seen::Duplicate { copies: 2 }
        );
        assert_eq!(r.observe(&peer(1), id(1), 120), Seen::New);
    }

    #[test]
    fn forget_makes_the_id_new_again() {
        let mut c = DedupCache::new(10, 100);
        c.observe(&peer(1), id(1), 0);
        c.forget(&peer(1), &id(1));
        assert!(c.is_empty());
        assert_eq!(c.entries().count(), 0);
        assert_eq!(c.observe(&peer(1), id(1), 1), Seen::New);
    }

    #[test]
    fn ack_budget_survives_restart() {
        let mut c = DedupCache::new(10, 100);
        c.observe(&peer(1), id(1), 0);
        c.observe(&peer(1), id(1), 1);
        c.observe(&peer(1), id(1), 2);
        let saved: Vec<_> = c
            .entries()
            .map(|(p, i, t, n)| (p.clone(), *i, t, n))
            .collect();
        let mut r = DedupCache::restore(10, 100, saved, 3);
        let next = r.observe(&peer(1), id(1), 3);
        assert_eq!(next, Seen::Duplicate { copies: 4 });
        assert!(!next.should_ack(3));
    }

    #[test]
    fn max_future_dated_frame_is_covered_at_freshness_boundary() {
        use crate::envelope::EnvelopeV2;
        let (skew, ttl_mins) = (30u64, 2u16);
        let retention = u64::from(ttl_mins) * 60 + 2 * skew;
        let mut c = DedupCache::new(10, retention);
        let now0 = 1000;
        let env = EnvelopeV2::text("x", (now0 + skew) as u32, ttl_mins);
        assert!(env.check_freshness(now0, skew, ttl_mins).is_ok());
        assert_eq!(c.observe(&peer(1), env.message_id, now0), Seen::New);
        let last_ok = env.expires_at() + skew;
        assert!(env.check_freshness(last_ok, skew, ttl_mins).is_ok());
        assert_ne!(c.observe(&peer(1), env.message_id, last_ok), Seen::New);
        assert!(env.check_freshness(last_ok + 1, skew, ttl_mins).is_err());
    }

    #[test]
    #[should_panic]
    fn zero_capacity_is_rejected() {
        let _ = DedupCache::new(0, 1);
    }
}
