// FUSE mount for bcachefs.
//
// Implements the fuser::Filesystem trait over bcachefs's internal btree
// operations, allowing a bcachefs filesystem to be mounted without kernel
// support. Uses the fuser crate (pure Rust FUSE implementation).
//
// Key design notes:
// - Inode numbers: FUSE uses flat u64. bcachefs uses (subvol, inum) pairs.
//   Currently hardcoded to subvolume 1 with root inum 4096 mapped to FUSE
//   ino 1. This is a FUSE protocol limitation — snapshot subvolumes with
//   colliding inode numbers cannot be represented in a single FUSE mount.
// - Daemonization: Must fork() before spawning threads (Linux constraint).
//   bcachefs's shrinker threads and fs_start happen after fork.
// - I/O alignment: All reads and writes must be block-aligned. Unaligned
//   requests get read-modify-write treatment in the write handler.

use std::cell::Cell;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bch_bindgen::fs::FsExt;
use bch_bindgen::c;
use bch_bindgen::data::io::block_on;
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::{accounting, btree, dirent, namei, str_hash, xattr};
use bcachefs_kernel::btree::iter::{CommitFlags, CommitOpts};
use bcachefs_kernel::inode;
use bcachefs_kernel::opt_set;

use crate::util::AlignedBuf;

/// Guard that calls rcu_unregister_thread on drop (i.e. thread exit).
struct RcuGuard;

impl Drop for RcuGuard {
    fn drop(&mut self) {
        eprintln!("fuse worker thread exiting, unregistering RCU");
        unsafe { c::rust_fuse_rcu_unregister() };
    }
}

thread_local! {
    static THREAD_INITIALIZED: Cell<bool> = const { Cell::new(false) };
    // Hold the guard so it lives until the thread exits
    static RCU_GUARD: Cell<Option<RcuGuard>> = const { Cell::new(None) };
}

/// Ensure the current thread has a valid `current` task_struct and
/// is registered with URCU for btree operations.
/// fuser spawns worker threads that don't run the sched_init() constructor,
/// so `current` starts as NULL and RCU isn't set up.
fn ensure_thread_init() {
    THREAD_INITIALIZED.with(|init| {
        if !init.get() {
            unsafe { c::rust_fuse_ensure_current() };
            unsafe { c::rust_fuse_rcu_register() };
            RCU_GUARD.with(|g| g.set(Some(RcuGuard)));
            init.set(true);
            eprintln!("fuse worker thread initialized (current + RCU)");
        }
    });
}

use fuser::{
    Config, FileAttr, FileType, Filesystem, MountOption,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
    Errno, FileHandle, FopenFlags, Generation,
    INodeNo, OpenFlags, RenameFlags,
    BsdFileFlags, WriteFlags, LockOwner,
};

const TTL: Duration = Duration::MAX;

const BCACHEFS_ROOT_INO: u64 = 4096;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const DT_FIFO: u32 = 1;
const DT_CHR:  u32 = 2;
const DT_DIR:  u32 = 4;
const DT_BLK:  u32 = 6;
const DT_REG:  u32 = 8;
const DT_LNK:  u32 = 10;
const DT_SOCK: u32 = 12;

fn map_root_ino(ino: INodeNo) -> c::subvol_inum {
    let ino: u64 = ino.0;
    c::subvol_inum {
        subvol: 1,
        inum: if ino == 1 { BCACHEFS_ROOT_INO } else { ino },
    }
}

fn unmap_root_ino(ino: u64) -> u64 {
    if ino == BCACHEFS_ROOT_INO { 1 } else { ino }
}

fn mode_to_filetype(mode: u32) -> FileType {
    match rustix::fs::FileType::from_raw_mode(mode) {
        rustix::fs::FileType::RegularFile     => FileType::RegularFile,
        rustix::fs::FileType::Directory       => FileType::Directory,
        rustix::fs::FileType::Symlink         => FileType::Symlink,
        rustix::fs::FileType::BlockDevice     => FileType::BlockDevice,
        rustix::fs::FileType::CharacterDevice => FileType::CharDevice,
        rustix::fs::FileType::Fifo            => FileType::NamedPipe,
        rustix::fs::FileType::Socket          => FileType::Socket,
        _                                     => FileType::RegularFile,
    }
}

fn dtype_to_filetype(dtype: u32) -> FileType {
    match dtype {
        DT_DIR  => FileType::Directory,
        DT_REG  => FileType::RegularFile,
        DT_LNK  => FileType::Symlink,
        DT_BLK  => FileType::BlockDevice,
        DT_CHR  => FileType::CharDevice,
        DT_FIFO => FileType::NamedPipe,
        DT_SOCK => FileType::Socket,
        _       => FileType::RegularFile,
    }
}

fn signal_parent(fd: OwnedFd, byte: u8) {
    let _ = File::from(fd).write_all(&[byte]);
}

/// Convert a raw C return value (negative bcachefs error code) to a fuser Errno.
/// Walks the bcachefs error hierarchy to the root standard errno.
fn err(ret: i32) -> Errno {
    let e = BchError::from_raw(if ret < 0 { -ret } else { ret });
    Errno::from_i32(e.errno())
}

/// Convert a BchError to a fuser Errno.
fn bch_err(e: &BchError) -> Errno {
    Errno::from_i32(e.errno())
}

/// Mirrors avail_factor() in alloc/buckets.h: reserve 1/(2^6+1) of free space
/// so statfs's f_bavail matches what the allocator will actually hand out.
fn avail_factor(free: u64) -> u64 {
    const RESERVE_FACTOR: u32 = 6;
    (free << RESERVE_FACTOR) / ((1 << RESERVE_FACTOR) + 1)
}

fn start_fs(raw: *mut c::bch_fs) -> Result<(), BchError> {
    let fs = unsafe { Fs::borrow_raw(raw) };
    fs.start()
}

#[allow(clippy::too_many_arguments)]
fn fuse_create_inode(
    fs:    &Fs,
    dir:   c::subvol_inum,
    name:  &[u8],
    mode:  u16,
    rdev:  u64,
    uid:   u32,
    gid:   u32,
) -> Result<c::bch_inode_unpacked, BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();
    let mut subvol: c::bch_subvolume = Default::default();

    inode::init_early(fs, &mut inode);

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| {
            namei::create_trans(
                t,
                dir,
                &mut dir_u,
                &mut inode,
                &mut subvol,
                &qstr,
                uid,
                gid,
                mode,
                rdev,
                c::subvol_inum::default(),
                0,
            )
        },
    )?;

    Ok(inode)
}

fn fuse_unlink(fs: &Fs, dir: c::subvol_inum, name: &[u8]) -> Result<(), BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            namei::unlink_trans(
                t,
                dir,
                &mut dir_u,
                c::subvol_inum::default(),
                &mut inode,
                &qstr,
                false,
            )
        },
    )
}

fn fuse_link(
    fs:        &Fs,
    inum:      c::subvol_inum,
    newparent: c::subvol_inum,
    name:      &[u8],
) -> Result<c::bch_inode_unpacked, BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| namei::link_trans(t, newparent, &mut dir_u, inum, &mut inode, &qstr),
    )?;

    Ok(inode)
}

/// Pick the rename mode the way the kernel VFS does: EXCHANGE if asked for,
/// otherwise probe the destination to tell a plain rename from an overwrite.
fn fuse_rename_mode(
    fs:       &Fs,
    dst_dir:  c::subvol_inum,
    dst_qstr: &c::qstr,
    flags:    RenameFlags,
) -> Result<c::bch_rename_mode, BchError> {
    if flags.contains(RenameFlags::RENAME_EXCHANGE) {
        return Ok(c::bch_rename_mode::BCH_RENAME_EXCHANGE);
    }

    let dir_u = inode::find_by_inum(fs, dst_dir)?;
    let hash_info = str_hash::hash_info_init(fs, &dir_u)?;

    match dirent::lookup(fs, dst_dir, &hash_info, dst_qstr) {
        Ok(_) => {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                Err(BchError::from_raw(libc::EEXIST))
            } else {
                Ok(c::bch_rename_mode::BCH_RENAME_OVERWRITE)
            }
        }
        Err(e) if e.matches_errno(libc::ENOENT) => Ok(c::bch_rename_mode::BCH_RENAME),
        Err(e) => Err(e),
    }
}

fn fuse_rename(
    fs:       &Fs,
    src_dir:  c::subvol_inum,
    src_name: &[u8],
    dst_dir:  c::subvol_inum,
    dst_name: &[u8],
    flags:    RenameFlags,
) -> Result<(), BchError> {
    let src_qstr = dirent::qstr(src_name);
    let dst_qstr = dirent::qstr(dst_name);
    let mode = fuse_rename_mode(fs, dst_dir, &dst_qstr, flags)?;
    let mut src_dir_u: c::bch_inode_unpacked = Default::default();
    let mut dst_dir_u: c::bch_inode_unpacked = Default::default();
    let mut src_inode_u: c::bch_inode_unpacked = Default::default();
    let mut dst_inode_u: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| {
            namei::rename_trans(
                t,
                src_dir,
                &mut src_dir_u,
                dst_dir,
                &mut dst_dir_u,
                &mut src_inode_u,
                &mut dst_inode_u,
                &src_qstr,
                &dst_qstr,
                mode,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn fuse_setattr(
    fs:         &Fs,
    inum:       c::subvol_inum,
    mode:       Option<u16>,
    uid:        Option<u32>,
    gid:        Option<u32>,
    size:       Option<u64>,
    atime_flag: i32,
    atime:      u64,
    mtime_flag: i32,
    mtime:      u64,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode_out: c::bch_inode_unpacked = Default::default();

    // Truncate first: setting bi_size alone would leave extents past EOF.
    if let Some(size) = size {
        let mut i_sectors_delta: u64 = 0;
        let ret = unsafe { c::bch2_truncate(fs.raw, inum, size, &mut i_sectors_delta) };
        errcode::ret_to_result_void(ret)?;
    }

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            let now = fs.current_time();
            let mut iter = btree::iter::BtreeIter::uninit();
            let mut inode_u: c::bch_inode_unpacked = Default::default();

            let t = inode::peek(
                t,
                &mut iter,
                &mut inode_u,
                inum,
                btree::iter::BtreeIterFlags::INTENT,
            )?;

            if let Some(mode) = mode {
                inode_u.bi_mode = mode;
            }
            if let Some(uid) = uid {
                inode_u.bi_uid = uid;
            }
            if let Some(gid) = gid {
                inode_u.bi_gid = gid;
            }
            if atime_flag == 1 {
                inode_u.bi_atime = atime;
            }
            if atime_flag == 2 {
                inode_u.bi_atime = now;
            }
            if mtime_flag == 1 {
                inode_u.bi_mtime = mtime;
            }
            if mtime_flag == 2 {
                inode_u.bi_mtime = now;
            }
            inode_u.bi_ctime = now;

            let t = inode::write(t, &mut iter, &mut inode_u)?;
            inode_out = inode_u;
            Ok(t)
        },
    )?;

    Ok(inode_out)
}

fn fuse_update_inode_after_write(fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            let now = fs.current_time();
            let mut iter = btree::iter::BtreeIter::uninit();
            let mut inode_u: c::bch_inode_unpacked = Default::default();

            let t = inode::peek(
                t,
                &mut iter,
                &mut inode_u,
                inum,
                btree::iter::BtreeIterFlags::INTENT,
            )?;
            inode_u.bi_mtime = now;
            inode_u.bi_ctime = now;
            inode::write(t, &mut iter, &mut inode_u)
        },
    )
}

const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
const XATTR_REPLACE:       i32 = 0x02;

// KEY_TYPE_XATTR_INDEX_* from fs/xattr_format.h: plain #defines, not in the
// bindgen allowlist. Same treatment as src/copy_fs.rs.
const KEY_TYPE_XATTR_INDEX_USER:     i32 = 0;
const KEY_TYPE_XATTR_INDEX_TRUSTED:  i32 = 3;
const KEY_TYPE_XATTR_INDEX_SECURITY: i32 = 4;

/// Namespace prefixes FUSE will hand us, mapped to bcachefs xattr type indices.
/// system.* is deliberately absent: it maps to the POSIX ACL indices, whose
/// values need ACL parsing rather than an opaque blob passthrough.
const XATTR_NAMESPACES: &[(&[u8], i32)] = &[
    (b"user.",     KEY_TYPE_XATTR_INDEX_USER),
    (b"trusted.",  KEY_TYPE_XATTR_INDEX_TRUSTED),
    (b"security.", KEY_TYPE_XATTR_INDEX_SECURITY),
];

/// Split "user.foo" into its type index and the short name bcachefs stores.
fn parse_xattr_name(full: &[u8]) -> Option<(i32, CString)> {
    for (prefix, typ) in XATTR_NAMESPACES {
        if let Some(short) = full.strip_prefix(*prefix) {
            return CString::new(short).ok().map(|n| (*typ, n));
        }
    }
    None
}

fn xattr_type_to_prefix(typ: u8) -> Option<&'static [u8]> {
    XATTR_NAMESPACES
        .iter()
        .find(|(_, t)| *t == typ as i32)
        .map(|(p, _)| *p)
}

fn fuse_xattr_hash_info(
    fs:   &Fs,
    inum: c::subvol_inum,
) -> Result<c::bch_hash_info, BchError> {
    let bi = inode::find_by_inum(fs, inum)?;
    str_hash::hash_info_init(fs, &bi)
}

/// Read an xattr value. ENOENT is translated to ENODATA, which is what
/// getxattr(2) callers expect for a missing attribute.
fn fuse_xattr_get(
    fs:    &Fs,
    inum:  c::subvol_inum,
    typ:   i32,
    name:  &CStr,
) -> Result<Vec<u8>, BchError> {
    let hash_info = fuse_xattr_hash_info(fs, inum)?;

    xattr::get(fs, inum, &hash_info, name, typ as u8).map_err(|e| {
        if e.matches_errno(libc::ENOENT) {
            BchError::from_raw(libc::ENODATA)
        } else {
            e
        }
    })
}

/// Set an xattr, or remove it when `value` is `None`.
fn fuse_xattr_set(
    fs:    &Fs,
    inum:  c::subvol_inum,
    typ:   i32,
    name:  &CStr,
    value: Option<&[u8]>,
    flags: i32,
) -> Result<(), BchError> {
    let mut inode_u: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| xattr::set(t, inum, &mut inode_u, name, value, typ, flags),
    )
}

struct BcachefsFs {
    c: *mut c::bch_fs,
    /// Write end of a pipe used to signal the parent process that the
    /// FUSE mount is established. Written in init(), None in foreground mode.
    signal_fd: Option<OwnedFd>,
}

// Safety: bch_fs is internally synchronized with its own locking.
unsafe impl Send for BcachefsFs {}
unsafe impl Sync for BcachefsFs {}

impl BcachefsFs {
    fn fs(&self) -> std::mem::ManuallyDrop<Fs> {
        unsafe { Fs::borrow_raw(self.c) }
    }

    fn inode_to_attr(&self, bi: &c::bch_inode_unpacked) -> FileAttr {
        let fs = self.fs();
        let ts_a = fs.time_to_timespec(bi.bi_atime as i64);
        let ts_m = fs.time_to_timespec(bi.bi_mtime as i64);
        let ts_c = fs.time_to_timespec(bi.bi_ctime as i64);
        let blksize = fs.block_bytes() as u32;
        let nlink = Fs::inode_nlink_get(bi);

        FileAttr {
            ino: INodeNo(unmap_root_ino(bi.bi_inum)),
            size: bi.bi_size,
            blocks: bi.bi_sectors,
            atime: ts_to_systime(ts_a),
            mtime: ts_to_systime(ts_m),
            ctime: ts_to_systime(ts_c),
            crtime: UNIX_EPOCH,
            kind: mode_to_filetype(bi.bi_mode as u32),
            perm: (bi.bi_mode & 0o7777),
            nlink,
            uid: bi.bi_uid,
            gid: bi.bi_gid,
            rdev: bi.bi_dev,
            blksize,
            flags: 0,
        }
    }
}

fn ts_to_systime(ts: c::timespec) -> SystemTime {
    if ts.tv_sec >= 0 {
        UNIX_EPOCH + Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    } else {
        UNIX_EPOCH
    }
}

impl Filesystem for BcachefsFs {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        eprintln!("bcachefs fuse: init callback fired");
        // Signal parent that mount is established
        if let Some(fd) = self.signal_fd.take() {
            eprintln!("bcachefs fuse: signaling parent");
            signal_parent(fd, 0);
        }
        eprintln!("bcachefs fuse: init returning Ok");
        Ok(())
    }

    fn destroy(&mut self) {
        eprintln!("bcachefs fuse: destroy");
        unsafe { c::bch2_fs_exit(self.c) };
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_lookup(dir={}, name={:?})", dir.inum, name);

        let fs = self.fs();
        let qstr = dirent::qstr(name_bytes);
        let lookup = inode::find_by_inum(&fs, dir)
            .and_then(|dir_u| str_hash::hash_info_init(&fs, &dir_u))
            .and_then(|hash_info| dirent::lookup(&fs, dir, &hash_info, &qstr))
            .and_then(|inum| inode::find_by_inum(&fs, inum).map(|bi| (inum, bi)));

        let (inum, bi) = match lookup {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  lookup -> err {}", e);
                // Negative dentry caching: return empty entry for ENOENT
                if e.matches_errno(libc::ENOENT) {
                    let attr = FileAttr {
                        ino: INodeNo(0),
                        size: 0, blocks: 0,
                        atime: UNIX_EPOCH, mtime: UNIX_EPOCH,
                        ctime: UNIX_EPOCH, crtime: UNIX_EPOCH,
                        kind: FileType::RegularFile, perm: 0,
                        nlink: 0, uid: 0, gid: 0, rdev: 0,
                        blksize: 0, flags: 0,
                    };
                    reply.entry(&TTL, &attr, Generation(0));
                    return;
                }
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  lookup -> ok inum={}", inum.inum);
        let attr = self.inode_to_attr(&bi);
        reply.entry(&TTL, &attr, Generation(bi.bi_generation as u64));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_getattr(inum={})", inum.inum);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => {
                eprintln!("  getattr -> err {}", e.raw());
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  getattr -> ok");
        reply.attr(&TTL, &self.inode_to_attr(&bi));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_setattr(inum={})", inum.inum);

        let fs = self.fs();

        let parse_time = |time: &Option<TimeOrNow>| match time {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = c::timespec {
                    tv_sec: d.as_secs() as _,
                    tv_nsec: d.subsec_nanos() as _,
                    ..unsafe { std::mem::zeroed() }
                };
                (1, fs.timespec_to_time(ts) as u64)
            }
        };

        let (atime_flag, atime_val) = parse_time(&atime);
        let (mtime_flag, mtime_val) = parse_time(&mtime);

        let bi = match fuse_setattr(
            &fs,
            inum,
            mode.map(|mode| mode as u16),
            uid,
            gid,
            size,
            atime_flag,
            atime_val,
            mtime_flag,
            mtime_val,
        ) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        reply.attr(&TTL, &self.inode_to_attr(&bi));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_readlink(inum={})", inum.inum);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let size = bi.bi_size as usize;
        let block_size = fs.block_bytes() as usize;
        let aligned_size = (size + block_size - 1) & !(block_size - 1);

        let mut buf = AlignedBuf::new(aligned_size);

        if let Err(e) = block_on(fs.read(inum, 0, &bi, &mut buf)) {
            reply.error(bch_err(&e));
            return;
        }

        let end = buf[..size].iter().position(|&b| b == 0).unwrap_or(size);
        reply.data(&buf[..end]);
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_mknod(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let fs = self.fs();
        let new_inode = match fuse_create_inode(
            &fs, dir, name_bytes, mode as u16, rdev as u64, req.uid(), req.gid(),
        ) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&new_inode);
        reply.entry(&TTL, &attr, Generation(new_inode.bi_generation as u64));
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        eprintln!("fuse_mkdir(dir={}, name={:?})", parent.0, name);
        self.mknod(req, parent, name, mode | S_IFDIR, umask, 0, reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_unlink(dir={}, name={:?})", dir.inum, name);

        let fs = self.fs();
        match fuse_unlink(&fs, dir, name_bytes) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("fuse_rmdir(dir={}, name={:?})", parent.0, name);
        self.unlink(req, parent, name, reply);
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        let link_bytes = link.as_os_str().as_bytes();
        eprintln!("fuse_symlink(dir={}, name={:?}, link={:?})", dir.inum, name, link);

        // Create the symlink inode
        let fs = self.fs();
        let new_inode = match fuse_create_inode(
            &fs, dir, name_bytes, (S_IFLNK | 0o777) as u16, 0, req.uid(), req.gid(),
        ) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        // Write link target (include NUL terminator, like the C code did)
        let block_size = fs.block_bytes();
        let link_with_nul_len = link_bytes.len() + 1;
        let padded = (link_with_nul_len as u64).div_ceil(block_size) * block_size;

        let mut buf = AlignedBuf::new(padded as usize);
        buf[..link_bytes.len()].copy_from_slice(link_bytes);
        // buf is zero-initialized, so NUL terminator and padding are already 0

        let sym_inum = c::subvol_inum { subvol: dir.subvol, inum: new_inode.bi_inum };
        if let Err(e) = block_on(fs.write(new_inode.bi_inum, 0, dir.subvol as u32,
                                          1, &buf, link_with_nul_len as u64)) {
            reply.error(bch_err(&e));
            return;
        }

        // Re-read inode to get updated state
        let fs = self.fs();
        let new_inode = match inode::find_by_inum(&fs, sym_inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&new_inode);
        reply.entry(&TTL, &attr, Generation(new_inode.bi_generation as u64));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        let src_dir = map_root_ino(parent);
        let dst_dir = map_root_ino(newparent);
        let src_bytes = name.as_bytes();
        let dst_bytes = newname.as_bytes();
        eprintln!("fuse_rename(src_dir={}, {:?} -> dst_dir={}, {:?})",
               src_dir.inum, name, dst_dir.inum, newname);

        let supported = RenameFlags::RENAME_NOREPLACE | RenameFlags::RENAME_EXCHANGE;
        if !(flags - supported).is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }

        let fs = self.fs();
        match fuse_rename(&fs, src_dir, src_bytes, dst_dir, dst_bytes, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let src_inum = map_root_ino(ino);
        let parent = map_root_ino(newparent);
        let name_bytes = newname.as_bytes();
        eprintln!("fuse_link(ino={}, newparent={}, name={:?})",
               src_inum.inum, parent.inum, newname);

        let fs = self.fs();
        let inode_u = match fuse_link(&fs, src_inum, parent, name_bytes) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&inode_u);
        reply.entry(&TTL, &attr, Generation(inode_u.bi_generation as u64));
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        eprintln!("fuse_open(ino={})", ino.0);
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        let size = size as usize;
        eprintln!("fuse_read(ino={}, offset={}, size={})", inum.inum, offset, size);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let end = std::cmp::min(bi.bi_size, offset + size as u64);
        if end <= offset {
            reply.data(&[]);
            return;
        }
        let read_size = (end - offset) as usize;

        let block_size = fs.block_bytes();
        let aligned_start = offset & !(block_size - 1);
        let pad_start = (offset - aligned_start) as usize;
        let aligned_end = (offset + read_size as u64).div_ceil(block_size) * block_size;
        let aligned_size = (aligned_end - aligned_start) as usize;

        let mut buf = AlignedBuf::new(aligned_size);

        if let Err(e) = block_on(fs.read(inum, aligned_start, &bi, &mut buf)) {
            reply.error(bch_err(&e));
            return;
        }

        reply.data(&buf[pad_start..pad_start + read_size]);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        let size = data.len();
        eprintln!("fuse_write(ino={}, offset={}, size={})", inum.inum, offset, size);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let block_size = fs.block_bytes();

        // Compute alignment
        let aligned_start = offset & !(block_size - 1);
        let pad_start = (offset - aligned_start) as usize;
        let aligned_end = (offset + size as u64).div_ceil(block_size) * block_size;
        let aligned_size = (aligned_end - aligned_start) as usize;

        let mut buf = AlignedBuf::new(aligned_size);

        // RMW: read partial start block
        if pad_start > 0 {
            let mut start_block = AlignedBuf::new(block_size as usize);
            if let Err(e) = block_on(fs.read(inum, aligned_start, &bi, &mut start_block)) {
                reply.error(bch_err(&e));
                return;
            }
            buf[..block_size as usize].copy_from_slice(&start_block);
        }

        // RMW: read partial end block (if different from start)
        let pad_end = (aligned_end - offset - size as u64) as usize;
        if pad_end > 0 && !(pad_start > 0 && aligned_size == block_size as usize) {
            let end_block_offset = aligned_end - block_size;
            let buf_offset = aligned_size - block_size as usize;
            let mut end_block = AlignedBuf::new(block_size as usize);
            if let Err(e) = block_on(fs.read(inum, end_block_offset, &bi, &mut end_block)) {
                reply.error(bch_err(&e));
                return;
            }
            buf[buf_offset..].copy_from_slice(&end_block);
        }

        // Overlay user data
        buf[pad_start..pad_start + size].copy_from_slice(data);

        // Get inode opts for replicas
        let opts = inode::opts_get_inode(&fs, &bi);
        let replicas = std::cmp::max(opts.data_replicas as u32, 1);

        // Write aligned buffer
        let new_i_size = offset + size as u64;
        if let Err(e) = block_on(fs.write(bi.bi_inum, aligned_start, inum.subvol as u32,
                                          replicas, &buf, new_i_size)) {
            reply.error(bch_err(&e));
            return;
        }

        // Update inode times
        if let Err(e) = fuse_update_inode_after_write(&fs, inum) {
            reply.error(bch_err(&e));
            return;
        }

        reply.written(size as u32);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(ino);
        eprintln!("fuse_readdir(dir={}, offset={})", dir.inum, offset);

        let mut pos = offset;

        // Handle . and ..
        if pos == 0 {
            if reply.add(INodeNo(unmap_root_ino(dir.inum)), 1, FileType::Directory, ".") {
                reply.ok();
                return;
            }
            pos = 1;
        }
        if pos == 1 {
            let fs = self.fs();
            let parent = match inode::find_by_inum(&fs, dir) {
                Ok(bi) => unmap_root_ino(bi.bi_dir),
                Err(e) => { reply.error(bch_err(&e)); return; }
            };
            if reply.add(INodeNo(parent), 2, FileType::Directory, "..") {
                reply.ok();
                return;
            }
            pos = 2;
        }

        // Read remaining entries via C shim with callback
        unsafe extern "C" fn filldir(
            ctx: *mut std::ffi::c_void,
            name: *const std::ffi::c_char,
            name_len: std::ffi::c_uint,
            ino: u64,
            dtype: std::ffi::c_uint,
            pos: u64,
        ) -> std::ffi::c_int {
            let reply = unsafe { &mut *(ctx as *mut ReplyDirectory) };
            let name_bytes = unsafe {
                std::slice::from_raw_parts(name as *const u8, name_len as usize)
            };
            let name_str = OsStr::from_bytes(name_bytes);
            let file_type = dtype_to_filetype(dtype);
            let full = reply.add(INodeNo(unmap_root_ino(ino)), pos, file_type, name_str);
            if full { -1 } else { 0 }
        }

        let ret = unsafe {
            c::rust_fuse_readdir(
                self.c, dir, pos,
                &mut reply as *mut ReplyDirectory as *mut _,
                Some(filldir),
            )
        };

        if ret != 0 {
            reply.error(err(ret));
        } else {
            reply.ok();
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        ensure_thread_init();
        eprintln!("fuse_statfs");

        let fs = self.fs();
        let usage = fs.usage_read_short();
        let block_size = fs.block_bytes();
        let shift = unsafe { (*self.c).block_bits } as u64;

        let nr_inodes = accounting::nr_inodes(&fs);

        reply.statfs(
            usage.capacity >> shift,
            usage.free >> shift,
            avail_factor(usage.free) >> shift,
            nr_inodes,
            u64::MAX,
            block_size as u32,
            255,
            block_size as u32,
        );
    }

    /// FUSE flush is close(2), not a writeback point: there is no page cache
    /// here, so there is nothing to write back. fsync is what commits.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        eprintln!("fuse_fsync(ino={})", ino.0);

        // Writes go straight to the btree, so flushing the journal is
        // sufficient to make them durable.
        let ret = unsafe { c::bch2_journal_flush(&raw mut (*self.c).journal) };
        match errcode::ret_to_result_void(ret) {
            Ok(())  => reply.ok(),
            Err(e)  => reply.error(bch_err(&e)),
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_fallocate(ino={}, offset={}, length={}, mode={:#x})",
               inum.inum, offset, length, mode);

        // Only plain allocation and KEEP_SIZE are handled; punch/collapse/
        // insert/zero would each need real extent work.
        if mode & !FALLOC_FL_KEEP_SIZE != 0 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }

        let end = offset + length;
        let keep_size = mode & FALLOC_FL_KEEP_SIZE != 0;
        let fs = self.fs();

        let res = btree::iter::trans_commit_do(
            &fs,
            None,
            CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
            |t| {
                let now = fs.current_time();
                let mut iter = btree::iter::BtreeIter::uninit();
                let mut inode_u: c::bch_inode_unpacked = Default::default();

                let t = inode::peek(
                    t,
                    &mut iter,
                    &mut inode_u,
                    inum,
                    btree::iter::BtreeIterFlags::INTENT,
                )?;

                if !keep_size && end > inode_u.bi_size {
                    inode_u.bi_size  = end;
                    inode_u.bi_mtime = now;
                    inode_u.bi_ctime = now;
                }

                inode::write(t, &mut iter, &mut inode_u)
            },
        );

        match res {
            Ok(())  => reply.ok(),
            Err(e)  => reply.error(bch_err(&e)),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_setxattr(ino={}, name={:?})", inum.inum, name);

        let Some((typ, short)) = parse_xattr_name(name.as_bytes()) else {
            reply.error(Errno::EOPNOTSUPP);
            return;
        };

        let fs = self.fs();
        match fuse_xattr_set(&fs, inum, typ, &short, Some(value), flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_getxattr(ino={}, name={:?}, size={})", inum.inum, name, size);

        let Some((typ, short)) = parse_xattr_name(name.as_bytes()) else {
            reply.error(Errno::EOPNOTSUPP);
            return;
        };

        let fs = self.fs();
        let val = match fuse_xattr_get(&fs, inum, typ, &short) {
            Ok(v)  => v,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        if size == 0 {
            reply.size(val.len() as u32);
        } else if (size as usize) < val.len() {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&val);
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_listxattr(ino={}, size={})", inum.inum, size);

        let fs = self.fs();
        let attrs = match xattr::list(&fs, inum) {
            Ok(v)  => v,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        // NUL-separated, NUL-terminated names, as listxattr(2) specifies.
        let mut buf: Vec<u8> = Vec::new();
        for (typ, short) in &attrs {
            let Some(prefix) = xattr_type_to_prefix(*typ) else { continue };
            buf.extend_from_slice(prefix);
            buf.extend_from_slice(short);
            buf.push(0);
        }

        if size == 0 {
            reply.size(buf.len() as u32);
        } else if (size as usize) < buf.len() {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&buf);
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_removexattr(ino={}, name={:?})", inum.inum, name);

        let Some((typ, short)) = parse_xattr_name(name.as_bytes()) else {
            reply.error(Errno::EOPNOTSUPP);
            return;
        };

        let fs = self.fs();
        // XATTR_REPLACE makes the removal fail if the attribute is absent.
        match fuse_xattr_set(&fs, inum, typ, &short, None, XATTR_REPLACE) {
            Ok(()) => reply.ok(),
            Err(e) if e.matches_errno(libc::ENOENT) => reply.error(Errno::ENODATA),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_create(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let fs = self.fs();
        let new_inode = match fuse_create_inode(
            &fs, dir, name_bytes, mode as u16, 0, req.uid(), req.gid(),
        ) {
            Ok(inode) => inode,
            Err(e)    => {
                eprintln!("  create -> err {}", e);
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  create -> ok inum={}", new_inode.bi_inum);
        let attr = self.inode_to_attr(&new_inode);
        reply.created(
            &TTL, &attr,
            Generation(new_inode.bi_generation as u64),
            FileHandle(0),
            FopenFlags::FOPEN_KEEP_CACHE,
        );
    }
}

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "fusemount")]
pub struct Cli {
    /// Mount options (-o key=value,...)
    #[arg(short = 'o')]
    pub options: Option<String>,

    /// Run in foreground
    #[arg(short = 'f')]
    pub foreground: bool,

    /// Device(s) to mount (dev1:dev2:...)
    pub device: String,

    /// Mountpoint
    pub mountpoint: String,
}

fn parse_fuse_mount_options(
    device: &str,
    options: Option<&str>,
) -> anyhow::Result<(c::bch_opts, Vec<MountOption>)> {
    let mut mount_options = vec![
        MountOption::FSName(device.to_string()),
        // Use CUSTOM instead of Subtype — fuser categorizes Subtype as
        // "Fusermount" group, which is only passed when using the fusermount3
        // helper. With a direct mount syscall (as root), Subtype gets
        // silently dropped and the mount shows as "fuse" instead of
        // "fuse.bcachefs" in /proc/mounts.
        MountOption::CUSTOM("subtype=bcachefs".to_string()),
    ];
    let parsed = options
        .map(super::mount::parse_mountflag_options)
        .unwrap_or_default();
    let mut bch_opts = bcachefs_kernel::opts::parse_mount_opts(None, parsed.fs_opts.as_deref(), true)?;

    opt_set!(bch_opts, nostart, 1);

    // read-only is both a fuser option (carried in parsed.fuse_options) and a
    // filesystem-open concern - the latter isn't a mount flag, so set it here:
    if parsed.flags & libc::MS_RDONLY != 0 {
        opt_set!(bch_opts, read_only, 1);
    }

    mount_options.extend(parsed.fuse_options);

    Ok((bch_opts, mount_options))
}

pub fn cmd_fusemount(cli: Cli) -> anyhow::Result<()> {
    use crate::device_scan::scan_sbs;

    let (bch_opts, mount_options) = parse_fuse_mount_options(&cli.device, cli.options.as_deref())?;

    let sbs = scan_sbs(&cli.device, &bch_opts)?;
    let devs: Vec<_> = sbs.iter().map(|(p, _)| p.clone()).collect();

    let fs = Fs::open(&devs, bch_opts)
        .map_err(|e| anyhow::anyhow!("Error opening filesystem: {}", e))?;
    let fs_raw = fs.raw;
    // BcachefsFs::destroy takes ownership — prevent Fs double-free
    std::mem::forget(fs);

    let mut config = Config::default();
    config.mount_options = mount_options;
    // Worker threads get current + RCU via ensure_thread_init() with
    // a Drop guard for cleanup. No need to restrict to single-threaded.

    if cli.foreground {
        unsafe { c::linux_shrinkers_init() };
        if let Err(e) = start_fs(fs_raw) {
            unsafe { c::bch2_fs_exit(fs_raw) };
            anyhow::bail!("Error starting filesystem: {}", e);
        }
        let bcachefs_fs = BcachefsFs { c: fs_raw, signal_fd: None };
        fuser::mount2(bcachefs_fs, &cli.mountpoint, &config)?;
        return Ok(());
    }

    // Daemonize with pipe-based synchronization.
    //
    // The parent must not return until the FUSE mount is established,
    // otherwise mount(8) reports success before the mountpoint is usable.
    // The child signals readiness from the FUSE init() callback, which
    // fires after the kernel has acknowledged the mount.
    //
    // fork() must happen before spawning threads (linux_shrinkers_init,
    // bch2_fs_start) because only the calling thread survives fork().
    let (read_fd, write_fd) = rustix::pipe::pipe()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork() failed");
    }

    if pid > 0 {
        // Parent: wait for child to signal mount readiness
        drop(write_fd);
        let mut buf = [0u8; 1];
        let n = File::from(read_fd).read(&mut buf)?;

        if n == 1 && buf[0] == 0 {
            std::process::exit(0);
        } else {
            let pid = rustix::process::Pid::from_raw(pid)
                .ok_or_else(|| anyhow::anyhow!("invalid child pid {}", pid))?;
            let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
            anyhow::bail!("FUSE mount failed in child process");
        }
    }

    // Child
    drop(read_fd);
    rustix::process::setsid()?;

    // Daemon mode must not inherit the caller's stderr or grow a fixed log
    // file under /tmp; foreground mode still leaves debug output visible.
    if let Ok(f) = std::fs::File::create("/dev/null") {
        rustix::stdio::dup2_stderr(&f)?;
    }

    unsafe { c::linux_shrinkers_init() };

    eprintln!("fusemount: starting filesystem");
    if let Err(e) = start_fs(fs_raw) {
        eprintln!("fusemount: bch2_fs_start failed: {}", e);
        unsafe { c::bch2_fs_exit(fs_raw) };
        signal_parent(write_fd, 1);
        std::process::exit(1);
    }
    eprintln!("fusemount: filesystem started, calling fuser::mount2");

    let bcachefs_fs = BcachefsFs { c: fs_raw, signal_fd: Some(write_fd.try_clone()?) };

    match fuser::mount2(bcachefs_fs, &cli.mountpoint, &config) {
        Ok(()) => {
            eprintln!("fusemount: fuser::mount2 returned normally (unmounted)");
        }
        Err(e) => {
            eprintln!("fusemount: fuser::mount2 failed: {}", e);
            signal_parent(write_fd, 1);
            std::process::exit(1);
        }
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("fusemount", "FUSE mount", Cli, cmd_fusemount);

#[cfg(test)]
mod tests {
    use super::parse_fuse_mount_options;
    use bcachefs_kernel::opt_get;
    use fuser::MountOption;

    #[test]
    fn parse_fuse_mount_options_sets_bcachefs_read_only_and_fuse_ro() {
        let (opts, mount_options) =
            parse_fuse_mount_options("/dev/test", Some("ro,norecovery")).unwrap();

        assert_eq!(opt_get!(opts, read_only), 1);
        assert!(mount_options.contains(&MountOption::RO));
        assert!(mount_options.contains(&MountOption::FSName("/dev/test".to_string())));
        assert!(mount_options.contains(&MountOption::CUSTOM("subtype=bcachefs".to_string())));
    }

    #[test]
    fn parse_fuse_mount_options_preserves_supported_fuse_flags() {
        let (_opts, mount_options) = parse_fuse_mount_options(
            "/dev/test",
            Some("nodev,nosuid,noexec,noatime,dirsync,sync"),
        )
        .unwrap();

        for option in [
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
            MountOption::NoAtime,
            MountOption::DirSync,
            MountOption::Sync,
        ] {
            assert!(mount_options.contains(&option));
        }
    }
}
