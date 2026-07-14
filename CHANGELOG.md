# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-12

### Added

- SQLite file-format reader: 100-byte header parsing (page size, reserved bytes, stale-page-count fallback via `version-valid-for`, freelist head, text encoding, auto-vacuum flag) and a `PageSource` trait implemented by both live database files and store-backed snapshots.
- Content-addressed object store (`<db>.litegraft/`): raw pages keyed by an in-tree, dependency-free SHA-256; snapshots are line-based text manifests whose id is the hash of the whole state, so re-snapping an unchanged database is idempotent and identical states share one manifest.
- `litegraft snap`: page-level deduplicating snapshots — only pages the store has never seen are written, with counts, byte totals and elapsed milliseconds reported (`--json` for scripts).
- Branching and restore: `branch` (fork at head or `--at <ref>`), `checkout` (atomic temp-file + rename restore, byte-identical, stale `-wal`/`-shm`/`-journal` sidecars removed), detached-HEAD checkouts of raw snapshot ids, `log`, `status`, and unique-prefix ref resolution.
- Table-aware diff: a b-tree walker that attributes every page to its owner — tables, indexes, `sqlite_schema`, overflow chains, freelist trunks/leaves, pointer-map pages, the lock-byte page — so `litegraft diff` reports "users: 6 pages changed" instead of raw page numbers; works between any two refs, including snapshots, without touching the working file.
- `litegraft tables`: per-object page-ownership breakdown of any state (working file or historical snapshot).
- Safety guards: refuses to snapshot or restore while `<db>-wal` holds uncheckpointed frames (with the exact `PRAGMA wal_checkpoint(TRUNCATE);` fix printed) or a hot rollback journal exists; `checkout` refuses to discard un-snapshotted working states without `--force`.
- Store maintenance: `verify` (re-hash every object, recompute every manifest id, check refs) and `gc` (delete objects unreachable from any snapshot).
- Test suite: 48 unit tests, 43 integration tests (file-format walker against a committed real-SQLite fixture plus byte-built databases, and end-to-end CLI runs), and `scripts/smoke.sh`.

[0.1.0]: https://github.com/JaydenCJ/litegraft/releases/tag/v0.1.0
