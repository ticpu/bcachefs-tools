//! Dump sanitize / de-replication transforms.
//!
//! In-place buffer transforms the write path applies before dumping: `--sanitize`
//! scrubbing (inline data, optionally filenames) and `--single-replica`
//! de-replication of btree pointers, plus the crypto that decrypts, edits, and
//! re-encrypts / re-checksums modified bsets and journal entries so they stay
//! checksum-valid. Everything here walks raw on-disk buffers by offset and, for
//! packed keys, a format-only unpack -- no struct btree, so none of
//! btree_node_read_done's repair runs on a dump.

use anyhow::{anyhow, bail, Result};

use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{AllTypes, AnyKey, BkeyP, BkeyS, FamilyVal};
use bcachefs_kernel::btree::read::BsetHolder;
use bcachefs_kernel::c;
use bcachefs_kernel::data::inline_data::InlineDataMut;
use bcachefs_kernel::data::ptrs::Ptrs;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::format::BkeyType;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::journal::{entry_type, JsetEntryType};
use bcachefs_kernel::util::vstructs::{cast_plain, cast_plain_slice, from_bytes_mut, Head,
                                      SelfSized, Unvalidated};

// The sanitize path's crypto: a bset - a node's first, or a later entry's, at
// byte @offset of the node - at the start of @buf, checked whole already.

/// Decrypted, or encrypted again: ChaCha20 is its own inverse.
fn bset_encrypt(fs: &Fs, buf: &mut [u8], first: bool, offset: u32) -> Result<(), BchError> {
    if first {
        c::btree_node::<Unvalidated>::of_mut(buf).crypt(fs, offset)
    } else {
        c::btree_node_entry::<Unvalidated>::of_mut(buf).crypt(fs, offset)
    }
}

fn bset_csum_set(fs: &Fs, buf: &mut [u8], first: bool, offset: u32) {
    fn set<H: BsetHolder + ?Sized>(fs: &Fs, n: &mut H, offset: u32) {
        n.set_csum(n.checksum(fs, offset));
    }

    if first {
        set(fs, c::btree_node::<Unvalidated>::of_mut(buf), offset)
    } else {
        set(fs, c::btree_node_entry::<Unvalidated>::of_mut(buf), offset)
    }
}

// ---- Sanitize implementation ----

/// Sanitize a bkey value region in-place. Returns true if modified.
///
///  - inline data, any type: its data zeroed, and its checksum, if it has one,
///    set to none
///  - dirent: d_inum(8), d_type(1), d_name — fill with 'X'
///
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
const MEMBER_INVALID: u8 = c::BCH_SB_MEMBER_INVALID as u8;

/// De-replicate a btree pointer key in place: keep the pointer on the lowest
/// device index and set every other pointer's device to `BCH_SB_MEMBER_INVALID`.
/// Returns whether it changed anything. The surviving replica is the one the
/// dump actually wrote, and the only one the read path will consider.
fn derep(fs: &Fs, k: BkeyS<'_, FamilyVal<AllTypes, Unvalidated>>) -> bool {
    let Some(mut k) = k.arm::<Ptrs>() else { return false };
    let min_dev = k.ptrs_mut(fs)
        .map(|p| p.dev())
        .filter(|&d| d != MEMBER_INVALID)
        .min();
    let Some(min_dev) = min_dev else { return false };

    let mut modified = false;
    for p in k.ptrs_mut(fs) {
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
fn sanitize_val(fs: &Fs, mut k: BkeyS<'_, FamilyVal<AllTypes, Unvalidated>>, opts: SanitizeOpts) -> bool {
    const BTREE_PTR:            BkeyType = BkeyType::btree_ptr;
    const BTREE_PTR_V2:         BkeyType = BkeyType::btree_ptr_v2;
    const DIRENT:               BkeyType = BkeyType::dirent;

    match k.k.key_type() {
        BTREE_PTR | BTREE_PTR_V2 if opts.single_replica => derep(fs, k),
        _ if opts.sanitize && k.k.extent_is_inline_data() => {
            if let Some(d) = InlineDataMut::of(k.as_s()) {
                d.data.fill(0);
                // zeroed, so no checksum: else every read of it fails
                if let Some(h) = d.csum {
                    *h = c::InlineDataCsum { flags: h.flags, offset: h.offset, ..Default::default() };
                }
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

/// Sanitize a journal buffer in-place: walk jset entries, handle encryption,
/// zero inline data, optionally scramble filenames, clear checksums.
pub(super) fn sanitize_journal(fs: &Fs, buf: &mut [u8], opts: SanitizeOpts) -> Result<()> {
    let jset_magic = fs.sb_lock().sb().jset_magic();
    let block_bits = fs.block_bits();

    // A jset that overruns what holds it - or an entry or key one - is an
    // error: the walk can't scrub it, and skipping it would dump it as it is
    let mut pos = 0;
    while cast_plain::<Head<c::jset>>(&buf[pos..]).is_some_and(|j| j.magic.get() == jset_magic) {
        let j = c::jset::<Unvalidated>::from_bytes_mut(&mut buf[pos..])
            .map_err(|e| anyhow!("journal entry at {pos:#x} overruns its bucket: {e}"))?;
        let csum_type = j.csum_type();
        let mut modified = false;

        if csum_type.is_encryption() {
            if fs.encryption_key().is_none() {
                bail!("journal entry at {pos:#x} is encrypted, and the filesystem has no key to decrypt it");
            }

            j.encrypt(&fs).map_err(|e| anyhow!("decrypting journal entry at {pos:#x}: {e}"))?;
            modified = true;
        }

        for e in j.entries_mut() {
            let e = e.map_err(|e| anyhow!("journal entry at {pos:#x}: {e}"))?;
            if matches!(entry_type(e), JsetEntryType::btree_keys
                                     | JsetEntryType::btree_root
                                     | JsetEntryType::write_buffer_keys) {
                for k in e.keys_mut().into_iter().flatten() {
                    let k = k.map_err(|e| anyhow!("journal entry at {pos:#x}: {e}"))?;
                    modified |= sanitize_val(&fs, BkeyS::from(k), opts);
                }
            }
        }

        if modified {
            // Re-encrypt (symmetric) if encrypted, then recompute the csum so
            // the entry stays checksum-valid instead of csum-cleared.
            if csum_type.is_encryption() {
                j.encrypt(&fs).map_err(|e| anyhow!("re-encrypting journal entry at {pos:#x}: {e}"))?;
            }
            j.csum = j.checksum(&fs);
        }

        pos += (j.sectors(block_bits) << 9).min(buf.len() - pos);
    }

    Ok(())
}

/// Sanitize a btree node buffer in-place: walk bset entries, handle
/// encryption, zero inline data, optionally scramble filenames.
pub(super) fn sanitize_btree(fs: &Fs, buf: &mut [u8], opts: SanitizeOpts) -> Result<()> {
    let bset_magic = fs.sb_lock().sb().bset_magic();
    let block_bits = fs.block_bits();

    // The node's packed-key format, from the btree_node header at the start of
    // the buffer, once it's decrypted: packed keys are unpacked against it.
    let mut format = c::bkey_format::default();

    let mut first = true;
    let mut seq: u64 = 0;
    let mut pos = 0;
    let mut bset_byte_offset: usize = 0;

    // The node, then its entries - each as long as its bset - until one isn't
    // this node's. A bset of this node's that overruns the buffer, or a key
    // that overruns its bset, is an error, as in sanitize_journal()
    let overrun = |pos: usize, e| anyhow!("btree node bset at {pos:#x} overruns the node: {e}");

    while pos < buf.len() {
        // where the bset is, and the whole's bytes and sectors
        let (bset_off, vstruct_bytes, sectors, csum_type) = if first {
            let Some(n) = cast_plain::<Head<c::btree_node>>(&buf[pos..]) else { break };
            if n.magic.get() != bset_magic {
                break;
            }
            let n = c::btree_node::<Unvalidated>::from_bytes(&buf[pos..]).map_err(|e| overrun(pos, e))?;
            seq = n.keys.seq.get();
            (pos + std::mem::offset_of!(Head<c::btree_node>, keys), n.bytes(), n.sectors(block_bits),
             n.keys.csum_type())
        } else {
            let Some(n) = cast_plain::<Head<c::btree_node_entry>>(&buf[pos..]) else { break };
            if n.keys.seq.get() != seq {
                break;
            }
            let n = c::btree_node_entry::<Unvalidated>::from_bytes(&buf[pos..]).map_err(|e| overrun(pos, e))?;
            (pos + std::mem::offset_of!(Head<c::btree_node_entry>, keys), n.bytes(), n.sectors(block_bits),
             n.keys.csum_type())
        };
        let mut modified = false;

        if csum_type.is_encryption() {
            if fs.encryption_key().is_none() {
                bail!("btree node bset at {pos:#x} is encrypted, and the filesystem has no key to decrypt it");
            }

            bset_encrypt(&fs, &mut buf[pos..], first, bset_byte_offset as u32)
                .map_err(|e| anyhow!("decrypting btree node bset at {pos:#x}: {e}"))?;
            modified = true;
        }

        // the node's header from its flags on is encrypted with its first bset
        if first {
            format = cast_plain::<Head<c::btree_node>>(buf).expect("a btree node's header, read above").format;
        }

        // Walk packed keys in bset data region
        let key_end = pos + vstruct_bytes;
        let mut key_pos = bset_off + std::mem::size_of::<Head<c::bset>>();
        while key_pos + 3 <= key_end {
            let key_u64s = buf[key_pos] as usize;
            if key_u64s == 0 {
                break;
            }

            let key_bytes = key_u64s * 8;
            if key_pos + key_bytes > key_end {
                bail!("btree node bset at {pos:#x}: key at {key_pos:#x}, {key_bytes} bytes, overruns the bset's end at {key_end:#x}");
            }

            let key_format = buf[key_pos + 1] & 0x7f;
            // KEY_FORMAT_LOCAL_BTREE (0) uses the btree node's packed format;
            // anything else (KEY_FORMAT_CURRENT = 1) is the canonical
            // unpacked layout.
            let key_hdr_u64s = if key_format == 0 { format.key_u64s as usize } else { c::BKEY_U64s };
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
                    let key = cast_plain_slice(&buf[key_pos..][..key_bytes])
                        .and_then(BkeyP::from_slice)
                        .ok_or_else(|| anyhow!("btree node bset at {pos:#x}: key at {key_pos:#x} not 8 byte aligned"))?;

                    u = format.unpack_key(key);
                    let v = buf[vs..].as_mut_ptr().cast::<c::bch_val>();
                    // SAFETY: the value after the packed key, in the node's
                    // buffer: as long as the unpacked key's u64s says
                    unsafe { BkeyS::from_parts(&mut u, v) }
                } else {
                    let ki = from_bytes_mut::<AnyKey<Unvalidated>>(&mut buf[key_pos..][..key_bytes])
                        .map_err(|e| anyhow!("btree node bset at {pos:#x}: key at {key_pos:#x}: {e}"))?;
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
            if csum_type.is_encryption() {
                bset_encrypt(&fs, &mut buf[pos..], first, bset_byte_offset as u32)
                    .map_err(|e| anyhow!("re-encrypting btree node bset at {pos:#x}: {e}"))?;
            }
            bset_csum_set(&fs, &mut buf[pos..], first, bset_byte_offset as u32);
        }

        first = false;
        let advance = (sectors << 9).min(buf.len() - pos);
        bset_byte_offset += advance;
        pos += advance;
    }

    Ok(())
}
