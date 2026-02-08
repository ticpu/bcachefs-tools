use std::{collections::HashMap, env, ffi::CStr, mem, os::fd::{AsRawFd, OwnedFd}, path::{Path, PathBuf}};
use chrono::{Local, TimeZone};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    BCH_SUBVOL_SNAPSHOT_RO, bch_ioctl_snapshot_node, bch_ioctl_subvol_dirent,
    bch_ioctl_subvol_readdir,
};
use clap::{Parser, Subcommand, ValueEnum};

use crate::wrappers::handle::BcachefsHandle;

// bcachefs file ioctl numbers
const BCH_IOCTL_SUBVOLUME_LIST: u32 = 31;
const BCH_IOCTL_SUBVOLUME_TO_PATH: u32 = 32;
const BCH_IOCTL_SNAPSHOT_TREE_USAGE: u32 = 33;

#[derive(Clone, Debug, ValueEnum)]
enum SortBy {
    Name,
    Size,
    Time,
}

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

/// Subvolumes-related commands
#[derive(Subcommand, Debug)]
enum Subcommands {
    #[command(visible_aliases = ["new"])]
    Create {
        /// Paths
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    #[command(visible_aliases = ["del"])]
    Delete {
        /// Path
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    #[command(allow_missing_positional = true, visible_aliases = ["snap"])]
    Snapshot {
        /// Make snapshot read only
        #[arg(long, short)]
        read_only: bool,
        source:    Option<PathBuf>,
        dest:      PathBuf,
    },

    #[command(visible_aliases = ["ls"])]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Show subvolume tree structure (implies -R)
        #[arg(long, short)]
        tree: bool,

        /// List subvolumes recursively
        #[arg(long, short = 'R')]
        recursive: bool,

        /// Include snapshot subvolumes
        #[arg(long, short)]
        snapshots: bool,

        /// Only show read-only subvolumes
        #[arg(long)]
        readonly: bool,

        /// Sort order
        #[arg(long, value_enum)]
        sort: Option<SortBy>,

        /// Filesystem (device, mountpoint, or UUID)
        target: PathBuf,
    },

    /// List snapshots and their disk usage
    #[command(visible_aliases = ["ls-snap", "list-snap"])]
    ListSnapshots {
        /// Show flat list instead of tree
        #[arg(long, short)]
        flat: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only show read-only snapshots (flat view only)
        #[arg(long)]
        readonly: bool,

        /// Sort order (flat view only)
        #[arg(long, value_enum)]
        sort: Option<SortBy>,

        /// Filesystem (device, mountpoint, or UUID)
        target: PathBuf,
    },
}

struct SubvolEntry {
    subvolid: u32,
    flags: u32,
    snapshot_parent: u32,
    otime_sec: i64,
    otime_nsec: u32,
    path: String,
}

const fn bch_ioc_rw<T>(nr: u32) -> libc::c_ulong {
    ((3u32 << 30) | ((mem::size_of::<T>() as u32) << 16) | (0xbcu32 << 8) | nr) as libc::c_ulong
}

fn bcachefs_ioctl<T>(fd: &OwnedFd, nr: u32, arg: &mut T) -> std::io::Result<()> {
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), bch_ioc_rw::<T>(nr), arg as *mut T) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Trait for ioctls returning a header + variable-length node array.
trait FlexArrayIoctl: Copy {
    type Node: Copy;
    const NR: u32;
    fn set_capacity(&mut self, n: u32);
    fn nr(&self) -> u32;
    fn total(&self) -> u32;
}

/// Call a bcachefs ioctl that returns a flex array, with ERANGE retry.
fn bcachefs_flex_ioctl<H: FlexArrayIoctl>(
    fd: &OwnedFd,
    mut arg: H,
) -> Result<(H, Vec<H::Node>)> {
    let hdr_size = mem::size_of::<H>();
    let node_size = mem::size_of::<H::Node>();
    let request = bch_ioc_rw::<H>(H::NR);
    let mut capacity = 256u32;

    loop {
        arg.set_capacity(capacity);
        let buf_size = hdr_size + node_size * capacity as usize;
        let mut buf = vec![0u8; buf_size];

        unsafe {
            std::ptr::copy_nonoverlapping(
                &arg as *const H as *const u8, buf.as_mut_ptr(), hdr_size);
        }

        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), request, buf.as_mut_ptr()) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ERANGE) {
                let hdr = unsafe { &*(buf.as_ptr() as *const H) };
                capacity = hdr.total();
                continue;
            }
            return Err(err.into());
        }

        let hdr = unsafe { *(buf.as_ptr() as *const H) };
        let nr = hdr.nr() as usize;
        let nodes = (0..nr).map(|i| unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr().add(hdr_size + i * node_size) as *const H::Node)
        }).collect();

        return Ok((hdr, nodes));
    }
}

#[repr(C)]
struct BchIoctlSubvolToPath {
    subvolid:   u32,
    buf_size:   u32,
    buf:        u64,
}

// Subvolume flags — matches LE32_BITMASK definitions in snapshots/format.h
const BCH_SUBVOLUME_RO:       u32 = 1 << 0;
const BCH_SUBVOLUME_UNLINKED: u32 = 1 << 2;

fn flags_str(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & BCH_SUBVOLUME_RO != 0       { parts.push("ro"); }
    if flags & BCH_SUBVOLUME_UNLINKED != 0 { parts.push("unlinked"); }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(",")
    }
}

fn format_time(sec: i64, _nsec: u32) -> String {
    if sec == 0 {
        return "-".to_string();
    }
    match Local.timestamp_opt(sec, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => sec.to_string(),
    }
}

fn human_readable_size(sectors: u64) -> String {
    let bytes = sectors * 512;
    const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P"];
    if bytes == 0 {
        return "0B".to_string();
    }
    let mut val = bytes as f64;
    for &unit in UNITS {
        if val < 1024.0 {
            return if val == val.trunc() {
                format!("{:.0}{}", val, unit)
            } else {
                format!("{:.1}{}", val, unit)
            };
        }
        val /= 1024.0;
    }
    format!("{:.1}E", val)
}

fn open_dir(path: &Path) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(f.into())
}

// Readdir ioctl: one call + parse → Vec<SubvolEntry>
fn subvol_readdir(fd: &OwnedFd, pos: &mut u32) -> Result<Vec<SubvolEntry>> {
    let buf_size = 64 * 1024u32;
    let mut buf = vec![0u8; buf_size as usize];
    let mut arg = bch_ioctl_subvol_readdir {
        pos: *pos,
        buf_size,
        buf: buf.as_mut_ptr() as u64,
        used: 0,
        pad: 0,
    };

    bcachefs_ioctl(fd, BCH_IOCTL_SUBVOLUME_LIST, &mut arg)
        .context("BCH_IOCTL_SUBVOLUME_LIST")?;
    *pos = arg.pos;

    let mut entries = Vec::new();
    let mut offset = 0usize;
    let used = arg.used as usize;
    let hdr_size = mem::size_of::<bch_ioctl_subvol_dirent>();

    while offset + hdr_size <= used {
        let dirent = unsafe { &*(buf.as_ptr().add(offset) as *const bch_ioctl_subvol_dirent) };
        let reclen = dirent.reclen as usize;
        if reclen < hdr_size || offset + reclen > used { break; }

        let path_bytes = &buf[offset + hdr_size..offset + reclen];
        let path = CStr::from_bytes_until_nul(path_bytes)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();

        entries.push(SubvolEntry {
            subvolid: dirent.subvolid,
            flags: dirent.flags,
            snapshot_parent: dirent.snapshot_parent,
            otime_sec: dirent.otime_sec as i64,
            otime_nsec: dirent.otime_nsec,
            path,
        });
        offset += reclen;
    }

    Ok(entries)
}

fn list_children(fd: &OwnedFd) -> Result<Vec<SubvolEntry>> {
    let mut all = Vec::new();
    let mut pos = 0u32;
    loop {
        let entries = subvol_readdir(fd, &mut pos)?;
        if entries.is_empty() { break; }
        all.extend(entries);
    }
    Ok(all)
}

// Subvol-to-path ioctl wrapper
fn subvol_to_path(fd: &OwnedFd, subvolid: u32) -> Result<String> {
    let mut buf = vec![0u8; 4096];
    let mut arg = BchIoctlSubvolToPath {
        subvolid,
        buf_size: buf.len() as u32,
        buf: buf.as_mut_ptr() as u64,
    };

    bcachefs_ioctl(fd, BCH_IOCTL_SUBVOLUME_TO_PATH, &mut arg)
        .context("BCH_IOCTL_SUBVOLUME_TO_PATH")?;

    let path = CStr::from_bytes_until_nul(&buf)
        .context("invalid path from ioctl")?
        .to_string_lossy()
        .into_owned();
    Ok(format!("/{}", path))
}

fn resolve_subvol_path(fd: &OwnedFd, subvolid: u32) -> Option<String> {
    subvol_to_path(fd, subvolid).ok()
}

fn snapshot_parent_str(fd: &OwnedFd, parent: u32) -> String {
    if parent == 0 {
        return String::new();
    }
    resolve_subvol_path(fd, parent)
        .unwrap_or_else(|| parent.to_string())
}

// Snapshot tree ioctl
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BchIoctlSnapshotTreeQuery {
    tree_id:        u32,
    master_subvol:  u32,
    root_snapshot:  u32,
    nr:             u32,
    total:          u32,
    pad:            u32,
}

impl FlexArrayIoctl for BchIoctlSnapshotTreeQuery {
    type Node = bch_ioctl_snapshot_node;
    const NR: u32 = BCH_IOCTL_SNAPSHOT_TREE_USAGE;
    fn set_capacity(&mut self, n: u32) { self.nr = n; }
    fn nr(&self) -> u32 { self.nr }
    fn total(&self) -> u32 { self.total }
}

type SnapshotNode = bch_ioctl_snapshot_node;

struct SnapshotTreeResult {
    master_subvol:  u32,
    root_snapshot:  u32,
    nodes:          Vec<SnapshotNode>,
}

fn query_snapshot_tree(fd: &OwnedFd, tree_id: u32) -> Result<SnapshotTreeResult> {
    let (hdr, nodes) = bcachefs_flex_ioctl(fd, BchIoctlSnapshotTreeQuery {
        tree_id,
        ..Default::default()
    })?;

    Ok(SnapshotTreeResult {
        master_subvol: hdr.master_subvol,
        root_snapshot: hdr.root_snapshot,
        nodes,
    })
}

/// Query snapshot tree and compute cumulative sectors per subvolume.
fn subvol_sizes(fd: &OwnedFd) -> Option<HashMap<u32, u64>> {
    let tree = query_snapshot_tree(fd, 0).ok()?;
    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter()
        .map(|n| (n.id, n)).collect();

    let mut sizes = HashMap::new();
    for n in &tree.nodes {
        if n.subvol == 0 { continue; }
        let mut cumulative = 0u64;
        let mut cur = n.id;
        loop {
            if let Some(node) = by_id.get(&cur) {
                cumulative += node.sectors;
                if node.parent == 0 { break; }
                cur = node.parent;
            } else {
                break;
            }
        }
        sizes.insert(n.subvol, cumulative);
    }
    Some(sizes)
}

fn collect_entries(dir: &Path, prefix: &str, recursive: bool) -> Result<Vec<(String, SubvolEntry)>> {
    let fd = open_dir(dir)?;
    let children = list_children(&fd)?;
    let mut out = Vec::new();

    for e in children {
        let full_path = if prefix.is_empty() {
            e.path.clone()
        } else {
            format!("{}/{}", prefix, e.path)
        };

        let child_dir = dir.join(&e.path);
        out.push((full_path.clone(), e));

        if recursive {
            if let Ok(sub) = collect_entries(&child_dir, &full_path, true) {
                out.extend(sub);
            }
        }
    }

    Ok(out)
}

fn print_flat(dir: &Path, recursive: bool, show_snapshots: bool,
              readonly: bool, sort: Option<SortBy>) -> Result<()> {
    let fd = open_dir(dir)?;
    let sizes = subvol_sizes(&fd);
    let mut entries = collect_entries(dir, "", recursive)?;

    entries.retain(|(_, e)| {
        if !show_snapshots && e.snapshot_parent != 0 { return false; }
        if readonly && (e.flags & BCH_SUBVOLUME_RO) == 0 { return false; }
        true
    });

    if let Some(ref sort) = sort {
        match sort {
            SortBy::Name => entries.sort_by(|a, b| a.0.cmp(&b.0)),
            SortBy::Size => entries.sort_by(|a, b| {
                let sa = sizes.as_ref().and_then(|s| s.get(&a.1.subvolid)).copied().unwrap_or(0);
                let sb = sizes.as_ref().and_then(|s| s.get(&b.1.subvolid)).copied().unwrap_or(0);
                sb.cmp(&sa)
            }),
            SortBy::Time => entries.sort_by(|a, b| b.1.otime_sec.cmp(&a.1.otime_sec)),
        }
    }

    if show_snapshots {
        println!("{:<24} {:<8} {:<16} {:<12} {:<12} {}",
            "Path", "ID", "Created", "Flags", "Size", "Snapshot");
    } else {
        println!("{:<24} {:<8} {:<16} {:<12} {}",
            "Path", "ID", "Created", "Flags", "Size");
    }

    for (path, e) in &entries {
        let f = flags_str(e.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        let size = sizes.as_ref()
            .and_then(|s| s.get(&e.subvolid))
            .map(|&s| human_readable_size(s))
            .unwrap_or_default();

        if show_snapshots {
            let snap = if e.snapshot_parent != 0 {
                snapshot_parent_str(&fd, e.snapshot_parent)
            } else {
                String::new()
            };
            println!("{:<24} {:<8} {:<16} {:<12} {:<12} {}",
                path, e.subvolid,
                format_time(e.otime_sec, e.otime_nsec),
                flags_display, size, snap);
        } else {
            println!("{:<24} {:<8} {:<16} {:<12} {}",
                path, e.subvolid,
                format_time(e.otime_sec, e.otime_nsec),
                flags_display, size);
        }
    }

    Ok(())
}

fn print_tree_recursive(dir: &Path, prefix: &str, show_snapshots: bool,
                        sizes: &Option<HashMap<u32, u64>>) -> Result<()> {
    let fd = open_dir(dir)?;
    let entries = list_children(&fd)?;

    let entries: Vec<_> = entries.into_iter()
        .filter(|e| show_snapshots || e.snapshot_parent == 0)
        .collect();

    for (i, e) in entries.iter().enumerate() {
        let is_last = i == entries.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_indent = if is_last { "    " } else { "│   " };

        let mut annotations = Vec::new();
        if e.snapshot_parent != 0 {
            let parent = resolve_subvol_path(&fd, e.snapshot_parent)
                .unwrap_or_else(|| e.snapshot_parent.to_string());
            annotations.push(format!("snap of {}", parent));
        }
        let f = flags_str(e.flags);
        if !f.is_empty() { annotations.push(f); }
        if let Some(&sectors) = sizes.as_ref().and_then(|s| s.get(&e.subvolid)) {
            annotations.push(human_readable_size(sectors));
        }
        let otime = format_time(e.otime_sec, e.otime_nsec);
        if otime != "-" { annotations.push(otime); }
        let suffix = if annotations.is_empty() {
            String::new()
        } else {
            format!(" [{}]", annotations.join(", "))
        };

        println!("{}{}{}{}", prefix, connector, e.path, suffix);

        let next_prefix = format!("{}{}", prefix, child_indent);
        print_tree_recursive(&dir.join(&e.path), &next_prefix, show_snapshots, sizes)?;
    }

    Ok(())
}

fn print_json(dir: &Path, recursive: bool, show_snapshots: bool, readonly: bool) -> Result<()> {
    let top_fd = open_dir(dir)?;
    let sizes = subvol_sizes(&top_fd);

    fn collect(dir: &Path, recursive: bool, show_snapshots: bool, readonly: bool,
               sizes: &Option<HashMap<u32, u64>>) -> Result<Vec<serde_json::Value>> {
        let fd = open_dir(dir)?;
        let entries = list_children(&fd)?;
        let mut result = Vec::new();
        for e in &entries {
            if !show_snapshots && e.snapshot_parent != 0 { continue; }
            if readonly && (e.flags & BCH_SUBVOLUME_RO) == 0 { continue; }

            let mut obj = serde_json::Map::new();
            obj.insert("subvolid".into(), serde_json::Value::Number(e.subvolid.into()));
            obj.insert("path".into(), serde_json::Value::String(e.path.clone()));
            if e.otime_sec != 0 {
                obj.insert("otime".into(), serde_json::Value::String(
                    format_time(e.otime_sec, e.otime_nsec)));
                obj.insert("otime_unix".into(), serde_json::Value::Number(e.otime_sec.into()));
            }
            if e.snapshot_parent != 0 {
                let parent_path = resolve_subvol_path(&fd, e.snapshot_parent)
                    .unwrap_or_else(|| e.snapshot_parent.to_string());
                obj.insert("snapshot_parent".into(), serde_json::Value::String(parent_path));
            }
            let f = flags_str(e.flags);
            if !f.is_empty() {
                obj.insert("flags".into(), serde_json::Value::String(f));
            }
            if let Some(&sectors) = sizes.as_ref().and_then(|s| s.get(&e.subvolid)) {
                obj.insert("size".into(), serde_json::Value::String(human_readable_size(sectors)));
                obj.insert("sectors".into(), serde_json::Value::Number(sectors.into()));
            }
            if recursive {
                let children = collect(&dir.join(&e.path), true, show_snapshots, readonly, sizes)?;
                if !children.is_empty() {
                    obj.insert("children".into(), serde_json::Value::Array(children));
                }
            }
            result.push(serde_json::Value::Object(obj));
        }
        Ok(result)
    }

    let tree = collect(dir, recursive, show_snapshots, readonly, &sizes)?;
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(())
}

fn print_snapshot_tree(dir: &Path) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = match query_snapshot_tree(&fd, 0) {
        Ok(t) => t,
        Err(e) => {
            if let Some(inner) = e.downcast_ref::<std::io::Error>() {
                if inner.raw_os_error() == Some(libc::ENOTTY) {
                    eprintln!("snapshot tree ioctl not supported by this kernel");
                    return Ok(());
                }
            }
            return Err(e);
        }
    };

    if tree.nodes.is_empty() {
        println!("(no snapshot nodes)");
        return Ok(());
    }

    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter().map(|n| (n.id, n)).collect();

    let mut subvol_names: HashMap<u32, String> = HashMap::new();
    for n in &tree.nodes {
        if n.subvol != 0 {
            let name = resolve_subvol_path(&fd, n.subvol)
                .unwrap_or_else(|| format!("subvol {}", n.subvol));
            subvol_names.insert(n.id, name);
        }
    }

    fn node_children(node: &SnapshotNode, by_id: &HashMap<u32, &SnapshotNode>) -> Vec<u32> {
        node.children.iter()
            .copied()
            .filter(|&c| c != 0 && by_id.contains_key(&c))
            .collect()
    }

    fn node_label(id: u32, node: &SnapshotNode, subvol_names: &HashMap<u32, String>) -> String {
        let name = subvol_names.get(&id)
            .cloned()
            .unwrap_or_else(|| "(shared)".to_string());
        let mut label = format!("{} [{}]", name, human_readable_size(node.sectors));
        let f = flags_str(node.flags);
        if !f.is_empty() {
            label.push_str(&format!(" ({})", f));
        }
        label
    }

    fn print_subtree(
        id: u32,
        by_id: &HashMap<u32, &SnapshotNode>,
        subvol_names: &HashMap<u32, String>,
        prefix: &str,
        is_last: bool,
    ) {
        let Some(node) = by_id.get(&id) else { return };

        let connector = if is_last { "└── " } else { "├── " };
        println!("{}{}{}", prefix, connector, node_label(id, node, subvol_names));

        let child_prefix = if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };

        let children = node_children(node, by_id);
        for (i, &child_id) in children.iter().enumerate() {
            print_subtree(child_id, by_id, subvol_names, &child_prefix, i == children.len() - 1);
        }
    }

    // Print root, then recurse into children with tree connectors
    if let Some(root) = by_id.get(&tree.root_snapshot) {
        println!("{}", node_label(tree.root_snapshot, root, &subvol_names));

        let children = node_children(root, &by_id);
        for (i, &child_id) in children.iter().enumerate() {
            print_subtree(child_id, &by_id, &subvol_names, "", i == children.len() - 1);
        }
    }

    Ok(())
}

fn print_snapshot_flat(dir: &Path, readonly: bool, sort: Option<SortBy>) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = query_snapshot_tree(&fd, 0)?;

    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter()
        .map(|n| (n.id, n)).collect();

    struct SnapFlatEntry {
        path: String,
        subvol: u32,
        own_sectors: u64,
        cumulative: u64,
        flags: u32,
    }

    let mut entries: Vec<SnapFlatEntry> = Vec::new();

    for n in &tree.nodes {
        if n.subvol == 0 { continue; }
        if readonly && (n.flags & BCH_SUBVOLUME_RO) == 0 { continue; }

        let path = resolve_subvol_path(&fd, n.subvol)
            .unwrap_or_else(|| format!("subvol {}", n.subvol));

        let mut cumulative = 0u64;
        let mut cur = n.id;
        loop {
            if let Some(node) = by_id.get(&cur) {
                cumulative += node.sectors;
                if node.parent == 0 { break; }
                cur = node.parent;
            } else {
                break;
            }
        }

        entries.push(SnapFlatEntry {
            path,
            subvol: n.subvol,
            own_sectors: n.sectors,
            cumulative,
            flags: n.flags,
        });
    }

    if let Some(ref sort) = sort {
        match sort {
            SortBy::Name => entries.sort_by(|a, b| a.path.cmp(&b.path)),
            SortBy::Size => entries.sort_by(|a, b| b.cumulative.cmp(&a.cumulative)),
            SortBy::Time => {} // no time data from snapshot tree ioctl
        }
    }

    println!("{:<24} {:<8} {:<12} {:<12} {}",
        "Path", "ID", "Own", "Total", "Flags");

    for e in &entries {
        let f = flags_str(e.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        println!("{:<24} {:<8} {:<12} {:<12} {}",
            e.path, e.subvol,
            human_readable_size(e.own_sectors),
            human_readable_size(e.cumulative),
            flags_display);
    }

    Ok(())
}

fn print_snapshot_json(dir: &Path) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = query_snapshot_tree(&fd, 0)?;

    let mut nodes_json = Vec::new();
    for n in &tree.nodes {
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), n.id.into());
        obj.insert("parent".into(), n.parent.into());
        obj.insert("children".into(), serde_json::json!(
            n.children.iter().filter(|&&c| c != 0).collect::<Vec<_>>()));
        obj.insert("subvol".into(), n.subvol.into());
        obj.insert("sectors".into(), n.sectors.into());
        obj.insert("size".into(), human_readable_size(n.sectors).into());
        let f = flags_str(n.flags);
        if !f.is_empty() {
            obj.insert("flags".into(), f.into());
        }
        if n.subvol != 0 {
            if let Some(path) = resolve_subvol_path(&fd, n.subvol) {
                obj.insert("path".into(), path.into());
            }
        }
        nodes_json.push(serde_json::Value::Object(obj));
    }

    let mut query_root = serde_json::Map::new();
    query_root.insert("subvol".into(), tree.master_subvol.into());
    query_root.insert("snapshot".into(), tree.root_snapshot.into());
    if let Some(path) = resolve_subvol_path(&fd, tree.master_subvol) {
        query_root.insert("path".into(), path.into());
    }

    let query_root_json = serde_json::to_string(&serde_json::Value::Object(query_root))?;
    let nodes_json_str = serde_json::to_string_pretty(&serde_json::Value::Array(nodes_json))?;
    // Indent nodes array to sit inside the outer object
    let nodes_indented = nodes_json_str.lines()
        .enumerate()
        .map(|(i, l)| if i == 0 { l.to_string() } else { format!("  {}", l) })
        .collect::<Vec<_>>()
        .join("\n");

    println!("{{\n  \"query_root\": {},\n  \"nodes\": {}\n}}", query_root_json, nodes_indented);
    Ok(())
}

pub fn subvolume(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    match cli.subcommands {
        Subcommands::Create { targets } => {
            for target in targets {
                let target = if target.is_absolute() {
                    target
                } else {
                    env::current_dir()
                        .map(|p| p.join(target))
                        .context("unable to get current directory")?
                };

                if let Some(dirname) = target.parent() {
                    let fs =
                        BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
                    fs.create_subvolume(target)
                        .context("Failed to create the subvolume")?;
                }
            }
        }
        Subcommands::Delete { targets } => {
            for target in targets {
                let target = target
                    .canonicalize()
                    .context("subvolume path does not exist or can not be canonicalized")?;

                if let Some(dirname) = target.parent() {
                    let fs =
                        BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
                    fs.delete_subvolume(target)
                        .context("Failed to delete the subvolume")?;
                }
            }
        }
        Subcommands::Snapshot {
            read_only,
            source,
            dest,
        } => {
            if let Some(dirname) = dest.parent() {
                let dot = PathBuf::from(".");
                let dir = if dirname.as_os_str().is_empty() {
                    &dot
                } else {
                    dirname
                };
                let fs = BcachefsHandle::open(dir).context("Failed to open the filesystem")?;

                fs.snapshot_subvolume(
                    if read_only {
                        BCH_SUBVOL_SNAPSHOT_RO
                    } else {
                        0x0
                    },
                    source,
                    dest,
                )
                .context("Failed to snapshot the subvolume")?;
            }
        }
        Subcommands::List { json, tree, recursive, snapshots, readonly, sort, target } => {
            let recursive = recursive || tree;
            if json {
                print_json(&target, recursive, snapshots, readonly)?;
            } else if tree {
                let fd = open_dir(&target)?;
                let sizes = subvol_sizes(&fd);
                println!("{}", target.display());
                print_tree_recursive(&target, "", snapshots, &sizes)?;
            } else {
                print_flat(&target, recursive, snapshots, readonly, sort)?;
            }
        }
        Subcommands::ListSnapshots { flat, json, readonly, sort, target } => {
            if json {
                print_snapshot_json(&target)?;
            } else if flat {
                print_snapshot_flat(&target, readonly, sort)?;
            } else {
                print_snapshot_tree(&target)?;
            }
        }
    }

    Ok(())
}
