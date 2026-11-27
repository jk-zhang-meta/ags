//! Authenticated loopback HTTP/1.1 + SSE Streaming + WebSocket Upgrade
//! (`101 Switching Protocols`) reverse proxy for Berth v2 AGS launches.
//!
//! Enforces all Codex Review Blocker #5.3 security controls:
//! 1. Binds ONLY to `127.0.0.1:<port>` (ephemeral `:0` by default).
//! 2. Per-launch random 32-byte hex bearer secret (`ags-shim-<64hex>`) checked
//!    in constant time on every HTTP request and WebSocket upgrade.
//! 3. Validates `Host` is strictly `127.0.0.1:<port>` or `localhost:<port>`.
//! 4. Rejects ANY request carrying an `Origin` header (`403 Forbidden`).
//! 5. Strictly allowlists provider API prefixes (`/v1/`, `/v1beta/`,
//!    `/responses`, `/messages`, `/models`, `/chat/completions`, plus exact
//!    authenticated `/healthz`).
//! 6. Keeps the real Berth Ed25519 `lease_token` strictly in RAM; strips the
//!    ephemeral shim token and injects upstream Berth headers + fresh
//!    `X-Berth-Operation-ID` per request.
//! 7. Zero-buffered SSE streaming AND full-duplex bidirectional WebSocket
//!    tunneling over both `http://` and `https://` (`rustls`).
//! 8. Pre-commit `409 cage_rebind_required` detection writing ONLY opaque
//!    `RebindSignal` (NEVER `lease_token`) atomically with `0600` perms.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::berth::{
    AGS_VERSION_HEADER_VALUE, BerthError, ClientTransport, ParsedUrl, RebindSignal,
    parse_http_status_code, read_chunked_body, write_private_file_atomic,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_REBIND_BODY_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_CONNS: usize = 256;
const CLIENT_HEAD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct CageShimConfig {
    pub bind_addr: String,
    pub ingress_url: String,
    pub lease_id: String,
    pub lease_token: String,
    pub work_session_id: String,
    pub cage_tz: String,
    pub cage_country: String,
    pub cage_strict: bool,
    pub rebind_file: Option<PathBuf>,
    pub ready_file: Option<PathBuf>,
    pub upstream_timeout: Duration,
    /// When true, once a `409 cage_rebind_required` is intercepted, immediately
    /// fast-reject subsequent client SDK retry requests with non-retryable HTTP 400
    /// so `bh --auto-rebind` does not wait through 15-30s of SDK exponential backoff.
    pub abort_retries_on_rebind: bool,
}

impl fmt::Debug for CageShimConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CageShimConfig")
            .field("bind_addr", &self.bind_addr)
            .field("ingress_url", &self.ingress_url)
            .field("lease_id", &self.lease_id)
            .field("lease_token", &"[REDACTED]")
            .field("work_session_id", &self.work_session_id)
            .field("cage_tz", &self.cage_tz)
            .field("cage_country", &self.cage_country)
            .field("cage_strict", &self.cage_strict)
            .field("rebind_file", &self.rebind_file)
            .field("ready_file", &self.ready_file)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyFilePayload {
    pub listen_url: String,
    pub shim_token: String,
}

impl fmt::Debug for ReadyFilePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyFilePayload")
            .field("listen_url", &self.listen_url)
            .field("shim_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimMetricsSnapshot {
    pub requests_total: u64,
    pub requests_forwarded: u64,
    pub auth_rejected: u64,
    pub forbidden_rejected: u64,
    pub not_found_rejected: u64,
    pub ws_upgrades: u64,
    pub rebind_signals: u64,
    pub last_rebind: Option<RebindSignal>,
}

struct LeaseSecrets {
    lease_id: String,
    lease_token: String,
    work_session_id: String,
    cage_tz: String,
    cage_country: String,
    cage_strict: bool,
}

impl fmt::Debug for LeaseSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseSecrets")
            .field("lease_id", &self.lease_id)
            .field("lease_token", &"[REDACTED]")
            .finish()
    }
}

struct ShimMetrics {
    requests_total: AtomicU64,
    requests_forwarded: AtomicU64,
    auth_rejected: AtomicU64,
    forbidden_rejected: AtomicU64,
    not_found_rejected: AtomicU64,
    ws_upgrades: AtomicU64,
    rebind_signals: AtomicU64,
    last_rebind: Mutex<Option<RebindSignal>>,
}

impl ShimMetrics {
    fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            requests_forwarded: AtomicU64::new(0),
            auth_rejected: AtomicU64::new(0),
            forbidden_rejected: AtomicU64::new(0),
            not_found_rejected: AtomicU64::new(0),
            ws_upgrades: AtomicU64::new(0),
            rebind_signals: AtomicU64::new(0),
            last_rebind: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> ShimMetricsSnapshot {
        ShimMetricsSnapshot {
            requests_total: self.requests_total.load(Ordering::Relaxed),
            requests_forwarded: self.requests_forwarded.load(Ordering::Relaxed),
            auth_rejected: self.auth_rejected.load(Ordering::Relaxed),
            forbidden_rejected: self.forbidden_rejected.load(Ordering::Relaxed),
            not_found_rejected: self.not_found_rejected.load(Ordering::Relaxed),
            ws_upgrades: self.ws_upgrades.load(Ordering::Relaxed),
            rebind_signals: self.rebind_signals.load(Ordering::Relaxed),
            last_rebind: self.last_rebind.lock().ok().and_then(|g| g.clone()),
        }
    }
}

struct ShimSharedState {
    bound_port: u16,
    shim_token_digest: [u8; 32],
    ingress: ParsedUrl,
    upstream_timeout: Duration,
    rebind_file: Option<PathBuf>,
    abort_retries_on_rebind: bool,
    rebind_signaled: AtomicBool,
    secrets: RwLock<LeaseSecrets>,
    metrics: ShimMetrics,
    shutdown: AtomicBool,
    active_conns: AtomicUsize,
}

pub struct CageShim;

pub struct CageShimHandle {
    listen_addr: SocketAddr,
    proxy_addr: SocketAddr,
    shim_token: String,
    state: Arc<ShimSharedState>,
    accept_thread: Option<JoinHandle<()>>,
    proxy_thread: Option<JoinHandle<()>>,
    #[cfg(unix)]
    unix_bridge_shutdown: Arc<AtomicBool>,
    #[cfg(unix)]
    unix_bridge_threads: Vec<JoinHandle<()>>,
    #[cfg(unix)]
    unix_bridge_paths: Vec<PathBuf>,
}

impl CageShim {
    pub fn start(config: CageShimConfig) -> Result<CageShimHandle, BerthError> {
        let bind_socket: SocketAddr = config.bind_addr.parse().map_err(|_| {
            BerthError::InvalidArgument(format!(
                "invalid shim bind address `{}` (expected 127.0.0.1:<port>)",
                config.bind_addr
            ))
        })?;
        if bind_socket.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(BerthError::InvalidArgument(format!(
                "cage shim refuses non-loopback bind address `{}` (must be 127.0.0.1:<port>)",
                config.bind_addr
            )));
        }
        if config.lease_token.trim().is_empty() {
            return Err(BerthError::InvalidArgument(
                "lease_token must not be empty".into(),
            ));
        }
        let ingress = ParsedUrl::parse(&config.ingress_url)?;

        let listener = TcpListener::bind(bind_socket)?;
        let listen_addr = listener.local_addr()?;

        // Companion authenticated Forward Proxy (HTTP CONNECT + SOCKS5h) bound strictly on 127.0.0.1:0.
        let proxy_listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
        let proxy_addr = proxy_listener.local_addr()?;

        // 32 random bytes (64 lowercase hex chars) from OS CSPRNG via Uuid::new_v4().
        let shim_token = format!(
            "ags-shim-{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let shim_token_digest = sha256_bytes(shim_token.as_bytes());

        let state = Arc::new(ShimSharedState {
            bound_port: listen_addr.port(),
            shim_token_digest,
            ingress,
            upstream_timeout: if config.upstream_timeout.is_zero() {
                Duration::from_secs(30)
            } else {
                config.upstream_timeout
            },
            rebind_file: config.rebind_file.clone(),
            abort_retries_on_rebind: config.abort_retries_on_rebind,
            rebind_signaled: AtomicBool::new(false),
            secrets: RwLock::new(LeaseSecrets {
                lease_id: config.lease_id,
                lease_token: config.lease_token,
                work_session_id: config.work_session_id,
                cage_tz: config.cage_tz,
                cage_country: config.cage_country,
                cage_strict: config.cage_strict,
            }),
            metrics: ShimMetrics::new(),
            shutdown: AtomicBool::new(false),
            active_conns: AtomicUsize::new(0),
        });

        if let Some(ready_path) = &config.ready_file {
            let payload = ReadyFilePayload {
                listen_url: format!("http://{listen_addr}"),
                shim_token: shim_token.clone(),
            };
            let mut bytes = serde_json::to_vec_pretty(&payload)?;
            bytes.push(b'\n');
            write_private_file_atomic(ready_path, &bytes)?;
        }

        let accept_state = Arc::clone(&state);
        let accept_thread = thread::Builder::new()
            .name("ags-cage-shim-accept".into())
            .spawn(move || accept_loop(listener, accept_state))
            .map_err(|e| BerthError::Io(format!("failed to spawn shim accept thread: {e}")))?;

        let proxy_state = Arc::clone(&state);
        let proxy_thread = thread::Builder::new()
            .name("ags-cage-proxy-accept".into())
            .spawn(move || proxy_accept_loop(proxy_listener, proxy_state))
            .map_err(|e| BerthError::Io(format!("failed to spawn proxy accept thread: {e}")))?;

        Ok(CageShimHandle {
            listen_addr,
            proxy_addr,
            shim_token,
            state,
            accept_thread: Some(accept_thread),
            proxy_thread: Some(proxy_thread),
            #[cfg(unix)]
            unix_bridge_shutdown: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            unix_bridge_threads: Vec::new(),
            #[cfg(unix)]
            unix_bridge_paths: Vec::new(),
        })
    }
}

impl CageShimHandle {
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn listen_url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }

    pub fn proxy_addr(&self) -> SocketAddr {
        self.proxy_addr
    }

    pub fn proxy_http_url(&self) -> String {
        format!("http://bh:{}@{}", self.shim_token, self.proxy_addr)
    }

    pub fn proxy_socks5_url(&self) -> String {
        format!("socks5h://bh:{}@{}", self.shim_token, self.proxy_addr)
    }

    pub fn shim_token(&self) -> &str {
        &self.shim_token
    }

    pub fn has_rebind_signal(&self) -> bool {
        self.state.rebind_signaled.load(Ordering::Acquire)
    }

    pub fn metrics(&self) -> ShimMetricsSnapshot {
        self.state.metrics.snapshot()
    }

    pub fn update_lease_token(&self, new_token: String) {
        if let Ok(mut guard) = self.state.secrets.write() {
            guard.lease_token = new_token;
        }
    }

    /// Starts private `0600` Unix Domain Socket bridges (`shim.sock` and `proxy.sock`)
    /// forwarding raw byte streams directly to the host-side `CageShim` and `ForwardProxy`
    /// so Linux/WSL `--unshare-net` sandboxes need zero external network interfaces.
    #[cfg(unix)]
    pub fn start_unix_bridges(
        &mut self,
        dir: &std::path::Path,
    ) -> Result<(PathBuf, PathBuf), BerthError> {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

        let shim_sock = dir.join("shim.sock");
        let proxy_sock = dir.join("proxy.sock");
        let _ = std::fs::remove_file(&shim_sock);
        let _ = std::fs::remove_file(&proxy_sock);

        let shim_unix = UnixListener::bind(&shim_sock)?;
        let _ = std::fs::set_permissions(&shim_sock, std::fs::Permissions::from_mode(0o600));
        let proxy_unix = UnixListener::bind(&proxy_sock)?;
        let _ = std::fs::set_permissions(&proxy_sock, std::fs::Permissions::from_mode(0o600));

        self.unix_bridge_shutdown.store(false, Ordering::SeqCst);

        let shim_target = self.listen_addr;
        let state1 = Arc::clone(&self.state);
        let bshut1 = Arc::clone(&self.unix_bridge_shutdown);
        let t1 = thread::Builder::new()
            .name("ags-unix-shim-bridge".into())
            .spawn(move || unix_to_tcp_accept_loop(shim_unix, shim_target, state1, bshut1))
            .map_err(|e| BerthError::Io(format!("spawn unix shim bridge: {e}")))?;

        let proxy_target = self.proxy_addr;
        let state2 = Arc::clone(&self.state);
        let bshut2 = Arc::clone(&self.unix_bridge_shutdown);
        let t2 = thread::Builder::new()
            .name("ags-unix-proxy-bridge".into())
            .spawn(move || unix_to_tcp_accept_loop(proxy_unix, proxy_target, state2, bshut2))
            .map_err(|e| BerthError::Io(format!("spawn unix proxy bridge: {e}")))?;

        self.unix_bridge_threads.push(t1);
        self.unix_bridge_threads.push(t2);
        self.unix_bridge_paths.push(shim_sock.clone());
        self.unix_bridge_paths.push(proxy_sock.clone());

        Ok((shim_sock, proxy_sock))
    }

    /// Stops and joins Unix Domain Socket bridge listeners BEFORE the temp directory
    /// holding the socket paths is deleted.
    #[cfg(unix)]
    pub fn stop_unix_bridges(&mut self) {
        self.unix_bridge_shutdown.store(true, Ordering::SeqCst);
        for sock_path in &self.unix_bridge_paths {
            let _ = std::os::unix::net::UnixStream::connect(sock_path);
        }
        for handle in self.unix_bridge_threads.drain(..) {
            let _ = handle.join();
        }
        for sock_path in self.unix_bridge_paths.drain(..) {
            let _ = std::fs::remove_file(sock_path);
        }
    }

    pub fn shutdown(&mut self) {
        #[cfg(unix)]
        self.stop_unix_bridges();
        if !self.state.shutdown.swap(true, Ordering::SeqCst) {
            let _ = TcpStream::connect_timeout(&self.listen_addr, Duration::from_millis(200));
            let _ = TcpStream::connect_timeout(&self.proxy_addr, Duration::from_millis(200));
        }
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.proxy_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CageShimHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ActiveConnGuard(Arc<ShimSharedState>);

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        self.0.active_conns.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, state: Arc<ShimSharedState>) {
    for incoming in listener.incoming() {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = incoming else {
            if state.shutdown.load(Ordering::SeqCst) {
                break;
            }
            continue;
        };
        if state.shutdown.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
        let active = state.active_conns.fetch_add(1, Ordering::SeqCst);
        if active >= MAX_CONCURRENT_CONNS {
            state.active_conns.fetch_sub(1, Ordering::SeqCst);
            let mut s = stream;
            let _ = write_simple_response(
                &mut s,
                503,
                "Service Unavailable",
                &[],
                b"{\"error\":\"shim_overloaded\"}",
            );
            continue;
        }
        let conn_state = Arc::clone(&state);
        let _ = thread::Builder::new()
            .name("ags-cage-shim-conn".into())
            .spawn(move || {
                let _guard = ActiveConnGuard(Arc::clone(&conn_state));
                let _ = handle_client_connection(stream, &conn_state);
            });
    }
}

#[derive(Debug)]
struct ParsedRequestHead {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    trailing: Vec<u8>,
}

fn read_http_head<R: Read>(reader: &mut R) -> io::Result<Option<(String, Vec<(String, String)>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let header_end;
    loop {
        if let Some(pos) = find_header_terminator(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP header section exceeds limit",
            ));
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before HTTP header terminator",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let head_bytes = &buf[..header_end];
    let trailing = buf[header_end + 4..].to_vec();
    let head_str = std::str::from_utf8(head_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 HTTP headers"))?;
    let mut lines = head_str.split("\r\n");
    let first_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP start line"))?
        .to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed HTTP header line",
            ));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(Some((first_line, headers, trailing)))
}

fn find_header_terminator(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_head(stream: &mut TcpStream) -> io::Result<Option<ParsedRequestHead>> {
    let Some((start_line, headers, trailing)) = read_http_head(stream)? else {
        return Ok(None);
    };
    let mut parts = start_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty()
        || target.is_empty()
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid HTTP/1.x request line",
        ));
    }
    Ok(Some(ParsedRequestHead {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        trailing,
    }))
}

fn handle_client_connection(mut client: TcpStream, state: &ShimSharedState) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let _ = client.set_read_timeout(Some(CLIENT_HEAD_TIMEOUT));
    let _ = client.set_write_timeout(Some(Duration::from_secs(30)));

    let req = match parse_request_head(&mut client) {
        Ok(Some(req)) => req,
        Ok(None) => return Ok(()),
        Err(_) => {
            return write_simple_response(
                &mut client,
                400,
                "Bad Request",
                &[],
                b"{\"error\":\"malformed_http_request\"}",
            );
        }
    };

    state.metrics.requests_total.fetch_add(1, Ordering::Relaxed);

    // Reject absolute-URI proxy-style requests or control characters.
    if !req.target.starts_with('/') || req.target.bytes().any(|b| b.is_ascii_control()) {
        state
            .metrics
            .forbidden_rejected
            .fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            403,
            "Forbidden",
            &[],
            b"{\"error\":\"invalid_request_target\"}",
        );
    }

    // Control #2: Strictly validate Host header == 127.0.0.1:<port> or localhost:<port>.
    if !validate_loopback_host(&req.headers, state.bound_port) {
        state
            .metrics
            .forbidden_rejected
            .fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            403,
            "Forbidden",
            &[],
            b"{\"error\":\"invalid_host_header\"}",
        );
    }

    // Control #3: Reject ANY request with an Origin header (browser CSRF / DNS rebinding).
    if req
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("origin"))
    {
        state
            .metrics
            .forbidden_rejected
            .fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            403,
            "Forbidden",
            &[],
            b"{\"error\":\"origin_header_rejected\"}",
        );
    }

    // Control #1: Per-launch bearer secret (constant-time comparison).
    if !verify_client_auth(&req.headers, &state.shim_token_digest) {
        state.metrics.auth_rejected.fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            401,
            "Unauthorized",
            &[("WWW-Authenticate", "Bearer realm=\"ags-cage-shim\"")],
            b"{\"error\":\"unauthorized\"}",
        );
    }

    // Control #4: Strict provider path allowlist.
    let path_only = req.target.split('?').next().unwrap_or("");
    if path_only == "/healthz" {
        let lease_id = state
            .secrets
            .read()
            .map(|s| s.lease_id.clone())
            .unwrap_or_default();
        let body = serde_json::json!({
            "ok": true,
            "lease_id": lease_id,
            "rebind_signals": state.metrics.rebind_signals.load(Ordering::Relaxed),
        })
        .to_string();
        return write_simple_response(&mut client, 200, "OK", &[], body.as_bytes());
    }
    if !is_allowed_provider_path(path_only) {
        state
            .metrics
            .not_found_rejected
            .fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            404,
            "Not Found",
            &[],
            b"{\"error\":\"path_not_allowed\"}",
        );
    }

    forward_to_upstream(client, req, state)
}

fn validate_loopback_host(headers: &[(String, String)], port: u16) -> bool {
    let mut found: Option<&str> = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            if found.is_some() {
                // Duplicate Host header is rejected.
                return false;
            }
            found = Some(value.as_str());
        }
    }
    let Some(host) = found else {
        return false;
    };
    let expected_ip = format!("127.0.0.1:{port}");
    let expected_localhost = format!("localhost:{port}");
    host == expected_ip || host.eq_ignore_ascii_case(&expected_localhost)
}

fn verify_client_auth(headers: &[(String, String)], expected_digest: &[u8; 32]) -> bool {
    for (name, value) in headers {
        let candidate = if name.eq_ignore_ascii_case("authorization") {
            let mut parts = value.splitn(2, char::is_whitespace);
            let scheme = parts.next().unwrap_or("");
            let token = parts.next().map(str::trim).unwrap_or("");
            if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
                Some(token)
            } else {
                None
            }
        } else if name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("x-goog-api-key")
        {
            let token = value.trim();
            if !token.is_empty() { Some(token) } else { None }
        } else {
            None
        };
        if let Some(tok) = candidate
            && constant_time_eq_digest(&sha256_bytes(tok.as_bytes()), expected_digest)
        {
            return true;
        }
    }
    false
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[inline(never)]
fn constant_time_eq_digest(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for idx in 0..32 {
        diff |= a[idx] ^ b[idx];
    }
    diff == 0
}

pub fn is_allowed_provider_path(path: &str) -> bool {
    if path.contains("..") || path.contains('\\') || path.contains("//") {
        return false;
    }
    path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
        || path == "/responses"
        || path.starts_with("/responses/")
        || path == "/messages"
        || path.starts_with("/messages/")
        || path == "/models"
        || path.starts_with("/models/")
        || path == "/chat/completions"
        || path.starts_with("/chat/completions/")
}

fn is_websocket_upgrade_request(headers: &[(String, String)]) -> bool {
    let has_upgrade_ws = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket")
    });
    let has_conn_upgrade = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("connection")
            && v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
    });
    has_upgrade_ws && has_conn_upgrade
}

fn forward_to_upstream(
    mut client: TcpStream,
    req: ParsedRequestHead,
    state: &ShimSharedState,
) -> io::Result<()> {
    if state.abort_retries_on_rebind && state.rebind_signaled.load(Ordering::Acquire) {
        return write_simple_response(
            &mut client,
            400,
            "Bad Request",
            &[("X-Berth-Action", "cage_rebind_required")],
            b"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"cage_rebind_in_progress\",\"message\":\"Berth Cage rebind is already in progress\"}}",
        );
    }
    let is_ws = is_websocket_upgrade_request(&req.headers);

    // Read full request body for non-WS requests before dialing or stream it right after headers.
    let mut content_length: Option<usize> = None;
    let mut req_chunked = false;
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("content-length") {
            content_length = v.parse::<usize>().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            req_chunked = true;
        }
    }

    let mut req_body = Vec::new();
    let mut client_ws_trailing = Vec::new();
    if is_ws {
        client_ws_trailing = req.trailing;
    } else if req_chunked {
        let chained = io::Cursor::new(req.trailing).chain(&mut client);
        let mut br = BufReader::new(chained);
        match read_chunked_body(&mut br, 16 * 1024 * 1024) {
            Ok(decoded) => req_body = decoded,
            Err(_) => {
                return write_simple_response(
                    &mut client,
                    400,
                    "Bad Request",
                    &[],
                    b"{\"error\":\"invalid_chunked_request_body\"}",
                );
            }
        }
    } else if let Some(len) = content_length
        && len > 0
    {
        req_body.reserve(len);
        let from_trailing = len.min(req.trailing.len());
        req_body.extend_from_slice(&req.trailing[..from_trailing]);
        let remaining = len - from_trailing;
        if remaining > 0 {
            let start = req_body.len();
            req_body.resize(len, 0);
            client.read_exact(&mut req_body[start..])?;
        }
    }

    let (lease_id, lease_token, work_session_id, cage_tz, cage_country, cage_strict) = {
        let guard = state.secrets.read().map_err(|_| {
            io::Error::other("shim secret lock poisoned")
        })?;
        (
            guard.lease_id.clone(),
            guard.lease_token.clone(),
            guard.work_session_id.clone(),
            guard.cage_tz.clone(),
            guard.cage_country.clone(),
            guard.cage_strict,
        )
    };

    let mut upstream = match ClientTransport::connect_direct(&state.ingress, state.upstream_timeout) {
        Ok(conn) => conn,
        Err(err) => {
            let body = serde_json::json!({
                "error": "upstream_connect_failed",
                "message": err.to_string(),
            })
            .to_string();
            return write_simple_response(&mut client, 502, "Bad Gateway", &[], body.as_bytes());
        }
    };

    let upstream_target = state.ingress.join_path(&req.target);
    let op_id = Uuid::new_v4().to_string();

    let mut wire_req = Vec::with_capacity(1024 + req_body.len());
    let _ = write!(
        &mut wire_req,
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        req.method,
        upstream_target,
        state.ingress.host_header()
    );

    for (name, value) in &req.headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "proxy-connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "content-length"
        ) || lower.starts_with("x-berth-")
        {
            continue;
        }
        if !is_ws && matches!(lower.as_str(), "connection" | "upgrade") {
            continue;
        }
        let _ = write!(&mut wire_req, "{name}: {value}\r\n");
    }

    if !is_ws {
        wire_req.extend_from_slice(b"Connection: close\r\n");
        if !req_body.is_empty() || content_length.is_some() || req_chunked {
            let _ = write!(&mut wire_req, "Content-Length: {}\r\n", req_body.len());
        }
    }

    // Inject authoritative Berth upstream headers (Control #6).
    let _ = write!(
        &mut wire_req,
        "Authorization: Bearer {lease_token}\r\n\
         X-Berth-Lease-ID: {lease_id}\r\n\
         X-Berth-Work-Session-ID: {work_session_id}\r\n\
         X-Berth-Cage-TZ: {cage_tz}\r\n\
         X-Berth-Cage-Country: {cage_country}\r\n\
         X-Berth-Cage-Strict: {}\r\n\
         X-Berth-AGS-Version: {AGS_VERSION_HEADER_VALUE}\r\n\
         X-Berth-Operation-ID: {op_id}\r\n\r\n",
        if cage_strict { "true" } else { "false" }
    );
    if !is_ws && !req_body.is_empty() {
        wire_req.extend_from_slice(&req_body);
    }

    if let Err(err) = upstream.write_all(&wire_req).and_then(|()| upstream.flush()) {
        let body = serde_json::json!({
            "error": "upstream_write_failed",
            "message": err.to_string(),
        })
        .to_string();
        return write_simple_response(&mut client, 502, "Bad Gateway", &[], body.as_bytes());
    }

    let (status_line, resp_headers, resp_trailing) = match read_http_head(&mut upstream) {
        Ok(Some(parts)) => parts,
        Ok(None) | Err(_) => {
            return write_simple_response(
                &mut client,
                502,
                "Bad Gateway",
                &[],
                b"{\"error\":\"upstream_invalid_response\"}",
            );
        }
    };

    let status_code = parse_http_status_code(&status_line).unwrap_or(502);
    state
        .metrics
        .requests_forwarded
        .fetch_add(1, Ordering::Relaxed);

    // Branch 1: WebSocket 101 Switching Protocols bidirectional tunnel.
    if status_code == 101 {
        state.metrics.ws_upgrades.fetch_add(1, Ordering::Relaxed);
        let mut wire_resp = Vec::with_capacity(512);
        let _ = write!(&mut wire_resp, "{status_line}\r\n");
        for (k, v) in &resp_headers {
            let _ = write!(&mut wire_resp, "{k}: {v}\r\n");
        }
        wire_resp.extend_from_slice(b"\r\n");
        client.write_all(&wire_resp)?;
        if !resp_trailing.is_empty() {
            client.write_all(&resp_trailing)?;
        }
        client.flush()?;
        if !client_ws_trailing.is_empty() {
            upstream.write_all(&client_ws_trailing)?;
            upstream.flush()?;
        }
        return run_websocket_tunnel(client, upstream, &state.shutdown);
    }

    // Branch 2: Pre-commit 409 cage_rebind_required inspection (Control #8).
    if status_code == 409 {
        let resp_body =
            read_bounded_response_body(&mut upstream, &resp_headers, resp_trailing, MAX_REBIND_BODY_BYTES)?;
        if let Some(signal) = extract_rebind_signal(&resp_headers, &resp_body, &cage_tz) {
            state.metrics.rebind_signals.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = state.metrics.last_rebind.lock() {
                *guard = Some(signal.clone());
            }
            if let Some(rebind_path) = &state.rebind_file {
                if let Ok(mut encoded) = serde_json::to_vec_pretty(&signal) {
                    encoded.push(b'\n');
                    let _ = write_private_file_atomic(rebind_path, &encoded);
                }
            }
            state.rebind_signaled.store(true, Ordering::Release);
        }
        let mut wire_resp = Vec::with_capacity(512 + resp_body.len());
        let _ = write!(&mut wire_resp, "{status_line}\r\n");
        for (k, v) in &resp_headers {
            let lower = k.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "connection" | "keep-alive" | "transfer-encoding" | "content-length"
            ) {
                continue;
            }
            let _ = write!(&mut wire_resp, "{k}: {v}\r\n");
        }
        let _ = write!(
            &mut wire_resp,
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            resp_body.len()
        );
        wire_resp.extend_from_slice(&resp_body);
        client.write_all(&wire_resp)?;
        client.flush()?;
        return Ok(());
    }

    // Branch 3: Zero-buffered streaming HTTP/1.1 & SSE forwarding.
    let _ = upstream.set_read_timeout(None);
    let _ = client.set_write_timeout(None);

    let mut resp_content_len: Option<usize> = None;
    let mut resp_chunked = false;
    let mut wire_head = Vec::with_capacity(512);
    let _ = write!(&mut wire_head, "{status_line}\r\n");
    for (k, v) in &resp_headers {
        let lower = k.to_ascii_lowercase();
        if lower == "content-length" {
            resp_content_len = v.parse::<usize>().ok();
        } else if lower == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            resp_chunked = true;
        }
        if matches!(lower.as_str(), "connection" | "keep-alive") {
            continue;
        }
        let _ = write!(&mut wire_head, "{k}: {v}\r\n");
    }
    wire_head.extend_from_slice(b"Connection: close\r\n\r\n");
    client.write_all(&wire_head)?;
    client.flush()?;

    if req.method == "HEAD" || status_code == 204 || status_code == 304 {
        return Ok(());
    }

    if resp_chunked {
        stream_chunked_passthrough(&mut client, &mut upstream, &resp_trailing)?;
    } else if let Some(total) = resp_content_len {
        let mut remaining = total;
        if !resp_trailing.is_empty() {
            let take = remaining.min(resp_trailing.len());
            client.write_all(&resp_trailing[..take])?;
            client.flush()?;
            remaining -= take;
        }
        let mut buf = [0u8; 4096];
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let n = upstream.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            client.write_all(&buf[..n])?;
            client.flush()?;
            remaining -= n;
        }
    } else {
        if !resp_trailing.is_empty() {
            client.write_all(&resp_trailing)?;
            client.flush()?;
        }
        let mut buf = [0u8; 4096];
        loop {
            let n = upstream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            client.write_all(&buf[..n])?;
            client.flush()?;
        }
    }
    Ok(())
}

/// Streams HTTP/1.1 chunked responses immediately chunk-by-chunk with zero
/// buffering and stops cleanly on the terminal `0\r\n...\r\n` chunk even if
/// the upstream keeps the TCP socket open.
fn stream_chunked_passthrough<W: Write, R: Read>(
    client: &mut W,
    upstream: &mut R,
    initial: &[u8],
) -> io::Result<()> {
    let chained = io::Cursor::new(initial).chain(upstream);
    let mut reader = BufReader::with_capacity(4096, chained);
    let mut buf = [0u8; 4096];

    loop {
        let mut size_line = String::new();
        let n = reader.read_line(&mut size_line)?;
        if n == 0 {
            break;
        }
        client.write_all(size_line.as_bytes())?;
        let hex_part = size_line
            .trim()
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let chunk_size = usize::from_str_radix(hex_part, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
        if chunk_size == 0 {
            loop {
                let mut trailer = String::new();
                let tn = reader.read_line(&mut trailer)?;
                if tn == 0 {
                    break;
                }
                client.write_all(trailer.as_bytes())?;
                if trailer.trim().is_empty() {
                    break;
                }
            }
            client.flush()?;
            break;
        }
        let mut left = chunk_size + 2; // chunk bytes + trailing \r\n
        while left > 0 {
            let want = left.min(buf.len());
            let read_n = reader.read(&mut buf[..want])?;
            if read_n == 0 {
                client.flush()?;
                return Ok(());
            }
            client.write_all(&buf[..read_n])?;
            client.flush()?;
            left -= read_n;
        }
    }
    Ok(())
}

fn read_bounded_response_body(
    upstream: &mut ClientTransport,
    headers: &[(String, String)],
    trailing: Vec<u8>,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());

    if is_chunked {
        let chained = io::Cursor::new(trailing).chain(upstream);
        let mut br = BufReader::new(chained);
        read_chunked_body(&mut br, max_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    } else if let Some(len) = content_length {
        let capped = len.min(max_bytes);
        let mut body = Vec::with_capacity(capped);
        let from_trailing = capped.min(trailing.len());
        body.extend_from_slice(&trailing[..from_trailing]);
        let rem = capped - from_trailing;
        if rem > 0 {
            let start = body.len();
            body.resize(capped, 0);
            upstream.read_exact(&mut body[start..])?;
        }
        Ok(body)
    } else {
        let mut body = trailing;
        if body.len() < max_bytes {
            let take = (max_bytes - body.len()) as u64;
            upstream.take(take).read_to_end(&mut body)?;
        } else {
            body.truncate(max_bytes);
        }
        Ok(body)
    }
}

/// Extracts ONLY an opaque `RebindSignal` from a pre-commit HTTP `409` body.
/// Explicitly ignores any secret/token fields even if present in the payload.
pub fn extract_rebind_signal(
    headers: &[(String, String)],
    body: &[u8],
    fallback_current_tz: &str,
) -> Option<RebindSignal> {
    let text = std::str::from_utf8(body).ok()?;
    let header_marked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("x-berth-action")
            && v.eq_ignore_ascii_case("cage_rebind_required")
    });
    if !header_marked && !text.contains("cage_rebind_required") {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(text).ok()?;

    // Verify error code matches when structured.
    let err_obj = json.get("error").and_then(|v| v.as_object());
    let code = json
        .get("code")
        .and_then(|v| v.as_str())
        .or_else(|| err_obj.and_then(|e| e.get("code")).and_then(|v| v.as_str()))
        .or_else(|| err_obj.and_then(|e| e.get("type")).and_then(|v| v.as_str()));
    if !header_marked
        && code
            .map(|c| !c.contains("cage_rebind_required"))
            .unwrap_or(false)
    {
        return None;
    }

    let data_obj = json.get("data").and_then(|v| v.as_object());
    let offer = err_obj
        .and_then(|e| e.get("offer").or_else(|| e.get("recommended_cohort")))
        .or_else(|| data_obj.and_then(|d| d.get("recommended_cohort")))
        .or_else(|| json.get("offer").or_else(|| json.get("recommended_cohort")))
        .and_then(|v| v.as_object());
    let offer_cage = offer
        .and_then(|o| o.get("cage"))
        .and_then(|v| v.as_object());

    let pick_str = |keys: &[&str]| -> Option<String> {
        for key in keys {
            if let Some(s) = json.get(*key).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
            if let Some(s) = err_obj.and_then(|e| e.get(*key)).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
            if let Some(s) = data_obj.and_then(|d| d.get(*key)).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
            if let Some(s) = offer.and_then(|o| o.get(*key)).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
            if let Some(s) = offer_cage.and_then(|c| c.get(*key)).and_then(|v| v.as_str())
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
        None
    };

    let rebind_ref = pick_str(&["rebind_ref", "cohort_key"])
        .unwrap_or_else(|| format!("rebind-{}", Uuid::new_v4().simple()));
    let current_cohort_tz =
        pick_str(&["current_cohort_tz"]).unwrap_or_else(|| fallback_current_tz.to_string());
    let recommended_cohort_tz =
        pick_str(&["recommended_cohort_tz", "cohort_timezone", "timezone"]).unwrap_or_default();
    let recommended_country_code =
        pick_str(&["recommended_country_code", "cohort_country_code", "country_code"])
            .unwrap_or_default();
    let recommended_locale =
        pick_str(&["recommended_locale", "cohort_locale", "locale"]).unwrap_or_default();
    let recommended_account_id = offer
        .and_then(|o| o.get("account_id"))
        .or_else(|| err_obj.and_then(|e| e.get("account_id")))
        .or_else(|| json.get("account_id"))
        .and_then(|v| v.as_i64())
        .filter(|id| *id > 0);

    let available_cohorts = json
        .get("available_cohorts")
        .or_else(|| err_obj.and_then(|e| e.get("available_cohorts")))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Some(RebindSignal {
        rebind_ref,
        current_cohort_tz,
        recommended_cohort_tz,
        recommended_country_code,
        recommended_locale,
        recommended_account_id,
        available_cohorts,
    })
}

fn run_websocket_tunnel(
    client: TcpStream,
    upstream: ClientTransport,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    match upstream {
        ClientTransport::Plain(upstream_tcp) => {
            let _ = client.set_read_timeout(None);
            let _ = client.set_write_timeout(None);
            let _ = upstream_tcp.set_read_timeout(None);
            let _ = upstream_tcp.set_write_timeout(None);

            let mut c_read = client.try_clone()?;
            let mut u_write = upstream_tcp.try_clone()?;
            let mut u_read = upstream_tcp;
            let mut c_write = client;

            let c2u = thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = c_read.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if u_write.write_all(&buf[..n]).is_err() || u_write.flush().is_err() {
                        break;
                    }
                }
                let _ = u_write.shutdown(Shutdown::Write);
            });

            let mut buf = [0u8; 8192];
            while let Ok(n) = u_read.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if c_write.write_all(&buf[..n]).is_err() || c_write.flush().is_err() {
                    break;
                }
            }
            let _ = c_write.shutdown(Shutdown::Write);
            let _ = c2u.join();
            Ok(())
        }
        ClientTransport::Tls(stream_owned) => {
            let (mut tls_conn, mut upstream_tcp) = stream_owned.into_parts();
            let mut client_tcp = client;
            client_tcp.set_nonblocking(true)?;
            upstream_tcp.set_nonblocking(true)?;

            let mut c_buf = [0u8; 8192];
            let mut u_buf = [0u8; 8192];
            let mut pending_to_client: Vec<u8> = Vec::new();
            let mut pending_to_client_off = 0usize;
            let mut idle_polls = 0u32;

            while !shutdown.load(Ordering::Relaxed) {
                let mut progressed = false;

                // 1. Drain pending decrypted bytes to client.
                if pending_to_client_off < pending_to_client.len() {
                    match client_tcp.write(&pending_to_client[pending_to_client_off..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            pending_to_client_off += n;
                            if pending_to_client_off == pending_to_client.len() {
                                pending_to_client.clear();
                                pending_to_client_off = 0;
                            }
                            progressed = true;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                }

                // 2. Read plaintext from client -> encrypt into rustls.
                match client_tcp.read(&mut c_buf) {
                    Ok(0) => {
                        tls_conn.send_close_notify();
                        let _ = flush_tls_writes(&mut tls_conn, &mut upstream_tcp);
                        break;
                    }
                    Ok(n) => {
                        if tls_conn.writer().write_all(&c_buf[..n]).is_err() {
                            break;
                        }
                        progressed = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }

                // 3. Flush encrypted TLS records to upstream.
                match flush_tls_writes(&mut tls_conn, &mut upstream_tcp) {
                    Ok(wrote) if wrote > 0 => progressed = true,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }

                // 4. Read TLS ciphertext from upstream -> decrypt -> buffer for client.
                match tls_conn.read_tls(&mut upstream_tcp) {
                    Ok(0) => break,
                    Ok(_) => {
                        progressed = true;
                        if tls_conn.process_new_packets().is_err() {
                            break;
                        }
                        loop {
                            match tls_conn.reader().read(&mut u_buf) {
                                Ok(0) => break,
                                Ok(n) => pending_to_client.extend_from_slice(&u_buf[..n]),
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }

                if progressed {
                    idle_polls = 0;
                } else {
                    idle_polls = idle_polls.saturating_add(1);
                    let delay_ms = if idle_polls < 10 { 1 } else { 5 };
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }

            let _ = client_tcp.shutdown(Shutdown::Both);
            let _ = upstream_tcp.shutdown(Shutdown::Both);
            Ok(())
        }
    }
}

fn flush_tls_writes(
    tls_conn: &mut rustls::ClientConnection,
    upstream_tcp: &mut TcpStream,
) -> io::Result<usize> {
    let mut total = 0usize;
    while tls_conn.wants_write() {
        let n = tls_conn.write_tls(upstream_tcp)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn write_simple_response(
    client: &mut TcpStream,
    status: u16,
    reason: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut out = Vec::with_capacity(256 + body.len());
    let _ = write!(
        &mut out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    client.write_all(&out)?;
    client.flush()
}

// ---------------------------------------------------------------------------
// Unified Forward Proxy (HTTP CONNECT + Plain HTTP + SOCKS5h -> Berth Landing)
// ---------------------------------------------------------------------------

fn proxy_accept_loop(listener: TcpListener, state: Arc<ShimSharedState>) {
    for incoming in listener.incoming() {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = incoming else {
            if state.shutdown.load(Ordering::SeqCst) {
                break;
            }
            continue;
        };
        if state.shutdown.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
        let active = state.active_conns.fetch_add(1, Ordering::SeqCst);
        if active >= MAX_CONCURRENT_CONNS {
            state.active_conns.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let conn_state = Arc::clone(&state);
        let _ = thread::Builder::new()
            .name("ags-cage-proxy-conn".into())
            .spawn(move || {
                let _guard = ActiveConnGuard(Arc::clone(&conn_state));
                let _ = handle_forward_proxy_conn(stream, &conn_state);
            });
    }
}

#[cfg(unix)]
fn unix_to_tcp_accept_loop(
    listener: std::os::unix::net::UnixListener,
    target: SocketAddr,
    state: Arc<ShimSharedState>,
    bridge_shutdown: Arc<AtomicBool>,
) {
    for incoming in listener.incoming() {
        if bridge_shutdown.load(Ordering::SeqCst) || state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(unix_stream) = incoming else {
            if bridge_shutdown.load(Ordering::SeqCst) || state.shutdown.load(Ordering::SeqCst) {
                break;
            }
            continue;
        };
        if bridge_shutdown.load(Ordering::SeqCst) || state.shutdown.load(Ordering::SeqCst) {
            let _ = unix_stream.shutdown(Shutdown::Both);
            break;
        }
        let _ = thread::Builder::new()
            .name("ags-unix-bridge-conn".into())
            .spawn(move || {
                let Ok(tcp) = TcpStream::connect_timeout(&target, Duration::from_secs(5)) else {
                    let _ = unix_stream.shutdown(Shutdown::Both);
                    return;
                };
                let _ = tcp.set_nodelay(true);
                crate::sandbox::pipe_tcp_and_unix(tcp, unix_stream);
            });
    }
}

fn handle_forward_proxy_conn(client: TcpStream, state: &ShimSharedState) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let _ = client.set_read_timeout(Some(CLIENT_HEAD_TIMEOUT));
    let _ = client.set_write_timeout(Some(Duration::from_secs(30)));

    let mut first = [0u8; 1];
    match client.peek(&mut first) {
        Ok(0) => return Ok(()),
        Ok(_) => {}
        Err(_) => return Ok(()),
    }

    if first[0] == 0x05 {
        return handle_socks5_proxy(client, state);
    }
    handle_http_forward_proxy(client, state)
}

fn open_berth_connect_tunnel(
    target_host_port: &str,
    state: &ShimSharedState,
) -> Result<(ClientTransport, Vec<u8>), (u16, String)> {
    let (lease_id, lease_token, work_session_id) = {
        let guard = state
            .secrets
            .read()
            .map_err(|_| (500u16, "shim secret lock poisoned".to_string()))?;
        (
            guard.lease_id.clone(),
            guard.lease_token.clone(),
            guard.work_session_id.clone(),
        )
    };

    let mut upstream = ClientTransport::connect_direct(&state.ingress, state.upstream_timeout)
        .map_err(|e| (502u16, format!("berth ingress connect failed: {e}")))?;

    let req = format!(
        "CONNECT {target_host_port} HTTP/1.1\r\n\
         Host: {target_host_port}\r\n\
         X-Berth-Lease: {lease_token}\r\n\
         X-Berth-Connect-Target: {target_host_port}\r\n\
         X-Berth-Lease-ID: {lease_id}\r\n\
         X-Berth-Work-Session-ID: {work_session_id}\r\n\
         X-Berth-AGS-Version: {AGS_VERSION_HEADER_VALUE}\r\n\r\n"
    );
    upstream
        .write_all(req.as_bytes())
        .and_then(|()| upstream.flush())
        .map_err(|e| (502u16, format!("berth CONNECT write failed: {e}")))?;

    let (status_line, _headers, trailing) = read_http_head(&mut upstream)
        .map_err(|e| (502u16, format!("berth CONNECT response failed: {e}")))?
        .ok_or_else(|| (502u16, "berth CONNECT unexpected EOF".to_string()))?;

    let status = parse_http_status_code(&status_line).unwrap_or(502);
    if status != 200 {
        return Err((status, format!("berth CONNECT rejected with HTTP {status}")));
    }
    Ok((upstream, trailing))
}

fn handle_socks5_proxy(mut client: TcpStream, state: &ShimSharedState) -> io::Result<()> {
    // 1. Greeting: VER(1) NMETHODS(1) METHODS(n)
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr)?;
    if hdr[0] != 0x05 || hdr[1] == 0 {
        return Ok(());
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    client.read_exact(&mut methods)?;
    // Require RFC 1929 Username/Password authentication (0x02).
    if !methods.contains(&0x02) {
        let _ = client.write_all(&[0x05, 0xFF]);
        let _ = client.flush();
        state.metrics.auth_rejected.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    client.write_all(&[0x05, 0x02])?;
    client.flush()?;

    // 2. RFC 1929 Subnegotiation: VER(1)=0x01 ULEN(1) UNAME PLEN(1) PASSWD
    let mut auth_hdr = [0u8; 2];
    client.read_exact(&mut auth_hdr)?;
    if auth_hdr[0] != 0x01 {
        let _ = client.write_all(&[0x01, 0x01]);
        return Ok(());
    }
    let mut uname = vec![0u8; auth_hdr[1] as usize];
    client.read_exact(&mut uname)?;
    let mut plen = [0u8; 1];
    client.read_exact(&mut plen)?;
    let mut pass = vec![0u8; plen[0] as usize];
    client.read_exact(&mut pass)?;

    if !constant_time_eq_digest(&sha256_bytes(&pass), &state.shim_token_digest) {
        let _ = client.write_all(&[0x01, 0x01]);
        let _ = client.flush();
        state.metrics.auth_rejected.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    client.write_all(&[0x01, 0x00])?;
    client.flush()?;

    // 3. Request: VER(1) CMD(1) RSV(1) ATYP(1) DST.ADDR DST.PORT(2)
    let mut req_hdr = [0u8; 4];
    client.read_exact(&mut req_hdr)?;
    if req_hdr[0] != 0x05 || req_hdr[1] != 0x01 {
        // Only TCP CONNECT (0x01) is supported (rejects BIND & UDP ASSOCIATE!).
        let _ = client.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        let _ = client.flush();
        return Ok(());
    }

    let host = match req_hdr[3] {
        0x01 => {
            let mut ip4 = [0u8; 4];
            client.read_exact(&mut ip4)?;
            Ipv4Addr::from(ip4).to_string()
        }
        0x03 => {
            let mut dlen = [0u8; 1];
            client.read_exact(&mut dlen)?;
            let mut dom = vec![0u8; dlen[0] as usize];
            client.read_exact(&mut dom)?;
            match String::from_utf8(dom) {
                Ok(s) if !s.is_empty() && !s.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace()) => s,
                _ => {
                    let _ = client.write_all(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                    return Ok(());
                }
            }
        }
        // Reject IPv6 (0x04) and unknown address types (IPv4-only / remote-domain policy).
        _ => {
            let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            let _ = client.flush();
            return Ok(());
        }
    };
    let mut port_bytes = [0u8; 2];
    client.read_exact(&mut port_bytes)?;
    let port = u16::from_be_bytes(port_bytes);
    if port == 0 {
        let _ = client.write_all(&[0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        return Ok(());
    }
    let target = format!("{host}:{port}");

    match open_berth_connect_tunnel(&target, state) {
        Ok((upstream, trailing)) => {
            client.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])?;
            if !trailing.is_empty() {
                client.write_all(&trailing)?;
            }
            client.flush()?;
            state
                .metrics
                .requests_forwarded
                .fetch_add(1, Ordering::Relaxed);
            run_websocket_tunnel(client, upstream, &state.shutdown)
        }
        Err((status, _)) => {
            let rep = if status == 403 { 0x02 } else { 0x05 };
            let _ = client.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            let _ = client.flush();
            Ok(())
        }
    }
}

fn handle_http_forward_proxy(mut client: TcpStream, state: &ShimSharedState) -> io::Result<()> {
    let req = match parse_request_head(&mut client) {
        Ok(Some(req)) => req,
        Ok(None) => return Ok(()),
        Err(_) => {
            return write_simple_response(
                &mut client,
                400,
                "Bad Request",
                &[],
                b"{\"error\":\"malformed_proxy_request\"}",
            );
        }
    };

    if req
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("origin"))
    {
        state
            .metrics
            .forbidden_rejected
            .fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            403,
            "Forbidden",
            &[],
            b"{\"error\":\"origin_header_rejected\"}",
        );
    }

    if !verify_proxy_auth(&req.headers, &state.shim_token_digest) {
        state.metrics.auth_rejected.fetch_add(1, Ordering::Relaxed);
        return write_simple_response(
            &mut client,
            407,
            "Proxy Authentication Required",
            &[("Proxy-Authenticate", "Basic realm=\"berth-cage-proxy\"")],
            b"{\"error\":\"proxy_auth_required\"}",
        );
    }

    if req.method.eq_ignore_ascii_case("CONNECT") {
        let target = req.target.trim();
        if target.is_empty()
            || !target.contains(':')
            || target.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            return write_simple_response(
                &mut client,
                400,
                "Bad Request",
                &[],
                b"{\"error\":\"invalid_connect_target\"}",
            );
        }
        match open_berth_connect_tunnel(target, state) {
            Ok((mut upstream, upstream_trailing)) => {
                client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                if !upstream_trailing.is_empty() {
                    client.write_all(&upstream_trailing)?;
                }
                client.flush()?;
                if !req.trailing.is_empty() {
                    upstream.write_all(&req.trailing)?;
                    upstream.flush()?;
                }
                state
                    .metrics
                    .requests_forwarded
                    .fetch_add(1, Ordering::Relaxed);
                return run_websocket_tunnel(client, upstream, &state.shutdown);
            }
            Err((status, msg)) => {
                let reason = if status == 403 {
                    "Forbidden"
                } else {
                    "Bad Gateway"
                };
                let body = serde_json::json!({ "error": msg }).to_string();
                return write_simple_response(&mut client, status, reason, &[], body.as_bytes());
            }
        }
    }

    // Plain HTTP proxy request (e.g., `GET http://example.com/path HTTP/1.1`).
    let parsed = match ParsedUrl::parse(&req.target) {
        Ok(p) if !p.is_https => p,
        _ => {
            return write_simple_response(
                &mut client,
                400,
                "Bad Request",
                &[],
                b"{\"error\":\"plain_proxy_requires_http_url\"}",
            );
        }
    };
    let connect_target = format!("{}:{}", parsed.host, parsed.port);
    let (mut upstream, _) = match open_berth_connect_tunnel(&connect_target, state) {
        Ok(pair) => pair,
        Err((status, msg)) => {
            let reason = if status == 403 { "Forbidden" } else { "Bad Gateway" };
            let body = serde_json::json!({ "error": msg }).to_string();
            return write_simple_response(&mut client, status, reason, &[], body.as_bytes());
        }
    };

    let rel_path = if parsed.path_and_query.is_empty() {
        "/".to_string()
    } else {
        parsed.path_and_query.clone()
    };
    let mut wire = Vec::with_capacity(512 + req.trailing.len());
    let _ = write!(
        &mut wire,
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        req.method,
        rel_path,
        parsed.host_header()
    );
    for (k, v) in &req.headers {
        let lk = k.to_ascii_lowercase();
        if matches!(lk.as_str(), "host" | "proxy-authorization" | "proxy-connection") {
            continue;
        }
        let _ = write!(&mut wire, "{k}: {v}\r\n");
    }
    wire.extend_from_slice(b"Connection: close\r\n\r\n");
    wire.extend_from_slice(&req.trailing);
    upstream.write_all(&wire)?;
    upstream.flush()?;
    state
        .metrics
        .requests_forwarded
        .fetch_add(1, Ordering::Relaxed);
    run_websocket_tunnel(client, upstream, &state.shutdown)
}

fn verify_proxy_auth(headers: &[(String, String)], expected_digest: &[u8; 32]) -> bool {
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("proxy-authorization") {
            continue;
        }
        let mut parts = value.splitn(2, char::is_whitespace);
        let scheme = parts.next().unwrap_or("");
        let creds = parts.next().map(str::trim).unwrap_or("");
        if scheme.eq_ignore_ascii_case("bearer") && !creds.is_empty() {
            if constant_time_eq_digest(&sha256_bytes(creds.as_bytes()), expected_digest) {
                return true;
            }
        } else if scheme.eq_ignore_ascii_case("basic") && !creds.is_empty() {
            if let Some(decoded) = decode_base64_std(creds)
                && let Ok(text) = String::from_utf8(decoded)
                && let Some((_user, pass)) = text.split_once(':')
                && constant_time_eq_digest(&sha256_bytes(pass.as_bytes()), expected_digest)
            {
                return true;
            }
        }
    }
    false
}

fn decode_base64_std(input: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for b in bytes {
        if b == b'=' {
            break;
        }
        let v = val(b)?;
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use tempfile::TempDir;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::HeaderValue;
    use tungstenite::Message;

    fn send_raw_http(addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        resp
    }

    fn base_config(ingress_url: String) -> CageShimConfig {
        CageShimConfig {
            bind_addr: "127.0.0.1:0".into(),
            ingress_url,
            lease_id: "lease_abc".into(),
            lease_token: "bst_real_ed25519_secret_token_do_not_leak".into(),
            work_session_id: "ws_999".into(),
            cage_tz: "Asia/Tokyo".into(),
            cage_country: "JP".into(),
            cage_strict: true,
            rebind_file: None,
            ready_file: None,
            upstream_timeout: Duration::from_secs(5),
            abort_retries_on_rebind: false,
        }
    }

    #[test]
    fn refuses_non_loopback_bind_address() {
        let mut cfg = base_config("http://127.0.0.1:9999".into());
        cfg.bind_addr = "0.0.0.0:0".into();
        let err = CageShim::start(cfg).err().expect("must reject 0.0.0.0");
        assert!(err.to_string().contains("non-loopback"));
    }

    #[test]
    fn security_controls_auth_host_origin_and_path_allowlist() {
        let dir = TempDir::new().unwrap();
        let ready_file = dir.path().join("ready.json");
        let mut cfg = base_config("http://127.0.0.1:9".into());
        cfg.ready_file = Some(ready_file.clone());
        let shim = CageShim::start(cfg).unwrap();
        let addr = shim.listen_addr();
        let port = addr.port();
        let token = shim.shim_token().to_string();

        assert!(token.starts_with("ags-shim-"));
        assert_eq!(token.len(), "ags-shim-".len() + 64);

        // Ready file exists with 0600 perms and contains NO lease_token!
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&ready_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let ready_raw = fs::read_to_string(&ready_file).unwrap();
        assert!(!ready_raw.contains("bst_real_ed25519_secret_token_do_not_leak"));
        let ready: ReadyFilePayload = serde_json::from_str(&ready_raw).unwrap();
        assert_eq!(ready.listen_url, shim.listen_url());
        assert_eq!(ready.shim_token, token);

        // 1. Missing token -> 401
        let resp = send_raw_http(
            addr,
            &format!("GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 401 "), "got: {resp}");

        // 2. Wrong token -> 401
        let resp = send_raw_http(
            addr,
            &format!(
                "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer wrong\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 401 "), "got: {resp}");

        // 3. Non-loopback Host -> 403
        let resp = send_raw_http(
            addr,
            &format!(
                "GET /v1/models HTTP/1.1\r\nHost: evil.attacker.com:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 403 "), "got: {resp}");

        // 4. Browser Origin header -> 403
        let resp = send_raw_http(
            addr,
            &format!(
                "GET /v1/models HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: https://evil.example\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 403 "), "got: {resp}");

        // 5. Disallowed path -> 404
        let resp = send_raw_http(
            addr,
            &format!(
                "GET /admin/secrets HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 404 "), "got: {resp}");

        // 6. Traversal path -> 404
        let resp = send_raw_http(
            addr,
            &format!(
                "GET /v1/../admin HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 404 "), "got: {resp}");

        // 7. Authenticated /healthz -> 200 (also tests x-api-key and x-goog-api-key!)
        let resp = send_raw_http(
            addr,
            &format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-api-key: {token}\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 200 "), "got: {resp}");
        assert!(!resp.contains("bst_real_ed25519_secret_token_do_not_leak"));

        let resp = send_raw_http(
            addr,
            &format!(
                "GET /healthz HTTP/1.1\r\nHost: localhost:{port}\r\nx-goog-api-key: {token}\r\n\r\n"
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 200 "), "got: {resp}");

        let metrics = shim.metrics();
        assert_eq!(metrics.auth_rejected, 2);
        assert_eq!(metrics.forbidden_rejected, 2);
        assert_eq!(metrics.not_found_rejected, 2);
    }

    #[test]
    fn forwarding_injects_lease_headers_and_fresh_operation_id() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_clone = Arc::clone(&captured);

        let up_thread = thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = upstream.accept().unwrap();
                let (start, headers, trailing) = read_http_head(&mut s).unwrap().unwrap();
                let mut hdr_dump = format!("{start}\n");
                let mut cl = 0usize;
                for (k, v) in &headers {
                    hdr_dump.push_str(&format!("{}: {}\n", k.to_ascii_lowercase(), v));
                    if k.eq_ignore_ascii_case("content-length") {
                        cl = v.parse().unwrap_or(0);
                    }
                }
                let mut body = trailing;
                if body.len() < cl {
                    let start_len = body.len();
                    body.resize(cl, 0);
                    s.read_exact(&mut body[start_len..]).unwrap();
                }
                captured_clone.lock().unwrap().push(hdr_dump);
                let resp_body = format!("{{\"echo\":{}}}", String::from_utf8_lossy(&body));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                s.write_all(resp.as_bytes()).unwrap();
            }
        });

        let shim = CageShim::start(base_config(format!("http://{upstream_addr}"))).unwrap();
        let addr = shim.listen_addr();
        let port = addr.port();
        let token = shim.shim_token().to_string();

        for i in 0..2 {
            let body = format!("{{\"turn\":{i}}}");
            let raw = format!(
                "POST /v1/responses HTTP/1.1\r\n\
                 Host: 127.0.0.1:{port}\r\n\
                 Authorization: Bearer {token}\r\n\
                 X-Berth-Lease-ID: spoofed\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let resp = send_raw_http(addr, &raw);
            assert!(resp.starts_with("HTTP/1.1 200 "), "got: {resp}");
            assert!(resp.contains(&format!("\"turn\":{i}")));
        }

        up_thread.join().unwrap();
        let dumps = captured.lock().unwrap();
        assert_eq!(dumps.len(), 2);
        let mut op_ids = Vec::new();
        for dump in dumps.iter() {
            assert!(dump.contains("authorization: Bearer bst_real_ed25519_secret_token_do_not_leak"));
            assert!(!dump.contains(&token));
            assert!(dump.contains("x-berth-lease-id: lease_abc"));
            assert!(!dump.contains("spoofed"));
            assert!(dump.contains("x-berth-work-session-id: ws_999"));
            assert!(dump.contains("x-berth-cage-tz: Asia/Tokyo"));
            assert!(dump.contains("x-berth-cage-country: JP"));
            assert!(dump.contains("x-berth-cage-strict: true"));
            assert!(dump.contains("x-berth-ags-version: 2"));
            let op_id = dump
                .lines()
                .find_map(|l| l.strip_prefix("x-berth-operation-id: "))
                .expect("missing X-Berth-Operation-ID");
            assert!(Uuid::parse_str(op_id).is_ok());
            op_ids.push(op_id.to_string());
        }
        assert_ne!(op_ids[0], op_ids[1], "operation IDs must be unique per request");
    }

    #[test]
    fn sse_streams_chunk_by_chunk_without_buffering() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let (tx_seen_chunk1, rx_seen_chunk1) = mpsc::channel::<()>();

        let up_thread = thread::spawn(move || {
            let (mut s, _) = upstream.accept().unwrap();
            let _ = read_http_head(&mut s).unwrap();
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            let chunk1 = b"data: chunk-one\n\n";
            write!(s, "{:x}\r\n", chunk1.len()).unwrap();
            s.write_all(chunk1).unwrap();
            s.write_all(b"\r\n").unwrap();
            s.flush().unwrap();

            // Wait until the client has ALREADY received chunk 1 before sending chunk 2!
            // If the shim buffered the SSE response, this wait would time out.
            rx_seen_chunk1
                .recv_timeout(Duration::from_secs(3))
                .expect("client did not receive SSE chunk 1 before chunk 2 was sent");

            let chunk2 = b"data: chunk-two\n\n";
            write!(s, "{:x}\r\n", chunk2.len()).unwrap();
            s.write_all(chunk2).unwrap();
            s.write_all(b"\r\n0\r\n\r\n").unwrap();
            s.flush().unwrap();
        });

        let shim = CageShim::start(base_config(format!("http://{upstream_addr}"))).unwrap();
        let mut client = TcpStream::connect(shim.listen_addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let raw = format!(
            "GET /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nAccept: text/event-stream\r\n\r\n",
            shim.listen_addr().port(),
            shim.shim_token()
        );
        client.write_all(raw.as_bytes()).unwrap();
        client.flush().unwrap();

        let mut received = String::new();
        let mut buf = [0u8; 512];
        while !received.contains("data: chunk-one") {
            let n = client.read(&mut buf).unwrap();
            assert!(n > 0, "EOF before chunk 1");
            received.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        tx_seen_chunk1.send(()).unwrap();

        while !received.contains("data: chunk-two") {
            let n = client.read(&mut buf).unwrap();
            assert!(n > 0, "EOF before chunk 2");
            received.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        up_thread.join().unwrap();
    }

    #[test]
    fn websocket_101_bidirectional_tunnel() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let saw_auth = Arc::new(AtomicBool::new(false));
        let saw_auth_clone = Arc::clone(&saw_auth);

        let up_thread = thread::spawn(move || {
            let (stream, _) = upstream.accept().unwrap();
            let mut ws = tungstenite::accept_hdr(
                stream,
                move |req: &tungstenite::handshake::server::Request,
                      resp: tungstenite::handshake::server::Response| {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer bst_real_ed25519_secret_token_do_not_leak" {
                        saw_auth_clone.store(true, Ordering::SeqCst);
                    }
                    Ok(resp)
                },
            )
            .unwrap();

            let msg = ws.read().unwrap();
            assert_eq!(msg.into_text().unwrap().as_str(), "hello-via-shim");
            ws.send(Message::Text("pong-from-ingress".into())).unwrap();
            let _ = ws.close(None);
        });

        let shim = CageShim::start(base_config(format!("http://{upstream_addr}"))).unwrap();
        let ws_url = format!("ws://{}/v1/responses", shim.listen_addr());
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", shim.shim_token())).unwrap(),
        );
        let (mut client_ws, resp) = tungstenite::connect(req).unwrap();
        assert_eq!(resp.status().as_u16(), 101);

        client_ws
            .send(Message::Text("hello-via-shim".into()))
            .unwrap();
        let reply = client_ws.read().unwrap();
        assert_eq!(reply.into_text().unwrap().as_str(), "pong-from-ingress");
        let _ = client_ws.close(None);

        up_thread.join().unwrap();
        assert!(saw_auth.load(Ordering::SeqCst));
        assert_eq!(shim.metrics().ws_upgrades, 1);
    }

    #[test]
    fn precommit_409_rebind_writes_0600_opaque_signal_without_lease_token() {
        let dir = TempDir::new().unwrap();
        let rebind_path = dir.path().join("rebind.json");
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        let up_thread = thread::spawn(move || {
            let (mut s, _) = upstream.accept().unwrap();
            let _ = read_http_head(&mut s).unwrap();
            let body = serde_json::json!({
                "error": {
                    "code": "cage_rebind_required",
                    "message": "Tokyo cohort exhausted",
                    "rebind_ref": "rb_houston_01",
                    "current_cohort_tz": "Asia/Tokyo",
                    "lease_token": "DECOY_SECRET_MUST_NEVER_BE_WRITTEN",
                    "offer": {
                        "cohort_key": "openai:US:America/Chicago:en_US.UTF-8:linux",
                        "cage": {
                            "timezone": "America/Chicago",
                            "country_code": "US",
                            "locale": "en_US.UTF-8"
                        }
                    },
                    "available_cohorts": ["America/Chicago", "Europe/London"]
                }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 409 Conflict\r\nX-Berth-Action: cage_rebind_required\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).unwrap();
        });

        let mut cfg = base_config(format!("http://{upstream_addr}"));
        cfg.rebind_file = Some(rebind_path.clone());
        let shim = CageShim::start(cfg).unwrap();

        let resp = send_raw_http(
            shim.listen_addr(),
            &format!(
                "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 2\r\n\r\n{{}}",
                shim.listen_addr().port(),
                shim.shim_token()
            ),
        );
        up_thread.join().unwrap();
        assert!(resp.starts_with("HTTP/1.1 409 "), "got: {resp}");
        assert!(resp.contains("cage_rebind_required"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&rebind_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let raw_rebind = fs::read_to_string(&rebind_path).unwrap();
        assert!(!raw_rebind.contains("bst_real_ed25519_secret_token_do_not_leak"));
        assert!(!raw_rebind.contains("DECOY_SECRET_MUST_NEVER_BE_WRITTEN"));
        let sig: RebindSignal = serde_json::from_str(&raw_rebind).unwrap();
        assert_eq!(sig.rebind_ref, "rb_houston_01");
        assert_eq!(sig.current_cohort_tz, "Asia/Tokyo");
        assert_eq!(sig.recommended_cohort_tz, "America/Chicago");
        assert_eq!(sig.recommended_country_code, "US");
        assert_eq!(sig.recommended_locale, "en_US.UTF-8");
        assert_eq!(sig.available_cohorts, vec!["America/Chicago", "Europe/London"]);
        assert_eq!(shim.metrics().rebind_signals, 1);
    }

    fn encode_base64_std(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(b2 & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn forward_proxy_http_connect_and_socks5h_tunnel_via_berth_ingress() {
        // Mock Berth Ingress Relay accepting `CONNECT host:port` + X-Berth-Lease and echoing payload.
        let ingress_ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let ingress_addr = ingress_ln.local_addr().unwrap();
        let seen_targets = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen_targets);

        let _ingress_thread = thread::spawn(move || {
            for stream in ingress_ln.incoming().take(2) {
                let mut s = stream.unwrap();
                let (line, hdrs, trailing) = read_http_head(&mut s).unwrap().unwrap();
                assert!(line.starts_with("CONNECT "), "expected CONNECT, got {line}");
                let has_lease = hdrs.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("x-berth-lease")
                        && v == "bst_real_ed25519_secret_token_do_not_leak"
                });
                assert!(has_lease, "missing authoritative X-Berth-Lease header");
                let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
                seen_clone.lock().unwrap().push(target);

                s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .unwrap();
                s.flush().unwrap();
                if !trailing.is_empty() {
                    s.write_all(&trailing).unwrap();
                    s.flush().unwrap();
                }
                let mut buf = [0u8; 256];
                if let Ok(n) = s.read(&mut buf)
                    && n > 0
                {
                    let _ = s.write_all(&buf[..n]);
                    let _ = s.flush();
                }
            }
        });

        let shim = CageShim::start(base_config(format!("http://{ingress_addr}"))).unwrap();
        let proxy_addr = shim.proxy_addr();
        let token = shim.shim_token().to_string();

        // 1. Unauthenticated HTTP CONNECT MUST return 407 Proxy Authentication Required!
        let unauth = send_raw_http(
            proxy_addr,
            "CONNECT statsig.anthropic.com:443 HTTP/1.1\r\nHost: statsig.anthropic.com:443\r\n\r\n",
        );
        assert!(unauth.starts_with("HTTP/1.1 407 "), "expected 407, got: {unauth}");

        // 2. Authenticated HTTP CONNECT tunnels full-duplex bytes through Berth Ingress!
        let basic = encode_base64_std(format!("bh:{token}").as_bytes());
        let mut c1 = TcpStream::connect(proxy_addr).unwrap();
        c1.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            c1,
            "CONNECT statsig.anthropic.com:443 HTTP/1.1\r\nHost: statsig.anthropic.com:443\r\nProxy-Authorization: Basic {basic}\r\n\r\n"
        )
        .unwrap();
        c1.flush().unwrap();
        let (st1, _, _) = read_http_head(&mut c1).unwrap().unwrap();
        assert!(st1.contains("200"), "expected 200, got {st1}");
        c1.write_all(b"ping-connect").unwrap();
        c1.flush().unwrap();
        let mut echo1 = [0u8; 12];
        c1.read_exact(&mut echo1).unwrap();
        assert_eq!(&echo1, b"ping-connect");
        drop(c1);

        // 3. Authenticated SOCKS5h (RFC 1928 + RFC 1929 + ATYP=0x03 domain) tunnels cleanly!
        let mut c2 = TcpStream::connect(proxy_addr).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Greeting: VER=5, NMETHODS=1, METHOD=0x02 (user/pass)
        c2.write_all(&[0x05, 0x01, 0x02]).unwrap();
        let mut method_resp = [0u8; 2];
        c2.read_exact(&mut method_resp).unwrap();
        assert_eq!(method_resp, [0x05, 0x02]);
        // Auth: VER=1, "bh", token
        let mut auth_msg = vec![0x01, 2, b'b', b'h', token.len() as u8];
        auth_msg.extend_from_slice(token.as_bytes());
        c2.write_all(&auth_msg).unwrap();
        let mut auth_resp = [0u8; 2];
        c2.read_exact(&mut auth_resp).unwrap();
        assert_eq!(auth_resp, [0x01, 0x00]);
        // Request: CONNECT domain="api.ipify.org" port=443
        let dom = b"api.ipify.org";
        let mut req_msg = vec![0x05, 0x01, 0x00, 0x03, dom.len() as u8];
        req_msg.extend_from_slice(dom);
        req_msg.extend_from_slice(&443u16.to_be_bytes());
        c2.write_all(&req_msg).unwrap();
        let mut rep = [0u8; 10];
        c2.read_exact(&mut rep).unwrap();
        assert_eq!(rep[0], 0x05);
        assert_eq!(rep[1], 0x00);
        c2.write_all(b"ping-socks5h").unwrap();
        c2.flush().unwrap();
        let mut echo2 = [0u8; 12];
        c2.read_exact(&mut echo2).unwrap();
        assert_eq!(&echo2, b"ping-socks5h");
        drop(c2);

        let targets = seen_targets.lock().unwrap().clone();
        assert_eq!(
            targets,
            vec!["statsig.anthropic.com:443", "api.ipify.org:443"]
        );
    }

    #[test]
    fn berth_edge_409_recommended_cohort_extracted_and_subsequent_retries_fast_aborted() {
        let upstream_ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream_ln.local_addr().unwrap();
        let upstream_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&upstream_hits);

        let _up_thread = thread::spawn(move || {
            for stream in upstream_ln.incoming().take(2) {
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let mut s = stream.unwrap();
                let _ = read_http_head(&mut s);
                let body = serde_json::json!({
                    "error": {
                        "code": "cage_rebind_required",
                        "message": "No healthy accounts remain in cohort SG/Asia/Singapore",
                        "recommended_cohort": {
                            "account_id": 24,
                            "landing_node_id": 2,
                            "cohort_timezone": "America/Los_Angeles",
                            "cohort_country_code": "US",
                            "cohort_locale": "en_US.UTF-8"
                        }
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 409 Conflict\r\nX-Berth-Action: cage_rebind_required\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });

        let mut cfg = base_config(format!("http://{upstream_addr}"));
        cfg.cage_tz = "Asia/Singapore".into();
        cfg.abort_retries_on_rebind = true;
        let shim = CageShim::start(cfg).unwrap();
        let token = shim.shim_token().to_string();
        let host = format!("127.0.0.1:{}", shim.listen_addr().port());

        let req_raw = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let resp1 = send_raw_http(shim.listen_addr(), &req_raw);
        assert!(resp1.starts_with("HTTP/1.1 409 "), "resp1={resp1}");
        assert!(shim.has_rebind_signal());
        let sig = shim.metrics().last_rebind.expect("rebind signal captured");
        assert_eq!(sig.recommended_cohort_tz, "America/Los_Angeles");
        assert_eq!(sig.recommended_country_code, "US");
        assert_eq!(sig.recommended_locale, "en_US.UTF-8");
        assert_eq!(sig.recommended_account_id, Some(24));

        // Second request (SDK retry) MUST be fast-rejected locally with HTTP 400 without hitting upstream!
        let resp2 = send_raw_http(shim.listen_addr(), &req_raw);
        assert!(resp2.starts_with("HTTP/1.1 400 "), "resp2={resp2}");
        assert!(resp2.contains("cage_rebind_in_progress"));
        assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);
    }
}
