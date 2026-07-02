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

use std::collections::HashMap;

use anyhow::{Context, Result};
use bch_bindgen::accounting::data_type_from_u8;
use bch_bindgen::c::{bch_ioctl_rev_map, btree_id, BCH_REV_MAP_RESOLVE_PATH};
use bcachefs_kernel::btree;
use bcachefs_kernel::opts::prt_data_type;
use bcachefs_kernel::util::printbuf::Printbuf;
use clap::Parser;
use errno::Errno;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::commands::device;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::{self, DevInfo};

const DEFAULT_SAMPLES: u64 = 100_000;
const PATH_BUF_SIZE: usize = 4096;
const TREE_DEPTH: usize = 4;
const TREE_TOP_K: usize = 15;

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
        run_sampler(&handle, cli.samples, cli.seed, cli.path.as_deref())?;
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

struct DeviceGeom {
    idx: u32,
    total_sectors: u64,
}

/// Everything sampling needs about the filesystem's shape: online devices,
/// per-device geometry, the capacity-weighted device selection
/// distribution, and the resolved RNG seed.
struct SamplingSetup {
    devs: Vec<DevInfo>,
    geoms: Vec<DeviceGeom>,
    total_sectors: u64,
    dev_dist: WeightedIndex<u64>,
    seed: u64,
}

fn sampling_setup(handle: &BcachefsHandle, seed: Option<u64>) -> Result<SamplingSetup> {
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

/// Per-device sample accounting, keyed on the ioctl's `data_type` output
/// so it lines up 1:1 with `bcachefs fs usage -f devices`.
#[derive(Default)]
struct PerDeviceCounters {
    total: f64,
    free: f64,
    by_data_type: HashMap<u8, f64>,
}

/// A node in the du-style path tree. Real files/directories are nested by
/// path component; pseudo-categories (`<free>`, `<unresolved>`, `<reflink>`,
/// `<btree>`, ...) are single-level children of the root.
#[derive(Default)]
struct DirNode {
    total: f64,
    children: HashMap<String, DirNode>,
}

impl DirNode {
    fn insert_path(&mut self, path: &str, weight: f64) {
        self.total += weight;
        let mut node = self;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            node = node.children.entry(comp.to_string()).or_default();
            node.total += weight;
        }
    }

    fn insert_pseudo(&mut self, name: &str, weight: f64) {
        self.total += weight;
        let child = self.children.entry(name.to_string()).or_default();
        child.total += weight;
    }

    fn find(&self, path: &str) -> Option<&DirNode> {
        let mut node = self;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            node = node.children.get(comp)?;
        }
        Some(node)
    }

    fn sorted_children(&self) -> Vec<(&String, &DirNode)> {
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

fn fmt_sectors(sectors: f64) -> String {
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

#[allow(clippy::too_many_arguments)]
fn classify_sample(
    arg: &bch_ioctl_rev_map,
    path_buf: &[u8],
    weight: f64,
    tree: &mut DirNode,
    dev: &mut PerDeviceCounters,
    target_totals: &mut HashMap<String, f64>,
    devs: &[DevInfo],
) {
    dev.total += weight;

    if arg.found == 0 {
        dev.free += weight;
        tree.insert_pseudo("<free>", weight);
        return;
    }

    *dev.by_data_type.entry(arg.data_type).or_insert(0.0) += weight;

    let bid = arg.btree_id as u32;
    if bid == u32::from(btree_id::extents) {
        *target_totals.entry(describe_target(arg.background_target, devs)).or_insert(0.0) += weight;

        match extract_path(path_buf) {
            // Disconnected inodes resolve successfully but yield diagnostic
            // text (e.g. "(ENOENT_inode_no_backpointer: disconnected at
            // ...)") instead of a real path. Real paths always start with
            // '/'; fold anything else into <unresolved> instead of letting
            // it clutter the top-level tree with one node per string.
            Some(path) if path.starts_with('/') => tree.insert_path(&path, weight),
            _ => tree.insert_pseudo("<unresolved>", weight),
        }
    } else if bid == u32::from(btree_id::reflink) {
        tree.insert_pseudo("<reflink>", weight);
    } else {
        tree.insert_pseudo(&format!("<{}>", data_type_name(arg.data_type)), weight);
    }
}

fn run_sampler(
    handle: &BcachefsHandle,
    n_samples: u64,
    seed: Option<u64>,
    path_prefix: Option<&str>,
) -> Result<()> {
    if n_samples == 0 {
        anyhow::bail!("--samples must be greater than zero");
    }

    let SamplingSetup { devs, geoms, total_sectors, dev_dist, seed } =
        sampling_setup(handle, seed)?;
    let mut rng = StdRng::seed_from_u64(seed);

    let weight_per_sample = total_sectors as f64 / n_samples as f64;

    let mut path_buf = vec![0u8; PATH_BUF_SIZE];
    let mut tree = DirNode::default();
    let mut per_dev: Vec<PerDeviceCounters> = devs.iter().map(|_| PerDeviceCounters::default()).collect();
    let mut target_totals: HashMap<String, f64> = HashMap::new();

    for _ in 0..n_samples {
        let dev_pos = dev_dist.sample(&mut rng);
        let dev_idx = geoms[dev_pos].idx;
        let dev_total = geoms[dev_pos].total_sectors;
        let offset = rng.gen_range(0..dev_total);

        // Clear only the leading byte: the kernel writes a NUL-terminated
        // path on success, but leaves the buffer untouched when no path is
        // resolved, so a stale byte here would be misread as a leftover
        // path from a previous sample.
        path_buf[0] = 0;

        let mut arg = bch_ioctl_rev_map {
            dev: dev_idx,
            flags: BCH_REV_MAP_RESOLVE_PATH,
            offset,
            buf: path_buf.as_mut_ptr() as u64,
            buf_size: path_buf.len() as u32,
            ..Default::default()
        };

        if let Err(Errno(raw)) = handle.rev_map(&mut arg) {
            // -ERANGE means the path didn't fit `buf`; found/data_type/
            // btree_id are still valid, we just can't attribute a path.
            if raw != libc::ERANGE {
                return Err(std::io::Error::from_raw_os_error(raw)).with_context(|| {
                    format!("rev_map ioctl failed for dev {} offset {}", dev_idx, offset)
                });
            }
            path_buf[0] = 0;
        }

        classify_sample(&arg, &path_buf, weight_per_sample, &mut tree, &mut per_dev[dev_pos], &mut target_totals, &devs);
    }

    print_report(total_sectors, n_samples, seed, &devs, &per_dev, &tree, &target_totals, path_prefix);
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
}

pub const CMD: super::CmdDef = typed_cmd!("du", "Disk usage statistics", Cli, fs_du);
