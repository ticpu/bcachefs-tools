// bcachefs fs du: disk usage statistics via reverse-mapping
//
// Sampler: uniform-physical-offset, area-weighted estimation (the btdu
// estimator). Each sample picks a uniformly random physical sector across
// all online devices (device chosen with probability proportional to its
// capacity, then a uniform sector within it) and resolves it via
// BCH_IOCTL_REV_MAP. Every sample carries the same fixed weight
// (total_sectors / n_samples): probability of landing on a given file is
// already proportional to that file's physical size, so no additional
// weighting by the ioctl's returned `sectors` field is needed. This lets
// the sample count trade accuracy for ioctl calls independent of
// filesystem size.
//
// Per-device/data_type accounting is always keyed on the ioctl's
// `data_type` output (free/sb/journal/btree/user/cached/parity/stripe/...)
// so it's directly comparable to `bcachefs fs usage -f devices`. Path
// attribution is separate and keyed on `btree_id`: BTREE_ID_extents carries
// a resolvable inode/path (this includes cached-replica pointers, which
// physically belong to the same file), BTREE_ID_reflink is indirect
// (fanned out, can't be attributed to one file), and any other btree_id is
// metadata addressed by its data_type name.
//
// Path resolution is deferred: sampling only records (inode, snapshot) for
// extents samples and (key_offset, key_size) for reflink samples, with no
// BCH_REV_MAP_RESOLVE_PATH path buffer round-trip per sample. After
// sampling, extents samples are aggregated by distinct (inode, snapshot)
// and reflink samples are resolved to their referencing (inode, snapshot)
// set via the reflink map; both then share one BCH_IOCTL_INUM_TO_PATH
// resolution pass and cache, since a filesystem tends to reuse the same
// small set of inodes across many samples. This turns an O(samples) path
// walk into O(distinct inodes), which dominates at scale (millions of
// samples touching thousands of files).
//
// Sampling and resolution are each random-IO-bound (backpointer/reflink
// btree reads, dirent/inode chain walks) and embarrassingly parallel across
// independent ioctls, so both phases fan out across `--threads` handles,
// each opened separately per thread rather than shared behind a lock —
// serializing IO on one fd would defeat the purpose.
//
// The interactive TUI (fs_du_tui.rs) is the default when stdout is a
// terminal; `-b`/`--batch` forces this file's one-shot text report, which
// is also the default when stdout isn't a terminal (pipes/scripts). The
// TUI reuses this file's sampler, classifier, reflink map and resolver
// building blocks (all marked `pub(super)` for that purpose) but can't
// reuse the fixed-N weighting scheme above: it samples continuously with
// no final sample count to pre-scale by. It instead accumulates raw visit
// counts (weight 1.0 per sample, same DirNode/PerDeviceCounters types) and
// converts to sectors at render time via total_sectors/samples_so_far,
// which is mathematically the same estimator — just evaluated with a
// growing N instead of a fixed one — and refines live as more samples
// arrive. `try_attribute()` is factored out of `attribute_samples()` for
// the same reason: the TUI resolves keys incrementally and needs the same
// path/ENOENT decision but with a "not yet resolved" outcome batch mode
// never sees.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::time::Instant;

use anyhow::{Context, Result};
use bch_bindgen::accounting::data_type_from_u8;
use bch_bindgen::c::{
    bch_ioctl_inum_to_path, bch_ioctl_reflink_map, bch_ioctl_reflink_map_entry, bch_ioctl_rev_map,
    btree_id, BCH_REV_MAP_RESOLVE_PATH,
};
use bcachefs_kernel::btree;
use bcachefs_kernel::opts::prt_data_type;
use bcachefs_kernel::util::printbuf::Printbuf;
use clap::Parser;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::commands::device;
use crate::commands::fs_du_tui;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_wr;
use crate::wrappers::sysfs::{self, DevInfo};

const BCH_IOCTL_INUM_TO_PATH_NR: u32 = 36;
const DEFAULT_SAMPLES: u64 = 100_000;
const PATH_BUF_SIZE: usize = 4096;
const TREE_DEPTH: usize = 4;
const TREE_TOP_K: usize = 15;
const REFLINK_MAP_BATCH: usize = 4096;

#[derive(Parser, Debug)]
#[command(name = "du", about = "Disk usage statistics")]
pub struct Cli {
    /// Filesystem path or device
    filesystem: String,

    /// Probe a specific device offset: --probe <DEV> <OFFSET>.
    /// DEV may be a numeric device index or a device path (e.g. /dev/vdb1).
    /// Hidden: single-offset BCH_IOCTL_REV_MAP diagnostic, not part of the
    /// user-facing du surface.
    #[arg(long, value_names = ["DEV", "OFFSET"], num_args = 2, hide = true)]
    probe: Vec<String>,

    /// Number of samples to take before reporting
    #[arg(long, default_value_t = DEFAULT_SAMPLES)]
    samples: u64,

    /// RNG seed, for reproducible sampling runs
    #[arg(long)]
    seed: Option<u64>,

    /// Restrict/aggregate the report to usage under this path prefix
    #[arg(long)]
    path: Option<String>,

    /// Sampling/resolution threads (default: min(8, available parallelism)).
    /// Each thread opens its own filesystem handle and seeds its RNG from
    /// seed+thread_index, so a run is reproducible only for a fixed
    /// --threads value.
    #[arg(long)]
    threads: Option<usize>,

    /// Plain one-shot text report (--samples samples, then exit) instead
    /// of the interactive TUI. This is the default when stdout is not a
    /// terminal (pipes/scripts); pass explicitly to force it on a terminal.
    #[arg(short = 'b', long, visible_alias = "oneshot")]
    batch: bool,

    /// Hidden compatibility alias: the interactive TUI is now the default
    /// on a terminal. Kept as a forcing flag (TUI even when stdout isn't a
    /// terminal) for scripts still passing --tui.
    #[arg(long, hide = true)]
    tui: bool,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

fn fs_du(cli: Cli) -> Result<()> {
    let handle = BcachefsHandle::open(&cli.filesystem)
        .with_context(|| format!("opening filesystem '{}'", cli.filesystem))?;

    if !cli.probe.is_empty() {
        let dev = device::resolve_dev(&handle, &cli.probe[0])?;
        let offset: u64 = cli.probe[1].parse()
            .context("offset must be a number")?;

        probe_offset(&handle, dev, offset)?;
    } else {
        let n_threads = cli.threads.unwrap_or_else(default_threads).max(1);
        let use_tui = !cli.batch && (cli.tui || std::io::stdout().is_terminal());
        if use_tui {
            fs_du_tui::run(&handle, &cli.filesystem, cli.seed, cli.path.as_deref(), n_threads)?;
        } else {
            run_sampler(&handle, &cli.filesystem, cli.samples, cli.seed, cli.path.as_deref(), n_threads)?;
        }
    }

    Ok(())
}

fn probe_offset(handle: &BcachefsHandle, dev: u32, offset: u64) -> Result<()> {
    const PATH_BUF_SIZE: usize = 4096;
    let mut path_buf = vec![0u8; PATH_BUF_SIZE];

    let mut arg = bch_ioctl_rev_map {
        dev,
        flags: BCH_REV_MAP_RESOLVE_PATH,
        offset,
        buf: path_buf.as_mut_ptr() as u64,
        buf_size: path_buf.len() as u32,
        ..Default::default()
    };

    handle.rev_map(&mut arg)
        .map_err(|e| anyhow::anyhow!("rev_map ioctl failed: {}", e))?;

    if arg.found == 0 {
        println!("dev {} offset {}: free space", dev, offset);
        return Ok(());
    }

    // Found
    println!("dev {} offset {}: found", dev, offset);
    println!("  data_type: {} ({})",
        arg.data_type,
        data_type_name(arg.data_type));
    println!("  btree_id:  {} ({})",
        arg.btree_id,
        btree::types::btree_id_str(arg.btree_id.into()));
    println!("  ec:        {}", arg.ec);
    println!("  snapshot:  {}", arg.snapshot);
    println!("  inode:     {}", arg.inode);
    println!("  sectors:   {}", arg.sectors);
    println!("  background_target: {}", arg.background_target);
    println!("  key_offset: {}", arg.key_offset);
    println!("  key_size:   {}", arg.key_size);

    // Extract path (if resolved)
    if let Some(nul_pos) = path_buf.iter().position(|&b| b == 0) {
        if nul_pos > 0 {
            let path = String::from_utf8_lossy(&path_buf[..nul_pos]);
            println!("  path:      {}", path);
        }
    }

    Ok(())
}

/// Format a raw `data_type` byte via the C `__bch2_data_types[]` table
/// (same formatting `fs usage` uses), so names can't drift from the kernel.
pub(super) fn data_type_name(dt: u8) -> String {
    let mut pb = Printbuf::new();
    prt_data_type(&mut pb, data_type_from_u8(dt));
    pb.as_str().to_string()
}

// ──────────────────────────── Sampler ────────────────────────────

pub(super) struct DeviceGeom {
    pub(super) idx: u32,
    pub(super) total_sectors: u64,
}

/// Everything sampling needs about the filesystem's shape, shared by the
/// batch sampler (`run_sampler`) and the TUI (`fs_du_tui::run`): online
/// devices, per-device geometry, the capacity-weighted device selection
/// distribution, and the resolved RNG seed.
pub(super) struct SamplingSetup {
    pub(super) devs: Vec<DevInfo>,
    pub(super) geoms: Vec<DeviceGeom>,
    pub(super) total_sectors: u64,
    pub(super) dev_dist: WeightedIndex<u64>,
    pub(super) seed: u64,
}

pub(super) fn sampling_setup(handle: &BcachefsHandle, seed: Option<u64>) -> Result<SamplingSetup> {
    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path, sysfs::DeviceNameMode::Raw)
        .context("enumerating filesystem devices")?;
    if devs.is_empty() {
        anyhow::bail!("filesystem has no online devices");
    }

    let mut geoms = Vec::with_capacity(devs.len());
    for dev in &devs {
        let usage = handle.dev_usage(dev.idx)
            .map_err(|e| anyhow::anyhow!("getting usage for device {}: {}", dev.idx, e))?;
        geoms.push(DeviceGeom { idx: dev.idx, total_sectors: usage.capacity_sectors() });
    }

    let total_sectors: u64 = geoms.iter().map(|g| g.total_sectors).sum();
    if total_sectors == 0 {
        anyhow::bail!("filesystem devices report zero capacity");
    }

    let weights: Vec<u64> = geoms.iter().map(|g| g.total_sectors).collect();
    let dev_dist = WeightedIndex::new(&weights).context("building device selection weights")?;
    let seed = seed.unwrap_or_else(|| rand::thread_rng().gen());

    Ok(SamplingSetup { devs, geoms, total_sectors, dev_dist, seed })
}

/// Open `n` independent filesystem handles, one per worker thread (see
/// module notes on why handles aren't shared behind a lock).
pub(super) fn open_handles(fs_path: &str, n: usize, role: &str) -> Result<Vec<BcachefsHandle>> {
    (0..n)
        .map(|_| {
            BcachefsHandle::open(fs_path)
                .with_context(|| format!("opening filesystem '{}' for {} thread", fs_path, role))
        })
        .collect()
}

/// Per-device sample accounting, keyed on the ioctl's `data_type` output
/// so it lines up 1:1 with `bcachefs fs usage -f devices`.
///
/// Batch mode accumulates pre-scaled sector weights here (fixed
/// `total_sectors/n_samples` per sample); the TUI's continuous sampler
/// (`fs_du_tui`) instead accumulates raw visit counts (weight 1.0 per
/// sample) and converts to sectors at render time, since it has no fixed
/// sample count to pre-scale by. `PerDeviceCounters` itself is agnostic to
/// which convention a caller uses.
#[derive(Default)]
pub(super) struct PerDeviceCounters {
    pub(super) total: f64,
    pub(super) free: f64,
    pub(super) by_data_type: HashMap<u8, f64>,
}

impl PerDeviceCounters {
    /// Merge another (per-thread) counter set into this one, summing
    /// weights — sibling of `DirNode::merge`.
    pub(super) fn merge(&mut self, other: PerDeviceCounters) {
        self.total += other.total;
        self.free += other.free;
        for (dt, weight) in other.by_data_type {
            *self.by_data_type.entry(dt).or_insert(0.0) += weight;
        }
    }
}

/// A node in the du-style path tree. Real files/directories are nested by
/// path component; pseudo-categories (`<free>`, `<unresolved>`, `<reflink>`,
/// `<btree>`, ...) are single-level children of the root.
///
/// Weight units follow the same batch-vs-TUI convention as
/// `PerDeviceCounters` above.
#[derive(Default)]
pub(super) struct DirNode {
    pub(super) total: f64,
    pub(super) children: HashMap<String, DirNode>,
}

impl DirNode {
    pub(super) fn insert_path(&mut self, path: &str, weight: f64) {
        self.total += weight;
        let mut node = self;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            node = node.children.entry(comp.to_string()).or_default();
            node.total += weight;
        }
    }

    pub(super) fn insert_pseudo(&mut self, name: &str, weight: f64) {
        self.total += weight;
        let child = self.children.entry(name.to_string()).or_default();
        child.total += weight;
    }

    /// Merge another (per-thread) tree into this one, summing weights.
    pub(super) fn merge(&mut self, other: DirNode) {
        self.total += other.total;
        for (name, child) in other.children {
            self.children.entry(name).or_default().merge(child);
        }
    }

    pub(super) fn find(&self, path: &str) -> Option<&DirNode> {
        let mut node = self;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            node = node.children.get(comp)?;
        }
        Some(node)
    }

    pub(super) fn sorted_children(&self) -> Vec<(&String, &DirNode)> {
        let mut v: Vec<_> = self.children.iter().collect();
        v.sort_by(|a, b| b.1.total.partial_cmp(&a.1.total).unwrap());
        v
    }
}

fn print_tree(node: &DirNode, indent: usize, depth_remaining: usize, top_k: usize) {
    for (name, child) in node.sorted_children().into_iter().take(top_k) {
        println!("{}{}\t{}", "  ".repeat(indent), name, fmt_sectors(child.total));
        if depth_remaining > 0 {
            print_tree(child, indent + 1, depth_remaining - 1, top_k);
        }
    }
}

pub(super) fn fmt_sectors(sectors: f64) -> String {
    let mut pb = Printbuf::new();
    pb.set_human_readable(true);
    pb.units_sectors(sectors.round() as u64);
    pb.as_str().to_string()
}

fn extract_path(buf: &[u8]) -> Option<String> {
    let nul = buf.iter().position(|&b| b == 0)?;
    if nul == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
}

/// Decode a `background_target` value into a device or disk-group label.
/// Mirrors `target_decode()` in fs/alloc/disk_groups.h (TARGET_NULL=0,
/// TARGET_DEV_START=1, TARGET_GROUP_START=257). Group *names* require
/// reading the disk_groups superblock section, which isn't wired up here
/// (`fs usage` reads it via the mounted handle's accounting path) — we
/// print the numeric group index instead.
fn describe_target(target: u16, devs: &[DevInfo]) -> String {
    const TARGET_DEV_START: u16 = 1;
    const TARGET_GROUP_START: u16 = 257;

    if target == 0 {
        "none".to_string()
    } else if target < TARGET_GROUP_START {
        let dev_idx = (target - TARGET_DEV_START) as u32;
        match devs.iter().find(|d| d.idx == dev_idx) {
            Some(d) => format!("dev:{}", d.dev),
            None => format!("dev:{}", dev_idx),
        }
    } else {
        format!("group:{}", target - TARGET_GROUP_START)
    }
}

/// A sample that landed on the reflink btree, deferred for attribution to
/// referencing files once the full sampling pass is done. `key_offset` is
/// the owning indirect extent's bpos.offset (its end); `key_size` is its
/// logical length in sectors (0 if the key could not be fetched).
pub(super) struct ReflinkSample {
    pub(super) key_offset: u64,
    pub(super) key_size: u32,
    pub(super) weight: f64,
}

/// One sampling thread's accumulated results, merged into the run-wide
/// totals once all threads join (batch mode) or once per flushed batch
/// (TUI mode, `fs_du_tui`). Extents samples are pre-aggregated by distinct
/// (inode, snapshot) here rather than kept one-per-sample, since path
/// resolution only needs the distinct set.
#[derive(Default)]
pub(super) struct ThreadAccum {
    pub(super) per_dev: Vec<PerDeviceCounters>,
    pub(super) tree: DirNode,
    pub(super) extent_totals: HashMap<(u64, u32), f64>,
    pub(super) reflink_samples: Vec<ReflinkSample>,
}

impl ThreadAccum {
    pub(super) fn new(n_devs: usize) -> Self {
        ThreadAccum {
            per_dev: (0..n_devs).map(|_| PerDeviceCounters::default()).collect(),
            ..Default::default()
        }
    }
}

/// Classify one raw rev_map result into a thread's local accumulators.
/// No path resolution happens here — extents samples are aggregated by
/// (inode, snapshot) and resolved once, later, by the caller.
pub(super) fn classify_sample(arg: &bch_ioctl_rev_map, weight: f64, dev_pos: usize, accum: &mut ThreadAccum) {
    let dev = &mut accum.per_dev[dev_pos];
    dev.total += weight;

    if arg.found == 0 {
        dev.free += weight;
        accum.tree.insert_pseudo("<free>", weight);
        return;
    }

    *dev.by_data_type.entry(arg.data_type).or_insert(0.0) += weight;

    let bid = arg.btree_id as u32;
    if bid == u32::from(btree_id::extents) {
        *accum.extent_totals.entry((arg.inode, arg.snapshot)).or_insert(0.0) += weight;
    } else if bid == u32::from(btree_id::reflink) {
        accum.reflink_samples.push(ReflinkSample {
            key_offset: arg.key_offset,
            key_size: arg.key_size,
            weight,
        });
    } else {
        accum.tree.insert_pseudo(&format!("<{}>", data_type_name(arg.data_type)), weight);
    }
}

// ──────────────────────────── Reflink attribution ────────────────────────────
//
// The reflink btree holds indirect extents with no reverse index to the
// reflink_p keys that reference them (only a refcount). BCH_IOCTL_REFLINK_MAP
// streams every reflink_p in the filesystem as (inum, snapshot, idx, len);
// [idx, idx+len) is the referenced range in the reflink btree. We accumulate
// these into an interval map and, for each sample that landed on the reflink
// btree, query which files' reflink_p keys overlap that sample's owning
// indirect extent. A sample with N distinct referencers is charged weight/N
// to each, so the fractional attribution sums exactly back to the sampled
// physical usage.

/// One reflink_p pointer: file (inum, snapshot) references [start, end) in
/// the reflink btree.
struct ReflinkRef {
    start: u64,
    end: u64,
    inum: u64,
    snapshot: u32,
}

/// Interval map over reflink_p ranges, queried by physical overlap.
///
/// Sorted by `start`, with a parallel prefix-max of `end`: to find every
/// interval overlapping [q_start, q_end), binary-search for the last entry
/// with `start < q_end`, then scan backward while the running max `end`
/// still exceeds `q_start` (a monotonic bound, since it's a prefix max) —
/// once it drops below, no earlier entry can overlap either.
pub(super) struct ReflinkMap {
    refs: Vec<ReflinkRef>,
    max_end: Vec<u64>,
}

impl ReflinkMap {
    fn build(mut refs: Vec<ReflinkRef>) -> Self {
        refs.sort_by_key(|r| r.start);
        let mut running = 0u64;
        let max_end = refs
            .iter()
            .map(|r| {
                running = running.max(r.end);
                running
            })
            .collect();
        Self { refs, max_end }
    }

    /// Distinct (inum, snapshot) referencers overlapping [q_start, q_end).
    pub(super) fn overlaps(&self, q_start: u64, q_end: u64) -> Vec<(u64, u32)> {
        let mut out: Vec<(u64, u32)> = Vec::new();
        let mut i = self.refs.partition_point(|r| r.start < q_end);
        while i > 0 {
            i -= 1;
            if self.max_end[i] <= q_start {
                break;
            }
            let r = &self.refs[i];
            if r.end > q_start {
                let key = (r.inum, r.snapshot);
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
        out
    }

    /// Distinct referencers of one reflink sample's owning indirect extent:
    /// `key_offset` is the extent's end position, `key_size` its logical
    /// length (0 when the key couldn't be fetched — probe a single sector).
    /// An empty result means the sample is `<reflink unreferenced>`; a
    /// non-empty one is charged `weight / len` per referencer by the caller.
    pub(super) fn sample_referencers(&self, sample: &ReflinkSample) -> Vec<(u64, u32)> {
        let q_end = sample.key_offset;
        let q_start = q_end.saturating_sub(sample.key_size.max(1) as u64);
        self.overlaps(q_start, q_end)
    }
}

/// Enumerate every reflink_p key via BCH_IOCTL_REFLINK_MAP, cursor-batched.
pub(super) fn build_reflink_map(handle: &BcachefsHandle) -> Result<ReflinkMap> {
    let mut refs = Vec::new();
    let mut buf: Vec<bch_ioctl_reflink_map_entry> =
        std::iter::repeat_with(Default::default).take(REFLINK_MAP_BATCH).collect();
    let mut arg = bch_ioctl_reflink_map {
        entries: buf.as_mut_ptr() as u64,
        nr_entries: buf.len() as u32,
        ..Default::default()
    };

    loop {
        arg.nr_entries = buf.len() as u32;

        handle.reflink_map(&mut arg)
            .map_err(|e| anyhow::anyhow!("reflink_map ioctl failed: {}", e))?;

        for entry in &buf[..arg.nr_entries as usize] {
            refs.push(ReflinkRef {
                start: entry.idx,
                end: entry.idx + entry.len as u64,
                inum: entry.inum,
                snapshot: entry.snapshot,
            });
        }

        if arg.done != 0 {
            break;
        }
    }

    Ok(ReflinkMap::build(refs))
}

/// Resolve (inum, snapshot) to a path and background_target via
/// BCH_IOCTL_INUM_TO_PATH. `Ok(None)` means the inode is gone (ENOENT):
/// the caller folds that referencer into `<reflink unreferenced>`.
pub(super) fn resolve_inum_to_path(
    handle: &BcachefsHandle,
    inum: u64,
    snapshot: u32,
) -> Result<Option<(String, u16)>> {
    let mut path_buf = vec![0u8; PATH_BUF_SIZE];
    let mut arg = bch_ioctl_inum_to_path {
        inum,
        snapshot,
        buf: path_buf.as_mut_ptr() as u64,
        buf_size: path_buf.len() as u32,
        ..Default::default()
    };

    let request = bch_ioc_wr::<bch_ioctl_inum_to_path>(BCH_IOCTL_INUM_TO_PATH_NR);
    let ret = unsafe { libc::ioctl(handle.ioctl_fd_raw(), request, &mut arg as *mut _) };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        if raw == libc::ENOENT {
            return Ok(None);
        }
        if raw == libc::ERANGE {
            // Path didn't fit `buf`; background_target is still valid, we
            // just can't attribute a path (same as rev_map's handling).
            return Ok(Some((String::new(), arg.background_target)));
        }
        return Err(err)
            .with_context(|| format!("inum_to_path ioctl failed for inum {} snapshot {}", inum, snapshot));
    }

    Ok(Some((extract_path(&path_buf).unwrap_or_default(), arg.background_target)))
}

/// (inode, snapshot) -> resolved (path, background_target), or `None` for
/// an inode that no longer exists (ENOENT).
pub(super) type PathCache = HashMap<(u64, u32), Option<(String, u16)>>;
type PathCacheEntry = ((u64, u32), Option<(String, u16)>);

/// Resolve a batch of distinct (inum, snapshot) keys via BCH_IOCTL_INUM_TO_PATH,
/// fanned out across `n_threads` handles. Same random-IO-bound shape as
/// sampling, so the same per-thread-handle pattern applies.
fn resolve_inums_parallel(fs_path: &str, keys: &[(u64, u32)], n_threads: usize) -> Result<PathCache> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }

    let n_threads = n_threads.max(1).min(keys.len());
    let chunk_size = keys.len().div_ceil(n_threads).max(1);
    let chunks: Vec<&[(u64, u32)]> = keys.chunks(chunk_size).collect();

    let handles = open_handles(fs_path, chunks.len(), "resolve")?;

    std::thread::scope(|s| -> Result<PathCache> {
        let mut joins = Vec::with_capacity(chunks.len());
        for (handle, chunk) in handles.into_iter().zip(chunks) {
            joins.push(s.spawn(move || -> Result<Vec<PathCacheEntry>> {
                let mut out = Vec::with_capacity(chunk.len());
                for &(inum, snapshot) in chunk {
                    out.push(((inum, snapshot), resolve_inum_to_path(&handle, inum, snapshot)?));
                }
                Ok(out)
            }));
        }

        let mut merged = HashMap::with_capacity(keys.len());
        for join in joins {
            let part = join.join().map_err(|_| anyhow::anyhow!("resolve thread panicked"))??;
            merged.extend(part);
        }
        Ok(merged)
    })
}

/// Attribute `weight` for `key` using `path_cache`, if it's resolved
/// there. Returns `true` once handled — a real path, or a confirmed-gone
/// (ENOENT) key folded into `not_found_label` — and `false` if `key` isn't
/// in the cache yet, leaving attribution up to the caller (batch mode
/// treats this as equivalent to ENOENT, since every key it queries is
/// resolved before this runs; the TUI, which resolves keys as it goes,
/// defers unresolved weight into a `<resolving>` pseudo-category instead).
pub(super) fn try_attribute(
    key: (u64, u32),
    weight: f64,
    path_cache: &PathCache,
    not_found_label: &str,
    tree: &mut DirNode,
    target_totals: &mut HashMap<String, f64>,
    devs: &[DevInfo],
) -> bool {
    match path_cache.get(&key) {
        Some(Some((path, background_target))) => {
            *target_totals.entry(describe_target(*background_target, devs)).or_insert(0.0) += weight;
            if path.starts_with('/') {
                tree.insert_path(path, weight);
            } else {
                tree.insert_pseudo("<unresolved>", weight);
            }
            true
        }
        Some(None) => {
            tree.insert_pseudo(not_found_label, weight);
            true
        }
        None => false,
    }
}

/// Attribute deferred extents and reflink samples using a shared inode ->
/// (path, background_target) cache, merging fractional weights into `tree`
/// and `target_totals` in place. `reflink_referencers` pairs each reflink
/// sample's resolved referencer set with its weight. Every key here was
/// included in the caller's resolution pass, so `try_attribute` always
/// finds it in `path_cache`; the `!try_attribute(..)` fallback exists only
/// for defensive symmetry with `fs_du_tui`, which shares this helper.
fn attribute_samples(
    path_cache: &PathCache,
    extent_totals: &HashMap<(u64, u32), f64>,
    reflink_referencers: &[(Vec<(u64, u32)>, f64)],
    tree: &mut DirNode,
    target_totals: &mut HashMap<String, f64>,
    devs: &[DevInfo],
) {
    for (&key, &weight) in extent_totals {
        if !try_attribute(key, weight, path_cache, "<unresolved>", tree, target_totals, devs) {
            tree.insert_pseudo("<unresolved>", weight);
        }
    }

    for (referencers, weight) in reflink_referencers {
        if referencers.is_empty() {
            tree.insert_pseudo("<reflink unreferenced>", *weight);
            continue;
        }

        let share = weight / referencers.len() as f64;
        for &key in referencers {
            if !try_attribute(key, share, path_cache, "<reflink unreferenced>", tree, target_totals, devs) {
                tree.insert_pseudo("<reflink unreferenced>", share);
            }
        }
    }
}

/// Sample one uniformly random physical offset via BCH_IOCTL_REV_MAP,
/// returning the raw ioctl result and the device it landed on (an index
/// into `geoms`). Shared between the batch sampler's fixed-count loop
/// below and the TUI's continuous sampler (`fs_du_tui::sampler_loop`).
pub(super) fn sample_one(
    handle: &BcachefsHandle,
    rng: &mut StdRng,
    dev_dist: &WeightedIndex<u64>,
    geoms: &[DeviceGeom],
) -> Result<(bch_ioctl_rev_map, usize)> {
    let dev_pos = dev_dist.sample(rng);
    let dev_idx = geoms[dev_pos].idx;
    let dev_total = geoms[dev_pos].total_sectors;
    let offset = rng.gen_range(0..dev_total);

    // No BCH_REV_MAP_RESOLVE_PATH: path resolution is deferred to a
    // separate pass over distinct (inode, snapshot) pairs, so no path
    // buffer round-trip (and no possible -ERANGE) here.
    let mut arg = bch_ioctl_rev_map {
        dev: dev_idx,
        flags: 0,
        offset,
        buf: 0,
        buf_size: 0,
        ..Default::default()
    };

    handle.rev_map(&mut arg).map_err(|e| {
        anyhow::anyhow!("rev_map ioctl failed for dev {} offset {}: {}", dev_idx, offset, e)
    })?;

    Ok((arg, dev_pos))
}

/// Sample `n_samples` uniformly random physical offsets on its own handle,
/// returning locally-accumulated results for the caller to merge.
fn sample_thread(
    handle: &BcachefsHandle,
    n_samples: u64,
    seed: u64,
    weight_per_sample: f64,
    devs: &[DevInfo],
    geoms: &[DeviceGeom],
    dev_dist: &WeightedIndex<u64>,
) -> Result<ThreadAccum> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut accum = ThreadAccum::new(devs.len());

    for _ in 0..n_samples {
        let (arg, dev_pos) = sample_one(handle, &mut rng, dev_dist, geoms)?;
        classify_sample(&arg, weight_per_sample, dev_pos, &mut accum);
    }

    Ok(accum)
}

fn run_sampler(
    handle: &BcachefsHandle,
    fs_path: &str,
    n_samples: u64,
    seed: Option<u64>,
    path_prefix: Option<&str>,
    n_threads: usize,
) -> Result<()> {
    if n_samples == 0 {
        anyhow::bail!("--samples must be greater than zero");
    }

    let SamplingSetup { devs, geoms, total_sectors, dev_dist, seed } =
        sampling_setup(handle, seed)?;
    let weight_per_sample = total_sectors as f64 / n_samples as f64;

    // ──────────── Phase 1: parallel sampling ────────────

    let sampling_start = Instant::now();
    let n_sample_threads = n_threads.min(n_samples as usize).max(1);
    let base = n_samples / n_sample_threads as u64;
    let rem = n_samples % n_sample_threads as u64;

    let sample_handles = open_handles(fs_path, n_sample_threads, "sampling")?;

    let thread_results = std::thread::scope(|s| -> Result<Vec<ThreadAccum>> {
        let mut joins = Vec::with_capacity(n_sample_threads);
        for (i, sample_handle) in sample_handles.into_iter().enumerate() {
            let count = base + if (i as u64) < rem { 1 } else { 0 };
            let thread_seed = seed.wrapping_add(i as u64);
            let devs = &devs;
            let geoms = &geoms;
            let dev_dist = &dev_dist;
            joins.push(s.spawn(move || {
                sample_thread(&sample_handle, count, thread_seed, weight_per_sample, devs, geoms, dev_dist)
            }));
        }

        let mut out = Vec::with_capacity(joins.len());
        for join in joins {
            out.push(join.join().map_err(|_| anyhow::anyhow!("sampling thread panicked"))??);
        }
        Ok(out)
    })?;

    let sampling_secs = sampling_start.elapsed().as_secs_f64();

    let mut tree = DirNode::default();
    let mut per_dev: Vec<PerDeviceCounters> = devs.iter().map(|_| PerDeviceCounters::default()).collect();
    let mut extent_totals: HashMap<(u64, u32), f64> = HashMap::new();
    let mut reflink_samples: Vec<ReflinkSample> = Vec::new();

    for accum in thread_results {
        for (pd, thread_pd) in per_dev.iter_mut().zip(accum.per_dev) {
            pd.merge(thread_pd);
        }
        tree.merge(accum.tree);
        for (key, weight) in accum.extent_totals {
            *extent_totals.entry(key).or_insert(0.0) += weight;
        }
        reflink_samples.extend(accum.reflink_samples);
    }

    // ──────────── Phase 2: reflink map ────────────

    let reflink_start = Instant::now();
    let reflink_attributed = !reflink_samples.is_empty();
    let map = if reflink_attributed {
        Some(build_reflink_map(handle).context("building reflink attribution map (BCH_IOCTL_REFLINK_MAP)")?)
    } else {
        None
    };
    let reflink_secs = reflink_start.elapsed().as_secs_f64();

    // Resolve each reflink sample's referencing (inode, snapshot) set now
    // (in-memory interval query, no ioctl) so the distinct key set below
    // includes them alongside the extents samples.
    let mut resolve_keys: HashSet<(u64, u32)> = extent_totals.keys().copied().collect();
    let reflink_referencers: Vec<(Vec<(u64, u32)>, f64)> = reflink_samples
        .iter()
        .map(|s| {
            let referencers =
                map.as_ref().expect("reflink map built when samples present").sample_referencers(s);
            resolve_keys.extend(referencers.iter().copied());
            (referencers, s.weight)
        })
        .collect();

    // ──────────── Phase 3: parallel resolution ────────────

    let resolve_start = Instant::now();
    let resolve_keys: Vec<(u64, u32)> = resolve_keys.into_iter().collect();
    let n_resolved = resolve_keys.len();
    let path_cache = resolve_inums_parallel(fs_path, &resolve_keys, n_threads)?;
    let resolve_secs = resolve_start.elapsed().as_secs_f64();

    let mut target_totals: HashMap<String, f64> = HashMap::new();
    attribute_samples(
        &path_cache,
        &extent_totals,
        &reflink_referencers,
        &mut tree,
        &mut target_totals,
        &devs,
    );

    print_report(
        total_sectors,
        n_samples,
        seed,
        &devs,
        &per_dev,
        &tree,
        &target_totals,
        path_prefix,
        reflink_attributed,
    );

    println!(
        "phases: sampling {:.1}s, reflink map {:.1}s, resolve {:.1}s ({} inodes)",
        sampling_secs, reflink_secs, resolve_secs, n_resolved
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_report(
    total_sectors: u64,
    n_samples: u64,
    seed: u64,
    devs: &[DevInfo],
    per_dev: &[PerDeviceCounters],
    tree: &DirNode,
    target_totals: &HashMap<String, f64>,
    path_prefix: Option<&str>,
    reflink_attributed: bool,
) {
    let free_est: f64 = per_dev.iter().map(|d| d.free).sum();
    let used_est = total_sectors as f64 - free_est;

    println!("samples: {}  seed: {}", n_samples, seed);
    println!("total: {}  used (est): {}  free (est): {}",
        fmt_sectors(total_sectors as f64), fmt_sectors(used_est), fmt_sectors(free_est));
    println!();

    for (dev, d) in devs.iter().zip(per_dev.iter()) {
        println!("device {} ({}):  used (est): {}  free (est): {}",
            dev.idx, dev.dev, fmt_sectors(d.total - d.free), fmt_sectors(d.free));

        let mut dts: Vec<_> = d.by_data_type.iter().collect();
        dts.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        for (dt, sectors) in dts {
            println!("  {:<12} {}", data_type_name(*dt), fmt_sectors(*sectors));
        }
        println!();
    }

    if !target_totals.is_empty() {
        println!("by background_target:");
        let mut ts: Vec<_> = target_totals.iter().collect();
        ts.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        for (label, sectors) in ts {
            println!("  {:<20} {}", label, fmt_sectors(*sectors));
        }
        println!();
    }

    match path_prefix {
        Some(prefix) => {
            println!("under {}:", prefix);
            match tree.find(prefix) {
                Some(node) => {
                    println!("  total: {}", fmt_sectors(node.total));
                    print_tree(node, 1, 0, usize::MAX);
                }
                None => println!("  no samples landed under this prefix"),
            }
        }
        None => {
            println!("top paths (depth {}, top {} per level):", TREE_DEPTH, TREE_TOP_K);
            print_tree(tree, 0, TREE_DEPTH - 1, TREE_TOP_K);
        }
    }

    println!();
    println!("note: <free> includes sb/journal and bucket reserves (no backpointers); compare with 'bcachefs fs usage'");
    if reflink_attributed {
        println!("note: reflinked (shared) data is charged fractionally to each referencing file");
    }
}

pub const CMD: super::CmdDef = typed_cmd!("du", "Disk usage statistics", Cli, fs_du);
