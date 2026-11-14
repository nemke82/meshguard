//! Bounded duplicate / replay cache keyed by `(sender, message_id)`.
//!
//! Meshtastic relays can deliver the same packet more than once, and future
//! transports may deliver the same logical message twice. A message is shown
//! (and notified) only the first time it is observed; later copies are
//! reported as [`Seen::Duplicate`] so the caller can still re-send a safe ACK.
//!
//! For replay protection the retention window must be at least the largest
//! accepted TTL plus clock skew: anything older is already rejected by
//! [`crate::envelope::EnvelopeV2::check_freshness`]. The cache is pure
//! in-memory state; callers persist it via [`DedupCache::entries`] and
//! [`DedupCache::restore`] so it survives restarts.

use std::collections::{HashMap, VecDeque};

use crate::ids::{MessageId, PeerIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    New,
    Duplicate,
}

type Key = (PeerIdentity, MessageId);

#[derive(Debug, Clone)]
pub struct DedupCache {
    capacity: usize,
    retention_secs: u64,
    seen: HashMap<Key, u64>,
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

    /// Rebuild from persisted `(sender, id, seen_at)` entries, oldest first.
    pub fn restore(
        capacity: usize,
        retention_secs: u64,
        entries: impl IntoIterator<Item = (PeerIdentity, MessageId, u64)>,
        now: u64,
    ) -> Self {
        let mut cache = Self::new(capacity, retention_secs);
        for (sender, id, seen_at) in entries {
            cache.insert((sender, id), seen_at);
        }
        cache.prune(now);
        cache
    }

    /// Record an observation and report whether it is the first one.
    pub fn observe(&mut self, sender: &PeerIdentity, id: MessageId, now: u64) -> Seen {
        self.prune(now);
        let key = (sender.clone(), id);
        if self.seen.contains_key(&key) {
            return Seen::Duplicate;
        }
        self.insert(key, now);
        Seen::New
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

    /// Entries for persistence, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = (&PeerIdentity, &MessageId, u64)> {
        self.order.iter().map(|((p, id), t)| (p, id, *t))
    }

    fn insert(&mut self, key: Key, at: u64) {
        if self.seen.contains_key(&key) {
            return;
        }
        while self.seen.len() >= self.capacity {
            self.pop_oldest();
        }
        self.seen.insert(key.clone(), at);
        self.order.push_back((key, at));
    }

    fn prune(&mut self, now: u64) {
        while let Some((_, at)) = self.order.front() {
            if at.saturating_add(self.retention_secs) > now {
                break;
            }
            self.pop_oldest();
        }
    }

    fn pop_oldest(&mut self) {
        if let Some((key, _)) = self.order.pop_front() {
            self.seen.remove(&key);
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
        assert_eq!(c.observe(&peer(1), id(1), 1), Seen::Duplicate);
        assert_eq!(c.observe(&peer(1), id(1), 99), Seen::Duplicate);
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
        assert_eq!(c.observe(&peer(1), id(1), 100), Seen::New);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut c = DedupCache::new(2, 1000);
        c.observe(&peer(1), id(1), 0);
        c.observe(&peer(1), id(2), 1);
        c.observe(&peer(1), id(3), 2);
        assert_eq!(c.len(), 2);
        assert!(!c.contains(&peer(1), &id(1)));
        assert!(c.contains(&peer(1), &id(3)));
    }

    #[test]
    fn survives_persist_and_restore() {
        let mut c = DedupCache::new(10, 100);
        c.observe(&peer(1), id(1), 10);
        c.observe(&peer(1), id(2), 50);
        let saved: Vec<_> = c.entries().map(|(p, i, t)| (p.clone(), *i, t)).collect();
        let mut r = DedupCache::restore(10, 100, saved, 120);
        // id(1) seen at 10 is past retention at 120; id(2) is not.
        assert_eq!(r.len(), 1);
        assert_eq!(r.observe(&peer(1), id(2), 120), Seen::Duplicate);
        assert_eq!(r.observe(&peer(1), id(1), 120), Seen::New);
    }

    #[test]
    #[should_panic]
    fn zero_capacity_is_rejected() {
        let _ = DedupCache::new(0, 1);
    }
}
