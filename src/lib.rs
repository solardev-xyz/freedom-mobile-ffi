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
