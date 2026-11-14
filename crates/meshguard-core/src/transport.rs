//! Transport contract: adapters move **opaque encrypted frames**.
//!
//! A transport does not own identity, message ids, text, application ACKs or
//! history. A successful [`Transport::send`] is only a *submission*; it never
//! means the recipient got the message.
//!
//! The trait is synchronous and poll-based (instead of the RFC's
//! callback-style `subscribe`) so it stays free of any async runtime and is
//! easy to drive from tests, tokio, or an Android service thread.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::ids::TransportId;

/// Transport-level destination or source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransportAddress {
    /// Meshtastic node number (the `to`/`from` of a `MeshPacket`).
    MeshtasticNode(u32),
    /// Anything else (future gateway, LAN peer), opaque to the engine.
    Opaque(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub can_send: bool,
    pub can_receive: bool,
    /// Largest frame the transport accepts, in bytes.
    pub mtu: usize,
    pub metered: bool,
    /// Whether the transport reports its own (non-app) delivery ACKs.
    pub supports_mesh_ack: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ready,
    Degraded(String),
    Disconnected(String),
}

/// Proof that a local transport accepted a frame. Not a delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionReceipt {
    pub transport: TransportId,
    /// Transport-local reference, e.g. a Meshtastic packet id, used to match
    /// a later mesh-level ACK.
    pub local_ref: Option<u32>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("transport unavailable: {0}")]
    Unavailable(String),
    #[error("frame of {len} bytes exceeds transport MTU {mtu}")]
    TooLarge { len: usize, mtu: usize },
    #[error("transport rejected frame: {0}")]
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingFrame {
    pub transport: TransportId,
    pub from: TransportAddress,
    pub frame: Vec<u8>,
}

pub trait Transport {
    fn id(&self) -> TransportId;
    fn capabilities(&self) -> Capabilities;
    fn health(&self) -> Health;
    fn start(&mut self) -> Result<(), TransportError>;
    fn stop(&mut self);
    fn send(
        &mut self,
        to: &TransportAddress,
        frame: &[u8],
    ) -> Result<SubmissionReceipt, TransportError>;
    /// Drain frames received since the last call.
    fn poll_incoming(&mut self) -> Vec<IncomingFrame>;
}

// ── Mock ─────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct MediumInner {
    inboxes: HashMap<TransportAddress, VecDeque<(TransportAddress, Vec<u8>)>>,
    down: HashMap<TransportAddress, bool>,
    drop_next: usize,
    duplicate_next: usize,
    delivered: usize,
}

/// Shared, deterministic in-memory medium with fault injection.
///
/// Clone it to hand the same medium to several [`MockTransport`]s.
#[derive(Debug, Clone, Default)]
pub struct MockMedium(Arc<Mutex<MediumInner>>);

impl MockMedium {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MediumInner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Silently lose the next `n` submitted frames (like LoRa loss).
    pub fn drop_next(&self, n: usize) {
        self.lock().drop_next = n;
    }

    /// Deliver each of the next `n` frames twice (like a mesh relay echo).
    pub fn duplicate_next(&self, n: usize) {
        self.lock().duplicate_next = n;
    }

    /// Simulate the local link of `addr` going down (e.g. BLE off).
    pub fn set_down(&self, addr: &TransportAddress, down: bool) {
        self.lock().down.insert(addr.clone(), down);
    }

    /// Frames that actually reached an inbox (counts duplicates).
    pub fn delivered_count(&self) -> usize {
        self.lock().delivered
    }
}

pub struct MockTransport {
    id: TransportId,
    address: TransportAddress,
    medium: MockMedium,
    mtu: usize,
    started: bool,
    next_ref: u32,
    /// Every frame this endpoint submitted, for airtime assertions.
    pub sent: Vec<(TransportAddress, Vec<u8>)>,
}

impl MockTransport {
    pub fn new(id: &str, address: TransportAddress, medium: MockMedium, mtu: usize) -> Self {
        medium.lock().inboxes.entry(address.clone()).or_default();
        Self {
            id: TransportId::new(id),
            address,
            medium,
            mtu,
            started: false,
            next_ref: 1,
            sent: Vec::new(),
        }
    }

    pub fn address(&self) -> &TransportAddress {
        &self.address
    }

    fn is_down(&self) -> bool {
        self.medium
            .lock()
            .down
            .get(&self.address)
            .copied()
            .unwrap_or(false)
    }
}

impl Transport for MockTransport {
    fn id(&self) -> TransportId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_send: true,
            can_receive: true,
            mtu: self.mtu,
            metered: false,
            supports_mesh_ack: false,
        }
    }

    fn health(&self) -> Health {
        if !self.started {
            Health::Disconnected("stopped".into())
        } else if self.is_down() {
            Health::Disconnected("link down".into())
        } else {
            Health::Ready
        }
    }

    fn start(&mut self) -> Result<(), TransportError> {
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) {
        self.started = false;
    }

    fn send(
        &mut self,
        to: &TransportAddress,
        frame: &[u8],
    ) -> Result<SubmissionReceipt, TransportError> {
        if self.health() != Health::Ready {
            return Err(TransportError::Unavailable("link down".into()));
        }
        if frame.len() > self.mtu {
            return Err(TransportError::TooLarge {
                len: frame.len(),
                mtu: self.mtu,
            });
        }
        self.sent.push((to.clone(), frame.to_vec()));
        let mut m = self.medium.lock();
        let copies = if m.drop_next > 0 {
            m.drop_next -= 1;
            0
        } else if m.duplicate_next > 0 {
            m.duplicate_next -= 1;
            2
        } else {
            1
        };
        if let Some(inbox) = m.inboxes.get_mut(to) {
            for _ in 0..copies {
                inbox.push_back((self.address.clone(), frame.to_vec()));
            }
            m.delivered += copies;
        }
        drop(m);
        let local_ref = self.next_ref;
        self.next_ref = self.next_ref.wrapping_add(1);
        Ok(SubmissionReceipt {
            transport: self.id.clone(),
            local_ref: Some(local_ref),
        })
    }

    fn poll_incoming(&mut self) -> Vec<IncomingFrame> {
        if !self.started || self.is_down() {
            return Vec::new();
        }
        let mut m = self.medium.lock();
        let inbox = m.inboxes.entry(self.address.clone()).or_default();
        inbox
            .drain(..)
            .map(|(from, frame)| IncomingFrame {
                transport: self.id.clone(),
                from,
                frame,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (MockMedium, MockTransport, MockTransport) {
        let m = MockMedium::new();
        let mut a = MockTransport::new(
            "mock-a",
            TransportAddress::MeshtasticNode(1),
            m.clone(),
            228,
        );
        let mut b = MockTransport::new(
            "mock-b",
            TransportAddress::MeshtasticNode(2),
            m.clone(),
            228,
        );
        a.start().unwrap();
        b.start().unwrap();
        (m, a, b)
    }

    #[test]
    fn delivers_opaque_frames() {
        let (_, mut a, mut b) = pair();
        let to = b.address().clone();
        let r = a.send(&to, b"frame").unwrap();
        assert_eq!(r.transport, TransportId::new("mock-a"));
        let got = b.poll_incoming();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].frame, b"frame");
        assert_eq!(got[0].from, TransportAddress::MeshtasticNode(1));
        assert!(b.poll_incoming().is_empty());
    }

    #[test]
    fn faults_drop_duplicate_down_and_mtu() {
        let (m, mut a, mut b) = pair();
        let to = TransportAddress::MeshtasticNode(2);
        m.drop_next(1);
        a.send(&to, b"lost").unwrap();
        assert!(b.poll_incoming().is_empty());
        m.duplicate_next(1);
        a.send(&to, b"echo").unwrap();
        assert_eq!(b.poll_incoming().len(), 2);
        m.set_down(&TransportAddress::MeshtasticNode(1), true);
        assert!(matches!(
            a.send(&to, b"x"),
            Err(TransportError::Unavailable(_))
        ));
        m.set_down(&TransportAddress::MeshtasticNode(1), false);
        assert!(matches!(
            a.send(&to, &[0u8; 229]),
            Err(TransportError::TooLarge { len: 229, mtu: 228 })
        ));
    }
}
