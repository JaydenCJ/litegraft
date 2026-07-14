# Contributing to litegraft

Thanks for your interest in improving litegraft. Issues, discussions and pull requests are all welcome.

## Getting started

Prerequisites: Rust 1.75 or newer (stable toolchain). Python 3 with the stdlib `sqlite3` module is used only by `scripts/smoke.sh` and the examples to create real databases.

```bash
git clone https://github.com/JaydenCJ/litegraft.git
cd litegraft
cargo build
cargo test
bash scripts/smoke.sh
```

`scripts/smoke.sh` creates a real SQLite database, then drives the compiled binary through the full snapshot → branch → mutate → diff → checkout → verify → gc loop and asserts on the output. It finishes in seconds and must print `SMOKE OK`.

## Before you open a pull request

1. `cargo fmt` — formatting is enforced.
2. `cargo clippy --all-targets -- -D warnings` — clippy must be clean.
3. `cargo test` — unit tests and both integration suites must pass.
4. `bash scripts/smoke.sh` — the smoke test must print `SMOKE OK`.
5. Add tests for behavior changes. Format logic lives in pure modules (`sha256`, `record`, `dbfile`, `btree`, `diff`) that are easy to unit-test; please keep it that way. Anything that touches the b-tree walker should be exercised against both the byte-built databases in `tests/common` and the committed real-SQLite fixture.

## Ground rules

- Zero runtime dependencies. litegraft reads the SQLite file format directly, so it needs no SQLite driver, no hash crate, no serde — adding any dependency needs a very strong justification in the PR description.
- No network calls, ever. litegraft operates on local files only; there is no telemetry and nothing to phone home to.
- Never write to a user's database except in `checkout`, and there only via the atomic temp-file + rename path. Read paths must stay strictly read-only.
- Honesty about staleness: the WAL/journal guards exist so litegraft never silently snapshots a file that SQLite considers incomplete. Do not weaken them; add explicit flags instead.
- Code comments and doc comments are written in English.

## Reporting bugs

Please include the `litegraft --version` output, the exact command and its stderr, `litegraft verify --json` output for store issues, and — for format bugs — `PRAGMA page_size/page_count/freelist_count` values from the affected database plus how it was produced (WAL or rollback mode, auto-vacuum on/off). A minimal database file that reproduces the problem is the fastest path to a fix.

## Security

If you find a security issue (e.g. a crafted database file causing out-of-bounds reads), please do not open a public issue. Use GitHub's private vulnerability reporting on this repository instead.
