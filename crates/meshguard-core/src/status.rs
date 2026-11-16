//! Family check-in, SOS and location messages (v2 type `Status`, 0x04).
//!
//! Body layout:
//!
//! ```text
//! kind u8 | flags u8 | [location, 15 bytes, if flags & HAS_LOCATION] | text (UTF-8)
//! location = lat_e7 i32 BE | lon_e7 i32 BE | accuracy_m u16 BE (0xFFFF = unknown)
//!          | fix_time u32 BE (unix seconds) | source u8
//! ```
//!
//! `text` is the sender's own wording (presets are user-editable), so the
//! receiver shows it as-is; `kind` drives icon and urgency. Unknown kinds are
//! shown as custom; unknown flag bits are rejected because they could change
//! the layout.

use crate::error::CoreError;

/// Feature bit: understands the `Status` message type.
pub const FEATURE_STATUS: u32 = 1 << 0;
/// Features this build supports (announced in `Caps`).
pub const SUPPORTED_FEATURES: u32 = FEATURE_STATUS | crate::relay::FEATURE_RELAY;

/// `Caps` flag: "please send your `Caps` back" (ADR-006 §4). Older builds
/// ignore the trailing byte.
pub const CAPS_REPLY: u8 = 0x01;

/// Encode a `Caps` body. Longer bodies are allowed for future fields.
pub fn encode_caps(features: u32) -> Vec<u8> {
    features.to_be_bytes().to_vec()
}

/// Encode a `Caps` body that asks the peer to announce its own features.
pub fn encode_caps_request(features: u32) -> Vec<u8> {
    let mut b = encode_caps(features);
    b.push(CAPS_REPLY);
    b
}

/// Decode a `Caps` body (extra trailing bytes are ignored).
pub fn decode_caps(body: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(body.get(..4)?.try_into().ok()?))
}

/// Whether a `Caps` body asks for the receiver's `Caps` in return.
pub fn caps_reply_requested(body: &[u8]) -> bool {
    body.get(4).is_some_and(|f| f & CAPS_REPLY != 0)
}

const HAS_LOCATION: u8 = 0x01;
const KNOWN_FLAGS: u8 = HAS_LOCATION;
const LOCATION_LEN: usize = 15;
const UNKNOWN_ACCURACY: u16 = u16::MAX;
/// Fixed part of the body before the text, with a location.
pub const STATUS_OVERHEAD_WITH_LOCATION: usize = 2 + LOCATION_LEN;
/// Fixed part of the body before the text, without a location.
pub const STATUS_OVERHEAD: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Custom,
    Ok,
    OnMyWay,
    NeedHelp,
    Sos,
    Location,
}

impl StatusKind {
    fn to_u8(self) -> u8 {
        match self {
            Self::Custom => 0,
            Self::Ok => 1,
            Self::OnMyWay => 2,
            Self::NeedHelp => 3,
            Self::Sos => 4,
            Self::Location => 5,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ok,
            2 => Self::OnMyWay,
            3 => Self::NeedHelp,
            4 => Self::Sos,
            5 => Self::Location,
            _ => Self::Custom,
        }
    }

    /// Urgent kinds are shown and notified prominently.
    pub fn is_urgent(self) -> bool {
        matches!(self, Self::NeedHelp | Self::Sos)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixSource {
    /// GPS of the Meshtastic radio.
    RadioGps,
    /// The phone's location service.
    Phone,
    /// Entered or fixed by the user.
    Manual,
    Unknown,
}

impl FixSource {
    fn to_u8(self) -> u8 {
        match self {
            Self::RadioGps => 0,
            Self::Phone => 1,
            Self::Manual => 2,
            Self::Unknown => 255,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::RadioGps,
            1 => Self::Phone,
            2 => Self::Manual,
            _ => Self::Unknown,
        }
    }
}

/// A position fix: where, how precise, and when it was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GeoFix {
    /// Latitude in 1e-7 degrees.
    pub lat_e7: i32,
    /// Longitude in 1e-7 degrees.
    pub lon_e7: i32,
    /// Estimated horizontal accuracy in metres, if known.
    pub accuracy_m: Option<u16>,
    /// When the fix was measured (unix seconds), not when it was sent.
    pub fix_time: u32,
    pub source: FixSource,
}

impl GeoFix {
    pub fn is_valid(&self) -> bool {
        (-900_000_000..=900_000_000).contains(&self.lat_e7)
            && (-1_800_000_000..=1_800_000_000).contains(&self.lon_e7)
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.lat_e7.to_be_bytes());
        out.extend_from_slice(&self.lon_e7.to_be_bytes());
        out.extend_from_slice(&self.accuracy_m.unwrap_or(UNKNOWN_ACCURACY).to_be_bytes());
        out.extend_from_slice(&self.fix_time.to_be_bytes());
        out.push(self.source.to_u8());
    }

    fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != LOCATION_LEN {
            return None;
        }
        let acc = u16::from_be_bytes([b[8], b[9]]);
        let fix = Self {
            lat_e7: i32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            lon_e7: i32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            accuracy_m: (acc != UNKNOWN_ACCURACY).then_some(acc),
            fix_time: u32::from_be_bytes([b[10], b[11], b[12], b[13]]),
            source: FixSource::from_u8(b[14]),
        };
        fix.is_valid().then_some(fix)
    }

    fn degrees(v: i32) -> f64 {
        f64::from(v) / 1e7
    }

    /// "44.81234, 20.45678 ±8 m" for plain-text fallbacks.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{:.5}, {:.5}",
            Self::degrees(self.lat_e7),
            Self::degrees(self.lon_e7)
        );
        if let Some(a) = self.accuracy_m {
            s.push_str(&format!(" ±{a} m"));
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusBody {
    pub kind: StatusKind,
    pub location: Option<GeoFix>,
    pub text: String,
}

impl StatusBody {
    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        let mut out = Vec::with_capacity(STATUS_OVERHEAD_WITH_LOCATION + self.text.len());
        out.push(self.kind.to_u8());
        match &self.location {
            Some(fix) => {
                if !fix.is_valid() {
                    return Err(CoreError::InvalidContent("location out of range".into()));
                }
                out.push(HAS_LOCATION);
                fix.encode_into(&mut out);
            }
            None => out.push(0),
        }
        out.extend_from_slice(self.text.as_bytes());
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&kind, rest) = b.split_first()?;
        let (&flags, rest) = rest.split_first()?;
        if flags & !KNOWN_FLAGS != 0 {
            return None;
        }
        let (location, text) = if flags & HAS_LOCATION != 0 {
            if rest.len() < LOCATION_LEN {
                return None;
            }
            let (loc, text) = rest.split_at(LOCATION_LEN);
            (Some(GeoFix::decode(loc)?), text)
        } else {
            (None, rest)
        };
        Some(Self {
            kind: StatusKind::from_u8(kind),
            location,
            text: std::str::from_utf8(text).ok()?.to_string(),
        })
    }

    /// [`Self::fallback_text`] cut to at most `max_bytes` UTF-8 bytes. The
    /// coordinates are kept whole; the user's text is shortened (with "…")
    /// on a character boundary.
    pub fn fallback_text_within(&self, max_bytes: usize) -> String {
        let full = self.fallback_text();
        if full.len() <= max_bytes {
            return full;
        }
        let suffix = full[self.text.len()..].to_string(); // " 📍 …" or ""
        const ELLIPSIS: &str = "…";
        let room = max_bytes.saturating_sub(suffix.len() + ELLIPSIS.len());
        let mut cut = room.min(self.text.len());
        while !self.text.is_char_boundary(cut) {
            cut -= 1;
        }
        let out = format!("{}{ELLIPSIS}{suffix}", &self.text[..cut]);
        if out.len() <= max_bytes {
            out
        } else {
            // Not even the coordinates fit: keep as much text as possible.
            let mut cut = max_bytes.min(self.text.len());
            while !self.text.is_char_boundary(cut) {
                cut -= 1;
            }
            self.text[..cut].to_string()
        }
    }

    /// Plain-text rendering for peers that only speak v1.
    pub fn fallback_text(&self) -> String {
        let mut s = self.text.clone();
        if let Some(fix) = &self.location {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str("\u{1F4CD} ");
            s.push_str(&fix.describe());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix() -> GeoFix {
        GeoFix {
            lat_e7: 448_123_400,
            lon_e7: 204_567_800,
            accuracy_m: Some(8),
            fix_time: 1_763_100_000,
            source: FixSource::RadioGps,
        }
    }

    #[test]
    fn roundtrip_with_and_without_location() {
        for location in [None, Some(fix())] {
            let s = StatusBody {
                kind: StatusKind::OnMyWay,
                location,
                text: "Krećem 🚶".into(),
            };
            let enc = s.encode().unwrap();
            let expected = if location.is_some() {
                STATUS_OVERHEAD_WITH_LOCATION
            } else {
                STATUS_OVERHEAD
            };
            assert_eq!(enc.len(), expected + s.text.len());
            assert_eq!(StatusBody::decode(&enc), Some(s));
        }
    }

    #[test]
    fn unknown_accuracy_and_kind() {
        let mut f = fix();
        f.accuracy_m = None;
        let s = StatusBody {
            kind: StatusKind::Sos,
            location: Some(f),
            text: String::new(),
        };
        let mut enc = s.encode().unwrap();
        assert_eq!(
            StatusBody::decode(&enc)
                .unwrap()
                .location
                .unwrap()
                .accuracy_m,
            None
        );
        enc[0] = 200;
        assert_eq!(StatusBody::decode(&enc).unwrap().kind, StatusKind::Custom);
    }

    #[test]
    fn rejects_bad_bodies() {
        assert!(StatusBody::decode(&[]).is_none());
        assert!(StatusBody::decode(&[1]).is_none());
        assert!(StatusBody::decode(&[1, 0x80]).is_none(), "unknown flag");
        assert!(
            StatusBody::decode(&[1, HAS_LOCATION, 0, 0]).is_none(),
            "short location"
        );
        assert!(
            StatusBody::decode(&[1, 0, 0xff, 0xfe]).is_none(),
            "bad utf-8"
        );
        let mut bad = fix();
        bad.lat_e7 = 950_000_000;
        let s = StatusBody {
            kind: StatusKind::Ok,
            location: Some(bad),
            text: String::new(),
        };
        assert!(s.encode().is_err());
    }

    #[test]
    fn fallback_is_cut_to_fit() {
        let s = StatusBody {
            kind: StatusKind::Sos,
            location: Some(fix()),
            text: "č".repeat(80), // 160 bytes
        };
        let out = s.fallback_text_within(173);
        assert!(out.len() <= 173, "{}", out.len());
        assert!(out.ends_with("±8 m"));
        assert!(out.contains('…'));
        let short = StatusBody {
            text: "OK".into(),
            ..s.clone()
        };
        assert_eq!(short.fallback_text_within(173), short.fallback_text());
        assert!(s.fallback_text_within(10).len() <= 10);
    }

    #[test]
    fn fallback_text_has_coordinates() {
        let s = StatusBody {
            kind: StatusKind::Sos,
            location: Some(fix()),
            text: "SOS".into(),
        };
        assert_eq!(s.fallback_text(), "SOS \u{1F4CD} 44.81234, 20.45678 ±8 m");
    }
}
