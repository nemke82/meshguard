//! Private MQTT relay: per-conversation secrets, opaque topics and the
//! bodies of the relay control messages (ADR-006).
//!
//! The relay carries the *same* MG2 frames as LoRa; nothing here encrypts
//! message content. A relay secret is independent of the chat key and only
//! decides where frames are published (topics) and proves possession in
//! accept/revoke messages.

use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Feature bit: understands the relay control messages (0x20–0x22).
pub const FEATURE_RELAY: u32 = 1 << 1;

/// Topic namespace and version.
pub const TOPIC_PREFIX: &str = "mg/r1/";
/// Bytes of a relay secret.
pub const SECRET_LEN: usize = 32;
/// Bytes of the `confirm` tag.
pub const CONFIRM_LEN: usize = 16;
/// Bytes of the broker tag.
pub const BROKER_TAG_LEN: usize = 8;
/// Longest broker username (inbox) accepted.
pub const MAX_INBOX_LEN: usize = 64;
/// Bytes of a topic alias before base32 (160 bits).
const ALIAS_BYTES: usize = 20;
/// Body format version of the relay control messages.
const BODY_VERSION: u8 = 1;

const SALT: &[u8] = b"meshguard-relay-v1";
const BROKER_LABEL: &[u8] = b"meshguard-relay-broker-v1";

/// A 256-bit per-conversation relay secret from the OS CSPRNG.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct RelaySecret([u8; SECRET_LEN]);

impl RelaySecret {
    pub fn random() -> Self {
        let mut b = [0u8; SECRET_LEN];
        rand::rngs::OsRng.fill_bytes(&mut b);
        Self(b)
    }

    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    fn expand(&self, info: &[u8], out: &mut [u8]) {
        Hkdf::<Sha256>::new(Some(SALT), &self.0)
            .expand(info, out)
            .expect("HKDF output length is small and fixed");
    }

    /// Proof of holding this secret, sent in accept and revoke messages.
    pub fn confirm_tag(&self) -> [u8; CONFIRM_LEN] {
        let mut t = [0u8; CONFIRM_LEN];
        self.expand(b"confirm", &mut t);
        t
    }

    /// Opaque alias of the direction `sender -> recipient` (node numbers).
    pub fn alias(&self, sender: u32, recipient: u32) -> String {
        let mut info = Vec::with_capacity(13);
        info.extend_from_slice(b"topic");
        info.extend_from_slice(&sender.to_be_bytes());
        info.extend_from_slice(&recipient.to_be_bytes());
        let mut a = [0u8; ALIAS_BYTES];
        self.expand(&info, &mut a);
        crate::qrpair::base32(&a).to_ascii_lowercase()
    }

    /// The exact topic `sender` publishes to and `recipient` subscribes to:
    /// `mg/r1/<recipient user>/<sender user>/<alias>`. The user segments
    /// let the broker ACL bind reads to the recipient and writes to the
    /// sender (ADR-006 §5).
    pub fn topic(
        &self,
        sender: u32,
        recipient: u32,
        sender_user: &str,
        recipient_user: &str,
    ) -> Result<String, RelayError> {
        validate_inbox(sender_user)?;
        validate_inbox(recipient_user)?;
        Ok(format!(
            "{TOPIC_PREFIX}{recipient_user}/{sender_user}/{}",
            self.alias(sender, recipient)
        ))
    }
}

impl std::fmt::Debug for RelaySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelaySecret(<redacted>)")
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RelayError {
    #[error(
        "broker username must be 1–{MAX_INBOX_LEN} characters of A–Z, a–z, 0–9, '.', '_' or '-'"
    )]
    BadInbox,
    #[error("malformed relay message")]
    Malformed,
    #[error("unsupported relay message version {0}")]
    Version(u8),
}

/// A broker username usable as an inbox segment of a topic: no MQTT
/// wildcards, separators or control characters, and short.
pub fn validate_inbox(inbox: &str) -> Result<(), RelayError> {
    let ok = (1..=MAX_INBOX_LEN).contains(&inbox.len())
        && inbox
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && inbox != "."
        && inbox != "..";
    if ok {
        Ok(())
    } else {
        Err(RelayError::BadInbox)
    }
}

/// A broker username to suggest: opaque, so topics carry no names
/// (`mg-` and 20 random base32 characters, 100 bits).
pub fn suggest_username() -> String {
    let mut b = [0u8; 13];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let mut s = crate::qrpair::base32(&b).to_ascii_lowercase();
    s.truncate(20);
    format!("mg-{s}")
}

/// Whether `user` has the app's opaque username form (ADR-006 §5): `mg-`
/// and 20 base32 characters. The app accepts only these for the relay, so
/// topic paths never carry a name; the wire format allows any inbox that
/// passes [`validate_inbox`].
pub fn is_app_username(user: &str) -> bool {
    user.len() == 23
        && user.starts_with("mg-")
        && user[3..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// Short fingerprint of a broker address, so a receiver can warn when its
/// contact uses another broker. Not secret.
pub fn broker_tag(host: &str, port: u16) -> [u8; BROKER_TAG_LEN] {
    let mut h = Sha256::new();
    h.update(BROKER_LABEL);
    h.update(host.trim().to_ascii_lowercase().as_bytes());
    h.update(b":");
    h.update(port.to_string().as_bytes());
    let d = h.finalize();
    let mut t = [0u8; BROKER_TAG_LEN];
    t.copy_from_slice(&d[..BROKER_TAG_LEN]);
    t
}

// ── Control message bodies ─────────────────────────────────────

/// `RelayOffer` (0x20): the sender proposes a relay secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOffer {
    pub secret: RelaySecret,
    pub broker_tag: [u8; BROKER_TAG_LEN],
    /// The offerer's broker username.
    pub inbox: String,
}

/// `RelayAccept` (0x21): the receiver of an offer accepted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAccept {
    pub confirm: [u8; CONFIRM_LEN],
    /// The accepter's broker; must equal the offerer's.
    pub broker_tag: [u8; BROKER_TAG_LEN],
    /// The accepter's broker username.
    pub inbox: String,
}

/// `RelayRevoke` (0x22): the sender stopped using the relay with us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRevoke {
    pub confirm: [u8; CONFIRM_LEN],
}

fn push_inbox(out: &mut Vec<u8>, inbox: &str) -> Result<(), RelayError> {
    validate_inbox(inbox)?;
    out.push(inbox.len() as u8);
    out.extend_from_slice(inbox.as_bytes());
    Ok(())
}

/// Read `len ‖ inbox` that must end the body exactly.
fn take_inbox(rest: &[u8]) -> Result<String, RelayError> {
    let (&len, name) = rest.split_first().ok_or(RelayError::Malformed)?;
    if name.len() != usize::from(len) {
        return Err(RelayError::Malformed);
    }
    let inbox = std::str::from_utf8(name).map_err(|_| RelayError::Malformed)?;
    validate_inbox(inbox)?;
    Ok(inbox.to_string())
}

fn check_version(body: &[u8]) -> Result<&[u8], RelayError> {
    let (&v, rest) = body.split_first().ok_or(RelayError::Malformed)?;
    if v != BODY_VERSION {
        return Err(RelayError::Version(v));
    }
    Ok(rest)
}

impl RelayOffer {
    pub fn encode(&self) -> Result<Vec<u8>, RelayError> {
        let mut out = Vec::with_capacity(1 + SECRET_LEN + BROKER_TAG_LEN + 1 + self.inbox.len());
        out.push(BODY_VERSION);
        out.extend_from_slice(self.secret.as_bytes());
        out.extend_from_slice(&self.broker_tag);
        push_inbox(&mut out, &self.inbox)?;
        Ok(out)
    }

    pub fn decode(body: &[u8]) -> Result<Self, RelayError> {
        let rest = check_version(body)?;
        if rest.len() < SECRET_LEN + BROKER_TAG_LEN {
            return Err(RelayError::Malformed);
        }
        let (secret, rest) = rest.split_at(SECRET_LEN);
        let (tag, rest) = rest.split_at(BROKER_TAG_LEN);
        Ok(Self {
            secret: RelaySecret::from_bytes(secret.try_into().map_err(|_| RelayError::Malformed)?),
            broker_tag: tag.try_into().map_err(|_| RelayError::Malformed)?,
            inbox: take_inbox(rest)?,
        })
    }
}

impl RelayAccept {
    pub fn encode(&self) -> Result<Vec<u8>, RelayError> {
        let mut out = Vec::with_capacity(1 + CONFIRM_LEN + BROKER_TAG_LEN + 1 + self.inbox.len());
        out.push(BODY_VERSION);
        out.extend_from_slice(&self.confirm);
        out.extend_from_slice(&self.broker_tag);
        push_inbox(&mut out, &self.inbox)?;
        Ok(out)
    }

    pub fn decode(body: &[u8]) -> Result<Self, RelayError> {
        let rest = check_version(body)?;
        if rest.len() < CONFIRM_LEN + BROKER_TAG_LEN {
            return Err(RelayError::Malformed);
        }
        let (confirm, rest) = rest.split_at(CONFIRM_LEN);
        let (tag, rest) = rest.split_at(BROKER_TAG_LEN);
        Ok(Self {
            confirm: confirm.try_into().map_err(|_| RelayError::Malformed)?,
            broker_tag: tag.try_into().map_err(|_| RelayError::Malformed)?,
            inbox: take_inbox(rest)?,
        })
    }
}

impl RelayRevoke {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + CONFIRM_LEN);
        out.push(BODY_VERSION);
        out.extend_from_slice(&self.confirm);
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, RelayError> {
        let rest = check_version(body)?;
        Ok(Self {
            confirm: rest.try_into().map_err(|_| RelayError::Malformed)?,
        })
    }
}

/// Constant-time comparison of two confirm tags.
pub fn confirm_matches(a: &[u8; CONFIRM_LEN], b: &[u8; CONFIRM_LEN]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> RelaySecret {
        let mut b = [0u8; SECRET_LEN];
        for (i, x) in b.iter_mut().enumerate() {
            *x = i as u8;
        }
        RelaySecret::from_bytes(b)
    }

    /// Fixed vectors: these values must never change between releases, or
    /// two app versions would publish and subscribe on different topics.
    #[test]
    fn topic_and_confirm_vectors_are_stable() {
        let s = secret();
        assert_eq!(
            s.topic(0x11, 0x22, "alice", "bob").unwrap(),
            format!("mg/r1/bob/alice/{}", VECTOR_ALIAS_11_22)
        );
        assert_eq!(s.alias(0x11, 0x22), VECTOR_ALIAS_11_22);
        assert_eq!(s.alias(0x22, 0x11), VECTOR_ALIAS_22_11);
        assert_eq!(hex(&s.confirm_tag()), VECTOR_CONFIRM);
        assert_eq!(hex(&broker_tag("Broker.Example.org ", 8883)), VECTOR_BROKER);
    }

    // Computed independently (Python hmac/hashlib, RFC 5869 HKDF).
    const VECTOR_ALIAS_11_22: &str = "rvj7jiog3f5gwurlezfyyxqwgheatl5u";
    const VECTOR_ALIAS_22_11: &str = "djomji4cxo5j6jdzzzzw2rgmibcav7e6";
    const VECTOR_CONFIRM: &str = "e585c0ef2043402981c8b785de1c5342";
    const VECTOR_BROKER: &str = "5434b66b64e2307f";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn directions_and_secrets_give_different_aliases() {
        let s = secret();
        let a = s.alias(1, 2);
        assert_eq!(a.len(), 32);
        assert!(a
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
        assert_ne!(a, s.alias(2, 1));
        assert_ne!(a, RelaySecret::random().alias(1, 2));
        assert_ne!(s.confirm_tag(), RelaySecret::random().confirm_tag());
    }

    #[test]
    fn inbox_names_are_restricted() {
        for ok in ["bob", "a", "fam.ily_1-x", &"x".repeat(64)] {
            assert!(validate_inbox(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a+",
            "#",
            "$SYS",
            "a b",
            "ä",
            "a\0",
            &"x".repeat(65),
        ] {
            assert_eq!(validate_inbox(bad), Err(RelayError::BadInbox), "{bad:?}");
        }
        assert!(secret().topic(1, 2, "a", "b/#").is_err());
        assert!(secret().topic(1, 2, "+", "b").is_err());
        let u = suggest_username();
        assert!(validate_inbox(&u).is_ok() && is_app_username(&u), "{u}");
        for bad in [
            "alice",
            "mg-",
            "mg-ABCDEFGHIJKLMNOPQRST",
            "mg-abcdefghijklmnopqrs1",
            "xx-abcdefghijklmnopqrst",
        ] {
            assert!(!is_app_username(bad), "{bad}");
        }
        assert_ne!(u, suggest_username());
    }

    #[test]
    fn broker_tag_ignores_case_and_spaces_but_not_port() {
        assert_eq!(
            broker_tag("a.example", 8883),
            broker_tag(" A.Example", 8883)
        );
        assert_ne!(broker_tag("a.example", 8883), broker_tag("a.example", 8884));
        assert_ne!(broker_tag("a.example", 8883), broker_tag("b.example", 8883));
    }

    #[test]
    fn bodies_round_trip_and_fit_the_v2_budget() {
        let offer = RelayOffer {
            secret: secret(),
            broker_tag: broker_tag("h", 8883),
            inbox: "x".repeat(MAX_INBOX_LEN),
        };
        let b = offer.encode().unwrap();
        assert!(b.len() <= 173, "{} bytes", b.len());
        assert_eq!(RelayOffer::decode(&b).unwrap(), offer);

        let accept = RelayAccept {
            confirm: secret().confirm_tag(),
            broker_tag: broker_tag("h", 8883),
            inbox: "x".repeat(MAX_INBOX_LEN),
        };
        assert!(accept.encode().unwrap().len() <= 173);
        assert_eq!(
            RelayAccept::decode(&accept.encode().unwrap()).unwrap(),
            accept
        );

        let revoke = RelayRevoke {
            confirm: secret().confirm_tag(),
        };
        assert_eq!(RelayRevoke::decode(&revoke.encode()).unwrap(), revoke);
    }

    #[test]
    fn malformed_bodies_are_rejected() {
        let good = RelayOffer {
            secret: secret(),
            broker_tag: [1; BROKER_TAG_LEN],
            inbox: "bob".into(),
        }
        .encode()
        .unwrap();
        // Truncated, trailing byte, wrong version, empty.
        assert!(RelayOffer::decode(&good[..good.len() - 1]).is_err());
        let mut long = good.clone();
        long.push(b'x');
        assert!(RelayOffer::decode(&long).is_err());
        let mut v = good.clone();
        v[0] = 9;
        assert_eq!(RelayOffer::decode(&v), Err(RelayError::Version(9)));
        assert!(RelayOffer::decode(&[]).is_err());
        // An inbox with a wildcard inside a well-formed body.
        let mut wild = good[..1 + SECRET_LEN + BROKER_TAG_LEN].to_vec();
        wild.extend_from_slice(&[1, b'#']);
        assert_eq!(RelayOffer::decode(&wild), Err(RelayError::BadInbox));
        assert!(RelayRevoke::decode(&[1; 5]).is_err());
        assert!(RelayAccept::decode(&[1; 25]).is_err());
    }

    #[test]
    fn confirm_comparison() {
        let t = secret().confirm_tag();
        assert!(confirm_matches(&t, &t));
        let mut u = t;
        u[15] ^= 1;
        assert!(!confirm_matches(&t, &u));
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let s = format!("{:?}", secret());
        assert!(!s.contains("01"), "{s}");
    }
}
