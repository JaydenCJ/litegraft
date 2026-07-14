# The litegraft store format

This document specifies the on-disk layout of a litegraft store (format
version 1), for tool authors and for anyone auditing what litegraft writes.
Everything here is plain files — no daemon, no lock server, no database of
its own.

## Location

By default a store lives in `<db>.litegraft/` next to the database it tracks,
the same way SQLite's own `-wal` and `-shm` sidecars sit next to the file
they belong to. `--store <dir>` overrides the location (useful when the
database lives on a read-only mount).

```text
app.db.litegraft/
├── LITEGRAFT            format marker: "litegraft store 1\n"
├── objects/ab/cd…       raw page bytes, keyed by SHA-256 (2-char shard dirs)
├── snaps/<id>           snapshot manifests (text, one file per snapshot)
├── refs/heads/<name>    branch heads (one snapshot id per file; names may
│                        contain "/" and become subdirectories)
└── HEAD                 "ref: <branch>\n" or "snap: <id>\n" (detached)
```

## Objects

An object is the verbatim content of one database page. Its key is the
lowercase hex SHA-256 of those bytes, stored at `objects/<first 2 hex
chars>/<remaining 62>`. Objects are immutable and idempotent: writing the
same page twice is a no-op, which is the whole deduplication story — a
snapshot only ever creates objects for pages the store has never seen.

Objects are written via a temp file and `rename()`, so a reader never
observes a truncated object, but they are deliberately **not fsynced**:
every object is re-creatable from the database file by re-running `snap`,
and `litegraft verify` re-hashes all of them, so a torn write after power
loss is detectable and cheap to repair. This keeps snapshots in the
low-millisecond range instead of paying one fsync per page.

## Snapshot manifests

A manifest is a line-based text file:

```text
litegraft snapshot 1
id 3b83d414bc5b…            (64 hex chars)
parent -                    (or a 64-hex parent id)
branch main                 (branch at creation time, informational)
created 1783872540          (unix seconds, UTC)
message baseline fixture    ("\n" and "\\" escaped; single line)
pagesize 4096
pages 22
--
<64-hex page hash, one line per page, in page order>
```

The snapshot id is **not** arbitrary: it is the SHA-256 of the state string
`"litegraft-state 1\n<pagesize>\n<hash1>\n<hash2>\n…"`. Two consequences:

1. Snapping an unchanged database is naturally idempotent — the id already
   exists, so `snap` just fast-forwards the branch.
2. `read_manifest` recomputes the id from the page-hash list on every load;
   a tampered or bit-rotted manifest is rejected with an integrity error.

Because the id covers only the *state* (not parent/message/created), two
branches that reach identical content share one manifest. The metadata lines
record the first creation.

## Refs and HEAD

A branch is a file under `refs/heads/` containing a snapshot id. `HEAD` names
the current branch (`ref: main`) or, after checking out a raw snapshot id, the
snapshot itself (`snap: <id>` — detached, and `snap` refuses to advance it
until you create a branch). A freshly initialized store has `HEAD → main`
with no `refs/heads/main` yet ("unborn", exactly like git).

Ref resolution order for user input: branch name, full 64-hex id, then unique
id prefix (minimum 4 hex chars). Branch names matching 64 hex chars are
rejected at creation time so they can never shadow ids.

## Page attribution

`diff` and `tables` map page numbers to owners by walking the file format
itself, over any `PageSource` (the live file or a manifest + objects):

| Page kind | How it is found |
|---|---|
| `sqlite_schema` | b-tree walk from page 1 |
| table / index b-trees | roots from decoded `sqlite_schema` records |
| overflow chains | spill pointers in leaf/index cells, per the min/max local payload formulas |
| freelist | trunk chain from header offset 32 |
| pointer map | computed positions when the auto-vacuum flag (offset 52) is set |
| lock byte | the page spanning offset 1 GiB, if the file is that large |

`WITHOUT ROWID` tables work unchanged: their rows live in index-type pages
and the walker dispatches on the page-type byte, not the schema. Corrupt
inputs (cycles, out-of-range pointers, unknown page types) fail with a
descriptive error — the walker never loops and never panics.

## Consistency guards

The main database file is only trustworthy when SQLite is not mid-write:

- `<db>-wal` larger than its 32-byte header ⇒ committed frames are not yet
  checkpointed; `snap`/`status`/`diff @`/`checkout` refuse and print the
  `PRAGMA wal_checkpoint(TRUNCATE);` fix. `--allow-wal` overrides.
- a non-empty `<db>-journal` ⇒ a rollback transaction may be in flight;
  litegraft refuses until it is resolved.
- `checkout` writes the restored file to a temp path in the same directory,
  fsyncs, then `rename()`s over the target, and removes stale `-wal`/`-shm`/
  `-journal` sidecars that belonged to the replaced state.

litegraft takes no SQLite locks; the guards catch the common failure modes,
but the documented contract is: snapshot when no writer is active.
