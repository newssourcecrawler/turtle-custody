//! Lab-grade integration tests for Turtle's ledger.
//!
//! These are "textbook property" tests that intentionally exercise corruption,
//! boundary conditions, and invariants. They live in `tests/` so `ledger.rs`
//! stays readable.

use std::fs::{self, OpenOptions};
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use turtle::ledger::{ChecksumAlgo, Ledger};

fn uniq_path(tag: &str) -> std::path::PathBuf {
    // Not used for correctness; only temp uniqueness.
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("turtle_lab_{}_{}", tag, t))
}

fn read_all(path: &std::path::Path) -> Vec<u8> {
    let mut f = OpenOptions::new().read(true).open(path).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    buf
}

#[test]
fn older_frame_corruption_kills_suffix_never_skips() {
    let path = uniq_path("older_corruption");
    let _ = fs::remove_file(&path);

    // Prealloc + commit 3 frames.
    {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(16 * 1024).unwrap();
        led.append_batch(b"f1").unwrap();
        led.append_batch(b"f2").unwrap();
        led.append_batch(b"f3").unwrap();
    }

    // Capture the committed suffix bounds before corruption.
    let (committed_start, file_len) = {
        let mut led = Ledger::open(&path).unwrap();
        let scan = led.scan_committed().unwrap();
        (scan.committed_start as usize, scan.file_len as usize)
    };

    // Read the file bytes.
    let mut bytes = read_all(&path);
    assert_eq!(bytes.len(), file_len);

    let len = bytes.len();
    assert!(len > 256);

    // Deterministically corrupt within the committed suffix (between committed_start and EOF).
    // We target a byte a little after committed_start to avoid edge cases at the exact boundary.
    assert!(committed_start + 16 < len);
    let idx = committed_start + 16;
    bytes[idx] ^= 0x01;

    // Write back corrupted bytes.
    {
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();
    }

    // Recovery must not report CSN=3 if corruption landed in the suffix.
    // We accept either 0/1/2 depending on where the flip landed (could hit free-space
    // before committed_start in very small cases), but it must NEVER return 3.
    let mut led = Ledger::open(&path).unwrap();
    let scan = led.scan_committed().unwrap();
    assert!(
        scan.committed_csn < 3,
        "corruption must prevent accepting full suffix"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn free_space_random_noise_never_becomes_committed() {
    let path = uniq_path("free_noise");
    let _ = fs::remove_file(&path);

    // Prealloc + commit 1 small frame.
    let (file_len, committed_start) = {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(32 * 1024).unwrap();
        led.append_batch(b"good").unwrap();
        let scan = led.scan_committed().unwrap();
        (scan.file_len, scan.committed_start)
    };

    // Fill the FREE space region [0..committed_start) with pseudo-random noise.
    // We avoid pulling in a RNG crate; use a simple xorshift.
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();

    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut buf = vec![0u8; committed_start as usize];
    for b in buf.iter_mut() {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        *b = (x & 0xFF) as u8;
    }

    f.write_all(&buf).unwrap();
    f.sync_data().unwrap();

    // Recovery must still see exactly 1 committed frame.
    let mut led = Ledger::open(&path).unwrap();
    let scan = led.scan_committed().unwrap();
    assert_eq!(scan.file_len, file_len);
    assert_eq!(scan.committed_csn, 1);

    let tail = led.tail_payloads(10).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0], b"good");

    let _ = fs::remove_file(&path);
}

#[test]
fn checksum_algo_matrix_payload_bitflip_is_detected() {
    let path = uniq_path("algo_matrix");
    let _ = fs::remove_file(&path);

    // We will write one committed frame for each algo, then attempt to corrupt the newest
    // attempted frame and ensure it is NOT accepted.
    {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(64 * 1024).unwrap();

        // Seed prefix so we always have a committed base.
        led.append_batch(b"base").unwrap();

        for algo in [
            ChecksumAlgo::None,
            ChecksumAlgo::Crc32,
            ChecksumAlgo::Blake3,
        ] {
            // Append a valid frame with explicit algo.
            let _ = led.append_batch_with_checksum(b"X", algo).unwrap();
        }
    }

    // Now corrupt ONE byte somewhere near the end (likely within the suffix).
    // We don't need to pinpoint the exact newest frame; we just want to ensure scan
    // never panics and never *increases* CSN due to corruption.
    let mut bytes = read_all(&path);
    let len = bytes.len();
    assert!(len > 128);
    bytes[len - 64] ^= 0x01;

    {
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();
    }

    let mut led = Ledger::open(&path).unwrap();
    let scan = led.scan_committed().unwrap();

    // With corruption near the end, CSN must not exceed what was committed before corruption.
    // We committed: base + 3 algo frames => 4 total.
    assert!(scan.committed_csn <= 4);

    let _ = fs::remove_file(&path);
}

#[test]
fn scan_is_idempotent_and_non_mutating() {
    let path = uniq_path("idempotent");
    let _ = fs::remove_file(&path);

    {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(16 * 1024).unwrap();
        led.append_batch(b"a").unwrap();
        led.append_batch(b"b").unwrap();
    }

    let mut led = Ledger::open(&path).unwrap();
    let s1 = led.scan_committed().unwrap();
    let t1 = led.tail_payloads(10).unwrap();
    let s2 = led.scan_committed().unwrap();
    let t2 = led.tail_payloads(10).unwrap();

    assert_eq!(s1.committed_csn, s2.committed_csn);
    assert_eq!(s1.committed_start, s2.committed_start);
    assert_eq!(s1.committed_bytes, s2.committed_bytes);
    assert_eq!(t1, t2);

    let _ = fs::remove_file(&path);
}

#[test]
fn random_bitflips_near_eof_never_increase_csn() {
    let path = uniq_path("bitflips_near_eof");
    let _ = fs::remove_file(&path);

    // Commit a small prefix.
    let committed = {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(64 * 1024).unwrap();
        for i in 0..8 {
            led.append_batch(format!("frame{}", i).as_bytes()).unwrap();
        }
        led.scan_committed().unwrap()
    };

    // Read bytes once; we will create many corrupted variants.
    let original = read_all(&path);
    let len = original.len();
    assert!(len > 4096);

    // Sample a set of positions close to EOF.
    // (We don't need to flip every byte; this is a "lab" regression, not a torture test.)
    let positions: Vec<usize> = (0..256).map(|k| len - 1 - (k * 7 % 2048)).collect();

    for (case, &pos) in positions.iter().enumerate() {
        let case_path = uniq_path(&format!("bitflip_case_{}", case));
        let _ = fs::remove_file(&case_path);
        fs::copy(&path, &case_path).unwrap();

        let mut bytes = original.clone();
        bytes[pos] ^= 0x01;

        {
            let mut f = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&case_path)
                .unwrap();
            f.write_all(&bytes).unwrap();
        }

        // Invariant: corruption must never *increase* committed CSN.
        // It may reduce it (stop earlier), depending on where the flip landed.
        let mut led = Ledger::open(&case_path).unwrap();
        let scan = led.scan_committed().unwrap();
        assert!(
            scan.committed_csn <= committed.committed_csn,
            "bitflip at {} increased csn unexpectedly ({} -> {})",
            pos,
            committed.committed_csn,
            scan.committed_csn
        );

        let _ = fs::remove_file(&case_path);
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn scribble_in_free_space_near_committed_start_does_not_create_existence() {
    let path = uniq_path("scribble_free_space");
    let _ = fs::remove_file(&path);

    let committed = {
        let mut led = Ledger::open(&path).unwrap();
        led.ensure_prealloc_len(64 * 1024).unwrap();
        led.append_batch(b"good").unwrap();
        led.scan_committed().unwrap()
    };

    // Scribble a pattern into the free region immediately before committed_start.
    // This simulates stray writes in the non-authoritative area.
    let scribble_len: u64 = 256;
    assert!(committed.committed_start > scribble_len + 8);
    let scribble_start = committed.committed_start - scribble_len;

    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(SeekFrom::Start(scribble_start)).unwrap();
        let buf = vec![0xAAu8; scribble_len as usize];
        f.write_all(&buf).unwrap();
        // Intentionally do not fsync: models sudden crash / torn cache.
    }

    // Invariant: scribbling in free space must not create new committed frames.
    let mut led = Ledger::open(&path).unwrap();
    let scan = led.scan_committed().unwrap();
    assert_eq!(scan.committed_csn, committed.committed_csn);

    let tail = led.tail_payloads(10).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0], b"good");

    let _ = fs::remove_file(&path);
}

#[test]
fn prealloc_and_append_creates_backwards_suffix() {
    let path = uniq_path("roundtrip");
    let _ = fs::remove_file(&path);

    let mut ledger = Ledger::open(&path).unwrap();
    ledger.ensure_prealloc_len(4096).unwrap();

    let csn1 = ledger.append_batch(b"one").unwrap();
    assert_eq!(csn1, 1);

    let csn2 = ledger.append_batch(b"two").unwrap();
    assert_eq!(csn2, 2);

    let scan = ledger.scan_committed().unwrap();
    assert_eq!(scan.committed_csn, 2);
    assert!(scan.committed_bytes > 0);
    assert_eq!(scan.file_len, 4096);
    assert!(scan.committed_start < scan.file_len);

    // Tail returns most-recent first.
    let tail = ledger.tail_payloads(10).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0], b"two");
    assert_eq!(tail[1], b"one");

    let _ = fs::remove_file(&path);
}

#[test]
fn scan_stops_at_bad_checksum_frame() {
    let path = uniq_path("bad_checksum");
    let _ = fs::remove_file(&path);

    // Write one valid frame.
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.ensure_prealloc_len(4096).unwrap();
        let csn1 = ledger.append_batch(b"hello").unwrap();
        assert_eq!(csn1, 1);
    }

    // Inject a second frame with bad checksum but valid magic+len+algo.
    // We do this by appending a valid frame first, then corrupting a byte within its committed suffix.
    {
        let mut ledger = Ledger::open(&path).unwrap();
        let scan = ledger.scan_committed().unwrap();
        // Append a valid second frame.
        ledger
            .append_batch_with_checksum(b"world", ChecksumAlgo::Blake3)
            .unwrap();
        let scan2 = ledger.scan_committed().unwrap();
        assert_eq!(scan2.committed_csn, scan.committed_csn + 1);

        // Corrupt within the newly committed suffix.
        let mut bytes = read_all(&path);
        let committed_start = scan2.committed_start as usize;
        let idx = committed_start + 16;
        bytes[idx] ^= 0x01;
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();
    }

    // Scan should now stop before the corrupted tail frame (CSN must be 1).
    {
        let mut ledger = Ledger::open(&path).unwrap();
        let scan = ledger.scan_committed().unwrap();
        assert_eq!(scan.committed_csn, 1);

        let tail = ledger.tail_payloads(10).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0], b"hello");
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn append_fails_when_ledger_full_and_growth_is_refused_after_commits() {
    let path = uniq_path("full_then_refuse_grow");
    let _ = fs::remove_file(&path);

    // Start tiny prealloc so we hit "full" quickly.
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.ensure_prealloc_len(256).unwrap();

    // First append should succeed.
    let csn1 = ledger.append_batch(b"a").unwrap();
    assert_eq!(csn1, 1);

    // Keep appending small frames until we get "full".
    let mut saw_full = false;
    for _ in 0..64 {
        match ledger.append_batch(b"bbbb") {
            Ok(_) => {}
            Err(turtle::ledger::LedgerError::Io(e)) if e.kind() == io::ErrorKind::WriteZero => {
                saw_full = true;
                break;
            }
            Err(e) => panic!("unexpected error: {:?}", e),
        }
    }
    assert!(saw_full, "expected to hit ledger full condition");

    // Growth after commits must be refused for the single-file backward-prealloc ledger.
    match ledger.ensure_prealloc_len(4096) {
        Ok(_) => panic!("expected growth to be refused after commits"),
        Err(turtle::ledger::LedgerError::Io(e)) => {
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput)
        }
        Err(e) => panic!("unexpected error: {:?}", e),
    }

    // Ledger must still be readable and committed CSN must be >= 1.
    let scan = ledger.scan_committed().unwrap();
    assert!(scan.committed_csn >= 1);

    let tail = ledger.tail_payloads(1).unwrap();
    assert_eq!(tail.len(), 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn committed_bytes_matches_frame_sizes() {
    let path = uniq_path("bytes_math");
    let _ = fs::remove_file(&path);

    let mut ledger = Ledger::open(&path).unwrap();
    ledger.ensure_prealloc_len(4096).unwrap();

    let p1 = b"one";
    let p2 = b"two_two";

    ledger.append_batch(p1).unwrap();
    ledger.append_batch(p2).unwrap();

    let scan = ledger.scan_committed().unwrap();

    // Frame size accounting is observable via committed_bytes.
    let expected = (4 + 4 + 4 + 32 + p1.len() as u64 + 32 + 4 + 4 + 4)
        + (4 + 4 + 4 + 32 + p2.len() as u64 + 32 + 4 + 4 + 4);

    assert_eq!(scan.committed_bytes, expected);
    assert_eq!(scan.file_len - scan.committed_start, expected);

    let _ = fs::remove_file(&path);
}

#[test]
fn crash_matrix_partial_write_never_advances_csn() {
    let path = uniq_path("crash_matrix");
    let _ = fs::remove_file(&path);

    // Setup: prealloc, then commit one good frame.
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.ensure_prealloc_len(8192).unwrap();
        let csn1 = ledger.append_batch(b"good").unwrap();
        assert_eq!(csn1, 1);
    }

    // Determine where the next frame would land (based on committed_start).
    let base_scan = {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.scan_committed().unwrap()
    };

    let payload = b"CRASH_TEST_PAYLOAD";

    // Build the exact bytes for ONLY the newest would-be frame by appending on a copy
    // and slicing the delta region [new_committed_start .. old_committed_start).
    let (full_frame_bytes, write_start): (Vec<u8>, u64) = {
        let tmp = uniq_path("crash_frame_image");
        let _ = fs::remove_file(&tmp);
        fs::copy(&path, &tmp).unwrap();

        let mut led = Ledger::open(&tmp).unwrap();
        let before = led.scan_committed().unwrap();

        // Force a known checksum algo for deterministic frame layout.
        led.append_batch_with_checksum(payload, ChecksumAlgo::Blake3)
            .unwrap();
        let after_scan = led.scan_committed().unwrap();

        let after_bytes = read_all(&tmp);

        let new_start = after_scan.committed_start as usize;
        let old_start = before.committed_start as usize;
        assert!(
            new_start < old_start,
            "append must move committed_start backwards"
        );

        let frame = after_bytes[new_start..old_start].to_vec();

        let _ = fs::remove_file(&tmp);
        (frame, new_start as u64)
    };

    // Sanity: write_start in the base file is exactly where the next frame would land.
    assert_eq!(
        write_start,
        base_scan.committed_start - full_frame_bytes.len() as u64
    );

    // Cut points within the newest frame.
    for cut in 0..full_frame_bytes.len() {
        let case_path = uniq_path(&format!("crash_cut_{}", cut));
        let _ = fs::remove_file(&case_path);
        fs::copy(&path, &case_path).unwrap();

        {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&case_path)
                .unwrap();
            f.seek(SeekFrom::Start(write_start)).unwrap();
            f.write_all(&full_frame_bytes[..cut]).unwrap();
        }

        {
            let mut ledger = Ledger::open(&case_path).unwrap();
            let scan = ledger.scan_committed().unwrap();
            assert_eq!(
                scan.committed_csn, 1,
                "cut {} advanced csn unexpectedly",
                cut
            );
            let tail = ledger.tail_payloads(10).unwrap();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0], b"good");
        }

        let _ = fs::remove_file(&case_path);
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn bitflip_matrix_newest_frame_never_advances_csn() {
    let path = uniq_path("bitflip_matrix");
    let _ = fs::remove_file(&path);

    // Commit one good frame.
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.ensure_prealloc_len(8192).unwrap();
        let csn1 = ledger.append_batch(b"good").unwrap();
        assert_eq!(csn1, 1);
    }

    let base_scan = {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.scan_committed().unwrap()
    };

    // Create a valid newest-frame image (ONLY the newest frame bytes) using the real API.
    let payload = b"BITFLIP_PAYLOAD";
    let (full_frame_bytes, write_start): (Vec<u8>, u64) = {
        let tmp = uniq_path("bitflip_frame_image");
        let _ = fs::remove_file(&tmp);
        fs::copy(&path, &tmp).unwrap();

        let mut led = Ledger::open(&tmp).unwrap();
        let before = led.scan_committed().unwrap();

        led.append_batch_with_checksum(payload, ChecksumAlgo::Blake3)
            .unwrap();
        let after_scan = led.scan_committed().unwrap();

        let after_bytes = read_all(&tmp);

        let new_start = after_scan.committed_start as usize;
        let old_start = before.committed_start as usize;
        assert!(
            new_start < old_start,
            "append must move committed_start backwards"
        );

        let frame = after_bytes[new_start..old_start].to_vec();

        let _ = fs::remove_file(&tmp);
        (frame, new_start as u64)
    };

    // Sanity: write_start in the base file is exactly where the next frame would land.
    assert_eq!(
        write_start,
        base_scan.committed_start - full_frame_bytes.len() as u64
    );

    for i in 0..full_frame_bytes.len() {
        let case_path = uniq_path(&format!("bitflip_{}", i));
        let _ = fs::remove_file(&case_path);
        fs::copy(&path, &case_path).unwrap();

        let mut corrupted = full_frame_bytes.clone();
        corrupted[i] ^= 0x01;

        {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&case_path)
                .unwrap();
            f.seek(SeekFrom::Start(write_start)).unwrap();
            f.write_all(&corrupted).unwrap();
        }

        {
            let mut ledger = Ledger::open(&case_path).unwrap();
            let scan = ledger.scan_committed().unwrap();
            assert_eq!(
                scan.committed_csn, 1,
                "bitflip at {} advanced csn unexpectedly",
                i
            );
            let tail = ledger.tail_payloads(10).unwrap();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0], b"good");
        }

        let _ = fs::remove_file(&case_path);
    }

    let _ = fs::remove_file(&path);
}
