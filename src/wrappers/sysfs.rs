use std::fs;
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
