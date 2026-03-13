// SPDX-License-Identifier: GPL-2.0

//! Block device utilities.
//!
//! Pure Rust replacements for get_size(), get_blocksize(), fd_to_dev_model()
//! from tools-util.c. These work on any fd (block device or regular file).

use std::os::unix::io::RawFd;

// linux/fs.h ioctl constants not exposed by libc crate
const BLKGETSIZE64: libc::c_ulong = 0x80081272;
const BLKPBSZGET: libc::c_ulong = 0x127b;

/// Returns the size of a file or block device in bytes.
///
/// For block devices, uses BLKGETSIZE64 ioctl.
/// For regular files, returns st_size from fstat.
pub fn get_size(fd: RawFd) -> u64 {
    let stat = fstat(fd);

    if is_blk(stat.st_mode) {
        let mut size: u64 = 0;
        unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size) };
        size
    } else {
        stat.st_size as u64
    }
}

/// Returns the physical block size in bytes.
///
/// For block devices, uses BLKPBSZGET ioctl.
/// For regular files, returns st_blksize from fstat.
pub fn get_blocksize(fd: RawFd) -> u32 {
    let stat = fstat(fd);

    if is_blk(stat.st_mode) {
        let mut bs: libc::c_uint = 0;
        unsafe { libc::ioctl(fd, BLKPBSZGET, &mut bs) };
        bs
    } else {
        stat.st_blksize as u32
    }
}

/// Returns the device model string for a block device fd, or a
/// fallback description for regular files / unknown devices.
pub fn fd_to_dev_model(fd: RawFd) -> String {
    let stat = fstat(fd);

    if !is_blk(stat.st_mode) {
        return "(image file)".to_string();
    }

    let major = libc::major(stat.st_rdev);
    let minor = libc::minor(stat.st_rdev);
    let sysfs = format!("/sys/dev/block/{}:{}", major, minor);

    // Try device/model, then parent's device/model (partition),
    // then loop/backing_file
    for suffix in &["device/model", "../device/model", "loop/backing_file"] {
        let path = format!("{}/{}", sysfs, suffix);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let trimmed = contents.trim_end_matches('\n');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    "(unknown model)".to_string()
}

/// Returns true if the block device is non-rotational (SSD).
///
/// For regular files, returns false.
pub fn nonrot(fd: RawFd) -> bool {
    let stat = fstat(fd);

    if !is_blk(stat.st_mode) {
        return false;
    }

    let major = libc::major(stat.st_rdev);
    let minor = libc::minor(stat.st_rdev);
    let path = format!("/sys/dev/block/{}:{}/queue/rotational", major, minor);

    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim() == "0",
        Err(_) => false,
    }
}

fn fstat(fd: RawFd) -> libc::stat {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstat(fd, &mut stat) };
    if ret != 0 {
        crate::wrappers::super_io::die("stat error");
    }
    stat
}

fn is_blk(mode: libc::mode_t) -> bool {
    (mode & libc::S_IFMT) == libc::S_IFBLK
}
