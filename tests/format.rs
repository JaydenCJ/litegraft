//! File-format integration tests: the b-tree walker, schema reader and page
//! attributor against (a) databases built byte-by-byte by the in-repo
//! builder and (b) a committed fixture produced by real SQLite
//! (`tests/fixtures/inventory.db`), which cross-checks the builder and the
//! parser against the reference implementation.

mod common;

use std::path::PathBuf;

use common::{multi_leaf_table, overflow_table, small_table, write_db, TempDir};
use litegraft::btree::{attribute_pages, freelist_pages, read_schema, walk_btree, PageKind};
use litegraft::dbfile::{DbFile, PageSource};
use litegraft::snapshot::{snap, SnapshotSource};
use litegraft::store::{default_store_dir, Store};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/inventory.db")
}

// ---- built databases ---------------------------------------------------------

#[test]
fn built_db_opens_with_expected_header() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[small_table("notes")], 0);
    let db = DbFile::open(&path).unwrap();
    assert_eq!(db.page_size(), 512);
    assert_eq!(db.page_count(), layout.page_count);
    assert_eq!(db.header().freelist_head, 0);
}

#[test]
fn schema_reader_finds_tables_and_roots() {
    let dir = TempDir::new();
    let (path, layout) = write_db(
        &dir,
        "a.db",
        &[small_table("notes"), small_table("tags")],
        0,
    );
    let mut db = DbFile::open(&path).unwrap();
    let schema = read_schema(&mut db).unwrap();
    assert_eq!(schema.len(), 2);
    assert_eq!(schema[0].name, "notes");
    assert_eq!(schema[0].rootpage, layout.roots["notes"]);
    assert_eq!(schema[1].name, "tags");
    assert_eq!(schema[1].rootpage, layout.roots["tags"]);
    assert!(schema.iter().all(|o| o.otype == "table"));
}

#[test]
fn single_leaf_table_walk_returns_only_its_root() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[small_table("notes")], 0);
    let mut db = DbFile::open(&path).unwrap();
    let pages = walk_btree(&mut db, layout.roots["notes"]).unwrap();
    assert_eq!(pages.btree, vec![layout.roots["notes"]]);
    assert!(pages.overflow.is_empty());
}

#[test]
fn multi_leaf_table_walk_collects_interior_and_all_leaves() {
    let dir = TempDir::new();
    let obj = multi_leaf_table("events", 60);
    let (path, layout) = write_db(&dir, "a.db", &[obj], 0);
    let mut db = DbFile::open(&path).unwrap();
    let expected = &layout.btree_pages["events"];
    assert!(
        expected.len() >= 4,
        "test premise: b-tree spans multiple pages, got {expected:?}"
    );
    let pages = walk_btree(&mut db, layout.roots["events"]).unwrap();
    assert_eq!(
        &pages.btree, expected,
        "walker must find the interior root and every leaf"
    );
}

#[test]
fn overflow_chain_pages_are_collected_in_order() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[overflow_table("blobs", 3)], 0);
    let mut db = DbFile::open(&path).unwrap();
    let pages = walk_btree(&mut db, layout.roots["blobs"]).unwrap();
    assert_eq!(pages.overflow, layout.overflow_pages["blobs"]);
    assert_eq!(
        pages.overflow.len(),
        3,
        "chain length must match the builder's plan"
    );
}

#[test]
fn freelist_walk_matches_builder_layout() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[small_table("notes")], 4);
    let mut db = DbFile::open(&path).unwrap();
    assert_eq!(db.header().freelist_count, 5, "trunk + 4 leaves");
    let pages = freelist_pages(&mut db).unwrap();
    assert_eq!(pages, layout.freelist_pages);
}

#[test]
fn attribution_covers_every_page_with_no_unattributed() {
    let dir = TempDir::new();
    let objs = vec![
        small_table("notes"),
        multi_leaf_table("events", 40),
        overflow_table("blobs", 2),
    ];
    let (path, layout) = write_db(&dir, "a.db", &objs, 3);
    let mut db = DbFile::open(&path).unwrap();
    let map = attribute_pages(&mut db).unwrap();
    assert_eq!(map.len() as u32, layout.page_count);
    for (pgno, owner) in &map {
        assert_ne!(
            owner.kind,
            PageKind::Unattributed,
            "page {pgno} left unattributed"
        );
    }
    assert_eq!(map[&1].owner.as_deref(), Some("sqlite_schema"));
    for &p in &layout.btree_pages["events"] {
        assert_eq!(map[&p].owner.as_deref(), Some("events"), "page {p}");
        assert_eq!(map[&p].kind, PageKind::BTree);
    }
    for &p in &layout.overflow_pages["blobs"] {
        assert_eq!(map[&p].owner.as_deref(), Some("blobs"), "page {p}");
        assert_eq!(map[&p].kind, PageKind::Overflow);
    }
    for &p in &layout.freelist_pages {
        assert_eq!(map[&p].kind, PageKind::Freelist, "page {p}");
    }
}

#[test]
fn attribution_of_a_snapshot_equals_attribution_of_the_file() {
    // The walker must behave identically over the object store, because
    // `diff` attributes historical states without restoring them.
    let dir = TempDir::new();
    let objs = vec![multi_leaf_table("events", 40), overflow_table("blobs", 2)];
    let (path, _layout) = write_db(&dir, "a.db", &objs, 2);
    let store = Store::init(&default_store_dir(&path)).unwrap();
    let outcome = snap(&store, &path, "baseline", false).unwrap();

    let mut file_src = DbFile::open(&path).unwrap();
    let file_map = attribute_pages(&mut file_src).unwrap();
    let mut snap_src = SnapshotSource::open(&store, &outcome.id).unwrap();
    let snap_map = attribute_pages(&mut snap_src).unwrap();
    assert_eq!(file_map, snap_map);
}

#[test]
fn walker_rejects_out_of_range_child_pointer() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[multi_leaf_table("events", 60)], 0);
    // Corrupt the interior root's right-most pointer to point past the file.
    let root = layout.roots["events"];
    let mut bytes = std::fs::read(&path).unwrap();
    let off = (root as usize - 1) * 512 + 8;
    bytes[off..off + 4].copy_from_slice(&9999u32.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let mut db = DbFile::open(&path).unwrap();
    let err = walk_btree(&mut db, root).unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn walker_rejects_pointer_cycles_instead_of_looping() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[multi_leaf_table("events", 60)], 0);
    // Point the interior root's right-most pointer back at the root itself.
    let root = layout.roots["events"];
    let mut bytes = std::fs::read(&path).unwrap();
    let off = (root as usize - 1) * 512 + 8;
    bytes[off..off + 4].copy_from_slice(&root.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let mut db = DbFile::open(&path).unwrap();
    let err = walk_btree(&mut db, root).unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
}

#[test]
fn schema_reader_rejects_corrupt_cell_pointers_without_panicking() {
    // A schema page claiming far more cells than fit must produce an error,
    // not an out-of-bounds panic: `tables`/`diff` run this on user files.
    let dir = TempDir::new();
    let (path, _layout) = write_db(&dir, "a.db", &[small_table("notes")], 0);
    let mut bytes = std::fs::read(&path).unwrap();
    // Page 1's b-tree header starts after the 100-byte file header; bytes
    // 3..5 of it are the cell count.
    bytes[103..105].copy_from_slice(&u16::MAX.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let mut db = DbFile::open(&path).unwrap();
    let err = read_schema(&mut db).unwrap_err();
    assert!(err.to_string().contains("truncated"), "{err}");
}

#[test]
fn walker_rejects_unknown_page_type() {
    let dir = TempDir::new();
    let (path, layout) = write_db(&dir, "a.db", &[small_table("notes")], 0);
    let root = layout.roots["notes"];
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[(root as usize - 1) * 512] = 42;
    std::fs::write(&path, &bytes).unwrap();
    let mut db = DbFile::open(&path).unwrap();
    let err = walk_btree(&mut db, root).unwrap_err();
    assert!(err.to_string().contains("page type"), "{err}");
}

// ---- the committed real-SQLite fixture -----------------------------------------

#[test]
fn fixture_header_matches_known_ground_truth() {
    // Values recorded from `PRAGMA page_size/page_count/freelist_count` when
    // the fixture was created with real SQLite.
    let db = DbFile::open(&fixture_path()).unwrap();
    assert_eq!(db.page_size(), 512);
    assert_eq!(db.page_count(), 39);
    assert_eq!(db.header().freelist_count, 15);
    assert_eq!(db.header().text_encoding, 1);
}

#[test]
fn fixture_schema_matches_sqlite_schema_query() {
    // Ground truth: SELECT type,name,rootpage FROM sqlite_schema ORDER BY name.
    let mut db = DbFile::open(&fixture_path()).unwrap();
    let schema = read_schema(&mut db).unwrap();
    let got: Vec<(&str, &str, u32)> = schema
        .iter()
        .map(|o| (o.otype.as_str(), o.name.as_str(), o.rootpage))
        .collect();
    assert_eq!(
        got,
        vec![
            ("index", "idx_parts_sku", 17),
            ("table", "manuals", 11),
            ("table", "parts", 2)
        ]
    );
}

#[test]
fn fixture_blob_table_has_an_overflow_chain() {
    // manuals holds one 3000-byte blob; with 512-byte pages that must spill.
    let mut db = DbFile::open(&fixture_path()).unwrap();
    let pages = walk_btree(&mut db, 11).unwrap();
    assert!(
        pages.overflow.len() >= 5,
        "3000-byte blob must span >=5 overflow pages, got {:?}",
        pages.overflow
    );
}

#[test]
fn fixture_freelist_count_matches_walked_freelist() {
    let mut db = DbFile::open(&fixture_path()).unwrap();
    let pages = freelist_pages(&mut db).unwrap();
    assert_eq!(pages.len() as u32, db.header().freelist_count);
}

#[test]
fn fixture_attribution_is_complete_and_disjoint() {
    // Every one of the 39 pages must be claimed by exactly one owner, and
    // real SQLite output must leave nothing unattributed.
    let mut db = DbFile::open(&fixture_path()).unwrap();
    let map = attribute_pages(&mut db).unwrap();
    assert_eq!(map.len(), 39);
    let mut by_kind: std::collections::BTreeMap<&str, u32> = Default::default();
    for owner in map.values() {
        assert_ne!(owner.kind, PageKind::Unattributed);
        *by_kind.entry(owner.kind.label()).or_default() += 1;
    }
    assert_eq!(by_kind["freelist"], 15);
    // parts (multi-page) + manuals + index + schema account for the rest.
    assert_eq!(by_kind["btree"] + by_kind["overflow"], 39 - 15);
    let index_pages = map
        .values()
        .filter(|o| o.owner.as_deref() == Some("idx_parts_sku"))
        .count();
    assert!(index_pages >= 1, "index b-tree must be attributed");
}

#[test]
fn fixture_multi_page_table_walk_is_consistent_with_roots() {
    let mut db = DbFile::open(&fixture_path()).unwrap();
    let parts = walk_btree(&mut db, 2).unwrap();
    assert!(
        parts.btree.len() > 1,
        "90 rows across 512-byte pages needs several pages"
    );
    assert!(parts.btree.contains(&2), "walk includes the root");
    assert!(
        parts.overflow.is_empty(),
        "parts rows are small and fully local"
    );
}
