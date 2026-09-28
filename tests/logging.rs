//! `freedom_mobile_init_logging` must leave freedom-ipfs's progress
//! recorder on the process's subscriber even though ant starts first
//! (freedom-browser-android#156: ant's log subscriber used to win the
//! global slot and the IPFS progress snapshot was always empty).
//!
//! Its own test binary: the subscriber is process-global and can only be
//! installed once. Needs no network — the IPFS gateway is the offline,
//! cache-only one, and a miss still walks the request phases.
//!
//! Run: `cargo test --release --test logging`

use freedom_mobile_ffi::{
    ant_free_string, ant_init, ant_shutdown, freedom_ipfs_node_free, freedom_ipfs_node_gateway_url,
    freedom_ipfs_node_new_in_memory, freedom_ipfs_node_progress_snapshot_json,
    freedom_ipfs_node_start_gateway, freedom_ipfs_node_stop_gateway, freedom_ipfs_string_free,
    freedom_mobile_init_logging,
};
use std::ffi::{c_char, CStr, CString};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ptr;
use std::time::Duration;

fn ipfs_string(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null());
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { freedom_ipfs_string_free(ptr) };
    s
}

#[test]
fn progress_snapshot_fills_with_ant_started_first() {
    assert!(
        freedom_mobile_init_logging(),
        "subscriber slot was already taken"
    );

    // The app's order: the Swarm node comes up before the IPFS node.
    let dir = std::env::temp_dir().join(format!("ant-logging-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_c = CString::new(dir.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let ant = unsafe { ant_init(dir_c.as_ptr(), &mut err) };
    if ant.is_null() {
        let msg = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { ant_free_string(err) };
        panic!("ant_init failed: {msg}");
    }
    // Still ours after ant tried to install its own.
    assert!(freedom_mobile_init_logging());

    let node = unsafe { freedom_ipfs_node_new_in_memory() };
    assert!(!node.is_null());
    let addr = CString::new("127.0.0.1:0").unwrap();
    assert!(unsafe { freedom_ipfs_node_start_gateway(node, addr.as_ptr()) });
    let url = ipfs_string(unsafe { freedom_ipfs_node_gateway_url(node) });
    let authority = url.trim_start_matches("http://").trim_end_matches('/');

    let path = "/ipfs/bafkreigh2akiscaildcqabsyg3dfr6chu3fgpregiymsck7e7aqa4s52zy";
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1"),
        "no response: {response:?}"
    );

    let snapshot = ipfs_string(unsafe { freedom_ipfs_node_progress_snapshot_json(node) });
    let value: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    let events = value["events"].as_array().unwrap();
    assert!(
        events.iter().any(|event| event["path"] == path),
        "no progress events for {path}: {snapshot}"
    );

    assert!(unsafe { freedom_ipfs_node_stop_gateway(node) });
    unsafe { freedom_ipfs_node_free(node) };
    unsafe { ant_shutdown(ant) };
    let _ = std::fs::remove_dir_all(&dir);
}
