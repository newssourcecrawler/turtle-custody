use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::block_index::BlockLocator;
use crate::ledger::{
    compute_sum, ChecksumAlgo, ScanResult, FOOTER_LEN, HEADER_LEN, MAGIC, SUM_LEN,
};
use crate::{block_id_from_ledger_sum, BlockId, BlockIdAlgo};

/// Memory-map scanning of the ledger file.
///
/// Semantics:
/// - Stop at first invalid/incomplete frame.
/// - Treat absurd lengths as invalid tail (stop), never as a hard error.
/// - Backward-prealloc layout: committed data is a contiguous suffix anchored at EOF.

#[derive(Clone, Copy, Debug)]
struct FrameView {
    header_start: usize,
    payload_start: usize,
    payload_end: usize,
    payload_len_u32: u32,
    algo_u32: u32,
    sum: [u8; 32],
}

// Attempt to parse and custody-verify the newest frame ending at `pos`.
// Returns None if the tail is invalid/incomplete.
fn parse_frame_at(data: &[u8], pos: usize, max_frame_len: u32) -> Option<FrameView> {
    if pos < FOOTER_LEN as usize {
        return None;
    }

    let footer_start = pos - (FOOTER_LEN as usize);

    // Footer: [sum32][algo][len][magic]
    let magic = read_u32_le(data, footer_start + (SUM_LEN as usize) + 4 + 4)?;
    if magic != MAGIC {
        return None;
    }

    let len_u32 = read_u32_le(data, footer_start + (SUM_LEN as usize) + 4)?;
    if len_u32 > max_frame_len {
        return None;
    }
    let payload_len = len_u32 as usize;

    let algo_u32 = read_u32_le(data, footer_start + (SUM_LEN as usize))?;
    let algo = ChecksumAlgo::from_u32(algo_u32)?;

    // Compute start positions.
    let header_start = footer_start.checked_sub(payload_len + (HEADER_LEN as usize))?;
    let payload_start = header_start + (HEADER_LEN as usize);
    let payload_end = payload_start.checked_add(payload_len)?;
    if payload_end != footer_start {
        return None;
    }

    // Validate header matches footer.
    let h_magic = read_u32_le(data, header_start)?;
    let h_len = read_u32_le(data, header_start + 4)?;
    let h_algo_u32 = read_u32_le(data, header_start + 8)?;
    if h_magic != MAGIC || h_len != len_u32 || h_algo_u32 != algo_u32 {
        return None;
    }

    let sum_hdr = read_sum32(data, header_start + 12)?;
    let sum_f = read_sum32(data, footer_start)?;
    if sum_hdr != sum_f {
        return None;
    }

    // Verify checksum.
    let payload = &data[payload_start..payload_end];
    let sum_actual = compute_sum(payload, algo);
    if sum_actual != sum_f {
        return None;
    }

    Some(FrameView {
        header_start,
        payload_start,
        payload_end,
        payload_len_u32: len_u32,
        algo_u32,
        sum: sum_f,
    })
}

/// Memory-map scanning of the ledger file.
///
/// Semantics:
/// - Stop at first invalid/incomplete frame.
/// - Treat absurd lengths as invalid tail (stop), never as a hard error.
/// - Backward-prealloc layout: committed data is a contiguous suffix anchored at EOF.
pub fn scan_committed_mmap(path: &Path, max_frame_len: u32) -> io::Result<ScanResult> {
    let file = File::open(path)?;
    let meta_len = file.metadata()?.len();
    if meta_len == 0 {
        return Ok(ScanResult {
            committed_csn: 0,
            committed_bytes: 0,
            committed_start: 0,
            file_len: 0,
        });
    }
    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap[..];

    // Consistency: the mmap length should match the current metadata length.
    // If it does not, treat it as a race (file changed while mapping) and fail fast.
    let eof = data.len();
    let file_len = eof as u64;
    if meta_len != file_len {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "ledger mmap/metadata length mismatch: metadata={} mmap={} (file changed while mapping)",
                meta_len, file_len
            ),
        ));
    }

    // Start at EOF and walk backwards by reading footers.
    let mut pos = eof;
    // NOTE: this is a count of committed frames found, not a global CSN.
    let mut committed_frames: u64 = 0;
    let mut committed_bytes: u64 = 0;

    loop {
        let fv = match parse_frame_at(data, pos, max_frame_len) {
            Some(v) => v,
            None => break,
        };

        committed_frames += 1;
        committed_bytes += (HEADER_LEN as u64) + (fv.payload_len_u32 as u64) + (FOOTER_LEN as u64);

        pos = fv.header_start;
    }

    Ok(ScanResult {
        committed_csn: committed_frames,
        committed_bytes,
        committed_start: pos as u64,
        file_len,
    })
}

/// Scan the committed suffix and return oldest-first `(BlockId, BlockLocator)` entries
/// for canonical BLAKE3 frames.
///
/// This is a custody-layer helper:
/// - It does **not** allocate payload bytes.
/// - It does verify checksums (hashes payload slices) so returned ids/locators correspond to
///   committed, checksum-valid frames.
/// - `csn` here is a local monotonic counter (1..=N) in *oldest-first* order for this scan.
///   If you need a global csn, keep that above this layer.
pub fn scan_committed_block_locators_mmap(
    path: &Path,
    max_frame_len: u32,
) -> io::Result<Vec<(BlockId, BlockLocator)>> {
    let file = File::open(path)?;
    let meta_len = file.metadata()?.len();
    if meta_len == 0 {
        return Ok(Vec::new());
    }

    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap[..];
    let file_len = data.len() as u64;
    if meta_len != file_len {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "ledger mmap/metadata length mismatch: metadata={} mmap={} (file changed while mapping)",
                meta_len, file_len
            ),
        ));
    }

    let mut pos = data.len();
    // Local monotonic counter for returned BlockIds only (oldest-first).
    let mut block_csn: u64 = 0;
    let mut out: Vec<(BlockId, BlockLocator)> = Vec::new();

    loop {
        let fv = match parse_frame_at(data, pos, max_frame_len) {
            Some(v) => v,
            None => break,
        };

        // Only BLAKE3 is a canonical block identity. CRC32 and NONE remain valid
        // ledger checksum modes, but must not enter the content-addressed index.
        if fv.algo_u32 == u32::from(BlockIdAlgo::Blake3_256.as_u8()) {
            let id = block_id_from_ledger_sum(fv.algo_u32, fv.sum)
                .expect("canonical BLAKE3 ledger sum must yield a BlockId");
            block_csn += 1;
            out.push((
                id,
                BlockLocator {
                    csn: block_csn,
                    payload_pos: fv.payload_start as u64,
                    payload_len: fv.payload_len_u32,
                },
            ));
        } else {
            // Still advance csn even if not a BlockId? No: csn is for returned block ids only.
            // If you need a csn over all frames, keep that above this helper.
        }

        pos = fv.header_start;
    }

    Ok(out)
}

/// Memory-map tail that returns oldest-first owned payload bytes
/// (no borrowed slices escape the mmap lifetime).
pub fn tail_payloads_mmap(path: &Path, max_frame_len: u32, n: usize) -> io::Result<Vec<Vec<u8>>> {
    let file = File::open(path)?;
    let meta_len = file.metadata()?.len();
    if meta_len == 0 || n == 0 {
        return Ok(Vec::new());
    }

    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap[..];
    let file_len = data.len() as u64;
    if meta_len != file_len {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "ledger mmap/metadata length mismatch: metadata={} mmap={} (file changed while mapping)",
                meta_len, file_len
            ),
        ));
    }

    // Backward walk from EOF, collecting oldest-first.
    let mut pos = data.len();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n.min(1024));

    while out.len() < n {
        let fv = match parse_frame_at(data, pos, max_frame_len) {
            Some(v) => v,
            None => break,
        };

        let payload = &data[fv.payload_start..fv.payload_end];
        out.push(payload.to_vec());
        pos = fv.header_start;
    }

    Ok(out)
}

/// Read one bounded byte range from a payload identified by an already verified locator.
///
/// This helper is segment-local: `path` must name the same ledger segment from which
/// `locator` was derived. It trusts the caller's prior committed-frame and checksum
/// validation, then enforces range and allocation bounds before reading only the
/// requested payload bytes.
pub fn read_payload_range(
    path: &Path,
    locator: &BlockLocator,
    within_payload_offset: u64,
    requested_len: u64,
    max_output: u64,
) -> io::Result<Vec<u8>> {
    if requested_len > max_output {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "requested length exceeds max_output",
        ));
    }

    let range_end = within_payload_offset
        .checked_add(requested_len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload range overflow"))?;
    let payload_len = u64::from(locator.payload_len);

    if within_payload_offset > payload_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "range begins outside payload",
        ));
    }
    if range_end > payload_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "range extends beyond payload",
        ));
    }

    let absolute_start = locator
        .payload_pos
        .checked_add(within_payload_offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload offset overflow"))?;
    let absolute_end = absolute_start
        .checked_add(requested_len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload end overflow"))?;
    let output_len = usize::try_from(requested_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "requested length is not representable in memory",
        )
    })?;

    let mut file = File::open(path)?;
    if absolute_end > file.metadata()?.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "locator range exceeds segment length",
        ));
    }

    file.seek(SeekFrom::Start(absolute_start))?;
    let mut out = vec![0_u8; output_len];
    file.read_exact(&mut out)?;
    Ok(out)
}

fn read_sum32(data: &[u8], at: usize) -> Option<[u8; 32]> {
    let end = at.checked_add(SUM_LEN as usize)?;
    if end > data.len() {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&data[at..end]);
    Some(out)
}

fn read_u32_le(data: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    if end > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        data[at],
        data[at + 1],
        data[at + 2],
        data[at + 3],
    ]))
}
