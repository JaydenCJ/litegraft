//! End-to-end CLI tests against the compiled `litegraft` binary.
//!
//! Each test gets its own temp dir with a database produced by the in-repo
//! builder (`tests/common`), then drives the real binary through the same
//! workflows the README advertises: init -> snap -> branch -> mutate ->
//! diff -> checkout, plus the guard rails (WAL pending, dirty checkout,
//! corrupt store).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{encode_record, multi_leaf_table, overflow_table, small_table, write_db, TempDir, V};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_litegraft"))
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

/// Standard playground: events (multi-page) + notes tables, store initialized.
fn playground() -> (TempDir, PathBuf) {
    let dir = TempDir::new();
    let (db, _) = write_db(
        &dir,
        "app.db",
        &[multi_leaf_table("events", 120), small_table("notes")],
        0,
    );
    ok(&run(&["init", db.to_str().unwrap()]));
    (dir, db)
}

/// Rebuild the playground database with one extra `notes` row (a realistic
/// "the fixture changed" mutation: only notes pages + none of events').
fn mutate_notes(db: &Path) {
    let mut notes = small_table("notes");
    notes
        .rows
        .push((4, encode_record(&[V::I(4), V::T("a fourth note".into())])));
    let objs = [multi_leaf_table("events", 120), notes];
    let (bytes, _) = common::build_db(&objs, 0);
    std::fs::write(db, bytes).unwrap();
}

// ---- basics --------------------------------------------------------------------

#[test]
fn version_prints_the_cargo_version() {
    let out = ok(&run(&["--version"]));
    assert_eq!(
        out.trim(),
        format!("litegraft {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_lists_every_command() {
    let out = ok(&run(&["--help"]));
    for cmd in [
        "init", "snap", "log", "branch", "checkout", "status", "diff", "tables", "verify", "gc",
    ] {
        assert!(out.contains(cmd), "help missing {cmd}");
    }
}

#[test]
fn unknown_command_and_missing_db_exit_2() {
    let out = run(&["frobnicate", "x.db"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run(&["snap"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("database path"));
}

#[test]
fn init_rejects_a_non_sqlite_file() {
    let dir = TempDir::new();
    let path = dir.join("fake.db");
    std::fs::write(&path, b"definitely not a database, just bytes").unwrap();
    let out = run(&["init", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("SQLite"), "{}", stderr(&out));
}

#[test]
fn init_twice_fails_cleanly() {
    let (_dir, db) = playground();
    let out = run(&["init", db.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already initialized"));
}

#[test]
fn store_dir_can_be_overridden() {
    let dir = TempDir::new();
    let (db, _) = write_db(&dir, "app.db", &[small_table("notes")], 0);
    let store = dir.join("elsewhere");
    ok(&run(&[
        "init",
        db.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
    ]));
    assert!(store.join("LITEGRAFT").exists());
    ok(&run(&[
        "snap",
        db.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
    ]));
}

// ---- snap / log / status ---------------------------------------------------------

#[test]
fn first_snap_stores_every_page_and_second_is_a_noop() {
    let (_dir, db) = playground();
    let out = ok(&run(&["snap", db.to_str().unwrap(), "-m", "baseline"]));
    assert!(out.contains("snap "), "{out}");
    assert!(
        out.contains("0 deduped"),
        "first snap dedups nothing: {out}"
    );
    let again = ok(&run(&["snap", db.to_str().unwrap()]));
    assert!(again.contains("no page changes"), "{again}");
}

#[test]
fn snap_json_reports_dedup_counts() {
    let (_dir, db) = playground();
    ok(&run(&["snap", db.to_str().unwrap(), "-m", "baseline"]));
    mutate_notes(&db);
    let out = ok(&run(&[
        "snap",
        db.to_str().unwrap(),
        "-m",
        "one more note",
        "--json",
    ]));
    assert!(
        out.starts_with('{') && out.trim_end().ends_with('}'),
        "json object: {out}"
    );
    for key in [
        "\"id\":",
        "\"pages\":",
        "\"new_objects\":",
        "\"dedup_pages\":",
        "\"changed\":true",
    ] {
        assert!(out.contains(key), "missing {key} in {out}");
    }
    // Only the notes page (and page 1's header/schema page if touched)
    // changed; the events pages must all dedup.
    let new_objects: u32 = field(&out, "new_objects").parse().unwrap();
    let pages: u32 = field(&out, "pages").parse().unwrap();
    assert!(
        new_objects <= 3,
        "expected a tiny delta, wrote {new_objects} of {pages} pages"
    );
    assert!(pages > 10, "playground db should be multi-page");
}

/// Extract a bare (non-string) JSON field from single-line output.
fn field(json: &str, key: &str) -> String {
    let pat = format!("\"{key}\":");
    let start = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + pat.len();
    json[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect()
}

#[test]
fn log_lists_snapshots_newest_first_with_messages() {
    let (_dir, db) = playground();
    ok(&run(&["snap", db.to_str().unwrap(), "-m", "baseline"]));
    mutate_notes(&db);
    ok(&run(&[
        "snap",
        db.to_str().unwrap(),
        "-m",
        "added note four",
    ]));
    let out = ok(&run(&["log", db.to_str().unwrap()]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("added note four"), "{out}");
    assert!(lines[1].contains("baseline"), "{out}");
    let limited = ok(&run(&["log", db.to_str().unwrap(), "--limit", "1"]));
    assert_eq!(limited.lines().count(), 1);
    // JSON view: an array whose root snapshot has a null parent.
    let json = ok(&run(&["log", db.to_str().unwrap(), "--json"]));
    assert!(json.trim_start().starts_with('['), "{json}");
    assert!(
        json.contains("\"parent\":null"),
        "root snapshot has null parent: {json}"
    );
    assert!(json.matches("\"id\":").count() >= 2, "{json}");
}

#[test]
fn status_reports_clean_then_dirty() {
    let (_dir, db) = playground();
    ok(&run(&["snap", db.to_str().unwrap(), "-m", "baseline"]));
    let clean = ok(&run(&["status", db.to_str().unwrap()]));
    assert!(clean.contains("state:   clean"), "{clean}");
    mutate_notes(&db);
    let dirty = ok(&run(&["status", db.to_str().unwrap()]));
    assert!(dirty.contains("dirty"), "{dirty}");
    let json = ok(&run(&["status", db.to_str().unwrap(), "--json"]));
    assert!(json.contains("\"dirty\":true"), "{json}");
    assert!(json.contains("\"branch\":\"main\""), "{json}");
}

// ---- branch / checkout -----------------------------------------------------------

#[test]
fn branch_create_list_and_switch_roundtrip() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    let before = std::fs::read(&db).unwrap();

    ok(&run(&["branch", dbs, "fix/notes"]));
    let list = ok(&run(&["branch", dbs]));
    assert!(list.contains("* main"), "{list}");
    assert!(list.contains("  fix/notes"), "{list}");

    ok(&run(&["checkout", dbs, "fix/notes"]));
    mutate_notes(&db);
    ok(&run(&["snap", dbs, "-m", "notes work"]));

    // Back to main: the file must be byte-identical to the baseline.
    ok(&run(&["checkout", dbs, "main"]));
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "checkout must restore bytes exactly"
    );
    let list = ok(&run(&["branch", dbs]));
    assert!(list.contains("* main"), "{list}");

    // And forward again: the branch still has the extra note.
    ok(&run(&["checkout", dbs, "fix/notes"]));
    assert_ne!(std::fs::read(&db).unwrap(), before);
}

#[test]
fn branch_rejects_duplicates_and_bad_names() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    ok(&run(&["branch", dbs, "wip"]));
    let dup = run(&["branch", dbs, "wip"]);
    assert_eq!(dup.status.code(), Some(1));
    assert!(stderr(&dup).contains("already exists"));
    let bad = run(&["branch", dbs, "no spaces"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("invalid branch name"));
}

#[test]
fn checkout_refuses_to_discard_unsnapshotted_work() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    mutate_notes(&db);
    let refuse = run(&["checkout", dbs, "main"]);
    assert_eq!(refuse.status.code(), Some(1));
    assert!(
        stderr(&refuse).contains("un-snapshotted"),
        "{}",
        stderr(&refuse)
    );
    // --force discards on purpose.
    ok(&run(&["checkout", dbs, "main", "--force"]));
    let status = ok(&run(&["status", dbs]));
    assert!(status.contains("clean"), "{status}");
}

#[test]
fn checkout_by_id_prefix_detaches_and_snap_advises_branching() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    let first = ok(&run(&["snap", dbs, "-m", "baseline", "--json"]));
    let id = string_field(&first, "id");
    mutate_notes(&db);
    ok(&run(&["snap", dbs, "-m", "second"]));

    let out = ok(&run(&["checkout", dbs, &id[..8]]));
    assert!(out.contains("detached"), "{out}");
    mutate_notes(&db);
    let refused = run(&["snap", dbs, "-m", "on detached"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("litegraft branch"),
        "{}",
        stderr(&refused)
    );
}

fn string_field(json: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    let start = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + pat.len();
    json[start..].chars().take_while(|c| *c != '"').collect()
}

// ---- diff / tables ----------------------------------------------------------------

#[test]
fn diff_attributes_changes_to_the_mutated_table() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    mutate_notes(&db);
    let out = ok(&run(&["diff", dbs]));
    assert!(
        out.contains("notes"),
        "diff must name the mutated table: {out}"
    );
    assert!(
        !out.contains("events"),
        "events pages did not change: {out}"
    );
    assert!(out.contains("changed"), "{out}");
}

#[test]
fn diff_between_two_snapshot_refs_without_touching_the_working_file() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    ok(&run(&["branch", dbs, "fix"]));
    ok(&run(&["checkout", dbs, "fix"]));
    mutate_notes(&db);
    ok(&run(&["snap", dbs, "-m", "notes work"]));
    let mtime = std::fs::metadata(&db).unwrap().modified().unwrap();
    let out = ok(&run(&["diff", dbs, "main", "fix"]));
    assert!(out.contains("notes"), "{out}");
    assert_eq!(
        std::fs::metadata(&db).unwrap().modified().unwrap(),
        mtime,
        "diff must not write the db"
    );
}

#[test]
fn diff_of_identical_refs_is_empty() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    let out = ok(&run(&["diff", dbs, "main", "@"]));
    assert!(out.contains("no page-level differences"), "{out}");
}

#[test]
fn diff_json_has_owner_rollup_and_per_page_list() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    mutate_notes(&db);
    let out = ok(&run(&["diff", dbs, "--json"]));
    for key in [
        "\"owners\":[",
        "\"pages\":[",
        "\"pgno\":",
        "\"owner\":\"notes\"",
    ] {
        assert!(out.contains(key), "missing {key} in {out}");
    }
}

#[test]
fn diff_reports_added_pages_when_the_file_grows() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    // Rebuild with a new table appended: file gains pages.
    let objs = [
        multi_leaf_table("events", 120),
        small_table("notes"),
        overflow_table("blobs", 2),
    ];
    let (bytes, _) = common::build_db(&objs, 0);
    std::fs::write(&db, bytes).unwrap();
    let out = ok(&run(&["diff", dbs]));
    assert!(
        out.contains("blobs"),
        "new table appears in the rollup: {out}"
    );
    assert!(out.contains("added"), "{out}");
}

#[test]
fn tables_breaks_down_ownership_for_file_and_snapshot() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    let snap_json = ok(&run(&["snap", dbs, "-m", "baseline", "--json"]));
    let id = string_field(&snap_json, "id");
    let live = ok(&run(&["tables", dbs]));
    for label in ["OBJECT", "events", "notes", "sqlite_schema"] {
        assert!(live.contains(label), "missing {label} in {live}");
    }
    let snap_view = ok(&run(&["tables", dbs, &id[..12]]));
    assert!(snap_view.contains("events"), "{snap_view}");
    let json = ok(&run(&["tables", dbs, "--json"]));
    assert!(json.contains("\"object\":\"events\""), "{json}");
    assert!(json.contains("\"bytes\":"), "{json}");
}

// ---- guard rails -------------------------------------------------------------------

#[test]
fn snap_refuses_when_wal_frames_are_pending() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    // A -wal with more than 32 bytes means uncheckpointed frames.
    let wal = format!("{dbs}-wal");
    std::fs::write(&wal, vec![0u8; 32 + 24 + 512]).unwrap();
    let refused = run(&["snap", dbs, "-m", "stale"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("wal_checkpoint"),
        "{}",
        stderr(&refused)
    );
    // Explicit override still works (documented escape hatch).
    ok(&run(&[
        "snap",
        dbs,
        "-m",
        "explicitly stale",
        "--allow-wal",
    ]));
}

#[test]
fn snap_refuses_on_a_hot_journal() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    std::fs::write(format!("{dbs}-journal"), b"rollback bytes").unwrap();
    let refused = run(&["snap", dbs]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("journal"), "{}", stderr(&refused));
}

#[test]
fn restore_removes_stale_sidecar_files() {
    let (_dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    for suffix in ["-wal", "-shm", "-journal"] {
        std::fs::write(format!("{dbs}{suffix}"), b"").unwrap();
    }
    ok(&run(&["checkout", dbs, "main"]));
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !Path::new(&format!("{dbs}{suffix}")).exists(),
            "{suffix} must be removed with the restored file"
        );
    }
}

// ---- verify / gc -------------------------------------------------------------------

#[test]
fn verify_passes_then_detects_a_corrupted_object() {
    let (dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    let out = ok(&run(&["verify", dbs]));
    assert!(out.contains("verify OK"), "{out}");

    // Flip one byte of one object.
    let objects = dir.join("app.db.litegraft/objects");
    let shard = std::fs::read_dir(&objects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let victim = std::fs::read_dir(&shard)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&victim).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&victim, bytes).unwrap();

    let failed = run(&["verify", dbs]);
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        stdout(&failed).contains("verify FAILED"),
        "{}",
        stdout(&failed)
    );
    assert!(
        stderr(&failed).contains("does not match"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn gc_removes_only_unreachable_objects() {
    let (dir, db) = playground();
    let dbs = db.to_str().unwrap();
    ok(&run(&["snap", dbs, "-m", "baseline"]));
    // Plant an orphan object that no manifest references.
    let orphan_dir = dir.join("app.db.litegraft/objects/zz");
    std::fs::create_dir_all(&orphan_dir).unwrap();
    std::fs::write(orphan_dir.join("z".repeat(62)), b"orphan").unwrap();

    let out = ok(&run(&["gc", dbs]));
    assert!(out.contains("removed 1 unreachable object"), "{out}");
    assert!(!orphan_dir.join("z".repeat(62)).exists());
    // Everything referenced must survive: verify still passes, and a second
    // gc (JSON view) finds nothing left to remove.
    let verify = ok(&run(&["verify", dbs]));
    assert!(verify.contains("verify OK"), "{verify}");
    let json = ok(&run(&["gc", dbs, "--json"]));
    assert!(json.contains("\"removed\":0"), "{json}");
    let kept: u32 = field(&json, "kept").parse().unwrap();
    assert!(kept > 0, "{json}");
}
