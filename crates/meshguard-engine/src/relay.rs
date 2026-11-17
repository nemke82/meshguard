//! Private MQTT relay: per-contact exchange state, retries and topics
//! (ADR-006 §4–§5), without I/O.
//!
//! [`RelayBook`] is the whole secret state: the broker password, the relay
//! secret of each contact and where its exchange stands, tombstones of
//! revoked generations, control messages still to be (re)sent, and topics
//! the broker session may still hold. It is persisted sealed with the vault
//! key ([`seal_book`]); every change is saved *before* the frame that
//! caused it is acknowledged, or before the frame it produces is sent.
//!
//! The book never sends anything itself: it returns what changed, and the
//! engine seals and routes the control messages it lists.

use std::collections::{BTreeMap, BTreeSet};

use meshguard_core::crypto::SessionKey;
use meshguard_core::relay::{
    confirm_matches, is_app_username, RelayAccept, RelayControl, RelayOffer, RelayRevoke,
    RelaySecret, BROKER_TAG_LEN, CONFIRM_LEN, SECRET_LEN,
};
use meshguard_core::MessageId;
use serde::{Deserialize, Serialize};

use crate::error::MeshGuardError;

/// File holding the sealed book, next to `meshguard.json`.
pub const BOOK_FILE: &str = "relay_secrets.json";
const BOOK_AAD: &[u8] = b"meshguard-relay-book-v1";
/// Retries of a control message: first after a minute, doubling, at most
/// an hour apart; at most this many attempts.
pub const CONTROL_MAX_ATTEMPTS: u32 = 8;
const CONTROL_FIRST_RETRY: u64 = 60;
const CONTROL_MAX_RETRY: u64 = 3600;
/// A control message is not retried after its TTL (24 h, minus margin).
const CONTROL_LIFETIME: u64 = 24 * 3600 - 600;

// ── Stored forms ───────────────────────────────────────────────

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode hex byte pairs; `None` for odd length or any non-hex byte.
fn unhex_vec(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    b.chunks(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    unhex_vec(s)?.try_into().ok()
}

/// A relay secret as stored (hex, inside the sealed book only).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StoredSecret(String);

impl StoredSecret {
    fn new(s: &RelaySecret) -> Self {
        Self(hex(s.as_bytes()))
    }

    pub fn secret(&self) -> Option<RelaySecret> {
        unhex::<SECRET_LEN>(&self.0).map(RelaySecret::from_bytes)
    }

    fn confirm(&self) -> Option<[u8; CONFIRM_LEN]> {
        self.secret().map(|s| s.confirm_tag())
    }
}

impl std::fmt::Debug for StoredSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<secret>")
    }
}

/// Where the exchange with one contact stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContactRelay {
    /// We offered; waiting for the accept.
    Offered { secret: StoredSecret },
    /// The peer offered; waiting for our user.
    Incoming {
        secret: StoredSecret,
        peer_inbox: String,
        peer_tag: String,
    },
    /// Both sides use the relay. `rotation`: a newer offer from the peer
    /// waiting for our user (the active secret stays until then).
    Active {
        secret: StoredSecret,
        peer_inbox: String,
        #[serde(default)]
        rotation: Option<Box<ContactRelay>>,
    },
}

impl ContactRelay {
    fn secret(&self) -> &StoredSecret {
        match self {
            Self::Offered { secret }
            | Self::Incoming { secret, .. }
            | Self::Active { secret, .. } => secret,
        }
    }
}

/// A control message to (re)send until its AppAck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingControl {
    pub my_node: u32,
    pub peer: u32,
    /// For a revoke of an active relay: the topic it can still be published
    /// on over the relay after the contact's routing state is gone.
    #[serde(default)]
    pub topic: Option<String>,
    /// Send generation of the peer when queued (stamped by the engine).
    #[serde(default)]
    pub gen: Option<u64>,
    pub message_id: String,
    pub control: StoredControl,
    pub created_at: u64,
    pub attempts: u32,
    pub next_at: u64,
}

impl PendingControl {
    pub fn id(&self) -> Option<MessageId> {
        unhex::<16>(&self.message_id).map(MessageId::from_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredControl {
    Offer {
        secret: StoredSecret,
        tag: String,
        inbox: String,
    },
    Accept {
        confirm: String,
        tag: String,
        inbox: String,
    },
    Revoke {
        confirm: String,
    },
}

impl StoredControl {
    /// The wire form, or `None` if the stored data is damaged.
    pub fn control(&self) -> Option<RelayControl> {
        Some(match self {
            Self::Offer { secret, tag, inbox } => RelayControl::Offer(RelayOffer {
                secret: secret.secret()?,
                broker_tag: unhex::<BROKER_TAG_LEN>(tag)?,
                inbox: inbox.clone(),
            }),
            Self::Accept {
                confirm,
                tag,
                inbox,
            } => RelayControl::Accept(RelayAccept {
                confirm: unhex::<CONFIRM_LEN>(confirm)?,
                broker_tag: unhex::<BROKER_TAG_LEN>(tag)?,
                inbox: inbox.clone(),
            }),
            Self::Revoke { confirm } => RelayControl::Revoke(RelayRevoke {
                confirm: unhex::<CONFIRM_LEN>(confirm)?,
            }),
        })
    }
}

/// The topics of a relay that ended, while its revoke completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retiring {
    pub inbound: String,
    pub outbound: String,
    pub until: u64,
    /// Send generation of the peer (stamped by the engine).
    #[serde(default)]
    pub gen: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub confirm: String,
    pub until: u64,
}

/// The secret relay state (sealed at rest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayBook {
    /// Broker password.
    #[serde(default)]
    pub password: Option<String>,
    /// By `"<my_node hex>:<peer hex>"`.
    #[serde(default)]
    pub contacts: BTreeMap<String, ContactRelay>,
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    #[serde(default)]
    pub outgoing: Vec<PendingControl>,
    /// Topics the broker session may still hold (revoked while the relay
    /// was not connected); handed to the client at start.
    #[serde(default)]
    pub unsubscribe: BTreeSet<String>,
    /// Topics of relays that just ended (revoked), kept subscribed for the
    /// lifetime of the revoke so a lost ACK can be repeated and received;
    /// only control receipts are accepted on them.
    #[serde(default)]
    pub retiring: BTreeMap<String, Retiring>,
    /// Send generation of the peer (`config.send_generations`) each
    /// contact entry was made under. Blocking or removing raises it
    /// durably, so an entry that outlived a failed save is ignored.
    #[serde(default)]
    pub generations: BTreeMap<String, u64>,
    /// MeshGuard relay accounts to delete on the server (ADR-007 §6): each
    /// is kept until the server confirms, so deletion survives being
    /// offline and a later account never replaces an earlier one here.
    #[serde(default)]
    pub pending_deletes: Vec<PendingDelete>,
    /// Write-ahead record of a relay settings change: saved atomically with
    /// the credentials it belongs to, applied to the settings file, then
    /// cleared. Replayed at load if the process stopped in between.
    #[serde(default)]
    pub pending_settings: Option<PendingSettings>,
}

/// The relay settings a transaction is installing (`None`: none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSettings {
    pub settings: Option<crate::config::RelaySettings>,
}

/// Credentials of a managed relay account awaiting deletion.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDelete {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for PendingDelete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingDelete(<redacted>)")
    }
}

fn contact_key(my_node: u32, peer: u32) -> String {
    format!("{my_node:08x}:{peer:08x}")
}

fn parse_contact_key(k: &str) -> Option<(u32, u32)> {
    let (a, b) = k.split_once(':')?;
    Some((
        u32::from_str_radix(a, 16).ok()?,
        u32::from_str_radix(b, 16).ok()?,
    ))
}

// ── Context ────────────────────────────────────────────────────

/// What the book needs to know about this device's relay settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayIdentity {
    /// Our broker username (inbox).
    pub user: String,
    pub broker_tag: [u8; BROKER_TAG_LEN],
}

/// What happened to a received control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    /// Applied or deliberately ignored: acknowledge it.
    Handled,
    /// The peer offered the relay: ask the user (unless auto-accepted).
    OfferReceived { same_broker: bool },
    /// The relay with this contact is now active.
    Activated,
    /// The relay with this contact ended.
    Revoked,
}

/// Why a user action was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayRefusal {
    NotConfigured,
    DifferentBroker,
    NothingToAccept,
    AlreadyActive,
}

impl std::fmt::Display for RelayRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotConfigured => "Set up internet relay (Private MQTT) in Settings first",
            Self::DifferentBroker => "Your contact uses a different MQTT broker",
            Self::NothingToAccept => "There is no relay offer from this contact",
            Self::AlreadyActive => "Internet relay is already on for this contact",
        })
    }
}

impl RelayBook {
    fn contact(&self, my_node: u32, peer: u32) -> Option<&ContactRelay> {
        self.contacts.get(&contact_key(my_node, peer))
    }

    /// The exchange state with `peer`, for the UI.
    pub fn state(&self, my_node: u32, peer: u32) -> Option<&ContactRelay> {
        self.contact(my_node, peer)
    }

    fn tombstoned(&self, confirm: &[u8; CONFIRM_LEN], now: u64) -> bool {
        self.tombstones.iter().any(|t| {
            t.until >= now
                && unhex::<CONFIRM_LEN>(&t.confirm).is_some_and(|c| confirm_matches(&c, confirm))
        })
    }

    fn tombstone(&mut self, confirm: [u8; CONFIRM_LEN], until: u64) {
        let c = hex(&confirm);
        if let Some(t) = self.tombstones.iter_mut().find(|t| t.confirm == c) {
            t.until = t.until.max(until);
        } else {
            self.tombstones.push(Tombstone { confirm: c, until });
        }
    }

    fn tombstone_secret(&mut self, s: &StoredSecret, until: u64) {
        if let Some(c) = s.confirm() {
            self.tombstone(c, until);
        }
    }

    /// Forget expired tombstones (`until` is inclusive).
    pub fn prune(&mut self, now: u64) {
        self.tombstones.retain(|t| t.until >= now);
    }

    fn drop_outgoing(&mut self, my_node: u32, peer: u32, kind: Option<fn(&StoredControl) -> bool>) {
        self.outgoing.retain(|p| {
            !(p.my_node == my_node && p.peer == peer && kind.is_none_or(|k| k(&p.control)))
        });
    }

    fn queue(&mut self, my_node: u32, peer: u32, control: StoredControl, now: u64) {
        self.outgoing.push(PendingControl {
            my_node,
            peer,
            topic: None,
            gen: None,
            message_id: MessageId::random().to_hex(),
            control,
            created_at: now,
            attempts: 0,
            next_at: now,
        });
    }

    /// Topics of an active contact: (inbound, outbound).
    pub fn topics_of(
        &self,
        my_node: u32,
        peer: u32,
        me: &RelayIdentity,
    ) -> Option<(String, String)> {
        let ContactRelay::Active {
            secret, peer_inbox, ..
        } = self.contact(my_node, peer)?
        else {
            return None;
        };
        let s = secret.secret()?;
        let inbound = s.topic(peer, my_node, peer_inbox, &me.user).ok()?;
        let outbound = s.topic(my_node, peer, &me.user, peer_inbox).ok()?;
        Some((inbound, outbound))
    }

    /// Every inbound topic of radio `my_node`, mapped to its peer.
    pub fn inbound_topics(&self, my_node: u32, me: &RelayIdentity) -> BTreeMap<String, u32> {
        self.contacts
            .keys()
            .filter_map(|k| parse_contact_key(k))
            .filter(|(m, _)| *m == my_node)
            .filter_map(|(m, p)| self.topics_of(m, p, me).map(|(inb, _)| (inb, p)))
            .collect()
    }

    /// Remember that the topics of `peer` must leave the broker session.
    fn note_unsubscribe(&mut self, my_node: u32, peer: u32, me: Option<&RelayIdentity>) {
        if let Some((inbound, _)) = me.and_then(|me| self.topics_of(my_node, peer, me)) {
            self.unsubscribe.insert(inbound);
        }
    }

    // ── User actions ───────────────────────────────────────────

    /// Offer the relay to `peer` (the caller checked the contact is paired,
    /// v2 and announces `FEATURE_RELAY`). Accepting a pending offer instead
    /// when there is one.
    pub fn enable(
        &mut self,
        my_node: u32,
        peer: u32,
        me: Option<&RelayIdentity>,
        now: u64,
    ) -> Result<ControlOutcome, RelayRefusal> {
        let me = me.ok_or(RelayRefusal::NotConfigured)?;
        match self.contact(my_node, peer) {
            Some(ContactRelay::Incoming { .. }) => {
                return self.accept(my_node, peer, Some(me), now)
            }
            Some(ContactRelay::Active {
                rotation: Some(_), ..
            }) => return self.accept(my_node, peer, Some(me), now),
            Some(ContactRelay::Active { .. }) => return Err(RelayRefusal::AlreadyActive),
            Some(ContactRelay::Offered { .. }) => return Ok(ControlOutcome::Handled),
            None => {}
        }
        let secret = RelaySecret::random();
        let stored = StoredSecret::new(&secret);
        self.contacts.insert(
            contact_key(my_node, peer),
            ContactRelay::Offered {
                secret: stored.clone(),
            },
        );
        self.queue(
            my_node,
            peer,
            StoredControl::Offer {
                secret: stored,
                tag: hex(&me.broker_tag),
                inbox: me.user.clone(),
            },
            now,
        );
        Ok(ControlOutcome::Handled)
    }

    /// Accept the pending offer of `peer` (or its pending rotation).
    pub fn accept(
        &mut self,
        my_node: u32,
        peer: u32,
        me: Option<&RelayIdentity>,
        now: u64,
    ) -> Result<ControlOutcome, RelayRefusal> {
        let me = me.ok_or(RelayRefusal::NotConfigured)?;
        let key = contact_key(my_node, peer);
        let (offer, old) = match self.contacts.get(&key) {
            Some(c @ ContactRelay::Incoming { .. }) => (c.clone(), None),
            Some(ContactRelay::Active {
                rotation: Some(r),
                secret,
                ..
            }) => ((**r).clone(), Some(secret.clone())),
            _ => return Err(RelayRefusal::NothingToAccept),
        };
        let ContactRelay::Incoming {
            secret,
            peer_inbox,
            peer_tag,
        } = offer
        else {
            return Err(RelayRefusal::NothingToAccept);
        };
        if unhex::<BROKER_TAG_LEN>(&peer_tag) != Some(me.broker_tag) {
            return Err(RelayRefusal::DifferentBroker);
        }
        let until = now + crate::relay::tombstone_secs();
        if let Some(old) = old {
            self.note_unsubscribe(my_node, peer, Some(me));
            self.tombstone_secret(&old, until);
        }
        let confirm = secret.confirm().ok_or(RelayRefusal::NothingToAccept)?;
        self.contacts.insert(
            key,
            ContactRelay::Active {
                secret,
                peer_inbox,
                rotation: None,
            },
        );
        // A topic accepted again must not be removed from the session.
        if let Some((inbound, _)) = self.topics_of(my_node, peer, me) {
            self.unsubscribe.remove(&inbound);
        }
        self.retiring.remove(&contact_key(my_node, peer));
        self.drop_outgoing(
            my_node,
            peer,
            Some(|c| matches!(c, StoredControl::Accept { .. })),
        );
        self.queue(
            my_node,
            peer,
            StoredControl::Accept {
                confirm: hex(&confirm),
                tag: hex(&me.broker_tag),
                inbox: me.user.clone(),
            },
            now,
        );
        Ok(ControlOutcome::Activated)
    }

    /// Turn the relay off for `peer` (or decline / cancel an offer) and
    /// tell the peer.
    pub fn disable(
        &mut self,
        my_node: u32,
        peer: u32,
        me: Option<&RelayIdentity>,
        now: u64,
    ) -> bool {
        let Some(c) = self.contact(my_node, peer).cloned() else {
            return false;
        };
        // Captured before the state goes: the revoke can still be sent
        // over the relay (MQTT-only contacts).
        let out_topic = me
            .and_then(|m| self.topics_of(my_node, peer, m))
            .map(|(_, out)| out);
        self.retire(my_node, peer, me, now);
        let until = now + tombstone_secs();
        let mut confirms = vec![c.secret().clone()];
        if let ContactRelay::Active {
            rotation: Some(r), ..
        } = &c
        {
            confirms.push(r.secret().clone());
        }
        for s in confirms {
            self.tombstone_secret(&s, until);
            if let Some(confirm) = s.confirm() {
                self.queue(
                    my_node,
                    peer,
                    StoredControl::Revoke {
                        confirm: hex(&confirm),
                    },
                    now,
                );
                if let Some(p) = self.outgoing.last_mut() {
                    p.topic = out_topic.clone();
                }
            }
        }
        true
    }

    /// Remove the relay altogether (settings removed): every contact of
    /// every local radio ends. Those of radio `owner` are told (revoke, if a
    /// transport for them comes up); others are only forgotten. The broker
    /// password is wiped.
    pub fn remove_all(&mut self, owner: Option<u32>, me: Option<&RelayIdentity>, now: u64) {
        let keys: Vec<(u32, u32)> = self
            .contacts
            .keys()
            .filter_map(|k| parse_contact_key(k))
            .collect();
        for (my_node, peer) in keys {
            if Some(my_node) == owner {
                self.disable(my_node, peer, me, now);
            } else if let Some(c) = self.contacts.get(&contact_key(my_node, peer)).cloned() {
                self.end(my_node, peer, me);
                let until = now + tombstone_secs();
                self.tombstone_secret(c.secret(), until);
                if let ContactRelay::Active {
                    rotation: Some(r), ..
                } = &c
                {
                    self.tombstone_secret(r.secret(), until);
                }
            }
        }
        // Controls of other radios cannot be sent any more.
        self.outgoing.retain(|p| Some(p.my_node) == owner);
        self.password = None;
    }

    /// Forget everything about `peer` without telling it (blocked or
    /// removed): its secrets are tombstoned and its queued controls dropped.
    pub fn forget(&mut self, peer: u32, me: Option<&RelayIdentity>, now: u64) -> bool {
        let keys: Vec<String> = self
            .contacts
            .keys()
            .filter(|k| parse_contact_key(k).is_some_and(|(_, p)| p == peer))
            .cloned()
            .collect();
        let until = now + tombstone_secs();
        for k in &keys {
            let Some((my_node, _)) = parse_contact_key(k) else {
                continue;
            };
            if let Some(c) = self.contacts.get(k).cloned() {
                self.end(my_node, peer, me);
                self.tombstone_secret(c.secret(), until);
                if let ContactRelay::Active {
                    rotation: Some(r), ..
                } = &c
                {
                    self.tombstone_secret(r.secret(), until);
                }
            }
        }
        let gone: Vec<String> = self
            .retiring
            .keys()
            .filter(|k| parse_contact_key(k).is_some_and(|(_, p)| p == peer))
            .cloned()
            .collect();
        for k in &gone {
            if let Some(r) = self.retiring.remove(k) {
                self.unsubscribe.insert(r.inbound);
            }
        }
        let before = self.outgoing.len();
        self.outgoing.retain(|p| p.peer != peer);
        !keys.is_empty() || self.outgoing.len() != before
    }

    /// Like [`Self::end`] for a relay ended by a revoke (sent or received):
    /// its topics stay subscribed as *retiring* until `now` + the revoke's
    /// lifetime, so duplicate revokes and their ACKs still get through.
    fn retire(&mut self, my_node: u32, peer: u32, me: Option<&RelayIdentity>, now: u64) {
        let topics = me.and_then(|m| self.topics_of(my_node, peer, m));
        self.end(my_node, peer, me);
        if let Some((inbound, outbound)) = topics {
            self.unsubscribe.remove(&inbound);
            self.retiring.insert(
                contact_key(my_node, peer),
                Retiring {
                    inbound,
                    outbound,
                    until: now + CONTROL_LIFETIME,
                    gen: None,
                },
            );
        }
    }

    /// Unsubscribe retiring topics whose time is up.
    pub fn prune_retiring(&mut self, now: u64) {
        let done: Vec<String> = self
            .retiring
            .iter()
            .filter(|(_, r)| r.until < now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in done {
            if let Some(r) = self.retiring.remove(&k) {
                self.unsubscribe.insert(r.inbound);
            }
        }
    }

    /// Remove the contact's state, its pending controls (except revokes)
    /// and note its topics for unsubscription.
    fn end(&mut self, my_node: u32, peer: u32, me: Option<&RelayIdentity>) {
        self.note_unsubscribe(my_node, peer, me);
        self.retiring.remove(&contact_key(my_node, peer));
        self.contacts.remove(&contact_key(my_node, peer));
        self.drop_outgoing(
            my_node,
            peer,
            Some(|c| !matches!(c, StoredControl::Revoke { .. })),
        );
    }

    // ── Received control messages ──────────────────────────────

    /// Apply a control message from `peer` (authenticated, not a
    /// duplicate, peer not blocked). `me`: this device's relay identity, if
    /// configured.
    pub fn on_control(
        &mut self,
        my_node: u32,
        peer: u32,
        control: &RelayControl,
        me: Option<&RelayIdentity>,
        now: u64,
    ) -> ControlOutcome {
        self.prune(now);
        let until = now + tombstone_secs();
        let key = contact_key(my_node, peer);
        match control {
            RelayControl::Offer(o) => {
                let confirm = o.secret.confirm_tag();
                if self.tombstoned(&confirm, now) {
                    return ControlOutcome::Handled;
                }
                let incoming = ContactRelay::Incoming {
                    secret: StoredSecret::new(&o.secret),
                    peer_inbox: o.inbox.clone(),
                    peer_tag: hex(&o.broker_tag),
                };
                let same_broker = me.is_some_and(|m| m.broker_tag == o.broker_tag);
                match self.contacts.get(&key).cloned() {
                    None => {
                        self.contacts.insert(key, incoming);
                        ControlOutcome::OfferReceived { same_broker }
                    }
                    Some(ContactRelay::Incoming { secret, .. }) => {
                        if secret
                            .confirm()
                            .is_some_and(|c| confirm_matches(&c, &confirm))
                        {
                            return ControlOutcome::Handled;
                        }
                        // The newest offer wins; the older one is dead.
                        self.tombstone_secret(&secret, until);
                        self.contacts.insert(key, incoming);
                        ControlOutcome::OfferReceived { same_broker }
                    }
                    Some(ContactRelay::Offered { secret: mine }) => {
                        // Crossed offers: the lower node number's offer wins.
                        if my_node < peer {
                            self.tombstone(confirm, until);
                            return ControlOutcome::Handled;
                        }
                        self.tombstone_secret(&mine, until);
                        self.drop_outgoing(
                            my_node,
                            peer,
                            Some(|c| matches!(c, StoredControl::Offer { .. })),
                        );
                        self.contacts.insert(key, incoming);
                        // Our user already consented by enabling; the same
                        // checks as a manual accept apply.
                        match self.accept(my_node, peer, me, now) {
                            Ok(outcome) => outcome,
                            Err(_) => ControlOutcome::OfferReceived { same_broker },
                        }
                    }
                    Some(ContactRelay::Active {
                        secret,
                        peer_inbox,
                        rotation,
                    }) => {
                        if secret
                            .confirm()
                            .is_some_and(|c| confirm_matches(&c, &confirm))
                        {
                            // A retry of the offer we accepted; our accept
                            // is being retried separately.
                            return ControlOutcome::Handled;
                        }
                        if let Some(r) = &rotation {
                            if r.secret()
                                .confirm()
                                .is_some_and(|c| confirm_matches(&c, &confirm))
                            {
                                return ControlOutcome::Handled;
                            }
                            self.tombstone_secret(r.secret(), until);
                        }
                        self.contacts.insert(
                            key,
                            ContactRelay::Active {
                                secret,
                                peer_inbox,
                                rotation: Some(Box::new(incoming)),
                            },
                        );
                        ControlOutcome::OfferReceived { same_broker }
                    }
                }
            }
            RelayControl::Accept(a) => {
                if self.tombstoned(&a.confirm, now) {
                    return ControlOutcome::Handled;
                }
                let Some(ContactRelay::Offered { secret }) = self.contacts.get(&key).cloned()
                else {
                    return ControlOutcome::Handled;
                };
                let matches = secret
                    .confirm()
                    .is_some_and(|c| confirm_matches(&c, &a.confirm));
                let same_broker = me.is_some_and(|m| m.broker_tag == a.broker_tag);
                if !matches || !same_broker || !is_valid_inbox(&a.inbox) {
                    return ControlOutcome::Handled;
                }
                self.retiring.remove(&key);
                self.contacts.insert(
                    key,
                    ContactRelay::Active {
                        secret,
                        peer_inbox: a.inbox.clone(),
                        rotation: None,
                    },
                );
                if let Some((inbound, _)) = me.and_then(|me| self.topics_of(my_node, peer, me)) {
                    self.unsubscribe.remove(&inbound);
                }

                self.drop_outgoing(
                    my_node,
                    peer,
                    Some(|c| matches!(c, StoredControl::Offer { .. })),
                );
                ControlOutcome::Activated
            }
            RelayControl::Revoke(r) => {
                self.tombstone(r.confirm, until);
                let Some(c) = self.contacts.get(&key).cloned() else {
                    return ControlOutcome::Handled;
                };
                let is =
                    |s: &StoredSecret| s.confirm().is_some_and(|x| confirm_matches(&x, &r.confirm));
                if is(c.secret()) {
                    self.retire(my_node, peer, me, now);
                    // A newer offer waiting as a rotation is another
                    // generation: it stays, as an incoming offer.
                    if let ContactRelay::Active {
                        rotation: Some(rot),
                        ..
                    } = c
                    {
                        self.contacts.insert(key, *rot);
                    }
                    return ControlOutcome::Revoked;
                }
                if let ContactRelay::Active {
                    secret,
                    peer_inbox,
                    rotation: Some(rot),
                } = c
                {
                    if is(rot.secret()) {
                        self.contacts.insert(
                            key,
                            ContactRelay::Active {
                                secret,
                                peer_inbox,
                                rotation: None,
                            },
                        );
                    }
                }
                ControlOutcome::Handled
            }
        }
    }

    // ── Retries ────────────────────────────────────────────────

    /// The control messages of radio `my_node` due at `now`.
    pub fn due_controls(&self, my_node: u32, now: u64) -> Vec<PendingControl> {
        self.outgoing
            .iter()
            .filter(|p| p.my_node == my_node && p.next_at <= now)
            .cloned()
            .collect()
    }

    /// Record a send attempt of control `id`. Returns true when it was
    /// given up (too many attempts or past its lifetime): an unanswered
    /// offer then ends the exchange.
    pub fn control_attempted(&mut self, id: &str, now: u64) -> bool {
        !self.reserve_attempt(id, now)
    }

    /// Reserve the next send attempt of control `id` *before* it is sent:
    /// the attempt counts and the next deadline moves, so a failed send or
    /// a crash cannot cause a retry storm. Returns false (nothing to send)
    /// when it is gone, exhausted or past its lifetime; those are retired
    /// here (an unanswered offer then ends the exchange).
    pub fn reserve_attempt(&mut self, id: &str, now: u64) -> bool {
        let Some(i) = self.outgoing.iter().position(|p| p.message_id == id) else {
            return false;
        };
        let p = &mut self.outgoing[i];
        let give_up = p.attempts >= CONTROL_MAX_ATTEMPTS
            || now.saturating_sub(p.created_at) >= CONTROL_LIFETIME;
        if !give_up {
            p.attempts += 1;
            let backoff = CONTROL_FIRST_RETRY
                .saturating_mul(1u64 << (p.attempts - 1).min(10))
                .min(CONTROL_MAX_RETRY);
            p.next_at = now + backoff;
            return true;
        }
        {
            let p = self.outgoing.remove(i);
            if let StoredControl::Offer { secret, .. } = &p.control {
                // Nobody answered: back to no relay (still tombstoned).
                let key = contact_key(p.my_node, p.peer);
                if matches!(self.contacts.get(&key), Some(ContactRelay::Offered { secret: s }) if s == secret)
                {
                    self.contacts.remove(&key);
                    self.tombstone_secret(secret, now + tombstone_secs());
                }
            }
        }
        false
    }

    /// An authenticated AppAck from `peer` for `acked`. Returns true when it
    /// was one of our control messages (retries stop).
    pub fn control_acked(&mut self, my_node: u32, peer: u32, acked: &MessageId) -> bool {
        let id = acked.to_hex();
        let Some(i) = self
            .outgoing
            .iter()
            .position(|p| p.my_node == my_node && p.peer == peer && p.message_id == id)
        else {
            return false;
        };
        let p = self.outgoing.remove(i);
        let revokes_left = self.outgoing.iter().any(|q| {
            q.my_node == my_node
                && q.peer == peer
                && matches!(q.control, StoredControl::Revoke { .. })
        });
        if matches!(p.control, StoredControl::Revoke { .. }) && !revokes_left {
            // The peer has it: the retiring topics can go.
            if let Some(r) = self.retiring.remove(&contact_key(my_node, peer)) {
                self.unsubscribe.insert(r.inbound);
            }
        }
        true
    }
}

fn is_valid_inbox(s: &str) -> bool {
    meshguard_core::relay::validate_inbox(s).is_ok()
}

/// How long tombstones are kept: as long as any frame can still pass
/// freshness checks (dedup retention, inclusive).
pub fn tombstone_secs() -> u64 {
    meshguard_core::inbound::InboundPolicy::default().dedup_retention_secs()
}

/// Our relay identity from the settings, when they are complete.
pub fn identity(user: &str, host: &str, port: u16) -> Option<RelayIdentity> {
    is_app_username(user).then(|| RelayIdentity {
        user: user.to_string(),
        broker_tag: meshguard_core::relay::broker_tag(host, port),
    })
}

// ── Engine glue ────────────────────────────────────────────────

impl crate::Engine {
    /// This device's relay identity, when the relay is set up.
    pub fn relay_identity(&self) -> Option<RelayIdentity> {
        let s = self.config.relay.as_ref()?;
        identity(&s.username, &s.host, s.port)
    }

    /// Load the relay book with the vault key. An unreadable book (another
    /// vault key) is moved aside, never deleted, and a new one starts.
    pub(crate) fn load_relay(&mut self) {
        let Some(vault) = &self.vault else { return };
        let path = self.config_dir.join(BOOK_FILE);
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        match open_book(vault, &data) {
            Ok(book) => {
                self.relay = book;
                self.sync_relay_generations();
                self.replay_relay_settings();
            }
            Err(e) => {
                tracing::warn!("Relay settings could not be opened ({e}); starting new ones");
                let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                let aside = self
                    .config_dir
                    .join(format!("{BOOK_FILE}.unreadable-{stamp}"));
                if let Err(e) = std::fs::rename(&path, aside) {
                    tracing::warn!("Could not move the relay book aside: {e}");
                }
            }
        }
    }

    /// Finish a relay settings change that was interrupted after the book
    /// (with credentials and intended settings) was saved: apply the
    /// settings, then clear the record.
    fn replay_relay_settings(&mut self) {
        let Some(p) = self.relay.pending_settings.clone() else {
            return;
        };
        self.config.relay = p.settings;
        match self.config.save(&self.config_dir) {
            Ok(()) => {
                tracing::info!("Finished an interrupted relay settings change");
                if let Err(e) = self.update_relay(|b| b.pending_settings = None) {
                    tracing::warn!("Relay journal not cleared: {e}");
                }
            }
            Err(e) => tracing::warn!("Relay settings change not finished yet: {e}"),
        }
    }

    /// Save the relay book (sealed with the vault key, atomically).
    pub fn save_relay(&self) -> Result<(), MeshGuardError> {
        let vault = self.vault.as_ref().ok_or_else(|| {
            MeshGuardError::InvalidConfig(
                "This device has no secure key store, so internet relay is not available".into(),
            )
        })?;
        let data = seal_book(vault, &self.relay)?;
        std::fs::create_dir_all(&self.config_dir).map_err(|e| MeshGuardError::Io(e.to_string()))?;
        let tmp = self.config_dir.join(format!("{BOOK_FILE}.tmp"));
        std::fs::write(&tmp, data).map_err(|e| MeshGuardError::Io(e.to_string()))?;
        std::fs::rename(&tmp, self.config_dir.join(BOOK_FILE))
            .map_err(|e| MeshGuardError::Io(e.to_string()))
    }

    /// The radio whose contacts the relay serves now (keys are bound to it).
    fn relay_radio(&self) -> Result<u32, MeshGuardError> {
        self.keys_owner.ok_or(MeshGuardError::NotConnected)
    }

    /// Check that `peer` may use the relay: a confirmed v2 contact with a
    /// chat key, not blocked.
    fn relay_contact(&self, my_node: u32, peer: u32) -> Result<(), MeshGuardError> {
        let ok = !self.config.is_blocked(peer)
            && self.keys_owner == Some(my_node)
            && self.keys.contains_key(&peer)
            && self.config.peers.iter().any(|p| {
                p.node_num == peer && p.confirmed && p.protocol == crate::config::WireProtocol::V2
            });
        if ok {
            Ok(())
        } else {
            Err(MeshGuardError::Protocol(
                "Internet relay needs a paired MeshGuard 2.0 contact".into(),
            ))
        }
    }

    /// Whether `peer` announced that it understands the relay messages.
    pub fn peer_supports_relay(&self, peer: u32) -> bool {
        self.config
            .peers
            .iter()
            .any(|p| p.node_num == peer && p.features & meshguard_core::relay::FEATURE_RELAY != 0)
    }

    /// The user turns the relay on for `peer` (offer, or accept a pending
    /// offer).
    pub fn relay_enable(&mut self, peer: u32, now: u64) -> Result<ControlOutcome, MeshGuardError> {
        let my_node = self.relay_radio()?;
        self.relay_contact(my_node, peer)?;
        let incoming = matches!(
            self.relay.state(my_node, peer),
            Some(ContactRelay::Incoming { .. })
                | Some(ContactRelay::Active {
                    rotation: Some(_),
                    ..
                })
        );
        if !incoming && !self.peer_supports_relay(peer) {
            return Err(MeshGuardError::Protocol(
                "Your contact's MeshGuard does not support internet relay yet".into(),
            ));
        }
        let me = self.relay_identity();
        self.update_relay(|b| b.enable(my_node, peer, me.as_ref(), now))?
            .map_err(|r| MeshGuardError::Protocol(r.to_string()))
    }

    /// The user turns the relay off for `peer`, or declines its offer.
    pub fn relay_disable(&mut self, peer: u32, now: u64) -> Result<bool, MeshGuardError> {
        let my_node = self.relay_radio()?;
        let me = self.relay_identity();
        self.update_relay(|b| b.disable(my_node, peer, me.as_ref(), now))
    }

    /// Remove the relay settings' secrets: every contact ends (ADR-006 §7).
    pub fn relay_remove_all(&mut self, now: u64) -> Result<(), MeshGuardError> {
        let me = self.relay_identity();
        let owner = self.keys_owner;
        self.update_relay(|b| b.remove_all(owner, me.as_ref(), now))
    }

    /// Whether an ACK prepared for `peer` with `reply` may still go out.
    pub fn relay_reply_valid(&self, peer: u32, reply: &RelayReply) -> bool {
        !self.config.is_blocked(peer) && self.send_generation(peer) == reply.generation
    }

    /// Blocked or removed: forget the relay with `peer` without telling it.
    /// Kept in memory even if saving fails (logged), so it stops now.
    pub fn relay_forget(&mut self, peer: u32, now: u64) -> bool {
        let me = self.relay_identity();
        // If saving fails, the raised send generation (saved with the
        // block/removal) still makes any reloaded entry stale.
        let changed = self.relay.forget(peer, me.as_ref(), now);
        if changed {
            if let Err(e) = self.save_relay() {
                tracing::warn!("Relay state for {peer} forgotten but not saved: {e}");
            }
        }
        changed
    }

    /// Inbound topics of the current radio's relay contacts, mapped to the
    /// peer (blocked peers excluded).
    pub fn relay_inbound_topics(&self) -> BTreeMap<String, u32> {
        let (Some(my_node), Some(me)) = (self.keys_owner, self.relay_identity()) else {
            return BTreeMap::new();
        };
        let mut t = self.relay.inbound_topics(my_node, &me);
        t.retain(|_, peer| !self.config.is_blocked(*peer) && self.relay_current(my_node, *peer));
        t
    }

    /// Retiring inbound topics of the current radio (see
    /// [`RelayBook::retiring`]): topic -> (peer, reply topic).
    pub fn relay_retiring_topics(&self) -> BTreeMap<String, (u32, String)> {
        let Some(my_node) = self.keys_owner else {
            return BTreeMap::new();
        };
        self.relay
            .retiring
            .iter()
            .filter_map(|(k, r)| {
                let (m, peer) = parse_contact_key(k)?;
                (m == my_node
                    && !self.config.is_blocked(peer)
                    && r.gen == Some(self.send_generation(peer)))
                .then(|| (r.inbound.clone(), (peer, r.outbound.clone())))
            })
            .collect()
    }

    /// The topic to publish to `peer` on, when the relay is active with it.
    pub fn relay_outbound_topic(&self, peer: u32) -> Option<String> {
        let my_node = self.keys_owner?;
        if self.config.is_blocked(peer) {
            return None;
        }
        if !self.relay_current(my_node, peer) {
            return None;
        }
        let me = self.relay_identity()?;
        self.relay.topics_of(my_node, peer, &me).map(|(_, out)| out)
    }

    /// Seal the relay control messages due now (for the current radio).
    /// Those whose key is missing wait.
    pub fn relay_due_controls(&self, now: u64) -> Vec<ControlTransmit> {
        let Some(my_node) = self.keys_owner else {
            return Vec::new();
        };
        self.relay
            .due_controls(my_node, now)
            .into_iter()
            .filter(|p| !self.config.is_blocked(p.peer))
            .filter(|p| p.gen == Some(self.send_generation(p.peer)))
            .filter_map(|p| {
                let key = self.keys.get(&p.peer)?;
                let control = p.control.control()?;
                let frame = meshguard_core::outbound::seal_relay(
                    &control,
                    p.id()?,
                    key,
                    &meshguard_core::PeerIdentity::meshtastic_node(my_node),
                    &meshguard_core::PeerIdentity::meshtastic_node(p.peer),
                    u32::try_from(p.created_at).unwrap_or(u32::MAX),
                    meshguard_core::envelope::DEFAULT_FRAME_BUDGET,
                )
                .ok()?;
                Some(ControlTransmit {
                    my_node,
                    peer: p.peer,
                    message_id: p.message_id,
                    frame,
                    topic: p.topic,
                })
            })
            .collect()
    }

    /// Reserve (and save) a send attempt of control `c` right before it is
    /// handed to a transport, under the caller's send gate. False: do not
    /// send (cancelled, blocked, stale, exhausted, or the reservation could
    /// not be saved).
    pub fn relay_reserve_control(&mut self, c: &ControlTransmit, now: u64) -> bool {
        let pending = self.relay.outgoing.iter().any(|p| {
            p.message_id == c.message_id
                && p.peer == c.peer
                && p.my_node == c.my_node
                && p.gen == Some(self.send_generation(c.peer))
        });
        if !pending
            || self.config.is_blocked(c.peer)
            || self.keys_owner != Some(c.my_node)
            || !self.keys.contains_key(&c.peer)
        {
            return false;
        }
        match self.update_relay(|b| b.reserve_attempt(&c.message_id, now)) {
            Ok(go) => go,
            Err(e) => {
                tracing::warn!("Relay retry state not saved, not sending: {e}");
                false
            }
        }
    }

    /// Whether the relay entry with `peer` belongs to its current send
    /// generation (not left over from before a block or removal).
    fn relay_current(&self, my_node: u32, peer: u32) -> bool {
        self.relay
            .generations
            .get(&contact_key(my_node, peer))
            .is_some_and(|g| *g == self.send_generation(peer))
    }

    /// Drop relay entries and controls made under an older send generation
    /// of their peer (it was blocked or removed since), and stamp new ones
    /// with the current generation.
    pub(crate) fn sync_relay_generations(&mut self) {
        let gens = &self.config.send_generations;
        let gen_of = |peer: u32| gens.get(&peer).copied().unwrap_or(0);
        let book = &mut self.relay;
        let keys: Vec<String> = book.contacts.keys().cloned().collect();
        for k in keys {
            let Some((_, peer)) = parse_contact_key(&k) else {
                continue;
            };
            match book.generations.get(&k) {
                None => {
                    book.generations.insert(k, gen_of(peer));
                }
                Some(g) if *g != gen_of(peer) => {
                    book.contacts.remove(&k);
                    book.generations.remove(&k);
                }
                Some(_) => {}
            }
        }
        book.generations
            .retain(|k, _| book.contacts.contains_key(k));
        for (k, r) in book.retiring.iter_mut() {
            if let Some((_, peer)) = parse_contact_key(k) {
                r.gen.get_or_insert(gen_of(peer));
            }
        }
        let stale: Vec<String> = book
            .retiring
            .iter()
            .filter(|(k, r)| {
                parse_contact_key(k).is_none_or(|(_, peer)| r.gen != Some(gen_of(peer)))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(r) = book.retiring.remove(&k) {
                book.unsubscribe.insert(r.inbound);
            }
        }
        for c in &mut book.outgoing {
            c.gen.get_or_insert(gen_of(c.peer));
        }
        book.outgoing.retain(|c| c.gen == Some(gen_of(c.peer)));
    }

    /// Change the relay book with `f` and save it; on a failed save the
    /// change is undone. Unchanged books are not written.
    pub fn update_relay<T>(
        &mut self,
        f: impl FnOnce(&mut RelayBook) -> T,
    ) -> Result<T, MeshGuardError> {
        self.sync_relay_generations();
        let before = self.relay.clone();
        let out = f(&mut self.relay);
        self.sync_relay_generations();
        if self.relay != before {
            if let Err(e) = self.save_relay() {
                self.relay = before;
                return Err(e);
            }
        }
        Ok(out)
    }
}

/// A sealed relay control message to transmit.
#[derive(Debug, Clone)]
pub struct ControlTransmit {
    pub my_node: u32,
    pub peer: u32,
    pub message_id: String,
    pub frame: Vec<u8>,
    /// Relay topic kept for a revoke whose routing state is gone.
    pub topic: Option<String>,
}

/// Where an ACK for a relay frame goes, captured when the frame arrived
/// (the contact's routing state may change while handling it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayReply {
    pub topic: String,
    /// The peer's send generation then; the ACK is dropped if it changed
    /// (blocked or removed meanwhile).
    pub generation: u64,
}

// ── Sealing ────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct SealedBook {
    version: u32,
    /// base64-free hex of nonce ‖ AES-GCM(vault, JSON).
    blob: String,
}

/// Seal `book` with the vault key.
pub fn seal_book(vault: &SessionKey, book: &RelayBook) -> Result<Vec<u8>, MeshGuardError> {
    let json = zeroize::Zeroizing::new(
        serde_json::to_vec(book).map_err(|e| MeshGuardError::Serialization(e.to_string()))?,
    );
    let blob = vault
        .encrypt_with_aad(&json, BOOK_AAD)
        .map_err(|e| MeshGuardError::Encryption(e.to_string()))?;
    serde_json::to_vec(&SealedBook {
        version: 1,
        blob: hex(&blob),
    })
    .map_err(|e| MeshGuardError::Serialization(e.to_string()))
}

/// Open a sealed book. `Err`: wrong key or damaged.
pub fn open_book(vault: &SessionKey, data: &[u8]) -> Result<RelayBook, MeshGuardError> {
    let sealed: SealedBook =
        serde_json::from_slice(data).map_err(|e| MeshGuardError::Serialization(e.to_string()))?;
    if sealed.version != 1 || !sealed.blob.len().is_multiple_of(2) {
        return Err(MeshGuardError::Serialization(
            "unknown relay book format".into(),
        ));
    }
    let blob = unhex_vec(&sealed.blob);
    let blob = blob.ok_or_else(|| MeshGuardError::Serialization("damaged relay book".into()))?;
    let json = zeroize::Zeroizing::new(
        vault
            .decrypt_with_aad(&blob, BOOK_AAD)
            .map_err(|e| MeshGuardError::Encryption(e.to_string()))?,
    );
    serde_json::from_slice(&json).map_err(|e| MeshGuardError::Serialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 0x11; // lower node number
    const B: u32 = 0x22;
    const NOW: u64 = 1_763_200_000;

    fn me(user: &str) -> RelayIdentity {
        identity(user, "broker.example", 8883).unwrap()
    }

    fn ua() -> RelayIdentity {
        me("mg-aaaaaaaaaaaaaaaaaaaa")
    }

    fn ub() -> RelayIdentity {
        me("mg-bbbbbbbbbbbbbbbbbbbb")
    }

    /// Deliver every due control of `from` (radio `from_node`) to `to`.
    fn pump(
        from: &mut RelayBook,
        from_node: u32,
        from_me: &RelayIdentity,
        to: &mut RelayBook,
        to_node: u32,
        to_me: &RelayIdentity,
        now: u64,
    ) -> Vec<ControlOutcome> {
        let mut out = Vec::new();
        for p in from.due_controls(from_node, now) {
            let c = p.control.control().unwrap();
            out.push(to.on_control(to_node, from_node, &c, Some(to_me), now));
            // Delivered and acknowledged.
            assert!(from.control_acked(from_node, to_node, &p.id().unwrap()));
        }
        let _ = from_me;
        out
    }

    #[test]
    fn offer_accept_gives_matching_topics_on_both_sides() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        assert!(a.topics_of(A, B, &ua()).is_none(), "not active yet");
        let got = pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        assert_eq!(
            got,
            vec![ControlOutcome::OfferReceived { same_broker: true }]
        );
        assert_eq!(
            b.accept(B, A, Some(&ub()), NOW),
            Ok(ControlOutcome::Activated)
        );
        let got = pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        assert_eq!(got, vec![ControlOutcome::Activated]);

        let (a_in, a_out) = a.topics_of(A, B, &ua()).unwrap();
        let (b_in, b_out) = b.topics_of(B, A, &ub()).unwrap();
        assert_eq!(a_out, b_in);
        assert_eq!(b_out, a_in);
        assert!(a_out.starts_with("mg/r1/mg-bbbbbbbbbbbbbbbbbbbb/mg-aaaaaaaaaaaaaaaaaaaa/"));
        assert_eq!(a.inbound_topics(A, &ua()), BTreeMap::from([(a_in, B)]));
        assert!(a.outgoing.is_empty() && b.outgoing.is_empty());
    }

    #[test]
    fn a_different_broker_cannot_be_activated_on_either_side() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        let other = identity("mg-bbbbbbbbbbbbbbbbbbbb", "other.example", 8883).unwrap();
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let got = pump(&mut a, A, &ua(), &mut b, B, &other, NOW);
        assert_eq!(
            got,
            vec![ControlOutcome::OfferReceived { same_broker: false }]
        );
        assert_eq!(
            b.accept(B, A, Some(&other), NOW),
            Err(RelayRefusal::DifferentBroker)
        );
        assert_eq!(b.accept(B, A, None, NOW), Err(RelayRefusal::NotConfigured));
        // A forged/odd accept with another broker tag is ignored by A.
        let c = a.contact(A, B).unwrap().secret().confirm().unwrap();
        let accept = RelayControl::Accept(RelayAccept {
            confirm: c,
            broker_tag: other.broker_tag,
            inbox: other.user.clone(),
        });
        assert_eq!(
            a.on_control(A, B, &accept, Some(&ua()), NOW),
            ControlOutcome::Handled
        );
        assert!(matches!(
            a.contact(A, B),
            Some(ContactRelay::Offered { .. })
        ));
    }

    #[test]
    fn crossed_offers_settle_on_the_lower_nodes_secret() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        b.enable(B, A, Some(&ub()), NOW).unwrap();
        let a_offer = a.due_controls(A, NOW);
        let b_offer = b.due_controls(B, NOW);
        // Both offers cross on the air.
        let at_b = b.on_control(
            B,
            A,
            &a_offer[0].control.control().unwrap(),
            Some(&ub()),
            NOW,
        );
        let at_a = a.on_control(
            A,
            B,
            &b_offer[0].control.control().unwrap(),
            Some(&ua()),
            NOW,
        );
        assert_eq!(at_b, ControlOutcome::Activated, "higher node auto-accepts");
        assert_eq!(at_a, ControlOutcome::Handled, "lower node keeps its offer");
        // B's own offer is gone; its accept goes out and activates A.
        assert!(b
            .due_controls(B, NOW)
            .iter()
            .all(|p| matches!(p.control, StoredControl::Accept { .. })));
        a.control_acked(A, B, &a_offer[0].id().unwrap());
        let got = pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        assert_eq!(got, vec![ControlOutcome::Activated]);
        assert_eq!(
            a.topics_of(A, B, &ua()).unwrap().1,
            b.topics_of(B, A, &ub()).unwrap().0
        );
        // A late copy of B's dead offer changes nothing.
        let late = a.on_control(
            A,
            B,
            &b_offer[0].control.control().unwrap(),
            Some(&ua()),
            NOW,
        );
        assert_eq!(late, ControlOutcome::Handled);
        assert!(matches!(
            a.contact(A, B),
            Some(ContactRelay::Active { rotation: None, .. })
        ));
    }

    #[test]
    fn a_revoke_overtaking_its_offer_wins() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let offer = a.due_controls(A, NOW)[0].clone();
        assert!(a.disable(A, B, Some(&ua()), NOW));
        let revoke = a.due_controls(A, NOW);
        assert_eq!(revoke.len(), 1, "only the revoke is left");
        // The revoke arrives first, the offer later.
        let r = b.on_control(
            B,
            A,
            &revoke[0].control.control().unwrap(),
            Some(&ub()),
            NOW,
        );
        assert_eq!(r, ControlOutcome::Handled);
        let o = b.on_control(
            B,
            A,
            &offer.control.control().unwrap(),
            Some(&ub()),
            NOW + 60,
        );
        assert_eq!(o, ControlOutcome::Handled);
        assert!(b.contact(B, A).is_none());
    }

    #[test]
    fn revoke_ends_an_active_relay_and_unsubscribes() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        b.accept(B, A, Some(&ub()), NOW).unwrap();
        pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        let (b_in, _) = b.topics_of(B, A, &ub()).unwrap();

        assert!(a.disable(A, B, Some(&ua()), NOW));
        assert!(a.contact(A, B).is_none());
        let got = pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        assert_eq!(got, vec![ControlOutcome::Revoked]);
        assert!(b.contact(B, A).is_none());
        // Retiring first (a lost ACK can still be repeated), then gone.
        assert!(!b.unsubscribe.contains(&b_in));
        assert!(b.retiring.values().any(|r| r.inbound == b_in));
        b.prune_retiring(NOW + CONTROL_LIFETIME + 1);
        assert!(b.unsubscribe.contains(&b_in) && b.retiring.is_empty());
        // A replayed accept or offer of that generation is ignored.
        let stale = b.on_control(
            B,
            A,
            &RelayControl::Revoke(RelayRevoke { confirm: [9; 16] }),
            Some(&ub()),
            NOW,
        );
        assert_eq!(stale, ControlOutcome::Handled);
    }

    #[test]
    fn forgetting_a_blocked_peer_sends_nothing_and_tombstones() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let offer = a.due_controls(A, NOW)[0].clone();
        pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        assert!(b.forget(A, Some(&ub()), NOW));
        assert!(b.contact(B, A).is_none() && b.outgoing.is_empty());
        // The same offer again (e.g. replayed) stays dead.
        let again = b.on_control(
            B,
            A,
            &offer.control.control().unwrap(),
            Some(&ub()),
            NOW + 1,
        );
        assert_eq!(again, ControlOutcome::Handled);
        assert!(b.contact(B, A).is_none());
    }

    #[test]
    fn a_lost_accept_is_retried_and_duplicates_are_idempotent() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let offer = a.due_controls(A, NOW)[0].control.control().unwrap();
        b.on_control(B, A, &offer, Some(&ub()), NOW);
        b.accept(B, A, Some(&ub()), NOW).unwrap();
        // The first accept is lost: an attempt without an ACK.
        let acc = b.due_controls(B, NOW)[0].clone();
        assert!(!b.control_attempted(&acc.message_id, NOW));
        assert!(b.due_controls(B, NOW + 30).is_empty());
        let retry = b.due_controls(B, NOW + 60);
        assert_eq!(retry[0].message_id, acc.message_id, "same id on retry");
        // The offer arriving again does not disturb the active state.
        assert_eq!(
            b.on_control(B, A, &offer, Some(&ub()), NOW + 61),
            ControlOutcome::Handled
        );
        let got = a.on_control(
            A,
            B,
            &retry[0].control.control().unwrap(),
            Some(&ua()),
            NOW + 61,
        );
        assert_eq!(got, ControlOutcome::Activated);
        // And a duplicate accept is harmless.
        let dup = a.on_control(
            A,
            B,
            &retry[0].control.control().unwrap(),
            Some(&ua()),
            NOW + 62,
        );
        assert_eq!(dup, ControlOutcome::Handled);
    }

    #[test]
    fn rotation_needs_the_users_accept_and_keeps_the_old_secret_until_then() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        b.accept(B, A, Some(&ub()), NOW).unwrap();
        pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        let before = b.topics_of(B, A, &ub()).unwrap();
        // A offers a new secret (e.g. after reinstalling relay settings).
        let new_offer = RelayControl::Offer(RelayOffer {
            secret: RelaySecret::random(),
            broker_tag: ua().broker_tag,
            inbox: ua().user,
        });
        assert_eq!(
            b.on_control(B, A, &new_offer, Some(&ub()), NOW),
            ControlOutcome::OfferReceived { same_broker: true }
        );
        assert_eq!(
            b.topics_of(B, A, &ub()).unwrap(),
            before,
            "old still active"
        );
        assert_eq!(
            b.accept(B, A, Some(&ub()), NOW),
            Ok(ControlOutcome::Activated)
        );
        assert_ne!(b.topics_of(B, A, &ub()).unwrap(), before);
        assert!(
            b.unsubscribe.contains(&before.0),
            "old topic leaves the session"
        );
    }

    #[test]
    fn an_old_revoke_keeps_a_newer_rotation() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        b.accept(B, A, Some(&ub()), NOW).unwrap();
        pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        let s1 = b.contact(B, A).unwrap().secret().confirm().unwrap();
        // A rotates: Offer(S2) overtakes Revoke(S1).
        let s2 = RelaySecret::random();
        let offer = RelayControl::Offer(RelayOffer {
            secret: s2.clone(),
            broker_tag: ua().broker_tag,
            inbox: ua().user,
        });
        b.on_control(B, A, &offer, Some(&ub()), NOW);
        let revoke = RelayControl::Revoke(RelayRevoke { confirm: s1 });
        assert_eq!(
            b.on_control(B, A, &revoke, Some(&ub()), NOW),
            ControlOutcome::Revoked
        );
        match b.contact(B, A) {
            Some(ContactRelay::Incoming { secret, .. }) => {
                assert_eq!(secret.confirm(), Some(s2.confirm_tag()))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reservations_count_before_sending_and_stop_at_the_limit() {
        let mut a = RelayBook::default();
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let id = a.due_controls(A, NOW)[0].message_id.clone();
        let mut sent = 0;
        let mut t = NOW;
        while a.reserve_attempt(&id, t) {
            sent += 1;
            assert!(a.due_controls(A, t).is_empty(), "not due right after");
            t += CONTROL_MAX_RETRY;
        }
        assert_eq!(sent, CONTROL_MAX_ATTEMPTS);
        assert!(a.outgoing.is_empty() && a.contact(A, B).is_none());
    }

    #[test]
    fn damaged_books_are_errors_not_panics() {
        let vault = SessionKey::from_bytes([3; 32]);
        for blob in ["€0", "zz", "0", "€€"] {
            let data = format!("{{\"version\":1,\"blob\":\"{blob}\"}}");
            assert!(open_book(&vault, data.as_bytes()).is_err(), "{blob}");
        }
    }

    #[test]
    fn removing_the_relay_revokes_ours_and_forgets_other_radios() {
        let (mut a, mut b) = (RelayBook::default(), RelayBook::default());
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        pump(&mut a, A, &ua(), &mut b, B, &ub(), NOW);
        b.accept(B, A, Some(&ub()), NOW).unwrap();
        pump(&mut b, B, &ub(), &mut a, A, &ua(), NOW);
        let (_, out) = a.topics_of(A, B, &ua()).unwrap();
        // An entry of another local radio.
        a.enable(0x99, B, Some(&ua()), NOW).unwrap();
        a.password = Some("pw".into());
        a.remove_all(Some(A), Some(&ua()), NOW);
        assert!(a.contacts.is_empty() && a.password.is_none());
        let revokes: Vec<_> = a.outgoing.iter().collect();
        assert_eq!(revokes.len(), 1);
        assert!(matches!(revokes[0].control, StoredControl::Revoke { .. }));
        assert_eq!(revokes[0].my_node, A);
        assert_eq!(
            revokes[0].topic.as_deref(),
            Some(out.as_str()),
            "sendable over MQTT"
        );
    }

    #[test]
    fn the_book_round_trips_sealed_and_needs_the_vault() {
        let mut a = RelayBook {
            password: Some("pw".into()),
            ..Default::default()
        };
        a.enable(A, B, Some(&ua()), NOW).unwrap();
        let vault = SessionKey::from_bytes([3; 32]);
        let sealed = seal_book(&vault, &a).unwrap();
        let text = String::from_utf8_lossy(&sealed);
        assert!(!text.contains("pw") && !text.contains("mg-aaaa"), "{text}");
        assert_eq!(open_book(&vault, &sealed).unwrap(), a);
        assert!(open_book(&SessionKey::from_bytes([4; 32]), &sealed).is_err());
        assert!(format!("{a:?}").contains("<secret>"));
    }
}
