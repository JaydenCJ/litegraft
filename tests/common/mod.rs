//! Shared test support: a from-scratch SQLite database *writer*.
//!
//! The integration tests need real SQLite files with known page ownership —
//! including multi-level b-trees, overflow chains, indexes and freelist
//! pages — without depending on a SQLite binary. This module builds such
//! files byte by byte from the file-format specification, and records
//! exactly which page numbers each object landed on so tests can assert the
//! walker's attribution against ground truth.
//!
//! The committed fixture in `tests/fixtures/` was produced by real SQLite
//! and cross-checks this builder against the reference implementation.

#![allow(dead_code)] // each integration test binary uses a subset

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use litegraft::record::write_varint;

pub const PAGE_SIZE: usize = 512;

// ---- temp dirs ---------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Self-cleaning temp dir (std-only, unique per test).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "litegraft-test-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---- record encoding ---------------------------------------------------------

/// Test-side value for record encoding.
#[derive(Clone)]
pub enum V {
    Null,
    I(i64),
    T(String),
    B(Vec<u8>),
}

pub fn t(s: &str) -> V {
    V::T(s.to_string())
}

fn int_serial(v: i64) -> (u64, Vec<u8>) {
    if v == 0 {
        return (8, vec![]);
    }
    if v == 1 {
        return (9, vec![]);
    }
    if (-128..=127).contains(&v) {
        return (1, vec![v as u8]);
    }
    if (-32768..=32767).contains(&v) {
        return (2, (v as i16).to_be_bytes().to_vec());
    }
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        return (4, (v as i32).to_be_bytes().to_vec());
    }
    (6, v.to_be_bytes().to_vec())
}

/// Encode a record (header of serial types + body) per the file format.
pub fn encode_record(values: &[V]) -> Vec<u8> {
    let mut serials = Vec::new();
    let mut body = Vec::new();
    for v in values {
        match v {
            V::Null => serials.push(write_varint(0)),
            V::I(i) => {
                let (s, bytes) = int_serial(*i);
                serials.push(write_varint(s));
                body.extend_from_slice(&bytes);
            }
            V::T(s) => {
                serials.push(write_varint(13 + 2 * s.len() as u64));
                body.extend_from_slice(s.as_bytes());
            }
            V::B(b) => {
                serials.push(write_varint(12 + 2 * b.len() as u64));
                body.extend_from_slice(b);
            }
        }
    }
    let serials_len: usize = serials.iter().map(Vec::len).sum();
    // Header length varint counts itself; one byte is always enough here.
    let header_len = serials_len + 1;
    assert!(
        header_len <= 127,
        "test records keep a 1-byte header length"
    );
    let mut out = Vec::with_capacity(header_len + body.len());
    out.push(header_len as u8);
    for s in serials {
        out.extend_from_slice(&s);
    }
    out.extend_from_slice(&body);
    out
}

// ---- database building -------------------------------------------------------

/// One schema object to place in the file.
pub struct Obj {
    pub otype: &'static str, // "table" | "index"
    pub name: String,
    pub tbl_name: String,
    pub sql: String,
    /// For tables: (rowid, record payload). For indexes: rowid is ignored
    /// and the payload is the index key record.
    pub rows: Vec<(i64, Vec<u8>)>,
}

impl Obj {
    pub fn table(name: &str, columns_sql: &str, rows: Vec<(i64, Vec<u8>)>) -> Obj {
        Obj {
            otype: "table",
            name: name.to_string(),
            tbl_name: name.to_string(),
            sql: format!("CREATE TABLE {name}({columns_sql})"),
            rows,
        }
    }
    pub fn index(name: &str, tbl: &str, expr: &str, rows: Vec<(i64, Vec<u8>)>) -> Obj {
        Obj {
            otype: "index",
            name: name.to_string(),
            tbl_name: tbl.to_string(),
            sql: format!("CREATE INDEX {name} ON {tbl}({expr})"),
            rows,
        }
    }
}

/// Where each object's pages ended up (ground truth for attribution tests).
#[derive(Debug, Default)]
pub struct Layout {
    pub roots: std::collections::BTreeMap<String, u32>,
    pub btree_pages: std::collections::BTreeMap<String, Vec<u32>>,
    pub overflow_pages: std::collections::BTreeMap<String, Vec<u32>>,
    pub freelist_pages: Vec<u32>,
    pub page_count: u32,
}

const USABLE: usize = PAGE_SIZE; // reserved bytes = 0 in built files

fn max_local_table() -> usize {
    USABLE - 35
}

fn min_local() -> usize {
    (USABLE - 12) * 32 / 255 - 23
}

fn local_len(payload: usize, x: usize) -> usize {
    if payload <= x {
        return payload;
    }
    let m = min_local();
    let k = m + (payload - m) % (USABLE - 4);
    if k <= x {
        k
    } else {
        m
    }
}

/// A not-yet-numbered page under construction.
enum Plan {
    TableLeaf {
        cells: Vec<(i64, Vec<u8>)>,
    },
    IndexLeaf {
        cells: Vec<Vec<u8>>,
    },
    TableInterior {
        children: Vec<(usize, i64)>,
        rightmost: usize,
    }, // local leaf indices
    Overflow {
        data: Vec<u8>,
        next: Option<usize>,
    }, // local indices
}

/// Serialized table-leaf cell for a payload, splitting overflow chunks.
fn table_leaf_cell(rowid: i64, payload: &[u8], first_overflow: Option<u32>) -> Vec<u8> {
    let local = local_len(payload.len(), max_local_table());
    let mut cell = write_varint(payload.len() as u64);
    cell.extend_from_slice(&write_varint(rowid as u64));
    cell.extend_from_slice(&payload[..local]);
    if local < payload.len() {
        cell.extend_from_slice(
            &first_overflow
                .expect("spilled cell needs an overflow page")
                .to_be_bytes(),
        );
    }
    cell
}

fn index_leaf_cell(payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() <= (USABLE - 12) * 64 / 255 - 23,
        "test index payloads stay local"
    );
    let mut cell = write_varint(payload.len() as u64);
    cell.extend_from_slice(payload);
    cell
}

/// Size a table-leaf cell will occupy on the page.
fn table_leaf_cell_len(rowid: i64, payload_len: usize) -> usize {
    let local = local_len(payload_len, max_local_table());
    let spill = if local < payload_len { 4 } else { 0 };
    write_varint(payload_len as u64).len() + write_varint(rowid as u64).len() + local + spill
}

/// Build one object's pages. Returns (plans, root_local_idx).
/// Local page order: leaves, then overflow pages, then interior root (if any).
fn plan_object(obj: &Obj) -> (Vec<Plan>, usize) {
    let header = |leaf: bool| if leaf { 8 } else { 12 };
    let mut plans: Vec<Plan> = Vec::new();

    if obj.otype == "index" {
        // Tests keep indexes single-leaf; assert it fits.
        let mut used = header(true);
        let mut cells = Vec::new();
        for (_, payload) in &obj.rows {
            let cell = index_leaf_cell(payload);
            used += cell.len() + 2;
            assert!(used <= USABLE, "test index overflows one page; shrink it");
            cells.push(payload.clone());
        }
        plans.push(Plan::IndexLeaf { cells });
        return (plans, 0);
    }

    // Distribute table rows over leaves.
    let mut leaves: Vec<Vec<(i64, Vec<u8>)>> = vec![Vec::new()];
    let mut free = USABLE - header(true);
    for (rowid, payload) in &obj.rows {
        let need = table_leaf_cell_len(*rowid, payload.len()) + 2;
        if need > free && !leaves.last().unwrap().is_empty() {
            leaves.push(Vec::new());
            free = USABLE - header(true);
        }
        assert!(
            need <= USABLE - header(true),
            "single cell larger than a page"
        );
        leaves.last_mut().unwrap().push((*rowid, payload.clone()));
        free -= need;
    }
    let nleaves = leaves.len();
    let mut max_rowids = Vec::with_capacity(nleaves);
    for leaf in &leaves {
        max_rowids.push(leaf.iter().map(|(r, _)| *r).max().unwrap_or(0));
        plans.push(Plan::TableLeaf {
            cells: leaf.clone(),
        });
    }
    // Overflow chains: planned after leaves; leaf serialization later needs
    // their local indices, which we can compute deterministically here.
    let mut overflow_planned = 0usize;
    for leaf in &leaves {
        for (_, payload) in leaf {
            let local = local_len(payload.len(), max_local_table());
            if local < payload.len() {
                let mut rest = &payload[local..];
                let mut chunk_indices = Vec::new();
                while !rest.is_empty() {
                    let take = rest.len().min(USABLE - 4);
                    chunk_indices.push(nleaves + overflow_planned);
                    overflow_planned += 1;
                    plans.push(Plan::Overflow {
                        data: rest[..take].to_vec(),
                        next: None,
                    });
                    rest = &rest[take..];
                }
                // Link the chain.
                for w in chunk_indices.windows(2) {
                    if let Plan::Overflow { next, .. } = &mut plans[w[0]] {
                        *next = Some(w[1]);
                    }
                }
            }
        }
    }
    if nleaves == 1 {
        (plans, 0)
    } else {
        let children: Vec<(usize, i64)> = (0..nleaves - 1).map(|i| (i, max_rowids[i])).collect();
        let root = plans.len();
        plans.push(Plan::TableInterior {
            children,
            rightmost: nleaves - 1,
        });
        (plans, root)
    }
}

/// Pack cells into a serialized b-tree page.
fn serialize_btree_page(
    pgno: u32,
    ptype: u8,
    rightmost: Option<u32>,
    cells: &[Vec<u8>],
) -> Vec<u8> {
    let hoff = if pgno == 1 { 100 } else { 0 };
    let header_len = if rightmost.is_some() { 12 } else { 8 };
    let mut page = vec![0u8; PAGE_SIZE];
    page[hoff] = ptype;
    page[hoff + 3..hoff + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    if let Some(r) = rightmost {
        page[hoff + 8..hoff + 12].copy_from_slice(&r.to_be_bytes());
    }
    let mut content_end = PAGE_SIZE;
    for (i, cell) in cells.iter().enumerate() {
        content_end -= cell.len();
        page[content_end..content_end + cell.len()].copy_from_slice(cell);
        let po = hoff + header_len + 2 * i;
        page[po..po + 2].copy_from_slice(&(content_end as u16).to_be_bytes());
    }
    let ptr_end = hoff + header_len + 2 * cells.len();
    assert!(ptr_end <= content_end, "page {pgno} overpacked");
    page[hoff + 5..hoff + 7].copy_from_slice(&(content_end as u16).to_be_bytes());
    page
}

/// Build a complete database file, returning its bytes and the layout.
pub fn build_db(objects: &[Obj], freelist_leaves: usize) -> (Vec<u8>, Layout) {
    let mut layout = Layout::default();

    // Plan every object and assign global page numbers. Page 1 = schema.
    struct Placed<'a> {
        obj: &'a Obj,
        plans: Vec<Plan>,
        base: u32, // global pgno of local index 0
        root: u32,
    }
    let mut placed: Vec<Placed> = Vec::new();
    let mut next_page = 2u32;
    for obj in objects {
        let (plans, root_idx) = plan_object(obj);
        let base = next_page;
        next_page += plans.len() as u32;
        placed.push(Placed {
            obj,
            plans,
            base,
            root: base + root_idx as u32,
        });
    }
    // Freelist: one trunk + N leaves at the end of the file.
    let (freelist_head, freelist_count) = if freelist_leaves > 0 {
        let trunk = next_page;
        next_page += 1 + freelist_leaves as u32;
        (trunk, 1 + freelist_leaves as u32)
    } else {
        (0, 0)
    };
    let page_count = next_page - 1;

    // Serialize object pages.
    let mut pages: Vec<Vec<u8>> = vec![Vec::new(); page_count as usize]; // index 0 = page 1
    for p in &placed {
        let global = |local: usize| p.base + local as u32;
        // Overflow chain heads per leaf cell, in row order.
        let mut overflow_heads: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
        for (i, plan) in p.plans.iter().enumerate() {
            if let Plan::Overflow { data: _, next: _ } = plan {
                // Chain heads are the overflow plans not pointed to by another.
                let is_head = !p
                    .plans
                    .iter()
                    .any(|q| matches!(q, Plan::Overflow { next: Some(nx), .. } if *nx == i));
                if is_head {
                    overflow_heads.push_back(global(i));
                }
            }
        }
        for (i, plan) in p.plans.iter().enumerate() {
            let pgno = global(i);
            let bytes = match plan {
                Plan::TableLeaf { cells } => {
                    let mut encoded = Vec::new();
                    for (rowid, payload) in cells {
                        let spills = local_len(payload.len(), max_local_table()) < payload.len();
                        let head = if spills {
                            Some(overflow_heads.pop_front().expect("chain head"))
                        } else {
                            None
                        };
                        encoded.push(table_leaf_cell(*rowid, payload, head));
                    }
                    serialize_btree_page(pgno, 13, None, &encoded)
                }
                Plan::IndexLeaf { cells } => {
                    let encoded: Vec<Vec<u8>> = cells.iter().map(|c| index_leaf_cell(c)).collect();
                    serialize_btree_page(pgno, 10, None, &encoded)
                }
                Plan::TableInterior {
                    children,
                    rightmost,
                } => {
                    let encoded: Vec<Vec<u8>> = children
                        .iter()
                        .map(|(child, maxrow)| {
                            let mut cell = global(*child).to_be_bytes().to_vec();
                            cell.extend_from_slice(&write_varint(*maxrow as u64));
                            cell
                        })
                        .collect();
                    serialize_btree_page(pgno, 5, Some(global(*rightmost)), &encoded)
                }
                Plan::Overflow { data, next } => {
                    let mut page = vec![0u8; PAGE_SIZE];
                    let next_pgno = next.map(&global).unwrap_or(0);
                    page[0..4].copy_from_slice(&next_pgno.to_be_bytes());
                    page[4..4 + data.len()].copy_from_slice(data);
                    page
                }
            };
            pages[pgno as usize - 1] = bytes;
            match plan {
                Plan::Overflow { .. } => layout
                    .overflow_pages
                    .entry(p.obj.name.clone())
                    .or_default()
                    .push(pgno),
                _ => layout
                    .btree_pages
                    .entry(p.obj.name.clone())
                    .or_default()
                    .push(pgno),
            }
        }
        layout.roots.insert(p.obj.name.clone(), p.root);
    }

    // Schema page (page 1): one cell per object, rowids 1..N.
    let schema_cells: Vec<Vec<u8>> = placed
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let record = encode_record(&[
                t(p.obj.otype),
                t(&p.obj.name),
                t(&p.obj.tbl_name),
                V::I(p.root as i64),
                t(&p.obj.sql),
            ]);
            table_leaf_cell((i + 1) as i64, &record, None)
        })
        .collect();
    let mut page1 = serialize_btree_page(1, 13, None, &schema_cells);

    // Freelist pages.
    if freelist_head != 0 {
        let mut trunk = vec![0u8; PAGE_SIZE];
        trunk[0..4].copy_from_slice(&0u32.to_be_bytes());
        trunk[4..8].copy_from_slice(&(freelist_leaves as u32).to_be_bytes());
        layout.freelist_pages.push(freelist_head);
        for i in 0..freelist_leaves {
            let leaf = freelist_head + 1 + i as u32;
            trunk[8 + 4 * i..12 + 4 * i].copy_from_slice(&leaf.to_be_bytes());
            let mut junk = vec![0u8; PAGE_SIZE];
            junk[0] = 0xfe; // recognizable non-btree garbage
            junk[1] = i as u8;
            pages[leaf as usize - 1] = junk;
            layout.freelist_pages.push(leaf);
        }
        pages[freelist_head as usize - 1] = trunk;
    }

    // File header at the front of page 1.
    page1[0..16].copy_from_slice(b"SQLite format 3\0");
    page1[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_be_bytes());
    page1[18] = 1; // file format write version (legacy)
    page1[19] = 1; // file format read version (legacy)
    page1[20] = 0; // reserved bytes per page
    page1[21] = 64;
    page1[22] = 32;
    page1[23] = 32;
    page1[24..28].copy_from_slice(&1u32.to_be_bytes()); // change counter
    page1[28..32].copy_from_slice(&page_count.to_be_bytes());
    page1[32..36].copy_from_slice(&freelist_head.to_be_bytes());
    page1[36..40].copy_from_slice(&freelist_count.to_be_bytes());
    page1[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
    page1[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
    page1[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
    page1[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
    page1[96..100].copy_from_slice(&3_045_001u32.to_be_bytes()); // sqlite version
    pages[0] = page1;

    layout.page_count = page_count;
    let mut file = Vec::with_capacity(page_count as usize * PAGE_SIZE);
    for page in pages {
        assert_eq!(page.len(), PAGE_SIZE, "unassigned page in layout");
        file.extend_from_slice(&page);
    }
    (file, layout)
}

/// Write a built database to `<dir>/<name>` and return its path.
pub fn write_db(
    dir: &TempDir,
    name: &str,
    objects: &[Obj],
    freelist_leaves: usize,
) -> (PathBuf, Layout) {
    let (bytes, layout) = build_db(objects, freelist_leaves);
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    (path, layout)
}

// ---- ready-made specimens ------------------------------------------------------

/// A small single-leaf table: `notes(id, body)` with 3 rows.
pub fn small_table(name: &str) -> Obj {
    let rows = (1..=3)
        .map(|i| (i, encode_record(&[V::I(i), t(&format!("note body {i}"))])))
        .collect();
    Obj::table(name, "id INTEGER PRIMARY KEY, body TEXT", rows)
}

/// A table big enough to need an interior page (multi-leaf b-tree).
pub fn multi_leaf_table(name: &str, rows: usize) -> Obj {
    let rows = (1..=rows as i64)
        .map(|i| {
            (
                i,
                encode_record(&[V::I(i), t(&format!("row {i} padding padding padding"))]),
            )
        })
        .collect();
    Obj::table(name, "id INTEGER PRIMARY KEY, label TEXT", rows)
}

/// A table with one row whose blob spills into an overflow chain of at least
/// `chain_len` pages.
pub fn overflow_table(name: &str, chain_len: usize) -> Obj {
    let blob_len = max_local_table() + (USABLE - 4) * (chain_len - 1) + 40;
    let blob: Vec<u8> = (0..blob_len).map(|i| (i % 251) as u8).collect();
    let rows = vec![(1, encode_record(&[V::I(1), V::B(blob)]))];
    Obj::table(name, "id INTEGER PRIMARY KEY, data BLOB", rows)
}
