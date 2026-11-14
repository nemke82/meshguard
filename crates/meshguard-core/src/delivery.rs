//! Truthful outbound delivery state, retry policy and per-message bookkeeping.
//!
//! Only an authenticated application ACK from the recipient ([`DeliveryState::PeerAck`])
//! or a later read receipt counts as delivered. Transport submission and a
//! Meshtastic-level ACK do not. All functions take time as an argument so
//! the logic is deterministic and testable with a virtual clock.

use thiserror::Error;

use crate::ids::{MessageId, TransportId};

/// Outbound message status. See the RFC's "Truthful status vocabulary".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryState {
    /// Created but not queued.
    Draft,
    /// Persisted locally, awaiting a transport.
    Queued,
    /// Accepted by a local transport (BLE radio, gateway). Not delivered.
    Submitted,
    /// Meshtastic-side acknowledgement. Not recipient-app confirmation.
    MeshAck,
    /// Authenticated MeshGuard ACK from the intended recipient app.
    PeerAck,
    /// Optional explicit read receipt.
    Read,
    /// Waiting to retry under a bounded policy.
    Retrying,
    /// Gave up after the bounded number of attempts.
    Failed,
    /// TTL exceeded before a peer ACK.
    Expired,
}

impl DeliveryState {
    /// The only states the UI may call "delivered".
    pub fn is_delivered(self) -> bool {
        matches!(self, Self::PeerAck | Self::Read)
    }

    /// No further send attempts will be made.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Self::PeerAck | Self::Read | Self::Failed | Self::Expired
        )
    }

    /// Stable lower-case name for storage and UI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Submitted => "submitted",
            Self::MeshAck => "mesh_ack",
            Self::PeerAck => "peer_ack",
            Self::Read => "read",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryEvent {
    Enqueue,
    Submitted,
    SubmitFailed,
    MeshAck,
    PeerAck,
    Read,
    AckTimeout,
    Expire,
    GiveUp,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("invalid delivery transition: {from:?} + {event:?}")]
pub struct InvalidTransition {
    pub from: DeliveryState,
    pub event: DeliveryEvent,
}

/// Apply an event to a state.
///
/// A late, authenticated `PeerAck` is accepted even after `Failed`/`Expired`:
/// it is proof of delivery, and reporting otherwise would be untruthful.
/// Duplicate ACKs and receipts are idempotent.
pub fn apply(
    from: DeliveryState,
    event: DeliveryEvent,
) -> Result<DeliveryState, InvalidTransition> {
    use DeliveryEvent as E;
    use DeliveryState as S;
    let to = match (from, event) {
        (S::Draft, E::Enqueue) => S::Queued,

        (S::Queued | S::Retrying, E::Submitted) => S::Submitted,
        (S::Queued | S::Retrying, E::SubmitFailed) => S::Retrying,

        (S::Submitted | S::MeshAck, E::MeshAck) => S::MeshAck,
        (S::Submitted | S::MeshAck, E::AckTimeout) => S::Retrying,

        (S::Queued | S::Submitted | S::MeshAck | S::Retrying, E::PeerAck) => S::PeerAck,
        (S::Failed | S::Expired, E::PeerAck) => S::PeerAck,
        (S::PeerAck, E::PeerAck) => S::PeerAck,
        (S::Read, E::PeerAck | E::Read) => S::Read,
        (S::PeerAck, E::Read) => S::Read,

        (S::Queued | S::Submitted | S::MeshAck | S::Retrying, E::Expire) => S::Expired,
        (S::Queued | S::Submitted | S::MeshAck | S::Retrying, E::GiveUp) => S::Failed,

        _ => return Err(InvalidTransition { from, event }),
    };
    Ok(to)
}

/// Bounded exponential backoff with jitter.
///
/// Defaults are conservative because a shared LoRa mesh has very little
/// airtime; a retry storm hurts every user of the channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub base_delay_secs: u64,
    pub max_delay_secs: u64,
    /// Total transmission attempts, including the first.
    pub max_attempts: u32,
    /// Fraction in `[0, 1]` by which a delay may be shortened at random.
    pub jitter_ratio: f64,
    /// How long to wait for a peer ACK after a successful submission.
    pub ack_timeout_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_delay_secs: 30,
            max_delay_secs: 600,
            max_attempts: 5,
            jitter_ratio: 0.3,
            ack_timeout_secs: 60,
        }
    }
}

impl RetryPolicy {
    /// Delay before the attempt following attempt number `attempts` (1-based).
    ///
    /// `jitter_unit` must be a uniform random value in `[0, 1)`; passing it in
    /// keeps this function deterministic for tests.
    pub fn delay_after(&self, attempts: u32, jitter_unit: f64) -> u64 {
        let exp = attempts.saturating_sub(1).min(32);
        let raw = self
            .base_delay_secs
            .saturating_mul(1u64 << exp)
            .min(self.max_delay_secs);
        let u = jitter_unit.clamp(0.0, 1.0);
        let r = self.jitter_ratio.clamp(0.0, 1.0);
        let cut = (raw as f64 * r * u) as u64;
        raw.saturating_sub(cut).max(1)
    }
}

/// What the scheduler should do with an outbound message right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueAction {
    /// (Re)transmit, reusing the same message id with a fresh nonce.
    Send,
    /// No ACK arrived in time.
    AckTimedOut,
    /// TTL passed without delivery.
    Expire,
}

/// Bookkeeping for one outbound message. Persisted by the outbox owner.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundMessage {
    pub message_id: MessageId,
    pub state: DeliveryState,
    pub attempts: u32,
    pub expires_at: u64,
    /// Next send time (Queued/Retrying) or ACK deadline (Submitted/MeshAck).
    pub next_deadline: u64,
    pub last_transport: Option<TransportId>,
}

impl OutboundMessage {
    /// A message persisted to the outbox and eligible to send at `now`.
    pub fn queued(message_id: MessageId, now: u64, expires_at: u64) -> Self {
        Self {
            message_id,
            state: DeliveryState::Queued,
            attempts: 0,
            expires_at,
            next_deadline: now,
            last_transport: None,
        }
    }

    pub fn due_action(&self, now: u64) -> Option<DueAction> {
        if self.state.is_settled() {
            return None;
        }
        if now >= self.expires_at {
            return Some(DueAction::Expire);
        }
        if now < self.next_deadline {
            return None;
        }
        match self.state {
            DeliveryState::Queued | DeliveryState::Retrying => Some(DueAction::Send),
            DeliveryState::Submitted | DeliveryState::MeshAck => Some(DueAction::AckTimedOut),
            _ => None,
        }
    }

    fn step(&mut self, event: DeliveryEvent) -> Result<(), InvalidTransition> {
        self.state = apply(self.state, event)?;
        Ok(())
    }

    pub fn record_submitted(
        &mut self,
        now: u64,
        transport: TransportId,
        policy: &RetryPolicy,
    ) -> Result<(), InvalidTransition> {
        self.step(DeliveryEvent::Submitted)?;
        self.attempts += 1;
        self.next_deadline = now.saturating_add(policy.ack_timeout_secs);
        self.last_transport = Some(transport);
        Ok(())
    }

    pub fn record_submit_failed(
        &mut self,
        now: u64,
        policy: &RetryPolicy,
        jitter_unit: f64,
    ) -> Result<(), InvalidTransition> {
        self.attempts += 1;
        self.retry_or_give_up(DeliveryEvent::SubmitFailed, now, policy, jitter_unit)
    }

    pub fn record_ack_timeout(
        &mut self,
        now: u64,
        policy: &RetryPolicy,
        jitter_unit: f64,
    ) -> Result<(), InvalidTransition> {
        self.retry_or_give_up(DeliveryEvent::AckTimeout, now, policy, jitter_unit)
    }

    fn retry_or_give_up(
        &mut self,
        event: DeliveryEvent,
        now: u64,
        policy: &RetryPolicy,
        jitter_unit: f64,
    ) -> Result<(), InvalidTransition> {
        if self.attempts >= policy.max_attempts {
            return self.step(DeliveryEvent::GiveUp);
        }
        self.step(event)?;
        self.next_deadline = now.saturating_add(policy.delay_after(self.attempts, jitter_unit));
        Ok(())
    }

    pub fn record_mesh_ack(&mut self) -> Result<(), InvalidTransition> {
        self.step(DeliveryEvent::MeshAck)
    }

    pub fn record_peer_ack(&mut self) -> Result<(), InvalidTransition> {
        self.step(DeliveryEvent::PeerAck)
    }

    pub fn expire(&mut self) -> Result<(), InvalidTransition> {
        self.step(DeliveryEvent::Expire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeliveryEvent as E;
    use DeliveryState as S;

    const ALL_STATES: [S; 9] = [
        S::Draft,
        S::Queued,
        S::Submitted,
        S::MeshAck,
        S::PeerAck,
        S::Read,
        S::Retrying,
        S::Failed,
        S::Expired,
    ];
    const ALL_EVENTS: [E; 9] = [
        E::Enqueue,
        E::Submitted,
        E::SubmitFailed,
        E::MeshAck,
        E::PeerAck,
        E::Read,
        E::AckTimeout,
        E::Expire,
        E::GiveUp,
    ];

    #[test]
    fn only_peer_ack_and_read_are_delivered() {
        for s in ALL_STATES {
            assert_eq!(s.is_delivered(), matches!(s, S::PeerAck | S::Read), "{s:?}");
        }
    }

    #[test]
    fn no_transition_reaches_delivered_without_peer_ack_or_read() {
        for s in ALL_STATES {
            for e in ALL_EVENTS {
                if let Ok(to) = apply(s, e) {
                    if to.is_delivered() && !s.is_delivered() {
                        assert_eq!(e, E::PeerAck, "{s:?} + {e:?} -> {to:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn mesh_ack_is_not_delivery() {
        let s = apply(S::Submitted, E::MeshAck).unwrap();
        assert_eq!(s, S::MeshAck);
        assert!(!s.is_delivered());
    }

    #[test]
    fn happy_path() {
        let mut s = S::Draft;
        for e in [E::Enqueue, E::Submitted, E::MeshAck, E::PeerAck, E::Read] {
            s = apply(s, e).unwrap();
        }
        assert_eq!(s, S::Read);
    }

    #[test]
    fn late_peer_ack_overrides_failure() {
        assert_eq!(apply(S::Failed, E::PeerAck), Ok(S::PeerAck));
        assert_eq!(apply(S::Expired, E::PeerAck), Ok(S::PeerAck));
    }

    #[test]
    fn settled_states_reject_resend() {
        for s in [S::PeerAck, S::Read, S::Failed, S::Expired] {
            assert!(apply(s, E::Submitted).is_err());
            assert!(apply(s, E::SubmitFailed).is_err());
        }
    }

    #[test]
    fn backoff_grows_caps_and_jitters_down() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_after(1, 0.0), 30);
        assert_eq!(p.delay_after(2, 0.0), 60);
        assert_eq!(p.delay_after(3, 0.0), 120);
        assert_eq!(p.delay_after(10, 0.0), 600);
        assert_eq!(p.delay_after(u32::MAX, 0.0), 600);
        assert_eq!(p.delay_after(1, 0.999_999), 22);
        assert!(p.delay_after(1, 0.5) < 30);
    }

    #[test]
    fn outbound_retries_are_bounded() {
        let p = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        };
        let mut m = OutboundMessage::queued(MessageId::random(), 0, 100_000);
        let mut now = 0;
        let mut sends = 0;
        while let Some(action) = m.due_action(now) {
            match action {
                DueAction::Send => {
                    sends += 1;
                    m.record_submitted(now, TransportId::new("mock"), &p)
                        .unwrap();
                }
                DueAction::AckTimedOut => m.record_ack_timeout(now, &p, 0.0).unwrap(),
                DueAction::Expire => m.expire().unwrap(),
            }
            now = m.next_deadline.max(now);
            if m.state.is_settled() {
                break;
            }
        }
        assert_eq!(sends, 3);
        assert_eq!(m.state, S::Failed);
        assert!(!m.state.is_delivered());
    }

    #[test]
    fn submit_failure_retries_then_succeeds() {
        let p = RetryPolicy::default();
        let mut m = OutboundMessage::queued(MessageId::random(), 0, 10_000);
        assert_eq!(m.due_action(0), Some(DueAction::Send));
        m.record_submit_failed(0, &p, 0.0).unwrap();
        assert_eq!(m.state, S::Retrying);
        assert_eq!(m.due_action(29), None);
        assert_eq!(m.due_action(30), Some(DueAction::Send));
        m.record_submitted(30, TransportId::new("mock"), &p)
            .unwrap();
        m.record_peer_ack().unwrap();
        assert!(m.state.is_delivered());
        assert_eq!(m.due_action(10_000_000), None);
    }

    #[test]
    fn ttl_expiry_wins_over_pending_retry() {
        let p = RetryPolicy::default();
        let mut m = OutboundMessage::queued(MessageId::random(), 0, 50);
        m.record_submitted(0, TransportId::new("mock"), &p).unwrap();
        assert_eq!(m.due_action(50), Some(DueAction::Expire));
        m.expire().unwrap();
        assert_eq!(m.state, S::Expired);
    }
}
