//! The outbound HTTP client.
//!
//! A deliberately small HTTP/1.1 `POST` over `tokio-rustls`, because the
//! security properties wanted here are properties of the *connect*, which
//! general-purpose clients hide:
//!
//! * the name is resolved once per attempt, every answer is checked against
//!   [`policy::blocked_reason`], and the connection goes to **that address**
//!   rather than to a second lookup (DNS rebinding);
//! * the address is checked again by the dialer at the moment of connecting and
//!   once more on the connected socket;
//! * redirects are never followed: a `3xx` is a failure, so a receiver cannot
//!   bounce the request to an internal address;
//! * everything, DNS included, runs under one deadline;
//! * at most [`MAX_HEAD_BYTES`] of the response are ever read. The body is not
//!   consumed at all: delivery succeeds or fails on the status line.
//!
//! Errors never contain the URL, a path or a header value; see [`DeliveryError`].

use super::policy::{self, Host, Target};
use rustls::pki_types::ServerName;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Most response bytes read. A status line and headers fit in a fraction of this.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Longest a single connection attempt may take, within the overall deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A boxed future, so the traits below can be used as `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Turns a host name into addresses. Replaceable so tests can script DNS.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, std::io::Result<Vec<IpAddr>>>;
}

/// The system resolver.
#[derive(Debug, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, std::io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host, 0)).await?;
            Ok(addrs.map(|a| a.ip()).collect())
        })
    }
}

/// Opens a TCP connection to an address that has already been validated.
pub trait Dialer: Send + Sync {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<TcpStream>>;
}

/// The real dialer. It applies the address policy itself, at the point of
/// connecting, so no caller can reach `connect` with an unchecked address.
#[derive(Debug, Clone, Copy)]
pub struct TcpDialer {
    pub allow_private: bool,
}

impl Dialer for TcpDialer {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<TcpStream>> {
        Box::pin(async move {
            let refuse = |what: &str| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, what.to_string())
            };
            if !self.allow_private && policy::blocked_reason(addr.ip()).is_some() {
                return Err(refuse("address refused at connect time"));
            }
            let stream = TcpStream::connect(addr).await?;
            // The socket that actually connected, not the one asked for.
            if !self.allow_private
                && stream
                    .peer_addr()
                    .map_or(true, |p| policy::blocked_reason(p.ip()).is_some())
            {
                return Err(refuse("connected peer refused"));
            }
            Ok(stream)
        })
    }
}

/// Why a delivery attempt failed. `Display` is safe to log: it names a category
/// and, for a blocked address, the range, never the URL or any header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryError {
    /// The destination is in a range outbound traffic is not allowed to reach.
    Blocked(&'static str),
    /// The name did not resolve.
    Resolve,
    /// No connection could be made.
    Connect(std::io::ErrorKind),
    /// The TLS handshake failed (for example an untrusted certificate).
    Tls(String),
    /// The overall deadline passed.
    Timeout,
    /// Reading or writing failed.
    Io(std::io::ErrorKind),
    /// The head of the response was malformed or larger than [`MAX_HEAD_BYTES`].
    BadResponse(&'static str),
    /// The receiver answered with this non-success status.
    Status(u16),
    /// The receiver answered with a redirect, which is never followed.
    Redirect(u16),
}

impl DeliveryError {
    /// Whether another attempt could succeed.
    pub const fn retryable(&self) -> bool {
        match self {
            Self::Resolve | Self::Connect(_) | Self::Timeout | Self::Io(_) | Self::Tls(_) => true,
            Self::Status(code) => *code == 429 || *code >= 500 || *code == 408,
            Self::Blocked(_) | Self::BadResponse(_) | Self::Redirect(_) => false,
        }
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(why) => write!(f, "destination refused: {why}"),
            Self::Resolve => f.write_str("host name did not resolve"),
            Self::Connect(kind) => write!(f, "could not connect ({kind})"),
            Self::Tls(why) => write!(f, "TLS failure ({why})"),
            Self::Timeout => f.write_str("timed out"),
            Self::Io(kind) => write!(f, "connection error ({kind})"),
            Self::BadResponse(why) => write!(f, "bad response ({why})"),
            Self::Status(code) => write!(f, "receiver answered {code}"),
            Self::Redirect(code) => {
                write!(f, "receiver answered {code}; redirects are not followed")
            }
        }
    }
}

impl std::error::Error for DeliveryError {}

/// The HTTP client. Cheap to share: it holds no connection.
pub struct Client {
    resolver: Arc<dyn Resolver>,
    dialer: Arc<dyn Dialer>,
    tls: Arc<rustls::ClientConfig>,
    allow_private: bool,
    timeout: Duration,
}

impl Client {
    pub fn new(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn Dialer>,
        tls: Arc<rustls::ClientConfig>,
        allow_private: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            resolver,
            dialer,
            tls,
            allow_private,
            timeout,
        }
    }

    /// `POST` `body` to `target` and return the response status.
    ///
    /// # Errors
    ///
    /// See [`DeliveryError`]. A `2xx` is `Ok`; every other status, including
    /// redirects, is an error.
    pub async fn post(
        &self,
        target: &Target,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<u16, DeliveryError> {
        let status = tokio::time::timeout(self.timeout, self.exchange(target, headers, body))
            .await
            .map_err(|_| DeliveryError::Timeout)??;
        match status {
            200..=299 => Ok(status),
            300..=399 => Err(DeliveryError::Redirect(status)),
            _ => Err(DeliveryError::Status(status)),
        }
    }

    /// Resolve and validate. Every address must pass: a name with one public and
    /// one internal answer is refused outright rather than filtered, because the
    /// refusal is the more useful signal to the operator.
    async fn validated_addrs(&self, target: &Target) -> Result<Vec<IpAddr>, DeliveryError> {
        let ips = match &target.host {
            Host::Ip(ip) => vec![*ip],
            Host::Name(name) => self
                .resolver
                .resolve(name)
                .await
                .map_err(|_| DeliveryError::Resolve)?,
        };
        if ips.is_empty() {
            return Err(DeliveryError::Resolve);
        }
        if !self.allow_private {
            for ip in &ips {
                if let Some(why) = policy::blocked_reason(*ip) {
                    return Err(DeliveryError::Blocked(why));
                }
            }
        }
        Ok(ips)
    }

    async fn connect(&self, ips: &[IpAddr], port: u16) -> Result<TcpStream, DeliveryError> {
        let mut last = DeliveryError::Resolve;
        for ip in ips {
            // The address validated above, handed straight to the dialer: there
            // is no second lookup between the check and the connection.
            let addr = SocketAddr::new(*ip, port);
            match tokio::time::timeout(CONNECT_TIMEOUT, self.dialer.dial(addr)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(DeliveryError::Blocked("address refused at connect time"));
                }
                Ok(Err(e)) => last = DeliveryError::Connect(e.kind()),
                Err(_) => last = DeliveryError::Connect(std::io::ErrorKind::TimedOut),
            }
        }
        Err(last)
    }

    async fn exchange(
        &self,
        target: &Target,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<u16, DeliveryError> {
        let ips = self.validated_addrs(target).await?;
        let tcp = self.connect(&ips, target.port).await?;
        let request = build_request(target, headers, body);
        if target.https {
            let name = match &target.host {
                Host::Name(n) => ServerName::try_from(n.clone()),
                Host::Ip(ip) => Ok(ServerName::IpAddress((*ip).into())),
            }
            .map_err(|_| DeliveryError::Tls("invalid server name".to_string()))?;
            let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls));
            let tls = connector
                .connect(name, tcp)
                .await
                .map_err(|e| DeliveryError::Tls(tls_summary(&e)))?;
            round_trip(tls, &request).await
        } else {
            round_trip(tcp, &request).await
        }
    }

    /// Whether the client was built to reach private destinations (reported in
    /// the startup log).
    pub const fn allows_private(&self) -> bool {
        self.allow_private
    }
}

/// A short description of a handshake failure. The rustls error names the
/// certificate problem (`UnknownIssuer`, `NotValidForName`) and nothing secret.
fn tls_summary(e: &std::io::Error) -> String {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .map_or_else(|| format!("{}", e.kind()), |err| format!("{err}"))
}

/// Header names and values come from this crate (hex, a UUID, fixed words), but a
/// value with a line break would still split the request, so refuse one.
fn build_request(target: &Target, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(b"POST ");
    out.extend_from_slice(target.path.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    out.extend_from_slice(target.host_header().as_bytes());
    out.extend_from_slice(b"\r\nConnection: close\r\nContent-Length: ");
    out.extend_from_slice(body.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        if name
            .bytes()
            .chain(value.bytes())
            .any(|b| b == b'\r' || b == b'\n')
        {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

async fn round_trip<S>(mut stream: S, request: &[u8]) -> Result<u16, DeliveryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let io = |e: &std::io::Error| DeliveryError::Io(e.kind());
    // A receiver that answers early and closes (413, 401) can make the write
    // fail while a valid response is waiting, so a write error is held back and
    // only reported if there is no response to read.
    let write = async {
        stream.write_all(request).await?;
        stream.flush().await
    }
    .await;

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&buf) {
            Ok(httparse::Status::Complete(_)) => {
                return response
                    .code
                    .ok_or(DeliveryError::BadResponse("no status code"));
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => return Err(DeliveryError::BadResponse("malformed status line")),
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(DeliveryError::BadResponse("response head too large"));
        }
        let room = (MAX_HEAD_BYTES - buf.len()).min(chunk.len());
        match stream.read(&mut chunk[..room]).await {
            Ok(0) => {
                return Err(match write {
                    Err(e) => io(&e),
                    Ok(()) => DeliveryError::BadResponse("closed before a response"),
                });
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(write.err().map_or_else(|| io(&e), |w| io(&w))),
        }
    }
}

/// Trust anchors: Mozilla's roots, compiled in, plus any PEM bundle the
/// operator supplies for a private CA.
///
/// # Errors
///
/// Names the file and says why it could not be used.
pub fn build_tls_config(
    extra_ca_file: Option<&std::path::Path>,
) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = extra_ca_file {
        let pem = std::fs::read(path)
            .map_err(|e| format!("cannot read webhook_ca_file {}: {e}", path.display()))?;
        let mut added = 0usize;
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
            let cert = cert
                .map_err(|e| format!("webhook_ca_file {} is not valid PEM: {e}", path.display()))?;
            roots
                .add(cert)
                .map_err(|e| format!("webhook_ca_file {}: {e}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!(
                "webhook_ca_file {} contains no certificates",
                path.display()
            ));
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS configuration: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

use rustls::pki_types::pem::PemObject;
