//! Fifth member (feature `tor`): an embedded Arti (Tor) client behind a
//! loopback SOCKS5 listener that only ever connects to `.onion` hosts.
//!
//! The browser points its WebView at the listener for `*.onion` hosts only
//! (Chromium's SOCKS5 sends the hostname, so nothing is resolved locally).
//! The listener enforces the same rule on its own side: a CONNECT to
//! anything but a `.onion` domain name — an IP literal or a clearnet name —
//! is refused with "connection not allowed by ruleset", so a misrouted
//! request can never become a Tor exit connection either.
//!
//! One client per process, driven from a single global slot:
//! [`freedom_tor_start`] brings up a private tokio runtime, binds the
//! listener and starts bootstrapping in the background;
//! [`freedom_tor_status_json`] reports progress; [`freedom_tor_stop`] tears
//! the runtime (listener, client, every open stream) down. After a stop the
//! port is closed, so a caller that keeps routing `.onion` to it fails
//! closed.
//!
//! Trust boundary: a loopback port is reachable by every app on the device,
//! and Chromium's SOCKS5 client can't authenticate, so the listener can't
//! tell the browser's connections from another app's. What another app can
//! get is exactly what it could get by embedding Tor itself — `.onion`
//! streams, never an exit — so the listener only bounds what a misbehaving
//! peer can *cost*: the handshake must complete within
//! [`HANDSHAKE_TIMEOUT`], and at most [`MAX_CONNECTIONS`] connections are
//! open at once. At the cap a new connection is not refused: the one that
//! has gone longest without moving a byte (a stalled connect, an idle
//! keep-alive) is closed to make room. Refusing instead would let a peer
//! that parks [`MAX_CONNECTIONS`] cheap connections (CONNECTs to
//! nonexistent onions, idle streams) lock the browser out of `.onion`;
//! with eviction it has to keep opening or keep traffic flowing to hold
//! its slots, and the browser's newest connection is never the one to go.
//!
//! C header: `include/freedom_tor.h`.

use std::ffi::{c_char, CStr, CString};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arti_client::config::TorClientConfigBuilder;
use arti_client::TorClient;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tor_rtcompat::PreferredRuntime;

/// The Arti release linked in, as reported to the app. Cargo.toml pins
/// `arti-client` to exactly this version; bump both together.
pub const ARTI_VERSION: &str = "0.46.0";

/// How long a CONNECT may take (onion-service rendezvous included) before
/// the SOCKS client is told the host is unreachable (reply 0x04, the same
/// as a failed connect). Chromium's own connect timeout is longer, so this
/// is what the page sees.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a client has to send its greeting and CONNECT request. A peer
/// that opens the port and goes quiet is dropped after this, so it can't
/// pin a task and a file descriptor for good.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections open at once. Accepting one past this closes the idlest
/// open connection (see [`Conns`]). Far above what a browser opens to one
/// proxy, far below the process's fd limit.
pub const MAX_CONNECTIONS: usize = 256;

struct Running {
    runtime: tokio::runtime::Runtime,
    client: Arc<TorClient<PreferredRuntime>>,
    port: u16,
    last_error: Arc<Mutex<Option<String>>>,
}

static SLOT: Mutex<Option<Running>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Running>> {
    SLOT.lock().unwrap_or_else(|p| p.into_inner())
}

unsafe fn path_arg(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn set_err(out: *mut *mut c_char, msg: String) {
    if !out.is_null() {
        let c = CString::new(msg.replace('\0', " ")).unwrap_or_default();
        unsafe { *out = c.into_raw() };
    }
}

/// Start the Tor client, if it isn't running yet, and return the loopback
/// port its SOCKS5 listener is bound to (on `127.0.0.1`).
///
/// `state_dir` / `cache_dir` are Arti's persistent state (guards) and its
/// directory cache. `socks_port` 0 picks a free port. Bootstrapping runs in
/// the background; poll [`freedom_tor_status_json`]. A CONNECT before the
/// client is ready waits for bootstrap (bounded by the connect timeout).
///
/// Returns the bound port, or 0 on failure with `*err_out` (if non-null)
/// set to a message to free with [`freedom_tor_string_free`]. Calling it
/// while already running returns the running listener's port.
#[no_mangle]
pub unsafe extern "C" fn freedom_tor_start(
    state_dir: *const c_char,
    cache_dir: *const c_char,
    socks_port: u16,
    err_out: *mut *mut c_char,
) -> u16 {
    let mut slot = lock();
    if let Some(r) = slot.as_ref() {
        return r.port;
    }
    let (Some(state_dir), Some(cache_dir)) = (path_arg(state_dir), path_arg(cache_dir)) else {
        set_err(err_out, "state_dir and cache_dir are required".into());
        return 0;
    };
    match start(state_dir, cache_dir, socks_port) {
        Ok(r) => {
            let port = r.port;
            *slot = Some(r);
            port
        }
        Err(e) => {
            set_err(err_out, e);
            0
        }
    }
}

fn start(state_dir: PathBuf, cache_dir: PathBuf, socks_port: u16) -> Result<Running, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("arti")
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    let mut cfg = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir);
    // Android: app-private dirs are 0700 and owned by the app's uid, but
    // their ancestors (/data, /data/user/0) belong to system with group
    // bits Arti's fs-mistrust rejects, and there's no other user to hide
    // from inside the sandbox.
    cfg.storage().permissions().dangerously_trust_everyone();
    let cfg = cfg.build().map_err(|e| format!("config: {e}"))?;

    let last_error = Arc::new(Mutex::new(None));
    let (client, listener) = runtime.block_on(async {
        let client = TorClient::builder()
            .config(cfg)
            .create_unbootstrapped()
            .map_err(|e| format!("tor client: {e}"))?;
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, socks_port)))
            .await
            .map_err(|e| format!("bind 127.0.0.1:{socks_port}: {e}"))?;
        Ok::<_, String>((client, listener))
    })?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    {
        let client = client.clone();
        let last_error = last_error.clone();
        runtime.spawn(async move {
            if let Err(e) = client.bootstrap().await {
                *last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
            }
        });
    }
    {
        let client = client.clone();
        let conns = Conns::new(MAX_CONNECTIONS);
        runtime.spawn(async move {
            loop {
                let sock = match listener.accept().await {
                    Ok((sock, _)) => sock,
                    Err(_) => {
                        // e.g. out of file descriptors: don't spin.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let client = client.clone();
                conns.spawn(sock, move |sock| async move {
                    let _ = serve(sock, client).await;
                });
            }
        });
    }
    Ok(Running { runtime, client, port, last_error })
}

/// The listener's open connections, each with the time it last moved a
/// byte. Admitting one at the cap first aborts the idlest, so the slots
/// can't be held by connections that do nothing.
#[derive(Clone)]
struct Conns(Arc<ConnsInner>);

struct ConnsInner {
    cap: usize,
    epoch: Instant,
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, (Arc<AtomicU64>, tokio::task::AbortHandle)>>,
}

impl Conns {
    fn new(cap: usize) -> Self {
        Conns(Arc::new(ConnsInner {
            cap,
            epoch: Instant::now(),
            next_id: AtomicU64::new(0),
            open: Mutex::new(HashMap::new()),
        }))
    }

    fn now(&self) -> u64 {
        self.0.epoch.elapsed().as_millis() as u64
    }

    fn open(&self) -> std::sync::MutexGuard<'_, HashMap<u64, (Arc<AtomicU64>, tokio::task::AbortHandle)>> {
        self.0.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.open().len()
    }

    /// Run `serve` on `sock` (wrapped so its reads and writes count as
    /// activity) as a tracked task, evicting the idlest open connection
    /// first if at the cap. Must be called within a tokio runtime.
    fn spawn<F, Fut>(&self, sock: TcpStream, serve: F)
    where
        F: FnOnce(Touch<TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let last = Arc::new(AtomicU64::new(self.now()));
        let sock = Touch { inner: sock, last: last.clone(), conns: self.clone() };
        let this = self.clone();
        let mut open = self.open();
        let evicted = if open.len() >= self.0.cap {
            let idlest = open
                .iter()
                .min_by_key(|(id, (t, _))| (t.load(Ordering::Relaxed), **id))
                .map(|(id, _)| *id);
            idlest.and_then(|id| open.remove(&id)).map(|(_, h)| h)
        } else {
            None
        };
        // Spawned and registered under the lock: the task's own removal
        // (on finish or abort) waits for it, so it can't run first and
        // leave a stale entry behind.
        let task = tokio::spawn(async move {
            let _guard = Deregister(this, id);
            serve(sock).await;
        });
        open.insert(id, (last, task.abort_handle()));
        drop(open);
        if let Some(h) = evicted {
            h.abort();
        }
    }
}

/// Drops a connection's entry when its task ends, aborted or not.
struct Deregister(Conns, u64);

impl Drop for Deregister {
    fn drop(&mut self) {
        self.0.open().remove(&self.1);
    }
}

/// A connection's socket, stamping the time on every read or write that
/// moves bytes.
struct Touch<S> {
    inner: S,
    last: Arc<AtomicU64>,
    conns: Conns,
}

impl<S> Touch<S> {
    fn touch(&self) {
        self.last.store(self.conns.now(), Ordering::Relaxed);
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Touch<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.touch();
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Touch<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.inner).poll_write(cx, data);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            this.touch();
        }
        r
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// SOCKS5 (RFC 1928) reply codes.
const REP_OK: u8 = 0x00;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_CMD_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;

/// True for a hostname the listener will connect to: a DNS name whose last
/// label is `onion` (case-insensitive, trailing dot allowed), with a
/// non-empty label before it.
pub fn is_onion_host(host: &str) -> bool {
    let h = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    match h.strip_suffix(".onion") {
        Some(rest) => !rest.is_empty() && !rest.ends_with('.'),
        None => false,
    }
}

async fn reply<S: AsyncWrite + Unpin>(sock: &mut S, code: u8) -> std::io::Result<()> {
    sock.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// Read the SOCKS5 greeting and request, answering refusals itself.
/// `Some((host, port))` is an allowed `.onion` CONNECT still awaiting its
/// reply; `None` means the exchange is over (refused or not SOCKS5).
async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(sock: &mut S) -> std::io::Result<Option<(String, u16)>> {
    // Greeting: VER NMETHODS METHODS…; we only speak "no authentication".
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    if hdr[0] != 5 {
        return Ok(None);
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    sock.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        sock.write_all(&[5, 0xff]).await?;
        return Ok(None);
    }
    sock.write_all(&[5, 0]).await?;

    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != 5 {
        return Ok(None);
    }
    let host = match req[3] {
        3 => {
            let mut len = [0u8; 1];
            sock.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            sock.read_exact(&mut name).await?;
            String::from_utf8(name).ok()
        }
        1 => {
            let mut skip = [0u8; 4];
            sock.read_exact(&mut skip).await?;
            None
        }
        4 => {
            let mut skip = [0u8; 16];
            sock.read_exact(&mut skip).await?;
            None
        }
        _ => {
            reply(sock, REP_ATYP_UNSUPPORTED).await?;
            return Ok(None);
        }
    };
    let mut port = [0u8; 2];
    sock.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    if req[1] != 1 {
        reply(sock, REP_CMD_UNSUPPORTED).await?;
        return Ok(None);
    }
    // Onion-only: IP literals and clearnet names never leave through an exit.
    let Some(host) = host
        .filter(|h| is_onion_host(h))
        .map(|h| h.trim_end_matches('.').to_string())
    else {
        reply(sock, REP_NOT_ALLOWED).await?;
        return Ok(None);
    };
    Ok(Some((host, port)))
}

/// [`handshake`] bounded by `limit`: `None` (close the socket) when it was
/// refused, wasn't SOCKS5, failed, or the peer was too slow.
async fn read_target<S: AsyncRead + AsyncWrite + Unpin>(sock: &mut S, limit: Duration) -> Option<(String, u16)> {
    tokio::time::timeout(limit, handshake(sock)).await.ok()?.ok()?
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut sock: S, client: Arc<TorClient<PreferredRuntime>>) -> std::io::Result<()> {
    let Some((host, port)) = read_target(&mut sock, HANDSHAKE_TIMEOUT).await else {
        return Ok(());
    };

    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, client.connect((host.as_str(), port))).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::info!(target: "freedom_tor", "connect to {host}:{port} failed: {e}");
            reply(&mut sock, REP_HOST_UNREACHABLE).await?;
            return Ok(());
        }
        Err(_) => {
            tracing::info!(target: "freedom_tor", "connect to {host}:{port} timed out");
            reply(&mut sock, REP_HOST_UNREACHABLE).await?;
            return Ok(());
        }
    };
    reply(&mut sock, REP_OK).await?;
    let mut stream = stream;
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    Ok(())
}

/// Stop the Tor client: closes the SOCKS listener and every open stream and
/// drops the client (releasing its state-dir lock). A no-op when stopped.
#[no_mangle]
pub extern "C" fn freedom_tor_stop() {
    let taken = lock().take();
    if let Some(r) = taken {
        drop(r.client);
        r.runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

/// Current state as JSON:
/// `{"state":"stopped"}`, or
/// `{"state":"bootstrapping"|"running","port":N,"progress":0.0-1.0,
///   "summary":"…","blocked":"…"|null,"error":"…"|null}`.
/// `running` means the client is ready for traffic. Free the result with
/// [`freedom_tor_string_free`]. Never returns null.
#[no_mangle]
pub extern "C" fn freedom_tor_status_json() -> *mut c_char {
    let slot = lock();
    let json = match slot.as_ref() {
        None => r#"{"state":"stopped"}"#.to_string(),
        Some(r) => {
            let st = r.client.bootstrap_status();
            let err = current_error(&r.last_error, st.ready_for_traffic());
            let blocked = st.blocked().map(|b| b.to_string());
            format!(
                r#"{{"state":"{}","port":{},"progress":{},"summary":{},"blocked":{},"error":{}}}"#,
                if st.ready_for_traffic() { "running" } else { "bootstrapping" },
                r.port,
                st.as_frac(),
                json_str(&st.to_string()),
                blocked.as_deref().map_or("null".into(), json_str),
                err.as_deref().map_or("null".into(), json_str),
            )
        }
    };
    CString::new(json).unwrap_or_default().into_raw()
}

/// The background bootstrap's failure, if it still applies. Once the client
/// is ready for traffic (e.g. a later CONNECT bootstrapped it on demand) the
/// old failure is stale, so it's cleared rather than reported next to
/// `"running"`.
fn current_error(last_error: &Mutex<Option<String>>, ready: bool) -> Option<String> {
    let mut e = last_error.lock().unwrap_or_else(|p| p.into_inner());
    if ready {
        *e = None;
    }
    e.clone()
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The linked Arti version (e.g. "0.46.0"). Static; do not free.
#[no_mangle]
pub extern "C" fn freedom_tor_version() -> *const c_char {
    static V: &CStr = c"0.46.0";
    debug_assert_eq!(V.to_str().unwrap(), ARTI_VERSION);
    V.as_ptr()
}

/// Free a string returned by `freedom_tor_*`. Null is ignored.
#[no_mangle]
pub unsafe extern "C" fn freedom_tor_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onion_hosts() {
        assert!(is_onion_host("duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad.onion"));
        assert!(is_onion_host("www.example.ONION."));
        assert!(!is_onion_host("onion"));
        assert!(!is_onion_host(".onion"));
        assert!(!is_onion_host("a..onion"));
        assert!(!is_onion_host("example.com"));
        assert!(!is_onion_host("example.onion.com"));
        assert!(!is_onion_host("127.0.0.1"));
    }

    #[test]
    fn stale_bootstrap_error_cleared_once_ready() {
        let e = Mutex::new(Some("no network".to_string()));
        assert_eq!(current_error(&e, false).as_deref(), Some("no network"));
        assert_eq!(current_error(&e, true), None);
        // Stays cleared: it described a failure that no longer applies.
        assert_eq!(current_error(&e, false), None);
    }

    async fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        (c, s)
    }

    #[tokio::test]
    async fn silent_peer_dropped_after_handshake_timeout() {
        let (mut c, mut s) = pair().await;
        let t = std::time::Instant::now();
        assert_eq!(read_target(&mut s, Duration::from_millis(200)).await, None);
        assert!(t.elapsed() < Duration::from_secs(5));
        // A peer stalling mid-request is dropped the same way.
        let (mut c2, mut s2) = pair().await;
        c2.write_all(&[5, 1, 0, 5, 1, 0, 3]).await.unwrap();
        assert_eq!(read_target(&mut s2, Duration::from_millis(200)).await, None);
        drop(s2);
        let mut buf = Vec::new();
        c2.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, [5, 0]);
        drop(s);
        c.read_to_end(&mut buf).await.unwrap();
    }

    fn connect_req(host: &str, port: u16) -> Vec<u8> {
        let mut r = vec![5, 1, 0, 5, 1, 0, 3, host.len() as u8];
        r.extend_from_slice(host.as_bytes());
        r.extend_from_slice(&port.to_be_bytes());
        r
    }

    #[tokio::test]
    async fn handshake_allows_onion_refuses_rest() {
        let (mut c, mut s) = pair().await;
        c.write_all(&connect_req("Example.ONION.", 80)).await.unwrap();
        assert_eq!(
            read_target(&mut s, HANDSHAKE_TIMEOUT).await,
            Some(("Example.ONION".to_string(), 80))
        );

        let (mut c, mut s) = pair().await;
        c.write_all(&connect_req("example.com", 443)).await.unwrap();
        assert_eq!(read_target(&mut s, HANDSHAKE_TIMEOUT).await, None);
        drop(s);
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf[..4], [5, 0, 5, REP_NOT_ALLOWED]);
    }

    /// Stand-in for `serve`: reads until the peer closes.
    async fn drain(mut s: Touch<TcpStream>) {
        let mut buf = [0u8; 64];
        while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
    }

    /// Accepts `n` connections into `conns`, returning the client ends.
    async fn admit(conns: &Conns, l: &TcpListener, n: usize) -> Vec<TcpStream> {
        let mut out = Vec::new();
        for _ in 0..n {
            let c = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
            let (s, _) = l.accept().await.unwrap();
            conns.spawn(s, drain);
            out.push(c);
        }
        out
    }

    async fn closed(c: &mut TcpStream) -> bool {
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(Duration::from_secs(2), c.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
    }

    async fn still_open(c: &mut TcpStream) -> bool {
        let mut b = [0u8; 1];
        tokio::time::timeout(Duration::from_millis(200), c.read(&mut b)).await.is_err()
    }

    #[tokio::test]
    async fn full_listener_evicts_idlest_instead_of_refusing() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let conns = Conns::new(3);
        // Three squatters fill the listener, then two of them move bytes.
        let mut squat = admit(&conns, &l, 3).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        squat[0].write_all(b"x").await.unwrap();
        squat[2].write_all(b"x").await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(conns.len(), 3);

        // A fourth is admitted; the one that sat idle longest goes.
        let mut fresh = admit(&conns, &l, 1).await.pop().unwrap();
        assert!(closed(&mut squat[1]).await, "idlest connection evicted");
        assert!(still_open(&mut squat[0]).await);
        assert!(still_open(&mut squat[2]).await);
        assert!(still_open(&mut fresh).await, "new connection served, not closed on accept");
        assert_eq!(conns.len(), 3);

        // A squatter re-opening to push the newcomer out only evicts the
        // now-idlest of the rest, never the most recent.
        tokio::time::sleep(Duration::from_millis(30)).await;
        fresh.write_all(b"x").await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _again = admit(&conns, &l, 1).await;
        assert!(closed(&mut squat[0]).await);
        assert!(still_open(&mut fresh).await);

        // Connections that end on their own free their slot.
        drop(fresh);
        drop(squat);
        drop(_again);
        for _ in 0..100 {
            if conns.len() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(conns.len(), 0);
    }

    #[test]
    fn json_escapes() {
        assert_eq!(json_str("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
    }
}
