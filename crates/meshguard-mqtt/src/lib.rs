//! MeshGuard private MQTT relay client (ADR-006).
//!
//! Moves opaque MG2 frames between this installation and a self-hosted MQTT
//! broker. It knows nothing about messages, keys or contacts: the engine
//! decides what to publish where and checks everything it receives. The
//! only guarantees here are transport ones: TLS with full verification,
//! authenticated sessions, exact-topic subscriptions, bounded frames, and
//! truthful status (a PUBACK is *sent to broker*, never *delivered*).

pub mod client;
pub mod config;

pub use client::{
    validate_topic, BadTopic, PublishError, RelayClient, RelayFrame, RelayStatus, MAX_FRAME,
};
pub use config::{
    new_client_id, parse_broker_address, tls_config, BrokerAddress, BrokerSettings, ConfigError,
};
