//! Host smoke for the combined framework: prove the Swarm (`ant_*`) and
//! IPFS (`freedom_ipfs_*`) C ABIs are both live in ONE process, and that
//! the in-process bee gateway shim actually serves HTTP.
//!
//! The functions are reached through the aggregator's re-exported Rust
//! paths (`freedom_mobile_ffi::ant_init`, …) rather than an `extern "C"`
//! block: a dependency's `#[no_mangle]` symbols are only auto-exported
//! from a `staticlib` (the iOS slice), not force-loaded into a downstream
//! test binary, so a Rust-path call is what keeps them linked. Runs on
//! the host (macOS/Linux), no iOS device needed. `ant_init` spawns a real
//! libp2p node but `/health` is constant after bind, so this needs no
//! peers/network to pass.
//!
//! Run: `cargo test --release --test smoke -- --nocapture`

use freedom_mobile_ffi::{
    ant_free_string, ant_init, ant_shutdown, ant_start_gateway, ant_stop_gateway,
    freedom_ipfs_string_free, freedom_ipfs_version,
};
use std::ffi::{c_char, CStr, CString};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ptr;
use std::time::{Duration, Instant};

/// Drain + free an `out_err` C string written by the ant FFI.
fn take_err(err: *mut c_char) -> String {
    if err.is_null() {
        return "(no error message)".into();
    }
    let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
    unsafe { ant_free_string(err) };
    s
}

/// Minimal HTTP/1.1 GET; returns the full raw response. Retries until
/// `deadline` because `ant_start_gateway` binds asynchronously inside
/// the spawned task.
fn http_get_until(authority: &str, path: &str, deadline: Instant) -> Result<String, String> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    let mut last = String::from("never connected");
    while Instant::now() < deadline {
        match TcpStream::connect(authority) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                if let Err(e) = stream.write_all(req.as_bytes()) {
                    last = format!("write: {e}");
                } else {
                    let mut buf = String::new();
                    match stream.read_to_string(&mut buf) {
                        Ok(_) if !buf.is_empty() => return Ok(buf),
                        Ok(_) => last = "empty response".into(),
                        Err(e) => last = format!("read: {e}"),
                    }
                }
            }
            Err(e) => last = format!("connect: {e}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

#[test]
fn both_abis_live_and_gateway_serves_in_process() {
    // --- IPFS ABI is linked & callable in this binary ---
    let vptr = unsafe { freedom_ipfs_version() };
    assert!(!vptr.is_null(), "freedom_ipfs_version returned null");
    let ipfs_version = unsafe { CStr::from_ptr(vptr) }.to_string_lossy().into_owned();
    unsafe { freedom_ipfs_string_free(vptr) };
    assert!(!ipfs_version.is_empty(), "freedom_ipfs_version was empty");
    println!("freedom-ipfs version: {ipfs_version}");

    // --- Swarm: boot a real node + the in-process bee gateway ---
    let dir = std::env::temp_dir().join(format!("ant-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_c = CString::new(dir.to_str().unwrap()).unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { ant_init(dir_c.as_ptr(), &mut err) };
    assert!(!handle.is_null(), "ant_init failed: {}", take_err(err));

    // Non-default port so a real bee/antd on 1633 doesn't interfere.
    let authority = "127.0.0.1:16633";
    let addr_c = CString::new(authority).unwrap();
    // Full-node mode, no gnosis RPC (the nullable 4th arg — light-mode
    // chain reads aren't exercised by this host smoke).
    let mut gerr: *mut c_char = ptr::null_mut();
    let started =
        unsafe { ant_start_gateway(handle, addr_c.as_ptr(), false, ptr::null(), &mut gerr) };
    assert!(started, "ant_start_gateway failed: {}", take_err(gerr));

    // Idempotent: a second start while live is a no-op success.
    let again = unsafe {
        ant_start_gateway(handle, addr_c.as_ptr(), false, ptr::null(), ptr::null_mut())
    };
    assert!(again, "second ant_start_gateway should be a no-op success");

    // The gateway should serve bee's /health (200 + {status,version,apiVersion}).
    let deadline = Instant::now() + Duration::from_secs(5);
    let resp = http_get_until(authority, "/health", deadline)
        .unwrap_or_else(|e| panic!("GET /health failed: {e}"));
    let status_line = resp.lines().next().unwrap_or("");
    println!("/health status: {status_line}");
    assert!(status_line.contains("200"), "expected 200, got: {status_line}");
    assert!(resp.contains("apiVersion"), "health body missing apiVersion: {resp}");
    assert!(resp.contains("ant-ffi/"), "health version should be ant-ffi agent: {resp}");

    // Clean teardown.
    assert!(
        unsafe { ant_stop_gateway(handle) },
        "ant_stop_gateway should report a running gateway was stopped"
    );
    unsafe { ant_shutdown(handle) };
    let _ = std::fs::remove_dir_all(&dir);
}
