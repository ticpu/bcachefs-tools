//! Dump range collection, write path, and orchestration.
//!
//! Walks the superblock/journal/btree/alloc btrees to collect the on-disk byte
//! ranges each device must dump (`DumpDev`), then writes them out -- either to a
//! per-device qcow2 image (`write_dev_image`, routing journal/btree through the
//! sanitize transforms when asked) or streamed to a single fd. `dump_fs` ties the
//! collection, data-bucket walk, and write/stream paths together.

use std::ops::ControlFlow;
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{anyhow, Result};

use bcachefs_kernel::btree;
use bcachefs_kernel::c;
use bcachefs_kernel::data::extents::bkey_ptrs_sc;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::POS_MIN;

use crate::qcow2::{self, Qcow2Image, Ranges, range_add, ranges_sort};
use crate::wrappers::accounting::{data_type_from_u8, data_type_is_empty};
use crate::wrappers::super_io::vstruct_bytes_sb;

use super::DumpCli;
use super::sanitize::{SanitizeOpts, sanitize_btree, sanitize_journal};

// ---- Dump implementation ----

struct DumpDev {
    sb:      Ranges,
    journal: Ranges,
    btree:   Ranges,
    data:    Ranges,
}

impl DumpDev {
    fn new() -> Self {
        DumpDev {
            sb:      Vec::new(),
            journal: Vec::new(),
            btree:   Vec::new(),
            data:    Vec::new(),
        }
    }

    fn take_all_ranges(&mut self) -> Ranges {
        let mut ranges = Vec::new();
        ranges.append(&mut self.sb);
        ranges.append(&mut self.journal);
        ranges.append(&mut self.btree);
        ranges.append(&mut self.data);
        ranges
    }
}

fn dump_node(fs: &Fs, devs: &mut [DumpDev], k: btree::BkeySC<'_>, btree_node_size: u64,
             single_replica: bool) {
    let val = k.v();

    // Capture the length by value so the filter closure doesn't borrow `devs`,
    // which we mutate via range_add below.
    let ndevs = devs.len();
    let exists = |p: &&c::bch_extent_ptr| {
        let dev = p.dev() as usize;
        dev < ndevs && fs.dev_exists(dev as u32)
    };

    if single_replica {
        // Dump only the replica on the lowest device index; the other ptrs are
        // rewritten to an invalid device in the btree write path.
        if let Some(ptr) = bkey_ptrs_sc(&val).filter(exists).min_by_key(|p| p.dev()) {
            range_add(&mut devs[ptr.dev() as usize].btree, ptr.offset() << 9, btree_node_size);
        }
    } else {
        for ptr in bkey_ptrs_sc(&val).filter(exists) {
            range_add(&mut devs[ptr.dev() as usize].btree, ptr.offset() << 9, btree_node_size);
        }
    }
}

fn get_sb_journal(fs: &Fs, ca: &c::bch_dev, entire_journal: bool, d: &mut DumpDev) {
    let sb = unsafe { &*ca.disk_sb.sb };
    let sb_bytes = vstruct_bytes_sb(sb) as u64;
    let bucket_bytes = (ca.mi.bucket_size as u64) << 9;

    // Superblock layout
    range_add(&mut d.sb,
              (c::BCH_SB_LAYOUT_SECTOR as u64) << 9,
              std::mem::size_of::<c::bch_sb_layout>() as u64);

    // All superblock copies
    for i in 0..sb.layout.nr_superblocks as usize {
        let offset = u64::from_le(sb.layout.sb_offset[i]);
        range_add(&mut d.sb, offset << 9, sb_bytes);
    }

    // Journal buckets
    let last_seq_ondisk = unsafe { (*fs.raw).journal.last_seq_ondisk };
    for i in 0..ca.journal.nr as usize {
        let seq = unsafe { *ca.journal.bucket_seq.add(i) };
        if entire_journal || seq >= last_seq_ondisk {
            let bucket = unsafe { *ca.journal.buckets.add(i) };
            range_add(&mut d.journal, bucket_bytes * bucket, bucket_bytes);
        }
    }
}

fn write_sanitized_ranges(
    img: &mut Qcow2Image<'_>,
    fs_raw: *mut c::bch_fs,
    ranges: &mut Ranges,
    bucket_bytes: u64,
    opts: SanitizeOpts,
    sanitize_fn: fn(*mut c::bch_fs, &mut [u8], SanitizeOpts),
) -> Result<()> {
    ranges_sort(ranges);
    let mut buf = vec![0u8; bucket_bytes as usize];

    for r in ranges.iter() {
        let len = (r.end - r.start) as usize;
        assert!(len <= bucket_bytes as usize);

        qcow2::pread_exact(img.infd(), &mut buf[..len], r.start)?;
        sanitize_fn(fs_raw, &mut buf[..len], opts);
        img.write_buf(&buf[..len], r.start)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_dev_image(
    fs: &Fs,
    ca: &c::bch_dev,
    path: &str,
    force: bool,
    sanitize: bool,
    sanitize_filenames: bool,
    single_replica: bool,
    block_size: u32,
    d: &mut DumpDev,
) -> Result<()> {
    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.write(true).create(true);
    if !force {
        open_opts.create_new(true);
    } else {
        open_opts.truncate(true);
    }

    let outfile = open_opts.open(path)
        .map_err(|e| anyhow!("{}: {}", path, e))?;

    let infd = unsafe { BorrowedFd::borrow_raw((*ca.disk_sb.bdev).bd_fd) };
    let mut img = Qcow2Image::new(infd, outfile.as_fd(), block_size)?;

    img.write_ranges(&mut d.sb)?;

    let opts = SanitizeOpts { sanitize, sanitize_filenames, single_replica };
    let bucket_bytes = (ca.mi.bucket_size as u64) << 9;

    // The journal carries the btree_root pointers, so it goes through the
    // modify path for --single-replica (to de-replicate them) as well as
    // --sanitize.
    if sanitize || single_replica {
        write_sanitized_ranges(
            &mut img, fs.raw, &mut d.journal, bucket_bytes, opts, sanitize_journal,
        )?;
    } else {
        img.write_ranges(&mut d.journal)?;
    }

    // Btree nodes go through the modify path for --sanitize or --single-replica.
    if sanitize || single_replica {
        write_sanitized_ranges(
            &mut img, fs.raw, &mut d.btree, bucket_bytes, opts, sanitize_btree,
        )?;
    } else {
        img.write_ranges(&mut d.btree)?;
    }

    // Data buckets are copied verbatim -- never sanitized or de-replicated.
    img.write_ranges(&mut d.data)?;

    img.finish()
}

fn collect_data_buckets(fs: &Fs, devs: &mut [DumpDev], bucket_bytes: &[u64],
                        first_bucket: &[u64], nbuckets: &[u64],
                        include: &[c::bch_data_type],
                        verbose: bool) -> Result<()> {
    let trans = btree::BtreeTrans::new(fs);
    let mut iter = btree::BtreeIter::new(
        &trans,
        c::btree_id::alloc,
        POS_MIN,
        btree::BtreeIterFlags::PREFETCH,
    );

    iter.for_each(&trans, |k: btree::BkeySC| {
        let dev = k.pos().inode as usize;
        if dev >= devs.len() || !fs.dev_exists(dev as u32) {
            return ControlFlow::Continue(());
        }

        // Offline members carry no bucket geometry (bucket_bytes == 0); their
        // data can't be dumped and dump_fs warns about them separately.
        if bucket_bytes[dev] == 0 {
            return ControlFlow::Continue(());
        }

        // A corrupt fs can hold alloc keys outside the device's bucket range;
        // emitting them yields ranges past device end that kill the dump.
        // Mirror bch2_alloc_read's [first_bucket, nbuckets) clamp.
        let offset = k.pos().offset;
        if offset < first_bucket[dev] || offset >= nbuckets[dev] {
            return ControlFlow::Continue(());
        }

        let mut v4: c::bch_alloc_v4 = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
        unsafe {
            c::__bch2_alloc_to_v4(c::bkey_s_c { k: k.k, v: k.v }, &mut v4);
        }

        let dt = data_type_from_u8(v4.data_type);
        if !data_type_is_empty(dt) || include.contains(&dt) {
            range_add(&mut devs[dev].data, offset * bucket_bytes[dev], bucket_bytes[dev]);
        }

        ControlFlow::Continue(())
    }).map_err(|e| anyhow!("error walking alloc btree: {}", e))?;

    if verbose {
        for (i, d) in devs.iter().enumerate() {
            let bytes: u64 = d.data.iter().map(|r| r.end - r.start).sum();
            eprintln!("dev {}: {} data buckets, {} MiB", i, d.data.len(), bytes >> 20);
        }
    }

    Ok(())
}

pub(super) fn dump_fs(fs: &Fs, cli: &DumpCli, sanitize: bool, sanitize_filenames: bool, stream_out: Option<BorrowedFd>) -> Result<()> {
    if sanitize_filenames {
        println!("Sanitizing filenames and inline data extents");
    } else if sanitize {
        println!("Sanitizing inline data extents");
    }

    let nr_devices = fs.nr_devices() as usize;
    let mut devs: Vec<DumpDev> = (0..nr_devices).map(|_| DumpDev::new()).collect();

    let entire_journal = !cli.nojournal;
    let btree_node_size = unsafe { (*fs.raw).opts.btree_node_size as u64 };
    let block_size = unsafe { (*fs.raw).opts.block_size as u32 };

    let mut nr_online = 0u32;
    let mut bucket_bytes: Vec<u64> = vec![0; nr_devices];
    let mut first_bucket: Vec<u64> = vec![0; nr_devices];
    let mut nbuckets: Vec<u64> = vec![0; nr_devices];
    let mut bucket_err: Option<String> = None;
    let _ = fs.for_each_online_member(|ca| {
        if (sanitize || cli.single_replica) && (ca.mi.bucket_size as u32) % (block_size >> 9) != 0 {
            // bch_dev.name is a [c_char; 32] array
            let ca_name_bytes = &ca.name;
            let len = ca_name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
            // c_char is i8, but from_utf8 wants u8 - use from_raw_parts to reinterpret
            let ca_name_bytes_u8 = unsafe {
                std::slice::from_raw_parts(ca_name_bytes[..len].as_ptr() as *const u8, len)
            };
            let name = std::str::from_utf8(ca_name_bytes_u8).unwrap_or("?");
            bucket_err = Some(format!("{} has unaligned buckets, cannot sanitize or de-replicate", name));
            return ControlFlow::Break(());
        }

        let dev_idx = ca.dev_idx as usize;
        bucket_bytes[dev_idx] = (ca.mi.bucket_size as u64) << 9;
        first_bucket[dev_idx] = ca.mi.first_bucket as u64;
        nbuckets[dev_idx] = ca.mi.nbuckets;
        get_sb_journal(fs, ca, entire_journal, &mut devs[dev_idx]);
        nr_online += 1;
        ControlFlow::Continue(())
    });
    if let Some(err) = bucket_err {
        return Err(anyhow!("{}", err));
    }

    // A present-but-offline member has no online geometry, so its data is
    // silently absent from the dump; warn loudly per member so it isn't missed.
    for (dev, &bytes) in bucket_bytes.iter().enumerate() {
        if fs.dev_exists(dev as u32) && bytes == 0 {
            eprintln!("WARNING: device {} is offline: its data is NOT included in this dump", dev);
        }
    }

    // Walk all btree types (including dynamic) to collect metadata locations.
    // The node iterator is per-level, so loop every level and dump each node's
    // own location (b.key) -- this reaches the root and every interior level,
    // no special-casing. Walking via the iterator (not a raw DFS) applies the
    // journal overlay, so nodes reachable only through not-yet-replayed journal
    // entries are captured too.
    for id in 0..fs.btree_id_nr_alive() {
        let trans = btree::BtreeTrans::new(fs);

        for level in 0..(c::BTREE_MAX_DEPTH as u32) {
            let mut node_iter = btree::BtreeNodeIter::new(
                &trans,
                id,
                POS_MIN,
                0, // locks_want
                level,
                btree::BtreeIterFlags::PREFETCH,
            );

            node_iter.for_each(&trans, |b| {
                dump_node(fs, &mut devs, btree::BkeySC::from(&b.key), btree_node_size,
                          cli.single_replica);
                ControlFlow::Continue(())
            }).map_err(|e| anyhow!("error walking btree {}: {}",
                btree::types::btree_id_str(id), e))?;
        }
    }

    if !cli.metadata_only {
        let include: Vec<c::bch_data_type> = cli.include.iter().map(|i| i.as_c()).collect();
        collect_data_buckets(fs, &mut devs, &bucket_bytes, &first_bucket, &nbuckets,
                             &include, cli.verbose)?;
    }

    // Special case: streaming to stdout
    if cli.output == "-" {
        if cli.device.is_none() && nr_online != 1 {
            return Err(anyhow!("streaming a multi-device filesystem to stdout requires --device <N>; or use -o <prefix> for file output"));
        }

        if let Some(n) = cli.device {
            if !fs.dev_exists(n) {
                return Err(anyhow!("--device {}: no such device in filesystem", n));
            }
        }

        let mut streaming_dev: Option<(i32, usize)> = None;
        let _ = fs.for_each_online_member(|ca| {
            if cli.device.is_none_or(|n| ca.dev_idx as u32 == n) {
                streaming_dev = Some((unsafe { (*ca.disk_sb.bdev).bd_fd }, ca.dev_idx as usize));
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });

        let (infd_raw, dev_idx) = streaming_dev.ok_or_else(|| {
            if let Some(n) = cli.device {
                anyhow!("--device {}: device is not online", n)
            } else {
                anyhow!("no online device to stream")
            }
        })?;

        let infd = unsafe { BorrowedFd::borrow_raw(infd_raw) };
        let d = &mut devs[dev_idx];
        let mut ranges = d.take_all_ranges();

        qcow2::write_qcow2_stream(infd, stream_out.expect("stream fd"), block_size, &mut ranges)?;
        return Ok(());
    }

    // Write qcow2 image(s)
    let mut write_err: Option<anyhow::Error> = None;
    let _ = fs.for_each_online_member(|ca| {
        let dev_idx = ca.dev_idx;
        let path = if nr_online > 1 {
            format!("{}.{}.qcow2", cli.output, dev_idx)
        } else {
            format!("{}.qcow2", cli.output)
        };

        match write_dev_image(fs, ca, &path, cli.force, sanitize, sanitize_filenames,
                              cli.single_replica, block_size, &mut devs[dev_idx as usize]) {
            Ok(()) => ControlFlow::Continue(()),
            Err(e) => {
                write_err = Some(e);
                ControlFlow::Break(())
            }
        }
    });
    if let Some(e) = write_err {
        return Err(e);
    }

    Ok(())
}
