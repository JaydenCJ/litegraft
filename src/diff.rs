//! Page-level diff between two database states, attributed to tables.
//!
//! The raw diff is a positional comparison of two page-hash lists — O(pages)
//! with no I/O beyond the manifests. Attribution then maps each differing
//! page number to its owner (table, index, freelist, …) using the b-tree
//! walker, preferring the *new* side's ownership map and falling back to the
//! old side for pages that only exist there.

use std::collections::BTreeMap;
use std::io;

use crate::btree::{attribute_pages, PageKind, PageOwner};
use crate::dbfile::PageSource;

/// How one page differs between two states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Same page number, different content.
    Changed,
    /// Page number exists only in the new state (file grew).
    Added,
    /// Page number exists only in the old state (file shrank).
    Removed,
}

impl Change {
    pub fn label(&self) -> &'static str {
        match self {
            Change::Changed => "changed",
            Change::Added => "added",
            Change::Removed => "removed",
        }
    }
}

/// One differing page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDelta {
    pub pgno: u32,
    pub change: Change,
}

/// Positional page diff between two hash lists.
pub fn diff_hashes(old: &[String], new: &[String]) -> Vec<PageDelta> {
    let mut deltas = Vec::new();
    let common = old.len().min(new.len());
    for i in 0..common {
        if old[i] != new[i] {
            deltas.push(PageDelta {
                pgno: (i + 1) as u32,
                change: Change::Changed,
            });
        }
    }
    for i in common..new.len() {
        deltas.push(PageDelta {
            pgno: (i + 1) as u32,
            change: Change::Added,
        });
    }
    for i in common..old.len() {
        deltas.push(PageDelta {
            pgno: (i + 1) as u32,
            change: Change::Removed,
        });
    }
    deltas.sort_by_key(|d| d.pgno);
    deltas
}

/// Per-owner rollup of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerDelta {
    /// Table/index name, or a bookkeeping label like `(freelist)`.
    pub owner: String,
    pub changed: u32,
    pub added: u32,
    pub removed: u32,
}

impl OwnerDelta {
    pub fn total(&self) -> u32 {
        self.changed + self.added + self.removed
    }
}

/// A complete diff report.
#[derive(Debug)]
pub struct DiffReport {
    pub old_pages: u32,
    pub new_pages: u32,
    pub page_size: u32,
    pub deltas: Vec<PageDelta>,
    /// Rollup sorted by owner name (schema first, bookkeeping last).
    pub by_owner: Vec<OwnerDelta>,
}

impl DiffReport {
    pub fn total_changed(&self) -> u32 {
        self.deltas
            .iter()
            .filter(|d| d.change == Change::Changed)
            .count() as u32
    }
    pub fn total_added(&self) -> u32 {
        self.deltas
            .iter()
            .filter(|d| d.change == Change::Added)
            .count() as u32
    }
    pub fn total_removed(&self) -> u32 {
        self.deltas
            .iter()
            .filter(|d| d.change == Change::Removed)
            .count() as u32
    }
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

/// Display label for a page's owner: the object name for b-tree/overflow
/// pages, a parenthesized kind for bookkeeping pages.
pub fn owner_label(owner: &PageOwner) -> String {
    match (&owner.kind, &owner.owner) {
        (PageKind::BTree, Some(name)) | (PageKind::Overflow, Some(name)) => name.clone(),
        (kind, _) => format!("({})", kind.label()),
    }
}

/// Diff two page sources and attribute every differing page.
///
/// Both sides must share a page size (SQLite cannot change it without a
/// VACUUM, and a vacuumed file is a rewrite anyway — diffing it page by page
/// would be noise, so litegraft reports the mismatch instead).
pub fn diff_sources(
    old: &mut dyn PageSource,
    new: &mut dyn PageSource,
    old_hashes: &[String],
    new_hashes: &[String],
) -> io::Result<DiffReport> {
    if old.page_size() != new.page_size() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "page size differs ({} vs {}): states are not page-comparable (VACUUM or rebuilt file?)",
                old.page_size(),
                new.page_size()
            ),
        ));
    }
    let deltas = diff_hashes(old_hashes, new_hashes);
    let new_map = attribute_pages(new)?;
    let old_map = attribute_pages(old)?;

    let mut rollup: BTreeMap<String, OwnerDelta> = BTreeMap::new();
    for delta in &deltas {
        // Removed pages only exist in the old state; everything else is
        // attributed by where the page landed in the new state.
        let owner = match delta.change {
            Change::Removed => old_map.get(&delta.pgno),
            _ => new_map
                .get(&delta.pgno)
                .or_else(|| old_map.get(&delta.pgno)),
        };
        let label = owner
            .map(owner_label)
            .unwrap_or_else(|| "(unattributed)".to_string());
        let entry = rollup.entry(label.clone()).or_insert(OwnerDelta {
            owner: label,
            changed: 0,
            added: 0,
            removed: 0,
        });
        match delta.change {
            Change::Changed => entry.changed += 1,
            Change::Added => entry.added += 1,
            Change::Removed => entry.removed += 1,
        }
    }

    // Named objects first (alphabetical), bookkeeping "(...)" labels last.
    let mut by_owner: Vec<OwnerDelta> = rollup.into_values().collect();
    by_owner.sort_by(|a, b| {
        let ka = (a.owner.starts_with('('), a.owner.clone());
        let kb = (b.owner.starts_with('('), b.owner.clone());
        ka.cmp(&kb)
    });

    Ok(DiffReport {
        old_pages: old_hashes.len() as u32,
        new_pages: new_hashes.len() as u32,
        page_size: old.page_size(),
        deltas,
        by_owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(tags: &[&str]) -> Vec<String> {
        tags.iter()
            .map(|t| crate::sha256::hex_digest(t.as_bytes()))
            .collect()
    }

    #[test]
    fn identical_states_produce_an_empty_diff() {
        let a = hashes(&["p1", "p2", "p3"]);
        assert!(diff_hashes(&a, &a).is_empty());
        let report = DiffReport {
            old_pages: 3,
            new_pages: 3,
            page_size: 4096,
            deltas: vec![],
            by_owner: vec![],
        };
        assert!(report.is_empty());
        assert_eq!(
            report.total_changed() + report.total_added() + report.total_removed(),
            0
        );
    }

    #[test]
    fn changed_pages_are_reported_with_one_based_numbers() {
        let old = hashes(&["p1", "p2", "p3"]);
        let new = hashes(&["p1", "P2", "p3"]);
        let d = diff_hashes(&old, &new);
        assert_eq!(
            d,
            vec![PageDelta {
                pgno: 2,
                change: Change::Changed
            }]
        );
    }

    #[test]
    fn grown_file_reports_added_pages() {
        let old = hashes(&["p1"]);
        let new = hashes(&["p1", "p2", "p3"]);
        let d = diff_hashes(&old, &new);
        assert_eq!(
            d,
            vec![
                PageDelta {
                    pgno: 2,
                    change: Change::Added
                },
                PageDelta {
                    pgno: 3,
                    change: Change::Added
                },
            ]
        );
    }

    #[test]
    fn shrunk_file_reports_removed_pages() {
        let old = hashes(&["p1", "p2", "p3"]);
        let new = hashes(&["p1"]);
        let d = diff_hashes(&old, &new);
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.change == Change::Removed));
        assert_eq!(d[0].pgno, 2);
    }

    #[test]
    fn change_and_growth_combine() {
        let old = hashes(&["p1", "p2"]);
        let new = hashes(&["P1", "p2", "p3"]);
        let d = diff_hashes(&old, &new);
        assert_eq!(
            d,
            vec![
                PageDelta {
                    pgno: 1,
                    change: Change::Changed
                },
                PageDelta {
                    pgno: 3,
                    change: Change::Added
                },
            ]
        );
    }

    #[test]
    fn owner_label_uses_names_for_btrees_and_parens_for_bookkeeping() {
        let named = PageOwner {
            kind: PageKind::BTree,
            owner: Some("users".into()),
        };
        assert_eq!(owner_label(&named), "users");
        let overflow = PageOwner {
            kind: PageKind::Overflow,
            owner: Some("blobs".into()),
        };
        assert_eq!(owner_label(&overflow), "blobs");
        let free = PageOwner {
            kind: PageKind::Freelist,
            owner: None,
        };
        assert_eq!(owner_label(&free), "(freelist)");
    }
}
