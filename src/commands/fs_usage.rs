use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;

use crate::wrappers::accounting::{self, AccountingEntry, DiskAccountingPos};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use crate::wrappers::printbuf::Printbuf;
use crate::wrappers::sysfs::{self, DevInfo, bcachefs_kernel_version};

use c::bch_data_type::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
enum Field {
    Replicas,
    Btree,
    Compression,
    RebalanceWork,
    Devices,
}

#[derive(Parser, Debug)]
#[command(name = "usage", about = "Display detailed filesystem usage")]
pub struct Cli {
    /// Comma-separated list of fields
    #[arg(short = 'f', long = "fields", value_delimiter = ',', value_enum)]
    fields: Vec<Field>,

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

    let fields: Vec<Field> = if cli.all {
        vec![Field::Replicas, Field::Btree, Field::Compression,
             Field::RebalanceWork, Field::Devices]
    } else if cli.fields.is_empty() {
        vec![Field::RebalanceWork]
    } else {
        cli.fields
    };

    for path in &cli.mountpoints {
        let mut out = Printbuf::new();
        out.set_human_readable(cli.human_readable);
        fs_usage_to_text(&mut out, path, &fields)?;
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
    t == BCH_DATA_free as u8 ||
    t == BCH_DATA_need_gc_gens as u8 ||
    t == BCH_DATA_need_discard as u8
}

struct DevContext {
    info: DevInfo,
    usage: DevUsage,
    leaving: u64,
}

fn fs_usage_to_text(out: &mut Printbuf, path: &str, fields: &[Field]) -> Result<()> {
    let handle = BcachefsHandle::open(path)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;

    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path)?;

    fs_usage_v1_to_text(out, &handle, &devs, fields)
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;

    devs_usage_to_text(out, &handle, &devs, fields)?;

    Ok(())
}

fn fs_usage_v1_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    devs: &[DevInfo],
    fields: &[Field],
) -> Result<(), errno::Errno> {
    let has = |f: Field| -> bool { fields.contains(&f) };

    let mut accounting_types: u32 =
        (1 << 2) |  // BCH_DISK_ACCOUNTING_replicas
        (1 << 1);   // BCH_DISK_ACCOUNTING_persistent_reserved

    if has(Field::Compression) {
        accounting_types |= 1 << 4; // compression
    }
    if has(Field::Btree) {
        accounting_types |= 1 << 6; // btree
    }
    if has(Field::RebalanceWork) {
        let version_reconcile =
            c::bcachefs_metadata_version::bcachefs_metadata_version_reconcile as u64;
        if bcachefs_kernel_version() < version_reconcile {
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
    if has(Field::Replicas) {
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
                    let sectors = entry.counter(0) as i64;
                    if sectors == 0 { continue; }
                    write!(out, "reserved:\t1/{}\t[] ", nr_replicas).unwrap();
                    out.units_u64(sectors as u64 * 512);
                    write!(out, "\r\n").unwrap();
                }
                DiskAccountingPos::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                    let sectors = entry.counter(0) as i64;
                    if sectors == 0 { continue; }

                    let dur = replicas_durability(*nr_devs, *nr_required, dev_list, devs);

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

            let nr_extents = entry.counter(0);
            let sectors_uncompressed = entry.counter(1);
            let sectors_compressed = entry.counter(2);

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
            out.units_u64(entry.counter(0) << 9);
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
                out.units_u64(entry.counter(0) << 9);
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
                out.units_u64(entry.counter(0) << 9);
                out.tab_rjust();
                out.units_u64(entry.counter(1) << 9);
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
                reserved += entry.counter(0);
            }
            DiskAccountingPos::Replicas { data_type, nr_devs, nr_required, devs: dev_list } => {
                if *data_type == BCH_DATA_cached as u8 {
                    cached += entry.counter(0);
                    continue;
                }

                let d = replicas_durability(*nr_devs, *nr_required, dev_list, devs);
                let degraded = d.durability - d.minus_degraded;

                while matrix.len() <= d.durability as usize {
                    matrix.push(Vec::new());
                }
                let row = &mut matrix[d.durability as usize];
                while row.len() <= degraded as usize {
                    row.push(0);
                }
                row[degraded as usize] += entry.counter(0);
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

/// Print a device list like [sda sdb sdc].
fn prt_dev_list(out: &mut Printbuf, dev_list: &[u8], devs: &[DevInfo]) {
    for (i, &dev_idx) in dev_list.iter().enumerate() {
        if i > 0 { write!(out, " ").unwrap(); }
        if dev_idx == c::BCH_SB_MEMBER_INVALID as u8 {
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
    fields: &[Field],
) -> Result<()> {
    let has = |f: Field| -> bool { fields.contains(&f) };

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

    if has(Field::Devices) {
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
                if i as u8 != BCH_DATA_unstriped as u8 {
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
        if i as u8 != BCH_DATA_unstriped as u8 {
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
                return entry.counter(0);
            }
        }
    }
    0
}
