use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve the block device name for a bcachefs sysfs device directory.
///
/// Reads the `block` symlink at `dev_sysfs_path/block` (e.g.
/// `/sys/fs/bcachefs/<UUID>/dev-0/block`) and returns the basename
/// of the target (e.g. "sda"). Falls back to the directory name
/// (e.g. "dev-0") if the symlink is absent (offline device).
pub fn dev_name_from_sysfs(dev_sysfs_path: &Path) -> String {
    if let Ok(target) = fs::read_link(dev_sysfs_path.join("block")) {
        if let Some(name) = target.file_name() {
            return name.to_string_lossy().to_string();
        }
    }
    dev_sysfs_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn sysfs_path_from_fd(fd: i32) -> Result<PathBuf> {
    let link = format!("/proc/self/fd/{}", fd);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", fd))
}

/// Read a sysfs attribute as a u64.
pub fn read_sysfs_u64(path: &Path) -> io::Result<u64> {
    let s = fs::read_to_string(path)?;
    s.trim().parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("{}: {:?}", e, s.trim())))
}

const KERNEL_VERSION_PATH: &str = "/sys/module/bcachefs/parameters/version";

/// Read the bcachefs kernel module metadata version.
/// Returns 0 if the module isn't loaded.
pub fn bcachefs_kernel_version() -> u64 {
    read_sysfs_u64(Path::new(KERNEL_VERSION_PATH)).unwrap_or(0)
}

/// Info about a device in a mounted bcachefs filesystem, read from sysfs.
pub struct DevInfo {
    pub idx:        u32,
    pub dev:        String,
}

/// Enumerate devices for a mounted filesystem from its sysfs directory.
///
/// Reads `dev-N/` subdirectories under `sysfs_path`, extracting the block
/// device name (from the `block` symlink) for each.
pub fn fs_get_devices(sysfs_path: &Path) -> Result<Vec<DevInfo>> {
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
        let dev = dev_name_from_sysfs(&dev_path);

        devs.push(DevInfo { idx, dev });
    }
    devs.sort_by_key(|d| d.idx);
    Ok(devs)
}
