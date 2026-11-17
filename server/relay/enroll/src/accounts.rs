//! Accounts: a durable state per username in SQLite, kept in step with the
//! broker's Dynamic Security clients (ADR-007 §3).
//!
//! States: `creating` -> `active` -> `deleting`. Every change runs under one
//! lock (the caller holds `&self`). Reconciliation rolls back unfinished
//! creations, finishes deletions and removes broker clients without an
//! active account, so a crash at any step leaves nothing half-made.

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Accounts not renewed for this long are deleted.
pub const EXPIRY_SECS: i64 = 90 * 24 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("broker: {0}")]
    Broker(String),
}

pub type BrokerFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'a>>;

/// The broker side (Mosquitto Dynamic Security), abstracted for tests.
pub trait Broker: Send + Sync {
    /// Create a client bound to client id = username, with the relay role.
    /// Creating one that exists is an error the caller handles.
    fn create<'a>(&'a self, username: &'a str, password: &'a str) -> BrokerFuture<'a, ()>;
    /// Delete a client (and disconnect it). Deleting a missing one is Ok.
    fn delete<'a>(&'a self, username: &'a str) -> BrokerFuture<'a, ()>;
    /// Usernames of all relay clients.
    fn list(&self) -> BrokerFuture<'_, Vec<String>>;
}

pub fn valid_username(u: &str) -> bool {
    u.len() == 23
        && u.starts_with("mg-")
        && u[3..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

pub fn valid_password(p: &str) -> bool {
    p.len() == 32
        && p.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Verifier of a 128-bit random password (a plain hash is enough).
pub fn verifier(password: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"meshguard-relay-v1");
    h.update(password.as_bytes());
    hex::encode(h.finalize())
}

fn eq_ct(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[derive(Debug, PartialEq, Eq)]
pub enum Enrolled {
    Created,
    Renewed,
    /// The username exists with another password.
    Conflict,
    /// Account cap reached.
    Full,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Deleted {
    Removed,
    /// No such account (already deleted, expired, never existed).
    Gone,
    /// Wrong password for an existing account.
    Forbidden,
}

/// The database. Each statement locks it briefly (never across an await);
/// whole operations are serialized by the caller's operation lock.
pub struct Accounts {
    db: std::sync::Mutex<Connection>,
    max_accounts: i64,
}

impl Accounts {
    pub fn open(path: &str, max_accounts: i64) -> Result<Self, Error> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS accounts (
               username   TEXT PRIMARY KEY,
               verifier   TEXT NOT NULL,
               state      TEXT NOT NULL CHECK (state IN ('creating','active','deleting')),
               renewed_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            db: std::sync::Mutex::new(db),
            max_accounts,
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn row(&self, username: &str) -> Result<Option<(String, String)>, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT verifier, state FROM accounts WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    fn set_state(&self, username: &str, state: &str) -> Result<(), Error> {
        self.conn().execute(
            "UPDATE accounts SET state = ?2 WHERE username = ?1",
            params![username, state],
        )?;
        Ok(())
    }

    fn remove(&self, username: &str) -> Result<(), Error> {
        self.conn().execute(
            "DELETE FROM accounts WHERE username = ?1",
            params![username],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, Error> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?)
    }

    /// Create the account, or renew it with the same password.
    pub async fn enroll<B: Broker>(
        &self,
        broker: &B,
        username: &str,
        password: &str,
        now: i64,
    ) -> Result<Enrolled, Error> {
        let v = verifier(password);
        if let Some((stored, state)) = self.row(username)? {
            if !eq_ct(&stored, &v) {
                return Ok(Enrolled::Conflict);
            }
            if state == "active" {
                self.conn().execute(
                    "UPDATE accounts SET renewed_at = ?2 WHERE username = ?1",
                    params![username, now],
                )?;
                return Ok(Enrolled::Renewed);
            }
            // Unfinished: remove it cleanly, then create it again.
            self.finish_removal(broker, username).await?;
        }
        if self.count()? >= self.max_accounts {
            return Ok(Enrolled::Full);
        }
        self.conn().execute(
            "INSERT INTO accounts (username, verifier, state, renewed_at)
             VALUES (?1, ?2, 'creating', ?3)",
            params![username, v, now],
        )?;
        // A leftover broker client (crash before) would make create
        // fail: remove it first. It has no active account.
        broker.delete(username).await.map_err(Error::Broker)?;
        if let Err(e) = broker.create(username, password).await {
            let _ = broker.delete(username).await;
            self.remove(username)?;
            return Err(Error::Broker(e));
        }
        self.set_state(username, "active")?;
        Ok(Enrolled::Created)
    }

    async fn finish_removal<B: Broker>(&self, broker: &B, username: &str) -> Result<(), Error> {
        self.set_state(username, "deleting")?;
        broker.delete(username).await.map_err(Error::Broker)?;
        self.remove(username)
    }

    /// Delete the account if the password matches; unknown usernames are
    /// already gone.
    pub async fn delete<B: Broker>(
        &self,
        broker: &B,
        username: &str,
        password: &str,
    ) -> Result<Deleted, Error> {
        match self.row(username)? {
            None => Ok(Deleted::Gone),
            Some((stored, _)) if !eq_ct(&stored, &verifier(password)) => Ok(Deleted::Forbidden),
            Some(_) => {
                self.finish_removal(broker, username).await?;
                Ok(Deleted::Removed)
            }
        }
    }

    /// Bring the database and the broker in line, and delete expired
    /// accounts. Returns how many accounts were removed.
    pub async fn reconcile<B: Broker>(&self, broker: &B, now: i64) -> Result<usize, Error> {
        let mut removed = 0;
        let stale: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT username FROM accounts
                 WHERE state != 'active' OR renewed_at < ?1",
            )?;
            let rows = stmt.query_map(params![now - EXPIRY_SECS], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for u in stale {
            self.finish_removal(broker, &u).await?;
            removed += 1;
        }
        let clients = broker.list().await.map_err(Error::Broker)?;
        for u in clients {
            if !valid_username(&u) {
                continue; // admin and other non-relay clients
            }
            let active = matches!(self.row(&u)?, Some((_, s)) if s == "active");
            if !active {
                broker.delete(&u).await.map_err(Error::Broker)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory broker that can be told to fail.
    #[derive(Default)]
    pub struct FakeBroker {
        pub clients: Mutex<BTreeMap<String, String>>,
        pub fail_create: Mutex<bool>,
    }

    impl FakeBroker {
        pub fn clients(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
            self.clients.lock().unwrap()
        }
    }

    impl Broker for FakeBroker {
        fn create<'a>(&'a self, u: &'a str, p: &'a str) -> BrokerFuture<'a, ()> {
            Box::pin(async move {
                if *self.fail_create.lock().unwrap() {
                    return Err("down".into());
                }
                let mut c = self.clients();
                if c.contains_key(u) {
                    return Err("Client already exists".into());
                }
                c.insert(u.into(), p.into());
                Ok(())
            })
        }
        fn delete<'a>(&'a self, u: &'a str) -> BrokerFuture<'a, ()> {
            Box::pin(async move {
                self.clients().remove(u);
                Ok(())
            })
        }
        fn list(&self) -> BrokerFuture<'_, Vec<String>> {
            Box::pin(async move { Ok(self.clients().keys().cloned().collect()) })
        }
    }

    pub const U: &str = "mg-aaaaaaaaaaaaaaaaaaaa";
    pub const P: &str = "0123456789abcdef0123456789abcdef";
    const P2: &str = "ffffffffffffffffffffffffffffffff";

    fn accounts() -> Accounts {
        Accounts::open(":memory:", 10).unwrap()
    }

    #[test]
    fn validation() {
        assert!(valid_username(U));
        for bad in [
            "mg-AAAAAAAAAAAAAAAAAAAA",
            "mg-aaaa",
            "admin",
            "mg-aaaaaaaaaaaaaaaaaaa1",
        ] {
            assert!(!valid_username(bad), "{bad}");
        }
        assert!(valid_password(P));
        assert!(!valid_password("short") && !valid_password(&P.to_uppercase()));
    }

    #[tokio::test]
    async fn enroll_renew_conflict_delete() {
        let (a, b) = (accounts(), FakeBroker::default());
        assert_eq!(a.enroll(&b, U, P, 100).await.unwrap(), Enrolled::Created);
        assert_eq!(b.clients().get(U).map(String::as_str), Some(P));
        assert_eq!(a.enroll(&b, U, P, 200).await.unwrap(), Enrolled::Renewed);
        // Nobody can take over an account by enrolling again.
        assert_eq!(a.enroll(&b, U, P2, 300).await.unwrap(), Enrolled::Conflict);
        assert_eq!(b.clients().get(U).map(String::as_str), Some(P));
        assert_eq!(a.delete(&b, U, P2).await.unwrap(), Deleted::Forbidden);
        assert_eq!(a.delete(&b, U, P).await.unwrap(), Deleted::Removed);
        assert!(b.clients().is_empty());
        // A retried delete (lost response) is still a success.
        assert_eq!(a.delete(&b, U, P).await.unwrap(), Deleted::Gone);
    }

    #[tokio::test]
    async fn a_failed_creation_leaves_nothing() {
        let (a, b) = (accounts(), FakeBroker::default());
        *b.fail_create.lock().unwrap() = true;
        assert!(a.enroll(&b, U, P, 1).await.is_err());
        assert_eq!(a.count().unwrap(), 0);
        *b.fail_create.lock().unwrap() = false;
        assert_eq!(a.enroll(&b, U, P, 2).await.unwrap(), Enrolled::Created);
    }

    #[tokio::test]
    async fn reconcile_rolls_back_and_cleans_up() {
        let (a, b) = (accounts(), FakeBroker::default());
        // A crash after "creating" was written and the client made.
        a.conn()
            .execute(
                "INSERT INTO accounts VALUES (?1, ?2, 'creating', 1)",
                params![U, verifier(P)],
            )
            .unwrap();
        b.clients().insert(U.into(), P.into());
        // An orphan broker client, and a non-relay one that must stay.
        b.clients()
            .insert("mg-bbbbbbbbbbbbbbbbbbbb".into(), "x".into());
        b.clients().insert("admin".into(), "x".into());
        assert_eq!(a.reconcile(&b, 10).await.unwrap(), 2);
        assert_eq!(a.count().unwrap(), 0);
        assert_eq!(
            b.clients().keys().cloned().collect::<Vec<_>>(),
            vec!["admin".to_string()]
        );
        // The app's retry of the same enrollment then works.
        assert_eq!(a.enroll(&b, U, P, 11).await.unwrap(), Enrolled::Created);
    }

    #[tokio::test]
    async fn unrenewed_accounts_expire() {
        let (a, b) = (accounts(), FakeBroker::default());
        a.enroll(&b, U, P, 0).await.unwrap();
        assert_eq!(a.reconcile(&b, EXPIRY_SECS - 1).await.unwrap(), 0);
        assert_eq!(a.reconcile(&b, EXPIRY_SECS + 1).await.unwrap(), 1);
        assert!(b.clients().is_empty());
        assert_eq!(a.delete(&b, U, P).await.unwrap(), Deleted::Gone);
    }

    #[tokio::test]
    async fn the_account_cap_holds() {
        let (a, b) = (
            Accounts::open(":memory:", 1).unwrap(),
            FakeBroker::default(),
        );
        a.enroll(&b, U, P, 0).await.unwrap();
        assert_eq!(
            a.enroll(&b, "mg-bbbbbbbbbbbbbbbbbbbb", P, 0).await.unwrap(),
            Enrolled::Full
        );
        // Renewal is not blocked by the cap.
        assert_eq!(a.enroll(&b, U, P, 1).await.unwrap(), Enrolled::Renewed);
    }
}
