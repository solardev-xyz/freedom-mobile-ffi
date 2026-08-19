# Why this crate exists

The merged staticlib contains two SQLite consumers:

- **ant** (`ant-retrieval`) → `rusqlite` → `libsqlite3-sys` (`links = "sqlite3"`, bundled amalgamation)
- **radicle** (heartwood) → `sqlite` → `sqlite3-sys` → `sqlite3-src` (`links = "sqlite3"`, bundled amalgamation)

Cargo rejects that graph outright: only one package may declare
`links = "sqlite3"`, exactly because two bundled SQLite copies in one
binary is a hazard. Desktop never hits this — ant runs out-of-process
there; the iOS aggregator is the first place both share a graph.

This stand-in replaces `sqlite3-src` via `[patch.crates-io]`. Compared to
the real 0.7.0 it drops the `links` key and the build script (the real
crate's Rust surface is an empty `#![no_std]` lib — its only job was
compiling `sqlite3.c`). Net effect:

- the `links` conflict disappears, and
- `sqlite3-sys`'s `extern "C"` declarations resolve at **final app link**
  against the single bundled SQLite object that `libsqlite3-sys` already
  puts in the merged `.a` (its symbols are pulled in via ant's rusqlite
  usage and satisfy radicle's references too).

One SQLite, one allocator, deterministic resolution.

**Invariant:** ant must keep `rusqlite`'s bundled SQLite in the graph. If
ant ever drops rusqlite, radicle's SQLite symbols go unresolved at app
link — restore a real SQLite source then.

**Version note:** keep `version` at the exact release the `sqlite3-sys`
pin expects (`^0.7` today). The bundled SQLite that actually serves both
consumers is whatever `libsqlite3-sys` ships; radicle uses core SQL only,
so the amalgamation feature flags `rusqlite` builds with are sufficient
(verified by the freedom-mobile-ffi host smoke test that boots a radicle
profile and node — both create their sqlite databases through this path).
