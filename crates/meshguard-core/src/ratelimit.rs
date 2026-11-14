//! Time-injected rate limiters for shared-airtime protection.
//!
//! * [`TokenBucket`]: global budget, e.g. outbound ACK transmissions.
//! * [`Cooldown`]: at most one event per key per interval, e.g. one
//!   "chat request" notification per unknown node per minute.
//!
//! Both take `now` (unix seconds) as an argument, so they are deterministic.

use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: u32,
    refill_every_secs: u64,
    tokens: u32,
    last_refill: Option<u64>,
}

impl TokenBucket {
    /// Starts full. One token is added every `refill_every_secs`, up to
    /// `capacity`.
    pub fn new(capacity: u32, refill_every_secs: u64) -> Self {
        Self {
            capacity,
            refill_every_secs: refill_every_secs.max(1),
            tokens: capacity,
            last_refill: None,
        }
    }

    fn refill(&mut self, now: u64) {
        let Some(last) = self.last_refill else {
            self.last_refill = Some(now);
            return;
        };
        if now <= last {
            return;
        }
        let gained = (now - last) / self.refill_every_secs;
        if gained > 0 {
            let gained = u32::try_from(gained).unwrap_or(u32::MAX);
            self.tokens = self.tokens.saturating_add(gained).min(self.capacity);
            self.last_refill = Some(last + u64::from(gained) * self.refill_every_secs);
        }
        if self.tokens == self.capacity {
            self.last_refill = Some(now);
        }
    }

    /// Take one token if available.
    pub fn try_take(&mut self, now: u64) -> bool {
        self.refill(now);
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

#[derive(Debug, Clone)]
pub struct Cooldown<K> {
    interval_secs: u64,
    max_keys: usize,
    last: HashMap<K, u64>,
}

impl<K: Eq + Hash + Clone> Cooldown<K> {
    /// `max_keys` bounds memory; when full, expired keys are dropped and,
    /// if none expired, new keys are refused.
    pub fn new(interval_secs: u64, max_keys: usize) -> Self {
        Self {
            interval_secs,
            max_keys,
            last: HashMap::new(),
        }
    }

    /// True (and records `now`) if `key` has not fired within the interval.
    pub fn try_fire(&mut self, key: &K, now: u64) -> bool {
        if let Some(&t) = self.last.get(key) {
            if now < t.saturating_add(self.interval_secs) {
                return false;
            }
        } else if self.last.len() >= self.max_keys {
            let interval = self.interval_secs;
            self.last.retain(|_, t| now < t.saturating_add(interval));
            if self.last.len() >= self.max_keys {
                return false;
            }
        }
        self.last.insert(key.clone(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_bursts_then_refills() {
        let mut b = TokenBucket::new(3, 10);
        assert!(b.try_take(0));
        assert!(b.try_take(0));
        assert!(b.try_take(0));
        assert!(!b.try_take(5));
        assert!(b.try_take(10));
        assert!(!b.try_take(10));
        assert!(b.try_take(1000));
        assert!(b.try_take(1000));
        assert!(b.try_take(1000));
        assert!(!b.try_take(1000), "refill is capped at capacity");
    }

    #[test]
    fn bucket_ignores_clock_going_backwards() {
        let mut b = TokenBucket::new(1, 10);
        assert!(b.try_take(100));
        assert!(!b.try_take(50));
        assert!(b.try_take(110));
    }

    #[test]
    fn cooldown_per_key() {
        let mut c = Cooldown::new(60, 10);
        assert!(c.try_fire(&1u32, 0));
        assert!(!c.try_fire(&1u32, 59));
        assert!(c.try_fire(&2u32, 1));
        assert!(c.try_fire(&1u32, 60));
    }

    #[test]
    fn cooldown_bounds_memory() {
        let mut c = Cooldown::new(60, 2);
        assert!(c.try_fire(&1u32, 0));
        assert!(c.try_fire(&2u32, 0));
        assert!(!c.try_fire(&3u32, 10), "full of live keys");
        assert!(c.try_fire(&3u32, 60), "expired keys are evicted");
    }
}
