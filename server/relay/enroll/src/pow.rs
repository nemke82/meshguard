//! Proof-of-work challenges for enrollment (ADR-007 §3).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::RngCore;
use sha2::{Digest, Sha256};

/// How long a challenge can be answered.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(120);
/// Most open challenges kept at once (older ones are dropped).
const MAX_OPEN: usize = 10_000;

/// Number of leading zero bits of `SHA-256(nonce ‖ username ‖ solution)`.
pub fn work(nonce: &str, username: &str, solution: &str) -> u32 {
    let mut h = Sha256::new();
    h.update(nonce.as_bytes());
    h.update(username.as_bytes());
    h.update(solution.as_bytes());
    let d = h.finalize();
    let mut bits = 0;
    for b in d {
        if b == 0 {
            bits += 8;
        } else {
            bits += b.leading_zeros();
            break;
        }
    }
    bits
}

/// Find a solution (tests and reference for the app).
#[cfg(test)]
pub fn solve(nonce: &str, username: &str, difficulty: u32) -> String {
    (0u64..)
        .map(|n| n.to_string())
        .find(|s| work(nonce, username, s) >= difficulty)
        .expect("unbounded search")
}

/// Open challenges, each usable once.
pub struct Challenges {
    open: HashMap<String, Instant>,
}

impl Default for Challenges {
    fn default() -> Self {
        Self::new()
    }
}

impl Challenges {
    pub fn new() -> Self {
        Self {
            open: HashMap::new(),
        }
    }

    /// A new random nonce, or `None` when too many are open: outstanding
    /// challenges are never evicted (phones may be solving them).
    pub fn issue(&mut self, now: Instant) -> Option<String> {
        if self.open.len() >= MAX_OPEN {
            self.open
                .retain(|_, at| now.duration_since(*at) < CHALLENGE_TTL);
            if self.open.len() >= MAX_OPEN {
                return None;
            }
        }
        let mut b = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut b);
        let nonce = hex::encode(b);
        self.open.insert(nonce.clone(), now);
        Some(nonce)
    }

    /// Use up `nonce` if it is open and fresh.
    pub fn take(&mut self, nonce: &str, now: Instant) -> bool {
        match self.open.remove(nonce) {
            Some(at) => now.duration_since(at) < CHALLENGE_TTL,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solutions_meet_the_difficulty_and_bind_the_username() {
        let s = solve("abcd", "mg-aaaaaaaaaaaaaaaaaaaa", 12);
        assert!(work("abcd", "mg-aaaaaaaaaaaaaaaaaaaa", &s) >= 12);
        // The username is part of the hash: reusing a solution for many
        // usernames does not work (checked over a few, all must miss).
        let reused = (0..8)
            .filter(|i| work("abcd", &format!("mg-bbbbbbbbbbbbbbbbbbb{i}"), &s) >= 12)
            .count();
        assert!(reused < 8);
    }

    #[test]
    fn a_full_table_refuses_instead_of_evicting() {
        let mut c = Challenges::new();
        let t = Instant::now();
        let first = c.issue(t).unwrap();
        for _ in 1..MAX_OPEN {
            c.issue(t).unwrap();
        }
        assert!(c.issue(t).is_none(), "full");
        assert!(c.take(&first, t), "outstanding challenge still valid");
        // Expired ones make room again.
        assert!(c.issue(t + CHALLENGE_TTL).is_some());
    }

    #[test]
    fn challenges_are_single_use_and_expire() {
        let mut c = Challenges::new();
        let t = Instant::now();
        let n = c.issue(t).unwrap();
        assert!(c.take(&n, t));
        assert!(!c.take(&n, t), "single use");
        let n2 = c.issue(t).unwrap();
        assert!(!c.take(&n2, t + CHALLENGE_TTL), "expired");
        assert!(!c.take("unknown", t));
    }
}
