//! Mesh diagnostics: per-node signal history and path tests (traceroute).
//!
//! History is kept in memory only (it is metadata about other radios, and
//! only useful while connected): a bounded ring of samples per node.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Samples kept per node.
pub const HISTORY_LEN: usize = 240;
/// Nodes with history kept; the least recently heard are dropped first.
pub const HISTORY_NODES: usize = 256;
/// Firmware rate-limits traceroute; we allow one per node per this period.
pub const PATH_TEST_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// A path test without a reply is abandoned after this.
pub const PATH_TEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct SignalSample {
    /// Unix seconds (our clock).
    pub t: i64,
    /// Only for packets heard directly.
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub hops: Option<u32>,
    pub battery: Option<u32>,
    pub voltage: Option<f32>,
}

#[derive(Default)]
pub struct History {
    nodes: HashMap<u32, VecDeque<SignalSample>>,
}

impl History {
    pub fn record(&mut self, node: u32, sample: SignalSample) {
        if !self.nodes.contains_key(&node) && self.nodes.len() >= HISTORY_NODES {
            // Drop the node whose newest sample is oldest.
            if let Some(oldest) = self
                .nodes
                .iter()
                .min_by_key(|(_, s)| s.back().map(|x| x.t).unwrap_or(i64::MIN))
                .map(|(n, _)| *n)
            {
                self.nodes.remove(&oldest);
            }
        }
        let ring = self.nodes.entry(node).or_default();
        if ring.len() >= HISTORY_LEN {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    pub fn get(&self, node: u32) -> Vec<SignalSample> {
        self.nodes
            .get(&node)
            .map(|r| r.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
    }
}

/// Outstanding and recent path tests, per target node.
#[derive(Default)]
pub struct PathTests {
    started: HashMap<u32, Instant>,
}

impl PathTests {
    /// Register a new test unless one ran too recently. Returns the time to
    /// wait otherwise.
    pub fn try_start(&mut self, node: u32) -> Result<(), Duration> {
        let now = Instant::now();
        self.started.retain(|_, at| {
            now.duration_since(*at) < PATH_TEST_TIMEOUT.max(PATH_TEST_MIN_INTERVAL)
        });
        if let Some(at) = self.started.get(&node) {
            let since = now.duration_since(*at);
            if since < PATH_TEST_MIN_INTERVAL {
                return Err(PATH_TEST_MIN_INTERVAL - since);
            }
        }
        self.started.insert(node, now);
        Ok(())
    }

    /// Match a reply from `node`; returns the round-trip time if a test for
    /// it is outstanding (and closes it).
    pub fn finish(&mut self, node: u32) -> Option<Duration> {
        let at = self.started.get(&node).copied()?;
        let rtt = at.elapsed();
        if rtt > PATH_TEST_TIMEOUT {
            return None;
        }
        // Keep the entry for rate limiting, but mark it answered by moving
        // it back so a second reply is not reported twice.
        self.started.insert(
            node,
            Instant::now() - PATH_TEST_TIMEOUT - Duration::from_secs(1),
        );
        Some(rtt)
    }

    pub fn forget(&mut self, node: u32) {
        self.started.remove(&node);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathHop {
    pub node: u32,
    /// SNR (dB) with which this hop received the packet; None if unknown.
    pub snr: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathTestResult {
    pub node: u32,
    /// Hops from us to the target, ending with the target.
    pub towards: Vec<PathHop>,
    /// Hops from the target back to us, ending with us (if reported).
    pub back: Vec<PathHop>,
    pub rtt_ms: u64,
}

/// Meshtastic encodes SNR as dB x 4 in an int8; -128 means unknown.
fn snr_db(v: Option<&i32>) -> Option<f32> {
    match v {
        Some(&x) if x != i32::from(i8::MIN) => Some(x as f32 / 4.0),
        _ => None,
    }
}

/// Build the result from a RouteDiscovery reply sent by `target` to `me`.
pub fn path_result(
    me: u32,
    target: u32,
    route: &[u32],
    snr_towards: &[i32],
    route_back: &[u32],
    snr_back: &[i32],
    rtt: Duration,
) -> PathTestResult {
    let towards = route
        .iter()
        .copied()
        .chain(std::iter::once(target))
        .enumerate()
        .map(|(i, node)| PathHop {
            node,
            snr: snr_db(snr_towards.get(i)),
        })
        .collect();
    let back = if snr_back.is_empty() && route_back.is_empty() {
        Vec::new() // older firmware does not report the way back
    } else {
        route_back
            .iter()
            .copied()
            .chain(std::iter::once(me))
            .enumerate()
            .map(|(i, node)| PathHop {
                node,
                snr: snr_db(snr_back.get(i)),
            })
            .collect()
    };
    PathTestResult {
        node: target,
        towards,
        back,
        rtt_ms: rtt.as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: i64) -> SignalSample {
        SignalSample {
            t,
            rssi: Some(-80),
            snr: Some(5.0),
            hops: Some(0),
            battery: None,
            voltage: None,
        }
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::default();
        for t in 0..(HISTORY_LEN as i64 + 10) {
            h.record(1, sample(t));
        }
        let s = h.get(1);
        assert_eq!(s.len(), HISTORY_LEN);
        assert_eq!(s[0].t, 10);
        for n in 0..(HISTORY_NODES as u32 + 5) {
            h.record(1000 + n, sample(i64::from(n) + 1000));
        }
        assert!(h.nodes.len() <= HISTORY_NODES);
    }

    #[test]
    fn path_tests_are_rate_limited_and_answered_once() {
        let mut p = PathTests::default();
        assert!(p.try_start(7).is_ok());
        assert!(p.try_start(7).is_err());
        assert!(p.finish(7).is_some());
        assert!(p.finish(7).is_none(), "second reply ignored");
        assert!(p.finish(8).is_none(), "no test for 8");
    }

    #[test]
    fn route_includes_target_and_decodes_snr() {
        let r = path_result(
            1,
            9,
            &[5],
            &[24, i32::from(i8::MIN)],
            &[],
            &[],
            Duration::from_millis(1500),
        );
        assert_eq!(r.towards.len(), 2);
        assert_eq!(r.towards[0].node, 5);
        assert_eq!(r.towards[0].snr, Some(6.0));
        assert_eq!(r.towards[1].node, 9);
        assert_eq!(r.towards[1].snr, None);
        assert!(r.back.is_empty());
        let r = path_result(1, 9, &[], &[8], &[], &[-12], Duration::ZERO);
        assert_eq!(r.back.len(), 1);
        assert_eq!(r.back[0].node, 1);
        assert_eq!(r.back[0].snr, Some(-3.0));
    }
}
