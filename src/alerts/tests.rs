//! End-to-end tests of the webhook path, against receivers that run inside the
//! test process. No external network: addresses are loopback, and "public"
//! addresses are documentation-range stand-ins that a test dialer maps onto the
//! local listener (while recording what it was asked to connect to).

use super::client::{
    BoxFuture, Client, DeliveryError, Dialer, Resolver, SystemResolver, TcpDialer, build_tls_config,
};
use super::metrics::{AlertMetrics, Kind};
use super::notifier::{Deliverer, DeliveryPolicy, Notifier};
use super::payload::{Notification, PayloadFormat};
use super::policy::Target;
use super::{AlertsHandle, Clock, ManualClock, Secret};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const SECRET: &str = "0123456789abcdef-test-secret";
const TOKEN_PATH: &str = "/services/T0000/B0000/XYZ-VERY-SECRET-TOKEN";

// ── a receiver ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Seen {
    head: String,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

#[derive(Clone)]
enum Reply {
    Status(u16),
    Redirect(String),
    /// A valid head promising a gigabyte, then an endless body.
    EndlessBody,
    /// A head that never ends.
    EndlessHead,
    /// Accept and say nothing.
    Hang,
    /// Close without answering.
    Close,
}

struct Receiver {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request<S: AsyncReadExt + Unpin>(s: &mut S) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    while buf.len() < end + len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(Seen {
        head,
        body: buf[end..].to_vec(),
    })
}

async fn respond<S: AsyncWriteExt + Unpin>(s: &mut S, reply: &Reply) {
    match reply {
        Reply::Status(code) => {
            let _ = s
                .write_all(
                    format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await;
        }
        Reply::Redirect(to) => {
            let _ = s
                .write_all(
                    format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n")
                        .as_bytes(),
                )
                .await;
        }
        Reply::EndlessBody => {
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000000\r\n\r\n")
                .await;
            let junk = vec![b'x'; 65536];
            for _ in 0..4096 {
                if s.write_all(&junk).await.is_err() {
                    break;
                }
            }
        }
        Reply::EndlessHead => {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\n").await;
            let junk = vec![b'a'; 4096];
            for _ in 0..4096 {
                if s.write_all(b"X-Junk: ").await.is_err() || s.write_all(&junk).await.is_err() {
                    break;
                }
            }
        }
        Reply::Hang => tokio::time::sleep(Duration::from_secs(60)).await,
        Reply::Close => {}
    }
}

/// Plain-HTTP receiver. `script` maps the request number (from 0) to a reply;
/// the last entry repeats.
async fn receiver(script: Vec<Reply>) -> Receiver {
    serve(script, None).await
}

async fn serve(script: Vec<Reply>, tls: Option<Arc<rustls::ServerConfig>>) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let (seen2, conns2) = (Arc::clone(&seen), Arc::clone(&connections));
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let n = conns2.fetch_add(1, Ordering::SeqCst);
            let reply = script[n.min(script.len() - 1)].clone();
            let seen = Arc::clone(&seen2);
            let tls = tls.clone();
            tokio::spawn(async move {
                if let Some(cfg) = tls {
                    let Ok(mut s) = tokio_rustls::TlsAcceptor::from(cfg).accept(tcp).await else {
                        return;
                    };
                    if let Some(req) = read_request(&mut s).await {
                        seen.lock().push(req);
                        respond(&mut s, &reply).await;
                    }
                } else {
                    let mut s = tcp;
                    if let Some(req) = read_request(&mut s).await {
                        seen.lock().push(req);
                        respond(&mut s, &reply).await;
                    }
                }
            });
        }
    });
    Receiver {
        addr,
        seen,
        connections,
        task,
    }
}

// ── scripted DNS, and a dialer that records ─────────────────────────────────

/// Answers from a list per call; the last answer repeats. Counts calls.
struct ScriptedResolver {
    answers: Mutex<Vec<Vec<IpAddr>>>,
    calls: AtomicUsize,
}

impl ScriptedResolver {
    fn new(answers: &[&[&str]]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                answers
                    .iter()
                    .map(|a| a.iter().map(|s| s.parse().unwrap()).collect())
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
        })
    }
}

impl Resolver for ScriptedResolver {
    fn resolve<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, std::io::Result<Vec<IpAddr>>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let answers = self.answers.lock();
        let answer = answers[n.min(answers.len() - 1)].clone();
        Box::pin(async move { Ok(answer) })
    }
}

/// Maps every address onto the local receiver and records what it was asked for.
struct MappingDialer {
    to: SocketAddr,
    asked: Mutex<Vec<SocketAddr>>,
}

impl Dialer for MappingDialer {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<TcpStream>> {
        self.asked.lock().push(addr);
        Box::pin(async move { TcpStream::connect(self.to).await })
    }
}

// ── building clients and deliverers ─────────────────────────────────────────

fn plain_tls() -> Arc<rustls::ClientConfig> {
    build_tls_config(None).unwrap()
}

fn policy(url: &str, attempts: u32) -> DeliveryPolicy {
    DeliveryPolicy {
        target: Target::parse(url).unwrap(),
        secret: Some(Secret::new(SECRET)),
        format: PayloadFormat::Generic,
        max_attempts: attempts,
        backoff_base: Duration::from_millis(5),
        backoff_cap: Duration::from_millis(20),
    }
}

fn clock() -> Arc<ManualClock> {
    ManualClock::new(Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap())
}

struct Rig {
    deliverer: Deliverer,
    metrics: Arc<AlertMetrics>,
    clock: Arc<ManualClock>,
}

fn rig(
    url: &str,
    attempts: u32,
    allow_private: bool,
    resolver: Arc<dyn Resolver>,
    dialer: Arc<dyn Dialer>,
    tls: Arc<rustls::ClientConfig>,
    timeout: Duration,
) -> Rig {
    let metrics = Arc::new(AlertMetrics::default());
    let clock = clock();
    let client = Client::new(resolver, dialer, tls, allow_private, timeout);
    let deliverer = Deliverer::new(
        client,
        policy(url, attempts),
        clock.clone(),
        Arc::clone(&metrics),
    );
    Rig {
        deliverer,
        metrics,
        clock,
    }
}

/// A rig that talks to a loopback receiver (allowed explicitly).
fn local_rig(addr: SocketAddr, attempts: u32, timeout: Duration) -> Rig {
    rig(
        &format!("http://127.0.0.1:{}{TOKEN_PATH}", addr.port()),
        attempts,
        true,
        Arc::new(SystemResolver),
        Arc::new(TcpDialer {
            allow_private: true,
        }),
        plain_tls(),
        timeout,
    )
}

fn note(kind: Kind) -> Notification {
    let mut n = Notification::test(Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap());
    n.kind = kind;
    n
}

async fn deliver(rig: &Rig, n: &Notification) -> Result<u32, DeliveryError> {
    let (_tx, mut rx) = watch::channel(false);
    rig.deliverer.deliver(n, &mut rx).await
}

fn expected_signature(timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

// ── delivery behaviour ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_delivery_is_signed_and_the_signature_verifies() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let rig = local_rig(rx.addr, 1, Duration::from_secs(5));
    let n = note(Kind::Alert);
    assert_eq!(deliver(&rig, &n).await, Ok(1));

    let seen = rx.seen.lock()[0].clone();
    assert!(
        seen.head
            .starts_with(&format!("POST {TOKEN_PATH} HTTP/1.1"))
    );
    assert_eq!(
        seen.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(seen.header("x-mallard-event").as_deref(), Some("alert"));
    assert_eq!(
        seen.header("x-mallard-delivery").as_deref(),
        Some(n.delivery_id.as_str())
    );
    let ts = seen.header("x-mallard-timestamp").unwrap();
    assert_eq!(ts, rig.clock.now().timestamp().to_string());
    assert_eq!(
        seen.header("x-mallard-signature").unwrap(),
        expected_signature(&ts, &seen.body)
    );
    // A tampered body or a different timestamp does not verify.
    assert_ne!(
        seen.header("x-mallard-signature").unwrap(),
        expected_signature(&ts, b"{}")
    );
    assert_ne!(
        seen.header("x-mallard-signature").unwrap(),
        expected_signature("1", &seen.body)
    );
    let json: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(json["delivery_id"], n.delivery_id.as_str());
    assert_eq!(rig.metrics.sent_count(Kind::Alert), 1);
    assert_eq!(
        rig.metrics.last_success_unix(),
        u64::try_from(rig.clock.now().timestamp()).unwrap()
    );
}

#[tokio::test]
async fn without_a_secret_there_is_no_signature_header() {
    let rx = receiver(vec![Reply::Status(204)]).await;
    let mut rig = local_rig(rx.addr, 1, Duration::from_secs(5));
    rig.deliverer = {
        let mut p = policy(&format!("http://127.0.0.1:{}/x", rx.addr.port()), 1);
        p.secret = None;
        let client = Client::new(
            Arc::new(SystemResolver),
            Arc::new(TcpDialer {
                allow_private: true,
            }),
            plain_tls(),
            true,
            Duration::from_secs(5),
        );
        Deliverer::new(client, p, rig.clock.clone(), Arc::clone(&rig.metrics))
    };
    deliver(&rig, &note(Kind::Report)).await.unwrap();
    assert!(rx.seen.lock()[0].header("x-mallard-signature").is_none());
}

#[tokio::test]
async fn transient_failures_are_retried_with_a_fresh_signature_and_the_same_delivery_id() {
    let rx = receiver(vec![
        Reply::Status(503),
        Reply::Status(500),
        Reply::Status(200),
    ])
    .await;
    let rig = local_rig(rx.addr, 4, Duration::from_secs(5));
    let n = note(Kind::Alert);
    assert_eq!(deliver(&rig, &n).await, Ok(3));
    let seen = rx.seen.lock().clone();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        assert_eq!(
            s.header("x-mallard-delivery").as_deref(),
            Some(n.delivery_id.as_str())
        );
        let ts = s.header("x-mallard-timestamp").unwrap();
        assert_eq!(
            s.header("x-mallard-signature").unwrap(),
            expected_signature(&ts, &s.body)
        );
    }
    assert_eq!(rig.metrics.sent_count(Kind::Alert), 1);
    assert_eq!(rig.metrics.failed_count(Kind::Alert), 0);
}

#[tokio::test]
async fn it_gives_up_after_the_attempt_limit() {
    let rx = receiver(vec![Reply::Status(500)]).await;
    let rig = local_rig(rx.addr, 3, Duration::from_secs(5));
    let started = Instant::now();
    assert_eq!(
        deliver(&rig, &note(Kind::Report)).await,
        Err(DeliveryError::Status(500))
    );
    assert_eq!(rx.seen.lock().len(), 3, "exactly max_attempts requests");
    assert_eq!(rig.metrics.failed_count(Kind::Report), 1);
    assert_eq!(rig.metrics.sent_count(Kind::Report), 0);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_client_error_is_not_retried() {
    let rx = receiver(vec![Reply::Status(400)]).await;
    let rig = local_rig(rx.addr, 5, Duration::from_secs(5));
    assert_eq!(
        deliver(&rig, &note(Kind::Alert)).await,
        Err(DeliveryError::Status(400))
    );
    assert_eq!(rx.seen.lock().len(), 1);
}

#[tokio::test]
async fn a_closed_port_is_retried_then_abandoned() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let rig = local_rig(addr, 2, Duration::from_secs(5));
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert!(matches!(err, DeliveryError::Connect(_)), "{err}");
    assert_eq!(rig.metrics.failed_count(Kind::Alert), 1);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let elsewhere = receiver(vec![Reply::Status(200)]).await;
    let first = receiver(vec![Reply::Redirect(format!(
        "http://127.0.0.1:{}/internal",
        elsewhere.addr.port()
    ))])
    .await;
    let rig = local_rig(first.addr, 3, Duration::from_secs(5));
    assert_eq!(
        deliver(&rig, &note(Kind::Alert)).await,
        Err(DeliveryError::Redirect(302))
    );
    assert_eq!(
        first.seen.lock().len(),
        1,
        "a redirect is permanent: no retries"
    );
    assert_eq!(
        elsewhere.connections.load(Ordering::SeqCst),
        0,
        "the Location was never contacted"
    );
}

#[tokio::test]
async fn an_endless_body_is_ignored_and_an_endless_head_is_refused() {
    let big = receiver(vec![Reply::EndlessBody]).await;
    let rig = local_rig(big.addr, 1, Duration::from_secs(5));
    let started = Instant::now();
    assert_eq!(
        deliver(&rig, &note(Kind::Alert)).await,
        Ok(1),
        "the status line decides; the body is never read"
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    let endless = receiver(vec![Reply::EndlessHead]).await;
    let rig = local_rig(endless.addr, 3, Duration::from_secs(5));
    let started = Instant::now();
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert!(matches!(err, DeliveryError::BadResponse(_)), "{err}");
    assert_eq!(
        endless.seen.lock().len(),
        1,
        "a malformed response is not retried"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_receiver_that_never_answers_times_out() {
    let rx = receiver(vec![Reply::Hang]).await;
    let rig = local_rig(rx.addr, 2, Duration::from_millis(300));
    let started = Instant::now();
    assert_eq!(
        deliver(&rig, &note(Kind::Alert)).await,
        Err(DeliveryError::Timeout)
    );
    assert_eq!(
        rx.seen.lock().len(),
        2,
        "timeouts are retried, each bounded"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_connection_closed_without_a_response_is_a_failure() {
    let rx = receiver(vec![Reply::Close]).await;
    let rig = local_rig(rx.addr, 1, Duration::from_secs(5));
    assert!(deliver(&rig, &note(Kind::Alert)).await.is_err());
}

// ── address policy at connect time ──────────────────────────────────────────

fn strict_rig(
    url: &str,
    resolver: Arc<dyn Resolver>,
    dialer: Arc<dyn Dialer>,
    attempts: u32,
) -> Rig {
    rig(
        url,
        attempts,
        false,
        resolver,
        dialer,
        plain_tls(),
        Duration::from_secs(5),
    )
}

#[tokio::test]
async fn literal_internal_addresses_are_refused_without_connecting() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    for url in [
        format!("http://127.0.0.1:{}/x", rx.addr.port()),
        "http://169.254.169.254/latest/meta-data".to_string(),
        "http://[::1]/x".to_string(),
        "http://[fd00:ec2::254]/x".to_string(),
        "http://10.1.2.3/x".to_string(),
    ] {
        let dialer = Arc::new(MappingDialer {
            to: rx.addr,
            asked: Mutex::default(),
        });
        let rig = strict_rig(&url, Arc::new(SystemResolver), dialer.clone(), 3);
        let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
        assert!(matches!(err, DeliveryError::Blocked(_)), "{url}: {err}");
        assert!(dialer.asked.lock().is_empty(), "{url}: nothing was dialled");
        assert_eq!(rig.metrics.failed_count(Kind::Alert), 1);
    }
    assert_eq!(rx.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_hostname_that_resolves_to_loopback_is_refused() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let resolver = ScriptedResolver::new(&[&["127.0.0.1"]]);
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    let rig = strict_rig(
        "https://innocent.example/hook",
        resolver.clone(),
        dialer.clone(),
        3,
    );
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert_eq!(err, DeliveryError::Blocked("loopback address"));
    assert!(dialer.asked.lock().is_empty());
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "a refusal is permanent: no retry"
    );
    assert_eq!(rx.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn localhost_through_the_system_resolver_is_refused() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let rig = rig(
        &format!("http://localhost:{}/x", rx.addr.port()),
        1,
        false,
        Arc::new(SystemResolver),
        Arc::new(TcpDialer {
            allow_private: false,
        }),
        plain_tls(),
        Duration::from_secs(5),
    );
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert!(matches!(err, DeliveryError::Blocked(_)), "{err}");
    assert_eq!(rx.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_internal_answer_among_public_ones_refuses_the_name() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let resolver = ScriptedResolver::new(&[&["203.0.113.10", "169.254.169.254"]]);
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    let rig = strict_rig("http://mixed.example/x", resolver, dialer.clone(), 1);
    assert_eq!(
        deliver(&rig, &note(Kind::Alert)).await,
        Err(DeliveryError::Blocked("cloud metadata address"))
    );
    assert!(dialer.asked.lock().is_empty());
}

#[tokio::test]
async fn the_private_flag_lifts_the_restriction() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let resolver = ScriptedResolver::new(&[&["127.0.0.1"]]);
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    let rig = rig(
        "http://internal.example/x",
        1,
        true,
        resolver,
        dialer,
        plain_tls(),
        Duration::from_secs(5),
    );
    assert_eq!(deliver(&rig, &note(Kind::Alert)).await, Ok(1));
}

#[tokio::test]
async fn dns_rebinding_cannot_redirect_a_validated_connection() {
    // First answer: a public-looking address. Every later answer: loopback.
    let rx = receiver(vec![Reply::Status(503), Reply::Status(200)]).await;
    let resolver = ScriptedResolver::new(&[&["203.0.113.10"], &["127.0.0.1"]]);
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    let rig = strict_rig(
        "http://rebind.example:8080/x",
        resolver.clone(),
        dialer.clone(),
        3,
    );

    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();

    // Attempt 1: resolved once, validated, and dialled exactly that address.
    // The receiver answered 503, so a retry followed.
    assert_eq!(
        dialer.asked.lock().clone(),
        vec!["203.0.113.10:8080".parse::<SocketAddr>().unwrap()],
        "only the validated address was ever dialled"
    );
    // Attempt 2 resolved again (one lookup per attempt, never between the check
    // and the connect), got loopback, and was refused before dialling.
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert_eq!(err, DeliveryError::Blocked("loopback address"));
    assert_eq!(rx.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_real_dialer_refuses_an_internal_address_even_if_the_caller_did_not() {
    let rx = receiver(vec![Reply::Status(200)]).await;
    let dialer = TcpDialer {
        allow_private: false,
    };
    let err = dialer.dial(rx.addr).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(rx.connections.load(Ordering::SeqCst), 0);
    let allowed = TcpDialer {
        allow_private: true,
    };
    assert!(allowed.dial(rx.addr).await.is_ok());
}

// ── TLS ─────────────────────────────────────────────────────────────────────

struct Pki {
    cert_pem: String,
    server: Arc<rustls::ServerConfig>,
}

fn pki(names: &[&str]) -> Pki {
    let key = rcgen::KeyPair::generate().unwrap();
    let params =
        rcgen::CertificateParams::new(names.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
            .unwrap();
    let cert = params.self_signed(&key).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        )
        .unwrap();
    Pki {
        cert_pem: cert.pem(),
        server: Arc::new(server),
    }
}

fn trusting(pki: &Pki, dir: &tempfile::TempDir) -> Arc<rustls::ClientConfig> {
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, &pki.cert_pem).unwrap();
    build_tls_config(Some(&path)).unwrap()
}

#[tokio::test]
async fn https_delivers_to_a_public_address_with_a_verified_certificate() {
    let pki = pki(&["hook.test"]);
    let dir = tempfile::tempdir().unwrap();
    let rx = serve(vec![Reply::Status(200)], Some(pki.server.clone())).await;
    let resolver = ScriptedResolver::new(&[&["203.0.113.10"]]);
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    // allow_private is false throughout: the only address in play is "public".
    let rig = rig(
        &format!("https://hook.test{TOKEN_PATH}"),
        1,
        false,
        resolver,
        dialer.clone(),
        trusting(&pki, &dir),
        Duration::from_secs(5),
    );
    assert_eq!(deliver(&rig, &note(Kind::Alert)).await, Ok(1));
    let seen = rx.seen.lock()[0].clone();
    assert_eq!(seen.header("host").as_deref(), Some("hook.test"));
    assert_eq!(
        dialer.asked.lock().clone(),
        vec!["203.0.113.10:443".parse::<SocketAddr>().unwrap()]
    );
}

#[tokio::test]
async fn an_untrusted_certificate_is_refused() {
    let pki = pki(&["hook.test"]);
    let rx = serve(vec![Reply::Status(200)], Some(pki.server.clone())).await;
    let dialer = Arc::new(MappingDialer {
        to: rx.addr,
        asked: Mutex::default(),
    });
    let rig = rig(
        "https://hook.test/x",
        1,
        false,
        ScriptedResolver::new(&[&["203.0.113.10"]]),
        dialer,
        plain_tls(), // Mozilla roots only: the throwaway CA is not among them
        Duration::from_secs(5),
    );
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert!(matches!(err, DeliveryError::Tls(_)), "{err}");
    assert!(
        rx.seen.lock().is_empty(),
        "nothing was sent over the failed handshake"
    );
}

#[tokio::test]
async fn a_certificate_for_another_name_is_refused() {
    let pki = pki(&["other.test"]);
    let dir = tempfile::tempdir().unwrap();
    let rx = serve(vec![Reply::Status(200)], Some(pki.server.clone())).await;
    let rig = rig(
        "https://hook.test/x",
        1,
        false,
        ScriptedResolver::new(&[&["203.0.113.10"]]),
        Arc::new(MappingDialer {
            to: rx.addr,
            asked: Mutex::default(),
        }),
        trusting(&pki, &dir),
        Duration::from_secs(5),
    );
    let err = deliver(&rig, &note(Kind::Alert)).await.unwrap_err();
    assert!(matches!(err, DeliveryError::Tls(_)), "{err}");
    assert!(rx.seen.lock().is_empty());
}

#[test]
fn a_bad_ca_file_is_reported_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.pem");
    assert!(
        build_tls_config(Some(&missing))
            .unwrap_err()
            .contains("nope.pem")
    );
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "not a certificate").unwrap();
    assert!(build_tls_config(Some(&empty)).is_err());
}

// ── secrets stay out of the logs ────────────────────────────────────────────

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(Arc::clone(&self.0))
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn neither_the_secret_nor_the_url_reaches_the_logs_or_the_errors() {
    let logs = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut collected_errors = Vec::new();

    // Success, retry-then-fail, refused address, redirect, bad TLS: every path.
    let ok = receiver(vec![Reply::Status(200)]).await;
    let rig_ok = local_rig(ok.addr, 1, Duration::from_secs(2));
    deliver(&rig_ok, &note(Kind::Alert)).await.unwrap();

    let failing = receiver(vec![Reply::Status(500)]).await;
    let rig_fail = local_rig(failing.addr, 2, Duration::from_secs(2));
    collected_errors.push(
        deliver(&rig_fail, &note(Kind::Alert))
            .await
            .unwrap_err()
            .to_string(),
    );

    let redirect = receiver(vec![Reply::Redirect(format!(
        "http://127.0.0.1:1{TOKEN_PATH}"
    ))])
    .await;
    let rig_redirect = local_rig(redirect.addr, 1, Duration::from_secs(2));
    collected_errors.push(
        deliver(&rig_redirect, &note(Kind::Alert))
            .await
            .unwrap_err()
            .to_string(),
    );

    let blocked = strict_rig(
        &format!("https://169.254.169.254{TOKEN_PATH}"),
        Arc::new(SystemResolver),
        Arc::new(TcpDialer {
            allow_private: false,
        }),
        1,
    );
    collected_errors.push(
        deliver(&blocked, &note(Kind::Alert))
            .await
            .unwrap_err()
            .to_string(),
    );

    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let rig_dead = local_rig(dead_addr, 1, Duration::from_secs(2));
    collected_errors.push(
        deliver(&rig_dead, &note(Kind::Alert))
            .await
            .unwrap_err()
            .to_string(),
    );

    let pki = pki(&["hook.test"]);
    let tls = serve(vec![Reply::Status(200)], Some(pki.server.clone())).await;
    let rig_tls = rig(
        &format!("https://hook.test{TOKEN_PATH}"),
        1,
        false,
        ScriptedResolver::new(&[&["203.0.113.10"]]),
        Arc::new(MappingDialer {
            to: tls.addr,
            asked: Mutex::default(),
        }),
        plain_tls(),
        Duration::from_secs(2),
    );
    collected_errors.push(
        deliver(&rig_tls, &note(Kind::Alert))
            .await
            .unwrap_err()
            .to_string(),
    );

    // The metrics exposition and the settings' Debug output too.
    let mut metrics = String::new();
    rig_ok.metrics.write_prometheus(&mut metrics);
    let mut settings = super::AlertSettings {
        webhook_url: Some(format!("https://hooks.example.com{TOKEN_PATH}")),
        webhook_secret: Some(Secret::new(SECRET)),
        ..super::AlertSettings::default()
    };
    settings.allow_http_webhook = true;
    let settings_debug = format!("{settings:?}");

    let log = String::from_utf8(logs.0.lock().clone()).unwrap();
    assert!(
        log.contains("Webhook notification delivered"),
        "the test saw no log lines:\n{log}"
    );
    assert!(
        log.contains("target=http://127.0.0.1") || log.contains("target=https://"),
        "{log}"
    );
    for (what, text) in [
        ("logs", &log),
        ("metrics", &metrics),
        ("settings debug", &settings_debug),
        ("errors", &collected_errors.join("\n")),
    ] {
        for needle in [SECRET, "XYZ-VERY-SECRET-TOKEN", "T0000", "/services/"] {
            assert!(!text.contains(needle), "{what} leak {needle:?}:\n{text}");
        }
    }
}

// ── shutdown ────────────────────────────────────────────────────────────────

fn handle_with_worker(deliverer: Deliverer, metrics: &Arc<AlertMetrics>) -> AlertsHandle {
    let notifier = Notifier::new(8, Arc::clone(metrics));
    let (closing, rx) = watch::channel(false);
    let worker = tokio::spawn(notifier.clone().run_worker(deliverer, rx));
    AlertsHandle {
        notifier,
        closing,
        tasks: vec![worker],
    }
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_hanging_receiver_and_counts_what_it_discards() {
    let rx = receiver(vec![Reply::Hang]).await;
    let rig = local_rig(rx.addr, 5, Duration::from_secs(30));
    let metrics = Arc::clone(&rig.metrics);
    let handle = handle_with_worker(rig.deliverer, &metrics);

    handle.notifier.enqueue(note(Kind::Alert));
    handle.notifier.enqueue(note(Kind::Report));
    handle.notifier.enqueue(note(Kind::Resolved));
    // Wait until the first delivery is genuinely in flight.
    let started = Instant::now();
    while rx.connections.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no delivery started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let t = Instant::now();
    handle.shutdown(Duration::from_millis(300)).await;
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "shutdown took {:?}",
        t.elapsed()
    );
    assert_eq!(
        metrics.dropped_count(Kind::Report),
        1,
        "queued behind the hung one"
    );
    assert_eq!(metrics.dropped_count(Kind::Resolved), 1);
    assert_eq!(
        rx.connections.load(Ordering::SeqCst),
        1,
        "no retry after shutdown began"
    );
}

#[tokio::test]
async fn shutdown_lets_a_delivery_in_flight_finish_within_the_grace() {
    // The receiver answers 503 first: the retry's backoff sleep is interrupted
    // by shutdown, so the notification is abandoned promptly, not slept on.
    let rx = receiver(vec![Reply::Status(503), Reply::Status(200)]).await;
    let mut rig = local_rig(rx.addr, 5, Duration::from_secs(5));
    rig.deliverer = {
        let mut p = policy(&format!("http://127.0.0.1:{}/x", rx.addr.port()), 5);
        p.backoff_base = Duration::from_secs(30);
        p.backoff_cap = Duration::from_secs(60);
        let client = Client::new(
            Arc::new(SystemResolver),
            Arc::new(TcpDialer {
                allow_private: true,
            }),
            plain_tls(),
            true,
            Duration::from_secs(5),
        );
        Deliverer::new(client, p, rig.clock.clone(), Arc::clone(&rig.metrics))
    };
    let metrics = Arc::clone(&rig.metrics);
    let handle = handle_with_worker(rig.deliverer, &metrics);
    handle.notifier.enqueue(note(Kind::Alert));
    while rx.connections.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await; // now in the backoff sleep
    let t = Instant::now();
    handle.shutdown(Duration::from_secs(5)).await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(metrics.failed_count(Kind::Alert), 1);
}

#[tokio::test]
async fn a_dead_receiver_cannot_grow_the_queue() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let rig = local_rig(addr, 1, Duration::from_secs(1));
    let metrics = Arc::clone(&rig.metrics);
    let notifier = Notifier::new(4, Arc::clone(&metrics));
    // No worker running: everything queues, and the queue stays at its bound.
    for _ in 0..100 {
        notifier.enqueue(note(Kind::Alert));
    }
    assert_eq!(notifier.len(), 4);
    assert_eq!(metrics.dropped_count(Kind::Alert), 96);
    drop(rig);
}
