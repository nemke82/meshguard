//! Mosquitto Dynamic Security over the broker's local admin listener: the
//! [`Broker`] used in production.
//!
//! Commands go to `$CONTROL/dynamic-security/v1`; each carries a
//! `correlationData` that the answer on `.../response` echoes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::accounts::{Broker, BrokerFuture};

const TOPIC: &str = "$CONTROL/dynamic-security/v1";
const RESPONSE: &str = "$CONTROL/dynamic-security/v1/response";
const TIMEOUT: Duration = Duration::from_secs(10);
/// The role every relay account gets.
pub const ROLE: &str = "meshguard-user";

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

pub struct Dynsec {
    client: AsyncClient,
    waiters: Waiters,
}

impl Dynsec {
    /// Connect as the dynsec admin and start reading answers.
    pub async fn connect(host: &str, port: u16, user: &str, password: &str) -> Self {
        let mut opts = MqttOptions::new("meshguard-enroll", host, port);
        opts.set_credentials(user, password)
            .set_keep_alive(Duration::from_secs(30))
            .set_clean_session(true)
            // Account list pages are larger than rumqttc's 10 KiB default.
            .set_max_packet_size(256 * 1024, 64 * 1024);
        let (client, mut eventloop) = AsyncClient::new(opts, 32);
        let waiters: Waiters = Arc::default();
        let w = waiters.clone();
        let c = client.clone();
        // Commands are only sent once answers can be received.
        let (ready_tx, mut ready_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        let _ = ready_tx.send(false);
                        let _ = c.subscribe(RESPONSE, QoS::AtLeastOnce).await;
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {
                        let _ = ready_tx.send(true);
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) if p.topic == RESPONSE => {
                        let Ok(v) = serde_json::from_slice::<Value>(&p.payload) else {
                            continue;
                        };
                        for r in v["responses"].as_array().cloned().unwrap_or_default() {
                            if let Some(id) = r["correlationData"].as_str() {
                                if let Some(tx) = w.lock().unwrap().remove(id) {
                                    let _ = tx.send(r);
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("broker admin connection: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(30), ready_rx.wait_for(|r| *r)).await;
        Self { client, waiters }
    }

    /// Run one command; `Ok(response)` even when it carries an `error`.
    async fn command(&self, mut cmd: Value) -> Result<Value, String> {
        let id = hex::encode(rand::random::<[u8; 12]>());
        cmd["correlationData"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(id.clone(), tx);
        let payload = json!({ "commands": [cmd] }).to_string();
        if let Err(e) = self
            .client
            .publish(TOPIC, QoS::AtLeastOnce, false, payload)
            .await
        {
            self.waiters.lock().unwrap().remove(&id);
            return Err(e.to_string());
        }
        match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(v)) => Ok(v),
            _ => {
                self.waiters.lock().unwrap().remove(&id);
                Err("no answer from the broker".into())
            }
        }
    }

    async fn ok(&self, cmd: Value, tolerated: &[&str]) -> Result<Value, String> {
        let r = self.command(cmd).await?;
        match r["error"].as_str() {
            None => Ok(r),
            Some(e) if tolerated.contains(&e) => Ok(r),
            Some(e) => Err(e.to_string()),
        }
    }

    /// Default-deny access, the relay role with its rules (ADR-006 §5,
    /// ADR-007 §2), and no generated demo client. Idempotent.
    pub async fn setup(&self) -> Result<(), String> {
        self.ok(
            json!({"command": "setDefaultACLAccess", "acls": [
                {"acltype": "publishClientSend", "allow": false},
                {"acltype": "publishClientReceive", "allow": false},
                {"acltype": "subscribe", "allow": false},
                {"acltype": "unsubscribe", "allow": false}
            ]}),
            &[],
        )
        .await?;
        self.ok(
            json!({"command": "createRole", "rolename": ROLE}),
            &["Role already exists"],
        )
        .await?;
        for (acltype, topic) in [
            ("subscribePattern", "mg/r1/%u/#"),
            ("unsubscribePattern", "mg/r1/%u/#"),
            ("publishClientReceive", "mg/r1/%u/#"),
            ("publishClientSend", "mg/r1/+/%u/#"),
        ] {
            self.ok(
                json!({"command": "addRoleACL", "rolename": ROLE, "acltype": acltype,
                       "topic": topic, "priority": 0, "allow": true}),
                &["ACL with this topic already exists"],
            )
            .await?;
        }
        // The plugin's generated config has a demo "user" client.
        self.ok(
            json!({"command": "deleteClient", "username": "user"}),
            &["Client not found"],
        )
        .await?;
        Ok(())
    }
}

impl Broker for Dynsec {
    fn create<'a>(&'a self, username: &'a str, password: &'a str) -> BrokerFuture<'a, ()> {
        Box::pin(async move {
            self.ok(
                json!({"command": "createClient", "username": username, "password": password,
                       "clientid": username, "roles": [{"rolename": ROLE}]}),
                &[],
            )
            .await
            .map(|_| ())
        })
    }

    fn delete<'a>(&'a self, username: &'a str) -> BrokerFuture<'a, ()> {
        Box::pin(async move {
            self.ok(
                json!({"command": "deleteClient", "username": username}),
                &["Client not found"],
            )
            .await
            .map(|_| ())
        })
    }

    fn list(&self) -> BrokerFuture<'_, Vec<String>> {
        Box::pin(async move {
            // In pages, so each answer stays small whatever the count.
            const PAGE: usize = 500;
            let mut all = Vec::new();
            loop {
                let r = self
                    .ok(
                        json!({"command": "listClients", "verbose": false,
                               "count": PAGE, "offset": all.len()}),
                        &[],
                    )
                    .await?;
                let page: Vec<String> = r["data"]["clients"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.as_str().or_else(|| c["username"].as_str()))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let n = page.len();
                all.extend(page);
                if n < PAGE {
                    return Ok(all);
                }
            }
        })
    }
}
