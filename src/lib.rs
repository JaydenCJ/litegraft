//! litegraft — instant branch, snapshot and diff for SQLite database files.
//!
//! litegraft works on the SQLite file format itself: it hashes a database
//! page by page into a content-addressed object store, so snapshots cost
//! only the pages that actually changed, branches are just named pointers,
//! restores are byte-identical, and diffs can be attributed to the tables
//! that own each differing page — all offline, with no server and no SQL
//! layer.
//!
//! Module map:
//! - [`sha256`]  — dependency-free SHA-256 (the content-address function)
//! - [`dbfile`]  — SQLite header parsing + the [`dbfile::PageSource`] trait
//! - [`record`]  — varints and record (serial-type) decoding
//! - [`btree`]   — b-tree/freelist/ptrmap walking and page attribution
//! - [`store`]   — object store, snapshot manifests, branches, HEAD
//! - [`snapshot`] — snap, restore, working-state hashing
//! - [`diff`]    — page diff + per-table rollup
//! - [`cli`]     — the `litegraft` command

pub mod btree;
pub mod cli;
pub mod dbfile;
pub mod diff;
pub mod record;
pub mod sha256;
pub mod snapshot;
pub mod store;
