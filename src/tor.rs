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
//! C header: `include/freedom_tor.h`.

use std::ffi::{c_char, CStr, CString};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arti_client::config::TorClientConfigBuilder;
use arti_client::TorClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tor_rtcompat::PreferredRuntime;

/// The Arti release linked in, as reported to the app. Cargo.toml pins
/// `arti-client` to exactly this version; bump both together.
pub const ARTI_VERSION: &str = "0.46.0";

/// How long a CONNECT may take (onion-service rendezvous included) before
/// the SOCKS client is told the host is unreachable. Chromium's own
/// connect timeout is longer, so this is what the page sees.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

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
                tokio::spawn(async move {
                    let _ = serve(sock, client).await;
                });
            }
        });
    }
    Ok(Running { runtime, client, port, last_error })
}

// SOCKS5 (RFC 1928) reply codes.
const REP_OK: u8 = 0x00;
const REP_GENERAL: u8 = 0x01;
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

async fn reply(sock: &mut TcpStream, code: u8) -> std::io::Result<()> {
    sock.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

async fn serve(mut sock: TcpStream, client: Arc<TorClient<PreferredRuntime>>) -> std::io::Result<()> {
    // Greeting: VER NMETHODS METHODS…; we only speak "no authentication".
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    if hdr[0] != 5 {
        return Ok(());
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    sock.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        sock.write_all(&[5, 0xff]).await?;
        return Ok(());
    }
    sock.write_all(&[5, 0]).await?;

    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != 5 {
        return Ok(());
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
            reply(&mut sock, REP_ATYP_UNSUPPORTED).await?;
            return Ok(());
        }
    };
    let mut port = [0u8; 2];
    sock.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    if req[1] != 1 {
        reply(&mut sock, REP_CMD_UNSUPPORTED).await?;
        return Ok(());
    }
    // Onion-only: IP literals and clearnet names never leave through an exit.
    let Some(host) = host
        .filter(|h| is_onion_host(h))
        .map(|h| h.trim_end_matches('.').to_string())
    else {
        reply(&mut sock, REP_NOT_ALLOWED).await?;
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
            reply(&mut sock, REP_GENERAL).await?;
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
            let err = r.last_error.lock().unwrap_or_else(|p| p.into_inner()).clone();
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
    fn json_escapes() {
        assert_eq!(json_str("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
    }
}
