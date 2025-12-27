use anyhow::{bail, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{stdout, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::logging;

/// Iterator over lines with lossy UTF-8 handling
struct LossyLines<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: BufRead> LossyLines<R> {
    fn new(reader: R) -> Self {
        Self { reader, buf: Vec::with_capacity(1024) }
    }
}

impl<R: BufRead> Iterator for LossyLines<R> {
    type Item = std::io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        self.buf.clear();
        match self.reader.read_until(b'\n', &mut self.buf) {
            Ok(0) => None,
            Ok(_) => {
                if self.buf.ends_with(b"\n") {
                    self.buf.pop();
                }
                Some(Ok(String::from_utf8_lossy(&self.buf).into_owned()))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

const INODE_OPTS: &[&str] = &[
    "bi_data_checksum",
    "bi_compression",
    "bi_background_compression",
    "bi_data_replicas",
    "bi_promote_target",
    "bi_foreground_target",
    "bi_background_target",
    "bi_erasure_code",
    "bi_project",
];

fn format_opt(name: &str, val: u64) -> String {
    let short_name = name.strip_prefix("bi_").unwrap_or(name);

    // inode opts have +1 bias: stored 0=inherit, 1=actual 0, 2=actual 1, etc.
    // Subtract 1 to get actual value (we only show non-zero stored values)
    let actual = val.saturating_sub(1);

    let val_str = match name {
        "bi_compression" | "bi_background_compression" => match actual {
            0 => "none".into(),
            1 => "lz4".into(),
            2 => "gzip".into(),
            3 => "zstd".into(),
            _ => format!("{}", actual),
        },
        "bi_data_checksum" => match actual {
            0 => "none".into(),
            1 => "crc32c".into(),
            2 => "crc64".into(),
            3 => "xxhash".into(),
            _ => format!("{}", actual),
        },
        "bi_data_replicas" => format!("{}", actual),
        "bi_promote_target" | "bi_foreground_target" | "bi_background_target" => {
            // TODO: resolve target ID to label from sysfs
            format!("{}", actual)
        }
        _ => format!("{}", actual),
    };

    format!("{}={}", short_name, val_str)
}

#[derive(Debug)]
struct InodeMatch {
    inum: u64,
    subvol: u32,
    mode: u16,
    bi_dir: u64,
    bi_dir_offset: u64,
    opts: Vec<(&'static str, u64)>,
}

fn mode_to_type_char(mode: u16) -> char {
    match mode & 0o170000 {
        0o040000 => 'd',
        0o100000 => '-',
        0o120000 => 'l',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '?',
    }
}

struct FsInfo {
    uuid: String,
    debugfs: PathBuf,
}

fn get_fs_info(mount_path: &Path) -> Result<FsInfo> {
    let mount_path = mount_path.canonicalize()?;
    let mount_str = mount_path.to_string_lossy();

    let file = File::open("/proc/self/mountinfo")?;
    let reader = BufReader::new(file);

    for line in LossyLines::new(reader) {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 10 {
            continue;
        }

        let mp = parts[4];
        if mp != mount_str {
            continue;
        }

        let sep_idx = parts.iter().position(|&p| p == "-");
        if let Some(idx) = sep_idx {
            if idx + 2 < parts.len() {
                let fstype = parts[idx + 1];
                if fstype != "bcachefs" {
                    bail!("{} is not a bcachefs mount (found: {})", mount_str, fstype);
                }
                let source = parts[idx + 2];
                let device = source.split(':').next().unwrap_or(source);

                // Get UUID from blkid
                let output = std::process::Command::new("blkid")
                    .args(["-s", "UUID", "-o", "value", device])
                    .output()?;

                if !output.status.success() {
                    bail!("blkid failed for {}", device);
                }

                let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let debugfs = PathBuf::from(format!("/sys/kernel/debug/bcachefs/{}", uuid));

                if !debugfs.is_dir() {
                    bail!("debugfs not found: {} (need root?)", debugfs.display());
                }

                return Ok(FsInfo { uuid, debugfs });
            }
        }
    }

    bail!("mount point not found: {}", mount_str)
}

/// Sorted Vec-based parent cache with binary search lookup
struct ParentCache {
    // Sorted by inum: (inum, bi_dir, bi_dir_offset)
    data: Vec<(u64, u64, u64)>,
}

impl ParentCache {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn push(&mut self, inum: u64, bi_dir: u64, bi_dir_offset: u64) {
        if bi_dir != 0 {
            self.data.push((inum, bi_dir, bi_dir_offset));
        }
    }

    fn finalize(&mut self) {
        self.data.sort_unstable_by_key(|&(inum, _, _)| inum);
        self.data.dedup_by_key(|&mut (inum, _, _)| inum);
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

fn parse_inodes(
    reader: impl BufRead,
    total_bytes: Option<u64>,
    dirs_only: bool,
    verbose: bool,
    quiet: bool,
    build_cache: bool,
) -> Result<(Vec<InodeMatch>, Option<ParentCache>)> {
    let mut matches = Vec::new();
    let mut cache = if build_cache { Some(ParentCache::new()) } else { None };
    let mut current_inum: Option<u64> = None;
    let mut current_subvol: u32 = 0;
    let mut current_opts: Vec<(&'static str, u64)> = Vec::new();
    let mut current_mode: u16 = 0;
    let mut bi_dir: u64 = 0;
    let mut bi_dir_offset: u64 = 0;
    let mut last_inum: Option<u64> = None;

    let mut read_bytes: u64 = 0;
    let mut last_pct: i32 = -1;

    let flush = |inum: Option<u64>,
                 subvol: u32,
                 mode: u16,
                 last: &mut Option<u64>,
                 opts: &[(&'static str, u64)],
                 bi_dir: u64,
                 bi_dir_offset: u64,
                 dirs_only: bool,
                 verbose: bool,
                 matches: &mut Vec<InodeMatch>,
                 cache: &mut Option<ParentCache>| {
        if let Some(inum) = inum {
            // Always cache parent info if caching is enabled
            if let Some(ref mut c) = cache {
                c.push(inum, bi_dir, bi_dir_offset);
            }

            let is_dir = (mode & 0o170000) == 0o040000;
            if *last != Some(inum) && !opts.is_empty() && (!dirs_only || is_dir) {
                if verbose {
                    eprintln!("inum={} opts={:?}", inum, opts);
                }
                matches.push(InodeMatch {
                    inum,
                    subvol,
                    mode,
                    bi_dir,
                    bi_dir_offset,
                    opts: opts.to_vec(),
                });
            }
            *last = Some(inum);
        }
    };

    for line in LossyLines::new(reader) {
        let line = line?;
        read_bytes += line.len() as u64 + 1;

        if let Some(total) = total_bytes {
            if !quiet && total > 0 {
                let pct = (1000 * read_bytes / total) as i32;
                if pct != last_pct {
                    eprint!("\rinodes: {:.1}%", pct as f32 / 10.0);
                    std::io::stderr().flush().ok();
                    last_pct = pct;
                }
            }
        }

        let line = line.trim_end();

        if line.starts_with("u64s ") && line.contains("inode_v3") {
            flush(
                current_inum,
                current_subvol,
                current_mode,
                &mut last_inum,
                &current_opts,
                bi_dir,
                bi_dir_offset,
                dirs_only,
                verbose,
                &mut matches,
                &mut cache,
            );

            // Parse subvol:inum from key at index 4
            // e.g. "u64s 18 type inode_v3 0:4096:U32_MAX len 0 ver 0"
            if let Some(key) = line.split_whitespace().nth(4) {
                let mut parts = key.split(':');
                current_subvol = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                current_inum = parts.next().and_then(|s| s.parse().ok());
            } else {
                current_inum = None;
                current_subvol = 0;
            }
            current_opts.clear();
            current_mode = 0;
            bi_dir = 0;
            bi_dir_offset = 0;
        } else if line.starts_with("u64s ") {
            flush(
                current_inum,
                current_subvol,
                current_mode,
                &mut last_inum,
                &current_opts,
                bi_dir,
                bi_dir_offset,
                dirs_only,
                verbose,
                &mut matches,
                &mut cache,
            );
            current_inum = None;
            current_subvol = 0;
        } else if current_inum.is_some() && line.contains('=') {
            let trimmed = line.trim();
            if let Some((key, val)) = trimmed.split_once('=') {
                match key {
                    "mode" => {
                        current_mode = u16::from_str_radix(val, 8).unwrap_or(0);
                    }
                    "bi_dir" => {
                        bi_dir = val.parse().unwrap_or(0);
                    }
                    "bi_dir_offset" => {
                        bi_dir_offset = val.parse().unwrap_or(0);
                    }
                    _ => {
                        for opt in INODE_OPTS {
                            if key == *opt {
                                if let Ok(v) = val.parse::<u64>() {
                                    if v != 0 {
                                        current_opts.push((*opt, v));
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    flush(
        current_inum,
        current_subvol,
        current_mode,
        &mut last_inum,
        &current_opts,
        bi_dir,
        bi_dir_offset,
        dirs_only,
        verbose,
        &mut matches,
        &mut cache,
    );

    // Finalize cache (sort and dedup)
    if let Some(ref mut c) = cache {
        c.finalize();
        if !quiet {
            eprintln!("\rinodes: done, {} matches, {} cached parents", matches.len(), c.len());
        }
    } else if !quiet {
        eprintln!("\rinodes: done, {} matches", matches.len());
    }

    Ok((matches, cache))
}

fn collect_needed_parents(matches: &[InodeMatch]) -> HashSet<u64> {
    matches
        .iter()
        .filter_map(|m| if m.bi_dir != 0 { Some(m.bi_dir) } else { None })
        .collect()
}

fn build_parent_map(
    debugfs: &Path,
    mut needed: HashSet<u64>,
    total_bytes: Option<u64>,
    quiet: bool,
) -> Result<HashMap<u64, (u64, u64)>> {
    let mut parent_map = HashMap::new();
    let inode_keys = debugfs.join("btrees/inodes/keys");

    while !needed.is_empty() {
        if !quiet {
            eprintln!("resolving {} parent inums...", needed.len());
        }

        let file = File::open(&inode_keys)?;
        let reader = BufReader::new(file);

        let mut current_inum: Option<u64> = None;
        let mut bi_dir: u64 = 0;
        let mut bi_dir_offset: u64 = 0;
        let mut found_this_pass: HashMap<u64, (u64, u64)> = HashMap::new();
        let mut read_bytes: u64 = 0;
        let mut last_pct: i32 = -1;

        for line in LossyLines::new(reader) {
            // Early exit if we found all needed
            if found_this_pass.len() == needed.len() {
                break;
            }

            let line = line?;

            read_bytes += line.len() as u64 + 1;
            if let Some(total) = total_bytes {
                if !quiet && total > 0 {
                    let pct = (1000 * read_bytes / total) as i32;
                    if pct != last_pct {
                        eprint!("\rparents: {:.1}%", pct as f32 / 10.0);
                        std::io::stderr().flush().ok();
                        last_pct = pct;
                    }
                }
            }
            let line = line.trim_end();

            if line.starts_with("u64s ") && line.contains("inode_v3") {
                if let Some(inum) = current_inum {
                    if needed.contains(&inum) && bi_dir != 0 {
                        found_this_pass.insert(inum, (bi_dir, bi_dir_offset));
                    }
                }

                if let Some(key) = line.split_whitespace().nth(4) {
                    if let Some(inum_str) = key.split(':').nth(1) {
                        current_inum = inum_str.parse().ok();
                    } else {
                        current_inum = None;
                    }
                } else {
                    current_inum = None;
                }
                bi_dir = 0;
                bi_dir_offset = 0;
            } else if line.starts_with("u64s ") {
                if let Some(inum) = current_inum {
                    if needed.contains(&inum) && bi_dir != 0 {
                        found_this_pass.insert(inum, (bi_dir, bi_dir_offset));
                    }
                }
                current_inum = None;
            } else if current_inum.is_some() {
                let trimmed = line.trim();
                if trimmed.starts_with("bi_dir=") && !trimmed.contains("offset") {
                    if let Some(val) = trimmed.strip_prefix("bi_dir=") {
                        bi_dir = val.parse().unwrap_or(0);
                    }
                } else if trimmed.starts_with("bi_dir_offset=") {
                    if let Some(val) = trimmed.strip_prefix("bi_dir_offset=") {
                        bi_dir_offset = val.parse().unwrap_or(0);
                    }
                }
            }
        }

        if let Some(inum) = current_inum {
            if needed.contains(&inum) && bi_dir != 0 {
                found_this_pass.insert(inum, (bi_dir, bi_dir_offset));
            }
        }

        if !quiet && total_bytes.is_some() {
            eprintln!("\rparents: found {}/{}", found_this_pass.len(), needed.len());
        }

        parent_map.extend(found_this_pass.iter());

        let mut next_needed = HashSet::new();
        for inum in &needed {
            if let Some((parent_dir, _)) = found_this_pass.get(inum) {
                if *parent_dir != 0 && !parent_map.contains_key(parent_dir) {
                    next_needed.insert(*parent_dir);
                }
            }
        }
        needed = next_needed;
    }

    Ok(parent_map)
}

fn collect_needed_dirents(
    matches: &[InodeMatch],
    parent_map: &HashMap<u64, (u64, u64)>,
) -> HashSet<(u64, u64)> {
    let mut needed = HashSet::new();

    for m in matches {
        if m.bi_dir != 0 {
            needed.insert((m.bi_dir, m.bi_dir_offset));
        }

        let mut inum = m.inum;
        let mut seen = HashSet::new();

        while inum != 0 && !seen.contains(&inum) {
            seen.insert(inum);
            if let Some((bi_dir, bi_offset)) = parent_map.get(&inum) {
                if *bi_dir != 0 {
                    needed.insert((*bi_dir, *bi_offset));
                }
                inum = *bi_dir;
            } else {
                break;
            }
        }
    }

    needed
}

fn build_dirent_map(
    debugfs: &Path,
    needed: &HashSet<(u64, u64)>,
    total_bytes: Option<u64>,
    quiet: bool,
) -> Result<HashMap<(u64, u64), String>> {
    let dirent_keys = debugfs.join("btrees/dirents/keys");
    let file = File::open(&dirent_keys)?;
    let reader = BufReader::new(file);

    let mut dirent_map = HashMap::new();
    let mut read_bytes: u64 = 0;
    let mut last_pct: i32 = -1;

    for line in LossyLines::new(reader) {
        let line = line?;
        read_bytes += line.len() as u64 + 1;

        if let Some(total) = total_bytes {
            if !quiet && total > 0 {
                let pct = (1000 * read_bytes / total) as i32;
                if pct != last_pct {
                    eprint!("\rdirents: {:.1}%", pct as f32 / 10.0);
                    std::io::stderr().flush().ok();
                    last_pct = pct;
                }
            }
        }

        if !line.contains("type dirent") {
            continue;
        }

        // Parse key "inode:offset:snapshot" - key is at index 4
        // e.g. "u64s 6 type dirent 4096:12345:U32_MAX len 0 ver 0"
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }

        let key_str = parts[4];
        let key_parts: Vec<&str> = key_str.split(':').collect();
        if key_parts.len() < 2 {
            continue;
        }

        let inode: u64 = match key_parts[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let offset: u64 = match key_parts[1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let key = (inode, offset);
        if !needed.contains(&key) {
            continue;
        }

        // Parse name: "... : name -> target type"
        if let Some(colon_pos) = line.rfind(": ") {
            let after_colon = &line[colon_pos + 2..];
            if let Some(arrow_pos) = after_colon.find(" -> ") {
                let name = after_colon[..arrow_pos].trim();
                if !name.is_empty() {
                    dirent_map.insert(key, name.to_string());
                }
            }
        }
    }

    if !quiet {
        eprintln!("\rdirents: done, {} resolved", dirent_map.len());
    }

    Ok(dirent_map)
}

fn resolve_path(
    m: &InodeMatch,
    parent_map: &HashMap<u64, (u64, u64)>,
    dirent_map: &HashMap<(u64, u64), String>,
) -> String {
    let mut parts = Vec::new();
    let mut seen = HashSet::new();

    if m.bi_dir != 0 {
        if let Some(name) = dirent_map.get(&(m.bi_dir, m.bi_dir_offset)) {
            parts.push(name.clone());
        } else {
            parts.push("?".to_string());
        }

        let mut current = m.bi_dir;
        while current != 0 && !seen.contains(&current) {
            seen.insert(current);
            if let Some((pbi_dir, pbi_offset)) = parent_map.get(&current) {
                if *pbi_dir == 0 {
                    break;
                }
                if let Some(name) = dirent_map.get(&(*pbi_dir, *pbi_offset)) {
                    parts.push(name.clone());
                } else {
                    parts.push("?".to_string());
                }
                current = *pbi_dir;
            } else {
                break;
            }
        }
    }

    if parts.is_empty() {
        return "/".to_string();
    }

    parts.reverse();
    format!("/{}", parts.join("/"))
}

fn get_btree_size(mount_path: &Path, btree: &str) -> Option<u64> {
    let output = std::process::Command::new("bcachefs")
        .args(["fs", "usage", "-f", "btree"])
        .arg(mount_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with(btree) {
            if let Some(val) = line.strip_prefix(btree).and_then(|s| s.strip_prefix(':')) {
                return val.trim().parse().ok();
            }
        }
    }
    None
}

/// Find inodes with non-default bcachefs options (mounted filesystem via debugfs)
#[derive(Parser, Debug)]
pub struct Cli {
    /// Only show directories
    #[arg(short, long)]
    dirs: bool,

    /// Resolve file paths (requires additional btree scans)
    #[arg(short = 'P', long)]
    resolve_paths: bool,

    /// Quiet mode (no progress output)
    #[arg(short, long)]
    quiet: bool,

    /// Verbose mode (log matches as found)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Force color on/off
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Low memory mode: rescan btree for parent resolution instead of caching
    #[arg(short = 'L', long)]
    low_memory: bool,

    /// Mount path
    mount_path: PathBuf,
}

fn cmd_inner(opt: &Cli) -> Result<()> {
    let fs_info = get_fs_info(&opt.mount_path)?;

    if !opt.quiet {
        eprintln!("uuid: {}", fs_info.uuid);
    }

    let inode_size = get_btree_size(&opt.mount_path, "inodes");
    let dirent_size = get_btree_size(&opt.mount_path, "dirents");

    if !opt.quiet {
        if let Some(size) = inode_size {
            eprintln!("inode btree: {:.2} GB", size as f64 / 1e9);
        }
        if let Some(size) = dirent_size {
            eprintln!("dirent btree: {:.2} GB", size as f64 / 1e9);
        }
    }

    let inode_keys = fs_info.debugfs.join("btrees/inodes/keys");
    let file = File::open(&inode_keys)?;
    let reader = BufReader::new(file);

    // Build cache if resolving paths and not in low-memory mode
    let build_cache = opt.resolve_paths && !opt.low_memory;
    let (matches, parent_cache) = parse_inodes(
        reader,
        inode_size,
        opt.dirs,
        opt.verbose > 0,
        opt.quiet,
        build_cache,
    )?;

    if matches.is_empty() {
        if !opt.quiet {
            eprintln!("no inodes with non-default options found");
        }
        return Ok(());
    }

    if opt.resolve_paths {
        // Build parent_map either from cache or by rescanning
        let parent_map: HashMap<u64, (u64, u64)> = if let Some(cache) = parent_cache {
            // Use cached data - no rescans needed
            if !opt.quiet {
                eprintln!("using cached parent data");
            }
            cache.data.iter().map(|&(i, d, o)| (i, (d, o))).collect()
        } else {
            // Low-memory mode: rescan for parents
            let initial_parents: HashMap<u64, (u64, u64)> = matches
                .iter()
                .filter(|m| m.bi_dir != 0)
                .map(|m| (m.inum, (m.bi_dir, m.bi_dir_offset)))
                .collect();

            let needed = collect_needed_parents(&matches);
            let mut parent_map =
                build_parent_map(&fs_info.debugfs, needed, inode_size, opt.quiet)?;
            parent_map.extend(initial_parents);
            parent_map
        };

        if !opt.quiet {
            eprintln!("parent_map entries: {}", parent_map.len());
        }

        let needed_dirents = collect_needed_dirents(&matches, &parent_map);
        if !opt.quiet {
            eprintln!("need {} dirent lookups", needed_dirents.len());
        }

        let dirent_map =
            build_dirent_map(&fs_info.debugfs, &needed_dirents, dirent_size, opt.quiet)?;

        for m in &matches {
            let path = resolve_path(m, &parent_map, &dirent_map);
            let opts_str: Vec<String> = m.opts.iter().map(|(k, v)| format_opt(k, *v)).collect();
            println!(
                "{} {}:{}\t{}\t{}",
                mode_to_type_char(m.mode),
                m.subvol,
                m.inum,
                opts_str.join(" "),
                path
            );
        }
    } else {
        for m in &matches {
            let opts_str: Vec<String> = m.opts.iter().map(|(k, v)| format_opt(k, *v)).collect();
            println!(
                "{} {}:{}\t{}",
                mode_to_type_char(m.mode),
                m.subvol,
                m.inum,
                opts_str.join(" ")
            );
        }
    }

    Ok(())
}

pub fn inode_opts_mounted(argv: Vec<String>) -> Result<()> {
    let opt = Cli::parse_from(argv);
    logging::setup(opt.verbose, opt.colorize);
    cmd_inner(&opt)
}
