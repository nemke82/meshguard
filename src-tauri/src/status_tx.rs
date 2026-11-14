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

pub struct StatusTx {
    last_sent: Option<Instant>,
    /// Burst of 24, then one transmission per 15 s.
    budget: TokenBucket,
}

impl Default for StatusTx {
    fn default() -> Self {
        Self {
            last_sent: None,
            budget: TokenBucket::new(24, 15),
        }
    }
}

impl StatusTx {
    /// Wait for the spacing, then take budget. Returns false (and takes
    /// nothing) when the budget is exhausted and `urgent` is false.
    pub async fn admit(&mut self, urgent: bool, now_unix: u64) -> bool {
        let took = self.budget.try_take(now_unix);
        if !took && !urgent {
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

/// Send a feature probe to v2 peers whose support for check-in/SOS messages
/// is not known yet. An ACK marks them capable (handled by the listener);
/// older builds drop the probe silently and keep getting the text fallback.
pub async fn probe_peers(state: &AppState) {
    let Some(my_num) = *state.my_node_num.lock().await else {
        return;
    };
    let candidates: Vec<u32> = state
        .config
        .lock()
        .await
        .peers
        .iter()
        .filter(|p| {
            p.confirmed && p.protocol == WireProtocol::V2 && p.features & FEATURE_STATUS == 0
        })
        .map(|p| p.node_num)
        .collect();
    for peer in candidates {
        let sealed = {
            let keys = state.session_keys.lock().await;
            let Some(key) = keys.get(&peer) else {
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
        let mut tx = state.status_tx.lock().await;
        if !tx
            .admit(false, u64::from(crate::commands::unix_now()))
            .await
        {
            tracing::debug!("Feature probes paused: airtime budget");
            return;
        }
        state.inbound.caps_probes.lock().await.insert(id, peer);
        match crate::commands::send_as(state, my_num, peer, sealed.frame).await {
            Ok(()) => {
                tx.mark_sent();
                tracing::info!("Probed node {peer} for check-in support");
            }
            Err(e) => {
                state.inbound.caps_probes.lock().await.remove(&id);
                tracing::debug!("Probe to {peer} not sent: {e}");
            }
        }
    }
}
