use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::device_multipath::preferred_multipath_devnode_for_block_name;

/// Selects how mounted device names are rendered in human-facing output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceNameMode {
    /// Show the raw kernel block name from sysfs, such as `dm-0` or `sda`.
    Raw,
    /// Show the mapper basename for dm-multipath devices when available.
    Mapper,
}

impl DeviceNameMode {
    /// Convert the command-line `--mapper-names` boolean into a display mode.
    pub fn from_mapper_names(mapper_names: bool) -> Self {
        if mapper_names {
            Self::Mapper
        } else {
            Self::Raw
        }
    }
}

/// Resolve the block device name for a bcachefs sysfs device directory.
///
/// Reads the `block` symlink at `dev_sysfs_path/block` (e.g.
/// `/sys/fs/bcachefs/<UUID>/dev-0/block`) and returns the basename
/// of the target (e.g. "sda"). Falls back to the directory name
/// (e.g. "dev-0") if the symlink is absent (offline device).
pub fn dev_name_from_sysfs(dev_sysfs_path: &Path) -> String {
    if let Ok(target) = fs::read_link(dev_sysfs_path.join("block")) {
        if let Some(name) = target.file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    dev_sysfs_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Resolve the block device display name for a bcachefs sysfs device directory.
///
/// Returns the raw kernel name by default, or the mapper basename for
/// dm-multipath devices when requested and available. This is intended for
/// human output, not for opening devices.
pub fn dev_display_name_from_sysfs(dev_sysfs_path: &Path, mode: DeviceNameMode) -> String {
    let name = dev_name_from_sysfs(dev_sysfs_path);
    if mode == DeviceNameMode::Raw {
        return name;
    }

    if let Some(path) = preferred_multipath_devnode_for_block_name(&name) {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(name)
    } else {
        name
    }
}

pub fn sysfs_path_from_fd(fd: BorrowedFd<'_>) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    let link = format!("/proc/self/fd/{}", raw);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", raw))
}

/// Read a sysfs attribute as a u64.
pub fn read_sysfs_u64(path: &Path) -> io::Result<u64> {
    let s = fs::read_to_string(path)?;
    s.trim().parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("{}: {:?}", e, s.trim())))
}

/// Read a sysfs attribute as a string, relative to a directory fd.
pub fn read_sysfs_fd_str(dirfd: BorrowedFd<'_>, path: &str) -> io::Result<String> {
    let flags = rustix::fs::OFlags::RDONLY;
    let fd = rustix::fs::openat(dirfd, path, flags, rustix::fs::Mode::empty())?;
    let mut buf = [0u8; 256];
    let n = rustix::io::read(&fd, &mut buf)?;
    let s = std::str::from_utf8(&buf[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(s.trim().to_string())
}


const KERNEL_VERSION_PATH: &str = "/sys/module/bcachefs/parameters/version";

/// Read the bcachefs kernel module metadata version.
/// Returns 0 if the module isn't loaded.
pub fn bcachefs_kernel_version() -> u64 {
    read_sysfs_u64(Path::new(KERNEL_VERSION_PATH)).unwrap_or(0)
}

/// Write a string value to a sysfs attribute file relative to a directory fd.
pub fn sysfs_write_str(sysfs_fd: BorrowedFd<'_>, path: &str, value: &str) {
    let flags = rustix::fs::OFlags::WRONLY;
    if let Ok(fd) = rustix::fs::openat(sysfs_fd, path, flags, rustix::fs::Mode::empty()) {
        let _ = rustix::io::write(&fd, value.as_bytes());
    }
}

/// Info about a device in a mounted bcachefs filesystem, read from sysfs.
#[derive(Clone)]
pub struct DevInfo {
    pub idx:        u32,
    /// Human-facing device name selected by [`DeviceNameMode`].
    pub dev:        String,
    pub label:      Option<String>,
    pub durability: u32,
    pub online:     bool,
}

/// Enumerate devices for a mounted filesystem from its sysfs directory.
///
/// Reads `dev-N/` subdirectories under `sysfs_path`, extracting the display
/// device name for each.
pub fn fs_get_devices(
    sysfs_path: &Path,
    name_mode: DeviceNameMode,
) -> Result<Vec<DevInfo>> {
    let mut devs = Vec::new();
    for entry in fs::read_dir(sysfs_path)
        .with_context(|| format!("reading sysfs dir {}", sysfs_path.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let idx: u32 = match name.strip_prefix("dev-").and_then(|s| s.parse().ok()) {
            Some(i) => i,
            None => continue,
        };

        let dev_path = entry.path();
        // metadata() follows the symlink (not symlink_metadata): a hot-removed
        // device can leave dev-N/block dangling, and that should read as offline.
        let online = fs::metadata(dev_path.join("block")).is_ok();
        let dev = dev_display_name_from_sysfs(&dev_path, name_mode);

        let label = fs::read_to_string(dev_path.join("label"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let durability = read_sysfs_u64(&dev_path.join("durability"))
            .unwrap_or(1) as u32;

        devs.push(DevInfo { idx, dev, label, durability, online });
    }
    devs.sort_by_key(|d| d.idx);
    Ok(devs)
}

/// Per-device IO totals from `dev-N/io_done`, in bytes.
///
/// The kernel emits JSON (`{"read": {...}, "write": {...}}`) with exact
/// `%llu` values, so unlike the human-readable counter files these lose no
/// precision.
#[derive(Deserialize)]
struct IoDone {
    read:  HashMap<String, u64>,
    write: HashMap<String, u64>,
}

/// Bytes read and written on one device for one data type.
pub struct DevIoEntry {
    pub dev:         String,
    pub data_type:   String,
    pub read_bytes:  u64,
    pub write_bytes: u64,
}

impl DevIoEntry {
    pub fn label(&self) -> String {
        format!("{}/{}", self.dev, self.data_type)
    }
}

/// Read every device's `io_done`, skipping data types with no traffic.
///
/// Only a handful of the data types the kernel prints are ever accounted:
/// reads are always attributed to `user` and writes to `user`, `btree`,
/// `journal`, `sb` or the stripe types. The rest stay zero and are dropped.
pub fn read_device_io(sysfs_path: &Path, name_mode: DeviceNameMode) -> Vec<DevIoEntry> {
    let mut entries = Vec::new();
    let Ok(dir) = fs::read_dir(sysfs_path) else { return entries };

    for entry in dir.flatten() {
        let dirname = entry.file_name().to_string_lossy().into_owned();
        if !dirname.starts_with("dev-") { continue }

        let dev_path = entry.path();
        let dev_name = dev_display_name_from_sysfs(&dev_path, name_mode);

        let io_done_path = dev_path.join("io_done");
        let Ok(content) = fs::read_to_string(&io_done_path) else { continue };
        let Ok(io_done) = serde_json::from_str::<IoDone>(&content) else { continue };

        for (dtype, &r) in &io_done.read {
            let w = io_done.write.get(dtype).copied().unwrap_or(0);
            if r != 0 || w != 0 {
                entries.push(DevIoEntry {
                    dev:         dev_name.clone(),
                    data_type:   dtype.clone(),
                    read_bytes:  r,
                    write_bytes: w,
                });
            }
        }
    }
    entries.sort_by_key(|e| e.label());
    entries
}
