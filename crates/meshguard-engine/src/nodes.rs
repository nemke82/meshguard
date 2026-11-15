//! Mesh nodes as the app shows them, kept up to date from what the radio
//! reports (NodeDB at connect time, then every packet heard).

use std::collections::HashMap;

use meshguard_core::status::{FixSource, GeoFix};
use meshtastic::protobufs;
use meshtastic::Message as _;

use crate::diagnostics::SignalSample;

/// Info about a node seen on the mesh network.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeshNodeInfo {
    pub node_num: u32,
    pub user_name: String,
    pub long_name: String,
    pub short_name: String,
    pub hw_model: String,
    /// SNR of the last packet heard from this node, in dB.
    pub snr: f32,
    /// RSSI of the last packet heard directly, in dBm (0 = unknown).
    pub rssi: i32,
    /// Unix seconds; refreshed by every packet we hear from the node.
    pub last_heard: i64,
    pub is_online: bool,
    /// Relay hops between us and the node (0 = heard directly).
    pub hops_away: Option<u32>,
    /// Battery percentage; above 100 means powered externally.
    pub battery_level: Option<u32>,
    pub voltage: Option<f32>,
    /// Channel utilisation seen by the node, in percent.
    pub channel_utilization: Option<f32>,
    /// The node's own transmit airtime over the last hour, in percent.
    pub air_util_tx: Option<f32>,
    /// Heard through an MQTT bridge rather than over the air.
    pub via_mqtt: bool,
}

/// A node counts as online when heard within this window (as in the
/// Meshtastic apps).
pub const ONLINE_WINDOW_SECS: i64 = 2 * 60 * 60;

/// A node from the radio's NodeDB. `now`: unix seconds.
pub fn from_node_info(ni: &protobufs::NodeInfo, now: i64) -> MeshNodeInfo {
    let (user_name, long_name, short_name, hw_model) = ni
        .user
        .as_ref()
        .map(|u| {
            (
                u.long_name.clone(),
                u.long_name.clone(),
                u.short_name.clone(),
                format!(
                    "{:?}",
                    protobufs::HardwareModel::try_from(u.hw_model)
                        .unwrap_or(protobufs::HardwareModel::Unset)
                ),
            )
        })
        .unwrap_or_default();

    let metrics = ni.device_metrics.as_ref();
    MeshNodeInfo {
        node_num: ni.num,
        user_name,
        long_name,
        short_name,
        hw_model,
        snr: ni.snr,
        rssi: 0,
        last_heard: ni.last_heard as i64,
        is_online: ni.last_heard > 0 && (now - ni.last_heard as i64) < ONLINE_WINDOW_SECS,
        hops_away: ni.hops_away,
        battery_level: metrics.and_then(|m| m.battery_level),
        voltage: metrics.and_then(|m| m.voltage),
        channel_utilization: metrics.and_then(|m| m.channel_utilization),
        air_util_tx: metrics.and_then(|m| m.air_util_tx),
        via_mqtt: ni.via_mqtt,
    }
}

/// Radio timestamps before this (2020-09-13) mean the clock was never set.
pub const MIN_VALID_UNIX_TIME: u32 = 1_600_000_000;

/// Convert a Meshtastic position into a fix with accuracy and fix time.
pub fn position_to_fix(pos: &protobufs::Position) -> Option<GeoFix> {
    let (lat, lon) = (pos.latitude_i?, pos.longitude_i?);
    if lat == 0 && lon == 0 {
        return None; // "no fix" placeholder
    }
    // GPS uncertainty: gps_accuracy is in mm, PDOP in 1/100 (as in the
    // Meshtastic apps).
    let gps = (pos.gps_accuracy > 0 && pos.pdop > 0)
        .then(|| f64::from(pos.gps_accuracy) / 1000.0 * f64::from(pos.pdop) / 100.0);
    // Deliberately reduced precision: one step of the coordinate grid.
    let grid = (1..32)
        .contains(&pos.precision_bits)
        .then(|| 2f64.powi(32 - pos.precision_bits as i32) * 1e-7 * 111_320.0);
    // A quantised position is never more accurate than its grid.
    let accuracy = match (gps, grid) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let source = match protobufs::position::LocSource::try_from(pos.location_source) {
        Ok(protobufs::position::LocSource::LocInternal) => FixSource::RadioGps,
        Ok(protobufs::position::LocSource::LocManual) => FixSource::Manual,
        _ => FixSource::Unknown,
    };
    let fix = GeoFix {
        lat_e7: lat,
        lon_e7: lon,
        accuracy_m: accuracy.map(|a| a.round().clamp(1.0, f64::from(u16::MAX - 1)) as u16),
        // `timestamp` is when the GPS solution was computed; `time` may be
        // when the packet was sent.
        fix_time: if pos.timestamp > 0 {
            pos.timestamp
        } else {
            pos.time
        },
        source,
    };
    fix.is_valid().then_some(fix)
}

/// The newer of the current fix and `pos` (packets can arrive out of order
/// after a reconnect).
pub fn newest_fix(current: Option<GeoFix>, pos: &protobufs::Position) -> Option<GeoFix> {
    match position_to_fix(pos) {
        Some(fix) if current.is_none_or(|c| fix.fix_time >= c.fix_time) => Some(fix),
        _ => current,
    }
}

/// A node we have only heard packets from, before any NodeInfo arrived.
pub fn unnamed_node(node_num: u32) -> MeshNodeInfo {
    MeshNodeInfo {
        node_num,
        user_name: String::new(),
        long_name: String::new(),
        short_name: String::new(),
        hw_model: String::new(),
        snr: 0.0,
        rssi: 0,
        last_heard: 0,
        is_online: false,
        hops_away: None,
        battery_level: None,
        voltage: None,
        channel_utilization: None,
        air_util_tx: None,
        via_mqtt: false,
    }
}

/// What one received packet means beyond its payload.
#[derive(Debug, Default)]
pub struct Observed {
    /// The node list changed; the UI should refresh it.
    pub nodes_changed: bool,
    /// Signal, hops or battery to record for the sender's diagnostics.
    pub sample: Option<SignalSample>,
    /// Our own radio reported its GPS position.
    pub my_position: Option<protobufs::Position>,
    /// A path test (traceroute) reply addressed to us.
    pub path_reply: Option<protobufs::Data>,
}

/// Refresh presence, signal and metadata of the sending node from any packet
/// we hear. The radio only pushes NodeInfo at connect time, so without this
/// a peer that is actively messaging us would still look offline, and a
/// chat request could arrive from a node whose name we do not know yet.
/// `now`: unix seconds.
pub fn observe(
    nodes: &mut HashMap<u32, MeshNodeInfo>,
    packet: &protobufs::MeshPacket,
    my_node_num: u32,
    now: i64,
) -> Observed {
    let mut seen = Observed::default();
    let decoded = match &packet.payload_variant {
        Some(protobufs::mesh_packet::PayloadVariant::Decoded(d)) => Some(d),
        _ => None,
    };
    let portnum = decoded.map(|d| d.portnum);

    if packet.from == my_node_num {
        // Our radio reports its own GPS position as POSITION_APP packets.
        if let Some(d) = decoded.filter(|_| portnum == Some(protobufs::PortNum::PositionApp as i32))
        {
            seen.my_position = protobufs::Position::decode(d.payload.as_slice()).ok();
        }
        return seen;
    }
    if packet.from == 0 {
        return seen;
    }
    if portnum == Some(protobufs::PortNum::TracerouteApp as i32) && packet.to == my_node_num {
        seen.path_reply = decoded.cloned();
    }

    // Add unknown nodes only when they announce a name or talk to MeshGuard,
    // so background traffic (telemetry, positions) does not flood the list.
    let adds_node = portnum == Some(protobufs::PortNum::NodeinfoApp as i32)
        || portnum == Some(protobufs::PortNum::PrivateApp as i32);
    if !nodes.contains_key(&packet.from) && !adds_node {
        return seen;
    }
    let node = nodes
        .entry(packet.from)
        .or_insert_with(|| unnamed_node(packet.from));

    // Prefer the radio's receive time: packets buffered while we were
    // disconnected must not look current. Fall back to our clock only when
    // the radio has no valid time (e.g. no GPS fix yet), and never move
    // last_heard backwards.
    let heard_at = if packet.rx_time > MIN_VALID_UNIX_TIME && i64::from(packet.rx_time) <= now + 60
    {
        i64::from(packet.rx_time)
    } else {
        now
    };
    node.last_heard = node.last_heard.max(heard_at);
    node.is_online = now - node.last_heard < ONLINE_WINDOW_SECS;
    node.via_mqtt = packet.via_mqtt;

    // Hop metadata is only trustworthy when hop_start is set (firmware 2.3+).
    let hops = (packet.hop_start > 0 && packet.hop_start >= packet.hop_limit)
        .then(|| packet.hop_start - packet.hop_limit);
    if let Some(h) = hops {
        node.hops_away = Some(h);
    }
    // Signal values describe the last hop, so they only describe the link to
    // this node when this very packet was received directly.
    let direct = hops == Some(0) && !packet.via_mqtt;
    let mut sample = SignalSample {
        t: heard_at,
        rssi: (direct && packet.rx_rssi != 0).then_some(packet.rx_rssi),
        snr: (direct && packet.rx_snr != 0.0).then_some(packet.rx_snr),
        hops,
        battery: None,
        voltage: None,
    };
    if direct {
        if packet.rx_snr != 0.0 {
            node.snr = packet.rx_snr;
        }
        if packet.rx_rssi != 0 {
            node.rssi = packet.rx_rssi;
        }
    }

    if let Some(d) = decoded {
        if d.portnum == protobufs::PortNum::NodeinfoApp as i32 {
            if let Ok(user) = protobufs::User::decode(d.payload.as_slice()) {
                if !user.long_name.is_empty() {
                    node.user_name = user.long_name.clone();
                    node.long_name = user.long_name;
                }
                if !user.short_name.is_empty() {
                    node.short_name = user.short_name;
                }
                if let Ok(hw) = protobufs::HardwareModel::try_from(user.hw_model) {
                    node.hw_model = format!("{hw:?}");
                }
            }
        } else if d.portnum == protobufs::PortNum::TelemetryApp as i32 {
            if let Ok(t) = protobufs::Telemetry::decode(d.payload.as_slice()) {
                if let Some(protobufs::telemetry::Variant::DeviceMetrics(m)) = t.variant {
                    sample.battery = m.battery_level;
                    sample.voltage = m.voltage;
                    node.battery_level = m.battery_level.or(node.battery_level);
                    node.voltage = m.voltage.or(node.voltage);
                    node.channel_utilization = m.channel_utilization.or(node.channel_utilization);
                    node.air_util_tx = m.air_util_tx.or(node.air_util_tx);
                }
            }
        }
    }
    if sample.rssi.is_some()
        || sample.snr.is_some()
        || sample.hops.is_some()
        || sample.battery.is_some()
    {
        seen.sample = Some(sample);
    }
    seen.nodes_changed = true;
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos() -> protobufs::Position {
        protobufs::Position {
            latitude_i: Some(448_123_400),
            longitude_i: Some(204_567_800),
            time: 1_763_100_000,
            ..Default::default()
        }
    }

    #[test]
    fn accuracy_from_gps_or_precision() {
        let mut p = pos();
        p.gps_accuracy = 3_000; // mm
        p.pdop = 250; // 2.5
        let f = position_to_fix(&p).unwrap();
        assert_eq!(f.accuracy_m, Some(8));
        assert_eq!(f.fix_time, 1_763_100_000);

        let mut p = pos();
        p.precision_bits = 13; // ~ 5.8 km grid in the Meshtastic apps
        let a = position_to_fix(&p).unwrap().accuracy_m.unwrap();
        assert!((5_000..6_000).contains(&a), "{a}");

        assert_eq!(position_to_fix(&pos()).unwrap().accuracy_m, None);
    }

    #[test]
    fn no_fix_is_ignored() {
        let mut p = pos();
        p.latitude_i = Some(0);
        p.longitude_i = Some(0);
        assert!(position_to_fix(&p).is_none());
        p.latitude_i = None;
        assert!(position_to_fix(&p).is_none());
    }

    const ME: u32 = 0x11;
    const NOW: i64 = 1_763_200_000;

    fn packet(from: u32, port: protobufs::PortNum, payload: Vec<u8>) -> protobufs::MeshPacket {
        protobufs::MeshPacket {
            from,
            to: ME,
            payload_variant: Some(protobufs::mesh_packet::PayloadVariant::Decoded(
                protobufs::Data {
                    portnum: port as i32,
                    payload,
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn background_traffic_from_unknown_nodes_is_not_listed() {
        let mut nodes = HashMap::new();
        let seen = observe(
            &mut nodes,
            &packet(0x22, protobufs::PortNum::TelemetryApp, vec![]),
            ME,
            NOW,
        );
        assert!(nodes.is_empty());
        assert!(!seen.nodes_changed);
    }

    #[test]
    fn node_info_adds_a_named_node() {
        use meshtastic::Message as _;
        let user = protobufs::User {
            long_name: "Alice".into(),
            short_name: "AL".into(),
            ..Default::default()
        };
        let mut nodes = HashMap::new();
        let p = packet(0x22, protobufs::PortNum::NodeinfoApp, user.encode_to_vec());
        let seen = observe(&mut nodes, &p, ME, NOW);
        assert!(seen.nodes_changed);
        let n = &nodes[&0x22];
        assert_eq!(
            (n.long_name.as_str(), n.short_name.as_str()),
            ("Alice", "AL")
        );
        assert_eq!(n.last_heard, NOW);
        assert!(n.is_online);
    }

    #[test]
    fn direct_packets_give_signal_relayed_ones_do_not() {
        let mut nodes = HashMap::new();
        let mut p = packet(0x22, protobufs::PortNum::PrivateApp, vec![1]);
        p.hop_start = 3;
        p.hop_limit = 3;
        p.rx_rssi = -90;
        p.rx_snr = 5.5;
        let s = observe(&mut nodes, &p, ME, NOW).sample.unwrap();
        assert_eq!((s.rssi, s.snr, s.hops), (Some(-90), Some(5.5), Some(0)));

        p.hop_limit = 1;
        let s = observe(&mut nodes, &p, ME, NOW).sample.unwrap();
        assert_eq!((s.rssi, s.snr, s.hops), (None, None, Some(2)));
        assert_eq!(nodes[&0x22].rssi, -90); // the direct value stays
    }

    #[test]
    fn buffered_packets_do_not_look_current() {
        let mut nodes = HashMap::new();
        let mut p = packet(0x22, protobufs::PortNum::PrivateApp, vec![1]);
        p.rx_time = (NOW - 3 * 3600) as u32;
        observe(&mut nodes, &p, ME, NOW);
        assert_eq!(nodes[&0x22].last_heard, NOW - 3 * 3600);
        assert!(!nodes[&0x22].is_online);
    }

    #[test]
    fn own_position_and_path_replies_are_reported() {
        use meshtastic::Message as _;
        let mut nodes = HashMap::new();
        let own = packet(ME, protobufs::PortNum::PositionApp, pos().encode_to_vec());
        let seen = observe(&mut nodes, &own, ME, NOW);
        assert!(seen.my_position.is_some());
        assert!(nodes.is_empty());

        let reply = packet(0x22, protobufs::PortNum::TracerouteApp, vec![]);
        assert!(observe(&mut nodes, &reply, ME, NOW).path_reply.is_some());
    }

    #[test]
    fn newest_fix_keeps_the_later_one() {
        let newer = position_to_fix(&pos()).unwrap();
        let mut old = pos();
        old.time -= 60;
        assert_eq!(newest_fix(Some(newer), &old), Some(newer));
        assert_eq!(newest_fix(None, &old).unwrap().fix_time, old.time);
    }
}
