use std::mem::MaybeUninit;
use std::path::Path;

use bch_bindgen::c::{
    bcache_fs_close, bcache_fs_open_fallible, bch_ioctl_err_msg, bch_ioctl_subvolume,
    bch_ioctl_subvolume_v2, bchfs_handle, BCH_IOCTL_SUBVOLUME_CREATE_v2,
    BCH_IOCTL_SUBVOLUME_DESTROY_v2, BCH_IOCTL_SUBVOLUME_CREATE, BCH_IOCTL_SUBVOLUME_DESTROY,
    BCH_SUBVOL_SNAPSHOT_CREATE,
};
use bch_bindgen::errcode::{BchError, ret_to_result};
use bch_bindgen::path_to_cstr;
use errno::Errno;

/// A handle to a bcachefs filesystem
/// This can be used to send [`libc::ioctl`] to the underlying filesystem.
pub(crate) struct BcachefsHandle {
    inner: bchfs_handle,
}

impl BcachefsHandle {
    pub(crate) fn sysfs_fd(&self) -> i32 {
        self.inner.sysfs_fd
    }

    /// Opens a bcachefs filesystem and returns its handle
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<Self, BchError> {
        let path = path_to_cstr(path);
        let mut handle: MaybeUninit<bchfs_handle> = MaybeUninit::uninit();
        ret_to_result(unsafe { bcache_fs_open_fallible(path.as_ptr(), handle.as_mut_ptr()) })?;
        let inner = unsafe { handle.assume_init() };
        Ok(Self { inner })
    }
}

/// I/O control commands that can be sent to a bcachefs filesystem
/// Those are non-exhaustive
#[repr(u32)]
#[non_exhaustive]
#[derive(Copy, Clone, Debug)]
pub enum BcachefsIoctl {
    SubvolumeCreate     = BCH_IOCTL_SUBVOLUME_CREATE,
    SubvolumeCreate2    = BCH_IOCTL_SUBVOLUME_CREATE_v2,
    SubvolumeDestroy    = BCH_IOCTL_SUBVOLUME_DESTROY,
    SubvolumeDestroy2   = BCH_IOCTL_SUBVOLUME_DESTROY_v2,
}

/// I/O control commands payloads
#[non_exhaustive]
pub enum BcachefsIoctlPayload {
    Subvolume(bch_ioctl_subvolume),
    Subvolume2(bch_ioctl_subvolume_v2),
}

impl From<&BcachefsIoctlPayload> for *const libc::c_void {
    fn from(value: &BcachefsIoctlPayload) -> Self {
        match value {
            BcachefsIoctlPayload::Subvolume(p) => (p as *const bch_ioctl_subvolume).cast(),
            BcachefsIoctlPayload::Subvolume2(p) => (p as *const bch_ioctl_subvolume_v2).cast(),
        }
    }
}

impl BcachefsHandle {
    /// Type-safe [`libc::ioctl`] for bcachefs filesystems
    unsafe fn __ioctl(
        &self,
        request: BcachefsIoctl,
        payload: *const libc::c_void,
    ) -> Result<(), Errno> {
        let payload_ptr: *const libc::c_void = payload.into();
        let ret = libc::ioctl(self.inner.ioctl_fd, request as libc::Ioctl, payload_ptr);

        if ret == -1 {
            Err(errno::errno())
        } else {
            Ok(())
        }
    }

    pub fn ioctl(
        &self,
        request: BcachefsIoctl,
        payload: &BcachefsIoctlPayload,
    ) -> Result<(), Errno> {
        unsafe { self.__ioctl(request, payload.into()) }
    }

    fn errmsg_ioctl(
        &self,
        request: BcachefsIoctl,
        payload: *const libc::c_void,
        errmsg: &mut bch_ioctl_err_msg
    ) -> Result<(), Errno> {
        let mut err: [u8; 8192] = [0; 8192];
        errmsg.msg_ptr = err.as_mut_ptr() as u64;
        errmsg.msg_len = err.len() as u32;

        let ret = unsafe { self.__ioctl(request, payload) };
        if ret.is_err() && ret != Err(errno::Errno(libc::ENOTTY)) {
            println!("{:?} error: {}\n", request, String::from_utf8_lossy(&err));
        }

        ret
    }

    fn subvol_ioctl(
        &self,
        new_nr: BcachefsIoctl,
        old_nr: BcachefsIoctl,
        flags: u32,
        dirfd: u32,
        mode: u16,
        dst_ptr: u64,
        src_ptr: u64
    ) -> Result<(), Errno> {
        let mut arg = bch_ioctl_subvolume_v2 {
            flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default()
        };

        let ret = self.errmsg_ioctl(
            new_nr,
            &arg as *const bch_ioctl_subvolume_v2 as *const libc::c_void,
            &mut arg.err,
        );
        if ret != Err(errno::Errno(libc::ENOTTY)) {
            ret
        } else {
            self.ioctl(
                old_nr,
                &BcachefsIoctlPayload::Subvolume(bch_ioctl_subvolume {
                    flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default()
                }),
            )
        }
    }


    /// Create a subvolume for this bcachefs filesystem
    /// at the given path
    pub fn create_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);

        self.subvol_ioctl(
            BcachefsIoctl::SubvolumeCreate2,
            BcachefsIoctl::SubvolumeCreate,
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0
        )
    }

    /// Delete the subvolume at the given path
    /// for this bcachefs filesystem
    pub fn delete_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);

        self.subvol_ioctl(
            BcachefsIoctl::SubvolumeDestroy2,
            BcachefsIoctl::SubvolumeDestroy,
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0
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

        let ret = self.subvol_ioctl(
            BcachefsIoctl::SubvolumeCreate2,
            BcachefsIoctl::SubvolumeCreate,
            BCH_SUBVOL_SNAPSHOT_CREATE | extra_flags,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            src.as_ref().map_or(0, |x| x.as_ptr() as u64)
        );

        drop(src);
        drop(dst);
        ret
    }
}

impl Drop for BcachefsHandle {
    fn drop(&mut self) {
        unsafe { bcache_fs_close(self.inner) };
    }
}
