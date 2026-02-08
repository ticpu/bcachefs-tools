use std::os::fd::BorrowedFd;
use std::path::Path;

use bch_bindgen::c::{
    bcache_fs_close, bcache_fs_open_fallible, bch_ioctl_subvolume,
    bch_ioctl_subvolume_v2, bchfs_handle,
    BCH_SUBVOL_SNAPSHOT_CREATE,
};
use bch_bindgen::errcode::{BchError, ret_to_result};
use bch_bindgen::path_to_cstr;
use errno::Errno;
use rustix::ioctl::{self, CompileTimeOpcode, Setter, WriteOpcode};

/// Opcodes for subvolume ioctls, computed at compile time.
///
/// These replace the MARK_FIX_753 bindgen workaround — the opcode is
/// built from (group, number, sizeof(struct)) in Rust, matching
/// the kernel's `_IOW(0xbc, N, struct ...)` macros.
type SubvolCreateOpcode    = WriteOpcode<0xbc, 16, bch_ioctl_subvolume>;
type SubvolCreateV2Opcode  = WriteOpcode<0xbc, 29, bch_ioctl_subvolume_v2>;
type SubvolDestroyOpcode   = WriteOpcode<0xbc, 17, bch_ioctl_subvolume>;
type SubvolDestroyV2Opcode = WriteOpcode<0xbc, 30, bch_ioctl_subvolume_v2>;

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
}

impl Drop for BcachefsHandle {
    fn drop(&mut self) {
        unsafe { bcache_fs_close(self.inner) };
    }
}
