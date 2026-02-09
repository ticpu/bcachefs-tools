use std::ffi::CStr;
use std::os::fd::BorrowedFd;
use std::path::Path;

use bch_bindgen::c::{
    bcache_fs_close, bcache_fs_open_fallible,
    bch_ioctl_disk, bch_ioctl_disk_v2,
    bch_ioctl_disk_set_state, bch_ioctl_disk_set_state_v2,
    bch_ioctl_disk_resize, bch_ioctl_disk_resize_v2,
    bch_ioctl_disk_resize_journal, bch_ioctl_disk_resize_journal_v2,
    bch_ioctl_subvolume, bch_ioctl_subvolume_v2,
    bchfs_handle,
    BCH_BY_INDEX, BCH_SUBVOL_SNAPSHOT_CREATE,
};
use bch_bindgen::errcode::{BchError, ret_to_result};
use bch_bindgen::path_to_cstr;
use errno::Errno;
use rustix::ioctl::{self, CompileTimeOpcode, Setter, WriteOpcode};

// Subvolume ioctl opcodes
type SubvolCreateOpcode    = WriteOpcode<0xbc, 16, bch_ioctl_subvolume>;
type SubvolCreateV2Opcode  = WriteOpcode<0xbc, 29, bch_ioctl_subvolume_v2>;
type SubvolDestroyOpcode   = WriteOpcode<0xbc, 17, bch_ioctl_subvolume>;
type SubvolDestroyV2Opcode = WriteOpcode<0xbc, 30, bch_ioctl_subvolume_v2>;

// Disk ioctl opcodes (_IOW(0xbc, N, struct))
type DiskAddOpcode         = WriteOpcode<0xbc, 4,  bch_ioctl_disk>;
type DiskAddV2Opcode       = WriteOpcode<0xbc, 23, bch_ioctl_disk_v2>;
type DiskRemoveOpcode      = WriteOpcode<0xbc, 5,  bch_ioctl_disk>;
type DiskRemoveV2Opcode    = WriteOpcode<0xbc, 24, bch_ioctl_disk_v2>;
type DiskOnlineOpcode      = WriteOpcode<0xbc, 6,  bch_ioctl_disk>;
type DiskOnlineV2Opcode    = WriteOpcode<0xbc, 25, bch_ioctl_disk_v2>;
type DiskOfflineOpcode     = WriteOpcode<0xbc, 7,  bch_ioctl_disk>;
type DiskOfflineV2Opcode   = WriteOpcode<0xbc, 26, bch_ioctl_disk_v2>;
type DiskSetStateOpcode    = WriteOpcode<0xbc, 8,  bch_ioctl_disk_set_state>;
type DiskSetStateV2Opcode  = WriteOpcode<0xbc, 22, bch_ioctl_disk_set_state_v2>;
type DiskResizeOpcode      = WriteOpcode<0xbc, 14, bch_ioctl_disk_resize>;
type DiskResizeV2Opcode    = WriteOpcode<0xbc, 27, bch_ioctl_disk_resize_v2>;
type DiskResizeJournalOpcode   = WriteOpcode<0xbc, 15, bch_ioctl_disk_resize_journal>;
type DiskResizeJournalV2Opcode = WriteOpcode<0xbc, 28, bch_ioctl_disk_resize_journal_v2>;

/// A handle to a bcachefs filesystem
/// This can be used to send [`libc::ioctl`] to the underlying filesystem.
pub(crate) struct BcachefsHandle {
    inner: bchfs_handle,
}

impl BcachefsHandle {
    pub(crate) fn sysfs_fd(&self) -> i32 {
        self.inner.sysfs_fd
    }

    pub(crate) fn ioctl_fd_raw(&self) -> i32 {
        self.inner.ioctl_fd
    }

    /// Device index when opened via a block device path; -1 when opened via mount point.
    pub(crate) fn dev_idx(&self) -> i32 {
        self.inner.dev_idx
    }

    /// Opens a bcachefs filesystem and returns its handle
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<Self, BchError> {
        let path = path_to_cstr(path);
        let mut handle = std::mem::MaybeUninit::uninit();
        ret_to_result(unsafe { bcache_fs_open_fallible(path.as_ptr(), handle.as_mut_ptr()) })?;
        let inner = unsafe { handle.assume_init() };
        Ok(Self { inner })
    }

    fn ioctl_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.inner.ioctl_fd) }
    }

    /// Try a v2 ioctl (with error message support), falling back to v1
    /// if the kernel returns ENOTTY.
    fn subvol_ioctl<V2: CompileTimeOpcode, V1: CompileTimeOpcode>(
        &self,
        flags: u32,
        dirfd: u32,
        mode: u16,
        dst_ptr: u64,
        src_ptr: u64,
    ) -> Result<(), Errno> {
        let mut err_buf = [0u8; 8192];
        let mut arg = bch_ioctl_subvolume_v2 {
            flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default()
        };
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<V2, _>::new(arg)) } {
            Ok(()) => return Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {}
            Err(e) => {
                let msg = String::from_utf8_lossy(&err_buf);
                let msg = msg.trim_end_matches('\0');
                if !msg.is_empty() {
                    eprintln!("ioctl error: {}", msg);
                }
                return Err(Errno(e.raw_os_error()));
            }
        }

        // Fallback: v1 ioctl without error message support
        let arg = bch_ioctl_subvolume {
            flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default()
        };
        unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<V1, _>::new(arg)) }
            .map_err(|e| Errno(e.raw_os_error()))
    }

    /// Create a subvolume for this bcachefs filesystem
    /// at the given path
    pub fn create_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolCreateV2Opcode, SubvolCreateOpcode>(
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0,
        )
    }

    /// Delete the subvolume at the given path
    /// for this bcachefs filesystem
    pub fn delete_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolDestroyV2Opcode, SubvolDestroyOpcode>(
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0,
        )
    }

    /// Snapshot a subvolume for this bcachefs filesystem
    /// at the given path
    pub fn snapshot_subvolume<P: AsRef<Path>>(
        &self,
        extra_flags: u32,
        src: Option<P>,
        dst: P,
    ) -> Result<(), Errno> {
        let src = src.map(|src| path_to_cstr(src));
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolCreateV2Opcode, SubvolCreateOpcode>(
            BCH_SUBVOL_SNAPSHOT_CREATE | extra_flags,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            src.as_ref().map_or(0, |x| x.as_ptr() as u64),
        )
    }

    // --- Disk ioctl helpers ---

    /// Try a v2 disk ioctl (with error message), falling back to v1 on ENOTTY.
    fn disk_ioctl<V2: CompileTimeOpcode, V1: CompileTimeOpcode>(
        &self,
        flags: u32,
        dev: u64,
    ) -> Result<(), Errno> {
        let mut err_buf = [0u8; 8192];
        let mut arg = bch_ioctl_disk_v2 {
            flags, dev, ..Default::default()
        };
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<V2, _>::new(arg)) } {
            Ok(()) => return Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {}
            Err(e) => {
                print_errmsg(&err_buf);
                return Err(Errno(e.raw_os_error()));
            }
        }

        let arg = bch_ioctl_disk { flags, dev, ..Default::default() };
        unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<V1, _>::new(arg)) }
            .map_err(|e| Errno(e.raw_os_error()))
    }

    /// Add a device to this filesystem.
    pub(crate) fn disk_add(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<DiskAddV2Opcode, DiskAddOpcode>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Remove a device (by index) from this filesystem.
    pub(crate) fn disk_remove(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<DiskRemoveV2Opcode, DiskRemoveOpcode>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Re-add an offline device to this filesystem.
    pub(crate) fn disk_online(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<DiskOnlineV2Opcode, DiskOnlineOpcode>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Take a device offline without removing it.
    pub(crate) fn disk_offline(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<DiskOfflineV2Opcode, DiskOfflineOpcode>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Change device state (rw, ro, evacuating, spare).
    pub(crate) fn disk_set_state(&self, dev_idx: u32, new_state: u32, flags: u32) -> Result<(), Errno> {
        let mut err_buf = [0u8; 8192];
        let mut arg = bch_ioctl_disk_set_state_v2 {
            flags: flags | BCH_BY_INDEX,
            new_state: new_state as u8,
            dev: dev_idx as u64,
            ..Default::default()
        };
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskSetStateV2Opcode, _>::new(arg)) } {
            Ok(()) => return Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {}
            Err(e) => {
                print_errmsg(&err_buf);
                return Err(Errno(e.raw_os_error()));
            }
        }

        let arg = bch_ioctl_disk_set_state {
            flags: flags | BCH_BY_INDEX,
            new_state: new_state as u8,
            dev: dev_idx as u64,
            ..Default::default()
        };
        unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskSetStateOpcode, _>::new(arg)) }
            .map_err(|e| Errno(e.raw_os_error()))
    }

    /// Resize filesystem on a device.
    pub(crate) fn disk_resize(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        let mut err_buf = [0u8; 8192];
        let mut arg = bch_ioctl_disk_resize_v2 {
            flags: BCH_BY_INDEX,
            dev: dev_idx as u64,
            nbuckets,
            ..Default::default()
        };
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskResizeV2Opcode, _>::new(arg)) } {
            Ok(()) => return Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {}
            Err(e) => {
                print_errmsg(&err_buf);
                return Err(Errno(e.raw_os_error()));
            }
        }

        let arg = bch_ioctl_disk_resize {
            flags: BCH_BY_INDEX,
            dev: dev_idx as u64,
            nbuckets,
            ..Default::default()
        };
        unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskResizeOpcode, _>::new(arg)) }
            .map_err(|e| Errno(e.raw_os_error()))
    }

    /// Resize journal on a device.
    pub(crate) fn disk_resize_journal(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        let mut err_buf = [0u8; 8192];
        let mut arg = bch_ioctl_disk_resize_journal_v2 {
            flags: BCH_BY_INDEX,
            dev: dev_idx as u64,
            nbuckets,
            ..Default::default()
        };
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskResizeJournalV2Opcode, _>::new(arg)) } {
            Ok(()) => return Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {}
            Err(e) => {
                print_errmsg(&err_buf);
                return Err(Errno(e.raw_os_error()));
            }
        }

        let arg = bch_ioctl_disk_resize_journal {
            flags: BCH_BY_INDEX,
            dev: dev_idx as u64,
            nbuckets,
            ..Default::default()
        };
        unsafe { ioctl::ioctl(self.ioctl_fd(), Setter::<DiskResizeJournalOpcode, _>::new(arg)) }
            .map_err(|e| Errno(e.raw_os_error()))
    }
}

fn print_errmsg(err_buf: &[u8]) {
    let len = err_buf.iter().position(|&b| b == 0).unwrap_or(err_buf.len());
    if len > 0 {
        let msg = String::from_utf8_lossy(&err_buf[..len]);
        eprintln!("ioctl error: {}", msg);
    }
}

impl Drop for BcachefsHandle {
    fn drop(&mut self) {
        unsafe { bcache_fs_close(self.inner) };
    }
}
