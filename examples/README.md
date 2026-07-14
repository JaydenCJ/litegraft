# litegraft examples

Runnable, self-contained demos. They need the `litegraft` binary (built with
`cargo build`) and `python3` (its stdlib `sqlite3` module creates the demo
databases — nothing to install, nothing leaves your machine).

| Example | What it shows |
|---|---|
| [`fixture-branching.sh`](fixture-branching.sh) | The core workflow: snapshot a test fixture, branch it, run a destructive migration, diff the two states table by table, then restore the baseline byte-for-byte. |

Run one from the repository root:

```bash
cargo build
bash examples/fixture-branching.sh
```

Each script works in a `mktemp -d` sandbox and cleans up after itself, so it
is safe to run repeatedly.

## The pattern for coding agents

`litegraft` is deliberately easy to wire into an agent loop — every command
takes the database path, returns proper exit codes, and speaks `--json`:

```bash
litegraft snap fixtures.db -m "before agent run" --json
# ... let the agent mutate the database ...
litegraft diff fixtures.db --json     # what did it actually touch?
litegraft checkout fixtures.db main --force   # reset for the next attempt
```

The `diff --json` owner rollup ("`users`: 3 pages changed") is the fastest
way to check that a change stayed inside the tables it was supposed to touch.
