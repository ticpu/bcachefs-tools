//! Dump sanitize / de-replication transforms.
//!
//! In-place buffer transforms the write path applies before dumping: `--sanitize`
//! scrubbing (inline data, optionally filenames) and `--single-replica`
//! de-replication of btree pointers, plus the crypto that decrypts, edits, and
//! re-encrypts / re-checksums modified bsets and journal entries so they stay
//! checksum-valid. Everything here walks raw on-disk buffers by offset and, for
//! packed keys, a format-only unpack -- no struct btree, so none of
//! btree_node_read_done's repair runs on a dump.

use bcachefs_kernel::btree::bkey::BkeyS;
use bcachefs_kernel::c;
use bcachefs_kernel::data::extents::bkey_ptrs_mut;
use bcachefs_kernel::fs::Fs;

// Crypto for the sanitize path. These drive the wrapped bch2_encrypt /
// bch2_checksum (and bset_encrypt) over the same byte ranges the kernel's
// bset_encrypt() and csum_vstruct() macro cover, so the metadata dump can
// decrypt, edit, and re-checksum bsets and journal entries without a C shim.
// @vstruct_bytes is the whole vstruct's byte length; the checksum covers
// everything after the leading csum field, and journal encryption everything
// after the encrypted_start marker. Encryption is symmetric, so the same call
// both decrypts and re-encrypts.

fn jset_encrypt(fs: &Fs, j: *mut c::jset, csum_type: u32, vstruct_bytes: usize) -> i32 {
    let off = std::mem::offset_of!(c::jset, encrypted_start);
    unsafe {
        c::bch2_encrypt(fs.raw, csum_type, c::journal_nonce(j),
                        (j as *mut u8).add(off) as *mut core::ffi::c_void,
                        vstruct_bytes - off)
    }
}

fn jset_csum_set(fs: &Fs, j: *mut c::jset, csum_type: u32, vstruct_bytes: usize) {
    let off = std::mem::size_of::<c::bch_csum>();
    let csum = unsafe {
        c::bch2_checksum(fs.raw, csum_type, c::journal_nonce(j),
                         (j as *const u8).add(off) as *const core::ffi::c_void,
                         vstruct_bytes - off)
    };
    unsafe { (*j).csum = csum };
}

/// @node points at the btree_node (first bset) or btree_node_entry, whose first
/// field is the csum; @i is the bset within it, at byte @offset from the node.
fn bset_csum_set(fs: &Fs, node: *mut u8, i: *mut c::bset, offset: u32,
                 csum_type: u32, vstruct_bytes: usize) {
    let off = std::mem::size_of::<c::bch_csum>();
    let csum = unsafe {
        c::bch2_checksum(fs.raw, csum_type, c::btree_nonce(i, offset),
                         node.add(off) as *const core::ffi::c_void,
                         vstruct_bytes - off)
    };
    unsafe { *(node as *mut c::bch_csum) = csum };
}

/// First 8 bytes of the superblock UUID interpreted as a little-endian u64.
fn sb_magic(sb: &c::bch_sb) -> u64 {
    u64::from_le_bytes(sb.uuid.b[..8].try_into().unwrap())
}

/// Journal set magic: sb_magic XOR JSET_MAGIC constant.
fn jset_magic(sb: &c::bch_sb) -> u64 {
    sb_magic(sb) ^ 0x245235c1a3625032
}

/// Btree set magic: sb_magic XOR BSET_MAGIC constant.
fn bset_magic(sb: &c::bch_sb) -> u64 {
    sb_magic(sb) ^ 0x90135c78b99e07f5
}

// ---- Sanitize implementation ----

// On-disk struct sizes (all __packed, little-endian x86_64)
const JSET_HDR: usize = 56;            // offsetof(jset, _data)
const JSET_ENTRY_HDR: usize = 8;       // offsetof(jset_entry, start)
const BSET_HDR: usize = 24;            // offsetof(bset, _data)
const BTREE_NODE_KEYS: usize = 136;    // offsetof(btree_node, keys)
const BNE_KEYS: usize = 16;            // offsetof(btree_node_entry, keys)
const BKEY_U64S: usize = 5;            // sizeof(bkey) / 8

fn read_le64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn read_le32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_le16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn csum_type_is_encryption(csum_type: u32) -> bool {
    csum_type == 3 || csum_type == 4 // chacha20_poly1305_{80,128}
}

/// Round `bytes` up to the block alignment determined by `block_bits`,
/// matching C `vstruct_sectors(s, block_bits) << 9`.
fn vstruct_aligned_bytes(bytes: usize, block_bits: usize) -> usize {
    let align = 512 << block_bits;
    bytes.div_ceil(align) * align
}

/// Sanitize a bkey value region in-place. Returns true if modified.
///
/// On-disk value layout (bch_val = 0 bytes):
///  - inline_data:          data at offset 0 — zero all
///  - indirect_inline_data: refcount(8), data — zero data
///  - dirent:               d_inum(8), d_type(1), d_name — fill with 'X'
/// What the write path should do to each buffer before dumping it.
#[derive(Clone, Copy)]
pub(super) struct SanitizeOpts {
    /// Zero inline data extents, and (with `sanitize_filenames`) scramble dirent
    /// names.
    pub(super) sanitize:           bool,
    pub(super) sanitize_filenames: bool,
    /// Keep only the lowest-device-index replica of each btree_ptr, rewriting
    /// the rest to an invalid device.
    pub(super) single_replica:     bool,
}

/// A member index that names no device (`BCH_SB_MEMBER_INVALID`); the read
/// path skips ptrs pointing at it.
const MEMBER_INVALID: u64 = c::BCH_SB_MEMBER_INVALID as u64;

/// De-replicate a btree pointer key in place: keep the pointer on the lowest
/// device index and set every other pointer's device to `BCH_SB_MEMBER_INVALID`.
/// Returns whether it changed anything. The surviving replica is the one the
/// dump actually wrote, and the only one the read path will consider.
fn derep(fs: &Fs, mut k: BkeyS) -> bool {
    let min_dev = bkey_ptrs_mut(fs, &mut k)
        .map(|p| p.dev())
        .filter(|&d| d != MEMBER_INVALID)
        .min();
    let Some(min_dev) = min_dev else { return false };

    let mut modified = false;
    for p in bkey_ptrs_mut(fs, &mut k) {
        if p.dev() != min_dev {
            p.set_dev(MEMBER_INVALID);
            modified = true;
        }
    }
    modified
}

/// Per-value transform, dispatched by key type. Handles both `--single-replica`
/// de-replication (btree pointers) and `--sanitize` scrubbing (inline data,
/// filenames). Called for every key in both btree nodes and journal entries, so
/// de-replication reaches the btree_root pointers carried in the journal as well
/// as the interior/leaf pointers in btree nodes. Returns whether it modified.
fn sanitize_val(fs: &Fs, mut k: BkeyS, opts: SanitizeOpts) -> bool {
    const BTREE_PTR:            c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_btree_ptr;
    const BTREE_PTR_V2:         c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_btree_ptr_v2;
    const INLINE_DATA:          c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_inline_data;
    const INDIRECT_INLINE_DATA: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_indirect_inline_data;
    const DIRENT:               c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_dirent;

    match k.key_type() {
        BTREE_PTR | BTREE_PTR_V2 if opts.single_replica => derep(fs, k),
        INLINE_DATA if opts.sanitize => {
            k.val_bytes_mut().fill(0);
            true
        }
        INDIRECT_INLINE_DATA if opts.sanitize => {
            let val = k.val_bytes_mut();
            if val.len() > 8 {
                val[8..].fill(0);
            }
            true
        }
        DIRENT if opts.sanitize && opts.sanitize_filenames => {
            let val = k.val_bytes_mut();
            if val.len() > 9 {
                val[9..].fill(b'X');
            }
            true
        }
        _ => false,
    }
}

/// Walk unpacked bkey_i entries in a jset_entry data region and sanitize.
fn sanitize_journal_keys(
    fs: &Fs,
    buf: &mut [u8],
    start: usize,
    end: usize,
    opts: SanitizeOpts,
) -> bool {
    let mut modified = false;
    let mut pos = start;

    while pos + 3 <= end {
        let key_u64s = buf[pos] as usize;
        if key_u64s == 0 {
            break;
        }

        let key_bytes = key_u64s * 8;
        if pos + key_bytes > end {
            break;
        }

        // Journal keys are always unpacked, so buf[pos..] is a bkey_i.
        let ki = unsafe { &mut *(buf.as_mut_ptr().add(pos) as *mut c::bkey_i) };
        if sanitize_val(fs, BkeyS::from(ki), opts) {
            modified = true;
        }

        pos += key_bytes;
    }

    modified
}

/// Sanitize a journal buffer in-place: walk jset entries, handle encryption,
/// zero inline data, optionally scramble filenames, clear checksums.
pub(super) fn sanitize_journal(fs_raw: *mut c::bch_fs, buf: &mut [u8], opts: SanitizeOpts) {
    let fs = unsafe { Fs::borrow_raw(fs_raw) };
    let jset_magic = jset_magic(unsafe { &*(*fs_raw).disk_sb.sb });
    let block_bits = unsafe { (*fs_raw).block_bits } as usize;

    let mut pos = 0;
    while pos + JSET_HDR <= buf.len() {
        if read_le64(buf, pos + 16) != jset_magic {
            break;
        }

        let u64s = read_le32(buf, pos + 40) as usize;
        let vstruct_bytes = JSET_HDR + u64s * 8;
        if vstruct_bytes > buf.len() - pos {
            break;
        }

        let csum_type = read_le32(buf, pos + 36) & 0xf;
        let mut modified = false;

        if csum_type_is_encryption(csum_type) {
            if !unsafe { (*fs_raw).chacha20_key_set } {
                eprintln!("found encrypted journal entry on non-encrypted filesystem");
                return;
            }

            let j = unsafe { buf.as_mut_ptr().add(pos) } as *mut c::jset;
            let ret = jset_encrypt(&fs, j, csum_type, vstruct_bytes);
            if ret != 0 {
                eprintln!("error decrypting journal entry: {}", ret);
                return;
            }
            modified = true;
        }

        // Walk jset entries: each is 8-byte header + u64s * 8 data
        let data_end = (pos + vstruct_bytes).min(buf.len());
        let mut entry_pos = pos + JSET_HDR;

        while entry_pos + JSET_ENTRY_HDR <= data_end {
            let entry_u64s = read_le16(buf, entry_pos) as usize;
            let entry_end = entry_pos + JSET_ENTRY_HDR + entry_u64s * 8;
            if entry_end > data_end {
                break;
            }

            // jset_entry_is_key: btree_keys(0), btree_root(1), write_buffer_keys(11)
            let entry_type = buf[entry_pos + 4];
            if (entry_type == 0 || entry_type == 1 || entry_type == 11)
                && sanitize_journal_keys(&fs, buf, entry_pos + JSET_ENTRY_HDR,
                                         entry_end, opts) {
                modified = true;
            }

            entry_pos = entry_end;
        }

        if modified {
            // Re-encrypt (symmetric) if encrypted, then recompute the csum so
            // the entry stays checksum-valid instead of csum-cleared.
            let j = unsafe { buf.as_mut_ptr().add(pos) } as *mut c::jset;
            if csum_type_is_encryption(csum_type) {
                jset_encrypt(&fs, j, csum_type, vstruct_bytes);
            }
            jset_csum_set(&fs, j, csum_type, vstruct_bytes);
        }

        pos += vstruct_aligned_bytes(vstruct_bytes, block_bits)
            .min(buf.len() - pos);
    }
}

/// Sanitize a btree node buffer in-place: walk bset entries, handle
/// encryption, zero inline data, optionally scramble filenames.
pub(super) fn sanitize_btree(fs_raw: *mut c::bch_fs, buf: &mut [u8], opts: SanitizeOpts) {
    let fs = unsafe { Fs::borrow_raw(fs_raw) };
    let bset_magic = bset_magic(unsafe { &*(*fs_raw).disk_sb.sb });
    let block_bits = unsafe { (*fs_raw).block_bits } as usize;

    // The node's packed-key format lives in the btree_node header at the start
    // of the buffer; packed keys are unpacked against it.
    let format_ptr = unsafe {
        buf.as_ptr().add(std::mem::offset_of!(c::btree_node, format)) as *const c::bkey_format
    };

    let mut first = true;
    let mut seq: u64 = 0;
    let mut format_key_u64s: usize = BKEY_U64S;
    let mut pos = 0;
    let mut bset_byte_offset: usize = 0;

    while pos < buf.len() {
        let (bset_off, data_off, vstruct_bytes);

        if first {
            if pos + BTREE_NODE_KEYS + BSET_HDR > buf.len() {
                break;
            }
            if read_le64(buf, pos + 16) != bset_magic {
                break;
            }

            bset_off = pos + BTREE_NODE_KEYS;
            data_off = pos + BTREE_NODE_KEYS + BSET_HDR;
            format_key_u64s = buf[pos + 80] as usize; // btree_node.format.key_u64s
            seq = read_le64(buf, bset_off);            // bset.seq
            let u64s = read_le16(buf, bset_off + 22) as usize;
            vstruct_bytes = BTREE_NODE_KEYS + BSET_HDR + u64s * 8;
        } else {
            if pos + BNE_KEYS + BSET_HDR > buf.len() {
                break;
            }

            bset_off = pos + BNE_KEYS;
            data_off = pos + BNE_KEYS + BSET_HDR;
            if read_le64(buf, bset_off) != seq {
                break;
            }
            let u64s = read_le16(buf, bset_off + 22) as usize;
            vstruct_bytes = BNE_KEYS + BSET_HDR + u64s * 8;
        }

        if pos + vstruct_bytes > buf.len() {
            break;
        }

        let csum_type = read_le32(buf, bset_off + 16) & 0xf;
        let mut modified = false;

        if csum_type_is_encryption(csum_type) {
            if !unsafe { (*fs_raw).chacha20_key_set } {
                eprintln!("found encrypted btree node on non-encrypted filesystem");
                return;
            }

            let ret = unsafe {
                c::bset_encrypt(fs.raw, buf.as_mut_ptr().add(bset_off) as *mut c::bset,
                                bset_byte_offset as u32)
            };
            if ret != 0 {
                eprintln!("error decrypting btree node: {}", ret);
                return;
            }
            modified = true;
        }

        // Walk packed keys in bset data region
        let u64s = read_le16(buf, bset_off + 22) as usize;
        let key_end = (data_off + u64s * 8).min(buf.len());

        let mut key_pos = data_off;
        while key_pos + 3 <= key_end {
            let key_u64s = buf[key_pos] as usize;
            if key_u64s == 0 {
                break;
            }

            let key_bytes = key_u64s * 8;
            if key_pos + key_bytes > key_end {
                break;
            }

            let key_format = buf[key_pos + 1] & 0x7f;
            // KEY_FORMAT_LOCAL_BTREE (0) uses the btree node's packed format;
            // anything else (KEY_FORMAT_CURRENT = 1) is the canonical
            // unpacked layout.
            let key_hdr_u64s = if key_format == 0 { format_key_u64s } else { BKEY_U64S };
            let val_off = key_hdr_u64s * 8;

            if val_off < key_bytes {
                let vs = key_pos + val_off;

                // An unpacked key (KEY_FORMAT_CURRENT) is a bkey_i in place; a
                // packed key is unpacked into a local bkey using the node format
                // only -- no struct btree, so none of btree_node_read_done's
                // repair runs. Either way the value is pointed at in place, so
                // sanitize_val's edits land on the buffer.
                let mut u: c::bkey;
                let k = if key_format == 0 {
                    u = unsafe { std::mem::zeroed() };
                    unsafe {
                        c::__bch2_bkey_unpack_key(
                            format_ptr,
                            &mut u,
                            buf.as_ptr().add(key_pos) as *const c::bkey_packed,
                        );
                    }
                    let v = unsafe { &mut *(buf.as_mut_ptr().add(vs) as *mut c::bch_val) };
                    BkeyS { k: &mut u, v }
                } else {
                    let ki = unsafe { &mut *(buf.as_mut_ptr().add(key_pos) as *mut c::bkey_i) };
                    BkeyS::from(ki)
                };

                if sanitize_val(&fs, k, opts) {
                    modified = true;
                }
            }

            key_pos += key_bytes;
        }

        if modified {
            // Re-encrypt (symmetric) if encrypted, then recompute the bset csum
            // over the btree_node (first bset) or btree_node_entry.
            let bset = unsafe { buf.as_mut_ptr().add(bset_off) } as *mut c::bset;
            if csum_type_is_encryption(csum_type) {
                unsafe { c::bset_encrypt(fs.raw, bset, bset_byte_offset as u32); }
            }
            let node = unsafe { buf.as_mut_ptr().add(pos) };
            bset_csum_set(&fs, node, bset, bset_byte_offset as u32, csum_type, vstruct_bytes);
        }

        first = false;
        let advance = vstruct_aligned_bytes(vstruct_bytes, block_bits)
            .min(buf.len() - pos);
        bset_byte_offset += advance;
        pos += advance;
    }
}
