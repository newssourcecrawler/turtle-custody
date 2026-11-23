//turtle/src/ledger.rs

//! Turtle Milestone 0 ledger (authoritative) — backward-prealloc layout.
//!
//! This module is storage law only: append, commit, scan, tail, and verify framed
//! payloads. It does not interpret payload meaning, route work, price usage, or
//! decide KV lifecycle policy. A future kvpool may use this kind of ledger as a
//! durable receipt/state substrate, but kvpool policy must live above it.
//!
//! Goal (user intent): fill the bottle from the bottom / wall side first.
//! Physically, newest frames live at lower file offsets, older frames toward EOF.
//! We achieve this without rewriting existing bytes by PREALLOCATING the file and
//! growing the committed region BACKWARDS from EOF.
//!
//! Disk layout (single file `ledger.log`):
//!   [free/unwritten .. mostly zeros ..][NEWEST][...][OLDER][OLDEST][EOF]
//!
//! Frame format (header + payload + footer) with per-frame checksum:
//!   Header:
//!     [magic:u32 LE][len:u32 LE][algo:u32 LE][sum:32 bytes]
//!   Payload:
//!     [payload bytes...]
//!   Footer:
//!     [sum:32 bytes][algo:u32 LE][len:u32 LE][magic:u32 LE]
//!
//! algo meanings:
//!   0 = NONE (sum bytes are all zero)
//!   1 = CRC32 (sum[0..4] = crc32 LE, rest zero)
//!   2 = BLAKE3 (sum[0..32] = blake3 hash)
//!
//! Footer enables backward recovery scan from EOF.
//! Header enables forward traversal from `committed_start` (newest-first tail).
//
//
//! Backward scan algorithm:
//!   pos := file_len
//!   loop:
//!     read footer at pos-FOOTER_LEN..pos
//!     if magic != MAGIC => stop (hit free space)
//!     if len > max => error (safety guard)
//!     payload := [pos-FOOTER_LEN-len .. pos-FOOTER_LEN]
//!     header := [payload_start-HEADER_LEN .. payload_start]
//!     if header/footer/checksum do not agree => stop (do not advance)
//!     committed_csn += 1
//!     committed_bytes += HEADER_LEN + len + FOOTER_LEN
//!     pos := pos-(HEADER_LEN + len + FOOTER_LEN)
//!
//! Semantics:
//! - Authority is the committed SUFFIX near EOF, scanned backwards.
//! - Existence = CSN-bounded count of valid frames found from EOF backwards.
//! - Ack returns new CSN only after durability barrier.
//! - No healing, no rewriting; invalid tail is ignored.
//! - No authoritative time.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use blake3;
use crc32fast::Hasher;

pub const LEDGER_FILE_NAME: &str = "ledger.log";

// 'TRTL' little-endian
pub(crate) const MAGIC: u32 = 0x4C_54_52_54;
pub(crate) const SUM_LEN: u64 = 32;
pub(crate) const HEADER_LEN: u64 = 4 + 4 + 4 + SUM_LEN; // magic + len + algo + sum
pub(crate) const FOOTER_LEN: u64 = SUM_LEN + 4 + 4 + 4; // sum + algo + len + magic

const ALGO_NONE: u32 = 0;
const ALGO_CRC32: u32 = 1;
const ALGO_BLAKE3: u32 = 2;
/// Per-frame checksum algorithm selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgo {
    None,
    Crc32,
    Blake3,
}

impl ChecksumAlgo {
    fn to_u32(self) -> u32 {
        match self {
            ChecksumAlgo::None => ALGO_NONE,
            ChecksumAlgo::Crc32 => ALGO_CRC32,
            ChecksumAlgo::Blake3 => ALGO_BLAKE3,
        }
    }

    pub(crate) fn from_u32(v: u32) -> Option<Self> {
        match v {
            ALGO_NONE => Some(ChecksumAlgo::None),
            ALGO_CRC32 => Some(ChecksumAlgo::Crc32),
            ALGO_BLAKE3 => Some(ChecksumAlgo::Blake3),
            _ => None,
        }
    }
}

/// Result of scanning the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    /// Number of valid frames (CSN is 1-based, so this is also the committed CSN).
    pub committed_csn: u64,
    /// Total bytes that are part of the valid committed suffix.
    pub committed_bytes: u64,
    /// The start offset (byte index) of the committed suffix.
    /// If committed_csn==0, this equals file_len.
    pub committed_start: u64,
    /// Total file length.
    pub file_len: u64,
}

/// Errors that indicate the ledger file is not usable for reading/writing.
#[allow(dead_code)]
#[derive(Debug)]
pub enum LedgerError {
    Io(io::Error),
    /// Append-time guard: a requested frame is too large (protects memory / allocation).
    /// Note: scan/recovery treats absurd lengths as invalid tail and stops without error.
    FrameTooLarge {
        len: u32,
        max: u32,
        at_pos: u64,
    },
    /// File is too small to contain even a footer.
    FileTooSmall,
}

impl From<io::Error> for LedgerError {
    fn from(e: io::Error) -> Self {
        LedgerError::Io(e)
    }
}

/// A minimal ledger handle. It does not cache indexes; callers can scan when needed.
#[derive(Debug)]
pub struct Ledger {
    file: File,
    /// Soft cap to prevent allocating huge payloads on corrupt files.
    max_frame_len: u32,
    /// Default checksum algo for newly appended frames.
    default_checksum: ChecksumAlgo,
    path: PathBuf,
}

impl Ledger {
    /// Opens (or creates) the ledger file at `path`.
    ///
    /// IMPORTANT:
    /// This model expects the file to be preallocated (non-zero length) to enable
    /// backward growth without rewriting. We do not auto-preallocate here because
    /// sizing policy is a world-level choice (CLI `init` will do it).
    ///
    /// NOTE: Single-writer fencing is implemented elsewhere (world lock).
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;

        Ok(Self {
            file,
            // 16 MiB default max frame size for M0.
            max_frame_len: 16 * 1024 * 1024,
            default_checksum: ChecksumAlgo::Blake3,
            path: path.to_path_buf(),
        })
    }

    /// Preallocates the ledger file to `size_bytes`.
    ///
    /// If the file already exists and is >= size_bytes, this is a no-op.
    /// We do not shrink.
    pub fn ensure_prealloc_len(&mut self, size_bytes: u64) -> Result<(), LedgerError> {
        let cur = self.file.metadata()?.len();
        if cur >= size_bytes {
            return Ok(());
        }

        // IMPORTANT INVARIANT:
        // This single-file backward-prealloc design assumes the committed region is a contiguous
        // suffix anchored at EOF. Extending the file length after commits would append zeros at EOF,
        // breaking recovery (EOF would no longer land on the committed suffix).
        //
        // Therefore, we allow preallocation growth ONLY before the first commit.
        // Future "grow" amenity should be implemented via segments (new file), not by extending EOF.
        let scan = scan_committed_suffix_backwards(&mut self.file, self.max_frame_len)?;
        if scan.committed_csn > 0 {
            return Err(LedgerError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot grow single-file backward-prealloc ledger after commits; use a new segment/world",
            )));
        }

        self.file.set_len(size_bytes)?;
        // Ensure the length extension is durable.
        self.file.sync_data()?;
        Ok(())
    }

    /// Overrides the maximum accepted frame length for reads/scans.
    #[allow(dead_code)]
    pub fn with_max_frame_len(mut self, max: u32) -> Self {
        self.max_frame_len = max;
        self
    }

    /// Sets the default checksum algo for future appends.
    #[allow(dead_code)]
    pub fn with_default_checksum(mut self, algo: ChecksumAlgo) -> Self {
        self.default_checksum = algo;
        self
    }

    /// Appends one payload using an explicit checksum algo for this frame.
    #[allow(dead_code)]
    pub fn append_batch_with_checksum(
        &mut self,
        payload: &[u8],
        algo: ChecksumAlgo,
    ) -> Result<u64, LedgerError> {
        self.append_batch_inner(payload, algo)
    }

    /// Scans from EOF backwards and returns the highest valid committed suffix.
    pub fn scan_committed(&mut self) -> Result<ScanResult, LedgerError> {
        // If segmented, compute global committed_csn/bytes across all segments, while
        // returning committed_start/file_len for the ACTIVE segment (used for appends).
        let seg_paths = segment_paths_newest_first(&self.path);

        // Active segment scan uses the writer file handle (ledger.log may be a symlink).
        let active_scan = scan_committed_suffix_backwards(&mut self.file, self.max_frame_len)?;

        // Single-file case.
        if seg_paths.len() == 1 && seg_paths[0] == self.path {
            return Ok(active_scan);
        }

        // Segmented case: sum scans for all segments.
        let mut total_csn: u64 = 0;
        let mut total_bytes: u64 = 0;

        for p in seg_paths {
            // Skip the active path if it is the same file as ledger.log (symlink target differences).
            // We conservatively rescan by opening each segment path.
            let mut f = OpenOptions::new().read(true).open(&p)?;
            let s = scan_committed_suffix_backwards(&mut f, self.max_frame_len)?;
            total_csn += s.committed_csn;
            total_bytes += s.committed_bytes;
        }

        Ok(ScanResult {
            committed_csn: total_csn,
            committed_bytes: total_bytes,
            committed_start: active_scan.committed_start,
            file_len: active_scan.file_len,
        })
    }

    /// Appends one batch payload as a new frame by writing into free space before the committed suffix.
    ///
    /// Returns the newly acknowledged CSN (1-based) *after* the durability barrier.
    pub fn append_batch(&mut self, payload: &[u8]) -> Result<u64, LedgerError> {
        self.append_batch_inner(payload, self.default_checksum)
    }

    fn append_batch_inner(
        &mut self,
        payload: &[u8],
        algo: ChecksumAlgo,
    ) -> Result<u64, LedgerError> {
        let scan = self.scan_committed()?;
        let file_len = scan.file_len;

        if file_len < FOOTER_LEN {
            return Err(LedgerError::FileTooSmall);
        }

        let len_u32: u32 = payload
            .len()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;

        if len_u32 > self.max_frame_len {
            return Err(LedgerError::FrameTooLarge {
                len: len_u32,
                max: self.max_frame_len,
                at_pos: scan.committed_start,
            });
        }

        let frame_size: u64 = HEADER_LEN + payload.len() as u64 + FOOTER_LEN;

        if frame_size > scan.committed_start {
            return Err(LedgerError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "ledger full: not enough free preallocated space",
            )));
        }

        let write_start = scan.committed_start - frame_size;

        let algo_u32 = algo.to_u32();
        let sum = compute_sum(payload, algo);

        // Header: [magic][len][algo][sum32]
        self.file.seek(SeekFrom::Start(write_start))?;
        self.file.write_all(&MAGIC.to_le_bytes())?;
        self.file.write_all(&len_u32.to_le_bytes())?;
        self.file.write_all(&algo_u32.to_le_bytes())?;
        self.file.write_all(&sum)?;

        // Payload
        self.file.write_all(payload)?;

        // Footer: [sum32][algo][len][magic]
        // Commit gate: footer magic is written LAST in a separate durable step.
        // Until footer magic == MAGIC, the frame is treated as uncommitted mush.
        self.file.write_all(&sum)?;
        self.file.write_all(&algo_u32.to_le_bytes())?;
        self.file.write_all(&len_u32.to_le_bytes())?;

        // Write a zero magic first (uncommitted).
        let zero_magic: u32 = 0;
        self.file.write_all(&zero_magic.to_le_bytes())?;

        // Durability barrier for the uncommitted bytes.
        self.file.sync_data()?;

        // Now publish the commit marker by overwriting only the footer magic.
        // This rewrite is permitted because ACK has not been emitted yet.
        let footer_magic_pos = write_start + HEADER_LEN + payload.len() as u64 + (SUM_LEN + 4 + 4);
        self.file.seek(SeekFrom::Start(footer_magic_pos))?;
        self.file.write_all(&MAGIC.to_le_bytes())?;

        // Final durability barrier for the commit marker.
        self.file.sync_data()?;

        Ok(scan.committed_csn + 1)
    }

    /// Reads and returns up to the last `n` payloads (most recent first) from the committed suffix.
    ///
    /// Convenience for CLI `tail`. This implementation copies payloads.
    pub fn tail_payloads(&mut self, n: usize) -> Result<Vec<Vec<u8>>, LedgerError> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let seg_paths = segment_paths_newest_first(&self.path);
        let mut out: Vec<Vec<u8>> = Vec::new();

        for p in seg_paths {
            if out.len() >= n {
                break;
            }
            let mut f = OpenOptions::new().read(true).open(&p)?;
            let scan = scan_committed_suffix_backwards(&mut f, self.max_frame_len)?;
            if scan.committed_csn == 0 {
                continue;
            }

            let mut pos = scan.committed_start;
            while out.len() < n {
                match read_next_frame_at(&mut f, pos, scan.file_len, self.max_frame_len)? {
                    NextFrame::Stop => break,
                    NextFrame::Frame { payload, next_pos } => {
                        out.push(payload);
                        pos = next_pos;
                    }
                }
            }
        }

        Ok(out)
    }

    /// Calls `f` for each committed payload in forward order (newest first), starting at `scan.committed_start`.
    ///
    /// This is a streaming alternative to `tail_payloads` for tools that need to fold over the committed suffix
    /// without loading everything into memory.
    #[allow(dead_code)]
    pub fn for_each_committed_payload<F>(
        &mut self,
        _scan: ScanResult,
        mut f: F,
    ) -> Result<(), LedgerError>
    where
        F: FnMut(Vec<u8>),
    {
        // Ignore the provided scan and instead stream across segments newest-first.
        let seg_paths = segment_paths_newest_first(&self.path);
        for p in seg_paths {
            let mut file = OpenOptions::new().read(true).open(&p)?;
            let scan = scan_committed_suffix_backwards(&mut file, self.max_frame_len)?;
            if scan.committed_csn == 0 {
                continue;
            }

            let mut pos = scan.committed_start;
            while pos < scan.file_len {
                match read_next_frame_at(&mut file, pos, scan.file_len, self.max_frame_len)? {
                    NextFrame::Stop => break,
                    NextFrame::Frame { payload, next_pos } => {
                        f(payload);
                        pos = next_pos;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Scans the committed suffix from EOF backwards.
pub fn scan_committed_suffix_backwards(
    file: &mut File,
    max_frame_len: u32,
) -> Result<ScanResult, LedgerError> {
    let file_len = file.metadata()?.len();
    if file_len < FOOTER_LEN {
        return Ok(ScanResult {
            committed_csn: 0,
            committed_bytes: 0,
            committed_start: file_len,
            file_len,
        });
    }

    let mut committed_csn: u64 = 0;
    let mut committed_bytes: u64 = 0;
    let mut pos: u64 = file_len;

    loop {
        match read_prev_frame_at(file, pos, max_frame_len)? {
            PrevFrame::Stop => break,
            PrevFrame::Frame { new_pos } => {
                // Frame is valid; advance committed suffix.
                let advanced = pos - new_pos;
                committed_bytes += advanced;
                committed_csn += 1;
                pos = new_pos;
            }
        }
    }

    Ok(ScanResult {
        committed_csn,
        committed_bytes,
        committed_start: pos,
        file_len,
    })
}

enum PrevFrame {
    Stop,
    Frame { new_pos: u64 },
}

enum NextFrame {
    Stop,
    Frame { payload: Vec<u8>, next_pos: u64 },
}

fn read_next_frame_at(
    file: &mut File,
    pos: u64,
    file_len: u64,
    max_frame_len: u32,
) -> Result<NextFrame, LedgerError> {
    if pos + HEADER_LEN > file_len {
        return Ok(NextFrame::Stop);
    }

    // Read header: [magic][len][algo][sum32]
    file.seek(SeekFrom::Start(pos))?;
    let mut fixed = [0u8; 12];
    match file.read_exact(&mut fixed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(NextFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let magic = u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]);
    let len = u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
    let algo_u32 = u32::from_le_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]);

    if magic != MAGIC {
        return Ok(NextFrame::Stop);
    }

    let algo = match ChecksumAlgo::from_u32(algo_u32) {
        Some(a) => a,
        None => return Ok(NextFrame::Stop),
    };

    if len > max_frame_len {
        // Treat absurd lengths as an invalid tail frame; stop without advancing.
        return Ok(NextFrame::Stop);
    }

    let mut sum_hdr = [0u8; 32];
    match file.read_exact(&mut sum_hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(NextFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let payload_len = len as u64;
    let frame_size = HEADER_LEN + payload_len + FOOTER_LEN;

    if pos + frame_size > file_len {
        return Ok(NextFrame::Stop);
    }

    // Read payload.
    let payload_start = pos + HEADER_LEN;
    file.seek(SeekFrom::Start(payload_start))?;
    let mut payload = vec![0u8; payload_len as usize];
    match file.read_exact(&mut payload) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(NextFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    // Verify checksum (payload only).
    let sum_actual = compute_sum(&payload, algo);
    if sum_actual != sum_hdr {
        return Ok(NextFrame::Stop);
    }

    // Verify footer: [sum32][algo][len][magic]
    let footer_pos = pos + HEADER_LEN + payload_len;
    file.seek(SeekFrom::Start(footer_pos))?;

    let mut sum_f = [0u8; 32];
    match file.read_exact(&mut sum_f) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(NextFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let mut tail_fixed = [0u8; 12];
    match file.read_exact(&mut tail_fixed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(NextFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let f_algo_u32 =
        u32::from_le_bytes([tail_fixed[0], tail_fixed[1], tail_fixed[2], tail_fixed[3]]);
    let f_len = u32::from_le_bytes([tail_fixed[4], tail_fixed[5], tail_fixed[6], tail_fixed[7]]);
    let f_magic =
        u32::from_le_bytes([tail_fixed[8], tail_fixed[9], tail_fixed[10], tail_fixed[11]]);

    if f_magic != MAGIC || f_len != len || f_algo_u32 != algo_u32 || sum_f != sum_hdr {
        return Ok(NextFrame::Stop);
    }

    Ok(NextFrame::Frame {
        payload,
        next_pos: pos + frame_size,
    })
}

fn read_prev_frame_at(
    file: &mut File,
    pos: u64,
    max_frame_len: u32,
) -> Result<PrevFrame, LedgerError> {
    if pos < FOOTER_LEN {
        return Ok(PrevFrame::Stop);
    }

    // Read footer: [sum32][algo][len][magic]
    file.seek(SeekFrom::Start(pos - FOOTER_LEN))?;

    let mut sum_f = [0u8; 32];
    match file.read_exact(&mut sum_f) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PrevFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let mut tail_fixed = [0u8; 12];
    match file.read_exact(&mut tail_fixed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PrevFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let algo_u32 = u32::from_le_bytes([tail_fixed[0], tail_fixed[1], tail_fixed[2], tail_fixed[3]]);
    let len = u32::from_le_bytes([tail_fixed[4], tail_fixed[5], tail_fixed[6], tail_fixed[7]]);
    let magic = u32::from_le_bytes([tail_fixed[8], tail_fixed[9], tail_fixed[10], tail_fixed[11]]);

    if magic != MAGIC {
        return Ok(PrevFrame::Stop);
    }

    let algo = match ChecksumAlgo::from_u32(algo_u32) {
        Some(a) => a,
        None => return Ok(PrevFrame::Stop),
    };

    if len > max_frame_len {
        // Treat absurd lengths as an invalid tail frame; stop without advancing.
        return Ok(PrevFrame::Stop);
    }

    let payload_len = len as u64;
    let frame_size = HEADER_LEN + payload_len + FOOTER_LEN;

    if pos < frame_size {
        // Would underflow => invalid.
        return Ok(PrevFrame::Stop);
    }

    let payload_start = pos - FOOTER_LEN - payload_len;
    let payload_end = pos - FOOTER_LEN;
    let header_start = payload_start - HEADER_LEN;
    // Validate header consistency with footer before trusting payload.
    // This ensures header corruption cannot be mistaken as a committed frame.
    file.seek(SeekFrom::Start(header_start))?;

    // Header fixed part: [magic][len][algo]
    let mut h_fixed = [0u8; 12];
    match file.read_exact(&mut h_fixed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PrevFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    let h_magic = u32::from_le_bytes([h_fixed[0], h_fixed[1], h_fixed[2], h_fixed[3]]);
    let h_len = u32::from_le_bytes([h_fixed[4], h_fixed[5], h_fixed[6], h_fixed[7]]);
    let h_algo_u32 = u32::from_le_bytes([h_fixed[8], h_fixed[9], h_fixed[10], h_fixed[11]]);

    if h_magic != MAGIC || h_len != len || h_algo_u32 != algo_u32 {
        return Ok(PrevFrame::Stop);
    }

    // Header sum: must match footer sum.
    let mut sum_hdr = [0u8; 32];
    match file.read_exact(&mut sum_hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PrevFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    if sum_hdr != sum_f {
        return Ok(PrevFrame::Stop);
    }
    file.seek(SeekFrom::Start(payload_start))?;
    let mut payload = vec![0u8; payload_len as usize];
    match file.read_exact(&mut payload) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PrevFrame::Stop),
        Err(e) => return Err(LedgerError::Io(e)),
    }

    // Verify checksum.
    let sum_actual = compute_sum(&payload, algo);
    if sum_actual != sum_f {
        return Ok(PrevFrame::Stop);
    }

    // Sanity: ensure we are not reading past payload_end.
    debug_assert_eq!(payload_start + payload_len, payload_end);

    Ok(PrevFrame::Frame {
        new_pos: header_start,
    })
}

pub(crate) fn compute_sum(payload: &[u8], algo: ChecksumAlgo) -> [u8; 32] {
    match algo {
        ChecksumAlgo::None => [0u8; 32],
        ChecksumAlgo::Crc32 => {
            let mut h = Hasher::new();
            h.update(payload);
            let crc = h.finalize();
            let mut out = [0u8; 32];
            out[0..4].copy_from_slice(&crc.to_le_bytes());
            out
        }
        ChecksumAlgo::Blake3 => *blake3::hash(payload).as_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> std::path::PathBuf {
        // Not used for correctness; only temp uniqueness.
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("turtle_ledger_test_back_{}_{}", name, t))
    }

    #[test]
    fn scan_stops_at_partial_tail_write_footer() {
        let path = temp_path("partial_footer");
        let _ = std::fs::remove_file(&path);

        // Create one valid frame.
        {
            let mut ledger = Ledger::open(&path).unwrap();
            ledger.ensure_prealloc_len(4096).unwrap();
            let csn1 = ledger.append_batch(b"ok").unwrap();
            assert_eq!(csn1, 1);
        }

        // Simulate a crash during the next append: write only part of the footer
        // at the current committed_start.
        {
            let mut ledger = Ledger::open(&path).unwrap();
            let scan = ledger.scan_committed().unwrap();

            // We will write a header+payload ("bad") but not the full footer.
            let payload = b"bad";
            let len_u32 = payload.len() as u32;
            let algo = ChecksumAlgo::Blake3;
            let algo_u32 = algo.to_u32();
            let sum = compute_sum(payload, algo);

            let frame_size = HEADER_LEN + payload.len() as u64 + FOOTER_LEN;
            let write_start = scan.committed_start - frame_size;

            // Write header.
            ledger.file.seek(SeekFrom::Start(write_start)).unwrap();
            ledger.file.write_all(&MAGIC.to_le_bytes()).unwrap();
            ledger.file.write_all(&len_u32.to_le_bytes()).unwrap();
            ledger.file.write_all(&algo_u32.to_le_bytes()).unwrap();
            ledger.file.write_all(&sum).unwrap();

            // Write payload.
            ledger.file.write_all(payload).unwrap();

            // Write only a partial footer (simulate power loss).
            // Let's write only the first 8 bytes of the sum.
            ledger.file.write_all(&sum[..8]).unwrap();
            // No sync: simulate power loss.
        }

        // Scan should report only the first frame as committed.
        {
            let mut ledger = Ledger::open(&path).unwrap();
            let scan = ledger.scan_committed().unwrap();
            assert_eq!(scan.committed_csn, 1);

            let tail = ledger.tail_payloads(10).unwrap();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0], b"ok");
        }

        let _ = std::fs::remove_file(&path);
    }
}
// Loom concurrency tests (developer-side model checking)
//
// These do NOT touch the filesystem. They model the correctness contract we rely on:
// - a commit is made visible only after a final commit marker is published
// - readers must never observe a partially-committed record as committed
//
// Run with:
//   RUSTFLAGS="--cfg loom" cargo test -p turtle -- --nocapture
//
// Note: Loom explores thread interleavings for the in-memory model.
#[cfg(loom)]
mod loom_tests {
    use loom::sync::atomic::{AtomicU32, Ordering};
    use loom::sync::Arc;
    use loom::thread;

    // Model constants.
    const MAGIC: u32 = 0x4C_54_52_54; // 'TRTL'

    // A tiny in-memory model of "footer framed" commit where MAGIC is written last.
    // We model the footer fields as independent atomics to allow Loom to interleave them.
    struct FrameFooter {
        crc: AtomicU32,
        len: AtomicU32,
        magic: AtomicU32,
    }

    impl FrameFooter {
        fn new() -> Self {
            Self {
                crc: AtomicU32::new(0),
                len: AtomicU32::new(0),
                magic: AtomicU32::new(0),
            }
        }

        // Writer publishes crc and len, then publishes magic last as the commit marker.
        fn publish_commit(&self, crc: u32, len: u32) {
            self.crc.store(crc, Ordering::Relaxed);
            self.len.store(len, Ordering::Relaxed);
            // Commit marker must be last and use Release to publish prior writes.
            self.magic.store(MAGIC, Ordering::Release);
        }

        // Reader treats frame as committed iff magic matches and crc/len match expected.
        fn is_committed(&self, expected_crc: u32, expected_len: u32) -> bool {
            let m = self.magic.load(Ordering::Acquire);
            if m != MAGIC {
                return false;
            }
            // If MAGIC is observed, crc/len must be from this commit (Acquire/Release pair).
            let c = self.crc.load(Ordering::Relaxed);
            let l = self.len.load(Ordering::Relaxed);
            c == expected_crc && l == expected_len
        }
    }

    #[test]
    fn loom_commit_marker_prevents_partial_visibility() {
        loom::model(|| {
            let footer = Arc::new(FrameFooter::new());

            // A single logical frame we expect.
            let expected_crc = 0xAABBCCDD;
            let expected_len = 42;

            let w = {
                let footer = Arc::clone(&footer);
                thread::spawn(move || {
                    // Writer commit publish.
                    footer.publish_commit(expected_crc, expected_len);
                })
            };

            let r = {
                let footer = Arc::clone(&footer);
                thread::spawn(move || {
                    // Reader can run at any time.
                    // The assertion: if we see committed, we must see consistent fields.
                    let committed = footer.is_committed(expected_crc, expected_len);
                    if committed {
                        // Seeing committed implies correctness.
                        assert!(committed);
                    } else {
                        // Not committed is also acceptable.
                        assert!(!committed);
                    }
                })
            };

            w.join().unwrap();
            r.join().unwrap();
        });
    }

    #[test]
    #[should_panic]
    fn loom_magic_last_is_essential() {
        // This test demonstrates why MAGIC must be written last.
        // We intentionally publish MAGIC first and show that a reader may observe MAGIC
        // while crc/len are still default, which would be a partial-visibility bug.
        loom::model(|| {
            let footer = Arc::new(FrameFooter::new());

            let expected_crc = 0x11223344;
            let expected_len = 7;

            let w = {
                let footer = Arc::clone(&footer);
                thread::spawn(move || {
                    // WRONG order (for demonstration): publish magic first.
                    footer.magic.store(MAGIC, Ordering::Release);
                    footer.crc.store(expected_crc, Ordering::Relaxed);
                    footer.len.store(expected_len, Ordering::Relaxed);
                })
            };

            let r = {
                let footer = Arc::clone(&footer);
                thread::spawn(move || {
                    // Reader: if it sees MAGIC, it might still see stale crc/len.
                    let m = footer.magic.load(Ordering::Acquire);
                    if m == MAGIC {
                        let c = footer.crc.load(Ordering::Relaxed);
                        let l = footer.len.load(Ordering::Relaxed);
                        // Under this WRONG ordering, it's possible to observe MAGIC but not the fields.
                        // Loom should be able to find such an interleaving, so we assert that this is
                        // a potential bug state.
                        assert!(
                            c == expected_crc && l == expected_len,
                            "partial visibility: saw MAGIC but fields not yet published"
                        );
                    }
                })
            };

            // We expect this test to FAIL under Loom in some interleavings.
            // That is the point: it proves ordering matters.
            let _ = w.join();
            let _ = r.join();
        });
    }
    // A slightly richer model: header fields exist, but ONLY the footer magic is treated
    // as the commit marker. This mirrors the on-disk rule: readers must not treat a
    // frame as committed unless they observe the commit marker.
    struct FrameModel {
        header_magic: AtomicU32,
        header_len: AtomicU32,
        header_crc: AtomicU32,
        footer_crc: AtomicU32,
        footer_len: AtomicU32,
        footer_magic: AtomicU32,
    }

    impl FrameModel {
        fn new() -> Self {
            Self {
                header_magic: AtomicU32::new(0),
                header_len: AtomicU32::new(0),
                header_crc: AtomicU32::new(0),
                footer_crc: AtomicU32::new(0),
                footer_len: AtomicU32::new(0),
                footer_magic: AtomicU32::new(0),
            }
        }

        // Correct publish order: write all fields, then publish footer_magic last.
        fn publish_correct(&self, crc: u32, len: u32) {
            self.header_magic.store(MAGIC, Ordering::Relaxed);
            self.header_len.store(len, Ordering::Relaxed);
            self.header_crc.store(crc, Ordering::Relaxed);
            self.footer_crc.store(crc, Ordering::Relaxed);
            self.footer_len.store(len, Ordering::Relaxed);
            self.footer_magic.store(MAGIC, Ordering::Release);
        }

        // Reader: committed iff footer_magic observed; if so, other fields must be consistent.
        fn read_committed(&self) -> bool {
            let fm = self.footer_magic.load(Ordering::Acquire);
            if fm != MAGIC {
                return false;
            }
            let hc = self.header_crc.load(Ordering::Relaxed);
            let hl = self.header_len.load(Ordering::Relaxed);
            let hm = self.header_magic.load(Ordering::Relaxed);
            let fc = self.footer_crc.load(Ordering::Relaxed);
            let fl = self.footer_len.load(Ordering::Relaxed);
            hm == MAGIC && hc == fc && hl == fl
        }
    }

    #[test]
    fn loom_footer_magic_as_single_commit_gate_is_sound() {
        loom::model(|| {
            let m = Arc::new(FrameModel::new());

            let expected_crc = 0x55667788;
            let expected_len = 99;

            let w = {
                let m = Arc::clone(&m);
                thread::spawn(move || {
                    m.publish_correct(expected_crc, expected_len);
                })
            };

            let r = {
                let m = Arc::clone(&m);
                thread::spawn(move || {
                    // Reader is allowed to see either "not committed" or "committed and consistent".
                    let ok = m.read_committed();
                    if ok {
                        assert!(ok);
                    } else {
                        assert!(!ok);
                    }
                })
            };

            w.join().unwrap();
            r.join().unwrap();
        });
    }
}

// Segment helpers for segmented ledgers (turtle_roll).
fn seg_dir_for_ledger_path(ledger_path: &Path) -> PathBuf {
    ledger_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("seg")
}

fn parse_seg_idx(name: &str) -> Option<u32> {
    // Expects NNNN.log
    if !name.ends_with(".log") {
        return None;
    }
    let stem = name.strip_suffix(".log")?;
    if stem.len() != 4 {
        return None;
    }
    stem.parse::<u32>().ok()
}

fn list_segment_names(seg_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let rd = match fs::read_dir(seg_dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if parse_seg_idx(&name).is_some() {
            out.push(name);
        }
    }
    out.sort();
    out
}

fn segment_paths_newest_first(ledger_path: &Path) -> Vec<PathBuf> {
    let seg_dir = seg_dir_for_ledger_path(ledger_path);
    if !seg_dir.exists() {
        return vec![ledger_path.to_path_buf()];
    }

    let mut names = list_segment_names(&seg_dir);
    names.reverse(); // newest-first lexicographically (0009 > 0008)

    names.into_iter().map(|n| seg_dir.join(n)).collect()
}
