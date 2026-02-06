use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::wrappers::handle::BcachefsHandle;

const SYSFS_BASE: &str = "/sys/fs/bcachefs";

// JSON structs matching kernel output from bch2_time_stats_to_json()

#[derive(Deserialize, Serialize, Debug, Clone)]
struct DurationStats {
    min:    u64,
    max:    u64,
    #[serde(default)]
    total:  u64,
    mean:   u64,
    stddev: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct EwmaStats {
    mean:   u64,
    stddev: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[allow(dead_code)]
struct TimeStats {
    count:              u64,
    duration_ns:        DurationStats,
    duration_ewma_ns:   EwmaStats,
    between_ns:         DurationStats,
    between_ewma_ns:    EwmaStats,
}

struct StatEntry {
    name:   String,
    stats:  TimeStats,
}

// Duration formatting

struct TimeUnit {
    name:   &'static str,
    nsecs:  u64,
}

const TIME_UNITS: &[TimeUnit] = &[
    TimeUnit { name: "ns", nsecs: 1 },
    TimeUnit { name: "us", nsecs: 1_000 },
    TimeUnit { name: "ms", nsecs: 1_000_000 },
    TimeUnit { name: "s",  nsecs: 1_000_000_000 },
];

fn pick_time_unit(ns: u64) -> &'static TimeUnit {
    let mut best = &TIME_UNITS[0];
    for u in TIME_UNITS {
        if ns >= u.nsecs * 10 {
            best = u;
        }
    }
    best
}

fn fmt_duration(ns: u64) -> String {
    if ns == 0 {
        return "0".to_string();
    }
    let u = pick_time_unit(ns);
    format!("{}{}", ns / u.nsecs, u.name)
}

fn fmt_duration_padded(ns: u64, width: usize) -> String {
    format!("{:>width$}", fmt_duration(ns), width = width)
}

// Sysfs path from fd: resolve /proc/self/fd/<N> to get the real path

fn sysfs_path_from_fd(fd: i32) -> Result<PathBuf> {
    let link = format!("/proc/self/fd/{}", fd);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", fd))
}

// Filesystem discovery

fn find_all_sysfs_dirs() -> Result<Vec<PathBuf>> {
    let base = Path::new(SYSFS_BASE);
    if !base.exists() {
        return Err(anyhow!("No bcachefs filesystems found ({}/ does not exist)", SYSFS_BASE));
    }

    let mut results = Vec::new();
    for entry in fs::read_dir(base).context("reading sysfs bcachefs directory")? {
        let entry = entry?;
        results.push(entry.path());
    }

    if results.is_empty() {
        return Err(anyhow!("No mounted bcachefs filesystems found"));
    }

    results.sort();
    Ok(results)
}

// Reading stats from sysfs

fn read_time_stats(sysfs_path: &Path) -> Result<Vec<StatEntry>> {
    let json_dir = sysfs_path.join("time_stats_json");
    let mut entries = Vec::new();

    if !json_dir.exists() {
        return Err(anyhow!("time_stats_json not found in {} - kernel may need to be updated",
                           sysfs_path.display()));
    }

    for file in fs::read_dir(&json_dir).context("reading time_stats_json directory")? {
        let file = file?;
        let name = file.file_name().to_string_lossy().to_string();
        let content = fs::read_to_string(file.path())
            .with_context(|| format!("reading {}", file.path().display()))?;

        match serde_json::from_str::<TimeStats>(&content) {
            Ok(stats) => entries.push(StatEntry { name, stats }),
            Err(e) => eprintln!("warning: failed to parse {}: {}", file.path().display(), e),
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn read_device_latency_stats(sysfs_path: &Path) -> Result<Vec<StatEntry>> {
    let mut entries = Vec::new();

    let Ok(dir) = fs::read_dir(sysfs_path) else { return Ok(entries) };

    for entry in dir {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("dev-") { continue }

        let dev_path = entry.path();
        for (suffix, label) in [("io_latency_stats_read_json", "read"),
                                 ("io_latency_stats_write_json", "write")] {
            let stat_path = dev_path.join(suffix);
            if !stat_path.exists() { continue }

            let content = fs::read_to_string(&stat_path)
                .with_context(|| format!("reading {}", stat_path.display()))?;

            match serde_json::from_str::<TimeStats>(&content) {
                Ok(stats) => entries.push(StatEntry {
                    name: format!("{}/{}", name, label),
                    stats,
                }),
                Err(e) => eprintln!("warning: failed to parse {}: {}", stat_path.display(), e),
            }
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

// Table formatting

const COL_WIDTH: usize = 10;

fn print_header() {
    println!("{:<40} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$}",
        "NAME", "COUNT",
        "DUR_MIN", "DUR_MAX", "DUR_TOTAL",
        "MEAN_SINCE", "MEAN_RECENT",
        "STDDEV_SINCE", "STDDEV_RECENT",
    );
}

fn print_stat(entry: &StatEntry) {
    let s = &entry.stats;
    println!("{:<40} {:>COL_WIDTH$} {} {} {} {} {} {} {}",
        entry.name,
        s.count,
        fmt_duration_padded(s.duration_ns.min,      COL_WIDTH),
        fmt_duration_padded(s.duration_ns.max,      COL_WIDTH),
        fmt_duration_padded(s.duration_ns.total,    COL_WIDTH),
        fmt_duration_padded(s.duration_ns.mean,     COL_WIDTH),
        fmt_duration_padded(s.duration_ewma_ns.mean, COL_WIDTH),
        fmt_duration_padded(s.duration_ns.stddev,   COL_WIDTH),
        fmt_duration_padded(s.duration_ewma_ns.stddev, COL_WIDTH),
    );
}

fn sort_entries(entries: &mut [StatEntry], sort_by: &SortBy) {
    match sort_by {
        SortBy::Name    => entries.sort_by(|a, b| a.name.cmp(&b.name)),
        SortBy::Count   => entries.sort_by(|a, b| b.stats.count.cmp(&a.stats.count)),
        SortBy::Mean    => entries.sort_by(|a, b| b.stats.duration_ns.mean.cmp(&a.stats.duration_ns.mean)),
        SortBy::Total   => entries.sort_by(|a, b| b.stats.duration_ns.total.cmp(&a.stats.duration_ns.total)),
    }
}

fn print_json(all_stats: &[(PathBuf, Vec<StatEntry>, Vec<StatEntry>)]) -> Result<()> {
    let mut out: BTreeMap<String, BTreeMap<&str, &TimeStats>> = BTreeMap::new();

    for (sysfs_path, fs_stats, dev_stats) in all_stats {
        let label = sysfs_path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut fs_map = BTreeMap::new();
        for e in fs_stats { fs_map.insert(e.name.as_str(), &e.stats); }
        for e in dev_stats { fs_map.insert(e.name.as_str(), &e.stats); }
        out.insert(label, fs_map);
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn display_stats(all_stats: Vec<(PathBuf, Vec<StatEntry>, Vec<StatEntry>)>, cli: &Cli) -> Result<()> {
    if cli.json {
        return print_json(&all_stats);
    }

    let multi = all_stats.len() > 1;

    for (sysfs_path, mut fs_stats, mut dev_stats) in all_stats {
        if multi {
            let label = sysfs_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            println!("{}:", label);
        }

        if !cli.all {
            fs_stats.retain(|e| e.stats.count > 0);
            dev_stats.retain(|e| e.stats.count > 0);
        }

        sort_entries(&mut fs_stats, &cli.sort);

        print_header();
        for entry in &fs_stats {
            print_stat(entry);
        }

        if !dev_stats.is_empty() {
            println!();
            println!("Per-device IO latency:");
            print_header();
            sort_entries(&mut dev_stats, &cli.sort);
            for entry in &dev_stats {
                print_stat(entry);
            }
        }

        println!();
    }

    Ok(())
}

// CLI

#[derive(ValueEnum, Clone, Debug, Default)]
enum SortBy {
    #[default]
    Name,
    Count,
    Mean,
    Total,
}

#[derive(Parser, Debug)]
#[command(about = "Display bcachefs time statistics")]
pub struct Cli {
    /// Show stats with zero count
    #[arg(short, long)]
    all: bool,

    /// Sort by column
    #[arg(short, long, default_value = "name")]
    sort: SortBy,

    /// Output raw JSON
    #[arg(long)]
    json: bool,

    /// Skip per-device IO latency stats
    #[arg(long)]
    no_device_stats: bool,

    /// Filesystem UUID, device, or mount point (default: all)
    filesystem: Option<String>,
}

pub fn timestats(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let sysfs_paths: Vec<PathBuf> = if let Some(ref fs_arg) = cli.filesystem {
        // Use BcachefsHandle to resolve the filesystem
        let handle = BcachefsHandle::open(fs_arg)
            .map_err(|e| anyhow!("Failed to open filesystem '{}': {:?}", fs_arg, e))?;
        let path = sysfs_path_from_fd(handle.sysfs_fd())?;
        vec![path]
    } else {
        find_all_sysfs_dirs()?
    };

    let mut all_stats = Vec::new();
    for sysfs_path in sysfs_paths {
        let fs_stats = read_time_stats(&sysfs_path)?;

        let dev_stats = if cli.no_device_stats {
            Vec::new()
        } else {
            read_device_latency_stats(&sysfs_path)?
        };

        all_stats.push((sysfs_path, fs_stats, dev_stats));
    }

    display_stats(all_stats, &cli)
}
