//! Command-line interface: argument parsing, command dispatch, output.
//!
//! Every command takes the database path first; the store location is
//! derived (`<db>.litegraft`) unless `--store` overrides it. All commands
//! that report data accept `--json` so coding agents and scripts can consume
//! results without scraping human output.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::btree::{attribute_pages, PageKind};
use crate::dbfile::{DbFile, PageSource};
use crate::diff::{diff_hashes, diff_sources, owner_label, DiffReport};
use crate::snapshot::{self, guard_sidecars, restore, snap, state_of_file, SnapshotSource};
use crate::store::{default_store_dir, validate_branch_name, Head, Store};

const USAGE: &str = "litegraft — instant branch, snapshot and diff for SQLite database files

USAGE:
    litegraft <COMMAND> <db> [ARGS] [OPTIONS]

COMMANDS:
    init <db>                   Create a snapshot store next to the database
    snap <db> [-m <msg>]        Snapshot the database (page-level dedup)
    log <db>                    Snapshot history of the current branch
    branch <db> [<name>]        List branches, or fork one at the current head
    checkout <db> <ref>         Restore the file from a branch or snapshot
    status <db>                 Working file vs current branch head
    diff <db> [<ref> [<ref>]]   Page diff attributed to tables (default: head vs @)
    tables <db> [<ref>]         Page-ownership breakdown of a state
    verify <db>                 Re-hash every object, validate every manifest
    gc <db>                     Delete objects unreachable from any snapshot

REFS:
    a branch name, a snapshot id (or unique prefix >= 4 hex chars),
    or `@` for the working database file.

OPTIONS:
    --store <dir>        Store directory (default: <db>.litegraft)
    --json               Machine-readable output
    -m, --message <msg>  Snapshot message (snap)
    --at <ref>           Fork point for `branch` (default: current head)
    --allow-wal          Proceed even if -wal holds uncheckpointed frames
    --force              Checkout even if the working state is not snapshotted
    --limit <n>          Maximum entries for `log` (default: all)
    -h, --help           Print this help
    -V, --version        Print version";

/// Parsed command line.
struct Args {
    command: String,
    positional: Vec<String>,
    store: Option<PathBuf>,
    json: bool,
    message: String,
    at: Option<String>,
    allow_wal: bool,
    force: bool,
    limit: Option<usize>,
}

/// Print a line to stdout, tolerating a closed pipe.
///
/// CLI output is routinely piped into `head` or `grep -q`, which close the
/// read end as soon as they are done. `println!` panics on the resulting
/// `EPIPE`; we instead treat a broken pipe as a normal end of output and
/// exit 0, the way coreutils do.
macro_rules! out {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let mut stdout = ::std::io::stdout().lock();
        if let Err(e) = writeln!(stdout, $($arg)*) {
            if e.kind() == ::std::io::ErrorKind::BrokenPipe {
                ::std::process::exit(0);
            }
            panic!("failed printing to stdout: {e}");
        }
    }};
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        command: String::new(),
        positional: Vec::new(),
        store: None,
        json: false,
        message: String::new(),
        at: None,
        allow_wal: false,
        force: false,
        limit: None,
    };
    let mut it = argv.iter().peekable();
    let take_value = |it: &mut std::iter::Peekable<std::slice::Iter<String>>, flag: &str| {
        it.next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()), // caller prints usage, exit 0
            "-V" | "--version" => {
                args.command = "version".into();
                return Ok(args);
            }
            "--json" => args.json = true,
            "--allow-wal" => args.allow_wal = true,
            "--force" => args.force = true,
            "-m" | "--message" => args.message = take_value(&mut it, arg)?,
            "--store" => args.store = Some(PathBuf::from(take_value(&mut it, arg)?)),
            "--at" => args.at = Some(take_value(&mut it, arg)?),
            "--limit" => {
                let v = take_value(&mut it, arg)?;
                args.limit = Some(
                    v.parse()
                        .map_err(|_| format!("--limit: not a number: {v}"))?,
                );
            }
            s if s.starts_with('-')
                && s != "-"
                && s.len() > 1
                && !s.chars().nth(1).unwrap().is_ascii_digit() =>
            {
                return Err(format!("unknown option {s}"));
            }
            _ => {
                if args.command.is_empty() {
                    args.command = arg.clone();
                } else {
                    args.positional.push(arg.clone());
                }
            }
        }
    }
    if args.command.is_empty() {
        return Err("no command given".into());
    }
    Ok(args)
}

/// Entry point; returns the process exit code.
pub fn run(argv: Vec<String>) -> i32 {
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(msg) if msg.is_empty() => {
            out!("{USAGE}");
            return 0;
        }
        Err(msg) => {
            eprintln!("litegraft: {msg}");
            eprintln!("run `litegraft --help` for usage");
            return 2;
        }
    };
    if args.command == "version" {
        out!("litegraft {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let Some(db) = args.positional.first().cloned() else {
        eprintln!("litegraft {}: a database path is required", args.command);
        return 2;
    };
    let db = PathBuf::from(db);
    let store_dir = args.store.clone().unwrap_or_else(|| default_store_dir(&db));

    let result = match args.command.as_str() {
        "init" => cmd_init(&db, &store_dir),
        "snap" => cmd_snap(&db, &store_dir, &args),
        "log" => cmd_log(&store_dir, &args),
        "branch" => cmd_branch(&db, &store_dir, &args),
        "checkout" => cmd_checkout(&db, &store_dir, &args),
        "status" => cmd_status(&db, &store_dir, &args),
        "diff" => cmd_diff(&db, &store_dir, &args),
        "tables" => cmd_tables(&db, &store_dir, &args),
        "verify" => cmd_verify(&store_dir, &args),
        "gc" => cmd_gc(&store_dir, &args),
        other => {
            eprintln!("litegraft: unknown command {other:?}");
            eprintln!("run `litegraft --help` for usage");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("litegraft {}: {e}", args.command);
            1
        }
    }
}

// ---- commands ---------------------------------------------------------------

fn cmd_init(db: &Path, store_dir: &Path) -> io::Result<i32> {
    let file = DbFile::open(db)?; // validate it really is a SQLite database
    let pages = file.page_count();
    let page_size = file.page_size();
    drop(file);
    Store::init(store_dir)?;
    out!(
        "initialized empty store at {} (branch main; {pages} pages x {page_size} bytes tracked)",
        store_dir.display()
    );
    out!("next: litegraft snap {} -m \"baseline\"", db.display());
    Ok(0)
}

fn cmd_snap(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let out = snap(&store, db, &args.message, args.allow_wal)?;
    if args.json {
        out!(
            "{{\"id\":{},\"branch\":{},\"pages\":{},\"new_objects\":{},\"dedup_pages\":{},\"bytes_written\":{},\"elapsed_ms\":{:.2},\"changed\":{}}}",
            json_str(&out.id),
            json_str(&out.branch),
            out.pages,
            out.new_objects,
            out.dedup_pages,
            out.bytes_written,
            out.elapsed_ms,
            out.changed
        );
    } else if out.changed {
        out!("snap {} (branch {})", short(&out.id), out.branch);
        out!(
            "  pages: {} total, {} new, {} deduped ({} written) in {:.1} ms",
            out.pages,
            out.new_objects,
            out.dedup_pages,
            human_bytes(out.bytes_written),
            out.elapsed_ms
        );
    } else {
        out!(
            "no page changes; {} stays at {}",
            out.branch,
            short(&out.id)
        );
    }
    Ok(0)
}

fn cmd_log(store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let mut cursor = match store.head()? {
        Head::Branch(name) => store.branch_head(&name)?,
        Head::Detached(id) => Some(id),
    };
    let mut entries = Vec::new();
    while let Some(id) = cursor {
        let m = store.read_manifest(&id)?;
        cursor = m.parent.clone();
        entries.push(m);
        if let Some(limit) = args.limit {
            if entries.len() >= limit {
                break;
            }
        }
    }
    if args.json {
        let items: Vec<String> = entries
            .iter()
            .map(|m| {
                format!(
                    "{{\"id\":{},\"parent\":{},\"branch\":{},\"created\":{},\"created_utc\":{},\"pages\":{},\"message\":{}}}",
                    json_str(&m.id),
                    m.parent.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
                    json_str(&m.branch),
                    m.created,
                    json_str(&format_utc(m.created)),
                    m.page_count(),
                    json_str(&m.message)
                )
            })
            .collect();
        out!("[{}]", items.join(","));
        return Ok(0);
    }
    if entries.is_empty() {
        out!("no snapshots yet (run `litegraft snap <db>`)");
        return Ok(0);
    }
    for m in &entries {
        out!(
            "snap {}  {}  {:>5} pages  {}",
            short(&m.id),
            format_utc(m.created),
            m.page_count(),
            if m.message.is_empty() {
                "(no message)"
            } else {
                &m.message
            }
        );
    }
    Ok(0)
}

fn cmd_branch(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let head = store.head()?;
    match args.positional.get(1) {
        None => {
            // List mode.
            let branches = store.list_branches()?;
            if branches.is_empty() {
                out!("no branches with snapshots yet (run `litegraft snap <db>`)");
                return Ok(0);
            }
            for (name, id) in &branches {
                let marker = match &head {
                    Head::Branch(current) if current == name => "*",
                    _ => " ",
                };
                out!("{marker} {name:<24} {}", short(id));
            }
            if let Head::Detached(id) = &head {
                out!("! HEAD detached at {}", short(id));
            }
            Ok(0)
        }
        Some(name) => {
            validate_branch_name(name)?;
            if store.branch_exists(name) {
                return Err(other(format!("branch {name:?} already exists")));
            }
            let at = match &args.at {
                Some(r) => store.resolve(r)?,
                None => match &head {
                    Head::Branch(current) => store.branch_head(current)?.ok_or_else(|| {
                        other(format!(
                            "branch {current:?} has no snapshots yet; snap first or use --at <ref>"
                        ))
                    })?,
                    Head::Detached(id) => id.clone(),
                },
            };
            store.set_branch(name, &at)?;
            out!("branch {name} created at {}", short(&at));
            out!("switch with: litegraft checkout {} {name}", db.display());
            Ok(0)
        }
    }
}

fn cmd_checkout(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let Some(refname) = args.positional.get(1) else {
        return Err(other(
            "checkout needs a ref (branch, snapshot id, or prefix)",
        ));
    };
    let target = store.resolve(refname)?;

    // Safety: never silently discard a working state that exists nowhere in
    // the store.
    if db.exists() && !args.force {
        let file_state = state_of_file(db)?;
        guard_sidecars(db, file_state.page_size, false).map_err(|e| {
            other(format!(
                "{e} (or pass --force to discard the working state)"
            ))
        })?;
        if !store.has_snapshot(&file_state.id) {
            return Err(other(format!(
                "working file has un-snapshotted changes (state {}); \
                 run `litegraft snap {}` first, or pass --force to discard them",
                short(&file_state.id),
                db.display()
            )));
        }
    }

    restore(&store, &target, db)?;
    let manifest = store.read_manifest(&target)?;
    if store.branch_exists(refname) {
        store.set_head(&Head::Branch(refname.clone()))?;
        out!(
            "checked out branch {refname} at {} ({} pages restored)",
            short(&target),
            manifest.page_count()
        );
    } else {
        store.set_head(&Head::Detached(target.clone()))?;
        out!(
            "checked out snapshot {} ({} pages restored); HEAD is detached — \
             `litegraft branch {} <name>` to keep working here",
            short(&target),
            manifest.page_count(),
            db.display()
        );
    }
    Ok(0)
}

fn cmd_status(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let head = store.head()?;
    let (branch_label, head_id) = match &head {
        Head::Branch(name) => (name.clone(), store.branch_head(name)?),
        Head::Detached(id) => ("(detached)".to_string(), Some(id.clone())),
    };
    let file_state = state_of_file(db)?;
    guard_sidecars(db, file_state.page_size, args.allow_wal)?;

    let (dirty, pages_changed, head_short, message) = match &head_id {
        Some(id) => {
            let m = store.read_manifest(id)?;
            let changed = diff_hashes(&m.page_hashes, &file_state.page_hashes).len() as u32;
            (file_state.id != *id, changed, short(id), m.message)
        }
        None => (
            true,
            file_state.page_hashes.len() as u32,
            "-".to_string(),
            String::new(),
        ),
    };

    if args.json {
        out!(
            "{{\"db\":{},\"branch\":{},\"head\":{},\"pages\":{},\"page_size\":{},\"dirty\":{},\"pages_changed\":{}}}",
            json_str(&db.display().to_string()),
            json_str(&branch_label),
            head_id.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
            file_state.page_hashes.len(),
            file_state.page_size,
            dirty,
            pages_changed
        );
        return Ok(0);
    }
    out!(
        "db:      {} ({} pages x {} bytes)",
        db.display(),
        file_state.page_hashes.len(),
        file_state.page_size
    );
    out!("branch:  {branch_label}");
    match head_id {
        Some(_) => out!("head:    {head_short} {message:?}"),
        None => out!("head:    (no snapshots yet)"),
    }
    if dirty {
        out!("state:   dirty ({pages_changed} page(s) differ; see `litegraft diff`)");
    } else {
        out!("state:   clean");
    }
    Ok(0)
}

/// One side of a diff: either the working file or a snapshot.
enum Side {
    Working,
    Snap(String),
}

impl Side {
    fn parse(store: &Store, name: &str) -> io::Result<Side> {
        if name == "@" {
            Ok(Side::Working)
        } else {
            Ok(Side::Snap(store.resolve(name)?))
        }
    }
    fn label(&self) -> String {
        match self {
            Side::Working => "@ (working file)".to_string(),
            Side::Snap(id) => short(id),
        }
    }
}

fn cmd_diff(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let refs = &args.positional[1..];
    let (old_side, new_side) = match refs {
        [] => {
            let head_id = match store.head()? {
                Head::Branch(name) => store.branch_head(&name)?.ok_or_else(|| {
                    other(format!(
                        "branch {name:?} has no snapshots yet; nothing to diff against"
                    ))
                })?,
                Head::Detached(id) => id,
            };
            (Side::Snap(head_id), Side::Working)
        }
        [a] => (Side::parse(&store, a)?, Side::Working),
        [a, b] => (Side::parse(&store, a)?, Side::parse(&store, b)?),
        _ => return Err(other("diff takes at most two refs")),
    };

    let (old_hashes, mut old_src) = open_side(&store, db, args.allow_wal, &old_side)?;
    let (new_hashes, mut new_src) = open_side(&store, db, args.allow_wal, &new_side)?;
    let report = diff_sources(
        old_src.as_source(),
        new_src.as_source(),
        &old_hashes,
        &new_hashes,
    )?;

    if args.json {
        print_diff_json(&old_side, &new_side, &report);
        return Ok(0);
    }
    out!("diff {} -> {}", old_side.label(), new_side.label());
    if report.is_empty() {
        out!("  no page-level differences ({} pages)", report.new_pages);
        return Ok(0);
    }
    let width = report
        .by_owner
        .iter()
        .map(|o| o.owner.len())
        .max()
        .unwrap_or(0);
    for o in &report.by_owner {
        let mut parts = Vec::new();
        if o.changed > 0 {
            parts.push(format!("{} changed", o.changed));
        }
        if o.added > 0 {
            parts.push(format!("{} added", o.added));
        }
        if o.removed > 0 {
            parts.push(format!("{} removed", o.removed));
        }
        out!("  {:<width$}  {}", o.owner, parts.join(", "), width = width);
    }
    out!(
        "{} changed, {} added, {} removed ({} -> {} pages x {} bytes)",
        report.total_changed(),
        report.total_added(),
        report.total_removed(),
        report.old_pages,
        report.new_pages,
        report.page_size
    );
    Ok(0)
}

fn print_diff_json(old_side: &Side, new_side: &Side, report: &DiffReport) {
    let side_json = |s: &Side| match s {
        Side::Working => "\"@\"".to_string(),
        Side::Snap(id) => json_str(id),
    };
    let owners: Vec<String> = report
        .by_owner
        .iter()
        .map(|o| {
            format!(
                "{{\"owner\":{},\"changed\":{},\"added\":{},\"removed\":{}}}",
                json_str(&o.owner),
                o.changed,
                o.added,
                o.removed
            )
        })
        .collect();
    let pages: Vec<String> = report
        .deltas
        .iter()
        .map(|d| {
            format!(
                "{{\"pgno\":{},\"change\":{}}}",
                d.pgno,
                json_str(d.change.label())
            )
        })
        .collect();
    out!(
        "{{\"old\":{},\"new\":{},\"page_size\":{},\"changed\":{},\"added\":{},\"removed\":{},\"owners\":[{}],\"pages\":[{}]}}",
        side_json(old_side),
        side_json(new_side),
        report.page_size,
        report.total_changed(),
        report.total_added(),
        report.total_removed(),
        owners.join(","),
        pages.join(",")
    );
}

fn cmd_tables(db: &Path, store_dir: &Path, args: &Args) -> io::Result<i32> {
    // `tables` works without a store when inspecting the working file, so a
    // missing store is only an error for snapshot refs.
    let refname = args.positional.get(1).map(String::as_str).unwrap_or("@");
    if refname == "@" {
        let mut file = DbFile::open(db)?;
        guard_sidecars(db, file.page_size(), args.allow_wal)?;
        print_tables(&mut file, args, "@ (working file)")
    } else {
        let store = Store::open(store_dir)?;
        let id = store.resolve(refname)?;
        let mut src = SnapshotSource::open(&store, &id)?;
        let label = short(&id);
        print_tables(&mut src, args, &label)
    }
}

fn print_tables(src: &mut dyn PageSource, args: &Args, label: &str) -> io::Result<i32> {
    let page_size = src.page_size();
    let map = attribute_pages(src)?;

    // Group pages by display label.
    #[derive(Default)]
    struct Row {
        btree: u32,
        overflow: u32,
        other: u32,
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for owner in map.values() {
        let row = rows.entry(owner_label(owner)).or_default();
        match owner.kind {
            PageKind::BTree => row.btree += 1,
            PageKind::Overflow => row.overflow += 1,
            _ => row.other += 1,
        }
    }
    let mut labels: Vec<&String> = rows.keys().collect();
    labels.sort_by_key(|l| (l.starts_with('('), l.as_str()));

    if args.json {
        let items: Vec<String> = labels
            .iter()
            .map(|label| {
                let r = &rows[*label];
                let total = r.btree + r.overflow + r.other;
                format!(
                    "{{\"object\":{},\"pages\":{},\"btree\":{},\"overflow\":{},\"bytes\":{}}}",
                    json_str(label),
                    total,
                    r.btree,
                    r.overflow,
                    total as u64 * page_size as u64
                )
            })
            .collect();
        out!("[{}]", items.join(","));
        return Ok(0);
    }
    out!(
        "{:<28} {:>6} {:>6} {:>9} {:>10}",
        "OBJECT",
        "PAGES",
        "BTREE",
        "OVERFLOW",
        "BYTES"
    );
    for label in labels {
        let r = &rows[label];
        let total = r.btree + r.overflow + r.other;
        out!(
            "{label:<28} {total:>6} {:>6} {:>9} {:>10}",
            r.btree,
            r.overflow,
            human_bytes(total as u64 * page_size as u64)
        );
    }
    out!("{} pages x {} bytes in {}", map.len(), page_size, label);
    Ok(0)
}

/// Open one diff side as (page hashes, an owned PageSource).
fn open_side<'a>(
    store: &'a Store,
    db: &Path,
    allow_wal: bool,
    side: &Side,
) -> io::Result<(Vec<String>, Box<dyn SourceHolder + 'a>)> {
    match side {
        Side::Working => {
            let st = state_of_file(db)?;
            guard_sidecars(db, st.page_size, allow_wal)?;
            Ok((st.page_hashes, Box::new(FileHolder(DbFile::open(db)?))))
        }
        Side::Snap(id) => {
            let src = SnapshotSource::open(store, id)?;
            let hashes = src.manifest().page_hashes.clone();
            Ok((hashes, Box::new(SnapHolder(src))))
        }
    }
}

fn cmd_verify(store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let mut corrupt = 0u32;
    let objects = store.list_objects()?;
    for hash in &objects {
        let bytes = store.read_object(hash)?;
        if crate::store::page_hash(&bytes) != *hash {
            eprintln!("verify: object {hash} content does not match its hash");
            corrupt += 1;
        }
    }
    let snaps = store.list_snapshots()?;
    let mut missing = 0u32;
    for id in &snaps {
        match store.read_manifest(id) {
            Err(e) => {
                eprintln!("verify: snapshot {id}: {e}");
                missing += 1;
            }
            Ok(m) => {
                for hash in &m.page_hashes {
                    if !store.has_object(hash) {
                        eprintln!(
                            "verify: snapshot {} is missing page object {hash}",
                            short(id)
                        );
                        missing += 1;
                    }
                }
            }
        }
    }
    let mut bad_refs = 0u32;
    for (name, id) in store.list_branches()? {
        if !store.has_snapshot(&id) {
            eprintln!(
                "verify: branch {name} points at missing snapshot {}",
                short(&id)
            );
            bad_refs += 1;
        }
    }
    let ok = corrupt == 0 && missing == 0 && bad_refs == 0;
    if args.json {
        out!(
            "{{\"objects\":{},\"corrupt\":{},\"snapshots\":{},\"missing\":{},\"bad_refs\":{},\"ok\":{}}}",
            objects.len(),
            corrupt,
            snaps.len(),
            missing,
            bad_refs,
            ok
        );
    } else {
        out!(
            "objects: {} checked, {corrupt} corrupt; snapshots: {} checked, {missing} problem(s); refs: {bad_refs} broken",
            objects.len(),
            snaps.len()
        );
        out!("{}", if ok { "verify OK" } else { "verify FAILED" });
    }
    Ok(if ok { 0 } else { 1 })
}

fn cmd_gc(store_dir: &Path, args: &Args) -> io::Result<i32> {
    let store = Store::open(store_dir)?;
    let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for id in store.list_snapshots()? {
        for hash in store.read_manifest(&id)?.page_hashes {
            live.insert(hash);
        }
    }
    let mut removed = 0u32;
    let mut bytes_freed = 0u64;
    let mut kept = 0u32;
    for hash in store.list_objects()? {
        if live.contains(&hash) {
            kept += 1;
        } else {
            bytes_freed += store.read_object(&hash)?.len() as u64;
            store.remove_object(&hash)?;
            removed += 1;
        }
    }
    if args.json {
        out!("{{\"removed\":{removed},\"bytes_freed\":{bytes_freed},\"kept\":{kept}}}");
    } else {
        out!(
            "gc: removed {removed} unreachable object(s) ({}), kept {kept}",
            human_bytes(bytes_freed)
        );
    }
    Ok(0)
}

// ---- PageSource plumbing ------------------------------------------------------

/// Owns either a live file or a snapshot view and lends it as a PageSource.
trait SourceHolder {
    fn as_source(&mut self) -> &mut dyn PageSource;
}

struct FileHolder(DbFile);
impl SourceHolder for FileHolder {
    fn as_source(&mut self) -> &mut dyn PageSource {
        &mut self.0
    }
}

struct SnapHolder<'a>(snapshot::SnapshotSource<'a>);
impl SourceHolder for SnapHolder<'_> {
    fn as_source(&mut self) -> &mut dyn PageSource {
        &mut self.0
    }
}

// ---- helpers ------------------------------------------------------------------

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Abbreviated snapshot id for human output.
pub fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// JSON string literal with the required escapes.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `12.3 KiB`-style byte counts for human output.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a unix timestamp as `YYYY-MM-DD HH:MM:SS UTC` (proleptic Gregorian,
/// via the classic days-from-civil inverse).
pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days (Hinnant): shift epoch to 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Args, String> {
        parse_args(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_command_db_and_flags_in_any_order() {
        let a = parse(&["snap", "app.db", "--json", "-m", "seed rows"]).unwrap();
        assert_eq!(a.command, "snap");
        assert_eq!(a.positional, vec!["app.db"]);
        assert!(a.json);
        assert_eq!(a.message, "seed rows");
        let b = parse(&["--json", "diff", "app.db", "main", "@"]).unwrap();
        assert_eq!(b.command, "diff");
        assert_eq!(b.positional, vec!["app.db", "main", "@"]);
    }

    #[test]
    fn missing_or_malformed_flag_values_are_usage_errors() {
        assert!(parse(&["snap", "app.db", "-m"]).is_err());
        assert!(parse(&["log", "app.db", "--limit"]).is_err());
        assert!(parse(&["log", "app.db", "--limit", "ten"]).is_err());
        assert_eq!(
            parse(&["log", "app.db", "--limit", "3"]).unwrap().limit,
            Some(3)
        );
    }

    #[test]
    fn unknown_option_is_rejected_but_at_ref_is_positional() {
        assert!(parse(&["snap", "app.db", "--frobnicate"]).is_err());
        // `@` must survive as a positional ref, not be treated as a flag.
        let a = parse(&["diff", "app.db", "@"]).unwrap();
        assert_eq!(a.positional, vec!["app.db", "@"]);
    }

    #[test]
    fn help_is_signalled_with_empty_error() {
        assert!(matches!(parse(&["--help"]), Err(msg) if msg.is_empty()));
    }

    #[test]
    fn json_str_escapes_quotes_backslashes_and_control_chars() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_str("line\nnext\ttab"), "\"line\\nnext\\ttab\"");
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(4096), "4.0 KiB");
        assert_eq!(human_bytes(1_572_864), "1.5 MiB");
    }

    #[test]
    fn format_utc_known_timestamps() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00 UTC"); // leap day
        assert_eq!(format_utc(1_783_872_540), "2026-07-12 16:09:00 UTC");
    }

    #[test]
    fn short_id_is_twelve_chars() {
        assert_eq!(short(&"ab".repeat(32)), "abababababab");
        assert_eq!(short("abcd"), "abcd", "short input passes through");
    }
}
