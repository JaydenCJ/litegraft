#!/usr/bin/env bash
# Example: branch a SQLite test fixture, run a risky migration on the branch,
# inspect the damage table-by-table, and restore the baseline byte-for-byte.
#
# Self-contained: builds nothing, needs the compiled binary plus python3
# (stdlib sqlite3) to create and mutate the database. Run from the repo root:
#
#   cargo build && bash examples/fixture-branching.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="$PWD/target/debug/litegraft"
[ -x "$BIN" ] || { echo "build first: cargo build" >&2; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/litegraft-example.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
DB="$WORK/fixtures.db"

step() { printf '\n\033[1m$ %s\033[0m\n' "$*"; }

# 1. A realistic fixture: 500 users, 2000 orders, one index.
python3 - "$DB" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("PRAGMA page_size=4096")
con.execute("PRAGMA journal_mode=DELETE")
con.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, email TEXT, plan TEXT)")
con.executemany("INSERT INTO users VALUES(?,?,?,?)",
    [(i, f"user{i:04d}", f"user{i:04d}@example.test", "free") for i in range(1, 501)])
con.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INTEGER, total_cents INTEGER)")
con.executemany("INSERT INTO orders VALUES(?,?,?)",
    [(i, (i % 500) + 1, i * 137 % 90000) for i in range(1, 2001)])
con.execute("CREATE INDEX idx_orders_user ON orders(user_id)")
con.commit(); con.close()
EOF
echo "created fixture: $DB ($(wc -c < "$DB") bytes)"

step litegraft init "$DB"
"$BIN" init "$DB"

step litegraft snap "$DB" -m "baseline fixture"
"$BIN" snap "$DB" -m "baseline fixture"

step litegraft branch "$DB" try-migration
"$BIN" branch "$DB" try-migration
"$BIN" checkout "$DB" try-migration

# 2. The "risky migration" — on the branch, not on your baseline.
step "python3 migration.py   # UPDATE users SET plan='pro' ..."
python3 - "$DB" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("UPDATE users SET plan='pro' WHERE id % 50 = 0")
con.execute("INSERT INTO users VALUES(501,'user0501','user0501@example.test','pro')")
con.commit(); con.close()
EOF

step litegraft snap "$DB" -m "migration: plan column backfill"
"$BIN" snap "$DB" -m "migration: plan column backfill"

# 3. What did the migration actually touch? Pages, attributed to tables.
step litegraft diff "$DB" main try-migration
"$BIN" diff "$DB" main try-migration

# 4. Back to the pristine baseline — byte-identical, instantly.
step litegraft checkout "$DB" main
"$BIN" checkout "$DB" main

step litegraft log "$DB"
"$BIN" log "$DB"

step litegraft branch "$DB"
"$BIN" branch "$DB"

printf '\nDone. The baseline was never at risk: the branch still has the migrated state.\n'
