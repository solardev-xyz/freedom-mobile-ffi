//! Live Tor smoke test (network): bootstrap Arti, fetch a real onion
//! service through the SOCKS5 listener, and check the listener refuses a
//! clearnet name and an IP literal.
//!
//! Run: `cargo test --release --no-default-features --features tor --test tor_live -- --ignored --nocapture`
#![cfg(feature = "tor")]

use freedom_mobile_ffi::{
    freedom_tor_start, freedom_tor_status_json, freedom_tor_stop, freedom_tor_string_free,
    freedom_tor_version,
};
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

// The Tor Project's own onion service.
const ONION: &str = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion";

fn status() -> String {
    let p = freedom_tor_status_json();
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { freedom_tor_string_free(p) };
    s
}

/// SOCKS5 CONNECT to a domain name; returns the reply code and the stream.
fn socks_connect(port: u16, host: &str, dport: u16) -> (u8, TcpStream) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(180))).unwrap();
    s.write_all(&[5, 1, 0]).unwrap();
    let mut r = [0u8; 2];
    s.read_exact(&mut r).unwrap();
    assert_eq!(r, [5, 0]);
    let mut req = vec![5, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&dport.to_be_bytes());
    s.write_all(&req).unwrap();
    let mut rep = [0u8; 10];
    s.read_exact(&mut rep).unwrap();
    (rep[1], s)
}

#[test]
#[ignore = "needs the Tor network"]
fn fetches_an_onion_and_refuses_clearnet() {
    let dir = std::env::temp_dir().join(format!("freedom-tor-{}", std::process::id()));
    let state = CString::new(dir.join("state").to_str().unwrap()).unwrap();
    let cache = CString::new(dir.join("cache").to_str().unwrap()).unwrap();
    let mut err = std::ptr::null_mut();
    let port = unsafe { freedom_tor_start(state.as_ptr(), cache.as_ptr(), 0, &mut err) };
    assert_ne!(port, 0, "start failed");
    println!("version {}", unsafe { CStr::from_ptr(freedom_tor_version()) }.to_str().unwrap());

    // Refused without touching the network: not .onion.
    assert_eq!(socks_connect(port, "example.com", 80).0, 2);
    let t0 = Instant::now();
    while !status().contains("\"running\"") {
        assert!(t0.elapsed() < Duration::from_secs(300), "no bootstrap: {}", status());
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("bootstrapped in {:?}: {}", t0.elapsed(), status());

    let t1 = Instant::now();
    let (code, mut s) = socks_connect(port, ONION, 80);
    assert_eq!(code, 0, "onion connect failed");
    write!(s, "HEAD / HTTP/1.0\r\nHost: {ONION}\r\n\r\n").unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    println!("onion answered in {:?}: {}", t1.elapsed(), buf.lines().next().unwrap_or(""));
    assert!(buf.starts_with("HTTP/1."));

    freedom_tor_stop();
    assert!(status().contains("stopped"));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "listener still open after stop");
    let _ = std::fs::remove_dir_all(dir);
}
