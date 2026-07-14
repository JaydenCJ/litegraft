//! Snapshot creation, restore and working-state inspection.
//!
//! `snap` reads a database page by page, hashes each page, stores only the
//! pages the store has never seen (that is the dedup), and writes a manifest
//! whose id is the hash of the whole state. `restore` is the inverse: it
//! materializes a manifest into a byte-identical database file, atomically.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::dbfile::{self, DbFile, DbHeader, PageSource, WalState};
use crate::store::{page_hash, state_id, Head, Manifest, Store};

fn blocked(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Refuse to trust the main file while sidecar files say it is incomplete.
/// `snap` and `status` call this before reading; `restore` before writing.
pub fn guard_sidecars(db_path: &Path, page_size: u32, allow_wal: bool) -> io::Result<()> {
    if dbfile::hot_journal(db_path)? {
        return Err(blocked(format!(
            "{}-journal exists and is non-empty: a rollback transaction may be mid-flight; \
             close the writer (or delete a stale journal) and retry",
            db_path.display()
        )));
    }
    if let WalState::Pending { frames } = dbfile::wal_state(db_path, page_size)? {
        if !allow_wal {
            return Err(blocked(format!(
                "{}-wal holds ~{frames} frame(s) not yet checkpointed; the main file is stale. \
                 Run `PRAGMA wal_checkpoint(TRUNCATE);` (or close all connections), then retry. \
                 Use --allow-wal to snapshot the main file anyway",
                db_path.display()
            )));
        }
    }
    Ok(())
}

/// Result of hashing the working file.
pub struct FileState {
    pub id: String,
    pub page_size: u32,
    pub page_hashes: Vec<String>,
}

/// Hash every page of the database and compute its state id. Read-only.
pub fn state_of_file(db_path: &Path) -> io::Result<FileState> {
    let mut db = DbFile::open(db_path)?;
    let mut page_hashes = Vec::with_capacity(db.page_count() as usize);
    for pgno in 1..=db.page_count() {
        page_hashes.push(page_hash(&db.page(pgno)?));
    }
    let id = state_id(db.page_size(), &page_hashes);
    Ok(FileState {
        id,
        page_size: db.page_size(),
        page_hashes,
    })
}

/// Outcome of a `snap`.
pub struct SnapOutcome {
    pub id: String,
    pub parent: Option<String>,
    pub branch: String,
    pub pages: u32,
    /// Pages whose content the store had never seen before this snap.
    pub new_objects: u32,
    /// Pages deduplicated against existing objects.
    pub dedup_pages: u32,
    pub bytes_written: u64,
    pub elapsed_ms: f64,
    /// False when the state was already the branch head (no-op snap).
    pub changed: bool,
}

/// Snapshot `db_path` into `store` and advance the current branch.
pub fn snap(
    store: &Store,
    db_path: &Path,
    message: &str,
    allow_wal: bool,
) -> io::Result<SnapOutcome> {
    let started = Instant::now();
    let mut db = DbFile::open(db_path)?;
    guard_sidecars(db_path, db.page_size(), allow_wal)?;

    let branch = match store.head()? {
        Head::Branch(name) => name,
        Head::Detached(id) => {
            return Err(blocked(format!(
                "HEAD is detached at {}; create a branch first: litegraft branch <name>",
                &id[..12.min(id.len())]
            )))
        }
    };
    let parent = store.branch_head(&branch)?;

    let mut page_hashes = Vec::with_capacity(db.page_count() as usize);
    let mut new_objects = 0u32;
    let mut bytes_written = 0u64;
    for pgno in 1..=db.page_count() {
        let page = db.page(pgno)?;
        let hash = page_hash(&page);
        if store.write_object(&hash, &page)? {
            new_objects += 1;
            bytes_written += page.len() as u64;
        }
        page_hashes.push(hash);
    }
    let pages = page_hashes.len() as u32;
    let id = state_id(db.page_size(), &page_hashes);

    if parent.as_deref() == Some(id.as_str()) {
        return Ok(SnapOutcome {
            id,
            parent,
            branch,
            pages,
            new_objects,
            dedup_pages: pages - new_objects,
            bytes_written,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            changed: false,
        });
    }

    if !store.has_snapshot(&id) {
        let manifest = Manifest {
            id: id.clone(),
            parent: parent.clone(),
            branch: branch.clone(),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            message: message.to_string(),
            page_size: db.page_size(),
            page_hashes,
        };
        store.write_manifest(&manifest)?;
    }
    store.set_branch(&branch, &id)?;

    Ok(SnapOutcome {
        id,
        parent,
        branch,
        pages,
        new_objects,
        dedup_pages: pages - new_objects,
        bytes_written,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        changed: true,
    })
}

/// Materialize snapshot `id` as the database file at `db_path`.
///
/// Writes to a temp file in the same directory, fsyncs, then renames over
/// the target, so the database is never observable half-restored. Stale
/// `-wal`/`-shm`/`-journal` sidecars are removed: they belong to the state
/// being replaced and would corrupt the restored file if SQLite replayed
/// them.
pub fn restore(store: &Store, id: &str, db_path: &Path) -> io::Result<()> {
    let manifest = store.read_manifest(id)?;
    let dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".litegraft-restore-{}-{}",
        std::process::id(),
        &manifest.id[..12]
    ));
    let result = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        for hash in &manifest.page_hashes {
            let page = store.read_object(hash)?;
            if page.len() != manifest.page_size as usize {
                return Err(blocked(format!(
                    "object {hash} is {} bytes, expected page size {}",
                    page.len(),
                    manifest.page_size
                )));
            }
            f.write_all(&page)?;
        }
        f.sync_all()?;
        Ok(())
    })();
    let result = result.and_then(|()| fs::rename(&tmp, db_path));
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = dbfile::sidecar(db_path, suffix);
        if sidecar.exists() {
            fs::remove_file(&sidecar)?;
        }
    }
    Ok(())
}

/// A snapshot viewed as a [`PageSource`], so the b-tree walker can attribute
/// pages of historical states without restoring them to disk.
pub struct SnapshotSource<'a> {
    store: &'a Store,
    manifest: Manifest,
    header: DbHeader,
}

impl<'a> SnapshotSource<'a> {
    pub fn open(store: &'a Store, id: &str) -> io::Result<SnapshotSource<'a>> {
        let manifest = store.read_manifest(id)?;
        if manifest.page_hashes.is_empty() {
            return Err(blocked(format!("snapshot {id} has no pages")));
        }
        let page1 = store.read_object(&manifest.page_hashes[0])?;
        let file_len = manifest.page_size as u64 * manifest.page_count() as u64;
        let header = DbHeader::parse(&page1, file_len)?;
        Ok(SnapshotSource {
            store,
            manifest,
            header,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

impl PageSource for SnapshotSource<'_> {
    fn header(&self) -> &DbHeader {
        &self.header
    }

    fn page(&mut self, pgno: u32) -> io::Result<Vec<u8>> {
        let idx = pgno
            .checked_sub(1)
            .map(|i| i as usize)
            .filter(|&i| i < self.manifest.page_hashes.len())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "page {pgno} out of range (1..={})",
                        self.manifest.page_count()
                    ),
                )
            })?;
        self.store.read_object(&self.manifest.page_hashes[idx])
    }
}
