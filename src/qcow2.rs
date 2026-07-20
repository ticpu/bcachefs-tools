// SPDX-License-Identifier: GPL-2.0

//! QCOW2 sparse image format — writer and reader.
//!
//! Used by `bcachefs dump` to create sparse metadata images and
//! `bcachefs undump` to convert them back to raw device images.
//!
//! Streaming layout invariant (`write_qcow2_stream` / `qcow2_stream_to_raw`):
//! clusters are emitted strictly forward with no seeking — header cluster,
//! then the L1 table (padded to a cluster), then per used L1 index the L2
//! table immediately followed by that group's data clusters. Every offset is
//! therefore a deterministic function of the merged ranges, so the reader can
//! validate each L1/L2 entry against the position it must hold and reject any
//! image not written in this exact order (e.g. a seek-written file piped in).
//! The seek-based `Qcow2Image` writer and the file-based `qcow2_to_raw` reader
//! honor arbitrary offsets and are not bound by this invariant.

use std::ops::Range;
use std::os::fd::BorrowedFd;

use anyhow::{anyhow, Result};
use rustix::io::Errno;

// ---- Ranges helpers ----

pub type Ranges = Vec<Range<u64>>;

pub fn range_add(ranges: &mut Ranges, offset: u64, size: u64) {
    ranges.push(offset..offset + size);
}

fn ranges_roundup(ranges: &mut Ranges, block_size: u64) {
    for r in ranges.iter_mut() {
        r.start = r.start / block_size * block_size;
        r.end = round_up(r.end, block_size);
    }
}

fn ranges_sort_merge(ranges: &mut Ranges) {
    ranges.sort_by_key(|r| r.start);
    let mut merged = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            let last: &mut Range<u64> = last;
            if last.end >= r.start {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }
    *ranges = merged;
}

pub fn ranges_sort(ranges: &mut Ranges) {
    ranges.sort_by_key(|r| r.start);
}

// ---- I/O helpers ----

pub fn pread_exact(fd: BorrowedFd<'_>, buf: &mut [u8], mut offset: u64) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::pread(fd, &mut buf[pos..], offset) {
            Ok(0) => return Err(anyhow!("read error: unexpected EOF")),
            Ok(n) => {
                pos += n;
                offset += n as u64;
            }
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("read error: {e}")),
        }
    }
    Ok(())
}

pub(crate) fn pwrite_all(fd: BorrowedFd<'_>, buf: &[u8], mut offset: u64) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::pwrite(fd, &buf[pos..], offset) {
            Ok(0) => return Err(anyhow!("write error: no progress")),
            Ok(n) => {
                pos += n;
                offset += n as u64;
            }
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("write error: {e}")),
        }
    }
    Ok(())
}

fn write_all_seq(fd: BorrowedFd<'_>, buf: &[u8]) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::write(fd, &buf[pos..]) {
            Ok(0) => return Err(anyhow!("write error: no progress")),
            Ok(n) => pos += n,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("write error: {e}")),
        }
    }
    Ok(())
}

fn read_exact_seq(fd: BorrowedFd<'_>, buf: &mut [u8]) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::read(fd, &mut buf[pos..]) {
            Ok(0) => return Err(anyhow!("read error: unexpected EOF")),
            Ok(n) => pos += n,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("read error: {e}")),
        }
    }
    Ok(())
}

// ---- QCOW2 format helpers ----

fn qcow2_geometry(block_size: u32, image_size: u64) -> (usize, u64) {
    let l2_size = block_size as u64 / 8;
    let l1_size = image_size.div_ceil(block_size as u64 * l2_size) as usize;
    (l1_size, l2_size)
}

fn pack_l2(buf: &mut [u8], entries: &[u64]) {
    for (i, &e) in entries.iter().enumerate() {
        buf[i * 8..(i + 1) * 8].copy_from_slice(&e.to_be_bytes());
    }
}

fn prepare_output(outfd: BorrowedFd, image_size: u64) -> Result<()> {
    use rustix::fs::{fstat, FileType};
    let st = fstat(outfd)?;
    if FileType::from_raw_mode(st.st_mode) == FileType::BlockDevice {
        let dev_size = file_size_fd(outfd)?;
        if dev_size < image_size {
            return Err(anyhow!("output device too small: {dev_size} < {image_size} bytes"));
        }
        Ok(())
    } else {
        rustix::fs::ftruncate(outfd, image_size)?;
        Ok(())
    }
}

/// Write a qcow2 image strictly forward (no seeking) to a pipe or file.
///
/// Two passes over the merged ranges: pass 1 counts data clusters per L2 group
/// to assign each group's stream offset; pass 2 emits header, L1, then per used
/// group its L2 table followed by that group's data. Memory is O(ranges + L1);
/// no per-cluster bookkeeping. See the streaming layout invariant at the top of
/// this file.
pub fn write_qcow2_stream(infd: BorrowedFd, outfd: BorrowedFd, block_size: u32,
                          ranges: &mut Ranges) -> Result<()> {
    assert!(block_size.is_power_of_two());

    let bs = block_size as u64;

    ranges_roundup(ranges, bs);
    ranges_sort_merge(ranges);

    let image_size = file_size_fd(infd)?;
    let (l1_size, l2_size) = qcow2_geometry(block_size, image_size);
    let group_bytes = bs * l2_size;

    // Pass 1: data-cluster count per L2 group, then assign group offsets.
    let mut counts = vec![0u32; l1_size];
    for r in ranges.iter() {
        if r.start >= r.end {
            continue;
        }
        let first_g = (r.start / group_bytes) as usize;
        let last_g = ((r.end - 1) / group_bytes) as usize;
        for (g, cnt) in counts.iter_mut().enumerate().take(last_g + 1).skip(first_g) {
            let gw_start = g as u64 * group_bytes;
            let gw_end = gw_start + group_bytes;
            let cs = r.start.max(gw_start);
            let ce = r.end.min(gw_end);
            *cnt += ((ce - cs) / bs) as u32;
        }
    }

    let l1_off = bs;
    let l1_clusters = round_up(l1_size as u64 * 8, bs) / bs;
    let data_start = l1_off + l1_clusters * bs;

    let mut l1_table = vec![0u64; l1_size];
    let mut off = data_start;
    for (g, &cnt) in counts.iter().enumerate() {
        if cnt == 0 {
            continue;
        }
        l1_table[g] = off | QCOW_OFLAG_COPIED;
        off += bs * (1 + cnt as u64);
    }

    let mut cluster = vec![0u8; block_size as usize];
    let mut copybuf = vec![0u8; COPY_BUF_SIZE];

    encode_header(&mut cluster, QCOW_MAGIC, QCOW_VERSION, block_size.trailing_zeros(),
                  image_size, l1_size as u32, l1_off);
    write_all_seq(outfd, &cluster)?;

    let l1_bytes: Vec<u8> = l1_table.iter().flat_map(|v| v.to_be_bytes()).collect();
    write_all_seq(outfd, &l1_bytes)?;
    let l1_pad = (l1_clusters * bs) as usize - l1_bytes.len();
    copybuf[..l1_pad].fill(0);
    write_all_seq(outfd, &copybuf[..l1_pad])?;

    // Pass 2: emit each used group's L2 table then its data clusters.
    // A cursor (ri, cur) tracks the next unprocessed source offset across the
    // sorted ranges; a range straddling a group boundary keeps the cursor.
    let mut stream_pos = data_start;
    let mut ri = 0usize;
    let mut cur = ranges.first().map(|r| r.start).unwrap_or(0);

    for (g, &cnt) in counts.iter().enumerate() {
        if cnt == 0 {
            continue;
        }
        let l2_off = l1_table[g] & !QCOW_OFLAG_COPIED;
        let data_base = l2_off + bs;
        let gw_end = (g as u64 + 1) * group_bytes;

        // Build the L2 table via a non-advancing sub-walk of this group's clusters.
        cluster.fill(0);
        let (mut lri, mut lcur) = (ri, cur);
        let mut k = 0u64;
        loop {
            if lri >= ranges.len() {
                break;
            }
            if lcur >= ranges[lri].end {
                lri += 1;
                if lri < ranges.len() {
                    lcur = ranges[lri].start;
                }
                continue;
            }
            if lcur >= gw_end {
                break;
            }
            let l2_idx = (lcur / bs % l2_size) as usize;
            let entry = (data_base + k * bs) | QCOW_OFLAG_COPIED;
            cluster[l2_idx * 8..l2_idx * 8 + 8].copy_from_slice(&entry.to_be_bytes());
            k += 1;
            lcur += bs;
        }
        debug_assert_eq!(k, cnt as u64);
        write_all_seq(outfd, &cluster)?;
        stream_pos += bs;

        // Emit data: contiguous clusters within a clipped range are contiguous
        // in both source and stream, so copy them in COPY_BUF_SIZE chunks.
        let mut k2 = 0u64;
        loop {
            if ri >= ranges.len() {
                break;
            }
            if cur >= ranges[ri].end {
                ri += 1;
                if ri < ranges.len() {
                    cur = ranges[ri].start;
                }
                continue;
            }
            if cur >= gw_end {
                break;
            }
            let seg_end = ranges[ri].end.min(gw_end);
            let mut p = cur;
            while p < seg_end {
                let chunk = (seg_end - p).min(COPY_BUF_SIZE as u64);
                // ranges_roundup may push past a non-block-aligned image_size;
                // read what exists and zero-fill the cluster tail.
                let avail = image_size.saturating_sub(p).min(chunk);
                pread_exact(infd, &mut copybuf[..avail as usize], p)?;
                copybuf[avail as usize..chunk as usize].fill(0);
                write_all_seq(outfd, &copybuf[..chunk as usize])?;
                stream_pos += chunk;
                k2 += chunk / bs;
                p += chunk;
            }
            cur = seg_end;
        }
        debug_assert_eq!(k2, cnt as u64);
    }
    debug_assert_eq!(stream_pos, off);

    Ok(())
}

/// Convert a qcow2 image from stdin (pipe) to a raw device.
///
/// Reads header, L1, L2, and data sequentially without seeking input, and
/// validates that every L1/L2 entry points at the exact position the streaming
/// layout requires. Any image not written by `write_qcow2_stream` (e.g. a
/// seek-written file piped in) is rejected rather than silently mis-restored.
pub fn qcow2_stream_to_raw(infd: BorrowedFd<'_>, outfd: BorrowedFd<'_>) -> Result<()> {
    let mut hdr_buf = [0u8; QCOW2_HDR_BYTES];
    read_exact_seq(infd, &mut hdr_buf)?;
    let h = decode_validated_header(&hdr_buf)?;

    let bs = h.block_size as u64;
    let l2_size = h.block_size as usize / 8;

    let mut discard = vec![0u8; h.block_size as usize - QCOW2_HDR_BYTES];
    read_exact_seq(infd, &mut discard)?;

    prepare_output(outfd, h.image_size)?;

    let mut l1_buf = vec![0u8; h.l1_size * 8];
    read_exact_seq(infd, &mut l1_buf)?;

    let l1_clusters = round_up(h.l1_size as u64 * 8, bs) / bs;
    let l1_pad = (l1_clusters * bs) as usize - h.l1_size * 8;
    discard.resize(l1_pad, 0);
    read_exact_seq(infd, &mut discard)?;

    // Expected stream offset of the next unread cluster.
    let mut pos = bs + l1_clusters * bs;

    let mut l2_buf = vec![0u8; h.block_size as usize];
    let mut copybuf = vec![0u8; COPY_BUF_SIZE];

    for i in 0..h.l1_size {
        let l1_entry = read_be_u64(&l1_buf, i * 8);
        if l1_entry == 0 {
            continue;
        }
        if l1_entry & QCOW_OFLAG_COMPRESSED != 0 {
            return Err(anyhow!(
                "qcow2 L1 entry {i} has the compressed flag set ({l1_entry:#018x}); compressed images are unsupported"));
        }
        let l2_off = l1_entry & !QCOW_OFLAG_COPIED;
        if l2_off != pos {
            return Err(anyhow!(
                "image is not in streaming layout: L1 entry {i} points to L2 at {l2_off:#x}, expected {pos:#x}; save to a file and run `bcachefs undump <file.qcow2>`"));
        }
        read_exact_seq(infd, &mut l2_buf)?;
        pos += bs;

        // Coalesce each maximal run of consecutive nonzero L2 entries into one
        // read + one pwrite; validate every entry against its required offset.
        let mut j = 0usize;
        while j < l2_size {
            if read_be_u64(&l2_buf, j * 8) == 0 {
                j += 1;
                continue;
            }
            let run_start = j;
            let mut run_clusters = 0u64;
            while j < l2_size {
                let entry = read_be_u64(&l2_buf, j * 8);
                if entry == 0 {
                    break;
                }
                if entry & QCOW_OFLAG_COMPRESSED != 0 {
                    return Err(anyhow!(
                        "qcow2 L2 entry [{i}][{j}] has the compressed flag set ({entry:#018x}); compressed images are unsupported"));
                }
                let off = entry & !QCOW_OFLAG_COPIED;
                let expected = pos + run_clusters * bs;
                if off != expected {
                    return Err(anyhow!(
                        "image is not in streaming layout: L2 entry [{i}][{j}] points to {off:#x}, expected {expected:#x}; save to a file and run `bcachefs undump <file.qcow2>`"));
                }
                run_clusters += 1;
                j += 1;
            }

            let dst_base = (i as u64 * l2_size as u64 + run_start as u64) * bs;
            if dst_base >= h.image_size {
                return Err(anyhow!(
                    "qcow2 L2 entry [{i}][{run_start}] maps to {dst_base:#x}, past image size {:#x}", h.image_size));
            }
            let total = run_clusters * bs;
            let mut done = 0u64;
            while done < total {
                let chunk = (total - done).min(COPY_BUF_SIZE as u64);
                read_exact_seq(infd, &mut copybuf[..chunk as usize])?;
                let dst = dst_base + done;
                // Destination may end mid-cluster; clamp the final write.
                let writable = h.image_size.saturating_sub(dst).min(chunk);
                if writable == 0 {
                    return Err(anyhow!(
                        "qcow2 data cluster maps to {dst:#x}, past image size {:#x}", h.image_size));
                }
                pwrite_all(outfd, &copybuf[..writable as usize], dst)?;
                done += chunk;
                pos += chunk;
            }
        }
    }

    Ok(())
}

// ---- qcow2 format constants ----

const QCOW_MAGIC: u32 = (b'Q' as u32) << 24 | (b'F' as u32) << 16 | (b'I' as u32) << 8 | 0xfb;
const QCOW_VERSION: u32 = 2;
const QCOW_OFLAG_COPIED: u64 = 1 << 63;
const QCOW_OFLAG_COMPRESSED: u64 = 1 << 62;

// Validation limits for untrusted (piped/on-disk) qcow2 headers.
const QCOW2_MAX_BLOCK_BITS: u32 = 21; // qemu's MAX_CLUSTER_BITS
const QCOW2_MAX_IMAGE_SIZE: u64 = 1 << 50; // 1 PiB
const QCOW2_MAX_L1_BYTES: u64 = 1 << 30; // 1 GiB; a pipe reader must hold the whole L1
const COPY_BUF_SIZE: usize = 4 << 20; // >= any valid block_size (<= 2 MiB)

// Header field offsets and sizes (all big-endian on disk):
//   magic:                  u32   @ 0
//   version:                u32   @ 4
//   backing_file_offset:    u64   @ 8
//   backing_file_size:      u32   @ 16
//   block_bits:             u32   @ 20
//   size:                   u64   @ 24
//   crypt_method:           u32   @ 32
//   l1_size:                u32   @ 36
//   l1_table_offset:        u64   @ 40
//   refcount_table_offset:  u64   @ 48
//   refcount_table_blocks:  u32   @ 56
//   nb_snapshots:           u32   @ 60
//   snapshots_offset:       u64   @ 64
const QCOW2_HDR_BYTES: usize = 72;

fn encode_header(buf: &mut [u8], magic: u32, version: u32, block_bits: u32,
                 size: u64, l1_size: u32, l1_table_offset: u64) {
    buf[0..4].copy_from_slice(&magic.to_be_bytes());
    buf[4..8].copy_from_slice(&version.to_be_bytes());
    buf[8..16].copy_from_slice(&0u64.to_be_bytes());       // backing_file_offset
    buf[16..20].copy_from_slice(&0u32.to_be_bytes());       // backing_file_size
    buf[20..24].copy_from_slice(&block_bits.to_be_bytes());
    buf[24..32].copy_from_slice(&size.to_be_bytes());
    buf[32..36].copy_from_slice(&0u32.to_be_bytes());       // crypt_method
    buf[36..40].copy_from_slice(&l1_size.to_be_bytes());
    buf[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
    buf[48..56].copy_from_slice(&0u64.to_be_bytes());       // refcount_table_offset
    buf[56..60].copy_from_slice(&0u32.to_be_bytes());       // refcount_table_blocks
    buf[60..64].copy_from_slice(&0u32.to_be_bytes());       // nb_snapshots
    buf[64..72].copy_from_slice(&0u64.to_be_bytes());       // snapshots_offset
}

fn read_be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_be_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(buf[off..off + 8].try_into().unwrap())
}

#[cfg_attr(test, derive(Debug))]
struct Qcow2Header {
    block_size:      u32,
    image_size:      u64,
    l1_size:         usize,
    l1_table_offset: u64,
}

/// Parse and validate a qcow2 header from at least the first `QCOW2_HDR_BYTES`.
/// Shared by both readers so untrusted input is checked identically.
fn decode_validated_header(hdr: &[u8]) -> Result<Qcow2Header> {
    let magic = read_be_u32(hdr, 0);
    if magic != QCOW_MAGIC {
        return Err(anyhow!("not a qcow2 image: magic {magic:#010x} != {QCOW_MAGIC:#010x}"));
    }
    let version = read_be_u32(hdr, 4);
    if version != QCOW_VERSION {
        return Err(anyhow!("unsupported qcow2 version {version}, expected {QCOW_VERSION}"));
    }
    let backing_file_offset = read_be_u64(hdr, 8);
    if backing_file_offset != 0 {
        return Err(anyhow!(
            "qcow2 backing files are unsupported: backing_file_offset {backing_file_offset:#x}"));
    }
    let block_bits = read_be_u32(hdr, 20);
    if !(9..=QCOW2_MAX_BLOCK_BITS).contains(&block_bits) {
        return Err(anyhow!(
            "qcow2 block_bits {block_bits} out of range 9..={QCOW2_MAX_BLOCK_BITS}"));
    }
    let block_size = 1u32 << block_bits;

    let image_size = read_be_u64(hdr, 24);
    if image_size > QCOW2_MAX_IMAGE_SIZE {
        return Err(anyhow!(
            "qcow2 image size {image_size:#x} exceeds limit {QCOW2_MAX_IMAGE_SIZE:#x}"));
    }
    let crypt_method = read_be_u32(hdr, 32);
    if crypt_method != 0 {
        return Err(anyhow!("qcow2 encryption is unsupported: crypt_method {crypt_method}"));
    }

    let l1_size = read_be_u32(hdr, 36) as usize;
    let (expected_l1, _) = qcow2_geometry(block_size, image_size);
    if l1_size != expected_l1 {
        return Err(anyhow!(
            "qcow2 l1_size {l1_size} inconsistent with image size {image_size:#x} (expected {expected_l1})"));
    }
    let l1_bytes = l1_size as u64 * 8;
    if l1_bytes > QCOW2_MAX_L1_BYTES {
        return Err(anyhow!(
            "qcow2 L1 table {l1_bytes} bytes exceeds limit {QCOW2_MAX_L1_BYTES}"));
    }

    let l1_table_offset = read_be_u64(hdr, 40);
    Ok(Qcow2Header { block_size, image_size, l1_size, l1_table_offset })
}

// ---- Qcow2Image ----

pub struct Qcow2Image<'fd> {
    infd:       BorrowedFd<'fd>,
    outfd:      BorrowedFd<'fd>,
    image_size: u64,
    block_size: u32,
    l1_table:   Vec<u64>,
    l1_index:   Option<u32>,
    l2_table:   Vec<u64>,
    offset:     u64,
}

impl<'fd> Qcow2Image<'fd> {
    pub fn new(infd: BorrowedFd<'fd>, outfd: BorrowedFd<'fd>, block_size: u32) -> Result<Self> {
        assert!(block_size.is_power_of_two());

        let image_size = file_size_fd(infd)?;
        let (l1_size, l2_size) = qcow2_geometry(block_size, image_size);

        Ok(Qcow2Image {
            infd,
            outfd,
            image_size,
            block_size,
            l1_table:   vec![0u64; l1_size],
            l1_index:   None,
            l2_table:   vec![0u64; l2_size as usize],
            offset:     round_up(QCOW2_HDR_BYTES as u64, block_size as u64),
        })
    }

    /// Borrowed fd of the input device, for callers that need to read
    /// directly (e.g. sanitize path).
    pub fn infd(&self) -> BorrowedFd<'_> {
        self.infd
    }

    fn write_raw(&mut self, buf: &[u8]) -> Result<()> {
        assert!(buf.len() as u64 % self.block_size as u64 == 0);
        pwrite_all(self.outfd, buf, self.offset)?;
        self.offset += buf.len() as u64;
        Ok(())
    }

    fn flush_l2(&mut self) -> Result<()> {
        if let Some(idx) = self.l1_index {
            self.l1_table[idx as usize] = self.offset | QCOW_OFLAG_COPIED;

            let mut buf = vec![0u8; self.block_size as usize];
            pack_l2(&mut buf, &self.l2_table);
            self.write_raw(&buf)?;

            self.l2_table.fill(0);
            self.l1_index = None;
        }
        Ok(())
    }

    fn add_l2(&mut self, src_blk: u64, dst_offset: u64) -> Result<()> {
        let l2_size = self.block_size as u64 / 8;
        let l1_index = (src_blk / l2_size) as u32;
        let l2_index = (src_blk % l2_size) as usize;

        if self.l1_index != Some(l1_index) {
            self.flush_l2()?;
            self.l1_index = Some(l1_index);
        }

        self.l2_table[l2_index] = dst_offset | QCOW_OFLAG_COPIED;
        Ok(())
    }

    /// Write a buffer to the image, mapping src_offset blocks to the
    /// output position. buf.len() must be a multiple of block_size.
    pub fn write_buf(&mut self, buf: &[u8], src_offset: u64) -> Result<()> {
        let dst_offset = self.offset;
        self.write_raw(buf)?;

        let bs = self.block_size as u64;
        let nblocks = buf.len() as u64 / bs;
        for i in 0..nblocks {
            self.add_l2((src_offset + i * bs) / bs, dst_offset + i * bs)?;
        }
        Ok(())
    }

    /// Write ranges read from the input device to the image.
    /// Rounds up and merges the ranges in place.
    pub fn write_ranges(&mut self, ranges: &mut Ranges) -> Result<()> {
        ranges_roundup(ranges, self.block_size as u64);
        ranges_sort_merge(ranges);

        let bs = self.block_size as usize;
        let mut buf = vec![0u8; bs];

        for r in ranges.iter() {
            let mut src_offset = r.start;
            while src_offset < r.end {
                pread_exact(self.infd, &mut buf, src_offset)?;
                self.write_buf(&buf, src_offset)?;
                src_offset += bs as u64;
            }
        }
        Ok(())
    }

    /// Finalize the image: flush pending L2 entries, write L1 table
    /// and header. Consumes self.
    pub fn finish(mut self) -> Result<()> {
        self.flush_l2()?;

        // Write L1 table (big-endian)
        let l1_offset = self.offset;
        let l1_bytes: Vec<u8> = self.l1_table.iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        self.offset += round_up(l1_bytes.len() as u64, self.block_size as u64);
        pwrite_all(self.outfd, &l1_bytes, l1_offset)?;

        // Write header
        let mut header_buf = vec![0u8; self.block_size as usize];
        encode_header(
            &mut header_buf,
            QCOW_MAGIC,
            QCOW_VERSION,
            self.block_size.trailing_zeros(),
            self.image_size,
            self.l1_table.len() as u32,
            l1_offset,
        );
        pwrite_all(self.outfd, &header_buf, 0)?;

        Ok(())
    }
}

/// Convert a qcow2 image back to a raw device image.
pub fn qcow2_to_raw(infd: BorrowedFd<'_>, outfd: BorrowedFd<'_>) -> Result<()> {
    let mut hdr_buf = [0u8; QCOW2_HDR_BYTES];
    pread_exact(infd, &mut hdr_buf, 0)?;
    let h = decode_validated_header(&hdr_buf)?;

    let block_size = h.block_size;
    let image_size = h.image_size;
    let l1_size = h.l1_size;

    prepare_output(outfd, image_size)?;

    let l2_size = block_size as usize / 8;

    // Read L1 table
    let l1_offset = h.l1_table_offset;
    let mut l1_buf = vec![0u8; l1_size * 8];
    pread_exact(infd, &mut l1_buf, l1_offset)?;

    let mut l2_buf = vec![0u8; block_size as usize];
    let mut data_buf = vec![0u8; block_size as usize];

    for i in 0..l1_size {
        let l1_entry = read_be_u64(&l1_buf, i * 8);
        if l1_entry == 0 {
            continue;
        }

        pread_exact(infd, &mut l2_buf, l1_entry & !QCOW_OFLAG_COPIED)?;

        for j in 0..l2_size {
            let l2_entry = read_be_u64(&l2_buf, j * 8);
            let src_offset = l2_entry & !QCOW_OFLAG_COPIED;
            if src_offset == 0 {
                continue;
            }

            let dst_offset = (i as u64 * l2_size as u64 + j as u64) * block_size as u64;
            pread_exact(infd, &mut data_buf, src_offset)?;
            pwrite_all(outfd, &data_buf, dst_offset)?;
        }
    }

    Ok(())
}

fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

/// Get the size of a file or block device given a borrowed fd.
fn file_size_fd(fd: BorrowedFd<'_>) -> Result<u64> {
    use rustix::fs::{fstat, FileType};
    use rustix::ioctl::{self, Getter};

    let stat = fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::BlockDevice {
        const BLKGETSIZE64: ioctl::Opcode = bch_bindgen::c::BLKGETSIZE64 as ioctl::Opcode;
        Ok(unsafe { rustix::ioctl::ioctl(fd, Getter::<BLKGETSIZE64, u64>::new()) }?)
    } else {
        Ok(stat.st_size as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsFd;

    use super::*;

    fn pattern(i: usize) -> u8 {
        (i.wrapping_mul(2246822519) >> 13) as u8
    }

    fn make_src(path: &str, size: usize) -> Vec<u8> {
        let data: Vec<u8> = (0..size).map(pattern).collect();
        fs::write(path, &data).unwrap();
        data
    }

    // Write a stream, read it back through the sequential reader, return the raw output.
    fn stream_round_trip(name: &str, src: &str, block_size: u32, ranges: &mut Ranges) -> Vec<u8> {
        let stream = format!("target/qcow2-test-{name}-stream.qcow2");
        let raw = format!("target/qcow2-test-{name}-raw.img");

        {
            let infd = File::open(src).unwrap();
            let outfd = File::create(&stream).unwrap();
            write_qcow2_stream(infd.as_fd(), outfd.as_fd(), block_size, ranges).unwrap();
        }
        {
            let infd = File::open(&stream).unwrap();
            let outfd = OpenOptions::new().read(true).write(true).create(true).truncate(true)
                .open(&raw).unwrap();
            qcow2_stream_to_raw(infd.as_fd(), outfd.as_fd()).unwrap();
        }

        let out = fs::read(&raw).unwrap();
        fs::remove_file(&stream).unwrap();
        fs::remove_file(&raw).unwrap();
        out
    }

    fn expected_raw(src: &[u8], ranges: &Ranges) -> Vec<u8> {
        let mut exp = vec![0u8; src.len()];
        for r in ranges {
            let end = r.end.min(src.len() as u64);
            for o in r.start..end {
                exp[o as usize] = src[o as usize];
            }
        }
        exp
    }

    #[test]
    fn round_trip_straddling_and_unaligned() {
        let bs = 512u32; // group = bs * (bs/8) = 32768 bytes
        let src_path = "target/qcow2-test-straddle-src.img";
        let image_size = 165000usize; // not block-aligned
        let src = make_src(src_path, image_size);

        // group boundaries at 32768: [30000,40000) straddles g0/g1; empty g3.
        let mut ranges: Ranges = vec![
            30000..40000,
            70000..71000,
            140000..141000,
            164000..165000,
        ];
        let out = stream_round_trip("straddle", src_path, bs, &mut ranges);
        // ranges is now rounded + merged; rebuild the expected raw from it.
        let exp = expected_raw(&src, &ranges);

        fs::remove_file(src_path).unwrap();
        assert_eq!(out.len(), image_size);
        assert_eq!(out, exp);
    }

    #[test]
    fn round_trip_empty_ranges() {
        let bs = 512u32;
        let src_path = "target/qcow2-test-empty-src.img";
        let image_size = 100000usize;
        make_src(src_path, image_size);

        let mut ranges: Ranges = vec![];
        let out = stream_round_trip("empty", src_path, bs, &mut ranges);

        fs::remove_file(src_path).unwrap();
        assert_eq!(out.len(), image_size);
        assert!(out.iter().all(|&b| b == 0));
    }

    fn valid_header(block_bits: u32, image_size: u64, l1_size: u32) -> [u8; QCOW2_HDR_BYTES] {
        let mut hdr = [0u8; QCOW2_HDR_BYTES];
        encode_header(&mut hdr, QCOW_MAGIC, QCOW_VERSION, block_bits, image_size, l1_size, 512);
        hdr
    }

    #[test]
    fn header_valid_accepted() {
        let (l1, _) = qcow2_geometry(512, 100000);
        let hdr = valid_header(9, 100000, l1 as u32);
        assert!(decode_validated_header(&hdr).is_ok());
    }

    #[test]
    fn header_rejects_oversized_block_bits() {
        let hdr = valid_header(22, 100000, 0);
        let err = decode_validated_header(&hdr).unwrap_err().to_string();
        assert!(err.contains("block_bits"), "{err}");
    }

    #[test]
    fn header_rejects_huge_image_size() {
        let hdr = valid_header(9, QCOW2_MAX_IMAGE_SIZE + 1, 0);
        let err = decode_validated_header(&hdr).unwrap_err().to_string();
        assert!(err.contains("image size"), "{err}");
    }

    #[test]
    fn header_rejects_l1_size_mismatch() {
        let hdr = valid_header(9, 100000, 999);
        let err = decode_validated_header(&hdr).unwrap_err().to_string();
        assert!(err.contains("l1_size"), "{err}");
    }

    #[test]
    fn stream_rejects_compressed_entry() {
        let bs = 512u32;
        let src_path = "target/qcow2-test-compressed-src.img";
        let stream = "target/qcow2-test-compressed-stream.qcow2";
        let raw = "target/qcow2-test-compressed-raw.img";
        make_src(src_path, 100000);

        {
            let infd = File::open(src_path).unwrap();
            let outfd = File::create(stream).unwrap();
            let mut ranges: Ranges = vec![1000..2000];
            write_qcow2_stream(infd.as_fd(), outfd.as_fd(), bs, &mut ranges).unwrap();
        }

        // Set the compressed flag on the first (nonzero) L1 entry, at byte l1_off = bs.
        let mut bytes = fs::read(stream).unwrap();
        let l1_off = bs as usize;
        let mut entry = u64::from_be_bytes(bytes[l1_off..l1_off + 8].try_into().unwrap());
        assert_ne!(entry, 0);
        entry |= QCOW_OFLAG_COMPRESSED;
        bytes[l1_off..l1_off + 8].copy_from_slice(&entry.to_be_bytes());
        fs::write(stream, &bytes).unwrap();

        let infd = File::open(stream).unwrap();
        let outfd = OpenOptions::new().read(true).write(true).create(true).truncate(true)
            .open(raw).unwrap();
        let err = qcow2_stream_to_raw(infd.as_fd(), outfd.as_fd()).unwrap_err().to_string();

        fs::remove_file(src_path).unwrap();
        fs::remove_file(stream).unwrap();
        fs::remove_file(raw).unwrap();
        assert!(err.contains("compressed"), "{err}");
    }
}
