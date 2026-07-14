#!/usr/bin/env bash
# Smoke test: builds litegraft, creates a real SQLite database (via the
# Python stdlib sqlite3 module — no network, no extra installs), then drives
# the binary through the full loop: init -> snap -> branch -> mutate ->
# snap (dedup) -> diff (table attribution) -> checkout (byte-identical
# restore) -> WAL guard -> verify -> gc. Self-contained: temp dirs only,
# idempotent. Prints "SMOKE OK" on success.
set -euo pipefail

cd "$(dirname "$0")/.."

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

command -v python3 >/dev/null || fail "python3 (with stdlib sqlite3) is required to create the test database"

echo "[smoke] building..."
cargo build --quiet
BIN="$PWD/target/debug/litegraft"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/litegraft-smoke.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
DB="$WORK/fixtures.db"

# --- 1. version/help sanity ---------------------------------------------------
"$BIN" --version | grep -q '^litegraft 0\.1\.0$' || fail "--version mismatch"
"$BIN" --help | grep -q 'COMMANDS:' || fail "--help missing sections"
echo "[smoke] version/help OK"

# --- 2. create a real SQLite database -----------------------------------------
python3 - "$DB" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("PRAGMA page_size=4096")
con.execute("PRAGMA journal_mode=DELETE")
con.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, plan TEXT)")
con.executemany("INSERT INTO users VALUES(?,?,?)",
    [(i, f"user{i:04d}", "free") for i in range(1, 401)])
con.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INTEGER, cents INTEGER)")
con.executemany("INSERT INTO orders VALUES(?,?,?)",
    [(i, (i % 400) + 1, i * 131 % 90000) for i in range(1, 1201)])
con.commit(); con.close()
EOF
echo "[smoke] created $(basename "$DB") ($(wc -c < "$DB") bytes)"

# --- 3. init + baseline snapshot ------------------------------------------------
"$BIN" init "$DB" | grep -q 'initialized empty store' || fail "init output"
"$BIN" snap "$DB" -m "baseline fixture" | tee "$WORK/snap1.out"
grep -q '0 deduped' "$WORK/snap1.out" || fail "first snap should dedup nothing"
"$BIN" snap "$DB" | grep -q 'no page changes' || fail "idempotent re-snap not detected"

# --- 4. branch, mutate, dedup snapshot ------------------------------------------
"$BIN" branch "$DB" try-migration | grep -q 'branch try-migration created' || fail "branch create"
"$BIN" checkout "$DB" try-migration | grep -q 'checked out branch try-migration' || fail "checkout branch"
BASELINE_MD5=$( (md5sum "$DB" 2>/dev/null || md5 -q "$DB") | awk '{print $1}')

python3 - "$DB" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("UPDATE users SET plan='pro' WHERE id % 40 = 0")
con.commit(); con.close()
EOF
"$BIN" snap "$DB" -m "migration: upgrade every 40th user" --json > "$WORK/snap2.json"
grep -q '"changed":true' "$WORK/snap2.json" || fail "mutated snap not marked changed"
NEW=$(sed -n 's/.*"new_objects":\([0-9]*\).*/\1/p' "$WORK/snap2.json")
PAGES=$(sed -n 's/.*"pages":\([0-9]*\).*/\1/p' "$WORK/snap2.json")
[ "$NEW" -lt "$PAGES" ] || fail "dedup failed: wrote $NEW of $PAGES pages"
echo "[smoke] dedup snap wrote only $NEW of $PAGES pages"

# --- 5. table-attributed diff -----------------------------------------------------
"$BIN" diff "$DB" main try-migration | tee "$WORK/diff.out"
grep -q 'users' "$WORK/diff.out" || fail "diff must attribute changes to users"
grep -qE 'orders +[0-9]+ changed' "$WORK/diff.out" && fail "orders did not change but appears changed"
"$BIN" diff "$DB" main try-migration --json | grep -q '"owner":"users"' || fail "diff --json owner rollup"

# --- 6. byte-identical restore across branches -------------------------------------
"$BIN" checkout "$DB" main | grep -q 'checked out branch main' || fail "checkout main"
RESTORED_MD5=$( (md5sum "$DB" 2>/dev/null || md5 -q "$DB") | awk '{print $1}')
[ "$BASELINE_MD5" = "$RESTORED_MD5" ] || fail "restore is not byte-identical"
echo "[smoke] checkout main restored the baseline byte-for-byte"
"$BIN" status "$DB" | grep -q 'state:   clean' || fail "status should be clean after checkout"
"$BIN" tables "$DB" | grep -q 'orders' || fail "tables must list orders"

# --- 7. WAL guard -------------------------------------------------------------------
head -c 4152 /dev/zero > "$DB-wal"   # 32-byte header + one 4096-page frame
if "$BIN" snap "$DB" 2> "$WORK/wal.err"; then fail "snap must refuse with pending WAL frames"; fi
grep -q 'wal_checkpoint' "$WORK/wal.err" || fail "WAL refusal must print the checkpoint fix"
rm "$DB-wal"
echo "[smoke] WAL guard refused a stale main file"

# --- 8. verify + gc -----------------------------------------------------------------
"$BIN" verify "$DB" | grep -q 'verify OK' || fail "verify"
"$BIN" gc "$DB" --json | grep -q '"removed":0' || fail "gc removed live objects"

echo "SMOKE OK"
