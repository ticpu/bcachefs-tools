use anyhow::Result;
use bch_bindgen::bcachefs;
use bch_bindgen::btree::BtreeIter;
use bch_bindgen::btree::BtreeIterFlags;
use bch_bindgen::btree::BtreeTrans;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;
use std::io::{stdout, IsTerminal};
use std::path::PathBuf;

use crate::logging;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Add => write!(f, "A"),
            ChangeKind::Modify => write!(f, "M"),
            ChangeKind::Delete => write!(f, "D"),
        }
    }
}

#[derive(Debug)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: String,
}

/// Get dirent name from a bch_dirent - the name follows the fixed struct
fn get_dirent_name(v: &c::bch_val, k: &c::bkey) -> Option<String> {
    unsafe {
        let dirent = v as *const c::bch_val as *const c::bch_dirent;
        // Name starts after the bch_dirent struct in memory
        // bch_dirent has: bch_val (0) + d_inum union (8) + bitfield (1) + union for name
        // The flexible array d_name starts at offset after the fixed part
        let dirent_base_size = std::mem::size_of::<c::bch_dirent>();
        let val_bytes = (k.u64s as usize) * 8;
        let name_len = val_bytes.saturating_sub(dirent_base_size);

        if name_len > 0 && name_len < 512 {
            // d_name is in __bindgen_anon_2 union - get pointer to it
            let name_ptr = &(*dirent).__bindgen_anon_2 as *const _ as *const u8;
            let name_slice = std::slice::from_raw_parts(name_ptr, name_len);
            // Find null terminator
            let actual_len = name_slice.iter().position(|&b| b == 0).unwrap_or(name_len);
            if actual_len > 0 {
                return Some(String::from_utf8_lossy(&name_slice[..actual_len]).into_owned());
            }
        }
        None
    }
}

/// Look up the original dirent name at a given position from an older snapshot
fn lookup_dirent_name_at_pos(
    fs: &Fs,
    inode: u64,
    offset: u64,
) -> Option<String> {
    let trans = BtreeTrans::new(fs);
    let pos = bch_bindgen::spos(inode, offset, 0);
    let flags = BtreeIterFlags::ALL_SNAPSHOTS;

    let mut iter = BtreeIter::new(
        &trans,
        bcachefs::btree_id::BTREE_ID_dirents,
        pos,
        flags,
    );

    while let Some(k) = iter.peek_and_restart().ok()? {
        if k.k.p.inode != inode || k.k.p.offset != offset {
            break;
        }

        if k.k.type_ == bcachefs::bch_bkey_type::KEY_TYPE_dirent as u8 {
            return get_dirent_name(k.v, k.k);
        }

        iter.advance();
    }

    None
}

/// Find all changes made in snapshots between base_snapshot and child_snapshot
/// If base_snapshot is None, only finds changes at exactly child_snapshot (immediate parent diff)
/// If base_snapshot is Some, finds changes in range (base_snapshot, child_snapshot]
fn find_snapshot_changes(
    fs: &Fs,
    child_snapshot: u32,
    base_snapshot: Option<u32>,
) -> Result<Vec<Change>> {
    let trans = BtreeTrans::new(fs);
    let mut changes = Vec::new();

    let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;

    let mut iter = BtreeIter::new(
        &trans,
        bcachefs::btree_id::BTREE_ID_dirents,
        bch_bindgen::POS_MIN,
        flags,
    );

    while let Some(k) = iter.peek_and_restart()? {
        let key_snap = k.k.p.snapshot;

        // Filter based on whether we have a base or not
        let dominated = match base_snapshot {
            // No base: only keys at exactly child's snapshot ID
            None => key_snap != child_snapshot,
            // With base: keys where base < snap <= child
            // In bcachefs, higher snapshot ID = older, so:
            // child_snapshot <= key_snap < base_snapshot
            Some(base) => key_snap < child_snapshot || key_snap >= base,
        };

        if dominated {
            iter.advance();
            continue;
        }

        let key_type = k.k.type_;

        // Whiteouts = deletions
        if key_type == bcachefs::bch_bkey_type::KEY_TYPE_whiteout as u8 ||
           key_type == bcachefs::bch_bkey_type::KEY_TYPE_hash_whiteout as u8 {
            let name = if key_type == bcachefs::bch_bkey_type::KEY_TYPE_hash_whiteout as u8 {
                get_dirent_name(k.v, k.k)
            } else {
                lookup_dirent_name_at_pos(fs, k.k.p.inode, k.k.p.offset)
            };

            if let Some(name) = name {
                if name != "." && name != ".." {
                    changes.push(Change {
                        kind: ChangeKind::Delete,
                        path: format!("/{}", name),
                    });
                }
            }
            iter.advance();
            continue;
        }

        // Dirents = additions
        if key_type == bcachefs::bch_bkey_type::KEY_TYPE_dirent as u8 {
            if let Some(name) = get_dirent_name(k.v, k.k) {
                if name != "." && name != ".." {
                    changes.push(Change {
                        kind: ChangeKind::Add,
                        path: format!("/{}", name),
                    });
                }
            }
        }

        iter.advance();
    }

    Ok(changes)
}

// STATX_SUBVOL was added in Linux 6.12, not yet in libc crate
const STATX_SUBVOL: u32 = 0x8000;
// Offset of stx_subvol in kernel's struct statx
const STX_SUBVOL_OFFSET: usize = 0xa0;

/// Get subvolume ID from a mounted path using statx
fn get_subvol_id_from_path(path: &std::path::Path) -> Result<u32> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_cstr = CString::new(path.as_os_str().as_bytes())?;

    // Use raw buffer since libc::statx doesn't include stx_subvol yet
    // Kernel struct is ~0xb0 bytes, we allocate extra
    let mut buf = [0u8; 256];

    unsafe {
        let ret = libc::statx(
            libc::AT_FDCWD,
            path_cstr.as_ptr(),
            0,
            STATX_SUBVOL,
            buf.as_mut_ptr() as *mut libc::statx,
        );
        if ret != 0 {
            anyhow::bail!("statx failed: {}", std::io::Error::last_os_error());
        }

        // stx_mask is at offset 0
        let stx_mask = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
        if stx_mask & STATX_SUBVOL == 0 {
            anyhow::bail!("Kernel does not support STATX_SUBVOL (requires Linux 6.12+)");
        }

        // stx_subvol is u64 at offset 0xa0
        let stx_subvol = u64::from_ne_bytes(
            buf[STX_SUBVOL_OFFSET..STX_SUBVOL_OFFSET + 8].try_into().unwrap()
        );

        Ok(stx_subvol as u32)
    }
}

/// Look up a subvolume and return (root_inode, snapshot_id)
fn get_subvolume_info(fs: &Fs, subvol_id: u32) -> Result<(u64, u32)> {
    let trans = BtreeTrans::new(fs);

    // Look up in BTREE_ID_subvolumes
    let pos = bch_bindgen::spos(0, subvol_id as u64, 0);
    let mut iter = BtreeIter::new(
        &trans,
        bcachefs::btree_id::BTREE_ID_subvolumes,
        pos,
        BtreeIterFlags::empty(),
    );

    if let Some(k) = iter.peek_and_restart()? {
        if k.k.p.offset == subvol_id as u64 &&
           k.k.type_ == bcachefs::bch_bkey_type::KEY_TYPE_subvolume as u8 {
            let subvol = unsafe { &*(k.v as *const c::bch_val as *const c::bch_subvolume) };
            let root_ino = subvol.inode;
            let snapshot_id = subvol.snapshot;
            return Ok((root_ino, snapshot_id));
        }
    }

    anyhow::bail!("Subvolume {} not found", subvol_id)
}

/// Compare two subvolumes and return list of changes
fn diff_subvolumes(
    fs: &Fs,
    child_snapshot: u32,
    base_snapshot: Option<u32>,
) -> Result<Vec<Change>> {
    let mut changes = find_snapshot_changes(fs, child_snapshot, base_snapshot)?;
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// Diff a bcachefs snapshot against its parent (or a specified base)
#[derive(Parser, Debug)]
pub struct Cli {
    /// Snapshot subvolume ID (use --id) or path to snapshot directory
    #[arg(long, short = 'i')]
    id: Option<u32>,

    /// Path to mounted snapshot directory (alternative to --id)
    #[arg(long, short = 'p')]
    path: Option<PathBuf>,

    /// Base snapshot ID to diff against (default: immediate parent)
    /// Use this to diff against an older ancestor
    #[arg(long, short = 'b')]
    base: Option<u32>,

    /// Output in JSON format
    #[arg(long, short)]
    json: bool,

    /// Verbose output
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Force color on/off
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Device(s) containing the filesystem
    #[arg(required(true))]
    devices: Vec<PathBuf>,
}

fn cmd_diff_inner(opt: &Cli) -> Result<()> {
    // Get subvolume ID either from --id or --path
    let subvol_id = match (&opt.id, &opt.path) {
        (Some(id), None) => *id,
        (None, Some(path)) => get_subvol_id_from_path(path)?,
        (Some(_), Some(_)) => anyhow::bail!("Cannot specify both --id and --path"),
        (None, None) => anyhow::bail!("Must specify either --id or --path"),
    };

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

    let fs = Fs::open(&opt.devices, fs_opts)?;

    let (_root, snap_id) = get_subvolume_info(&fs, subvol_id)?;

    // Get base snapshot ID if specified
    let base_snap = match opt.base {
        Some(base_subvol) => {
            let (_base_root, base_snap_id) = get_subvolume_info(&fs, base_subvol)?;
            Some(base_snap_id)
        }
        None => None,
    };

    let changes = diff_subvolumes(&fs, snap_id, base_snap)?;

    if opt.json {
        print!("{{\"changes\":[");
        for (i, change) in changes.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            let kind_str = match change.kind {
                ChangeKind::Add => "add",
                ChangeKind::Modify => "modify",
                ChangeKind::Delete => "delete",
            };
            print!("{{\"kind\":\"{}\",\"path\":{:?}}}", kind_str, change.path);
        }
        println!("]}}");
    } else {
        for change in &changes {
            println!("{} {}", change.kind, change.path);
        }
    }

    Ok(())
}

pub fn subvol_diff(argv: Vec<String>) -> Result<()> {
    let opt = Cli::parse_from(argv);

    logging::setup(opt.verbose, opt.colorize);

    cmd_diff_inner(&opt)
}
