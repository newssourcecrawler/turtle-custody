use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use turtle::ledger::{ChecksumAlgo, Ledger};
use turtle::ledger_mmap::{read_payload_range, scan_committed_block_locators_mmap};
use turtle::{block_id_of_bytes, BlockId, BlockIndex, BlockLocator};

const PAYLOAD_LEN: usize = 8 * 1024;
const RANGE_OFFSET: u64 = 2 * 1024;
const RANGE_LEN: u64 = 64;
const MAX_CONTEXT_BYTES: usize = 256;
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

static NEXT_WORLD: AtomicU64 = AtomicU64::new(0);

struct SpillFixture {
    world_dir: PathBuf,
    ledger_path: PathBuf,
    block_id: BlockId,
    locator: BlockLocator,
}

impl SpillFixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let sequence = NEXT_WORLD.fetch_add(1, Ordering::Relaxed);
        let world_dir = std::env::temp_dir().join(format!(
            "turtle_spill_context_{}_{}_{}",
            std::process::id(),
            nonce,
            sequence
        ));
        fs::create_dir_all(&world_dir).expect("create test world");
        let ledger_path = world_dir.join("ledger.log");

        let mut payload = vec![b'x'; PAYLOAD_LEN];
        payload[RANGE_OFFSET as usize - 1] = b'[';
        for (index, byte) in payload
            [RANGE_OFFSET as usize..RANGE_OFFSET as usize + RANGE_LEN as usize]
            .iter_mut()
            .enumerate()
        {
            *byte = b'A' + u8::try_from(index % 26).expect("alphabet index");
        }
        payload[RANGE_OFFSET as usize + RANGE_LEN as usize] = b']';

        let block_id = block_id_of_bytes(&payload);
        let mut ledger = Ledger::open(&ledger_path).expect("open ledger");
        ledger
            .ensure_prealloc_len(128 * 1024)
            .expect("preallocate ledger");
        ledger
            .append_batch(&payload)
            .expect("append canonical payload");

        // The only payload-sized resident allocation is deliberately discarded.
        drop(payload);
        drop(ledger);

        let locators = scan_committed_block_locators_mmap(&ledger_path, MAX_FRAME_LEN)
            .expect("reconstruct committed block locators");
        let mut index = BlockIndex::new();
        index.extend(locators);
        let locator = *index
            .get(&block_id)
            .expect("resolve canonical BlockId to locator");

        Self {
            world_dir,
            ledger_path,
            block_id,
            locator,
        }
    }
}

impl Drop for SpillFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.world_dir);
    }
}

fn block_id_hex(id: &BlockId) -> String {
    let mut out = String::with_capacity(64);
    for byte in id.as_bytes() {
        use std::fmt::Write;
        write!(&mut out, "{byte:02x}").expect("write BlockId hex");
    }
    out
}

fn expect_invalid_input(path: &Path, locator: &BlockLocator, offset: u64, len: u64, max: u64) {
    let error = read_payload_range(path, locator, offset, len, max)
        .expect_err("range request should be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn canonical_spill_recovers_only_a_bounded_range_for_context() {
    let fixture = SpillFixture::new();

    let recovered = read_payload_range(
        &fixture.ledger_path,
        &fixture.locator,
        RANGE_OFFSET,
        RANGE_LEN,
        RANGE_LEN,
    )
    .expect("recover bounded payload range");

    assert_eq!(recovered.len(), RANGE_LEN as usize);
    assert!(recovered
        .iter()
        .enumerate()
        .all(|(index, byte)| *byte == b'A' + u8::try_from(index % 26).unwrap()));
    assert!(!recovered.contains(&b'['));
    assert!(!recovered.contains(&b']'));

    let excerpt = std::str::from_utf8(&recovered).expect("ASCII excerpt");
    let range_end = RANGE_OFFSET.checked_add(RANGE_LEN).expect("range end");
    let context = format!(
        "object=blake3:{}\nrange={}..{}\nexcerpt={}",
        block_id_hex(&fixture.block_id),
        RANGE_OFFSET,
        range_end,
        excerpt
    );

    assert!(context.as_bytes().len() <= MAX_CONTEXT_BYTES);
    assert!(context.contains(&format!("range={}..{}", RANGE_OFFSET, range_end)));
    assert!(context.contains(excerpt));
}

#[test]
fn bounded_read_rejects_range_beginning_outside_payload() {
    let fixture = SpillFixture::new();
    expect_invalid_input(
        &fixture.ledger_path,
        &fixture.locator,
        u64::from(fixture.locator.payload_len) + 1,
        1,
        1,
    );
}

#[test]
fn bounded_read_rejects_range_extending_beyond_payload() {
    let fixture = SpillFixture::new();
    expect_invalid_input(
        &fixture.ledger_path,
        &fixture.locator,
        u64::from(fixture.locator.payload_len) - 32,
        64,
        64,
    );
}

#[test]
fn bounded_read_rejects_request_exceeding_max_output() {
    let fixture = SpillFixture::new();
    expect_invalid_input(&fixture.ledger_path, &fixture.locator, 0, 65, 64);
}

#[test]
fn bounded_read_rejects_checked_range_overflow() {
    let fixture = SpillFixture::new();
    expect_invalid_input(&fixture.ledger_path, &fixture.locator, u64::MAX, 1, 1);
}

#[test]
fn content_locator_scan_excludes_noncanonical_ledger_checksums() {
    let fixture = SpillFixture::new();
    let mut ledger = Ledger::open(&fixture.ledger_path).expect("reopen ledger");
    ledger
        .append_batch_with_checksum(b"crc32 compatibility frame", ChecksumAlgo::Crc32)
        .expect("append CRC32 frame");
    ledger
        .append_batch_with_checksum(b"unhashed frame", ChecksumAlgo::None)
        .expect("append NONE frame");
    drop(ledger);

    let locators = scan_committed_block_locators_mmap(&fixture.ledger_path, MAX_FRAME_LEN)
        .expect("scan canonical block locators");

    assert_eq!(locators.len(), 1);
    assert_eq!(locators[0].0, fixture.block_id);
}
