//! Offline QR pairing (no passphrase typed).
//!
//! The device that shows the code generates a random 128-bit secret. The
//! scanning device reads it optically, in person, and both derive the chat
//! key from the secret and the two node numbers:
//!
//! ```text
//! key = HKDF-SHA256(salt = "meshguard-qr-v1", ikm = secret,
//!                   info = min(node_a, node_b) BE || max(node_a, node_b) BE)
//! ```
//!
//! Device names are not part of the key, so renaming a radio or not knowing
//! its name yet cannot break pairing. The code is a URI that also works as
//! text to type: `meshguard:pair?v=1&n=<node hex>&k=<secret, base32>`.

use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::crypto::SessionKey;
use crate::error::CoreError;

pub const SECRET_LEN: usize = 16;
const SALT: &[u8] = b"meshguard-qr-v1";
const PREFIX: &str = "meshguard:pair?";
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A pairing offer as encoded in the QR code.
#[derive(Clone, PartialEq, Eq)]
pub struct PairCode {
    /// Node number of the device showing the code.
    pub node: u32,
    pub secret: Zeroizing<[u8; SECRET_LEN]>,
}

impl std::fmt::Debug for PairCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairCode")
            .field("node", &self.node)
            .field("secret", &"<redacted>")
            .finish()
    }
}

fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn unbase32(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !matches!(c, '-' | ' ')) {
        let v = B32
            .iter()
            .position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    Some(out)
}

impl PairCode {
    /// A fresh offer from `node` with a random secret.
    pub fn generate(node: u32) -> Self {
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        OsRng.fill_bytes(secret.as_mut());
        Self { node, secret }
    }

    pub fn to_uri(&self) -> String {
        format!(
            "{PREFIX}v=1&n={:08x}&k={}",
            self.node,
            base32(self.secret.as_ref())
        )
    }

    /// Parse a scanned or typed code. Tolerates case, spaces and dashes in
    /// the secret, and surrounding whitespace.
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let bad = || CoreError::InvalidContent("not a MeshGuard pairing code".into());
        let query = input.trim().strip_prefix(PREFIX).ok_or_else(bad)?;
        let (mut v, mut n, mut k) = (None, None, None);
        for part in query.split('&') {
            let (key, value) = part.split_once('=').ok_or_else(bad)?;
            match key {
                "v" => v = Some(value),
                "n" => n = Some(value),
                "k" => k = Some(value),
                _ => {} // ignore future fields
            }
        }
        if v != Some("1") {
            return Err(CoreError::InvalidContent(
                "this pairing code needs a newer MeshGuard".into(),
            ));
        }
        let node = u32::from_str_radix(n.ok_or_else(bad)?, 16).map_err(|_| bad())?;
        let bytes = Zeroizing::new(unbase32(k.ok_or_else(bad)?).ok_or_else(bad)?);
        if bytes.len() != SECRET_LEN || node == 0 {
            return Err(bad());
        }
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        secret.copy_from_slice(&bytes);
        Ok(Self { node, secret })
    }

    /// The chat key between `self.node` and `other_node`.
    pub fn derive_key(&self, other_node: u32) -> Result<SessionKey, CoreError> {
        derive_qr_key(&self.secret, self.node, other_node)
    }
}

/// Chat key from a QR secret and the two node numbers (order-independent).
pub fn derive_qr_key(
    secret: &[u8; SECRET_LEN],
    node_a: u32,
    node_b: u32,
) -> Result<SessionKey, CoreError> {
    if node_a == node_b {
        return Err(CoreError::InvalidContent(
            "cannot pair a radio with itself".into(),
        ));
    }
    let (lo, hi) = (node_a.min(node_b), node_a.max(node_b));
    let mut info = [0u8; 8];
    info[..4].copy_from_slice(&lo.to_be_bytes());
    info[4..].copy_from_slice(&hi.to_be_bytes());
    let hk = Hkdf::<Sha256>::new(Some(SALT), secret);
    let mut key = Zeroizing::new([0u8; 32]);
    hk.expand(&info, key.as_mut())
        .map_err(|_| CoreError::KeyDerivation)?;
    Ok(SessionKey::from_bytes(*key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_and_tolerant_parsing() {
        let c = PairCode::generate(0x1a2b_3c4d);
        let uri = c.to_uri();
        assert!(uri.starts_with("meshguard:pair?v=1&n=1a2b3c4d&k="));
        assert_eq!(PairCode::parse(&uri).unwrap(), c);
        let k = uri.split("k=").nth(1).unwrap();
        let typed = format!(
            "  meshguard:pair?v=1&n=1a2b3c4d&k={}-{}  ",
            k[..13].to_lowercase(),
            &k[13..]
        );
        assert_eq!(PairCode::parse(&typed).unwrap(), c);
    }

    #[test]
    fn rejects_bad_codes() {
        for s in [
            "",
            "https://example.com",
            "meshguard:pair?v=2&n=1&k=AAAA",
            "meshguard:pair?v=1&n=zz&k=AAAAAAAAAAAAAAAAAAAAAAAAAA",
            "meshguard:pair?v=1&n=1a&k=AAAA",
            "meshguard:pair?v=1&n=0&k=AAAAAAAAAAAAAAAAAAAAAAAAAA",
            "meshguard:pair?v=1&n=1a&k=AAAAAAAAAAAAAAAAAAAAAAAAA1",
        ] {
            assert!(PairCode::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn both_sides_derive_the_same_key() {
        let c = PairCode::generate(10);
        let shower = derive_qr_key(&c.secret, 10, 20).unwrap();
        let scanner = PairCode::parse(&c.to_uri())
            .unwrap()
            .derive_key(20)
            .unwrap();
        let ct = shower.encrypt(b"hi").unwrap();
        assert_eq!(scanner.decrypt(&ct).unwrap(), b"hi");
        let other = derive_qr_key(&c.secret, 10, 30).unwrap();
        assert!(other.decrypt(&ct).is_err(), "bound to the node pair");
        assert!(derive_qr_key(&c.secret, 10, 10).is_err());
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let c = PairCode::generate(1);
        let k = c.to_uri().split("k=").nth(1).unwrap().to_string();
        assert!(!format!("{c:?}").contains(&k));
    }
}
