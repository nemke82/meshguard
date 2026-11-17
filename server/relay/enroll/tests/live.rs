//! Live tests of the relay server kit: the patched Mosquitto with Dynamic
//! Security and meshguard-limits, and this enrollment service. Ignored by
//! default; `server/relay/test/run-tests.sh` starts both and runs them.

use std::collections::BTreeSet;
use std::time::Duration;

use meshguard_mqtt::{BrokerAddress, BrokerSettings, PublishError, RelayClient, RelayStatus};
use serde_json::{json, Value};
use zeroize::Zeroizing;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} not set; run server/relay/test/run-tests.sh"))
}

fn url(path: &str) -> String {
    format!("{}{path}", env("ENROLL_URL"))
}

fn random_user() -> (String, String) {
    const B32: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let r: [u8; 20] = rand::random();
    let user: String = r.iter().map(|b| B32[(*b % 32) as usize] as char).collect();
    (
        format!("mg-{user}"),
        hex::encode(rand::random::<[u8; 16]>()),
    )
}

fn work(nonce: &str, user: &str, sol: &str) -> u32 {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(format!("{nonce}{user}{sol}").as_bytes());
    let mut bits = 0;
    for b in d {
        if b == 0 {
            bits += 8;
        } else {
            bits += b.leading_zeros();
            break;
        }
    }
    bits
}

fn enroll(user: &str, pw: &str) -> (u16, Value) {
    let c: Value = ureq::get(&url("/v1/challenge"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    let nonce = c["nonce"].as_str().unwrap();
    let bits = c["difficulty"].as_u64().unwrap() as u32;
    let sol = (0u64..)
        .map(|n| n.to_string())
        .find(|s| work(nonce, user, s) >= bits)
        .unwrap();
    let r = ureq::post(&url("/v1/enroll"))
        .send_json(json!({"username": user, "password": pw, "nonce": nonce, "solution": sol}));
    match r {
        Ok(res) => (res.status(), res.into_json().unwrap()),
        Err(ureq::Error::Status(code, res)) => (code, res.into_json().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

fn delete(user: &str, pw: &str) -> (u16, Value) {
    let r = ureq::delete(&url("/v1/account")).send_json(json!({"username": user, "password": pw}));
    match r {
        Ok(res) => (res.status(), res.into_json().unwrap()),
        Err(ureq::Error::Status(code, res)) => (code, res.into_json().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

fn settings(user: &str, pw: &str, client_id: &str) -> BrokerSettings {
    BrokerSettings {
        address: BrokerAddress {
            host: "localhost".into(),
            port: env("MQTT_PORT").parse().unwrap(),
        },
        username: user.into(),
        password: Zeroizing::new(pw.into()),
        ca_pem: Some(std::fs::read_to_string(env("MQTT_CA")).unwrap()),
        client_id: client_id.into(),
        session_expiry_secs: 86_400,
    }
}

fn set(t: &[&str]) -> BTreeSet<String> {
    t.iter().map(|s| s.to_string()).collect()
}

async fn wait(c: &RelayClient, want: fn(&RelayStatus) -> bool) -> RelayStatus {
    let mut rx = c.watch_status();
    tokio::time::timeout(Duration::from_secs(15), async {
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
    .await
    .unwrap_or_else(|_| panic!("status stayed {:?}", c.status()))
}

async fn connected(c: &RelayClient) {
    wait(c, |s| *s == RelayStatus::Connected).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
#[ignore]
async fn enrolled_accounts_talk_only_through_their_own_inboxes() {
    let (a, pa) = random_user();
    let (b, pb) = random_user();
    assert_eq!(enroll(&a, &pa).1["status"], "created");
    assert_eq!(enroll(&b, &pb).1["status"], "created");
    let a_to_b = format!("mg/r1/{b}/{a}/x1");
    let (ca, _arx) = RelayClient::start(&settings(&a, &pa, &a), set(&[]), set(&[])).unwrap();
    let (cb, mut brx) =
        RelayClient::start(&settings(&b, &pb, &b), set(&[&a_to_b]), set(&[])).unwrap();
    connected(&ca).await;
    connected(&cb).await;
    ca.publish(&a_to_b, b"hello", 60).await.unwrap();
    let f = tokio::time::timeout(Duration::from_secs(5), brx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.payload, b"hello");
    // A cannot publish as B, nor into a topic outside the pattern.
    let spoof = format!("mg/r1/{a}/{b}/x1");
    assert!(matches!(
        ca.publish(&spoof, b"x", 60).await,
        Err(PublishError::Refused(_))
    ));
    ca.stop().await;
    cb.stop().await;
    delete(&a, &pa);
    delete(&b, &pb);
}

#[tokio::test]
#[ignore]
async fn an_account_works_only_with_its_own_client_id_and_password() {
    let (a, pa) = random_user();
    enroll(&a, &pa);
    let (c, _rx) =
        RelayClient::start(&settings(&a, &pa, "mg-other-id"), set(&[]), set(&[])).unwrap();
    wait(&c, |s| *s == RelayStatus::AuthFailed).await;
    let (c, _rx) = RelayClient::start(
        &settings(&a, "00000000000000000000000000000000", &a),
        set(&[]),
        set(&[]),
    )
    .unwrap();
    wait(&c, |s| *s == RelayStatus::AuthFailed).await;
    delete(&a, &pa);
}

#[tokio::test]
#[ignore]
async fn renewal_needs_the_password_and_delete_disconnects() {
    let (a, pa) = random_user();
    assert_eq!(enroll(&a, &pa).1["status"], "created");
    assert_eq!(enroll(&a, &pa).1["status"], "renewed");
    let (code, _) = enroll(&a, &hex::encode(rand::random::<[u8; 16]>()));
    assert_eq!(code, 409, "no takeover");
    let (c, _rx) = RelayClient::start(&settings(&a, &pa, &a), set(&[]), set(&[])).unwrap();
    connected(&c).await;
    assert_eq!(delete(&a, &pa).1["status"], "deleted");
    // Kicked, and it cannot come back.
    wait(&c, |s| !matches!(s, RelayStatus::Connected)).await;
    c.retry();
    wait(&c, |s| *s == RelayStatus::AuthFailed).await;
    assert_eq!(delete(&a, &pa).1["status"], "gone");
}

/// Raw MQTT 5 client, to look at properties and push limits.
async fn raw(user: &str, pw: &str) -> (rumqttc::v5::AsyncClient, rumqttc::v5::EventLoop) {
    let s = settings(user, pw, user);
    let mut o = rumqttc::v5::MqttOptions::new(user, "localhost", s.address.port);
    o.set_transport(rumqttc::Transport::tls_with_config(
        rumqttc::TlsConfiguration::Rustls(meshguard_mqtt::tls_config(s.ca_pem.as_deref()).unwrap()),
    ))
    .set_credentials(user, pw)
    .set_clean_start(false)
    .set_session_expiry_interval(Some(3600));
    rumqttc::v5::AsyncClient::new(o, 128)
}

#[tokio::test]
#[ignore]
async fn the_broker_caps_message_lifetime_at_24_hours() {
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::Event;
    let (a, pa) = random_user();
    let (b, pb) = random_user();
    enroll(&a, &pa);
    enroll(&b, &pb);
    let topic = format!("mg/r1/{b}/{a}/life");
    // B subscribes, then goes away (session kept).
    let (bc, mut bel) = raw(&b, &pb).await;
    bc.subscribe(&topic, rumqttc::v5::mqttbytes::QoS::AtLeastOnce)
        .await
        .unwrap();
    for _ in 0..20 {
        if matches!(bel.poll().await, Ok(Event::Incoming(Packet::SubAck(_)))) {
            break;
        }
    }
    bc.disconnect().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while bel.poll().await.is_ok() {}
    })
    .await;
    drop(bel);
    // A publishes with no expiry at all.
    let (ac, mut ael) = raw(&a, &pa).await;
    ac.publish(
        &topic,
        rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
        false,
        b"forever?".to_vec(),
    )
    .await
    .unwrap();
    for _ in 0..20 {
        if matches!(ael.poll().await, Ok(Event::Incoming(Packet::PubAck(_)))) {
            break;
        }
    }
    // B comes back: the queued copy carries an expiry of at most 24 h.
    let (_bc2, mut bel2) = raw(&b, &pb).await;
    let mut expiry = None;
    for _ in 0..30 {
        if let Ok(Event::Incoming(Packet::Publish(p))) = bel2.poll().await {
            expiry = Some(p.properties.and_then(|pr| pr.message_expiry_interval));
            break;
        }
    }
    let expiry = expiry
        .expect("queued message delivered")
        .expect("expiry set by broker");
    assert!(expiry <= 86_400 && expiry > 86_000, "{expiry}");
    delete(&a, &pa);
    delete(&b, &pb);
}

#[tokio::test]
#[ignore]
async fn per_client_limits_hold() {
    use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason};
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::Event;
    let (a, pa) = random_user();
    let (b, pb) = random_user();
    enroll(&a, &pa);
    enroll(&b, &pb);
    // Publish rate: a burst of 20, then refused.
    let (ac, mut ael) = raw(&a, &pa).await;
    let topic = format!("mg/r1/{b}/{a}/rate");
    for _ in 0..40 {
        ac.publish(&topic, QoS::AtLeastOnce, false, b"x".to_vec())
            .await
            .unwrap();
    }
    let (mut ok, mut refused) = (0, 0);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while ok + refused < 40 {
            if let Ok(Event::Incoming(Packet::PubAck(p))) = ael.poll().await {
                if p.reason == PubAckReason::Success
                    || p.reason == PubAckReason::NoMatchingSubscribers
                {
                    ok += 1;
                } else {
                    refused += 1;
                }
            }
        }
    })
    .await;
    assert!(refused >= 15, "ok {ok}, refused {refused}");
    // Subscriptions: the 65th distinct filter ends the connection.
    let (bc, mut bel) = raw(&b, &pb).await;
    for i in 0..65 {
        bc.subscribe(format!("mg/r1/{b}/s/{i}"), QoS::AtLeastOnce)
            .await
            .unwrap();
    }
    let mut subacks = 0;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bel.poll().await {
                Ok(Event::Incoming(Packet::SubAck(_))) => subacks += 1,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ended && subacks <= 64, "ended {ended}, subacks {subacks}");
    delete(&a, &pa);
    delete(&b, &pb);
}
