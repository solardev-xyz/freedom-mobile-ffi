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
//! any new behaviour.

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
