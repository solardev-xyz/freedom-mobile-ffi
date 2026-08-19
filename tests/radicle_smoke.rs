//! Host smoke for the Radicle member: boot an embedded node through the
//! same UniFFI surface iOS will call, read identity + status, and shut
//! down cleanly. No network needed — `start` binds nothing (`listen` is
//! empty) and we never connect to seeds.
//!
//! Beyond "the symbols are live", this validates the vendored no-op
//! `sqlite3-src` patch (see vendor/sqlite3-src/README.md): profile/node
//! boot creates and queries radicle's sqlite databases (node DB, seeding
//! policies), so every one of these calls goes through `sqlite3-sys`
//! bindings resolved against the bundled SQLite that ant's
//! `libsqlite3-sys` contributes. A feature-flag mismatch between the two
//! sqlite stacks would fail here, on the host, not on a phone.
//!
//! Like tests/smoke.rs, functions are reached through the aggregator's
//! re-exported Rust paths so the scaffolding symbols stay linked.
//!
//! Run: `cargo test --release --test radicle_smoke -- --nocapture`

use freedom_mobile_ffi::{identity, list_repos, list_seeded_repos, shutdown, start, status};

/// Radicle's control socket lives under the profile home, and unix socket
/// paths cap at ~104 bytes on macOS (`SUN_LEN`) — so the home must be a
/// SHORT path. `/tmp` directly, not `std::env::temp_dir()` (which on
/// macOS resolves to a long `/var/folders/...` path). Same workaround as
/// desktop Freedom.
fn short_home() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/radffi-{}", std::process::id()))
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("not JSON ({e}): {s}"))
}

#[test]
fn radicle_node_boots_reads_and_shuts_down() {
    let home = short_home();
    let _ = std::fs::remove_dir_all(&home);

    let started = json(&start(home.to_str().unwrap().into(), "freedom-smoke".into()));
    assert!(
        started.get("did").map(|d| d.as_str().unwrap().starts_with("did:key:")) == Some(true),
        "start failed: {started}"
    );

    // Identity comes back consistent with the profile just created.
    let id = json(&identity());
    assert_eq!(id["did"], started["did"]);
    assert_eq!(id["alias"], "freedom-smoke");
    assert!(id["nid"].as_str().is_some_and(|n| !n.is_empty()));

    // Storage scan (git) and policy inventory (sqlite read) both work on
    // a fresh, empty profile.
    assert_eq!(json(&list_repos()), serde_json::json!([]));
    assert_eq!(json(&list_seeded_repos()), serde_json::json!([]));

    // Peerless node reports zero connections, not an error.
    let st = json(&status());
    assert_eq!(st["connectedPeers"], 0, "status: {st}");

    let stopped = json(&shutdown());
    assert_eq!(stopped["ok"], true, "shutdown: {stopped}");

    let _ = std::fs::remove_dir_all(&home);
}
