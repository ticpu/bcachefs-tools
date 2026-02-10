use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use clap::Parser;

use crate::wrappers::accounting::{self, AccountingEntry, DiskAccountingPos};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use crate::wrappers::printbuf::Printbuf;
use crate::wrappers::sysfs::{self, DevInfo, bcachefs_kernel_version};

// Field bitmask values
const FIELD_REPLICAS: u32       = 1 << 0;
const FIELD_BTREE: u32          = 1 << 1;
const FIELD_COMPRESSION: u32    = 1 << 2;
const FIELD_REBALANCE_WORK: u32 = 1 << 3;
const FIELD_DEVICES: u32        = 1 << 4;

const FIELD_NAMES: &[(&str, u32)] = &[
    ("replicas",       FIELD_REPLICAS),
    ("btree",          FIELD_BTREE),
    ("compression",    FIELD_COMPRESSION),
    ("rebalance_work", FIELD_REBALANCE_WORK),
    ("devices",        FIELD_DEVICES),
];

/// Version at which reconcile replaced rebalance_work accounting.
const VERSION_RECONCILE: u64 = (1 << 10) | 33; // BCH_VERSION(1, 33) = 1057

/// BCH_SB_MEMBER_INVALID
const SB_MEMBER_INVALID: u8 = 255;

/// BCH_DATA_unstriped
const DATA_UNSTRIPED: u8 = 10;
/// BCH_DATA_cached
const DATA_CACHED: u8 = 5;
/// BCH_DATA_user
const DATA_USER: u8 = 4;
/// BCH_DATA_free
const DATA_FREE: u8 = 0;
/// BCH_DATA_need_gc_gens
const DATA_NEED_GC_GENS: u8 = 8;
/// BCH_DATA_need_discard
const DATA_NEED_DISCARD: u8 = 9;


#[derive(Parser, Debug)]
#[command(name = "usage", about = "Display detailed filesystem usage")]
pub struct Cli {
    /// Comma-separated list of fields: replicas,btree,compression,rebalance_work,devices
    #[arg(short = 'f', long = "fields", value_delimiter = ',')]
    fields: Vec<String>,

    /// Print all accounting fields
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable")]
    human_readable: bool,

    /// Filesystem mountpoints
    #[arg(default_value = ".")]
    mountpoints: Vec<String>,
}

pub fn fs_usage(argv: Vec<String>) -> Result<()> {
    let cli = Cli::try_parse_from(argv)?;

    let mut fields: u32 = 0;
    if cli.all {
        fields = !0;
    } else {
        for f in &cli.fields {
            let mut found = false;
            for &(name, bit) in FIELD_NAMES {
                if name == f.as_str() {
                    fields |= bit;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(anyhow!("unknown field '{}'; valid fields: {}",
                    f, FIELD_NAMES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")));
            }
        }
    }

    if fields == 0 {
        fields = FIELD_REBALANCE_WORK;
    }

    for path in &cli.mountpoints {
        let mut out = Printbuf::new();
        out.set_human_readable(cli.human_readable);
        fs_usage_to_text(&mut out, path, fields)?;
        print!("{}", out);
    }

    Ok(())
}

fn fmt_uuid(uuid: &[u8; 16]) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        uuid[0], uuid[1], uuid[2], uuid[3],
        uuid[4], uuid[5],
        uuid[6], uuid[7],
        uuid[8], uuid[9],
        uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15])
}

fn data_type_is_empty(t: u8) -> bool {
    matches!(t, DATA_FREE | DATA_NEED_GC_GENS | DATA_NEED_DISCARD)
}

struct DevContext {
    info: DevInfo,
    usage: DevUsage,
    leaving: u64,
}

fn fs_usage_to_text(out: &mut Printbuf, path: &str, fields: u32) -> Result<()> {
    let handle = BcachefsHandle::open(path)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path)?;

    // Try v1 (query_accounting), fall back to v0 on ENOTTY
    let v1_ok = match fs_usage_v1_to_text(out, &handle, &devs, fields) {
        Ok(()) => true,
        Err(e) if e.0 == libc::ENOTTY => false,
        Err(e) => return Err(anyhow!("query_accounting failed: {}", e)),
    };

    if !v1_ok {
        fs_usage_v0_to_text(out, &handle, &devs, fields)?;
    }

    devs_usage_to_text(out, &handle, &devs, fields)?;

    Ok(())
}

// ──────────────────────────── v1 path (query_accounting) ────────────────────

fn fs_usage_v1_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: u32,
) -> Result<(), errno::Errno> {
    let mut accounting_types: u32 =
        (1 << 2) |  // BCH_DISK_ACCOUNTING_replicas
        (1 << 1);   // BCH_DISK_ACCOUNTING_persistent_reserved

    if fields & FIELD_COMPRESSION != 0 {
        accounting_types |= 1 << 4; // compression
    }
    if fields & FIELD_BTREE != 0 {
        accounting_types |= 1 << 6; // btree
    }
    if fields & FIELD_REBALANCE_WORK != 0 {
        if bcachefs_kernel_version() < VERSION_RECONCILE {
            accounting_types |= 1 << 7; // rebalance_work
        } else {
            accounting_types |= 1 << 9;  // reconcile_work
            accounting_types |= 1 << 10; // dev_leaving
        }
    }

    let result = handle.query_accounting(accounting_types)?;

    // Sort entries by bpos
    let mut sorted: Vec<&AccountingEntry> = result.entries.iter().collect();
    sorted.sort_by(|a, b| a.bpos.cmp(&b.bpos));

    // Header
    write!(out, "Filesystem: {}\n", fmt_uuid(&handle.uuid())).unwrap();

    out.tabstops_reset();
    out.tabstop_push(20);
    out.tabstop_push(16);

    write!(out, "Size:\t").unwrap();
    out.units_u64(result.capacity << 9);
    write!(out, "\r\n").unwrap();

    write!(out, "Used:\t").unwrap();
    out.units_u64(result.used << 9);
    write!(out, "\r\n").unwrap();

    write!(out, "Online reserved:\t").unwrap();
    out.units_u64(result.online_reserved << 9);
    write!(out, "\r\n").unwrap();

    // Replicas summary
    replicas_summary_to_text(out, &sorted, devs);

    // Detailed replicas
    if fields & FIELD_REPLICAS != 0 {
        out.tabstops_reset();
        out.tabstop_push(16);
        out.tabstop_push(16);
        out.tabstop_push(14);
        out.tabstop_push(14);
        out.tabstop_push(14);
        write!(out, "\nData type\tRequired/total\tDurability\tDevices\n").unwrap();

        for entry in &sorted {
            match &entry.pos {
                DiskAccountingPos::PersistentReserved { nr_replicas } => {
                    let sectors = entry.counters.first().copied().unwrap_or(0) as i64;
                    if sectors == 0 { continue; }
                    write!(out, "reserved:\t1/{}\t[] ", nr_replicas).unwrap();
                    out.units_u64(sectors as u64 * 512);
                    write!(out, "\r\n").unwrap();
                }
                DiskAccountingPos::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                    let sectors = entry.counters.first().copied().unwrap_or(0) as i64;
                    if sectors == 0 { continue; }

                    let dur = replicas_durability(*data_type, *nr_devs, *nr_required, dev_list, devs);

                    accounting::prt_data_type(out, *data_type);
                    write!(out, ":\t{}/{}\t{}\t[", nr_required, nr_devs, dur.durability).unwrap();

                    prt_dev_list(out, dev_list, devs);
                    write!(out, "]\t").unwrap();

                    out.units_u64(sectors as u64 * 512);
                    write!(out, "\r\n").unwrap();
                }
                _ => {}
            }
        }
    }

    // Compression
    let mut first_compression = true;
    for entry in &sorted {
        if let DiskAccountingPos::Compression { compression_type } = &entry.pos {
            if first_compression {
                write!(out, "\nCompression:\n").unwrap();
                out.tabstops_reset();
                out.tabstop_push(12);
                out.tabstop_push(16);
                out.tabstop_push(16);
                out.tabstop_push(24);
                write!(out, "type\tcompressed\runcompressed\raverage extent size\r\n").unwrap();
                first_compression = false;
            }

            let nr_extents = entry.counters.first().copied().unwrap_or(0);
            let sectors_uncompressed = entry.counters.get(1).copied().unwrap_or(0);
            let sectors_compressed = entry.counters.get(2).copied().unwrap_or(0);

            accounting::prt_compression_type(out, *compression_type);
            out.tab();

            out.units_u64(sectors_compressed << 9);
            out.tab_rjust();

            out.units_u64(sectors_uncompressed << 9);
            out.tab_rjust();

            let avg = if nr_extents > 0 {
                (sectors_uncompressed << 9) / nr_extents
            } else { 0 };
            out.units_u64(avg);
            write!(out, "\r\n").unwrap();
        }
    }

    // Btree usage
    let mut first_btree = true;
    for entry in &sorted {
        if let DiskAccountingPos::Btree { id } = &entry.pos {
            if first_btree {
                write!(out, "\nBtree usage:\n").unwrap();
                out.tabstops_reset();
                out.tabstop_push(12);
                out.tabstop_push(16);
                first_btree = false;
            }
            write!(out, "{}:\t", accounting::btree_id_str(*id)).unwrap();
            out.units_u64(entry.counters.first().copied().unwrap_or(0) << 9);
            write!(out, "\r\n").unwrap();
        }
    }

    // Rebalance / reconcile work
    let mut first_rebalance = true;
    let mut first_reconcile = true;
    for entry in &sorted {
        match &entry.pos {
            DiskAccountingPos::RebalanceWork => {
                if first_rebalance {
                    write!(out, "\nPending rebalance work:\n").unwrap();
                    first_rebalance = false;
                }
                out.units_u64(entry.counters.first().copied().unwrap_or(0) << 9);
                out.newline();
            }
            DiskAccountingPos::ReconcileWork { work_type } => {
                if first_reconcile {
                    out.tabstops_reset();
                    out.tabstop_push(32);
                    out.tabstop_push(12);
                    out.tabstop_push(12);
                    write!(out, "\nPending reconcile:\tdata\rmetadata\r\n").unwrap();
                    first_reconcile = false;
                }
                accounting::prt_reconcile_type(out, *work_type);
                write!(out, ":").unwrap();
                out.tab();
                out.units_u64(entry.counters.first().copied().unwrap_or(0) << 9);
                out.tab_rjust();
                out.units_u64(entry.counters.get(1).copied().unwrap_or(0) << 9);
                out.tab_rjust();
                out.newline();
            }
            _ => {}
        }
    }

    Ok(())
}

// ──────────────────────────── Replicas summary ──────────────────────────────

struct DurabilityDegraded {
    durability: u32,
    minus_degraded: u32,
}

fn replicas_durability(
    _data_type: u8,
    nr_devs: u8,
    nr_required: u8,
    dev_list: &[u8],
    devs: &[DevInfo],
) -> DurabilityDegraded {
    let mut durability: u32 = 0;
    let mut degraded: u32 = 0;

    for &dev_idx in dev_list {
        let dev = devs.iter().find(|d| d.idx == dev_idx as u32);
        let dev_durability = dev.map_or(1, |d| d.durability);

        if dev.is_none() {
            degraded += dev_durability;
        }
        durability += dev_durability;
    }

    if nr_required > 1 {
        durability = (nr_devs - nr_required + 1) as u32;
    }

    let minus_degraded = durability.saturating_sub(degraded);

    DurabilityDegraded { durability, minus_degraded }
}

fn replicas_summary_to_text(
    out: &mut Printbuf,
    sorted: &[&AccountingEntry],
    devs: &[DevInfo],
) {
    // Build durability x degraded matrix
    let mut matrix: Vec<Vec<u64>> = Vec::new(); // [durability][degraded] = sectors
    let mut cached: u64 = 0;
    let mut reserved: u64 = 0;

    for entry in sorted {
        match &entry.pos {
            DiskAccountingPos::PersistentReserved { .. } => {
                reserved += entry.counters.first().copied().unwrap_or(0);
            }
            DiskAccountingPos::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                if *data_type == DATA_CACHED {
                    cached += entry.counters.first().copied().unwrap_or(0);
                    continue;
                }

                let d = replicas_durability(*data_type, *nr_devs, *nr_required, dev_list, devs);
                let degraded = d.durability - d.minus_degraded;

                while matrix.len() <= d.durability as usize {
                    matrix.push(Vec::new());
                }
                let row = &mut matrix[d.durability as usize];
                while row.len() <= degraded as usize {
                    row.push(0);
                }
                row[degraded as usize] += entry.counters.first().copied().unwrap_or(0);
            }
            _ => {}
        }
    }

    write!(out, "\nData by durability desired and amount degraded:\n").unwrap();

    let max_degraded = matrix.iter().map(|r| r.len()).max().unwrap_or(0);

    if max_degraded > 0 {
        // Header
        out.tabstops_reset();
        out.tabstop_push(8);
        out.tab();
        for i in 0..max_degraded {
            out.tabstop_push(12);
            if i == 0 {
                write!(out, "undegraded\r").unwrap();
            } else {
                write!(out, "-{}x\r", i).unwrap();
            }
        }
        out.newline();

        // Rows
        for (dur, row) in matrix.iter().enumerate() {
            if row.is_empty() { continue; }

            write!(out, "{}x:\t", dur).unwrap();

            for val in row {
                if *val != 0 {
                    out.units_u64(*val << 9);
                }
                out.tab_rjust();
            }
            out.newline();
        }
    }

    if cached > 0 {
        write!(out, "cached:\t").unwrap();
        out.units_u64(cached << 9);
        write!(out, "\r\n").unwrap();
    }
    if reserved > 0 {
        write!(out, "reserved:\t").unwrap();
        out.units_u64(reserved << 9);
        write!(out, "\r\n").unwrap();
    }
}

// ──────────────────────────── v0 path (legacy ioctl) ────────────────────────

/// Header of bch_ioctl_fs_usage (fixed part).
#[repr(C)]
struct FsUsageHeader {
    capacity: u64,
    used: u64,
    online_reserved: u64,
    persistent_reserved: [u64; 4], // BCH_REPLICAS_MAX = 4
    replica_entries_bytes: u32,
    pad: u32,
}

fn fs_usage_v0_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: u32,
) -> Result<()> {
    let hdr_size = std::mem::size_of::<FsUsageHeader>();
    let mut replica_entries_bytes: u32 = 4096;

    let buf = loop {
        let total = hdr_size + replica_entries_bytes as usize;
        let mut buf = vec![0u8; total];

        // Set replica_entries_bytes in the buffer
        let reb_offset = std::mem::offset_of!(FsUsageHeader, replica_entries_bytes);
        buf[reb_offset..reb_offset + 4].copy_from_slice(&replica_entries_bytes.to_ne_bytes());

        let request = crate::wrappers::ioctl::bch_ioc_wr::<FsUsageHeader>(11);
        let ret = unsafe { libc::ioctl(handle.ioctl_fd_raw(), request, buf.as_mut_ptr()) };

        if ret == 0 {
            break buf;
        }

        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::ERANGE {
            replica_entries_bytes *= 2;
            continue;
        }
        return Err(anyhow!("BCH_IOCTL_FS_USAGE error: {}", std::io::Error::from_raw_os_error(errno)));
    };

    let hdr = unsafe { &*(buf.as_ptr() as *const FsUsageHeader) };

    write!(out, "Filesystem: {}\n", fmt_uuid(&handle.uuid())).unwrap();

    out.tabstops_reset();
    out.tabstop_push(20);
    out.tabstop_push(16);

    write!(out, "Size:").unwrap();
    out.tab();
    out.units_u64(hdr.capacity << 9);
    write!(out, "\r\n").unwrap();

    write!(out, "Used:").unwrap();
    out.tab();
    out.units_u64(hdr.used << 9);
    write!(out, "\r\n").unwrap();

    write!(out, "Online reserved:").unwrap();
    out.tab();
    out.units_u64(hdr.online_reserved << 9);
    write!(out, "\r\n").unwrap();

    out.newline();

    out.tabstops_reset();
    out.tabstop_push(16);
    out.tabstop_push(16);
    out.tabstop_push(14);
    out.tabstop_push(14);
    out.tabstop_push(14);

    if fields & FIELD_REPLICAS != 0 {
        write!(out, "Data type\tRequired/total\tDurability\tDevices\n").unwrap();

        for i in 0..4 {
            let sectors = hdr.persistent_reserved[i] as i64;
            if sectors == 0 { continue; }
            write!(out, "reserved:\t1/{}\t[] ", i).unwrap();
            out.units_u64(sectors as u64 * 512);
            write!(out, "\r\n").unwrap();
        }

        // Parse variable-length replicas entries
        let entries_data = &buf[hdr_size..hdr_size + hdr.replica_entries_bytes as usize];
        let replica_entries = parse_replica_entries(entries_data);

        // Print in order: metadata, user nr_required<=1, user nr_required>1, rest
        for r in &replica_entries {
            if r.data_type < DATA_USER {
                print_replica_entry(out, r, devs);
            }
        }
        for r in &replica_entries {
            if r.data_type == DATA_USER && r.nr_required <= 1 {
                print_replica_entry(out, r, devs);
            }
        }
        for r in &replica_entries {
            if r.data_type == DATA_USER && r.nr_required > 1 {
                print_replica_entry(out, r, devs);
            }
        }
        for r in &replica_entries {
            if r.data_type > DATA_USER {
                print_replica_entry(out, r, devs);
            }
        }
    }

    Ok(())
}

struct ReplicaEntry {
    sectors: i64,
    data_type: u8,
    nr_devs: u8,
    nr_required: u8,
    devs: Vec<u8>,
}

fn parse_replica_entries(data: &[u8]) -> Vec<ReplicaEntry> {
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + 11 <= data.len() { // 8 (sectors) + 3 (data_type, nr_devs, nr_required)
        let sectors = i64::from_ne_bytes(data[offset..offset+8].try_into().unwrap());
        let data_type = data[offset + 8];
        let nr_devs = data[offset + 9];
        let nr_required = data[offset + 10];

        let entry_end = offset + 11 + nr_devs as usize;
        if entry_end > data.len() { break; }

        let devs = data[offset + 11..entry_end].to_vec();

        entries.push(ReplicaEntry { sectors, data_type, nr_devs, nr_required, devs });

        // Entries are packed, no alignment
        offset = entry_end;
    }

    entries
}

fn print_replica_entry(out: &mut Printbuf, r: &ReplicaEntry, devs: &[DevInfo]) {
    if r.sectors == 0 { return; }

    let dur = replicas_durability(r.data_type, r.nr_devs, r.nr_required, &r.devs, devs);

    accounting::prt_data_type(out, r.data_type);
    write!(out, ":\t{}/{}\t{}\t[", r.nr_required, r.nr_devs, dur.durability).unwrap();

    prt_dev_list(out, &r.devs, devs);
    write!(out, "]\t").unwrap();

    out.units_u64(r.sectors as u64 * 512);
    write!(out, "\r\n").unwrap();
}

/// Print a device list like [sda sdb sdc].
fn prt_dev_list(out: &mut Printbuf, dev_list: &[u8], devs: &[DevInfo]) {
    for (i, &dev_idx) in dev_list.iter().enumerate() {
        if i > 0 { write!(out, " ").unwrap(); }
        if dev_idx == SB_MEMBER_INVALID {
            write!(out, "none").unwrap();
        } else if let Some(d) = devs.iter().find(|d| d.idx == dev_idx as u32) {
            write!(out, "{}", d.dev).unwrap();
        } else {
            write!(out, "{}", dev_idx).unwrap();
        }
    }
}

// ──────────────────────────── Device usage ───────────────────────────────────

fn devs_usage_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: u32,
) -> Result<()> {
    // Query dev_leaving accounting if available
    let dev_leaving_map = match handle.query_accounting(1 << 10) {
        Ok(result) => result.entries,
        Err(_) => Vec::new(),
    };

    let mut dev_ctxs: Vec<DevContext> = Vec::new();
    for dev in devs {
        let usage = handle.dev_usage(dev.idx)
            .map_err(|e| anyhow!("getting usage for device {}: {}", dev.idx, e))?;

        let leaving = dev_leaving_sectors(&dev_leaving_map, dev.idx);

        dev_ctxs.push(DevContext {
            info: DevInfo {
                idx: dev.idx,
                dev: dev.dev.clone(),
                label: dev.label.clone(),
                durability: dev.durability,
            },
            usage,
            leaving,
        });
    }

    // Sort by label, then dev name, then idx
    dev_ctxs.sort_by(|a, b| {
        a.info.label.cmp(&b.info.label)
            .then(a.info.dev.cmp(&b.info.dev))
            .then(a.info.idx.cmp(&b.info.idx))
    });

    let has_leaving = dev_ctxs.iter().any(|d| d.leaving != 0);

    out.tabstops_reset();
    out.newline();

    if fields & FIELD_DEVICES != 0 {
        // Full per-device breakdown
        out.tabstop_push(16);
        out.tabstop_push(20);
        out.tabstop_push(16);
        out.tabstop_push(14);

        for d in &dev_ctxs {
            dev_usage_full_to_text(out, d);
        }
    } else {
        // Summary table
        out.tabstop_push(32);
        out.tabstop_push(12);
        out.tabstop_push(8);
        out.tabstop_push(10);
        out.tabstop_push(10);
        out.tabstop_push(6);
        out.tabstop_push(10);

        write!(out, "Device label\tDevice\tState\tSize\rUsed\rUse%\r").unwrap();
        if has_leaving {
            write!(out, "Leaving\r").unwrap();
        }
        out.newline();

        for d in &dev_ctxs {
            let u = &d.usage;
            let capacity = u.nr_buckets * u.bucket_size as u64;
            let mut used: u64 = 0;
            for (i, dt) in u.data_types.iter().enumerate() {
                if i as u8 != DATA_UNSTRIPED {
                    used += dt.sectors;
                }
            }

            let label = d.info.label.as_deref().unwrap_or("(no label)");
            let state = accounting::member_state_str(u.state);

            write!(out, "{} (device {}):\t{}\t{}\t", label, d.info.idx, d.info.dev, state).unwrap();

            out.units_u64(capacity << 9);
            out.tab_rjust();
            out.units_u64(used << 9);

            let pct = if capacity > 0 { used * 100 / capacity } else { 0 };
            write!(out, "\r{:02}%\r", pct).unwrap();

            if d.leaving > 0 {
                out.units_u64(d.leaving << 9);
                out.tab_rjust();
            }

            out.newline();
        }
    }

    Ok(())
}

fn dev_usage_full_to_text(out: &mut Printbuf, d: &DevContext) {
    let u = &d.usage;
    let capacity = u.nr_buckets * u.bucket_size as u64;
    let mut used: u64 = 0;
    for (i, dt) in u.data_types.iter().enumerate() {
        if i as u8 != DATA_UNSTRIPED {
            used += dt.sectors;
        }
    }

    let label = d.info.label.as_deref().unwrap_or("(no label)");
    let state = accounting::member_state_str(u.state);
    let pct = if capacity > 0 { used * 100 / capacity } else { 0 };

    write!(out, "{} (device {}):\t{}\r{}\r    {:02}%\n", label, d.info.idx, d.info.dev, state, pct).unwrap();

    out.indent_add(2);
    write!(out, "\tdata\rbuckets\rfragmented\r\n").unwrap();

    for (i, dt) in u.data_types.iter().enumerate() {
        accounting::prt_data_type(out, i as u8);
        write!(out, ":\t").unwrap();

        let sectors = if data_type_is_empty(i as u8) {
            dt.buckets * u.bucket_size as u64
        } else {
            dt.sectors
        };
        out.units_u64(sectors << 9);

        write!(out, "\r{}\r", dt.buckets).unwrap();

        if dt.fragmented > 0 {
            out.units_u64(dt.fragmented << 9);
        }
        write!(out, "\r\n").unwrap();
    }

    write!(out, "capacity:\t").unwrap();
    out.units_u64(capacity << 9);
    write!(out, "\r{}\r\n", u.nr_buckets).unwrap();

    write!(out, "bucket size:\t").unwrap();
    out.units_u64(u.bucket_size as u64 * 512);
    write!(out, "\r\n").unwrap();

    out.indent_sub(2);
    out.newline();
}

fn dev_leaving_sectors(entries: &[AccountingEntry], dev_idx: u32) -> u64 {
    for entry in entries {
        if let DiskAccountingPos::DevLeaving { dev } = &entry.pos {
            if *dev == dev_idx {
                return entry.counters.first().copied().unwrap_or(0);
            }
        }
    }
    0
}
