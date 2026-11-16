//! The relay connection: one MQTT 5 session per installation (ADR-006 §2.5).
//!
//! * Each network connection is a fresh rumqttc client and event loop with
//!   its own *generation*. Nothing rumqttc queued carries over to the next
//!   connection, so a reply can never be matched to a request of another
//!   connection; whatever was waiting fails with its generation and the
//!   app's outbox retries. The broker session itself persists
//!   (`clean_start = false`), so subscriptions and queued frames for us
//!   survive reconnects.
//! * Publishes go out with QoS 1, `retain = false` and a message expiry.
//!   [`RelayClient::publish`] resolves on the broker's PUBACK, which means
//!   *sent to broker*, never delivered. Exactly one publish is outstanding
//!   at a time; a PUBACK timeout resets the connection rather than guess.
//! * Subscriptions are exact relay topics (validated; no wildcards), sent
//!   as one SUBSCRIBE at a time by a control task and matched to their
//!   SUBACK by packet id and generation; a SUBACK timeout also resets the
//!   connection. Refused topics are reported.
//! * The event loop never awaits anything but the broker: incoming frames
//!   that do not fit the bounded queue are dropped (the sender's app-level
//!   retry recovers them), and requests to rumqttc are made with `try_`.
//! * Certificate and auth failures stop reconnecting until
//!   [`RelayClient::retry`]; other errors reconnect with back-off.
//! * Nothing here logs topics, payloads or credentials.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rumqttc::v5::mqttbytes::v5::{
    ConnectReturnCode, Filter, Packet, PubAckReason, PublishProperties, SubscribeReasonCode,
    UnsubAckReason,
};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, ConnectionError, Event, MqttOptions};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::config::{tls_config, BrokerSettings, ConfigError};

/// Largest relay payload accepted or sent. MG2 frames are at most 228
/// bytes; this leaves room without letting a peer push large blobs.
pub const MAX_FRAME: usize = 512;
/// How long [`RelayClient::publish`] waits for the broker's PUBACK.
pub const PUBACK_TIMEOUT: Duration = Duration::from_secs(20);
/// Topic namespace this client accepts (ADR-006 §5).
pub const TOPIC_PREFIX: &str = "mg/r1/";
/// Longest topic accepted.
pub const MAX_TOPIC_LEN: usize = 200;
const KEEP_ALIVE: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT_SECS: u64 = 15;
const SUBACK_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Incoming frames waiting for the app; more are dropped.
const INBOUND_QUEUE: usize = 64;

/// Connection state, for the UI and the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStatus {
    Connecting,
    Connected,
    /// Wrong username/password or not authorised. No retry until settings
    /// change.
    AuthFailed,
    /// Certificate or hostname check failed. No retry until settings or the
    /// network change.
    TlsError(String),
    /// Lost or could not connect; retrying with back-off.
    Disconnected(String),
    Stopped,
}

impl RelayStatus {
    /// Stable name for the UI.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::AuthFailed => "auth_failed",
            Self::TlsError(_) => "tls_error",
            Self::Disconnected(_) => "disconnected",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PublishError {
    #[error("relay is not connected")]
    Unavailable,
    /// The previous publish is still waiting for its PUBACK.
    #[error("relay is busy")]
    Busy,
    #[error("frame of {0} bytes is too large for the relay")]
    TooLarge(usize),
    #[error("not a relay topic")]
    BadTopic,
    #[error("broker refused the message ({0})")]
    Refused(String),
    #[error("no answer from the broker")]
    Timeout,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("not a relay topic")]
pub struct BadTopic;

/// A frame received on one of our topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayFrame {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// An exact relay topic: `mg/r1/` and three non-empty segments of
/// `[A-Za-z0-9._-]`, at most [`MAX_TOPIC_LEN`] bytes. No wildcards.
pub fn validate_topic(topic: &str) -> Result<(), BadTopic> {
    let Some(rest) = topic.strip_prefix(TOPIC_PREFIX) else {
        return Err(BadTopic);
    };
    let segs: Vec<&str> = rest.split('/').collect();
    let ok = topic.len() <= MAX_TOPIC_LEN
        && segs.len() == 3
        && segs.iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(BadTopic)
    }
}

type AckTx = oneshot::Sender<Result<(), PublishError>>;

/// The single outstanding publish (request `seq`), owned by connection
/// generation `gen`.
enum Slot {
    Idle,
    /// Handed to that connection's event loop, packet id not known yet.
    Queued {
        seq: u64,
        gen: u64,
        tx: AckTx,
    },
    /// On the wire with this packet id.
    Sent {
        seq: u64,
        gen: u64,
        pkid: u16,
        tx: AckTx,
    },
}

impl Slot {
    fn finish(&mut self, r: Result<(), PublishError>) {
        if let Slot::Queued { tx, .. } | Slot::Sent { tx, .. } = std::mem::replace(self, Slot::Idle)
        {
            let _ = tx.send(r);
        }
    }

    fn seq(&self) -> Option<u64> {
        match self {
            Slot::Idle => None,
            Slot::Queued { seq, .. } | Slot::Sent { seq, .. } => Some(*seq),
        }
    }
}

/// Answer to a SUBSCRIBE or UNSUBSCRIBE.
enum CtlAck {
    Sub(Vec<SubscribeReasonCode>),
    Unsub(Vec<UnsubAckReason>),
}

/// The one outstanding SUBSCRIBE or UNSUBSCRIBE: its connection, its packet
/// id once sent, and who waits.
struct CtlWait {
    gen: u64,
    unsub: bool,
    pkid: Option<u16>,
    tx: oneshot::Sender<CtlAck>,
}

/// Bring the broker session in line: unsubscribe what is pending, then
/// subscribe to every desired topic.
struct Resync;

/// Why a connection should be ended.
enum Reset {
    /// Publish `seq` on connection `gen` got no PUBACK.
    Publish { gen: u64, seq: u64 },
    /// A SUBSCRIBE/UNSUBSCRIBE on connection `gen` got no answer (the
    /// control task waits for that connection to end before going on).
    Control { gen: u64 },
}

struct Shared {
    status: watch::Sender<RelayStatus>,
    topics: Mutex<BTreeSet<String>>,
    refused: Mutex<BTreeSet<String>>,
    /// The live connection: its generation and client. `None` between
    /// connections. Changed only together with `slot` (under its lock).
    conn: Mutex<Option<(u64, AsyncClient)>>,
    slot: Mutex<Slot>,
    ctl_wait: Mutex<Option<CtlWait>>,
    /// Topics to unsubscribe from in the broker session, kept across
    /// connections until the broker confirms (or forgot the session).
    unsub_pending: Mutex<BTreeSet<String>>,
    control: mpsc::UnboundedSender<Resync>,
    /// End a connection whose state a timeout made uncertain. Checked
    /// again when consumed, so it cannot end a newer connection or abort a
    /// newer publish.
    reset: mpsc::UnboundedSender<Reset>,
    retry: Notify,
    dropped_inbound: AtomicU64,
    next_seq: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn set_status(&self, s: RelayStatus) {
        self.status.send_if_modified(|cur| {
            if *cur == s {
                false
            } else {
                tracing::info!("MQTT relay: {}", s.as_str());
                *cur = s;
                true
            }
        });
    }

    /// The live connection, if it is connected.
    fn live(&self) -> Option<(u64, AsyncClient)> {
        if *self.status.borrow() != RelayStatus::Connected {
            return None;
        }
        lock(&self.conn).clone()
    }
}

/// Handle to the running relay connection. Dropping it stops the
/// connection.
pub struct RelayClient {
    shared: Arc<Shared>,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl RelayClient {
    /// Validate `settings` and `topics`, then connect in the background.
    /// Frames arriving on `topics` go to the returned receiver.
    /// `unsubscribe`: topics the broker session may still hold from an
    /// earlier run (revoked while offline); they are removed once
    /// connected. See [`Self::pending_unsubscribes`].
    pub fn start(
        settings: &BrokerSettings,
        topics: BTreeSet<String>,
        mut unsubscribe: BTreeSet<String>,
    ) -> Result<(Self, mpsc::Receiver<RelayFrame>), ConfigError> {
        settings.validate()?;
        if topics
            .iter()
            .chain(&unsubscribe)
            .any(|t| validate_topic(t).is_err())
        {
            return Err(ConfigError::BadTopic);
        }
        unsubscribe.retain(|t| !topics.contains(t));
        let tls = tls_config(settings.ca_pem.as_deref())?;
        let mut opts = MqttOptions::new(
            settings.client_id.clone(),
            settings.address.host.clone(),
            settings.address.port,
        );
        opts.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(tls)))
            .set_credentials(settings.username.clone(), settings.password.to_string())
            .set_keep_alive(KEEP_ALIVE)
            .set_clean_start(false)
            .set_session_expiry_interval(Some(settings.session_expiry_secs))
            .set_connection_timeout(CONNECT_TIMEOUT_SECS)
            // Tell the broker not to send anything much larger than a frame.
            .set_max_packet_size(Some((MAX_FRAME + 512) as u32))
            .set_topic_alias_max(None);
        let (status, _) = watch::channel(RelayStatus::Connecting);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (reset_tx, reset_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            status,
            topics: Mutex::new(topics),
            refused: Mutex::new(BTreeSet::new()),
            conn: Mutex::new(None),
            slot: Mutex::new(Slot::Idle),
            ctl_wait: Mutex::new(None),
            unsub_pending: Mutex::new(unsubscribe),
            control: control_tx,
            reset: reset_tx,
            retry: Notify::new(),
            dropped_inbound: AtomicU64::new(0),
            next_seq: AtomicU64::new(1),
        });
        let (tx, rx) = mpsc::channel(INBOUND_QUEUE);
        let tasks = [
            tokio::spawn(run(opts, shared.clone(), tx, reset_rx)),
            tokio::spawn(control(shared.clone(), control_rx)),
        ];
        Ok((Self { shared, tasks }, rx))
    }

    pub fn status(&self) -> RelayStatus {
        self.shared.status.borrow().clone()
    }

    /// Follow status changes.
    pub fn watch_status(&self) -> watch::Receiver<RelayStatus> {
        self.shared.status.subscribe()
    }

    /// Topics the broker refused to subscribe us to (e.g. strict ACL lines
    /// missing). Some brokers (Mosquitto 2) accept the SUBSCRIBE and filter
    /// on delivery instead.
    pub fn refused_topics(&self) -> BTreeSet<String> {
        lock(&self.shared.refused).clone()
    }

    /// Topics not yet confirmed unsubscribed by the broker. Persist these
    /// and pass them to the next [`Self::start`].
    pub fn pending_unsubscribes(&self) -> BTreeSet<String> {
        lock(&self.shared.unsub_pending).clone()
    }

    /// Incoming frames dropped because the app did not keep up.
    pub fn dropped_inbound(&self) -> u64 {
        self.shared.dropped_inbound.load(Ordering::Relaxed)
    }

    /// Reconnect now (after an auth/TLS failure or a network change).
    pub fn retry(&self) {
        self.shared.retry.notify_one();
    }

    /// Publish one opaque frame. Resolves on the broker's PUBACK: the frame
    /// is with the broker, *not* delivered to the recipient. Refused while
    /// disconnected or while another publish is outstanding, so nothing
    /// queues here; the app's outbox retries.
    pub async fn publish(
        &self,
        topic: &str,
        frame: &[u8],
        expiry_secs: u32,
    ) -> Result<(), PublishError> {
        if frame.len() > MAX_FRAME {
            return Err(PublishError::TooLarge(frame.len()));
        }
        validate_topic(topic).map_err(|_| PublishError::BadTopic)?;
        let (tx, rx) = oneshot::channel();
        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed);
        let gen = {
            let mut slot = lock(&self.shared.slot);
            if !matches!(*slot, Slot::Idle) {
                return Err(PublishError::Busy);
            }
            let (gen, client) = self.shared.live().ok_or(PublishError::Unavailable)?;
            let props = PublishProperties {
                message_expiry_interval: Some(expiry_secs.max(1)),
                ..Default::default()
            };
            // Under the slot lock: the event loop sees the slot as Queued
            // before it can see the request.
            client
                .try_publish_with_properties(
                    topic.to_string(),
                    QoS::AtLeastOnce,
                    false,
                    Bytes::copy_from_slice(frame),
                    props,
                )
                .map_err(|_| PublishError::Unavailable)?;
            *slot = Slot::Queued { seq, gen, tx };
            gen
        };
        match tokio::time::timeout(PUBACK_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(PublishError::Unavailable),
            Err(_) => {
                // No answer: the connection's state is uncertain. End it if
                // this publish is still outstanding (its end frees the slot);
                // the reset names the connection, so it cannot hit a newer
                // one.
                if lock(&self.shared.slot).seq() == Some(seq) {
                    tracing::info!("MQTT relay: no PUBACK, reconnecting");
                    let _ = self.shared.reset.send(Reset::Publish { gen, seq });
                }
                Err(PublishError::Timeout)
            }
        }
    }

    /// Replace the set of subscribed topics. Frames on removed topics are
    /// dropped from now on, even if the broker still delivers some.
    pub fn set_topics(&self, topics: BTreeSet<String>) -> Result<(), BadTopic> {
        for t in &topics {
            validate_topic(t)?;
        }
        {
            let mut cur = lock(&self.shared.topics);
            let mut pending = lock(&self.shared.unsub_pending);
            pending.extend(cur.difference(&topics).cloned());
            pending.retain(|t| !topics.contains(t));
            *cur = topics.clone();
        }
        lock(&self.shared.refused).retain(|t| topics.contains(t));
        let _ = self.shared.control.send(Resync);
        Ok(())
    }

    /// Disconnect and stop.
    pub async fn stop(self) {
        let client = lock(&self.shared.conn).as_ref().map(|(_, c)| c.clone());
        if let Some(c) = client {
            let _ = tokio::time::timeout(Duration::from_secs(2), c.disconnect()).await;
        }
        self.shared.set_status(RelayStatus::Stopped);
        lock(&self.shared.slot).finish(Err(PublishError::Unavailable));
        // Drop runs next and aborts the tasks.
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Whether a rustls error means "this broker cannot be trusted" (permanent)
/// rather than a broken connection.
fn rustls_is_permanent(e: &rustls::Error) -> bool {
    matches!(
        e,
        rustls::Error::InvalidCertificate(_)
            | rustls::Error::NoCertificatesPresented
            | rustls::Error::UnsupportedNameType
            | rustls::Error::PeerIncompatible(_)
    )
}

fn io_status(io: &std::io::Error) -> RelayStatus {
    match io
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(t) if rustls_is_permanent(t) => RelayStatus::TlsError(t.to_string()),
        Some(t) => RelayStatus::Disconnected(format!("TLS: {t}")),
        None => RelayStatus::Disconnected(io.kind().to_string()),
    }
}

/// What a connection error means for us.
fn classify(e: &ConnectionError) -> RelayStatus {
    use rumqttc::TlsError as T;
    match e {
        ConnectionError::ConnectionRefused(code) => match code {
            ConnectReturnCode::BadUserNamePassword
            | ConnectReturnCode::NotAuthorized
            | ConnectReturnCode::Banned
            | ConnectReturnCode::BadAuthenticationMethod
            | ConnectReturnCode::BadClientId
            | ConnectReturnCode::ClientIdentifierNotValid => RelayStatus::AuthFailed,
            other => RelayStatus::Disconnected(format!("refused: {other:?}")),
        },
        ConnectionError::Tls(t) => match t {
            T::Io(io) => io_status(io),
            T::TLS(r) if !rustls_is_permanent(r) => RelayStatus::Disconnected(format!("TLS: {r}")),
            other => RelayStatus::TlsError(other.to_string()),
        },
        ConnectionError::Io(io) => io_status(io),
        ConnectionError::Timeout(_) => RelayStatus::Disconnected("timed out".into()),
        other => RelayStatus::Disconnected(other.to_string()),
    }
}

fn jittered(d: Duration) -> Duration {
    use rand::Rng;
    d.mul_f64(rand::thread_rng().gen_range(0.75..1.25))
}

/// Send one SUBSCRIBE/UNSUBSCRIBE request (by `send`) on connection `gen`
/// and wait for its answer. `None`: no answer (the connection ended, or it
/// was reset because the answer did not come).
async fn ctl_request<F>(shared: &Shared, gen: u64, unsub: bool, send: F) -> Option<CtlAck>
where
    F: std::future::Future<Output = Result<(), rumqttc::v5::ClientError>>,
{
    let (tx, wait) = oneshot::channel();
    *lock(&shared.ctl_wait) = Some(CtlWait {
        gen,
        unsub,
        pkid: None,
        tx,
    });
    if send.await.is_err() {
        lock(&shared.ctl_wait).take();
        return None;
    }
    match tokio::time::timeout(SUBACK_TIMEOUT, wait).await {
        Ok(Ok(ack)) => Some(ack),
        Ok(Err(_)) => None,
        Err(_) => {
            // No answer: do not guess which later one is ours.
            if lock(&shared.ctl_wait).take().is_some_and(|w| w.gen == gen) {
                tracing::info!("MQTT relay: no SUBACK/UNSUBACK, reconnecting");
                let _ = shared.reset.send(Reset::Control { gen });
                // No further request on this connection: wait for its end.
                let mut status = shared.status.subscribe();
                while lock(&shared.conn).as_ref().is_some_and(|(g, _)| *g == gen) {
                    if tokio::time::timeout(Duration::from_secs(1), status.changed())
                        .await
                        .is_ok_and(|r| r.is_err())
                    {
                        break;
                    }
                }
            }
            None
        }
    }
}

/// Subscriptions, one request at a time so each answer is unambiguous.
async fn control(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Resync>) {
    'resync: while rx.recv().await.is_some() {
        // Coalesce queued resyncs: the current state covers them all.
        while rx.try_recv().is_ok() {}
        let Some((gen, client)) = shared.live() else {
            continue; // the next CONNACK resyncs
        };
        let pending: Vec<String> = lock(&shared.unsub_pending).iter().cloned().collect();
        for t in pending {
            match ctl_request(&shared, gen, true, client.unsubscribe(t.clone())).await {
                Some(CtlAck::Unsub(codes)) => {
                    let gone = codes.first().is_some_and(|c| {
                        matches!(
                            c,
                            UnsubAckReason::Success | UnsubAckReason::NoSubscriptionExisted
                        )
                    });
                    if gone {
                        lock(&shared.unsub_pending).remove(&t);
                    }
                }
                _ => continue 'resync,
            }
        }
        let topics: Vec<String> = lock(&shared.topics).iter().cloned().collect();
        if topics.is_empty() {
            lock(&shared.refused).clear();
            continue;
        }
        let filters: Vec<Filter> = topics
            .iter()
            .map(|t| Filter::new(t.clone(), QoS::AtLeastOnce))
            .collect();
        let Some(CtlAck::Sub(codes)) =
            ctl_request(&shared, gen, false, client.subscribe_many(filters)).await
        else {
            continue;
        };
        let refused: BTreeSet<String> = topics
            .into_iter()
            .zip(codes)
            .filter(|(_, c)| !matches!(c, SubscribeReasonCode::Success(_)))
            .map(|(t, _)| t)
            .collect();
        if !refused.is_empty() {
            tracing::warn!(
                "MQTT relay: broker refused {} subscription(s)",
                refused.len()
            );
        }
        *lock(&shared.refused) = refused;
    }
}

/// Handle one event of connection `gen`. `Err(())`: the app stopped
/// listening.
fn on_event(
    event: Event,
    gen: u64,
    shared: &Shared,
    inbound: &mpsc::Sender<RelayFrame>,
) -> Result<(), ()> {
    match event {
        Event::Incoming(Packet::ConnAck(ack)) => {
            if !ack.session_present {
                // The broker has no session for us: nothing to unsubscribe.
                lock(&shared.unsub_pending).clear();
            }
            shared.set_status(RelayStatus::Connected);
            // (Re-)subscribe on every connection: cheap, and correct
            // whether or not the broker kept the session.
            let _ = shared.control.send(Resync);
        }
        Event::Outgoing(Outgoing::Publish(pkid)) => {
            let mut slot = lock(&shared.slot);
            if matches!(*slot, Slot::Queued { gen: g, .. } if g == gen) {
                if let Slot::Queued { seq, tx, .. } = std::mem::replace(&mut *slot, Slot::Idle) {
                    *slot = Slot::Sent { seq, gen, pkid, tx };
                }
            }
        }
        Event::Incoming(Packet::PubAck(ack)) => {
            let mut slot = lock(&shared.slot);
            if matches!(*slot, Slot::Sent { gen: g, pkid, .. } if g == gen && pkid == ack.pkid) {
                slot.finish(match ack.reason {
                    PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Ok(()),
                    other => Err(PublishError::Refused(format!("{other:?}"))),
                });
            }
        }
        Event::Outgoing(Outgoing::Subscribe(pkid)) => note_ctl_pkid(shared, gen, false, pkid),
        Event::Outgoing(Outgoing::Unsubscribe(pkid)) => note_ctl_pkid(shared, gen, true, pkid),
        Event::Incoming(Packet::SubAck(ack)) => {
            if let Some(w) = take_ctl_wait(shared, gen, false, ack.pkid) {
                let _ = w.tx.send(CtlAck::Sub(ack.return_codes));
            }
        }
        Event::Incoming(Packet::UnsubAck(ack)) => {
            if let Some(w) = take_ctl_wait(shared, gen, true, ack.pkid) {
                let _ = w.tx.send(CtlAck::Unsub(ack.reasons));
            }
        }
        Event::Incoming(Packet::Publish(p)) => {
            let Ok(topic) = std::str::from_utf8(&p.topic) else {
                return Ok(());
            };
            if !lock(&shared.topics).contains(topic) {
                tracing::debug!("MQTT relay: frame on an unknown topic dropped");
                return Ok(());
            }
            if p.payload.len() > MAX_FRAME {
                tracing::info!("MQTT relay: oversize frame dropped");
                return Ok(());
            }
            let frame = RelayFrame {
                topic: topic.to_string(),
                payload: p.payload.to_vec(),
            };
            match inbound.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    shared.dropped_inbound.fetch_add(1, Ordering::Relaxed);
                    tracing::info!("MQTT relay: app busy, frame dropped");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
            }
        }
        _ => {}
    }
    Ok(())
}

fn note_ctl_pkid(shared: &Shared, gen: u64, unsub: bool, pkid: u16) {
    if let Some(w) = lock(&shared.ctl_wait).as_mut() {
        if w.gen == gen && w.unsub == unsub {
            w.pkid.get_or_insert(pkid);
        }
    }
}

fn take_ctl_wait(shared: &Shared, gen: u64, unsub: bool, pkid: u16) -> Option<CtlWait> {
    let mut wait = lock(&shared.ctl_wait);
    if wait
        .as_ref()
        .is_some_and(|w| w.gen == gen && w.unsub == unsub && w.pkid == Some(pkid))
    {
        wait.take()
    } else {
        None
    }
}

async fn run(
    opts: MqttOptions,
    shared: Arc<Shared>,
    inbound: mpsc::Sender<RelayFrame>,
    mut reset: mpsc::UnboundedReceiver<Reset>,
) {
    let mut backoff = BACKOFF_MIN;
    let mut gen = 0u64;
    loop {
        gen += 1;
        // A fresh client per connection: nothing queued for an older
        // connection can reach this one.
        let (client, mut eventloop) = AsyncClient::new(opts.clone(), 16);
        *lock(&shared.conn) = Some((gen, client));
        let ended = loop {
            tokio::select! {
                polled = eventloop.poll() => match polled {
                    Ok(event) => {
                        if matches!(event, Event::Incoming(Packet::ConnAck(_))) {
                            backoff = BACKOFF_MIN;
                        }
                        if on_event(event, gen, &shared, &inbound).is_err() {
                            return;
                        }
                    }
                    Err(e) => break classify(&e),
                },
                Some(r) = reset.recv() => {
                    let current = match r {
                        Reset::Publish { gen: g, seq } => {
                            g == gen && lock(&shared.slot).seq() == Some(seq)
                        }
                        Reset::Control { gen: g } => g == gen,
                    };
                    if current {
                        break RelayStatus::Disconnected("reset".into());
                    }
                }
            }
        };
        // End of this connection: everything waiting on it fails.
        {
            let mut slot = lock(&shared.slot);
            *lock(&shared.conn) = None;
            slot.finish(Err(PublishError::Unavailable));
        }
        lock(&shared.ctl_wait).take();
        drop(eventloop);
        let permanent = matches!(ended, RelayStatus::AuthFailed | RelayStatus::TlsError(_));
        shared.set_status(ended);
        if permanent {
            shared.retry.notified().await;
            backoff = BACKOFF_MIN;
        } else {
            tokio::select! {
                _ = tokio::time::sleep(jittered(backoff)) => {}
                _ = shared.retry.notified() => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        shared.set_status(RelayStatus::Connecting);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_and_certificates_as_permanent_only() {
        assert_eq!(
            classify(&ConnectionError::ConnectionRefused(
                ConnectReturnCode::BadUserNamePassword
            )),
            RelayStatus::AuthFailed
        );
        assert_eq!(
            classify(&ConnectionError::ConnectionRefused(
                ConnectReturnCode::NotAuthorized
            )),
            RelayStatus::AuthFailed
        );
        let bad_cert = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName),
            )
        };
        assert!(matches!(
            classify(&ConnectionError::Io(bad_cert())),
            RelayStatus::TlsError(_)
        ));
        assert!(matches!(
            classify(&ConnectionError::Tls(rumqttc::TlsError::Io(bad_cert()))),
            RelayStatus::TlsError(_)
        ));
        // A reset or EOF during the handshake is a network problem: retry.
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(matches!(
                classify(&ConnectionError::Tls(rumqttc::TlsError::Io(kind.into()))),
                RelayStatus::Disconnected(_)
            ));
        }
        assert!(matches!(
            classify(&ConnectionError::Io(
                std::io::ErrorKind::ConnectionRefused.into()
            )),
            RelayStatus::Disconnected(_)
        ));
        assert!(matches!(
            classify(&ConnectionError::ConnectionRefused(
                ConnectReturnCode::ServerBusy
            )),
            RelayStatus::Disconnected(_)
        ));
    }

    #[test]
    fn only_exact_relay_topics_are_accepted() {
        assert!(validate_topic("mg/r1/mg-bob/mg-alice/abc234").is_ok());
        for bad in [
            "mg/r1/#",
            "mg/r1/+/a/b",
            "mg/r1/a/b",
            "mg/r1/a/b/c/d",
            "mg/r1/a//c",
            "mg/r1/a/b/c+",
            "mg/r1/./b/c",
            "other/a/b/c",
            "",
            &format!("mg/r1/a/b/{}", "x".repeat(200)),
        ] {
            assert!(validate_topic(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jitter_stays_near_the_value() {
        for _ in 0..100 {
            let d = jittered(Duration::from_secs(10));
            assert!(d >= Duration::from_millis(7500) && d <= Duration::from_millis(12500));
        }
    }

    #[test]
    fn a_finished_slot_reports_once_and_is_idle() {
        let (tx, mut rx) = oneshot::channel();
        let mut s = Slot::Sent {
            seq: 7,
            gen: 1,
            pkid: 3,
            tx,
        };
        assert_eq!(s.seq(), Some(7));
        s.finish(Ok(()));
        assert!(matches!(s, Slot::Idle));
        assert_eq!(s.seq(), None);
        assert_eq!(rx.try_recv().unwrap(), Ok(()));
        // A slot whose caller is gone finishes silently.
        let (tx, rx) = oneshot::channel();
        drop(rx);
        let mut s = Slot::Queued { seq: 8, gen: 2, tx };
        s.finish(Err(PublishError::Unavailable));
        assert!(matches!(s, Slot::Idle));
    }
}
