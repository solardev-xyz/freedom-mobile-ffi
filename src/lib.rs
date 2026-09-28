//! Combined iOS FFI for the Freedom browser: Swarm (`ant-ffi`) + IPFS
//! (`freedom-ipfs-mobile`) in a single Rust staticlib.
//!
//! Both dependencies expose hand-written `#[no_mangle] extern "C"`
//! surfaces (`ant_*` and `freedom_ipfs_*`). Re-exporting each crate's
//! public items keeps them reachable from this staticlib crate so the
//! linker retains every C-ABI symbol in the produced `.a`; the symbol
//! namespaces are disjoint (`ant_` vs `freedom_ipfs_`), so the glob
//! re-exports don't collide on the C side.
//!
//! Nothing else lives here on purpose: the value of this crate is the
//! single compilation graph (one std / allocator / libp2p / tokio), not
//! any new behaviour — with one exception, [`freedom_mobile_init_logging`]:
//! the `tracing` subscriber is process-wide, and both nodes want a layer
//! on it, so only the crate that links them both can install it.

pub use ant_ffi::*;
pub use freedom_ipfs_mobile::*;
// SPIKE (Phase 0): third member — Myotis (`myotis_*` C ABI), same
// re-export-to-retain-symbols pattern; namespace disjoint from the others.
// Unlike ant-ffi, myotis-engine keeps its C surface in a `capi` module
// rather than the crate root.
pub use myotis_engine::capi::*;
// Fourth member: Radicle. Its C surface is UniFFI scaffolding
// (`uniffi_libradicle_uniffi_*` + `ffi_libradicle_uniffi_*`), emitted by
// macros as #[no_mangle] extern "C" — the same re-export keeps the crate in
// the link graph so the staticlib retains them. Namespace disjoint again.
// Feature-gated so the Android slice (--no-default-features) skips it.
#[cfg(feature = "radicle")]
pub use libradicle_uniffi::*;
// Fifth member: Tor (Arti), `freedom_tor_*` (include/freedom_tor.h). Arti
// has no C surface of its own, so this crate carries a small one: a
// loopback SOCKS5 listener that only connects to `.onion` hosts. Opt-in.
#[cfg(feature = "tor")]
pub mod tor;
#[cfg(feature = "tor")]
pub use tor::*;

/// Install the process's one `tracing` subscriber, carrying both nodes'
/// layers: ant's log output (logcat tag `ant-ffi` on Android, stderr
/// elsewhere, filtered by `ANT_LOG` / `RUST_LOG`) and freedom-ipfs's
/// retrieval-progress recorder (what `freedom_ipfs_node_progress_snapshot_json`
/// reports).
///
/// Each node installs its own layer as the global subscriber when it
/// starts, and the first claim wins: with ant started first, freedom-ipfs's
/// recorder never lands and its progress snapshot stays empty for the life
/// of the process. Call this before `ant_init*` and
/// `freedom_ipfs_node_new*`; both then find the slot taken and leave it.
///
/// Idempotent and thread-safe. Returns true when the subscriber is this
/// one — installed now or by an earlier call — and false when something
/// else had already claimed the slot (a node started first, or the host's
/// own subscriber), in which case nothing changed.
///
/// Records from the `log` crate are bridged into the subscriber when the
/// `log` slot is still free. That part is best effort: if the host already
/// installed a `log` logger (e.g. `android_logger::init_once`), its logger
/// keeps receiving them and the return value is still true — the tracing
/// subscriber, which is what this call is for, is in place either way.
#[no_mangle]
pub extern "C" fn freedom_mobile_init_logging() -> bool {
    use std::sync::OnceLock;
    use tracing_log::AsLog;
    use tracing_subscriber::layer::SubscriberExt;
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry()
            .with(ant_ffi::log_layer())
            .with(freedom_ipfs_mobile::progress_layer());
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            return false;
        }
        // After the subscriber, so the max-level hint is the subscriber's.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(tracing::level_filters::LevelFilter::current().as_log())
            .init();
        true
    })
}
