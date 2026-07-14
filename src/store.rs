//! The content-addressed store: page objects, snapshot manifests, branches.
//!
//! A store lives in a `<db>.litegraft/` directory next to the database it
//! tracks, the same way `-wal` and `-shm` sit next to the file they belong
//! to. Layout:
//!
//! ```text
//! app.db.litegraft/
//! ├── LITEGRAFT            format marker ("litegraft store 1")
//! ├── objects/ab/cd…       raw page bytes, keyed by SHA-256
//! ├── snaps/<id>           snapshot manifests (plain text, line-based)
//! ├── refs/heads/<name>    branch heads (one snapshot id per file)
//! └── HEAD                 "ref: <branch>" or "snap: <id>" (detached)
//! ```
//!
//! A snapshot id is the SHA-256 of the database *state* (page size + ordered
//! page hashes), so snapping an unchanged database is naturally idempotent
//! and two branches at the same content share one manifest.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::sha256::{hex_digest, Sha256};

const MARKER: &str = "litegraft store 1\n";
const MANIFEST_HEADER: &str = "litegraft snapshot 1";

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Where HEAD points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
}

/// A snapshot manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub parent: Option<String>,
    pub branch: String,
    pub created: u64,
    pub message: String,
    pub page_size: u32,
    pub page_hashes: Vec<String>,
}

impl Manifest {
    pub fn page_count(&self) -> u32 {
        self.page_hashes.len() as u32
    }
}

/// Compute the snapshot id for a database state.
pub fn state_id(page_size: u32, page_hashes: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(b"litegraft-state 1\n");
    h.update(page_size.to_string().as_bytes());
    h.update(b"\n");
    for hash in page_hashes {
        h.update(hash.as_bytes());
        h.update(b"\n");
    }
    crate::sha256::to_hex(&h.finish())
}

/// Default store directory for a database: `<db>.litegraft`.
pub fn default_store_dir(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(".litegraft");
    PathBuf::from(s)
}

#[derive(Debug)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    /// Create a fresh store. Fails if one already exists there.
    pub fn init(dir: &Path) -> io::Result<Store> {
        if dir.join("LITEGRAFT").exists() {
            return Err(invalid(format!(
                "store already initialized at {}",
                dir.display()
            )));
        }
        fs::create_dir_all(dir.join("objects"))?;
        fs::create_dir_all(dir.join("snaps"))?;
        fs::create_dir_all(dir.join("refs/heads"))?;
        fs::write(dir.join("LITEGRAFT"), MARKER)?;
        let store = Store {
            dir: dir.to_path_buf(),
        };
        store.set_head(&Head::Branch("main".to_string()))?;
        Ok(store)
    }

    /// Open an existing store.
    pub fn open(dir: &Path) -> io::Result<Store> {
        let marker = dir.join("LITEGRAFT");
        let content = fs::read_to_string(&marker).map_err(|_| {
            invalid(format!(
                "no litegraft store at {} (run `litegraft init` first)",
                dir.display()
            ))
        })?;
        if content != MARKER {
            return Err(invalid(format!(
                "unrecognized store format at {}",
                dir.display()
            )));
        }
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    // ---- objects -----------------------------------------------------------

    fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(&hash[..2]).join(&hash[2..])
    }

    pub fn has_object(&self, hash: &str) -> bool {
        self.object_path(hash).exists()
    }

    /// Store page bytes under their hash. Idempotent; writes via a temp file
    /// and rename so a concurrent reader never sees a truncated object.
    ///
    /// Deliberately not fsynced: objects are re-creatable from the database
    /// file by re-running `snap`, and `verify` re-hashes every object, so a
    /// torn write after power loss is detectable and cheap to repair. That
    /// keeps snapshots in the low-millisecond range instead of paying one
    /// fsync per page.
    pub fn write_object(&self, hash: &str, bytes: &[u8]) -> io::Result<bool> {
        let path = self.object_path(hash);
        if path.exists() {
            return Ok(false);
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
        }
        fs::rename(&tmp, &path)?;
        Ok(true)
    }

    pub fn read_object(&self, hash: &str) -> io::Result<Vec<u8>> {
        fs::read(self.object_path(hash)).map_err(|_| {
            invalid(format!(
                "missing object {hash} (store corrupt? run `litegraft verify`)"
            ))
        })
    }

    /// Every object hash in the store.
    pub fn list_objects(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let objects = self.dir.join("objects");
        for shard in fs::read_dir(&objects)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let prefix = shard.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(shard.path())? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".tmp") {
                    out.push(format!("{prefix}{name}"));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn remove_object(&self, hash: &str) -> io::Result<()> {
        fs::remove_file(self.object_path(hash))
    }

    // ---- manifests ---------------------------------------------------------

    fn snap_path(&self, id: &str) -> PathBuf {
        self.dir.join("snaps").join(id)
    }

    pub fn has_snapshot(&self, id: &str) -> bool {
        self.snap_path(id).exists()
    }

    pub fn write_manifest(&self, m: &Manifest) -> io::Result<()> {
        let mut text = String::new();
        text.push_str(MANIFEST_HEADER);
        text.push('\n');
        text.push_str(&format!("id {}\n", m.id));
        text.push_str(&format!("parent {}\n", m.parent.as_deref().unwrap_or("-")));
        text.push_str(&format!("branch {}\n", m.branch));
        text.push_str(&format!("created {}\n", m.created));
        text.push_str(&format!("message {}\n", escape_line(&m.message)));
        text.push_str(&format!("pagesize {}\n", m.page_size));
        text.push_str(&format!("pages {}\n", m.page_hashes.len()));
        text.push_str("--\n");
        for h in &m.page_hashes {
            text.push_str(h);
            text.push('\n');
        }
        let path = self.snap_path(&m.id);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn read_manifest(&self, id: &str) -> io::Result<Manifest> {
        let text = fs::read_to_string(self.snap_path(id))
            .map_err(|_| invalid(format!("no snapshot {id}")))?;
        parse_manifest(&text)
    }

    /// All snapshot ids in the store (sorted lexically).
    pub fn list_snapshots(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join("snaps"))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.len() == 64 && !name.ends_with(".tmp") {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    // ---- refs / HEAD -------------------------------------------------------

    pub fn head(&self) -> io::Result<Head> {
        let text = fs::read_to_string(self.dir.join("HEAD"))?;
        let text = text.trim_end();
        if let Some(branch) = text.strip_prefix("ref: ") {
            Ok(Head::Branch(branch.to_string()))
        } else if let Some(id) = text.strip_prefix("snap: ") {
            Ok(Head::Detached(id.to_string()))
        } else {
            Err(invalid(format!("unparseable HEAD: {text:?}")))
        }
    }

    pub fn set_head(&self, head: &Head) -> io::Result<()> {
        let text = match head {
            Head::Branch(b) => format!("ref: {b}\n"),
            Head::Detached(id) => format!("snap: {id}\n"),
        };
        fs::write(self.dir.join("HEAD"), text)
    }

    fn branch_path(&self, name: &str) -> io::Result<PathBuf> {
        validate_branch_name(name)?;
        Ok(self.dir.join("refs/heads").join(name))
    }

    /// Head snapshot of a branch; `None` for an unborn branch (exists as
    /// HEAD target but has no snapshot yet).
    pub fn branch_head(&self, name: &str) -> io::Result<Option<String>> {
        let path = self.branch_path(name)?;
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text.trim_end().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn set_branch(&self, name: &str, id: &str) -> io::Result<()> {
        let path = self.branch_path(name)?;
        // Names may contain `/` (e.g. fix/late-fees), stored as subdirectories.
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, format!("{id}\n"))
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        match self.branch_path(name) {
            Ok(p) => p.exists(),
            Err(_) => false,
        }
    }

    /// Branch name -> head id, sorted by name. Walks subdirectories so
    /// `fix/late-fees`-style names round-trip.
    pub fn list_branches(&self) -> io::Result<BTreeMap<String, String>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, String>) -> io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let full = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.file_type()?.is_dir() {
                    walk(&entry.path(), &full, out)?;
                } else {
                    let id = fs::read_to_string(entry.path())?.trim_end().to_string();
                    out.insert(full, id);
                }
            }
            Ok(())
        }
        let mut out = BTreeMap::new();
        walk(&self.dir.join("refs/heads"), "", &mut out)?;
        Ok(out)
    }

    /// Resolve a user-supplied ref: a branch name, a full snapshot id, or a
    /// unique id prefix (>= 4 hex chars).
    pub fn resolve(&self, refname: &str) -> io::Result<String> {
        if validate_branch_name(refname).is_ok() {
            if let Some(id) = self.branch_head(refname)? {
                return Ok(id);
            }
        }
        if refname.len() == 64 && self.has_snapshot(refname) {
            return Ok(refname.to_string());
        }
        if refname.len() >= 4 && refname.chars().all(|c| c.is_ascii_hexdigit()) {
            let matches: Vec<String> = self
                .list_snapshots()?
                .into_iter()
                .filter(|id| id.starts_with(refname))
                .collect();
            match matches.len() {
                1 => return Ok(matches.into_iter().next().unwrap()),
                0 => {}
                n => {
                    return Err(invalid(format!(
                        "ambiguous ref {refname:?} ({n} snapshots match)"
                    )))
                }
            }
        }
        Err(invalid(format!(
            "unknown ref {refname:?} (not a branch, snapshot id, or unique prefix)"
        )))
    }
}

/// Branch names: 1..=100 chars of `[a-zA-Z0-9._/-]`, no leading dash or dot,
/// no `..`, not all-hex-64 (would shadow snapshot ids).
pub fn validate_branch_name(name: &str) -> io::Result<()> {
    let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    if name.is_empty() || name.len() > 100 {
        return Err(invalid(format!(
            "invalid branch name {name:?}: empty or too long"
        )));
    }
    if name.starts_with('-') || name.starts_with('.') || name.starts_with('/') {
        return Err(invalid(format!(
            "invalid branch name {name:?}: bad leading character"
        )));
    }
    if name.contains("..") || name.ends_with('/') || !name.chars().all(ok_char) {
        return Err(invalid(format!("invalid branch name {name:?}")));
    }
    if name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "invalid branch name {name:?}: looks like a snapshot id"
        )));
    }
    if name == "@" {
        return Err(invalid(
            "invalid branch name \"@\": reserved for the working file",
        ));
    }
    Ok(())
}

fn escape_line(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_manifest(text: &str) -> io::Result<Manifest> {
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(invalid("bad manifest header"));
    }
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in lines.by_ref() {
        if line == "--" {
            break;
        }
        let (key, value) = line
            .split_once(' ')
            .ok_or_else(|| invalid(format!("bad manifest line {line:?}")))?;
        fields.insert(key, value);
    }
    let get = |key: &str| {
        fields
            .get(key)
            .copied()
            .ok_or_else(|| invalid(format!("manifest missing field {key:?}")))
    };
    let parent = match get("parent")? {
        "-" => None,
        id => Some(id.to_string()),
    };
    let pages: usize = get("pages")?
        .parse()
        .map_err(|_| invalid("bad pages field"))?;
    let mut page_hashes = Vec::with_capacity(pages);
    for line in lines {
        if line.len() != 64 || !line.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid(format!("bad page hash line {line:?}")));
        }
        page_hashes.push(line.to_string());
    }
    if page_hashes.len() != pages {
        return Err(invalid(format!(
            "manifest lists {} page hashes but declares {pages}",
            page_hashes.len()
        )));
    }
    let m = Manifest {
        id: get("id")?.to_string(),
        parent,
        branch: get("branch")?.to_string(),
        created: get("created")?
            .parse()
            .map_err(|_| invalid("bad created field"))?,
        message: unescape_line(get("message")?),
        page_size: get("pagesize")?
            .parse()
            .map_err(|_| invalid("bad pagesize field"))?,
        page_hashes,
    };
    // Integrity: the stored id must match the recomputed state id.
    let want = state_id(m.page_size, &m.page_hashes);
    if m.id != want {
        return Err(invalid(format!(
            "manifest id {} does not match its content ({want})",
            m.id
        )));
    }
    Ok(m)
}

/// Hash one page's raw bytes (the object key).
pub fn page_hash(bytes: &[u8]) -> String {
    hex_digest(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempdir::TempDir, Store) {
        let dir = tempdir::TempDir::new("litegraft-store-test");
        let store = Store::init(&dir.path().join("s")).unwrap();
        (dir, store)
    }

    /// Minimal in-crate temp-dir helper so tests need no dev-dependencies.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new(prefix: &str) -> TempDir {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let dir = std::env::temp_dir().join(format!(
                    "{prefix}-{}-{}-{n}",
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
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn sample_manifest(page_size: u32, pages: &[&[u8]]) -> Manifest {
        let hashes: Vec<String> = pages.iter().map(|p| page_hash(p)).collect();
        Manifest {
            id: state_id(page_size, &hashes),
            parent: None,
            branch: "main".into(),
            created: 1_760_000_000,
            message: "first".into(),
            page_size,
            page_hashes: hashes,
        }
    }

    #[test]
    fn init_creates_marker_and_unborn_main() {
        let (_t, store) = temp_store();
        assert_eq!(store.head().unwrap(), Head::Branch("main".into()));
        assert_eq!(
            store.branch_head("main").unwrap(),
            None,
            "main is unborn until first snap"
        );
        assert!(Store::init(&store.dir).is_err(), "double init must fail");
        // HEAD can also point directly at a snapshot (detached).
        let id = "c".repeat(64);
        store.set_head(&Head::Detached(id.clone())).unwrap();
        assert_eq!(store.head().unwrap(), Head::Detached(id));
    }

    #[test]
    fn open_rejects_a_plain_directory() {
        let (t, _store) = temp_store();
        let plain = t.path().join("not-a-store");
        std::fs::create_dir_all(&plain).unwrap();
        let e = Store::open(&plain).unwrap_err();
        assert!(e.to_string().contains("litegraft init"), "{e}");
    }

    #[test]
    fn objects_roundtrip_and_dedup() {
        let (_t, store) = temp_store();
        let page = vec![7u8; 512];
        let hash = page_hash(&page);
        assert!(
            store.write_object(&hash, &page).unwrap(),
            "first write stores"
        );
        assert!(
            !store.write_object(&hash, &page).unwrap(),
            "second write dedups"
        );
        assert_eq!(store.read_object(&hash).unwrap(), page);
        assert_eq!(store.list_objects().unwrap(), vec![hash]);
    }

    #[test]
    fn missing_object_error_mentions_verify() {
        let (_t, store) = temp_store();
        let e = store.read_object(&"0".repeat(64)).unwrap_err();
        assert!(e.to_string().contains("verify"), "{e}");
    }

    #[test]
    fn manifest_roundtrips_including_escaped_message() {
        let (_t, store) = temp_store();
        let mut m = sample_manifest(512, &[&[1u8; 512], &[2u8; 512]]);
        m.message = "line one\nline two \\ backslash".into();
        store.write_manifest(&m).unwrap();
        let back = store.read_manifest(&m.id).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn manifest_with_tampered_page_hash_is_rejected() {
        // A flipped hash changes the recomputed state id -> integrity error.
        let (_t, store) = temp_store();
        let m = sample_manifest(512, &[&[1u8; 512]]);
        store.write_manifest(&m).unwrap();
        let path = store.dir.join("snaps").join(&m.id);
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replace(&m.page_hashes[0], &"a".repeat(64));
        std::fs::write(&path, tampered).unwrap();
        let e = store.read_manifest(&m.id).unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
    }

    #[test]
    fn state_id_depends_on_order_size_and_content() {
        let a = page_hash(b"a");
        let b = page_hash(b"b");
        let id1 = state_id(512, &[a.clone(), b.clone()]);
        let id2 = state_id(512, &[b, a.clone()]);
        let id3 = state_id(1024, &[a.clone(), page_hash(b"b")]);
        assert_ne!(id1, id2, "page order matters");
        assert_ne!(id1, id3, "page size matters");
        assert_eq!(
            id1,
            state_id(512, &[a, page_hash(b"b")]),
            "same state, same id"
        );
    }

    #[test]
    fn branches_set_list_and_resolve() {
        let (_t, store) = temp_store();
        let m = sample_manifest(512, &[&[1u8; 512]]);
        store.write_manifest(&m).unwrap();
        store.set_branch("main", &m.id).unwrap();
        store.set_branch("fix/late-fees", &m.id).unwrap();
        let branches = store.list_branches().unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches["fix/late-fees"], m.id);
        assert_eq!(store.resolve("main").unwrap(), m.id);
        assert_eq!(
            store.resolve(&m.id[..8]).unwrap(),
            m.id,
            "unique prefix resolves"
        );
        assert_eq!(store.resolve(&m.id).unwrap(), m.id, "full id resolves");
    }

    #[test]
    fn resolve_rejects_unknown_and_short_prefixes() {
        let (_t, store) = temp_store();
        assert!(store.resolve("nope").is_err());
        assert!(
            store.resolve("abc").is_err(),
            "3 hex chars is below the prefix minimum"
        );
    }

    #[test]
    fn branch_name_validation() {
        for good in ["main", "fix/late-fees", "v0.1-wip", "A_b.c"] {
            assert!(validate_branch_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "a..b",
            "a/",
            "sp ace",
            "@",
            &"a".repeat(101),
        ] {
            assert!(validate_branch_name(bad).is_err(), "{bad:?}");
        }
        let hexy: String = "ab".repeat(32);
        assert!(
            validate_branch_name(&hexy).is_err(),
            "64-hex name would shadow ids"
        );
    }

    #[test]
    fn escape_roundtrip_edge_cases() {
        for msg in [
            "",
            "plain",
            "trailing backslash \\",
            "\\n literal",
            "multi\nline\nmsg",
        ] {
            assert_eq!(unescape_line(&escape_line(msg)), msg, "{msg:?}");
        }
    }
}
