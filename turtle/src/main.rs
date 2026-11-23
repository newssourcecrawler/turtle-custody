use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::exit;

mod ledger;
use ledger::{Ledger, LEDGER_FILE_NAME};

// ---- WORLD LAW (FROZEN; treat as ABI) ----
// Turtle defines existence isolation.
//
// Authority:
// - The authoritative world is the committed ledger prefix under <world>/turtle/ledger/.
// - A record exists iff its CSN <= committed CSN derived by scanning valid committed frames.
// - ACK == returning a new CSN. No ACK => no existence.
//
// Commit gate:
// - Commit marker is written last (footer magic published in a separate durable step).
// - Frames lacking the commit marker are treated as mush (non-existent), even if bytes are present.
//
// Durability & recovery:
// - ACKed CSNs are permanent.
// - Recovery scans frames and stops at the first invalid/incomplete frame; no healing or rewriting.
//
// Isolation:
// - Single-writer fence serializes CSN advancement.
// - Readers observe only committed prefixes; readers do not require the writer lock.
//
// Replication:
// - Duplication only (byte-identical prefix streaming). No merge, no consensus.
//
// Time:
// - No authoritative time; timestamps are data only.
//
// Non-authoritative surfaces:
// - <world>/turtle/mush/, <world>/turtle/view/, and <world>/turtle/status are derived/volatile.
// - Deleting them must never delete existence.
//
// -----------------------------------------

fn usage() -> ! {
    eprintln!(
        r#"turtle — existence isolation substrate (Milestone 0)

USAGE:
  turtle init   --dir <path>
  turtle append --dir <path> --record '<json>'
  turtle mush-put --dir <path> --key <k> --data '<bytes>'
  turtle mush-get --dir <path> --key <k>
  turtle mush-ls  --dir <path>
  turtle promote  --dir <path> --kind <k> --name <n> --from-mush <key> [--keep]
  turtle ds-open  --dir <path> --name <DSN> --recfm <RECFM> --lrecl <N>
  turtle ds-write --dir <path> --handle <id> --record '<text>'
  turtle ds-close --dir <path> --handle <id> [--keep]
  turtle ds-cat   --dir <path> --name <DSN> --n <count>
  turtle tail   --dir <path> --n <count>
  turtle verify --dir <path>
  turtle doctor --dir <path>
  turtle status --dir <path>

NOTES:
  - This is a CLI carrier only. No daemon.
  - Authority is the ledger on disk.
  - Views/mush directories exist in M0 but are non-authoritative.
"#
    );
    exit(2);
}

fn main() {
    let mut args = env::args();
    let _exe = args.next();

    let cmd = match args.next() {
        Some(c) => c,
        None => usage(),
    };

    match cmd.as_str() {
        "init" => cmd_init(args.collect()),
        "append" => cmd_append(args.collect()),
        "mush-put" => cmd_mush_put(args.collect()),
        "mush-get" => cmd_mush_get(args.collect()),
        "mush-ls" => cmd_mush_ls(args.collect()),
        "promote" => cmd_promote(args.collect()),
        "ds-open" => cmd_ds_open(args.collect()),
        "ds-write" => cmd_ds_write(args.collect()),
        "ds-close" => cmd_ds_close(args.collect()),
        "ds-cat" => cmd_ds_cat(args.collect()),
        "tail" => cmd_tail(args.collect()),
        "verify" => cmd_verify(args.collect()),
        "doctor" => cmd_doctor(args.collect()),
        "status" => cmd_status(args.collect()),
        _ => usage(),
    }
}

// ---- commands ----

fn cmd_init(args: Vec<String>) {
    let dir = world_dir(&args);

    fs::create_dir_all(&dir).unwrap_or_else(|e| {
        eprintln!("failed to create world dir {}: {}", dir.display(), e);
        exit(1);
    });

    // Linux-style + Plan9-shaped layout (presentation only):
    //   <world>/turtle/ledger/  (authority)
    //   <world>/turtle/mush/    (volatile, non-authoritative)
    //   <world>/turtle/view/    (rebuildable, non-authoritative)
    let turtle_dir = dir.join("turtle");
    let ledger_dir = turtle_dir.join("ledger");
    let mush_dir = turtle_dir.join("mush");
    let view_dir = turtle_dir.join("view");

    fs::create_dir_all(&turtle_dir).unwrap_or_else(|e| {
        eprintln!(
            "failed to create turtle root dir {}: {}",
            turtle_dir.display(),
            e
        );
        exit(1);
    });
    fs::create_dir_all(&ledger_dir).unwrap_or_else(|e| {
        eprintln!(
            "failed to create ledger dir {}: {}",
            ledger_dir.display(),
            e
        );
        exit(1);
    });
    fs::create_dir_all(&mush_dir).unwrap_or_else(|e| {
        eprintln!("failed to create mush dir {}: {}", mush_dir.display(), e);
        exit(1);
    });
    fs::create_dir_all(&view_dir).unwrap_or_else(|e| {
        eprintln!("failed to create view dir {}: {}", view_dir.display(), e);
        exit(1);
    });

    // Best-effort markers (non-authoritative).
    let _ = fs::write(
        mush_dir.join("README"),
        b"NON-AUTHORITATIVE. Volatile working set. Safe to delete.\n",
    );
    let _ = fs::write(
        view_dir.join("README"),
        b"NON-AUTHORITATIVE. Rebuildable views/indexes. Safe to delete.\n",
    );

    let ledger_path = ledger_dir.join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!(
            "failed to create/open ledger {}: {:?}",
            ledger_path.display(),
            e
        );
        exit(1);
    });

    // Preallocate ledger to 256 MiB by default (world-level capacity).
    // This enables backward growth without rewriting existing bytes.
    const DEFAULT_PREALLOC: u64 = 256 * 1024 * 1024;
    ledger
        .ensure_prealloc_len(DEFAULT_PREALLOC)
        .unwrap_or_else(|e| {
            eprintln!(
                "failed to preallocate ledger {}: {:?}",
                ledger_path.display(),
                e
            );
            exit(1);
        });

    let sz = fs::metadata(&ledger_path).map(|m| m.len()).unwrap_or(0);

    println!(
        "initialized world at {} (ledger_path={} prealloc_bytes={})",
        dir.display(),
        ledger_path.display(),
        sz
    );
}

fn cmd_append(args: Vec<String>) {
    let dir = world_dir(&args);
    let record = parse_flag(&args, "--record").unwrap_or_else(|| {
        eprintln!("append requires --record '<json>'");
        usage()
    });

    // Single-writer fence: only one process may advance CSN at a time.
    // Lock lives inside the world directory: <world>/lock/writer.lock/
    let _lock = acquire_writer_lock(&dir).unwrap_or_else(|e| {
        eprintln!("append: could not acquire writer lock: {}", e);
        exit(1);
    });

    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!("failed to open ledger {}: {:?}", ledger_path.display(), e);
        exit(1);
    });

    // Best-effort inflight marker (NOT authoritative): helps operators diagnose crashes.
    // Written only while holding the world writer lock.
    let scan = ledger.scan_committed().unwrap_or_else(|e| {
        eprintln!("append: pre-scan failed: {:?}", e);
        exit(1);
    });

    let inflight_path = dir
        .join("turtle")
        .join("ledger")
        .join("lock")
        .join("inflight.json");
    let inflight_body = format!(
        "{{\"op\":\"append\",\"from_csn\":{},\"from_committed_start\":{},\"file_len\":{}}}\n",
        scan.committed_csn, scan.committed_start, scan.file_len
    );

    // Avoid partial writes: write temp + rename.
    let tmp_path = inflight_path.with_extension("json.tmp");
    fs::write(&tmp_path, inflight_body.as_bytes()).unwrap_or_else(|e| {
        eprintln!(
            "append: failed to write inflight tmp {}: {}",
            tmp_path.display(),
            e
        );
        exit(1);
    });
    fs::rename(&tmp_path, &inflight_path).unwrap_or_else(|e| {
        eprintln!(
            "append: failed to rename inflight marker {}: {}",
            inflight_path.display(),
            e
        );
        exit(1);
    });

    let csn = ledger.append_batch(record.as_bytes()).unwrap_or_else(|e| {
        eprintln!("append failed: {:?}", e);
        exit(1);
    });

    // Clear inflight marker after a successful ack.
    let _ = fs::remove_file(&inflight_path);

    println!("ack csn={}", csn);
}

fn cmd_mush_put(args: Vec<String>) {
    let dir = world_dir(&args);
    let key = parse_flag(&args, "--key").unwrap_or_else(|| {
        eprintln!("mush-put requires --key <k>");
        usage()
    });
    let data = parse_flag(&args, "--data").unwrap_or_else(|| {
        eprintln!("mush-put requires --data '<bytes>'");
        usage()
    });

    // Non-authoritative: write into mush objects.
    let mush_obj_dir = dir.join("turtle").join("mush").join("objects");
    fs::create_dir_all(&mush_obj_dir).unwrap_or_else(|e| {
        eprintln!(
            "mush-put: failed to create mush objects dir {}: {}",
            mush_obj_dir.display(),
            e
        );
        exit(1);
    });

    // Restrict key to a safe file-name subset to avoid path traversal.
    if key.is_empty() || key.contains("/") || key.contains("\\") {
        eprintln!("mush-put: invalid key (must not contain '/' or '\\')");
        exit(1);
    }

    let obj_path = mush_obj_dir.join(&key);
    let tmp_path = obj_path.with_extension("tmp");

    fs::write(&tmp_path, data.as_bytes()).unwrap_or_else(|e| {
        eprintln!(
            "mush-put: failed to write tmp {}: {}",
            tmp_path.display(),
            e
        );
        exit(1);
    });
    fs::rename(&tmp_path, &obj_path).unwrap_or_else(|e| {
        eprintln!("mush-put: failed to rename {}: {}", obj_path.display(), e);
        exit(1);
    });

    println!("mush_ok key={}", key);
}

fn cmd_mush_get(args: Vec<String>) {
    let dir = world_dir(&args);
    let key = parse_flag(&args, "--key").unwrap_or_else(|| {
        eprintln!("mush-get requires --key <k>");
        usage()
    });

    if key.is_empty() || key.contains("/") || key.contains("\\") {
        eprintln!("mush-get: invalid key (must not contain '/' or '\\')");
        exit(1);
    }

    let obj_path = dir.join("turtle").join("mush").join("objects").join(&key);

    let bytes = fs::read(&obj_path).unwrap_or_else(|e| {
        eprintln!("mush-get: failed to read {}: {}", obj_path.display(), e);
        exit(1);
    });

    match std::str::from_utf8(&bytes) {
        Ok(s) => print!("{}", s),
        Err(_) => println!("<{} bytes binary>", bytes.len()),
    }
}

fn cmd_mush_ls(args: Vec<String>) {
    let dir = world_dir(&args);
    let mush_obj_dir = dir.join("turtle").join("mush").join("objects");

    // If objects dir doesn't exist yet, treat as empty.
    let rd = match fs::read_dir(&mush_obj_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!(
                "mush-ls: failed to read dir {}: {}",
                mush_obj_dir.display(),
                e
            );
            exit(1);
        }
    };

    for ent in rd {
        let ent = ent.unwrap_or_else(|e| {
            eprintln!("mush-ls: read_dir entry error: {}", e);
            exit(1);
        });
        let path = ent.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                // Skip non-object markers.
                if name == "README" {
                    continue;
                }
                println!("{}", name);
            }
        }
    }
}

fn cmd_promote(args: Vec<String>) {
    let dir = world_dir(&args);
    let kind = parse_flag(&args, "--kind").unwrap_or_else(|| {
        eprintln!("promote requires --kind <k>");
        usage()
    });
    let name = parse_flag(&args, "--name").unwrap_or_else(|| {
        eprintln!("promote requires --name <n>");
        usage()
    });
    let from_key = parse_flag(&args, "--from-mush").unwrap_or_else(|| {
        eprintln!("promote requires --from-mush <key>");
        usage()
    });
    let keep = has_flag(&args, "--keep");

    // Single-writer fence: only one process may advance CSN at a time.
    let _lock = acquire_writer_lock(&dir).unwrap_or_else(|e| {
        eprintln!("promote: could not acquire writer lock: {}", e);
        exit(1);
    });

    // Read mush object bytes (non-authoritative input).
    let mush_obj_path = dir
        .join("turtle")
        .join("mush")
        .join("objects")
        .join(&from_key);

    let bytes = fs::read(&mush_obj_path).unwrap_or_else(|e| {
        eprintln!(
            "promote: failed to read mush object {}: {}",
            mush_obj_path.display(),
            e
        );
        exit(1);
    });

    // Wrap bytes in a tiny NDJSON envelope (still just bytes in the authoritative ledger).
    // Note: This is not a schema commitment; it's a convenience for carriers.
    let payload = format!(
        "{{\"type\":\"promote\",\"kind\":\"{}\",\"name\":\"{}\",\"bytes_utf8\":{}}}\n",
        escape_json_str(&kind),
        escape_json_str(&name),
        json_string_literal_from_bytes(&bytes)
    );

    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!(
            "promote: failed to open ledger {}: {:?}",
            ledger_path.display(),
            e
        );
        exit(1);
    });

    let csn = ledger.append_batch(payload.as_bytes()).unwrap_or_else(|e| {
        eprintln!("promote failed: {:?}", e);
        exit(1);
    });

    // Best-effort: delete mush object after successful promotion unless --keep is set.
    if !keep {
        let _ = fs::remove_file(&mush_obj_path);
    }

    println!("ack csn={}", csn);
}

fn cmd_tail(args: Vec<String>) {
    let dir = world_dir(&args);
    let n = parse_flag(&args, "--n")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);

    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!("failed to open ledger {}: {:?}", ledger_path.display(), e);
        exit(1);
    });

    // Backward-prealloc ledger returns most-recent first.
    let payloads = ledger.tail_payloads(n).unwrap_or_else(|e| {
        eprintln!("tail failed: {:?}", e);
        exit(1);
    });

    for p in payloads {
        match std::str::from_utf8(&p) {
            Ok(s) => print!("{}", s),
            Err(_) => println!("<{} bytes binary>", p.len()),
        }
    }
}

fn cmd_verify(args: Vec<String>) {
    let dir = world_dir(&args);
    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!("failed to open ledger {}: {:?}", ledger_path.display(), e);
        exit(1);
    });

    let scan = ledger.scan_committed().unwrap_or_else(|e| {
        eprintln!("verify failed: {:?}", e);
        exit(1);
    });

    // Best-effort non-authoritative status surface (filesystem namespace).
    // Status is derived from the ledger; it can be deleted at any time.
    if let Err(e) = write_status_file(&dir, &scan) {
        eprintln!("verify: failed to write status file: {}", e);
    }

    println!(
        "committed_csn={} committed_bytes={} committed_start={} file_len={}",
        scan.committed_csn, scan.committed_bytes, scan.committed_start, scan.file_len
    );
}

fn cmd_doctor(args: Vec<String>) {
    let dir = world_dir(&args);

    let turtle_dir = dir.join("turtle");
    let ledger_dir = turtle_dir.join("ledger");
    let mush_dir = turtle_dir.join("mush");
    let view_dir = turtle_dir.join("view");
    let lock_dir = ledger_dir.join("lock");
    let writer_lock_dir = lock_dir.join("writer.lock");

    let mut ok = true;

    // Layout checks (presentation only).
    if !turtle_dir.is_dir() {
        eprintln!("doctor: missing turtle root dir {}", turtle_dir.display());
        ok = false;
    }
    if !ledger_dir.is_dir() {
        eprintln!("doctor: missing ledger dir {}", ledger_dir.display());
        ok = false;
    }
    if !mush_dir.is_dir() {
        eprintln!("doctor: missing mush dir {}", mush_dir.display());
        ok = false;
    }
    if !view_dir.is_dir() {
        eprintln!("doctor: missing view dir {}", view_dir.display());
        ok = false;
    }

    // Ledger scan (authoritative).
    let ledger_path = ledger_dir.join(LEDGER_FILE_NAME);
    let mut ledger = match Ledger::open(&ledger_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "doctor: failed to open ledger {}: {:?}",
                ledger_path.display(),
                e
            );
            exit(1);
        }
    };

    let scan = ledger.scan_committed().unwrap_or_else(|e| {
        eprintln!("doctor: scan failed: {:?}", e);
        exit(1);
    });

    // Best-effort status update.
    if let Err(e) = write_status_file(&dir, &scan) {
        eprintln!("doctor: failed to write status file: {}", e);
        ok = false;
    }

    // Lock observation (non-authoritative).
    let lock_state = if writer_lock_dir.is_dir() {
        "HELD"
    } else {
        "FREE"
    };

    // Mush objects count (non-authoritative).
    let mush_obj_dir = mush_dir.join("objects");
    let mush_objects = match fs::read_dir(&mush_obj_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n != "README")
                    .unwrap_or(false)
            })
            .count(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            eprintln!(
                "doctor: failed to read mush objects dir {}: {}",
                mush_obj_dir.display(),
                e
            );
            ok = false;
            0
        }
    };

    println!("world_dir={}", dir.display());
    println!("ledger_path={}", ledger_path.display());
    println!("committed_csn={}", scan.committed_csn);
    println!("committed_bytes={}", scan.committed_bytes);
    println!("committed_start={}", scan.committed_start);
    println!("file_len={}", scan.file_len);
    println!("writer_lock={}", lock_state);
    println!("mush_objects={}", mush_objects);
    println!(
        "status_path={}",
        dir.join("turtle").join("status").display()
    );

    if ok {
        println!("doctor_ok");
    } else {
        println!("doctor_warn");
        exit(1);
    }
}

fn cmd_status(args: Vec<String>) {
    let dir = world_dir(&args);
    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!("failed to open ledger {}: {:?}", ledger_path.display(), e);
        exit(1);
    });

    let scan = ledger.scan_committed().unwrap_or_else(|e| {
        eprintln!("status failed: {:?}", e);
        exit(1);
    });

    // Best-effort non-authoritative status surface (filesystem namespace).
    if let Err(e) = write_status_file(&dir, &scan) {
        eprintln!("status: failed to write status file: {}", e);
    }

    println!(
        "committed_csn={} committed_bytes={} committed_start={} file_len={}",
        scan.committed_csn, scan.committed_bytes, scan.committed_start, scan.file_len
    );
}

// ---- helpers ----

fn write_status_file(dir: &PathBuf, scan: &ledger::ScanResult) -> Result<(), String> {
    let status_path = dir.join("turtle").join("status");
    let tmp_path = status_path.with_extension("tmp");

    let body = format!(
        "committed_csn={}\ncommitted_bytes={}\ncommitted_start={}\nfile_len={}\n",
        scan.committed_csn, scan.committed_bytes, scan.committed_start, scan.file_len
    );

    fs::write(&tmp_path, body.as_bytes())
        .map_err(|e| format!("write status tmp {} failed: {}", tmp_path.display(), e))?;

    fs::rename(&tmp_path, &status_path)
        .map_err(|e| format!("rename status {} failed: {}", status_path.display(), e))?;

    Ok(())
}

fn escape_json_str(s: &str) -> String {
    // Minimal JSON string escaping for convenience fields.
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn json_string_literal_from_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => format!("\"{}\"", escape_json_str(s)),
        Err(_) => {
            // If bytes are not valid UTF-8, encode as a JSON array of integers.
            let mut out = String::from("[");
            for (i, b) in bytes.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                out.push_str(&b.to_string());
            }
            out.push(']');
            out
        }
    }
}

fn world_dir(args: &[String]) -> PathBuf {
    let dir = parse_flag(args, "--dir").unwrap_or_else(|| {
        eprintln!("command requires --dir <path>");
        usage()
    });
    PathBuf::from(dir)
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

// ---- single-writer fence (Milestone 0) ----

/// A best-effort lock guard implemented as an atomic directory creation.
///
/// Layout:
///   <world>/turtle/ledger/lock/              (directory)
///   <world>/turtle/ledger/lock/writer.lock/  (directory; exists iff lock is held)
///
/// Semantics:
/// - acquire: create `<world>/lock/writer.lock/` with `create_dir` (atomic).
/// - release: remove the `writer.lock` directory on drop.
/// - crash: lock directory may remain; operator may remove it if they are sure no writer exists.
///
/// This is an advisory single-writer fence that works on macOS and Linux without extra crates.
struct WriterLock {
    path: PathBuf,
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn acquire_writer_lock(world_dir: &PathBuf) -> Result<WriterLock, String> {
    let lock_root = world_dir.join("turtle").join("ledger").join("lock");
    fs::create_dir_all(&lock_root).map_err(|e| format!("create lock dir failed: {}", e))?;

    let lock_path = lock_root.join("writer.lock");

    match fs::create_dir(&lock_path) {
        Ok(()) => Ok(WriterLock { path: lock_path }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "writer lock already held at {}",
            lock_path.display()
        )),
        Err(e) => Err(format!("create writer lock failed: {}", e)),
    }
}

// ---- ds commands ----

fn cmd_ds_open(args: Vec<String>) {
    let dir = world_dir(&args);
    let name = parse_flag(&args, "--name").unwrap_or_else(|| {
        eprintln!("ds-open requires --name <DSN>");
        usage()
    });
    let recfm = parse_flag(&args, "--recfm").unwrap_or_else(|| {
        eprintln!("ds-open requires --recfm <RECFM>");
        usage()
    });
    let lrecl = parse_flag(&args, "--lrecl")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_else(|| {
            eprintln!("ds-open requires --lrecl <N>");
            usage()
        });

    let safe = ds_safe_name(&name);
    let handle = format!("{}.{}", safe, current_pid_string());

    let ds_dir = dir.join("turtle").join("mush").join("ds");
    fs::create_dir_all(&ds_dir).unwrap_or_else(|e| {
        eprintln!(
            "ds-open: failed to create ds dir {}: {}",
            ds_dir.display(),
            e
        );
        exit(1);
    });

    let path = ds_dir.join(format!("{}.jsonl", handle));
    if path.exists() {
        eprintln!("ds-open: handle already exists: {}", path.display());
        exit(1);
    }

    let header = format!(
        "{{\"type\":\"ds_stage\",\"name\":\"{}\",\"recfm\":\"{}\",\"lrecl\":{}}}\n",
        escape_json_str(&name),
        escape_json_str(&recfm),
        lrecl
    );

    fs::write(&path, header.as_bytes()).unwrap_or_else(|e| {
        eprintln!("ds-open: failed to write {}: {}", path.display(), e);
        exit(1);
    });

    println!("handle={}", handle);
}

fn cmd_ds_write(args: Vec<String>) {
    let dir = world_dir(&args);
    let handle = parse_flag(&args, "--handle").unwrap_or_else(|| {
        eprintln!("ds-write requires --handle <id>");
        usage()
    });
    let record = parse_flag(&args, "--record").unwrap_or_else(|| {
        eprintln!("ds-write requires --record '<text>'");
        usage()
    });

    if handle.is_empty() || handle.contains("/") || handle.contains("\\") {
        eprintln!("ds-write: invalid handle");
        exit(1);
    }

    let path = dir
        .join("turtle")
        .join("mush")
        .join("ds")
        .join(format!("{}.jsonl", handle));

    // Require ds-open to have created the stage file.
    if !path.is_file() {
        eprintln!(
            "ds-write: stage file not found (did you ds-open?): {}",
            path.display()
        );
        exit(1);
    }

    // Append one JSON string literal per line.
    let line = format!("{}\n", json_string_literal_from_bytes(record.as_bytes()));

    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| {
            eprintln!("ds-write: open append failed {}: {}", path.display(), e);
            exit(1);
        });

    use std::io::Write;
    f.write_all(line.as_bytes()).unwrap_or_else(|e| {
        eprintln!("ds-write: append failed {}: {}", path.display(), e);
        exit(1);
    });

    println!("ds_ok");
}

fn ds_safe_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

fn current_pid_string() -> String {
    std::process::id().to_string()
}

fn extract_json_field(line: &str, field: &str) -> Option<String> {
    // Extremely small helper: find `"field":"..."` and return the raw value (no unescape).
    let pat = format!("\"{}\":\"", field);
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_json_number_field(line: &str, field: &str) -> Option<String> {
    // Find `"field":123` and return digits.
    let pat = format!("\"{}\":", field);
    let start = line.find(&pat)? + pat.len();
    let rest = line[start..].trim_start();
    let mut end = 0usize;
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

fn extract_records_array(line: &str) -> Option<String> {
    // Best-effort: locate `"records":[ ... ]` and return the bracketed array substring.
    let pat = "\"records\":";
    let start = line.find(pat)? + pat.len();
    let rest = &line[start..];
    let lb = rest.find('[')?;
    let rest2 = &rest[lb..];

    // Walk to matching closing bracket.
    let mut depth = 0i32;
    for (i, ch) in rest2.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest2[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn cmd_ds_close(args: Vec<String>) {
    let dir = world_dir(&args);
    let handle = parse_flag(&args, "--handle").unwrap_or_else(|| {
        eprintln!("ds-close requires --handle <id>");
        usage()
    });
    let keep = has_flag(&args, "--keep");

    if handle.is_empty() || handle.contains("/") || handle.contains("\\") {
        eprintln!("ds-close: invalid handle");
        exit(1);
    }

    // Writer fence: close() == commit.
    let _lock = acquire_writer_lock(&dir).unwrap_or_else(|e| {
        eprintln!("ds-close: could not acquire writer lock: {}", e);
        exit(1);
    });

    let stage_path = dir
        .join("turtle")
        .join("mush")
        .join("ds")
        .join(format!("{}.jsonl", handle));

    let stage = fs::read_to_string(&stage_path).unwrap_or_else(|e| {
        eprintln!(
            "ds-close: failed to read stage {}: {}",
            stage_path.display(),
            e
        );
        exit(1);
    });

    let mut lines = stage.lines();
    let header = lines.next().unwrap_or("");

    // Best-effort parse of header fields (no full JSON parser; keep minimal).
    // Extract name, recfm, lrecl via naive substring search.
    let name = extract_json_field(header, "name").unwrap_or_else(|| {
        eprintln!("ds-close: invalid stage header (missing name)");
        exit(1);
    });
    let recfm = extract_json_field(header, "recfm").unwrap_or_else(|| {
        eprintln!("ds-close: invalid stage header (missing recfm)");
        exit(1);
    });
    let lrecl_str = extract_json_number_field(header, "lrecl").unwrap_or_else(|| {
        eprintln!("ds-close: invalid stage header (missing lrecl)");
        exit(1);
    });
    let lrecl: u32 = lrecl_str.parse().unwrap_or(0);

    // records are already JSON string literals on each line; collect into an array.
    let mut records_json = String::from("[");
    let mut first = true;
    for l in lines {
        if l.trim().is_empty() {
            continue;
        }
        if !first {
            records_json.push(',');
        }
        first = false;
        records_json.push_str(l);
    }
    records_json.push(']');

    let payload = format!(
        "{{\"type\":\"ds_commit\",\"name\":\"{}\",\"recfm\":\"{}\",\"lrecl\":{},\"records\":{}}}\n",
        escape_json_str(&name),
        escape_json_str(&recfm),
        lrecl,
        records_json
    );

    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!(
            "ds-close: failed to open ledger {}: {:?}",
            ledger_path.display(),
            e
        );
        exit(1);
    });

    let csn = ledger.append_batch(payload.as_bytes()).unwrap_or_else(|e| {
        eprintln!("ds-close failed: {:?}", e);
        exit(1);
    });

    if !keep {
        let _ = fs::remove_file(&stage_path);
    }

    println!("ack csn={}", csn);
}

fn cmd_ds_cat(args: Vec<String>) {
    let dir = world_dir(&args);
    let name = parse_flag(&args, "--name").unwrap_or_else(|| {
        eprintln!("ds-cat requires --name <DSN>");
        usage()
    });
    let n = parse_flag(&args, "--n")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10);

    let ledger_path = dir.join("turtle").join("ledger").join(LEDGER_FILE_NAME);
    let mut ledger = Ledger::open(&ledger_path).unwrap_or_else(|e| {
        eprintln!(
            "ds-cat: failed to open ledger {}: {:?}",
            ledger_path.display(),
            e
        );
        exit(1);
    });

    let payloads = ledger.tail_payloads(200).unwrap_or_else(|e| {
        eprintln!("ds-cat: tail failed: {:?}", e);
        exit(1);
    });

    let needle_type = "\"type\":\"ds_commit\"";
    let needle_name = format!("\"name\":\"{}\"", escape_json_str(&name));

    let mut printed = 0usize;
    for p in payloads {
        if printed >= n {
            break;
        }
        let s = match std::str::from_utf8(&p) {
            Ok(s) => s,
            Err(_) => continue,
        };

        for line in s.lines() {
            if printed >= n {
                break;
            }
            if !line.contains(needle_type) || !line.contains(&needle_name) {
                continue;
            }
            if let Some(records) = extract_records_array(line) {
                println!("{}", records);
                printed += 1;
            }
        }
    }
}
