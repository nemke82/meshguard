//! MeshGuard relay enrollment service (ADR-007 §3).
//!
//! `GET /v1/challenge`, `POST /v1/enroll`, `DELETE /v1/account`, behind the
//! TLS reverse proxy. It logs counts and errors only: no IP addresses, no
//! usernames.

mod accounts;
mod dynsec;
mod pow;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use accounts::{valid_password, valid_username, Accounts, Broker, Deleted, Enrolled};

/// Rate limits for enrollment (new accounts and renewals alike).
#[derive(Clone, Copy)]
pub struct Limits {
    pub per_ip_per_hour: u32,
    pub global_per_hour: u32,
}

struct Window {
    start: Instant,
    per_ip: HashMap<IpAddr, u32>,
    global: u32,
}

impl Window {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            per_ip: HashMap::new(),
            global: 0,
        }
    }

    /// Count one request from `ip` against `per_ip` / `global` per hour.
    fn admit(&mut self, ip: Option<IpAddr>, per_ip: u32, global: u32) -> bool {
        if self.start.elapsed() >= Duration::from_secs(3600) {
            *self = Self::new();
        }
        if self.global >= global {
            return false;
        }
        if let Some(ip) = ip {
            let n = self.per_ip.entry(ip).or_default();
            if *n >= per_ip {
                return false;
            }
            *n += 1;
        }
        self.global += 1;
        true
    }
}

pub struct App<B> {
    accounts: Accounts,
    broker: B,
    /// Serializes create / renew / delete / reconcile.
    op: tokio::sync::Mutex<()>,
    challenges: Mutex<pow::Challenges>,
    /// Enrollment attempts per hour.
    window: Mutex<Window>,
    /// Challenge requests per hour (each is cheap, but bounded too).
    challenge_window: Mutex<Window>,
    limits: Limits,
    difficulty: u32,
}

impl<B: Broker> App<B> {
    pub fn new(accounts: Accounts, broker: B, limits: Limits, difficulty: u32) -> Self {
        Self {
            accounts,
            broker,
            op: tokio::sync::Mutex::new(()),
            challenges: Mutex::new(pow::Challenges::new()),
            window: Mutex::new(Window::new()),
            challenge_window: Mutex::new(Window::new()),
            limits,
            difficulty,
        }
    }

    /// Count an enrollment attempt from `ip`; false when over a limit.
    fn admit(&self, ip: Option<IpAddr>) -> bool {
        self.window.lock().unwrap_or_else(|e| e.into_inner()).admit(
            ip,
            self.limits.per_ip_per_hour,
            self.limits.global_per_hour,
        )
    }

    /// Count a challenge request: 3x the enrollment limits (a phone may
    /// need a new challenge when one expires).
    fn admit_challenge(&self, ip: Option<IpAddr>) -> bool {
        self.challenge_window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(
                ip,
                self.limits.per_ip_per_hour.saturating_mul(3),
                self.limits.global_per_hour.saturating_mul(3),
            )
    }

    pub async fn reconcile(&self) {
        let _op = self.op.lock().await;
        match self.accounts.reconcile(&self.broker, unix_now()).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("reconcile: removed {n} account(s)"),
            Err(e) => tracing::warn!("reconcile failed: {e}"),
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The client address as the local reverse proxy reports it.
fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next_back()?
        .trim()
        .parse()
        .ok()
}

fn reply(code: StatusCode, status: &str) -> Response {
    (code, Json(json!({ "status": status }))).into_response()
}

async fn challenge<B: Broker + Send + Sync + 'static>(
    State(app): State<Arc<App<B>>>,
    headers: HeaderMap,
) -> Response {
    if !app.admit_challenge(client_ip(&headers)) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "try_later");
    }
    let Some(nonce) = app
        .challenges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue(Instant::now())
    else {
        return reply(StatusCode::TOO_MANY_REQUESTS, "try_later");
    };
    Json(json!({
        "nonce": nonce,
        "difficulty": app.difficulty,
        "expires_in": pow::CHALLENGE_TTL.as_secs(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct EnrollReq {
    username: String,
    password: String,
    nonce: String,
    solution: String,
}

async fn enroll<B: Broker + Send + Sync + 'static>(
    State(app): State<Arc<App<B>>>,
    headers: HeaderMap,
    Json(req): Json<EnrollReq>,
) -> Response {
    if !valid_username(&req.username) || !valid_password(&req.password) || req.solution.len() > 64 {
        return reply(StatusCode::BAD_REQUEST, "invalid");
    }
    let fresh = app
        .challenges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(&req.nonce, Instant::now());
    if !fresh || pow::work(&req.nonce, &req.username, &req.solution) < app.difficulty {
        return reply(StatusCode::BAD_REQUEST, "bad_challenge");
    }
    if !app.admit(client_ip(&headers)) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "try_later");
    }
    let _op = app.op.lock().await;
    match app
        .accounts
        .enroll(&app.broker, &req.username, &req.password, unix_now())
        .await
    {
        Ok(Enrolled::Created) => {
            tracing::info!("account created");
            reply(StatusCode::OK, "created")
        }
        Ok(Enrolled::Renewed) => reply(StatusCode::OK, "renewed"),
        Ok(Enrolled::Conflict) => reply(StatusCode::CONFLICT, "taken"),
        Ok(Enrolled::Full) => reply(StatusCode::TOO_MANY_REQUESTS, "try_later"),
        Err(e) => {
            tracing::warn!("enroll failed: {e}");
            reply(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}

#[derive(Deserialize)]
struct DeleteReq {
    username: String,
    password: String,
}

async fn delete_account<B: Broker + Send + Sync + 'static>(
    State(app): State<Arc<App<B>>>,
    Json(req): Json<DeleteReq>,
) -> Response {
    if !valid_username(&req.username) || !valid_password(&req.password) {
        return reply(StatusCode::BAD_REQUEST, "invalid");
    }
    let _op = app.op.lock().await;
    match app
        .accounts
        .delete(&app.broker, &req.username, &req.password)
        .await
    {
        Ok(Deleted::Removed) => {
            tracing::info!("account deleted");
            reply(StatusCode::OK, "deleted")
        }
        Ok(Deleted::Gone) => reply(StatusCode::OK, "gone"),
        Ok(Deleted::Forbidden) => reply(StatusCode::FORBIDDEN, "forbidden"),
        Err(e) => {
            tracing::warn!("delete failed: {e}");
            reply(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}

pub fn router<B: Broker + Send + Sync + 'static>(app: Arc<App<B>>) -> Router {
    Router::new()
        .route("/v1/challenge", get(challenge::<B>))
        .route("/v1/enroll", post(enroll::<B>))
        .route("/v1/account", delete(delete_account::<B>))
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .with_state(app)
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(env("RUST_LOG", "info"))
        .without_time()
        .init();
    let admin_password = std::env::var("DYNSEC_ADMIN_PASSWORD").expect("DYNSEC_ADMIN_PASSWORD");
    let broker = dynsec::Dynsec::connect(
        &env("BROKER_HOST", "127.0.0.1"),
        env_num("BROKER_PORT", 1884),
        &env("DYNSEC_ADMIN_USER", "admin"),
        &admin_password,
    )
    .await;
    // The broker may still be starting.
    let mut tries = 0;
    while let Err(e) = broker.setup().await {
        tries += 1;
        if tries > 30 {
            panic!("broker setup failed: {e}");
        }
        tracing::info!("waiting for the broker ({e})");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let accounts = Accounts::open(
        &env("DB_PATH", "/data/accounts.db"),
        env_num("MAX_ACCOUNTS", 10_000),
    )
    .expect("account database");
    let app = Arc::new(App::new(
        accounts,
        broker,
        Limits {
            per_ip_per_hour: env_num("ENROLL_PER_IP_PER_HOUR", 10),
            global_per_hour: env_num("ENROLL_GLOBAL_PER_HOUR", 500),
        },
        env_num("POW_BITS", 20),
    ));
    app.reconcile().await;
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            a.reconcile().await;
        }
    });
    let addr = env("LISTEN", "127.0.0.1:8080");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("listen");
    tracing::info!("enrollment service on {addr}");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use accounts::tests::{FakeBroker, P, U};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(per_ip: u32) -> Arc<App<FakeBroker>> {
        Arc::new(App::new(
            Accounts::open(":memory:", 100).unwrap(),
            FakeBroker::default(),
            Limits {
                per_ip_per_hour: per_ip,
                global_per_hour: 100,
            },
            8,
        ))
    }

    async fn call(
        app: &Arc<App<FakeBroker>>,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-forwarded-for", "203.0.113.7")
            .body(if body.is_null() {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        let res = router(app.clone()).oneshot(req).await.unwrap();
        let code = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    use serde_json::Value;

    async fn enroll_once(app: &Arc<App<FakeBroker>>, user: &str, pw: &str) -> (StatusCode, Value) {
        let (_, c) = call(app, "GET", "/v1/challenge", Value::Null).await;
        let nonce = c["nonce"].as_str().unwrap().to_string();
        let solution = pow::solve(&nonce, user, c["difficulty"].as_u64().unwrap() as u32);
        call(
            app,
            "POST",
            "/v1/enroll",
            json!({"username": user, "password": pw, "nonce": nonce, "solution": solution}),
        )
        .await
    }

    #[tokio::test]
    async fn enroll_renew_and_delete_over_http() {
        let a = app(10);
        assert_eq!(enroll_once(&a, U, P).await.1["status"], "created");
        assert_eq!(enroll_once(&a, U, P).await.1["status"], "renewed");
        let other = "ffffffffffffffffffffffffffffffff";
        assert_eq!(enroll_once(&a, U, other).await.0, StatusCode::CONFLICT);
        let (code, v) = call(
            &a,
            "DELETE",
            "/v1/account",
            json!({"username": U, "password": other}),
        )
        .await;
        assert_eq!(
            (code, v["status"].as_str()),
            (StatusCode::FORBIDDEN, Some("forbidden"))
        );
        let (_, v) = call(
            &a,
            "DELETE",
            "/v1/account",
            json!({"username": U, "password": P}),
        )
        .await;
        assert_eq!(v["status"], "deleted");
        let (_, v) = call(
            &a,
            "DELETE",
            "/v1/account",
            json!({"username": U, "password": P}),
        )
        .await;
        assert_eq!(v["status"], "gone");
    }

    #[tokio::test]
    async fn challenges_cannot_be_skipped_or_reused() {
        let a = app(10);
        let (_, c) = call(&a, "GET", "/v1/challenge", Value::Null).await;
        let nonce = c["nonce"].as_str().unwrap().to_string();
        let (code, _) = call(
            &a,
            "POST",
            "/v1/enroll",
            json!({"username": U, "password": P, "nonce": nonce, "solution": "x"}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "wrong solution");
        // The nonce was used up by the failed attempt too.
        let s = pow::solve(&nonce, U, 8);
        let (code, _) = call(
            &a,
            "POST",
            "/v1/enroll",
            json!({"username": U, "password": P, "nonce": nonce, "solution": s}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // Garbage is refused before any work.
        let (code, _) = call(
            &a,
            "POST",
            "/v1/enroll",
            json!({"username": "admin", "password": P, "nonce": "n", "solution": "s"}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn challenge_requests_are_rate_limited() {
        let a = app(2);
        for _ in 0..6 {
            assert_eq!(
                call(&a, "GET", "/v1/challenge", Value::Null).await.0,
                StatusCode::OK
            );
        }
        assert_eq!(
            call(&a, "GET", "/v1/challenge", Value::Null).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn per_ip_limit_applies() {
        let a = app(2);
        assert_eq!(enroll_once(&a, U, P).await.0, StatusCode::OK);
        assert_eq!(enroll_once(&a, U, P).await.0, StatusCode::OK);
        assert_eq!(enroll_once(&a, U, P).await.0, StatusCode::TOO_MANY_REQUESTS);
    }
}
