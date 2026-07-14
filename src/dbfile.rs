//! SQLite database file access: header parsing and page-granular reads.
//!
//! Everything litegraft does — snapshot, branch, diff — is defined in terms
//! of the fixed-size pages that make up a SQLite database file. This module
//! parses the 100-byte file header and exposes pages through the
//! [`PageSource`] trait, which is implemented both by a live database file
//! (here) and by a snapshot backed by the object store (`snapshot.rs`), so
//! the b-tree walker can attribute pages to tables in either world.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The 16-byte magic at the start of every SQLite database file.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Size of the database file header, which lives at the start of page 1.
pub const HEADER_LEN: usize = 100;

/// Parsed fields of the SQLite database header that litegraft cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbHeader {
    /// Page size in bytes (512..=65536, always a power of two).
    pub page_size: u32,
    /// Bytes reserved at the end of every page (usually 0).
    pub reserved: u8,
    /// File change counter (offset 24).
    pub change_counter: u32,
    /// Number of pages in the database.
    pub page_count: u32,
    /// First page of the freelist trunk chain (0 = empty freelist).
    pub freelist_head: u32,
    /// Total number of freelist pages.
    pub freelist_count: u32,
    /// Text encoding: 1 = UTF-8, 2 = UTF-16le, 3 = UTF-16be.
    pub text_encoding: u32,
    /// Largest root b-tree page; non-zero means auto-vacuum/incremental
    /// vacuum is on and pointer-map pages are interleaved in the file.
    pub largest_root_btree: u32,
}

impl DbHeader {
    /// Usable bytes per page (page size minus the reserved region).
    pub fn usable_size(&self) -> u32 {
        self.page_size - self.reserved as u32
    }

    /// Parse the first 100 bytes of a database file.
    ///
    /// `file_len` is used to compute the page count when the header copy is
    /// stale (legacy writers only bump it together with `version-valid-for`).
    pub fn parse(buf: &[u8], file_len: u64) -> io::Result<DbHeader> {
        if buf.len() < HEADER_LEN {
            return Err(bad(format!(
                "file too small for a SQLite header ({} bytes)",
                buf.len()
            )));
        }
        if &buf[0..16] != MAGIC {
            return Err(bad("not a SQLite database (bad magic)"));
        }
        let raw_page_size = u16::from_be_bytes([buf[16], buf[17]]);
        // Value 1 means 65536 (the real size does not fit in 16 bits).
        let page_size: u32 = if raw_page_size == 1 {
            65536
        } else {
            raw_page_size as u32
        };
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(bad(format!("invalid page size {page_size}")));
        }
        let reserved = buf[20];
        if page_size - (reserved as u32) < 480 {
            return Err(bad(format!(
                "reserved bytes {reserved} leave an unusably small page"
            )));
        }
        let be32 =
            |off: usize| u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let change_counter = be32(24);
        let header_page_count = be32(28);
        let version_valid_for = be32(92);
        // The in-header page count is only trustworthy when it was written
        // by the same transaction as the change counter.
        let page_count = if header_page_count != 0 && version_valid_for == change_counter {
            header_page_count
        } else {
            (file_len / page_size as u64) as u32
        };
        Ok(DbHeader {
            page_size,
            reserved,
            change_counter,
            page_count,
            freelist_head: be32(32),
            freelist_count: be32(36),
            text_encoding: be32(56),
            largest_root_btree: be32(52),
        })
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Uniform page-granular read access for live files and snapshots.
pub trait PageSource {
    fn header(&self) -> &DbHeader;
    /// Read page `pgno` (1-based, per the SQLite convention).
    fn page(&mut self, pgno: u32) -> io::Result<Vec<u8>>;

    fn page_size(&self) -> u32 {
        self.header().page_size
    }
    fn page_count(&self) -> u32 {
        self.header().page_count
    }
}

/// A live SQLite database file on disk.
pub struct DbFile {
    pub path: PathBuf,
    file: File,
    header: DbHeader,
}

impl DbFile {
    pub fn open(path: &Path) -> io::Result<DbFile> {
        let mut file = File::open(path).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot open {}: {e}", path.display()))
        })?;
        let len = file.metadata()?.len();
        let mut buf = [0u8; HEADER_LEN];
        file.read_exact(&mut buf).map_err(|_| {
            bad(format!(
                "{}: too small to be a SQLite database",
                path.display()
            ))
        })?;
        let header = DbHeader::parse(&buf, len)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        if len < header.page_size as u64 * header.page_count as u64 {
            return Err(bad(format!(
                "{}: truncated ({} bytes < {} pages x {} bytes)",
                path.display(),
                len,
                header.page_count,
                header.page_size
            )));
        }
        Ok(DbFile {
            path: path.to_path_buf(),
            file,
            header,
        })
    }
}

impl PageSource for DbFile {
    fn header(&self) -> &DbHeader {
        &self.header
    }

    fn page(&mut self, pgno: u32) -> io::Result<Vec<u8>> {
        if pgno == 0 || pgno > self.header.page_count {
            return Err(bad(format!(
                "page {pgno} out of range (1..={})",
                self.header.page_count
            )));
        }
        let mut buf = vec![0u8; self.header.page_size as usize];
        self.file.seek(SeekFrom::Start(
            (pgno as u64 - 1) * self.header.page_size as u64,
        ))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// State of the write-ahead-log sidecar file, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalState {
    /// No `-wal` file, or an empty stub: the main file is complete.
    Clean,
    /// The `-wal` file holds committed frames not yet checkpointed into the
    /// main file; snapshotting the main file alone would miss them.
    Pending { frames: u64 },
}

/// Inspect `<db>-wal` next to the database.
///
/// A WAL file is a 32-byte header plus `(24 + page_size)`-byte frames. Any
/// frame at all means the main file may be stale, so litegraft refuses to
/// snapshot or restore until the log is checkpointed.
pub fn wal_state(db_path: &Path, page_size: u32) -> io::Result<WalState> {
    let wal = sidecar(db_path, "-wal");
    match std::fs::metadata(&wal) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(WalState::Clean),
        Err(e) => Err(e),
        Ok(meta) if meta.len() <= 32 => Ok(WalState::Clean),
        Ok(meta) => {
            let frames = (meta.len() - 32) / (24 + page_size as u64);
            Ok(WalState::Pending {
                frames: frames.max(1),
            })
        }
    }
}

/// True if a non-empty rollback journal (`<db>-journal`) exists, meaning a
/// transaction may be mid-flight or was interrupted.
pub fn hot_journal(db_path: &Path) -> io::Result<bool> {
    let journal = sidecar(db_path, "-journal");
    match std::fs::metadata(&journal) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
        Ok(meta) => Ok(meta.len() > 0),
    }
}

/// Path of a SQLite sidecar file: `app.db` -> `app.db-wal` etc.
pub fn sidecar(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syntactically valid 100-byte header for parser tests.
    fn header_bytes(page_size: u16, pages: u32) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[0..16].copy_from_slice(MAGIC);
        h[16..18].copy_from_slice(&page_size.to_be_bytes());
        h[18] = 1; // write version
        h[19] = 1; // read version
        h[21] = 64; // max payload fraction
        h[22] = 32; // min payload fraction
        h[23] = 32; // leaf payload fraction
        h[24..28].copy_from_slice(&7u32.to_be_bytes()); // change counter
        h[28..32].copy_from_slice(&pages.to_be_bytes()); // page count
        h[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
        h[92..96].copy_from_slice(&7u32.to_be_bytes()); // version-valid-for
        h
    }

    #[test]
    fn parses_a_wellformed_header() {
        let h = DbHeader::parse(&header_bytes(4096, 12), 12 * 4096).unwrap();
        assert_eq!(h.page_size, 4096);
        assert_eq!(h.page_count, 12);
        assert_eq!(h.text_encoding, 1);
        assert_eq!(h.usable_size(), 4096);
    }

    #[test]
    fn page_size_one_means_65536() {
        // 65536 does not fit in the 16-bit field; SQLite stores 1.
        let h = DbHeader::parse(&header_bytes(1, 2), 2 * 65536).unwrap();
        assert_eq!(h.page_size, 65536);
    }

    #[test]
    fn rejects_wrong_magic_and_short_buffer() {
        let mut buf = header_bytes(4096, 1);
        buf[0] = b'X';
        let e = DbHeader::parse(&buf, 4096).unwrap_err();
        assert!(e.to_string().contains("magic"), "{e}");
        assert!(
            DbHeader::parse(&[0u8; 50], 4096).is_err(),
            "50 bytes is not a header"
        );
    }

    #[test]
    fn rejects_non_power_of_two_page_size() {
        for bad_size in [0u16, 100, 513, 4095] {
            let mut buf = header_bytes(4096, 1);
            buf[16..18].copy_from_slice(&bad_size.to_be_bytes());
            assert!(
                DbHeader::parse(&buf, 4096).is_err(),
                "size {bad_size} accepted"
            );
        }
    }

    #[test]
    fn stale_header_page_count_falls_back_to_file_length() {
        // version-valid-for != change counter: the stored count is stale and
        // the real count must come from the file size.
        let mut buf = header_bytes(512, 3);
        buf[92..96].copy_from_slice(&6u32.to_be_bytes());
        let h = DbHeader::parse(&buf, 10 * 512).unwrap();
        assert_eq!(h.page_count, 10);
    }

    #[test]
    fn reserved_bytes_shrink_usable_size() {
        let mut buf = header_bytes(4096, 1);
        buf[20] = 16;
        let h = DbHeader::parse(&buf, 4096).unwrap();
        assert_eq!(h.usable_size(), 4080);
    }

    #[test]
    fn sidecar_paths_append_suffix() {
        let p = sidecar(Path::new("/data/app.db"), "-wal");
        assert_eq!(p, PathBuf::from("/data/app.db-wal"));
    }
}
