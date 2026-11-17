//! Two engines talking over the relay end to end (ADR-006 §6, §8): the
//! offer/accept exchange over LoRa, then messages and ACKs over MQTT
//! topics, shared dedup with LoRa, and what a broker cannot do.

use meshguard_core::crypto::SessionKey;
use meshguard_core::delivery::DeliveryState;
use meshtastic::protobufs;

use crate::config::{PeerConfig, RelayPreference, RelaySettings, WireProtocol};
use crate::outbox::{pick_route, Attempt, Content, RouteCounts, Routes};
use crate::receive::{Action, Event, Route};
use crate::Engine;

const A: u32 = 0x11;
const B: u32 = 0x22;
const NOW: u64 = 1_763_200_000;

fn settings(user: &str) -> RelaySettings {
    RelaySettings {
        enabled: true,
        host: "broker.example".into(),
        port: 8883,
        username: user.into(),
        ca_pem: None,
        client_id: "mg-test".into(),
        session_expiry_secs: 86_400,
        preference: RelayPreference::Auto,
        managed: false,
        renewed_at: 0,
        renew_retry_at: 0,
    }
}

fn engine(name: &str, me: u32, peer: u32, user: &str) -> Engine {
    let dir = std::env::temp_dir().join(format!("mg-relayflow-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut e = Engine::new(dir, NOW);
    e.config.peers.push(PeerConfig {
        id: "p".into(),
        device_name: "Peer".into(),
        node_num: peer,
        protocol: WireProtocol::V2,
        features: meshguard_core::relay::FEATURE_RELAY,
        confirmed: true,
    });
    e.adopt_keys(me, vec![(peer, SessionKey::from_bytes([7; 32]))]);
    e.open_history(Some(&SessionKey::from_bytes([me as u8; 32])));
    e.my_node_num = Some(me);
    e.config.relay = Some(settings(user));
    e
}

fn packet(from: u32, to: u32, payload: Vec<u8>) -> protobufs::MeshPacket {
    protobufs::MeshPacket {
        from,
        to,
        payload_variant: Some(protobufs::mesh_packet::PayloadVariant::Decoded(
            protobufs::Data {
                portnum: protobufs::PortNum::PrivateApp as i32,
                payload,
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

/// Deliver `from`'s due relay controls to `to` over "LoRa", and the ACKs
/// back.
fn lora_controls(from: &mut Engine, from_node: u32, to: &mut Engine, to_node: u32) {
    for c in from.relay_due_controls(NOW) {
        let actions = to.on_mesh_packet(&packet(from_node, to_node, c.frame), to_node, NOW);
        for a in actions {
            if let Action::SendAck { frame, route, .. } = a {
                assert_eq!(route, Route::Radio);
                from.on_mesh_packet(&packet(to_node, from_node, frame), from_node, NOW);
            }
        }
    }
}

fn paired(test: &str) -> (Engine, Engine) {
    let mut a = engine(&format!("{test}-a"), A, B, "mg-aaaaaaaaaaaaaaaaaaaa");
    let mut b = engine(&format!("{test}-b"), B, A, "mg-bbbbbbbbbbbbbbbbbbbb");
    a.relay_enable(B, NOW).unwrap();
    lora_controls(&mut a, A, &mut b, B);
    b.relay_enable(A, NOW).unwrap(); // accepts the offer
    lora_controls(&mut b, B, &mut a, A);
    assert!(a.relay_outbound_topic(B).is_some());
    assert!(a.relay.outgoing.is_empty() && b.relay.outgoing.is_empty());
    (a, b)
}

fn relay_only() -> Routes {
    Routes {
        radio: None,
        relay: true,
    }
}

fn incoming_texts(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::Emit(Event::IncomingMessage(_))))
        .count()
}

#[test]
fn a_message_and_its_ack_travel_over_the_relay() {
    let (mut a, mut b) = paired("t1");
    let t = a
        .enqueue(
            B,
            &Content::Text("over the internet".into()),
            NOW,
            relay_only(),
        )
        .unwrap();
    assert_eq!(t.route, Route::Relay);
    let topic = t.topic.clone().unwrap();
    assert_eq!(b.relay_inbound_topics().get(&topic), Some(&A));

    // PUBACK: sent to broker, not delivered.
    let (st, ev) = a.finish_attempt(&t, Attempt::Submitted, NOW);
    assert_eq!(st, Some(DeliveryState::Submitted));
    assert_eq!(ev.unwrap().via, Some("relay"));

    let actions = b.on_relay_frame(&topic, &t.frame, NOW);
    assert_eq!(incoming_texts(&actions), 1);
    let ack = actions
        .iter()
        .find_map(|a| match a {
            Action::SendAck { frame, route, .. } => {
                assert_eq!(*route, Route::Relay, "ACK goes back the way it came");
                Some(frame.clone())
            }
            _ => None,
        })
        .unwrap();
    // B publishes the ACK on its outbound topic, which is A's inbound one.
    let back = b.relay_outbound_topic(A).unwrap();
    let actions = a.on_relay_frame(&back, &ack, NOW);
    let acked = actions.iter().any(
        |x| matches!(x, Action::PeerAck { peer: B, message_id } if *message_id == t.message_id),
    );
    assert!(acked, "{actions:?}");
}

#[test]
fn copies_over_lora_and_the_relay_show_once() {
    let (mut a, mut b) = paired("t2");
    let t = a
        .enqueue(B, &Content::Text("both ways".into()), NOW, relay_only())
        .unwrap();
    let topic = t.topic.clone().unwrap();
    let first = b.on_relay_frame(&topic, &t.frame, NOW);
    assert_eq!(incoming_texts(&first), 1);
    // The same message (same id) arrives over LoRa: no second entry.
    let lora = b.on_mesh_packet(&packet(A, B, t.frame.clone()), B, NOW);
    assert_eq!(incoming_texts(&lora), 0);
    // And the broker replaying it many times: shown once, ACKs bounded.
    let mut acks = 0;
    for _ in 0..10 {
        let r = b.on_relay_frame(&topic, &t.frame, NOW);
        assert_eq!(incoming_texts(&r), 0);
        acks += r
            .iter()
            .filter(|x| matches!(x, Action::SendAck { .. }))
            .count();
    }
    assert!(acks <= 3, "{acks}");
    // The LoRa allowance is untouched by the relay replays.
    let lora = b.on_mesh_packet(&packet(A, B, t.frame.clone()), B, NOW);
    assert!(lora.iter().any(|x| matches!(
        x,
        Action::SendAck {
            route: Route::Radio,
            ..
        }
    )));
}

#[test]
fn the_broker_cannot_inject_or_redirect() {
    let (mut a, mut b) = paired("t3");
    let t = a
        .enqueue(B, &Content::Text("x".into()), NOW, relay_only())
        .unwrap();
    let topic = t.topic.clone().unwrap();
    // Unknown topic, oversize, non-MG2, tampered: all dropped, nothing shown.
    assert!(b.on_relay_frame("mg/r1/x/y/z", &t.frame, NOW).is_empty());
    assert!(b.on_relay_frame(&topic, &[0u8; 600], NOW).is_empty());
    assert!(b.on_relay_frame(&topic, b"hello", NOW).is_empty());
    let mut bad = t.frame.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    let r = b.on_relay_frame(&topic, &bad, NOW);
    assert_eq!(incoming_texts(&r), 0);
    assert!(!r.iter().any(|x| matches!(x, Action::Undecryptable { .. })));
    // A's own frame reflected back to A on its inbound topic fails the
    // direction-bound AAD.
    let mine = a.relay_inbound_topics().keys().next().unwrap().clone();
    assert_eq!(incoming_texts(&a.on_relay_frame(&mine, &t.frame, NOW)), 0);
    // Once B blocks A, A's frames on the relay are ignored and the topic
    // is gone.
    b.config.blocked.push(crate::config::BlockedNode {
        node_num: A,
        name: "A".into(),
        blocked_at: 0,
    });
    b.relay_forget(A, NOW);
    assert!(b.relay_inbound_topics().is_empty());
    assert!(b.on_relay_frame(&topic, &t.frame, NOW).is_empty());
}

#[test]
fn routing_follows_the_preference_and_what_is_up() {
    use RelayPreference::*;
    let c = |radio, relay, last| RouteCounts { radio, relay, last };
    // Nothing up: wait. One up: that one.
    assert_eq!(pick_route(Auto, c(0, 0, None), false, false), None);
    assert_eq!(
        pick_route(Auto, c(0, 0, None), true, false),
        Some(Route::Radio)
    );
    assert_eq!(
        pick_route(Auto, c(0, 0, None), false, true),
        Some(Route::Relay)
    );
    // Auto alternates, relay first.
    assert_eq!(
        pick_route(Auto, c(0, 0, None), true, true),
        Some(Route::Relay)
    );
    assert_eq!(
        pick_route(Auto, c(0, 1, Some(Route::Relay)), true, true),
        Some(Route::Radio)
    );
    assert_eq!(
        pick_route(Auto, c(1, 1, Some(Route::Radio)), true, true),
        Some(Route::Relay)
    );
    // LoRa preferred: two LoRa attempts, then the relay, then LoRa again;
    // a radio that comes back after relay attempts gets its turn.
    assert_eq!(
        pick_route(LoraPreferred, c(0, 0, None), true, true),
        Some(Route::Radio)
    );
    assert_eq!(
        pick_route(LoraPreferred, c(2, 0, Some(Route::Radio)), true, true),
        Some(Route::Relay)
    );
    assert_eq!(
        pick_route(LoraPreferred, c(2, 1, Some(Route::Relay)), true, true),
        Some(Route::Radio)
    );
    assert_eq!(
        pick_route(LoraPreferred, c(0, 3, Some(Route::Relay)), true, true),
        Some(Route::Radio)
    );
}

#[test]
fn with_the_relay_off_messages_use_lora_only() {
    let (mut a, _b) = paired("t4");
    a.config.relay.as_mut().unwrap().enabled = false;
    let t = a
        .enqueue(
            B,
            &Content::Text("x".into()),
            NOW,
            Routes {
                radio: Some(A),
                relay: true,
            },
        )
        .unwrap();
    assert_eq!(t.route, Route::Radio);
    assert!(t.topic.is_none());
}

#[test]
fn a_revoke_and_its_ack_travel_over_the_relay_alone() {
    let (mut a, mut b) = paired("t5");
    a.relay_disable(B, NOW).unwrap();
    assert!(a.relay_outbound_topic(B).is_none(), "routing state is gone");
    let c = a.relay_due_controls(NOW).pop().expect("revoke queued");
    let topic = c.topic.clone().expect("revoke keeps its relay topic");
    // B receives it on its inbound topic, ends the relay, and the ACK still
    // has somewhere to go.
    let actions = b.on_relay_frame(&topic, &c.frame, NOW);
    assert!(b.relay_outbound_topic(A).is_none());
    let reply = actions
        .iter()
        .find_map(|x| match x {
            Action::SendAck {
                route: Route::Relay,
                reply,
                ..
            } => reply.clone(),
            _ => None,
        })
        .expect("ACK with its reply topic");
    assert!(b.relay_reply_valid(A, &reply));
    // That ACK is lost; A retries the revoke on the same topic. B, whose
    // relay with A has ended, still answers it on the retiring topic, and
    // A still receives the answer on its own retiring topic.
    let retry = a
        .relay_due_controls(NOW + 3600)
        .pop()
        .expect("revoke retried");
    let actions = b.on_relay_frame(&topic, &retry.frame, NOW + 3600);
    let ack = actions
        .iter()
        .find_map(|x| match x {
            Action::SendAck {
                frame,
                reply: Some(r),
                ..
            } => Some((frame.clone(), r.topic.clone())),
            _ => None,
        })
        .expect("duplicate revoke acknowledged again");
    let back = a.on_relay_frame(&ack.1, &ack.0, NOW + 3600);
    assert!(back.is_empty() || !back.iter().any(|x| matches!(x, Action::Emit(_))));
    assert!(a.relay.outgoing.is_empty(), "revoke completed by its ACK");
    assert!(a.relay.retiring.is_empty(), "and its topics retired");
    // A message on a retiring topic is not accepted.
    // Blocking A in between makes that ACK stale.
    b.config.send_generations.insert(A, 9);
    assert!(!b.relay_reply_valid(A, &reply));
}

#[test]
fn a_frame_refused_on_a_retiring_topic_still_counts_over_lora() {
    let (mut a, mut b) = paired("t6");
    a.relay_disable(B, NOW).unwrap();
    let c = a.relay_due_controls(NOW).pop().unwrap();
    let topic = c.topic.clone().unwrap();
    b.on_relay_frame(&topic, &c.frame, NOW);
    // A Caps frame (not allowed on an ended relay) arrives there first...
    let caps = meshguard_core::outbound::seal_caps(
        meshguard_core::status::SUPPORTED_FEATURES,
        &SessionKey::from_bytes([7; 32]),
        &meshguard_core::PeerIdentity::meshtastic_node(A),
        &meshguard_core::PeerIdentity::meshtastic_node(B),
        NOW as u32,
        meshguard_core::envelope::DEFAULT_FRAME_BUDGET,
    )
    .unwrap()
    .frame;
    assert!(b.on_relay_frame(&topic, &caps, NOW).is_empty());
    // ...and the same frame over LoRa is handled and acknowledged.
    let lora = b.on_mesh_packet(&packet(A, B, caps), B, NOW);
    assert!(lora.iter().any(|x| matches!(
        x,
        Action::SendAck {
            route: Route::Radio,
            ..
        }
    )));
}

#[test]
fn an_interrupted_settings_change_is_finished_on_load() {
    use crate::relay::PendingSettings;
    let (mut a, _b) = paired("t7");
    // As if the process stopped after the book (credentials + intended
    // settings) was saved but before the settings file was written.
    let mut intended = a.config.relay.clone().unwrap();
    intended.username = "mg-zzzzzzzzzzzzzzzzzzzz".into();
    a.relay.password = Some("new".into());
    a.relay.pending_settings = Some(PendingSettings {
        settings: Some(intended.clone()),
    });
    a.save_relay().unwrap();
    let mut reopened = Engine::new(a.config_dir.clone(), NOW);
    reopened.open_history(Some(&SessionKey::from_bytes([A as u8; 32])));
    assert_eq!(reopened.config.relay.as_ref(), Some(&intended));
    assert!(reopened.relay.pending_settings.is_none());
    assert_eq!(reopened.relay.password.as_deref(), Some("new"));
    // And the settings file itself was written.
    let from_disk = crate::config::AppConfig::load(&a.config_dir);
    assert_eq!(from_disk.relay.as_ref(), Some(&intended));
}
