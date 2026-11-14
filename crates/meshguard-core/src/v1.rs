//! Legacy v1 wire payload: `[1 byte type][UTF-8 payload]`.
//!
//! The whole blob is encrypted with [`crate::crypto::SessionKey::encrypt`]
//! (nonce || AES-256-GCM ciphertext, no associated data). v1 carries **no
//! message id and no timestamp**, so receivers cannot deduplicate it; that is
//! the main reason for [`crate::envelope`] (v2). This module is kept so v2
//! builds can still read v1 peers during the transition.

use thiserror::Error;

pub const TYPE_TEXT: u8 = 0x01;
pub const TYPE_PAIR_REQUEST: u8 = 0x02;
pub const TYPE_PAIR_ACCEPT: u8 = 0x03;

/// Decoded v1 payload (no local metadata — that is added by the app).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V1Payload {
    Text(String),
    PairRequest(String),
    PairAccept(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum V1Error {
    #[error("empty wire message")]
    Empty,
    #[error("invalid UTF-8 payload")]
    InvalidUtf8,
    #[error("unknown message type: 0x{0:02x}")]
    UnknownType(u8),
}

impl V1Payload {
    /// Serialize to the compact v1 wire form (before encryption).
    pub fn to_wire(&self) -> Vec<u8> {
        let (ty, s) = match self {
            Self::Text(s) => (TYPE_TEXT, s),
            Self::PairRequest(s) => (TYPE_PAIR_REQUEST, s),
            Self::PairAccept(s) => (TYPE_PAIR_ACCEPT, s),
        };
        let mut buf = Vec::with_capacity(1 + s.len());
        buf.push(ty);
        buf.extend_from_slice(s.as_bytes());
        buf
    }

    /// Parse the compact v1 wire form (after decryption).
    pub fn from_wire(data: &[u8]) -> Result<Self, V1Error> {
        let (&ty, payload) = data.split_first().ok_or(V1Error::Empty)?;
        let s = std::str::from_utf8(payload)
            .map_err(|_| V1Error::InvalidUtf8)?
            .to_string();
        match ty {
            TYPE_TEXT => Ok(Self::Text(s)),
            TYPE_PAIR_REQUEST => Ok(Self::PairRequest(s)),
            TYPE_PAIR_ACCEPT => Ok(Self::PairAccept(s)),
            t => Err(V1Error::UnknownType(t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_layout_matches_v1_spec() {
        assert_eq!(V1Payload::Text("hi".into()).to_wire(), b"\x01hi");
        assert_eq!(V1Payload::PairRequest("A".into()).to_wire(), b"\x02A");
        assert_eq!(V1Payload::PairAccept("B".into()).to_wire(), b"\x03B");
    }

    #[test]
    fn roundtrip_all_types() {
        for p in [
            V1Payload::Text("Zdravo 👋 ćao".into()),
            V1Payload::PairRequest("Alice-Radio".into()),
            V1Payload::PairAccept(String::new()),
        ] {
            assert_eq!(V1Payload::from_wire(&p.to_wire()).unwrap(), p);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(V1Payload::from_wire(&[]), Err(V1Error::Empty));
        assert_eq!(
            V1Payload::from_wire(&[0x01, 0xff, 0xfe]),
            Err(V1Error::InvalidUtf8)
        );
        assert_eq!(
            V1Payload::from_wire(&[0x7f, b'x']),
            Err(V1Error::UnknownType(0x7f))
        );
    }
}
