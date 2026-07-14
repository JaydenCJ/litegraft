//! B-tree walking and page attribution.
//!
//! A SQLite file is a forest of b-trees plus bookkeeping pages. This module
//! walks that forest directly — no SQL engine involved — to answer one
//! question: *which object does each page belong to?* That mapping is what
//! turns a raw page-level diff into "3 pages of `users` changed".
//!
//! It understands the four b-tree page types, overflow-page chains, the
//! freelist trunk/leaf chain, pointer-map pages (auto-vacuum) and the lock
//! byte page. `WITHOUT ROWID` tables store rows in index-type pages; the
//! walker handles both layouts uniformly because it dispatches on the page
//! type byte, not on the schema.

use std::collections::BTreeMap;
use std::io;

use crate::dbfile::{PageSource, HEADER_LEN};
use crate::record::{decode_record, read_varint, Value};

/// B-tree page types (first byte of the page header).
pub const INTERIOR_INDEX: u8 = 2;
pub const INTERIOR_TABLE: u8 = 5;
pub const LEAF_INDEX: u8 = 10;
pub const LEAF_TABLE: u8 = 13;

fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// What role a page plays in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageKind {
    /// Part of a b-tree (interior or leaf) owned by a named object.
    BTree,
    /// Overflow chain page holding spilled payload of a named object.
    Overflow,
    /// Freelist trunk or leaf page.
    Freelist,
    /// Pointer-map page (auto-vacuum databases).
    PtrMap,
    /// The page spanning the 1 GiB lock byte (never stores data).
    LockByte,
    /// Not reachable from the schema, the freelist or the header.
    Unattributed,
}

impl PageKind {
    pub fn label(&self) -> &'static str {
        match self {
            PageKind::BTree => "btree",
            PageKind::Overflow => "overflow",
            PageKind::Freelist => "freelist",
            PageKind::PtrMap => "ptrmap",
            PageKind::LockByte => "lockbyte",
            PageKind::Unattributed => "unattributed",
        }
    }
}

/// Ownership record for one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOwner {
    pub kind: PageKind,
    /// Object name for `BTree`/`Overflow` pages (e.g. `users`,
    /// `sqlite_schema`, `idx_users_email`); `None` for bookkeeping pages.
    pub owner: Option<String>,
}

/// One row of `sqlite_schema` that owns a b-tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    /// `table` or `index`.
    pub otype: String,
    pub name: String,
    /// The table an index belongs to (== `name` for tables).
    pub tbl_name: String,
    pub rootpage: u32,
}

/// Pages collected by walking one b-tree.
#[derive(Debug, Default)]
pub struct BtreePages {
    pub btree: Vec<u32>,
    pub overflow: Vec<u32>,
}

/// Offset of the b-tree page header within a page (page 1 embeds the file
/// header first).
fn btree_header_offset(pgno: u32) -> usize {
    if pgno == 1 {
        HEADER_LEN
    } else {
        0
    }
}

struct CellGeometry {
    usable: u32,
}

impl CellGeometry {
    /// Max local payload for a table-leaf cell before it spills.
    fn max_local_table(&self) -> u64 {
        self.usable as u64 - 35
    }
    /// Max local payload for index cells (leaf and interior).
    fn max_local_index(&self) -> u64 {
        ((self.usable as u64 - 12) * 64 / 255) - 23
    }
    fn min_local(&self) -> u64 {
        ((self.usable as u64 - 12) * 32 / 255) - 23
    }

    /// Local byte count actually stored in the cell for a payload of
    /// `payload_len` with the given spill threshold `x`.
    fn local_len(&self, payload_len: u64, x: u64) -> u64 {
        if payload_len <= x {
            return payload_len;
        }
        let m = self.min_local();
        let k = m + (payload_len - m) % (self.usable as u64 - 4);
        if k <= x {
            k
        } else {
            m
        }
    }
}

/// Walk the b-tree rooted at `root`, collecting every b-tree page and every
/// overflow page it references. `visited` guards against pointer cycles in
/// corrupt files (an error, never an infinite loop).
pub fn walk_btree(src: &mut dyn PageSource, root: u32) -> io::Result<BtreePages> {
    let mut out = BtreePages::default();
    let mut visited = vec![false; src.page_count() as usize + 1];
    let mut stack = vec![root];
    let geo = CellGeometry {
        usable: src.header().usable_size(),
    };

    while let Some(pgno) = stack.pop() {
        if pgno == 0 || pgno > src.page_count() {
            return Err(corrupt(format!(
                "b-tree pointer to page {pgno} out of range"
            )));
        }
        if visited[pgno as usize] {
            return Err(corrupt(format!("b-tree pointer cycle at page {pgno}")));
        }
        visited[pgno as usize] = true;
        out.btree.push(pgno);

        let page = src.page(pgno)?;
        let hoff = btree_header_offset(pgno);
        let ptype = page[hoff];
        let ncells = u16::from_be_bytes([page[hoff + 3], page[hoff + 4]]) as usize;
        let header_len = match ptype {
            INTERIOR_INDEX | INTERIOR_TABLE => 12,
            LEAF_INDEX | LEAF_TABLE => 8,
            other => {
                return Err(corrupt(format!(
                    "page {pgno}: unknown b-tree page type {other}"
                )))
            }
        };

        if matches!(ptype, INTERIOR_INDEX | INTERIOR_TABLE) {
            let right = u32::from_be_bytes([
                page[hoff + 8],
                page[hoff + 9],
                page[hoff + 10],
                page[hoff + 11],
            ]);
            stack.push(right);
        }

        let ptr_array = hoff + header_len;
        for i in 0..ncells {
            let po = ptr_array + 2 * i;
            if po + 2 > page.len() {
                return Err(corrupt(format!(
                    "page {pgno}: cell pointer array truncated"
                )));
            }
            let cell_off = u16::from_be_bytes([page[po], page[po + 1]]) as usize;
            if cell_off >= page.len() {
                return Err(corrupt(format!(
                    "page {pgno}: cell {i} offset {cell_off} out of page"
                )));
            }
            let cell = &page[cell_off..];
            match ptype {
                INTERIOR_TABLE => {
                    // 4-byte left child + varint rowid; no payload.
                    if cell.len() < 4 {
                        return Err(corrupt(format!("page {pgno}: interior cell truncated")));
                    }
                    stack.push(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]));
                }
                LEAF_TABLE => {
                    let (payload_len, n) = read_varint(cell).map_err(|e| corrupt(e.0))?;
                    let (_rowid, m) = read_varint(&cell[n..]).map_err(|e| corrupt(e.0))?;
                    let local = geo.local_len(payload_len, geo.max_local_table()) as usize;
                    collect_overflow(
                        src,
                        cell,
                        n + m,
                        payload_len,
                        local,
                        &mut out.overflow,
                        &mut visited,
                    )?;
                }
                LEAF_INDEX | INTERIOR_INDEX => {
                    let mut off = 0usize;
                    if ptype == INTERIOR_INDEX {
                        if cell.len() < 4 {
                            return Err(corrupt(format!("page {pgno}: interior cell truncated")));
                        }
                        stack.push(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]));
                        off = 4;
                    }
                    let (payload_len, n) = read_varint(&cell[off..]).map_err(|e| corrupt(e.0))?;
                    let local = geo.local_len(payload_len, geo.max_local_index()) as usize;
                    collect_overflow(
                        src,
                        cell,
                        off + n,
                        payload_len,
                        local,
                        &mut out.overflow,
                        &mut visited,
                    )?;
                }
                _ => unreachable!(),
            }
        }
    }
    out.btree.sort_unstable();
    out.overflow.sort_unstable();
    Ok(out)
}

/// Follow an overflow chain starting after `local` payload bytes at
/// `cell[start..]`, pushing every chain page into `pages`.
fn collect_overflow(
    src: &mut dyn PageSource,
    cell: &[u8],
    start: usize,
    payload_len: u64,
    local: usize,
    pages: &mut Vec<u32>,
    visited: &mut [bool],
) -> io::Result<()> {
    if payload_len as usize <= local {
        return Ok(()); // fully local, no chain
    }
    let p = start + local;
    if p + 4 > cell.len() {
        return Err(corrupt("cell overflow pointer truncated"));
    }
    let mut next = u32::from_be_bytes([cell[p], cell[p + 1], cell[p + 2], cell[p + 3]]);
    let mut remaining = payload_len as usize - local;
    let capacity = src.header().usable_size() as usize - 4;
    while next != 0 {
        if next > src.page_count() {
            return Err(corrupt(format!(
                "overflow pointer to page {next} out of range"
            )));
        }
        if visited[next as usize] {
            return Err(corrupt(format!("overflow chain cycle at page {next}")));
        }
        visited[next as usize] = true;
        pages.push(next);
        let page = src.page(next)?;
        next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        remaining = remaining.saturating_sub(capacity);
        if remaining == 0 {
            break;
        }
    }
    if remaining > 0 {
        return Err(corrupt("overflow chain shorter than payload length"));
    }
    Ok(())
}

/// Assemble the full payload of a table-leaf cell, following its overflow
/// chain if needed. Used to read `sqlite_schema` records.
fn read_cell_payload(
    src: &mut dyn PageSource,
    cell: &[u8],
    geo: &CellGeometry,
) -> io::Result<Vec<u8>> {
    let (payload_len, n) = read_varint(cell).map_err(|e| corrupt(e.0))?;
    let (_rowid, m) = read_varint(&cell[n..]).map_err(|e| corrupt(e.0))?;
    let local = geo.local_len(payload_len, geo.max_local_table()) as usize;
    let body = &cell[n + m..];
    if body.len() < local {
        return Err(corrupt("cell payload truncated"));
    }
    let mut payload = body[..local].to_vec();
    if local < payload_len as usize {
        if body.len() < local + 4 {
            return Err(corrupt("cell overflow pointer truncated"));
        }
        let mut next = u32::from_be_bytes([
            body[local],
            body[local + 1],
            body[local + 2],
            body[local + 3],
        ]);
        let capacity = src.header().usable_size() as usize - 4;
        let mut guard = 0u32;
        while next != 0 && payload.len() < payload_len as usize {
            guard += 1;
            if guard > src.page_count() {
                return Err(corrupt("overflow chain cycle while reading payload"));
            }
            let page = src.page(next)?;
            let want = (payload_len as usize - payload.len()).min(capacity);
            payload.extend_from_slice(&page[4..4 + want]);
            next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        }
        if payload.len() != payload_len as usize {
            return Err(corrupt("overflow chain shorter than payload length"));
        }
    }
    Ok(payload)
}

/// Read `sqlite_schema` (the table b-tree rooted at page 1) and return every
/// object that owns a b-tree (tables and indexes; views and triggers have no
/// root page).
pub fn read_schema(src: &mut dyn PageSource) -> io::Result<Vec<SchemaObject>> {
    if src.header().text_encoding != 1 {
        return Err(corrupt(format!(
            "unsupported text encoding {} (litegraft 0.1 reads UTF-8 databases)",
            src.header().text_encoding
        )));
    }
    let mut objects = Vec::new();
    let geo = CellGeometry {
        usable: src.header().usable_size(),
    };
    let mut stack = vec![1u32];
    let mut guard = 0u32;
    while let Some(pgno) = stack.pop() {
        guard += 1;
        if guard > src.page_count() {
            return Err(corrupt("schema b-tree cycle"));
        }
        let page = src.page(pgno)?;
        let hoff = btree_header_offset(pgno);
        let ptype = page[hoff];
        let ncells = u16::from_be_bytes([page[hoff + 3], page[hoff + 4]]) as usize;
        if ptype == INTERIOR_TABLE {
            let right = u32::from_be_bytes([
                page[hoff + 8],
                page[hoff + 9],
                page[hoff + 10],
                page[hoff + 11],
            ]);
            stack.push(right);
        } else if ptype != LEAF_TABLE {
            return Err(corrupt(format!(
                "schema page {pgno} has non-table type {ptype}"
            )));
        }
        let header_len = if ptype == INTERIOR_TABLE { 12 } else { 8 };
        for i in 0..ncells {
            let po = hoff + header_len + 2 * i;
            if po + 2 > page.len() {
                return Err(corrupt(format!(
                    "schema page {pgno}: cell pointer array truncated"
                )));
            }
            let cell_off = u16::from_be_bytes([page[po], page[po + 1]]) as usize;
            if cell_off >= page.len() {
                return Err(corrupt(format!(
                    "schema page {pgno}: cell {i} offset {cell_off} out of page"
                )));
            }
            let cell = &page[cell_off..];
            if ptype == INTERIOR_TABLE {
                if cell.len() < 4 {
                    return Err(corrupt(format!(
                        "schema page {pgno}: interior cell truncated"
                    )));
                }
                stack.push(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]));
                continue;
            }
            let payload = read_cell_payload(src, cell, &geo)?;
            let values = decode_record(&payload).map_err(|e| corrupt(e.0))?;
            // sqlite_schema: (type, name, tbl_name, rootpage, sql)
            if values.len() < 4 {
                return Err(corrupt("schema record with fewer than 4 columns"));
            }
            let otype = values[0].as_text().unwrap_or_default().to_string();
            let name = values[1].as_text().unwrap_or_default().to_string();
            let tbl_name = values[2].as_text().unwrap_or_default().to_string();
            let rootpage = match &values[3] {
                Value::Int(v) if *v > 0 => *v as u32,
                _ => 0,
            };
            if rootpage != 0 && (otype == "table" || otype == "index") {
                objects.push(SchemaObject {
                    otype,
                    name,
                    tbl_name,
                    rootpage,
                });
            }
        }
    }
    objects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(objects)
}

/// Walk the freelist trunk chain from the header, returning every freelist
/// page (trunks and leaves).
pub fn freelist_pages(src: &mut dyn PageSource) -> io::Result<Vec<u32>> {
    let mut pages = Vec::new();
    let mut trunk = src.header().freelist_head;
    let mut guard = 0u32;
    while trunk != 0 {
        guard += 1;
        if trunk > src.page_count() || guard > src.page_count() {
            return Err(corrupt(format!(
                "freelist trunk chain invalid at page {trunk}"
            )));
        }
        pages.push(trunk);
        let page = src.page(trunk)?;
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        let count = u32::from_be_bytes([page[4], page[5], page[6], page[7]]) as usize;
        let max_leaves = (src.header().usable_size() as usize - 8) / 4;
        if count > max_leaves {
            return Err(corrupt(format!(
                "freelist trunk {trunk} claims {count} leaves"
            )));
        }
        for i in 0..count {
            let off = 8 + 4 * i;
            let leaf = u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
            if leaf == 0 || leaf > src.page_count() {
                return Err(corrupt(format!("freelist leaf {leaf} out of range")));
            }
            pages.push(leaf);
        }
        trunk = next;
    }
    pages.sort_unstable();
    Ok(pages)
}

/// Pointer-map page numbers for an auto-vacuum database: page 2, then one
/// every `usable/5 + 1` pages.
pub fn ptrmap_pages(src: &dyn PageSource) -> Vec<u32> {
    if src.header().largest_root_btree == 0 {
        return Vec::new();
    }
    let entries_per_page = src.header().usable_size() / 5;
    let mut pages = Vec::new();
    let mut p = 2u32;
    while p <= src.page_count() {
        pages.push(p);
        p += entries_per_page + 1;
    }
    pages
}

/// The page containing the lock byte (offset 1 GiB), which SQLite never uses
/// for data. Only present in files larger than 1 GiB.
pub fn lock_byte_page(src: &dyn PageSource) -> Option<u32> {
    let pgno = 1_073_741_824 / src.page_size() + 1;
    if pgno <= src.page_count() {
        Some(pgno)
    } else {
        None
    }
}

/// Full page-ownership map for a database: page number -> owner.
pub fn attribute_pages(src: &mut dyn PageSource) -> io::Result<BTreeMap<u32, PageOwner>> {
    let mut map: BTreeMap<u32, PageOwner> = BTreeMap::new();
    for pgno in 1..=src.page_count() {
        map.insert(
            pgno,
            PageOwner {
                kind: PageKind::Unattributed,
                owner: None,
            },
        );
    }
    if let Some(p) = lock_byte_page(src) {
        map.insert(
            p,
            PageOwner {
                kind: PageKind::LockByte,
                owner: None,
            },
        );
    }
    for p in ptrmap_pages(src) {
        map.insert(
            p,
            PageOwner {
                kind: PageKind::PtrMap,
                owner: None,
            },
        );
    }
    for p in freelist_pages(src)? {
        map.insert(
            p,
            PageOwner {
                kind: PageKind::Freelist,
                owner: None,
            },
        );
    }

    let claim = |pages: &BtreePages, name: &str, map: &mut BTreeMap<u32, PageOwner>| {
        for &p in &pages.btree {
            map.insert(
                p,
                PageOwner {
                    kind: PageKind::BTree,
                    owner: Some(name.to_string()),
                },
            );
        }
        for &p in &pages.overflow {
            map.insert(
                p,
                PageOwner {
                    kind: PageKind::Overflow,
                    owner: Some(name.to_string()),
                },
            );
        }
    };

    // sqlite_schema itself (root page 1) first, then every schema object.
    let schema_pages = walk_btree(src, 1)?;
    claim(&schema_pages, "sqlite_schema", &mut map);
    for obj in read_schema(src)? {
        let pages = walk_btree(src, obj.rootpage)?;
        claim(&pages, &obj.name, &mut map);
    }
    Ok(map)
}
