//! Live P2P smoke for the Myotis member (Phase 0 spike): boot a mainnet
//! node through the same C-ABI surface iOS will call, wait for SYNCED,
//! and resolve one ENS name over verified state.
//!
//! Like tests/smoke.rs, functions are reached through the aggregator's
//! re-exported Rust paths so the `#[no_mangle]` symbols stay linked.
//!
//! IGNORED by default: needs live devp2p/libp2p peers and takes minutes
//! on a fresh data dir (cold sync). Run manually:
//!   cargo test --release --test myotis_live -- --ignored --nocapture

use freedom_mobile_ffi::{
    myotis_create, myotis_init, myotis_resolve_ens_json, myotis_start, myotis_status_json,
    myotis_stop, myotis_string_free,
};
use std::ffi::{c_char, CStr, CString};
use std::time::{Duration, Instant};

/// The engine ABI this test was written against (myotis v0.1.7).
const EXPECTED_ABI: i32 = 22;

/// Copy + free a myotis-owned C string.
fn take_string(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null(), "myotis returned a NULL string");
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    unsafe { myotis_string_free(ptr) };
    s
}

#[test]
#[ignore = "live P2P: needs peers, minutes of cold sync"]
fn mainnet_syncs_and_resolves_ens() {
    let abi = myotis_init();
    assert_eq!(abi, EXPECTED_ABI, "engine ABI mismatch — update the pin or this test");

    let data_dir = std::env::temp_dir().join(format!("myotis-smoke-{}", std::process::id()));
    let network = CString::new("mainnet").unwrap();
    let dir = CString::new(data_dir.to_str().unwrap()).unwrap();
    let handle = unsafe { myotis_create(network.as_ptr(), dir.as_ptr()) };
    assert!(handle >= 1, "myotis_create failed: {handle}");
    assert!(myotis_start(handle), "myotis_start returned false");

    // Cold sync budget. The docs put a fresh profile at "minutes"; CI
    // fresh-profile runs on desktop use similar ceilings.
    let deadline = Instant::now() + Duration::from_secs(600);
    let synced_status = loop {
        let status = take_string(myotis_status_json(handle));
        if status.contains("\"SYNCED\"") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "no SYNCED within budget; last status: {status}"
        );
        std::thread::sleep(Duration::from_secs(2));
    };
    println!("status at SYNCED: {synced_status}");

    // Right at SYNCED a verified read can race snap-state availability
    // ("state unavailable for <registry>") — the same transient the
    // desktop integration retries (freedom-browser f1faab04). Retry for a
    // bounded window; only a non-transient failure is a real failure.
    let name = CString::new("vitalik.eth").unwrap();
    let resolve_deadline = Instant::now() + Duration::from_secs(180);
    let resolved = loop {
        let r = take_string(unsafe { myotis_resolve_ens_json(handle, name.as_ptr()) });
        if r.contains("\"status\":\"ok\"") || Instant::now() >= resolve_deadline {
            break r;
        }
        println!("resolve_ens (retrying): {r}");
        std::thread::sleep(Duration::from_secs(5));
    };
    println!("resolve_ens: {resolved}");
    assert!(
        resolved.contains("\"status\":\"ok\"") && resolved.contains("\"addressHex\""),
        "expected a verified ENS address, got: {resolved}"
    );

    myotis_stop(handle);
    let _ = std::fs::remove_dir_all(&data_dir);
}
