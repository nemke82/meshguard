//! Integration tests against a local Mosquitto 2 broker with TLS, accounts
//! and the family ACL (ADR-006 §5, §8).
//!
//! Ignored by default; `dev/mqtt/run-tests.sh` starts a throw-away broker,
//! sets the environment and runs them.

use std::collections::BTreeSet;
use std::time::Duration;

use meshguard_mqtt::{
    BrokerAddress, BrokerSettings, PublishError, RelayClient, RelayFrame, RelayStatus,
};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} not set; run dev/mqtt/run-tests.sh"))
}

fn ca(var: &str) -> String {
    std::fs::read_to_string(env(var)).unwrap()
}

fn settings(user: char, host: &str, client_id: &str) -> BrokerSettings {
    BrokerSettings {
        address: BrokerAddress {
            host: host.into(),
            port: env("MG_MQTT_PORT").parse().unwrap(),
        },
        username: env(&format!("MG_MQTT_USER_{user}")),
        password: Zeroizing::new(env(&format!("MG_MQTT_PASS_{user}"))),
        ca_pem: Some(ca("MG_MQTT_CA")),
        client_id: client_id.into(),
        session_expiry_secs: 86_400,
    }
}

fn user(u: char) -> String {
    env(&format!("MG_MQTT_USER_{u}"))
}

/// Topic `sender -> recipient` with a fixed alias.
fn topic(sender: char, recipient: char, alias: &str) -> String {
    format!("mg/r1/{}/{}/{alias}", user(recipient), user(sender))
}

fn set(topics: &[&str]) -> BTreeSet<String> {
    topics.iter().map(|t| t.to_string()).collect()
}

fn unique(tag: &str) -> String {
    format!("mg-test-{tag}-{}", meshguard_mqtt::new_client_id())
}

async fn wait_status(c: &RelayClient, want: fn(&RelayStatus) -> bool) -> RelayStatus {
    let mut rx = c.watch_status();
    let r = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let s = rx.borrow_and_update().clone();
            if want(&s) {
                return s;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    })
    .await;
    r.unwrap_or_else(|_| panic!("status stayed {:?}", c.status()))
}

async fn connected(c: &RelayClient) {
    wait_status(c, |s| *s == RelayStatus::Connected).await;
    // SUBSCRIBE follows CONNACK; give the broker a moment to register it.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

async fn recv(rx: &mut mpsc::Receiver<RelayFrame>, within: Duration) -> Option<RelayFrame> {
    tokio::time::timeout(within, rx.recv()).await.ok().flatten()
}

#[tokio::test]
#[ignore]
async fn two_clients_exchange_frames_and_puback_is_only_submission() {
    let a_to_b = topic('A', 'B', "ab1");
    let b_to_a = topic('B', 'A', "ba1");
    let (a, mut a_rx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[&b_to_a]),
        set(&[]),
    )
    .unwrap();
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b")),
        set(&[&a_to_b]),
        set(&[]),
    )
    .unwrap();
    connected(&a).await;
    connected(&b).await;

    // PUBACK: accepted by the broker. It says nothing about B.
    a.publish(&a_to_b, b"MG2-frame-1", 60).await.unwrap();
    let got = recv(&mut b_rx, Duration::from_secs(5))
        .await
        .expect("B got it");
    assert_eq!(got.payload, b"MG2-frame-1");
    assert_eq!(got.topic, a_to_b);

    b.publish(&b_to_a, b"MG2-ack", 60).await.unwrap();
    assert_eq!(
        recv(&mut a_rx, Duration::from_secs(5))
            .await
            .unwrap()
            .payload,
        b"MG2-ack"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
#[ignore]
async fn offline_receiver_gets_queued_frames_when_its_session_resumes() {
    let t = topic('A', 'B', "queued");
    let b_id = unique("b-persist");
    let (b, _rx) =
        RelayClient::start(&settings('B', "localhost", &b_id), set(&[&t]), set(&[])).unwrap();
    connected(&b).await;
    b.stop().await;

    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&a).await;
    a.publish(&t, b"while-offline", 600).await.unwrap();

    // Same client id: the broker resumes the session and delivers.
    let (b, mut b_rx) =
        RelayClient::start(&settings('B', "localhost", &b_id), set(&[&t]), set(&[])).unwrap();
    let got = recv(&mut b_rx, Duration::from_secs(10))
        .await
        .expect("queued frame");
    assert_eq!(got.payload, b"while-offline");
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
#[ignore]
async fn frames_are_not_retained() {
    let t = topic('A', 'B', "noretain");
    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&a).await;
    a.publish(&t, b"not-kept", 60).await.unwrap();
    // A brand-new session subscribing afterwards gets nothing.
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b-new")),
        set(&[&t]),
        set(&[]),
    )
    .unwrap();
    connected(&b).await;
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
#[ignore]
async fn acl_blocks_reading_other_inboxes_and_spoofing_the_sender() {
    let a_to_b = topic('A', 'B', "acl");
    // C tries to read B's inbox.
    let (c, mut c_rx) = RelayClient::start(
        &settings('C', "localhost", &unique("c")),
        set(&[&a_to_b]),
        set(&[]),
    )
    .unwrap();
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b")),
        set(&[&a_to_b]),
        set(&[]),
    )
    .unwrap();
    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&c).await;
    connected(&b).await;
    connected(&a).await;

    a.publish(&a_to_b, b"for-b-only", 60).await.unwrap();
    assert!(recv(&mut b_rx, Duration::from_secs(5)).await.is_some());
    assert!(recv(&mut c_rx, Duration::from_secs(2)).await.is_none());
    // Mosquitto 2 grants the SUBSCRIBE and filters on delivery instead,
    // so C sees no refusal here; brokers that refuse in the SUBACK show up
    // in refused_topics(). Either way C receives nothing.
    assert!(b.refused_topics().is_empty());

    // C publishing as if it were A (sender segment A) is refused.
    let r = c.publish(&a_to_b, b"spoofed", 60).await;
    assert!(matches!(r, Err(PublishError::Refused(_))), "{r:?}");
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    a.stop().await;
    b.stop().await;
    c.stop().await;
}

#[tokio::test]
#[ignore]
async fn removed_topics_stop_delivering() {
    let t = topic('A', 'B', "unsub");
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b")),
        set(&[&t]),
        set(&[]),
    )
    .unwrap();
    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&b).await;
    connected(&a).await;
    b.set_topics(set(&[])).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.publish(&t, b"after-revoke", 60).await.unwrap();
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
#[ignore]
async fn oversize_frames_are_dropped_and_the_client_keeps_working() {
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::{AsyncClient, MqttOptions};
    use rumqttc::{TlsConfiguration, Transport};

    let t = topic('A', 'B', "big");
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b")),
        set(&[&t]),
        set(&[]),
    )
    .unwrap();
    connected(&b).await;

    // A raw client (not ours, which refuses large frames) sends 600 bytes.
    let s = settings('A', "localhost", &unique("raw"));
    let mut o = MqttOptions::new(s.client_id.clone(), "localhost", s.address.port);
    o.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
        meshguard_mqtt::tls_config(s.ca_pem.as_deref()).unwrap(),
    )))
    .set_credentials(s.username.clone(), s.password.to_string());
    let (raw, mut el) = AsyncClient::new(o, 10);
    let pump = tokio::spawn(async move { while el.poll().await.is_ok() {} });
    raw.publish(t.clone(), QoS::AtLeastOnce, false, vec![7u8; 600])
        .await
        .unwrap();
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    raw.publish(t.clone(), QoS::AtLeastOnce, false, b"small".to_vec())
        .await
        .unwrap();
    assert_eq!(
        recv(&mut b_rx, Duration::from_secs(5))
            .await
            .unwrap()
            .payload,
        b"small"
    );
    assert_eq!(b.status(), RelayStatus::Connected);
    pump.abort();
    b.stop().await;

    // Our own client refuses to send them.
    let (a, _arx) = RelayClient::start(&s, set(&[]), set(&[])).unwrap();
    connected(&a).await;
    assert_eq!(
        a.publish(&t, &[0u8; 513], 60).await,
        Err(PublishError::TooLarge(513))
    );
    a.stop().await;
}

#[tokio::test]
#[ignore]
async fn wrong_password_is_auth_failed() {
    let mut s = settings('A', "localhost", &unique("a"));
    s.password = Zeroizing::new("wrong".into());
    let (a, _rx) = RelayClient::start(&s, set(&[]), set(&[])).unwrap();
    wait_status(&a, |s| *s == RelayStatus::AuthFailed).await;
    assert_eq!(
        a.publish("mg/r1/x/y/z", b"x", 60).await,
        Err(PublishError::Unavailable)
    );
}

#[tokio::test]
#[ignore]
async fn unknown_ca_fails_closed() {
    let mut s = settings('A', "localhost", &unique("a"));
    s.ca_pem = Some(ca("MG_MQTT_OTHER_CA"));
    let (a, _rx) = RelayClient::start(&s, set(&[]), set(&[])).unwrap();
    wait_status(&a, |s| matches!(s, RelayStatus::TlsError(_))).await;
}

#[tokio::test]
#[ignore]
async fn public_roots_do_not_trust_a_private_broker() {
    let mut s = settings('A', "localhost", &unique("a"));
    s.ca_pem = None;
    let (a, _rx) = RelayClient::start(&s, set(&[]), set(&[])).unwrap();
    wait_status(&a, |s| matches!(s, RelayStatus::TlsError(_))).await;
}

#[tokio::test]
#[ignore]
async fn hostname_mismatch_fails_closed() {
    // The certificate names "localhost" only.
    let (a, _rx) = RelayClient::start(
        &settings('A', "127.0.0.1", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    wait_status(&a, |s| matches!(s, RelayStatus::TlsError(_))).await;
}

#[tokio::test]
#[ignore]
async fn many_subscriptions_and_sequential_publishes_work() {
    // More topics than the request channel holds (16) must not stall.
    let topics: Vec<String> = (0..40)
        .map(|i| topic('A', 'B', &format!("many{i}")))
        .collect();
    let refs: Vec<&str> = topics.iter().map(String::as_str).collect();
    let (b, mut b_rx) = RelayClient::start(
        &settings('B', "localhost", &unique("b")),
        set(&refs),
        set(&[]),
    )
    .unwrap();
    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&b).await;
    connected(&a).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (i, t) in topics.iter().enumerate().rev().take(5) {
        // One at a time: each resolves on its own PUBACK.
        a.publish(t, format!("n{i}").as_bytes(), 60).await.unwrap();
        let got = recv(&mut b_rx, Duration::from_secs(5)).await.unwrap();
        assert_eq!(got.topic, *t);
    }
    assert!(b.refused_topics().is_empty());
    // Wildcards and foreign topics are refused before reaching the broker.
    assert!(b.set_topics(set(&["mg/r1/#"])).is_err());
    assert_eq!(
        a.publish("mg/r1/+/x/y", b"x", 60).await,
        Err(PublishError::BadTopic)
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
#[ignore]
async fn topics_revoked_while_offline_are_removed_from_the_session() {
    let t = topic('A', 'B', "revoked");
    let b_id = unique("b-revoke");
    let (b, _rx) =
        RelayClient::start(&settings('B', "localhost", &b_id), set(&[&t]), set(&[])).unwrap();
    connected(&b).await;
    b.stop().await;
    // Next run: the contact was revoked meanwhile. Same session id.
    let (b, mut b_rx) =
        RelayClient::start(&settings('B', "localhost", &b_id), set(&[]), set(&[&t])).unwrap();
    connected(&b).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b.pending_unsubscribes().is_empty(), "UNSUBACK confirmed");
    let (a, _arx) = RelayClient::start(
        &settings('A', "localhost", &unique("a")),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    connected(&a).await;
    a.publish(&t, b"after-revoke", 60).await.unwrap();
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    // Not queued in the session either: a reconnect delivers nothing.
    b.stop().await;
    let (b, mut b_rx) =
        RelayClient::start(&settings('B', "localhost", &b_id), set(&[]), set(&[])).unwrap();
    connected(&b).await;
    assert!(recv(&mut b_rx, Duration::from_secs(2)).await.is_none());
    a.stop().await;
    b.stop().await;
}
