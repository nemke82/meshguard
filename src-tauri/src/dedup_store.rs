//! Interim on-disk persistence of the replay/dedup cache.
//!
//! Keeps `(sender, message_id, seen_at, copies)` in `dedup.json` next to
//! `meshguard.json`, so a captured v2 frame is not shown again after an app
//! restart, and the per-message ACK budget is not refilled. It holds ids and
//! timestamps only, never content. It will be replaced by the Rust-owned SQLite
//! store (ADR-001 §5).

use std::path::Path;

use meshguard_core::dedup::DedupCache;
use meshguard_core::inbound::InboundPolicy;
use meshguard_core::{MessageId, PeerIdentity};
use serde::{Deserialize, Serialize};

use crate::error::MeshGuardError;

const FILE: &str = "dedup.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    /// `[sender_hex, message_id_hex, seen_at, copies]`, oldest first.
    entries: Vec<(String, String, u64, u32)>,
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Load the cache, dropping expired entries. A missing or unreadable file
/// yields an empty cache (logged), never an error that blocks startup.
pub fn load(dir: &Path, policy: &InboundPolicy, capacity: usize, now: u64) -> DedupCache {
    let path = dir.join(FILE);
    let stored: Option<Stored> = match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str(&data) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("Ignoring unreadable {FILE}: {e}");
                None
            }
        },
        Err(_) => None,
    };
    let entries = stored
        .filter(|s| s.version == FORMAT_VERSION)
        .map(|s| s.entries)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(p, id, at, n)| {
            Some((
                PeerIdentity::from_bytes(from_hex(&p)?),
                MessageId::from_slice(&from_hex(&id)?)?,
                at,
                n,
            ))
        });
    DedupCache::restore(capacity, policy.dedup_retention_secs(), entries, now)
}

/// Write the cache atomically (temp file + rename).
pub fn save(dir: &Path, cache: &DedupCache) -> Result<(), MeshGuardError> {
    let stored = Stored {
        version: FORMAT_VERSION,
        entries: cache
            .entries()
            .map(|(p, id, at, n)| (to_hex(p.as_bytes()), id.to_hex(), at, n))
            .collect(),
    };
    let data =
        serde_json::to_vec(&stored).map_err(|e| MeshGuardError::Serialization(e.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| MeshGuardError::Io(e.to_string()))?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, data).map_err(|e| MeshGuardError::Io(e.to_string()))?;
    std::fs::rename(&tmp, dir.join(FILE)).map_err(|e| MeshGuardError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshguard_core::dedup::Seen;

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("meshguard-dedup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_keeps_ids_and_copy_counts() {
        let dir = tempdir();
        let policy = InboundPolicy::default();
        let peer = PeerIdentity::meshtastic_node(7);
        let id = MessageId::random();
        let mut c = policy.new_dedup_cache(16);
        c.observe(&peer, id, 100);
        c.observe(&peer, id, 101);
        save(&dir, &c).unwrap();

        let mut r = load(&dir, &policy, 16, 200);
        assert_eq!(r.observe(&peer, id, 200), Seen::Duplicate { copies: 3 });
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_or_corrupt_file_gives_empty_cache() {
        let dir = tempdir();
        let policy = InboundPolicy::default();
        assert!(load(&dir, &policy, 16, 0).is_empty());
        std::fs::write(dir.join(FILE), b"{not json").unwrap();
        assert!(load(&dir, &policy, 16, 0).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hex_rejects_garbage() {
        assert_eq!(from_hex("0a1b"), Some(vec![0x0a, 0x1b]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("é1"), None);
    }
}
