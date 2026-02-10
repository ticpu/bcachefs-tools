use std::ffi::CStr;

use bch_bindgen::c;

use super::handle::BcachefsHandle;
use super::printbuf::Printbuf;
use super::ioctl::bch_ioc_w;
use super::sysfs::bcachefs_kernel_version;

/// Version below which bpos needs byte-swapping for accounting keys.
const VERSION_DISK_ACCOUNTING_BIG_ENDIAN: u64 = (1 << 10) | 15; // BCH_VERSION(1, 15) = 1039

/// Decoded accounting key type.
#[derive(Debug)]
#[allow(dead_code)]
pub enum DiskAccountingPos {
    NrInodes,
    PersistentReserved { nr_replicas: u8 },
    Replicas { data_type: u8, nr_devs: u8, nr_required: u8, devs: Vec<u8> },
    DevDataType { dev: u8, data_type: u8 },
    Compression { compression_type: u8 },
    Snapshot { id: u32 },
    Btree { id: u32 },
    RebalanceWork,
    Inum { inum: u64 },
    ReconcileWork { work_type: u8 },
    DevLeaving { dev: u32 },
    Unknown(u8),
}

/// Accounting type discriminants (must match BCH_DISK_ACCOUNTING_TYPES enum).
mod acct_type {
    pub const NR_INODES: u8           = 0;
    pub const PERSISTENT_RESERVED: u8 = 1;
    pub const REPLICAS: u8            = 2;
    pub const DEV_DATA_TYPE: u8       = 3;
    pub const COMPRESSION: u8         = 4;
    pub const SNAPSHOT: u8            = 5;
    pub const BTREE: u8              = 6;
    pub const REBALANCE_WORK: u8     = 7;
    pub const INUM: u8               = 8;
    pub const RECONCILE_WORK: u8     = 9;
    pub const DEV_LEAVING: u8        = 10;
}

/// A single accounting entry from the ioctl.
#[derive(Debug)]
pub struct AccountingEntry {
    pub pos: DiskAccountingPos,
    pub bpos: c::bpos,
    pub counters: Vec<u64>,
}

/// Result of query_accounting ioctl.
pub struct AccountingResult {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    pub entries: Vec<AccountingEntry>,
}

/// Convert a bpos to a DiskAccountingPos by byte-reversing the 20-byte bpos
/// (memcpy_swab on little-endian) and parsing the type-tagged union.
fn bpos_to_disk_accounting_pos(p: &c::bpos) -> DiskAccountingPos {
    // bpos is 20 bytes: on little-endian, the accounting pos is the
    // byte-reversed form. We copy to a 20-byte LE array, then reverse all bytes.
    let mut raw = [0u8; 20];

    // Copy bpos fields into raw bytes in memory order (LE: snapshot, offset, inode)
    let snap_bytes = p.snapshot.to_ne_bytes();
    let off_bytes = p.offset.to_ne_bytes();
    let ino_bytes = p.inode.to_ne_bytes();
    raw[0..4].copy_from_slice(&snap_bytes);
    raw[4..12].copy_from_slice(&off_bytes);
    raw[12..20].copy_from_slice(&ino_bytes);

    // memcpy_swab: reverse all 20 bytes
    raw.reverse();

    // Now raw[0] is the accounting type
    let acct_type = raw[0];

    match acct_type {
        acct_type::NR_INODES => DiskAccountingPos::NrInodes,
        acct_type::PERSISTENT_RESERVED => DiskAccountingPos::PersistentReserved {
            nr_replicas: raw[1],
        },
        acct_type::REPLICAS => {
            let data_type = raw[1];
            let nr_devs = raw[2];
            let nr_required = raw[3];
            let devs = raw[4..4 + nr_devs as usize].to_vec();
            DiskAccountingPos::Replicas { data_type, nr_devs, nr_required, devs }
        }
        acct_type::DEV_DATA_TYPE => DiskAccountingPos::DevDataType {
            dev: raw[1],
            data_type: raw[2],
        },
        acct_type::COMPRESSION => DiskAccountingPos::Compression {
            compression_type: raw[1],
        },
        acct_type::SNAPSHOT => {
            // __packed __u32 id, stored big-endian in the swabbed bytes
            let id = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::Snapshot { id }
        }
        acct_type::BTREE => {
            let id = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::Btree { id }
        }
        acct_type::REBALANCE_WORK => DiskAccountingPos::RebalanceWork,
        acct_type::INUM => {
            let inum = u64::from_be_bytes([
                raw[1], raw[2], raw[3], raw[4],
                raw[5], raw[6], raw[7], raw[8],
            ]);
            DiskAccountingPos::Inum { inum }
        }
        acct_type::RECONCILE_WORK => DiskAccountingPos::ReconcileWork {
            work_type: raw[1],
        },
        acct_type::DEV_LEAVING => {
            let dev = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::DevLeaving { dev }
        }
        _ => DiskAccountingPos::Unknown(acct_type),
    }
}

/// Byte-swap a bpos in place (for old kernels that didn't do big-endian accounting).
fn bpos_swab(p: &mut c::bpos) {
    unsafe { c::bch2_bpos_swab(p) };
}

/// Header of bch_ioctl_query_accounting (fixed part before flex array).
#[repr(C)]
struct QueryAccountingHeader {
    capacity: u64,
    used: u64,
    online_reserved: u64,
    accounting_u64s: u32,
    accounting_types_mask: u32,
}

impl BcachefsHandle {
    /// Query filesystem accounting data via BCH_IOCTL_QUERY_ACCOUNTING.
    /// Returns None on ENOTTY (old kernel without this ioctl).
    pub fn query_accounting(&self, type_mask: u32) -> Result<AccountingResult, errno::Errno> {
        let hdr_size = std::mem::size_of::<QueryAccountingHeader>();
        let mut accounting_u64s: u32 = 128;

        loop {
            let total_bytes = hdr_size + (accounting_u64s as usize) * 8;
            let mut buf = vec![0u8; total_bytes];

            // Fill header
            let hdr = unsafe { &mut *(buf.as_mut_ptr() as *mut QueryAccountingHeader) };
            hdr.accounting_u64s = accounting_u64s;
            hdr.accounting_types_mask = type_mask;

            // BCH_IOCTL_QUERY_ACCOUNTING is _IOW(0xbc, 21, struct bch_ioctl_query_accounting)
            // The struct has a flex array, so the kernel uses the header size for the ioctl nr.
            // We use bch_ioc_w with the header size.
            let request = bch_ioc_w::<QueryAccountingHeader>(21);
            let ret = unsafe { libc::ioctl(self.ioctl_fd_raw(), request, buf.as_mut_ptr()) };

            if ret == 0 {
                let hdr = unsafe { &*(buf.as_ptr() as *const QueryAccountingHeader) };
                let entries = parse_accounting_entries(
                    &buf[hdr_size..hdr_size + (hdr.accounting_u64s as usize) * 8],
                );

                return Ok(AccountingResult {
                    capacity: hdr.capacity,
                    used: hdr.used,
                    online_reserved: hdr.online_reserved,
                    entries,
                });
            }

            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == libc::ENOTTY {
                return Err(errno::Errno(libc::ENOTTY));
            }
            if errno == libc::ERANGE {
                accounting_u64s *= 2;
                continue;
            }
            return Err(errno::Errno(errno));
        }
    }
}

/// Parse the raw u64 buffer of bkey_i_accounting entries.
///
/// Each entry starts with a `struct bkey` header (5 u64s = 40 bytes),
/// followed by counters. The `bkey.u64s` field gives the total size
/// of key + value in u64s.
fn parse_accounting_entries(data: &[u8]) -> Vec<AccountingEntry> {
    let mut entries = Vec::new();
    let kernel_version = bcachefs_kernel_version();
    let need_swab = kernel_version > 0 && kernel_version < VERSION_DISK_ACCOUNTING_BIG_ENDIAN;

    let u64s = |slice: &[u8], off: usize| -> u64 {
        let b = &slice[off..off + 8];
        u64::from_ne_bytes(b.try_into().unwrap())
    };

    let mut offset = 0;
    while offset < data.len() {
        // First byte of bkey is u64s
        if offset >= data.len() {
            break;
        }
        let key_u64s = data[offset] as usize;
        if key_u64s == 0 {
            break;
        }

        let entry_bytes = key_u64s * 8;
        if offset + entry_bytes > data.len() {
            break;
        }

        let entry_data = &data[offset..offset + entry_bytes];

        // bkey header is 5 u64s (40 bytes). The bpos is at the end of the bkey.
        // On little-endian: bkey layout is [u64s(1B), format:nw(1B), type(1B), pad(1B),
        //                                   bversion(12B), size(4B), bpos(20B)]
        // bpos starts at byte 20 (offset 20..40)
        const BKEY_U64S: usize = 5;
        const BPOS_OFFSET: usize = 20;

        if entry_bytes < BKEY_U64S * 8 {
            break;
        }

        // Extract bpos
        let mut bpos = c::bpos {
            snapshot: u32::from_ne_bytes(entry_data[BPOS_OFFSET..BPOS_OFFSET+4].try_into().unwrap()),
            offset: u64::from_ne_bytes(entry_data[BPOS_OFFSET+4..BPOS_OFFSET+12].try_into().unwrap()),
            inode: u64::from_ne_bytes(entry_data[BPOS_OFFSET+12..BPOS_OFFSET+20].try_into().unwrap()),
        };

        if need_swab {
            bpos_swab(&mut bpos);
        }

        let pos = bpos_to_disk_accounting_pos(&bpos);

        // Counters start after the bkey header (bch_accounting.d[])
        // bch_accounting has just a bch_val (0 bytes), then d[]
        // So counters start at u64 offset BKEY_U64S
        let nr_counters = key_u64s - BKEY_U64S;
        let mut counters = Vec::with_capacity(nr_counters);
        for i in 0..nr_counters {
            let c_offset = (BKEY_U64S + i) * 8;
            counters.push(u64s(entry_data, c_offset));
        }

        entries.push(AccountingEntry { pos, bpos, counters });
        offset += entry_bytes;
    }

    entries
}

/// Print a data type directly into a Printbuf via bch2_prt_data_type.
pub fn prt_data_type(out: &mut Printbuf, t: u8) {
    unsafe {
        c::bch2_prt_data_type(out.as_raw(), std::mem::transmute::<u32, c::bch_data_type>(t as u32));
    }
}

/// Print a compression type directly into a Printbuf via bch2_prt_compression_type.
pub fn prt_compression_type(out: &mut Printbuf, t: u8) {
    unsafe {
        c::bch2_prt_compression_type(out.as_raw(), std::mem::transmute::<u32, c::bch_compression_type>(t as u32));
    }
}

/// Print a reconcile accounting type directly into a Printbuf.
pub fn prt_reconcile_type(out: &mut Printbuf, t: u8) {
    unsafe {
        c::bch2_prt_reconcile_accounting_type(out.as_raw(), std::mem::transmute::<u32, c::bch_reconcile_accounting_type>(t as u32));
    }
}

/// Get a btree ID name string.
pub fn btree_id_str(id: u32) -> String {
    // bch2_btree_id_str takes an enum btree_id; we transmute from u32
    let btree_id: c::btree_id = unsafe { std::mem::transmute(id) };
    format!("{}", btree_id)
}

/// Get a member state string.
pub fn member_state_str(state: u8) -> &'static str {
    let ptr = unsafe { c::bch2_member_states[state as usize] };
    if ptr.is_null() {
        "unknown"
    } else {
        unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("unknown") }
    }
}
