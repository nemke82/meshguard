//! Shared transmit path for check-ins, SOS and feature probes.
//!
//! All of them go through one lock with a minimum spacing between
//! transmissions and a token-bucket airtime budget, so concurrent requests
//! cannot flood the radio's queue or the shared LoRa channel. Urgent
//! messages (SOS, need help) are never refused by the budget, only spaced.

use std::time::{Duration, Instant};

use meshguard_core::envelope::DEFAULT_FRAME_BUDGET;
use meshguard_core::outbound;
use meshguard_core::ratelimit::TokenBucket;
use meshguard_core::status::{FEATURE_STATUS, SUPPORTED_FEATURES};
use meshguard_core::PeerIdentity;

use crate::device_config::WireProtocol;
use crate::state::AppState;

/// Minimum gap between two transmissions from this path.
pub const SPACING: Duration = Duration::from_millis(400);

/// How long an urgent transmission may wait for its budget.
const URGENT_MAX_WAIT: Duration = Duration::from_secs(180);
/// A probe without an ACK is forgotten after this, and the peer may be
/// probed again (older builds never answer).
pub const PROBE_EXPIRY_SECS: u64 = 60 * 60;

pub struct StatusTx {
    last_sent: Option<Instant>,
    /// Check-ins and probes: burst of 24, then one per 15 s.
    budget: TokenBucket,
    /// SOS / need help: enough for one SOS to many contacts, then one per
    /// 5 s. Separate so routine traffic can never block an SOS, and bounded
    /// so repeated SOS taps cannot flood the channel either.
    urgent_budget: TokenBucket,
}

impl Default for StatusTx {
    fn default() -> Self {
        Self {
            last_sent: None,
            budget: TokenBucket::new(24, 15),
            urgent_budget: TokenBucket::new(64, 5),
        }
    }
}

impl StatusTx {
    /// Wait for the spacing and take budget. Routine traffic is refused
    /// (false) when its budget is empty; urgent traffic waits for its own
    /// budget, up to [`URGENT_MAX_WAIT`].
    pub async fn admit(&mut self, urgent: bool) -> bool {
        let now = || chrono::Utc::now().timestamp().max(0) as u64;
        if urgent {
            let deadline = Instant::now() + URGENT_MAX_WAIT;
            while !self.urgent_budget.try_take(now()) {
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        } else if !self.budget.try_take(now()) {
            return false;
        }
        if let Some(last) = self.last_sent {
            let elapsed = last.elapsed();
            if elapsed < SPACING {
                tokio::time::sleep(SPACING - elapsed).await;
            }
        }
        true
    }

    pub fn mark_sent(&mut self) {
        self.last_sent = Some(Instant::now());
    }
}

/// Answer a peer's request for our features with our `Caps` (rate-limited
/// by the engine). Best effort: not retried.
pub async fn announce_caps(state: &AppState, peer: u32) {
    let Some(my_num) = state.engine().my_node_num else {
        return;
    };
    let frame = {
        let engine = state.engine();
        if engine.keys_owner != Some(my_num) {
            return;
        }
        let Some(key) = engine.keys.get(&peer) else {
            return;
        };
        match outbound::seal_caps(
            SUPPORTED_FEATURES,
            key,
            &PeerIdentity::meshtastic_node(my_num),
            &PeerIdentity::meshtastic_node(peer),
            crate::commands::unix_now(),
            DEFAULT_FRAME_BUDGET,
        ) {
            Ok(s) => s.frame,
            Err(_) => return,
        }
    };
    let _tx = state.status_tx.lock().await;
    if let Err(e) = crate::commands::send_as(state, my_num, peer, frame).await {
        tracing::info!("Caps answer to {peer} not sent: {e}");
    }
}

/// Send a feature probe to v2 peers whose support for check-in/SOS messages
/// is not known yet. An ACK marks them capable (handled by the listener);
/// older builds drop the probe silently and keep getting the text fallback.
pub async fn probe_peers(state: &AppState) {
    let Some(my_num) = state.engine().my_node_num else {
        return;
    };
    let candidates: Vec<u32> = state
        .engine()
        .config
        .peers
        .iter()
        .filter(|p| {
            p.confirmed && p.protocol == WireProtocol::V2 && p.features & FEATURE_STATUS == 0
        })
        .map(|p| p.node_num)
        .collect();
    for peer in candidates {
        let sealed = {
            let engine = state.engine();
            if engine.keys_owner != Some(my_num) {
                return;
            }
            let Some(key) = engine.keys.get(&peer) else {
                continue;
            };
            match outbound::seal_caps(
                SUPPORTED_FEATURES,
                key,
                &PeerIdentity::meshtastic_node(my_num),
                &PeerIdentity::meshtastic_node(peer),
                crate::commands::unix_now(),
                DEFAULT_FRAME_BUDGET,
            ) {
                Ok(s) => s,
                Err(_) => continue,
            }
        };
        let Some(id) = sealed.message_id else {
            continue;
        };
        // Serialise with other probes/check-ins first, then check for an
        // outstanding probe, so concurrent callers cannot both send one.
        let mut tx = state.status_tx.lock().await;
        let now = u64::from(crate::commands::unix_now());
        {
            // One outstanding probe per peer; expired ones may be retried.
            let mut engine = state.engine();
            let probes = &mut engine.protection.caps_probes;
            probes.retain(|_, (_, at)| now.saturating_sub(*at) < PROBE_EXPIRY_SECS);
            if probes.contains_key(&peer) {
                continue;
            }
        }
        if !tx.admit(false).await {
            tracing::debug!("Feature probes paused: airtime budget");
            return;
        }
        state
            .engine()
            .protection
            .caps_probes
            .insert(peer, (id, now));
        match crate::commands::send_as(state, my_num, peer, sealed.frame).await {
            Ok(()) => {
                tx.mark_sent();
                tracing::info!("Probed node {peer} for check-in support");
            }
            Err(e) => {
                state.engine().protection.caps_probes.remove(&peer);
                tracing::debug!("Probe to {peer} not sent: {e}");
            }
        }
    }
}
