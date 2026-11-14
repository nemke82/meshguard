//! Offline QR pairing (ADR-004): no passphrase typed.
//!
//! The device that shows the code (A) creates a pairing offer: a random
//! 128-bit secret and an X25519 key pair. The QR code carries A's node
//! number, the secret and A's public key. The scanning device (B):
//!
//! 1. creates its own X25519 key pair;
//! 2. derives the **chat key** from the X25519 shared secret *and* the QR
//!    secret: `HKDF-SHA256(salt "meshguard-qr-v1", ikm = DH ‖ secret,
//!    info = min(node) BE ‖ max(node) BE)`;
//! 3. sends a v2 `PairRequest` sealed with a **request key** derived from
//!    the QR secret alone (salt "meshguard-qr-v1-request"); its body is
//!    `mgqr1:<B public key hex>:<B name>`.
//!
//! A opens the request with the request key, computes the same DH and chat
//! key, and answers with a `PairAccept` sealed with the chat key.
//!
//! Someone who photographs the code (or another family member who scans it)
//! learns the secret and both public keys, but not the DH secret, so cannot
//! derive anyone else's chat key. Device names are not part of any key.

use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::crypto::SessionKey;
use crate::error::CoreError;

pub const SECRET_LEN: usize = 16;
const CHAT_SALT: &[u8] = b"meshguard-qr-v1";
const REQUEST_SALT: &[u8] = b"meshguard-qr-v1-request";
const PREFIX: &str = "meshguard:pair?";
const BODY_PREFIX: &str = "mgqr1:";
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// What the QR code carries.
#[derive(Clone, PartialEq, Eq)]
pub struct PairCode {
    /// Node number of the device showing the code.
    pub node: u32,
    pub secret: Zeroizing<[u8; SECRET_LEN]>,
    /// X25519 public key of the device showing the code.
    pub public: [u8; 32],
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
        acc &= (1 << bits) - 1;
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

fn node_info(a: u32, b: u32) -> [u8; 8] {
    let (lo, hi) = (a.min(b), a.max(b));
    let mut info = [0u8; 8];
    info[..4].copy_from_slice(&lo.to_be_bytes());
    info[4..].copy_from_slice(&hi.to_be_bytes());
    info
}

fn hkdf(salt: &[u8], ikm: &[u8], a: u32, b: u32) -> Result<SessionKey, CoreError> {
    if a == b {
        return Err(CoreError::InvalidContent(
            "cannot pair a radio with itself".into(),
        ));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(&node_info(a, b), key.as_mut())
        .map_err(|_| CoreError::KeyDerivation)?;
    Ok(SessionKey::from_bytes(*key))
}

fn chat_key(
    shared: &x25519_dalek::SharedSecret,
    secret: &[u8; SECRET_LEN],
    a: u32,
    b: u32,
) -> Result<SessionKey, CoreError> {
    if !shared.was_contributory() {
        return Err(CoreError::InvalidContent("invalid pairing key".into()));
    }
    let mut ikm = Zeroizing::new([0u8; 32 + SECRET_LEN]);
    ikm[..32].copy_from_slice(shared.as_bytes());
    ikm[32..].copy_from_slice(secret);
    hkdf(CHAT_SALT, ikm.as_ref(), a, b)
}

impl PairCode {
    pub fn to_uri(&self) -> String {
        format!(
            "{PREFIX}v=1&n={:08x}&k={}&p={}",
            self.node,
            base32(self.secret.as_ref()),
            base32(&self.public)
        )
    }

    /// Parse a scanned or typed code. Tolerates case, spaces and dashes in
    /// the base32 parts, and surrounding whitespace.
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let bad = || CoreError::InvalidContent("not a MeshGuard pairing code".into());
        let query = input.trim().strip_prefix(PREFIX).ok_or_else(bad)?;
        let (mut v, mut n, mut k, mut p) = (None, None, None, None);
        for part in query.split('&') {
            let (key, value) = part.split_once('=').ok_or_else(bad)?;
            match key {
                "v" => v = Some(value),
                "n" => n = Some(value),
                "k" => k = Some(value),
                "p" => p = Some(value),
                _ => {} // ignore future fields
            }
        }
        if v != Some("1") {
            return Err(CoreError::InvalidContent(
                "this pairing code needs a newer MeshGuard".into(),
            ));
        }
        let node = u32::from_str_radix(n.ok_or_else(bad)?, 16).map_err(|_| bad())?;
        let k = Zeroizing::new(unbase32(k.ok_or_else(bad)?).ok_or_else(bad)?);
        let p = unbase32(p.ok_or_else(bad)?).ok_or_else(bad)?;
        if k.len() != SECRET_LEN || p.len() != 32 || node == 0 {
            return Err(bad());
        }
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        secret.copy_from_slice(&k);
        let mut public = [0u8; 32];
        public.copy_from_slice(&p);
        Ok(Self {
            node,
            secret,
            public,
        })
    }

    /// Key that seals the scanner's pair request (QR secret only).
    pub fn request_key(&self, scanner_node: u32) -> Result<SessionKey, CoreError> {
        hkdf(REQUEST_SALT, self.secret.as_ref(), self.node, scanner_node)
    }
}

/// The showing side: the code plus its private key. One offer may be
/// scanned by several devices; each gets an independent chat key.
pub struct PairOffer {
    code: PairCode,
    private: StaticSecret,
}

impl PairOffer {
    pub fn generate(node: u32) -> Self {
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        OsRng.fill_bytes(secret.as_mut());
        let private = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&private).to_bytes();
        Self {
            code: PairCode {
                node,
                secret,
                public,
            },
            private,
        }
    }

    pub fn code(&self) -> &PairCode {
        &self.code
    }

    /// From a decrypted pair request body, derive the chat key with the
    /// scanner. Returns the key and the scanner's announced name.
    pub fn accept(&self, scanner_node: u32, body: &str) -> Result<(SessionKey, String), CoreError> {
        let (their_public, name) = parse_request_body(body)
            .ok_or_else(|| CoreError::InvalidContent("not a QR pair request".into()))?;
        let shared = self.private.diffie_hellman(&PublicKey::from(their_public));
        let key = chat_key(&shared, &self.code.secret, self.code.node, scanner_node)?;
        Ok((key, name))
    }
}

/// The scanning side's half of the handshake.
pub struct ScanResult {
    /// Seals the pair request (the shower opens it with the QR secret).
    pub request_key: SessionKey,
    /// The chat key; install it before sending, the accept is sealed with it.
    pub chat_key: SessionKey,
    /// Pair request body carrying our public key and name.
    pub request_body: String,
}

/// Scan `code` as node `my_node` named `my_name`.
pub fn scan(code: &PairCode, my_node: u32, my_name: &str) -> Result<ScanResult, CoreError> {
    let private = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&private).to_bytes();
    let shared = private.diffie_hellman(&PublicKey::from(code.public));
    Ok(ScanResult {
        request_key: code.request_key(my_node)?,
        chat_key: chat_key(&shared, &code.secret, code.node, my_node)?,
        request_body: format!("{BODY_PREFIX}{}:{my_name}", hex32(&public)),
    })
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `mgqr1:<64 hex>:<name>` -> (public key, name).
pub fn parse_request_body(body: &str) -> Option<([u8; 32], String)> {
    let rest = body.strip_prefix(BODY_PREFIX)?;
    let (hex, name) = rest.split_once(':')?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some((out, name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_and_tolerant_parsing() {
        let offer = PairOffer::generate(0x1a2b_3c4d);
        let uri = offer.code().to_uri();
        assert!(uri.starts_with("meshguard:pair?v=1&n=1a2b3c4d&k="));
        assert_eq!(&PairCode::parse(&uri).unwrap(), offer.code());
        // Lower case, a dash inside the secret, surrounding spaces.
        let lowered = uri.to_lowercase();
        let k_at = lowered.find("&k=").unwrap() + 3 + 10;
        let typed = format!("  {}-{}  ", &lowered[..k_at], &lowered[k_at..]);
        assert_eq!(&PairCode::parse(&typed).unwrap(), offer.code());
    }

    #[test]
    fn rejects_bad_codes() {
        for s in [
            "",
            "https://example.com",
            "meshguard:pair?v=2&n=1&k=AAAA&p=AAAA",
            "meshguard:pair?v=1&n=zz&k=AAAAAAAAAAAAAAAAAAAAAAAAAA&p=AAAA",
            "meshguard:pair?v=1&n=1a&k=AAAA&p=AAAA",
            "meshguard:pair?v=1&n=1a&k=AAAAAAAAAAAAAAAAAAAAAAAAAA",
            "meshguard:pair?v=1&n=0&k=AAAAAAAAAAAAAAAAAAAAAAAAAA&p=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(PairCode::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn handshake_gives_both_sides_the_same_chat_key() {
        let offer = PairOffer::generate(10);
        let code = PairCode::parse(&offer.code().to_uri()).unwrap();
        let scanner = scan(&code, 20, "Bob").unwrap();

        // The shower opens the request with the request key ...
        let req = scanner
            .request_key
            .encrypt(scanner.request_body.as_bytes())
            .unwrap();
        let body = offer.code().request_key(20).unwrap().decrypt(&req).unwrap();
        // ... and derives the same chat key.
        let (shower_key, name) = offer
            .accept(20, std::str::from_utf8(&body).unwrap())
            .unwrap();
        assert_eq!(name, "Bob");
        let ct = shower_key.encrypt(b"accept").unwrap();
        assert_eq!(scanner.chat_key.decrypt(&ct).unwrap(), b"accept");
        assert!(
            scanner.request_key.decrypt(&ct).is_err(),
            "request key != chat key"
        );
    }

    #[test]
    fn someone_who_saw_the_code_cannot_derive_the_chat_key() {
        let offer = PairOffer::generate(10);
        let code = offer.code().clone();
        let bob = scan(&code, 20, "Bob").unwrap();
        // The eavesdropper knows the code and Bob's public key (it can open
        // Bob's request with the request key), but has no private key.
        let eve = scan(&code, 20, "Eve").unwrap();
        let ct = bob.chat_key.encrypt(b"secret").unwrap();
        assert!(eve.chat_key.decrypt(&ct).is_err());
        // Two family members scanning the same code get independent keys.
        let carol = scan(&code, 30, "Carol").unwrap();
        let (carol_at_shower, _) = offer.accept(30, &carol.request_body).unwrap();
        assert!(carol_at_shower.decrypt(&ct).is_err());
    }

    #[test]
    fn request_body_parsing() {
        let offer = PairOffer::generate(10);
        let s = scan(offer.code(), 20, "A:B").unwrap();
        let (_, name) = parse_request_body(&s.request_body).unwrap();
        assert_eq!(name, "A:B");
        assert!(parse_request_body("Bob").is_none());
        assert!(parse_request_body("mgqr1:zz:Bob").is_none());
        assert!(offer.accept(20, "Bob").is_err());
        assert!(offer.accept(10, &s.request_body).is_err(), "own node");
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let offer = PairOffer::generate(1);
        let uri = offer.code().to_uri();
        let k = uri
            .split("k=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        assert!(!format!("{:?}", offer.code()).contains(&k));
    }
}
