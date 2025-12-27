use anyhow::{bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::btree::BtreeIter;
use bch_bindgen::btree::BtreeIterFlags;
use bch_bindgen::btree::BtreeTrans;
use bch_bindgen::c;
use bch_bindgen::c::bch_degraded_actions;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{stdout, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::logging;

/// Get bcachefs devices from a mount path by parsing /proc/self/mountinfo
fn get_devices_from_mount(mount_path: &Path) -> Result<Vec<PathBuf>> {
    let mount_path = mount_path.canonicalize()?;
    let mount_str = mount_path.to_string_lossy();

    let file = fs::File::open("/proc/self/mountinfo")?;
    let reader = std::io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 10 {
            continue;
        }

        // mountinfo format: id parent major:minor root mount_point options ... - fstype source options
        let mp = parts[4];
        if mp != mount_str {
            continue;
        }

        // Find the separator "-"
        let sep_idx = parts.iter().position(|&p| p == "-");
        if let Some(idx) = sep_idx {
            if idx + 2 < parts.len() {
                let fstype = parts[idx + 1];
                if fstype != "bcachefs" {
                    bail!("{} is not a bcachefs mount (found: {})", mount_str, fstype);
                }
                let source = parts[idx + 2];
                // bcachefs source can be "dev1:dev2:dev3" for multi-device
                let devices: Vec<PathBuf> = source
                    .split(':')
                    .map(PathBuf::from)
                    .collect();
                return Ok(devices);
            }
        }
    }

    bail!("mount point not found: {}", mount_str)
}

/// Check if path is a mount point or a device
fn resolve_devices(path: &Path) -> Result<Vec<PathBuf>> {
    let meta = fs::metadata(path)?;

    if meta.is_dir() {
        // It's a directory, treat as mount point
        get_devices_from_mount(path)
    } else {
        // Assume it's a device
        Ok(vec![path.to_path_buf()])
    }
}

#[derive(Debug)]
struct InodeOpts {
    inum: u64,
    bi_dir: u64,
    bi_dir_offset: u64,
    opts: Vec<(&'static str, u64)>,
}

/// Get dirent name from a bch_dirent
fn get_dirent_name(v: &c::bch_val, k: &c::bkey) -> Option<String> {
    unsafe {
        let dirent = v as *const c::bch_val as *const c::bch_dirent;
        let dirent_base_size = std::mem::size_of::<c::bch_dirent>();
        let val_bytes = (k.u64s as usize) * 8;
        let name_len = val_bytes.saturating_sub(dirent_base_size);

        if name_len > 0 && name_len < 512 {
            let name_ptr = &(*dirent).__bindgen_anon_2 as *const _ as *const u8;
            let name_slice = std::slice::from_raw_parts(name_ptr, name_len);
            let actual_len = name_slice.iter().position(|&b| b == 0).unwrap_or(name_len);
            if actual_len > 0 {
                return Some(String::from_utf8_lossy(&name_slice[..actual_len]).into_owned());
            }
        }
        None
    }
}

fn collect_inode_opts(fs: &Fs, dirs_only: bool, verbose: bool) -> Result<Vec<InodeOpts>> {
    let trans = BtreeTrans::new(fs);
    let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;

    let mut iter = BtreeIter::new(
        &trans,
        bcachefs::btree_id::BTREE_ID_inodes,
        bch_bindgen::POS_MIN,
        flags,
    );

    let mut matches = Vec::new();
    let mut last_inum: Option<u64> = None;
    let mut count = 0u64;

    while let Some(k) = iter.peek_and_restart()? {
        count += 1;
        if count % 100_000 == 0 {
            eprint!("\rprocessed {} inodes...", count);
            std::io::stderr().flush().ok();
        }

        let inum = k.k.p.inode;

        // Skip if same inum as last (different snapshots)
        if last_inum == Some(inum) {
            iter.advance();
            continue;
        }
        last_inum = Some(inum);

        // Only process inode_v3 (current format)
        if k.k.type_ != bcachefs::bch_bkey_type::KEY_TYPE_inode_v3 as u8 {
            iter.advance();
            continue;
        }

        let mut unpacked: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
        let bkey_s_c = c::bkey_s_c { k: k.k, v: k.v };

        let ret = unsafe { c::bch2_inode_unpack(bkey_s_c, &mut unpacked) };
        if ret != 0 {
            iter.advance();
            continue;
        }

        if dirs_only {
            let is_dir = (unpacked.bi_mode & 0o170000) == 0o040000;
            if !is_dir {
                iter.advance();
                continue;
            }
        }

        let mut opts = Vec::new();

        macro_rules! check_opt {
            ($field:ident, $name:expr) => {
                if unpacked.$field != 0 {
                    opts.push(($name, unpacked.$field as u64));
                }
            };
        }

        check_opt!(bi_data_checksum, "data_checksum");
        check_opt!(bi_compression, "compression");
        check_opt!(bi_background_compression, "background_compression");
        check_opt!(bi_data_replicas, "data_replicas");
        check_opt!(bi_promote_target, "promote_target");
        check_opt!(bi_foreground_target, "foreground_target");
        check_opt!(bi_background_target, "background_target");
        check_opt!(bi_erasure_code, "erasure_code");
        check_opt!(bi_project, "project");

        if !opts.is_empty() {
            if verbose {
                eprintln!("inum={} opts={:?}", inum, opts);
            }
            matches.push(InodeOpts {
                inum,
                bi_dir: unpacked.bi_dir,
                bi_dir_offset: unpacked.bi_dir_offset,
                opts,
            });
        }

        iter.advance();
    }

    eprintln!("\rprocessed {} inodes, {} matches", count, matches.len());

    Ok(matches)
}

fn collect_needed_parents(matches: &[InodeOpts]) -> HashSet<u64> {
    matches.iter().filter_map(|m| {
        if m.bi_dir != 0 { Some(m.bi_dir) } else { None }
    }).collect()
}

fn build_parent_map(fs: &Fs, mut needed: HashSet<u64>) -> Result<HashMap<u64, (u64, u64)>> {
    let mut parent_map = HashMap::new();

    while !needed.is_empty() {
        eprintln!("resolving {} parent inums...", needed.len());

        let trans = BtreeTrans::new(fs);
        let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;

        let mut iter = BtreeIter::new(
            &trans,
            bcachefs::btree_id::BTREE_ID_inodes,
            bch_bindgen::POS_MIN,
            flags,
        );

        let mut found_this_pass = HashMap::new();

        while let Some(k) = iter.peek_and_restart()? {
            let inum = k.k.p.inode;

            if !needed.contains(&inum) {
                iter.advance();
                continue;
            }

            if k.k.type_ != bcachefs::bch_bkey_type::KEY_TYPE_inode_v3 as u8 {
                iter.advance();
                continue;
            }

            let mut unpacked: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
            let bkey_s_c = c::bkey_s_c { k: k.k, v: k.v };

            let ret = unsafe { c::bch2_inode_unpack(bkey_s_c, &mut unpacked) };
            if ret == 0 && unpacked.bi_dir != 0 {
                found_this_pass.insert(inum, (unpacked.bi_dir, unpacked.bi_dir_offset));
            }

            iter.advance();
        }

        parent_map.extend(found_this_pass.iter());

        // Find next level of parents
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
    matches: &[InodeOpts],
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

fn build_dirent_map(fs: &Fs, needed: &HashSet<(u64, u64)>) -> Result<HashMap<(u64, u64), String>> {
    let trans = BtreeTrans::new(fs);
    let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;

    let mut iter = BtreeIter::new(
        &trans,
        bcachefs::btree_id::BTREE_ID_dirents,
        bch_bindgen::POS_MIN,
        flags,
    );

    let mut dirent_map = HashMap::new();

    while let Some(k) = iter.peek_and_restart()? {
        let key = (k.k.p.inode, k.k.p.offset);

        if needed.contains(&key) {
            if k.k.type_ == bcachefs::bch_bkey_type::KEY_TYPE_dirent as u8 {
                if let Some(name) = get_dirent_name(k.v, k.k) {
                    dirent_map.insert(key, name);
                }
            }
        }

        iter.advance();
    }

    Ok(dirent_map)
}

fn resolve_path(
    m: &InodeOpts,
    parent_map: &HashMap<u64, (u64, u64)>,
    dirent_map: &HashMap<(u64, u64), String>,
) -> String {
    let mut parts = Vec::new();
    let mut seen = HashSet::new();

    // First, get own name
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

/// Find inodes with non-default bcachefs options
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

    /// Mount path or device(s). If a directory is given, devices are looked up from mountinfo.
    #[arg(required(true))]
    paths: Vec<PathBuf>,
}

fn cmd_inode_opts_inner(opt: &Cli) -> Result<()> {
    // Resolve paths to devices - if a directory, look up from mountinfo
    let mut devices = Vec::new();
    for path in &opt.paths {
        devices.extend(resolve_devices(path)?);
    }

    if !opt.quiet {
        eprintln!("devices: {:?}", devices);
    }

    let mut fs_opts = bcachefs::bch_opts::default();

    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, read_only, 1);
    opt_set!(fs_opts, norecovery, 1);
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(
        fs_opts,
        errors,
        bcachefs::bch_error_actions::BCH_ON_ERROR_continue as u8
    );

    if opt.verbose > 0 {
        opt_set!(fs_opts, verbose, 1);
    }

    let fs = Fs::open(&devices, fs_opts)?;

    let matches = collect_inode_opts(&fs, opt.dirs, opt.verbose > 0)?;

    if matches.is_empty() {
        if !opt.quiet {
            eprintln!("no inodes with non-default options found");
        }
        return Ok(());
    }

    if opt.resolve_paths {
        // Build parent map
        let initial_parents: HashMap<u64, (u64, u64)> = matches
            .iter()
            .filter(|m| m.bi_dir != 0)
            .map(|m| (m.inum, (m.bi_dir, m.bi_dir_offset)))
            .collect();

        let needed = collect_needed_parents(&matches);
        let mut parent_map = build_parent_map(&fs, needed)?;
        parent_map.extend(initial_parents);

        if !opt.quiet {
            eprintln!("parent_map entries: {}", parent_map.len());
        }

        // Collect needed dirents
        let needed_dirents = collect_needed_dirents(&matches, &parent_map);
        if !opt.quiet {
            eprintln!("need {} dirent lookups", needed_dirents.len());
        }

        // Build dirent map
        let dirent_map = build_dirent_map(&fs, &needed_dirents)?;
        if !opt.quiet {
            eprintln!("resolved {} dirents", dirent_map.len());
        }

        // Output with paths
        for m in &matches {
            let path = resolve_path(m, &parent_map, &dirent_map);
            let opts_str: Vec<String> = m.opts.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            println!("{}\t{}\t{}", m.inum, path, opts_str.join(" "));
        }
    } else {
        // Output without paths
        for m in &matches {
            let opts_str: Vec<String> = m.opts.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            println!("{}\t{}", m.inum, opts_str.join(" "));
        }
    }

    Ok(())
}

pub fn inode_opts_device(argv: Vec<String>) -> Result<()> {
    let opt = Cli::parse_from(argv);
    logging::setup(opt.verbose, opt.colorize);
    cmd_inode_opts_inner(&opt)
}
