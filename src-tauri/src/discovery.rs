//! Finding Meshtastic radios on the local Wi-Fi network.
//!
//! Wi-Fi capable Meshtastic firmware (ESP32 boards, meshtasticd) announces
//! its TCP API over mDNS as `_meshtastic._tcp` with TXT records `id`
//! (`!a1b2c3d4`) and `shortname`. Desktop browses with `mdns-sd`; Android
//! asks the system `NsdManager` (via the Kotlin plugin), because Android
//! drops multicast for app sockets unless the app holds a multicast lock.
//!
//! We deliberately do not sweep the subnet for port 4403: the firmware
//! serves one TCP client at a time, so a probe could cut off another phone.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::MeshGuardError;

pub const SERVICE_TYPE: &str = "_meshtastic._tcp.local.";
pub const DEFAULT_PORT: u16 = 4403;
pub const SCAN_TIME: Duration = Duration::from_secs(4);

/// A radio found on the network, ready for `connect_tcp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WifiRadio {
    /// mDNS instance name, e.g. `Meshtastic_b2c4`.
    pub name: String,
    /// `host:port` to connect to.
    pub address: String,
    pub node_num: Option<u32>,
    pub short_name: Option<String>,
}

/// One answer as the platform reports it, before normalising.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawService {
    pub name: String,
    /// IPv4 preferred; a global IPv6 address is bracketed when building
    /// the address.
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub shortname: Option<String>,
}

/// `!a1b2c3d4` → `0xa1b2c3d4`.
pub fn parse_node_id(id: &str) -> Option<u32> {
    let hex = id.trim().strip_prefix('!')?;
    if hex.is_empty() || hex.len() > 8 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// The service name without the `._meshtastic._tcp.local.` suffix.
fn instance_name(full: &str) -> &str {
    full.split("._meshtastic.").next().unwrap_or(full)
}

pub fn normalise(raw: Vec<RawService>) -> Vec<WifiRadio> {
    let mut out: Vec<WifiRadio> = Vec::new();
    for r in raw {
        if r.host.is_empty() {
            continue;
        }
        let port = if r.port == 0 { DEFAULT_PORT } else { r.port };
        // A link-local IPv6 address needs its interface scope to be dialed,
        // which a plain `host:port` cannot carry; firmware also announces
        // IPv4, so skip these.
        let host = r.host.as_str();
        if host.contains('%') || host.to_ascii_lowercase().starts_with("fe80:") {
            continue;
        }
        let address = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let radio = WifiRadio {
            name: instance_name(&r.name).to_string(),
            address,
            node_num: r.id.as_deref().and_then(parse_node_id),
            short_name: r.shortname.filter(|s| !s.is_empty()),
        };
        // One entry per radio (or per address when the node is unknown).
        let dup = out.iter().any(|o| match (o.node_num, radio.node_num) {
            (Some(a), Some(b)) => a == b,
            _ => o.address == radio.address,
        });
        if !dup {
            out.push(radio);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Browse the network for `timeout` and return the radios that answered.
pub async fn scan(
    #[allow(unused_variables)] app: &tauri::AppHandle,
    timeout: Duration,
) -> Result<Vec<WifiRadio>, MeshGuardError> {
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            service_type: &'static str,
            timeout_ms: u64,
        }
        #[derive(Deserialize)]
        struct Response {
            services: Vec<RawService>,
        }
        let args = Args {
            service_type: "_meshtastic._tcp",
            timeout_ms: timeout.as_millis() as u64,
        };
        // The Kotlin call blocks for the whole browse window.
        let app = app.clone();
        let response: Response = tokio::task::spawn_blocking(move || {
            let plugin = app
                .try_state::<crate::ble_plugin::BlePluginState<tauri::Wry>>()
                .ok_or_else(|| MeshGuardError::Io("Native plugin not initialized".into()))?;
            plugin
                .0
                .run_mobile_plugin::<Response>("discoverWifiRadios", args)
                .map_err(|e| MeshGuardError::Io(format!("Network scan failed: {e}")))
        })
        .await
        .map_err(|e| MeshGuardError::Io(e.to_string()))??;
        return Ok(normalise(response.services));
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let raw = tokio::task::spawn_blocking(move || browse_mdns(timeout))
            .await
            .map_err(|e| MeshGuardError::Io(e.to_string()))??;
        Ok(normalise(raw))
    }
    #[cfg(target_os = "ios")]
    {
        let _ = timeout;
        Err(MeshGuardError::Io(
            "Network scan is not available on this platform".into(),
        ))
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn browse_mdns(timeout: Duration) -> Result<Vec<RawService>, MeshGuardError> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    use std::time::Instant;

    let err = |e: mdns_sd::Error| MeshGuardError::Io(format!("Network scan failed: {e}"));
    let daemon = ServiceDaemon::new().map_err(err)?;
    let rx = daemon.browse(SERVICE_TYPE).map_err(err)?;
    let deadline = Instant::now() + timeout;
    let mut found = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                // IPv4 first, and a real interface over loopback.
                let mut ips: Vec<std::net::IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|ip| ip.to_ip_addr())
                    .collect();
                ips.sort_by_key(|ip| (ip.is_loopback(), !ip.is_ipv4()));
                let host = ips.first().map(|ip| ip.to_string());
                let Some(host) = host else { continue };
                found.push(RawService {
                    name: info.get_fullname().to_string(),
                    host,
                    port: info.get_port(),
                    id: info.get_property_val_str("id").map(str::to_owned),
                    shortname: info.get_property_val_str("shortname").map(str::to_owned),
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, host: &str, id: Option<&str>) -> RawService {
        RawService {
            name: name.into(),
            host: host.into(),
            port: 4403,
            id: id.map(Into::into),
            shortname: Some("B2C4".into()),
        }
    }

    #[test]
    fn node_ids() {
        assert_eq!(parse_node_id("!a1b2c3d4"), Some(0xa1b2c3d4));
        assert_eq!(parse_node_id("!0000002a"), Some(42));
        assert_eq!(parse_node_id("a1b2c3d4"), None);
        assert_eq!(parse_node_id("!"), None);
        assert_eq!(parse_node_id("!123456789"), None);
    }

    #[test]
    fn normalises_names_addresses_and_duplicates() {
        let out = normalise(vec![
            raw(
                "Meshtastic_b2c4._meshtastic._tcp.local.",
                "192.168.1.20",
                Some("!a1b2c3d4"),
            ),
            // Same radio answering on a second interface.
            raw(
                "Meshtastic_b2c4._meshtastic._tcp.local.",
                "fe80::1%wlan0",
                Some("!a1b2c3d4"),
            ),
            raw("Base", "2001:db8::2", None),
            raw("LinkLocal", "fe80::3", None),
            raw("Nameless", "", None),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Base");
        assert_eq!(out[0].address, "[2001:db8::2]:4403");
        assert_eq!(out[1].name, "Meshtastic_b2c4");
        assert_eq!(out[1].address, "192.168.1.20:4403");
        assert_eq!(out[1].node_num, Some(0xa1b2c3d4));
    }

    #[test]
    fn missing_port_defaults() {
        let mut r = raw("x", "10.0.0.2", None);
        r.port = 0;
        assert_eq!(normalise(vec![r])[0].address, "10.0.0.2:4403");
    }
}
