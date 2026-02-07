use std::{collections::HashMap, env, ffi::CStr, mem, os::fd::OwnedFd, path::{Path, PathBuf}};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    BCH_SUBVOL_SNAPSHOT_RO, bch_ioctl_snapshot_node, bch_ioctl_subvol_dirent,
    bch_ioctl_subvol_readdir,
};
use clap::{Parser, Subcommand};
use rustix::ioctl::{self, ReadWriteOpcode, Updater};

use crate::wrappers::handle::BcachefsHandle;

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

        /// Show snapshot tree with disk usage
        #[arg(long)]
        snapshot_tree: bool,

        /// List subvolumes recursively
        #[arg(long, short = 'R')]
        recursive: bool,

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

type SubvolReaddirOpcode = ReadWriteOpcode<0xbc, 31, bch_ioctl_subvol_readdir>;

#[repr(C)]
struct BchIoctlSubvolToPath {
    subvolid:   u32,
    buf_size:   u32,
    buf:        u64,
}

type SubvolToPathOpcode = ReadWriteOpcode<0xbc, 32, BchIoctlSubvolToPath>;

fn parse_readdir_buf(buf: &[u8], used: u32) -> Vec<SubvolEntry> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    let used = used as usize;
    let hdr_size = mem::size_of::<bch_ioctl_subvol_dirent>();

    while offset + hdr_size <= used {
        let dirent = unsafe { &*(buf.as_ptr().add(offset) as *const bch_ioctl_subvol_dirent) };
        let reclen = dirent.reclen as usize;

        if reclen < hdr_size || offset + reclen > used {
            break;
        }

        // path is NUL-terminated; alignment padding after the NUL is zeroed
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

    entries
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
    unsafe {
        let mut tm: libc::tm = mem::zeroed();
        let t = sec as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return sec.to_string();
        }
        let mut buf = [0u8; 32];
        let fmt = b"%Y-%m-%d %H:%M\0";
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr() as *const libc::c_char,
            &tm,
        );
        if n == 0 {
            return sec.to_string();
        }
        String::from_utf8_lossy(&buf[..n]).into_owned()
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

// Snapshot tree ioctl types
#[repr(C)]
struct BchIoctlSnapshotTreeQuery {
    tree_id:        u32,
    master_subvol:  u32,
    root_snapshot:  u32,
    nr:             u32,
    total:          u32,
    pad:            u32,
    // nodes[] follows
}

type SnapshotTreeOpcode = ReadWriteOpcode<0xbc, 33, BchIoctlSnapshotTreeQuery>;

struct SnapshotTreeResult {
    #[allow(dead_code)]
    master_subvol:  u32,
    root_snapshot:  u32,
    nodes:          Vec<SnapshotNode>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct SnapshotNode {
    id:         u32,
    parent:     u32,
    children:   [u32; 2],
    subvol:     u32,
    flags:      u32,
    sectors:    u64,
}

fn query_snapshot_tree(fd: &OwnedFd, tree_id: u32) -> Result<SnapshotTreeResult> {
    // First call: probe total count
    let hdr_size = mem::size_of::<BchIoctlSnapshotTreeQuery>();
    let node_size = mem::size_of::<bch_ioctl_snapshot_node>();

    let mut capacity = 256u32;
    loop {
        let buf_size = hdr_size + node_size * capacity as usize;
        let mut buf = vec![0u8; buf_size];

        let hdr = unsafe { &mut *(buf.as_mut_ptr() as *mut BchIoctlSnapshotTreeQuery) };
        hdr.tree_id = tree_id;
        hdr.nr = capacity;

        let ret = unsafe {
            ioctl::ioctl(fd, Updater::<SnapshotTreeOpcode, _>::new(hdr))
        };

        match ret {
            Ok(_) => {}
            Err(rustix::io::Errno::RANGE) => {
                let hdr = unsafe { &*(buf.as_ptr() as *const BchIoctlSnapshotTreeQuery) };
                capacity = hdr.total;
                continue;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("BCH_IOCTL_SNAPSHOT_TREE: {}", e));
            }
        }

        let hdr = unsafe { &*(buf.as_ptr() as *const BchIoctlSnapshotTreeQuery) };
        let nr = hdr.nr;

        let mut nodes = Vec::with_capacity(nr as usize);
        for i in 0..nr as usize {
            let node_ptr = unsafe {
                buf.as_ptr().add(hdr_size + i * node_size) as *const bch_ioctl_snapshot_node
            };
            let n = unsafe { &*node_ptr };
            nodes.push(SnapshotNode {
                id:       n.id,
                parent:   n.parent,
                children: n.children,
                subvol:   n.subvol,
                flags:    n.flags,
                sectors:  n.sectors,
            });
        }
        return Ok(SnapshotTreeResult {
            master_subvol: hdr.master_subvol,
            root_snapshot: hdr.root_snapshot,
            nodes,
        });
    }
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

fn list_children(fd: &OwnedFd) -> Result<Vec<SubvolEntry>> {
    let buf_size = 64 * 1024u32;
    let mut buf = vec![0u8; buf_size as usize];
    let mut all_entries = Vec::new();
    let mut pos = 0u32;

    loop {
        let mut arg = bch_ioctl_subvol_readdir {
            pos,
            buf_size,
            buf: buf.as_mut_ptr() as u64,
            used: 0,
            pad: 0,
        };

        unsafe {
            ioctl::ioctl(&fd, Updater::<SubvolReaddirOpcode, _>::new(&mut arg))
        }.context("BCH_IOCTL_SUBVOLUME_LIST")?;

        if arg.used == 0 {
            break;
        }

        all_entries.extend(parse_readdir_buf(&buf, arg.used));
        pos = arg.pos;
    }

    Ok(all_entries)
}

fn resolve_subvol_path(fd: &OwnedFd, subvolid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let mut arg = BchIoctlSubvolToPath {
        subvolid,
        buf_size: buf.len() as u32,
        buf: buf.as_mut_ptr() as u64,
    };

    let ret = unsafe {
        ioctl::ioctl(fd, Updater::<SubvolToPathOpcode, _>::new(&mut arg))
    };

    if ret.is_err() {
        return None;
    }

    CStr::from_bytes_until_nul(&buf)
        .ok()
        .map(|c| {
            let s = c.to_string_lossy().into_owned();
            if s.is_empty() { "(root)".to_string() } else { s }
        })
}

fn snapshot_parent_str(fd: &OwnedFd, parent: u32) -> String {
    if parent == 0 {
        return String::new();
    }
    resolve_subvol_path(fd, parent)
        .unwrap_or_else(|| parent.to_string())
}

fn print_flat(dir: &Path, prefix: &str, recursive: bool) -> Result<()> {
    let fd = open_dir(dir)?;
    let entries = list_children(&fd)?;

    for e in &entries {
        let f = flags_str(e.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        let full_path = if prefix.is_empty() {
            e.path.clone()
        } else {
            format!("{}/{}", prefix, e.path)
        };
        let snap = if e.snapshot_parent != 0 {
            snapshot_parent_str(&fd, e.snapshot_parent)
        } else {
            String::new()
        };
        println!("{:<24} {:<8} {:<16} {:<12} {}",
            full_path,
            e.subvolid,
            format_time(e.otime_sec, e.otime_nsec),
            flags_display,
            snap);

        if recursive {
            print_flat(&dir.join(&e.path), &full_path, true)?;
        }
    }

    Ok(())
}

fn print_tree_recursive(dir: &Path, prefix: &str) -> Result<()> {
    let fd = open_dir(dir)?;
    let entries = list_children(&fd)?;

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
        let otime = format_time(e.otime_sec, e.otime_nsec);
        if otime != "-" { annotations.push(otime); }
        let suffix = if annotations.is_empty() {
            String::new()
        } else {
            format!(" [{}]", annotations.join(", "))
        };

        println!("{}{}{}{}", prefix, connector, e.path, suffix);

        let next_prefix = format!("{}{}", prefix, child_indent);
        print_tree_recursive(&dir.join(&e.path), &next_prefix)?;
    }

    Ok(())
}

fn print_json(dir: &Path, recursive: bool) -> Result<()> {
    fn collect(dir: &Path, recursive: bool) -> Result<Vec<serde_json::Value>> {
        let fd = open_dir(dir)?;
        let entries = list_children(&fd)?;
        let mut result = Vec::new();
        for e in &entries {
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
            if recursive {
                let children = collect(&dir.join(&e.path), true)?;
                if !children.is_empty() {
                    obj.insert("children".into(), serde_json::Value::Array(children));
                }
            }
            result.push(serde_json::Value::Object(obj));
        }
        Ok(result)
    }

    let tree = collect(dir, recursive)?;
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(())
}

fn print_snapshot_tree(dir: &Path) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = match query_snapshot_tree(&fd, 0) {
        Ok(t) => t,
        Err(e) => {
            // ENOTTY means kernel doesn't support this ioctl yet
            if let Some(inner) = e.downcast_ref::<rustix::io::Errno>() {
                if *inner == rustix::io::Errno::NOTTY {
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

    // Build lookup maps
    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter().map(|n| (n.id, n)).collect();

    // Resolve subvol names
    let mut subvol_names: HashMap<u32, String> = HashMap::new();
    for n in &tree.nodes {
        if n.subvol != 0 {
            let name = resolve_subvol_path(&fd, n.subvol)
                .unwrap_or_else(|| format!("subvol {}", n.subvol));
            subvol_names.insert(n.id, name);
        }
    }

    // Compute cumulative sectors (sum self + all ancestors)
    fn cumulative_sectors(id: u32, by_id: &HashMap<u32, &SnapshotNode>) -> u64 {
        let mut total = 0u64;
        let mut cur = id;
        while let Some(n) = by_id.get(&cur) {
            total += n.sectors;
            if n.parent == 0 { break; }
            cur = n.parent;
        }
        total
    }

    // BCH_SNAPSHOT_DELETED = bit 2
    const BCH_SNAPSHOT_DELETED: u32 = 1 << 2;

    // Print tree recursively
    fn print_node(
        id: u32,
        by_id: &HashMap<u32, &SnapshotNode>,
        subvol_names: &HashMap<u32, String>,
        prefix: &str,
        is_last: bool,
    ) {
        let Some(node) = by_id.get(&id) else { return };

        if node.flags & BCH_SNAPSHOT_DELETED != 0 {
            return;
        }

        let connector = if prefix.is_empty() { "" }
            else if is_last { "└── " } else { "├── " };

        let name = subvol_names.get(&id)
            .cloned()
            .unwrap_or_else(|| format!("snap {}", id));

        let size = human_readable_size(node.sectors);

        let mut annotations = Vec::new();
        annotations.push(size);

        let cum = cumulative_sectors(id, by_id);
        if cum != node.sectors {
            annotations.push(format!("total: {}", human_readable_size(cum)));
        }

        println!("{}{}{} [{}]",
            prefix, connector, name,
            annotations.join(", "));

        let child_prefix = if prefix.is_empty() {
            String::new()
        } else if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };

        let children: Vec<u32> = [node.children[0], node.children[1]]
            .iter()
            .copied()
            .filter(|&c| c != 0 && by_id.contains_key(&c))
            .filter(|c| by_id.get(c).map_or(false, |n| n.flags & BCH_SNAPSHOT_DELETED == 0))
            .collect();

        for (i, &child_id) in children.iter().enumerate() {
            let last = i == children.len() - 1;
            print_node(child_id, by_id, subvol_names, &child_prefix, last);
        }
    }

    print_node(tree.root_snapshot, &by_id, &subvol_names, "", true);

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
        Subcommands::List { json, tree, snapshot_tree, recursive, target } => {
            if snapshot_tree {
                print_snapshot_tree(&target)?;
            } else {
                let recursive = recursive || tree;
                if json {
                    print_json(&target, recursive)?;
                } else if tree {
                    println!("{}", target.display());
                    print_tree_recursive(&target, "")?;
                } else {
                    println!("{:<24} {:<8} {:<16} {:<12} {}",
                        "Path", "ID", "Created", "Flags", "Snapshot");
                    print_flat(&target, "", recursive)?;
                }
            }
        }
    }

    Ok(())
}
