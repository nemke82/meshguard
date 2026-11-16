//! Broker settings, address parsing and the TLS configuration.
//!
//! TLS is always on. There is no plaintext scheme and no way to skip
//! certificate or hostname verification (ADR-006 §2.4).

use std::sync::Arc;

use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;
use thiserror::Error;
use zeroize::Zeroizing;

/// Default port of MQTT over TLS.
pub const DEFAULT_PORT: u16 = 8883;
/// Allowed session expiry, seconds (1 to 7 days).
pub const SESSION_EXPIRY_RANGE: std::ops::RangeInclusive<u32> = 86_400..=7 * 86_400;
/// Default session expiry: 3 days.
pub const DEFAULT_SESSION_EXPIRY: u32 = 3 * 86_400;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConfigError {
    #[error("Only mqtts:// (MQTT over TLS) is supported")]
    UnsupportedScheme,
    #[error("Broker address is not valid")]
    BadAddress,
    #[error("Broker port is not valid")]
    BadPort,
    #[error("Username is required")]
    MissingUsername,
    #[error("Password is required")]
    MissingPassword,
    #[error("The CA certificate could not be read: {0}")]
    BadCa(String),
    #[error("Session expiry must be between 1 and 7 days")]
    BadSessionExpiry,
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("Not a relay topic")]
    BadTopic,
}

/// Where the broker is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerAddress {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for BrokerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "mqtts://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "mqtts://{}:{}", self.host, self.port)
        }
    }
}

/// Parse `mqtts://host[:port]` or `host[:port]` (IPv6 in brackets).
/// Every other scheme is refused, including plain `mqtt://`.
pub fn parse_broker_address(input: &str) -> Result<BrokerAddress, ConfigError> {
    let s = input.trim();
    let rest = match s.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("mqtts") => rest,
        Some(_) => return Err(ConfigError::UnsupportedScheme),
        None => s,
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
        return Err(ConfigError::BadAddress);
    }
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or(ConfigError::BadAddress)?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(ConfigError::BadAddress)?),
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(ConfigError::BadAddress);
        }
        (host, port)
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            Some(_) => return Err(ConfigError::BadAddress),
            None => (rest, None),
        }
    };
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => match p.parse::<u16>() {
            Ok(p) if p != 0 => p,
            _ => return Err(ConfigError::BadPort),
        },
    };
    let valid_host = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    if !valid_host || rustls_pki_types::ServerName::try_from(host).is_err() {
        return Err(ConfigError::BadAddress);
    }
    Ok(BrokerAddress {
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Everything needed to connect. The password is wiped from memory when
/// this is dropped; never log it.
#[derive(Clone)]
pub struct BrokerSettings {
    pub address: BrokerAddress,
    pub username: String,
    pub password: Zeroizing<String>,
    /// PEM of a private CA. When set, *only* this CA is trusted for the
    /// broker; otherwise the Mozilla root set is used.
    pub ca_pem: Option<String>,
    /// Random, persisted per installation (`mg-<hex>`).
    pub client_id: String,
    pub session_expiry_secs: u32,
}

impl std::fmt::Debug for BrokerSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerSettings")
            .field("address", &self.address)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("custom_ca", &self.ca_pem.is_some())
            .field("session_expiry_secs", &self.session_expiry_secs)
            .finish()
    }
}

impl BrokerSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.username.trim().is_empty() {
            return Err(ConfigError::MissingUsername);
        }
        if self.password.is_empty() {
            return Err(ConfigError::MissingPassword);
        }
        if !SESSION_EXPIRY_RANGE.contains(&self.session_expiry_secs) {
            return Err(ConfigError::BadSessionExpiry);
        }
        tls_config(self.ca_pem.as_deref())?;
        Ok(())
    }
}

/// A new random client id for this installation.
pub fn new_client_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("mg-{hex}")
}

/// Trust anchors from a PEM bundle (one or more certificates).
fn roots_from_pem(pem: &str) -> Result<RootCertStore, ConfigError> {
    let mut roots = RootCertStore::empty();
    let mut n = 0;
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        let cert = cert.map_err(|e| ConfigError::BadCa(e.to_string()))?;
        roots
            .add(cert)
            .map_err(|e| ConfigError::BadCa(e.to_string()))?;
        n += 1;
    }
    if n == 0 {
        return Err(ConfigError::BadCa("no certificate found".into()));
    }
    Ok(roots)
}

/// The rustls client configuration: ring provider, safe protocol versions
/// (TLS 1.2 and 1.3), full chain and hostname verification.
pub fn tls_config(ca_pem: Option<&str>) -> Result<Arc<rustls::ClientConfig>, ConfigError> {
    let roots = match ca_pem.map(str::trim).filter(|p| !p.is_empty()) {
        Some(pem) => roots_from_pem(pem)?,
        None => RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ConfigError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(h: &str, p: u16) -> BrokerAddress {
        BrokerAddress {
            host: h.into(),
            port: p,
        }
    }

    #[test]
    fn parses_tls_addresses() {
        assert_eq!(
            parse_broker_address("mqtts://Broker.Example.org").unwrap(),
            addr("broker.example.org", 8883)
        );
        assert_eq!(
            parse_broker_address(" MQTTS://b.example:9883/ ").unwrap(),
            addr("b.example", 9883)
        );
        assert_eq!(
            parse_broker_address("b.example:1234").unwrap(),
            addr("b.example", 1234)
        );
        assert_eq!(
            parse_broker_address("10.0.0.5").unwrap(),
            addr("10.0.0.5", 8883)
        );
        assert_eq!(
            parse_broker_address("mqtts://[::1]:8884").unwrap(),
            addr("::1", 8884)
        );
        assert_eq!(addr("::1", 8884).to_string(), "mqtts://[::1]:8884");
    }

    #[test]
    fn refuses_plaintext_and_garbage() {
        for s in [
            "mqtt://b.example",
            "tcp://b.example",
            "ws://b",
            "wss://b",
            "http://b",
        ] {
            assert_eq!(
                parse_broker_address(s),
                Err(ConfigError::UnsupportedScheme),
                "{s}"
            );
        }
        for s in [
            "",
            "mqtts://",
            "b.example/x",
            "user@b.example",
            "b example",
            "::1",
            "[::1",
            "[nothost]:1",
            "a_b.example",
            "b.example:x",
        ] {
            assert!(parse_broker_address(s).is_err(), "{s}");
        }
        assert_eq!(
            parse_broker_address("b.example:0"),
            Err(ConfigError::BadPort)
        );
        assert_eq!(
            parse_broker_address("b.example:70000"),
            Err(ConfigError::BadPort)
        );
    }

    fn settings() -> BrokerSettings {
        BrokerSettings {
            address: addr("b.example", 8883),
            username: "alice".into(),
            password: Zeroizing::new("pw".into()),
            ca_pem: None,
            client_id: new_client_id(),
            session_expiry_secs: DEFAULT_SESSION_EXPIRY,
        }
    }

    #[test]
    fn validates_settings() {
        assert!(settings().validate().is_ok());
        let mut s = settings();
        s.username = " ".into();
        assert_eq!(s.validate(), Err(ConfigError::MissingUsername));
        let mut s = settings();
        s.password = Zeroizing::new(String::new());
        assert_eq!(s.validate(), Err(ConfigError::MissingPassword));
        let mut s = settings();
        s.session_expiry_secs = 60;
        assert_eq!(s.validate(), Err(ConfigError::BadSessionExpiry));
        let mut s = settings();
        s.ca_pem = Some("-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----".into());
        assert!(matches!(s.validate(), Err(ConfigError::BadCa(_))));
        let mut s = settings();
        s.ca_pem = Some("hello".into());
        assert!(matches!(s.validate(), Err(ConfigError::BadCa(_))));
    }

    #[test]
    fn debug_hides_credentials() {
        let d = format!("{:?}", settings());
        assert!(!d.contains("alice") && !d.contains("pw\""), "{d}");
    }

    #[test]
    fn client_ids_are_random() {
        let a = new_client_id();
        assert!(a.starts_with("mg-") && a.len() == 35);
        assert_ne!(a, new_client_id());
    }
}
