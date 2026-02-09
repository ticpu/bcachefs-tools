use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bch_bindgen::c::{
    bch_data_type::*,
    bch_member_state::*,
    bcachefs_metadata_version::bcachefs_metadata_version_reconcile,
    BCH_FORCE_IF_DATA_LOST, BCH_FORCE_IF_DEGRADED, BCH_FORCE_IF_METADATA_LOST,
};
use bch_bindgen::path_to_cstr;
use clap::Parser;

use crate::util::fmt_bytes_human;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::bcachefs_kernel_version;

/// Open a filesystem by block device path and return its handle + device index.
fn open_dev(path: &str) -> Result<(BcachefsHandle, u32)> {
    let handle = BcachefsHandle::open(path)
        .map_err(|e| anyhow!("Failed to open filesystem for '{}': {}", path, e))?;
    let dev_idx = handle.dev_idx();
    if dev_idx < 0 {
        return Err(anyhow!("'{}' does not appear to be a block device member", path));
    }
    Ok((handle, dev_idx as u32))
}

/// Resolve a device identifier (path or numeric index) to a dev_idx
/// on a given filesystem handle.
fn resolve_dev(handle: &BcachefsHandle, dev_str: &str) -> Result<u32> {
    // If it's a plain number, use it as a dev index directly
    if let Ok(idx) = dev_str.parse::<u32>() {
        return Ok(idx);
    }

    // Otherwise, open the device and compare UUIDs
    let dev_handle = BcachefsHandle::open(dev_str)
        .map_err(|e| anyhow!("Error opening '{}': {}", dev_str, e))?;

    if handle.uuid() != dev_handle.uuid() {
        return Err(anyhow!("{} does not appear to be a member of this filesystem", dev_str));
    }

    let idx = dev_handle.dev_idx();
    if idx < 0 {
        return Err(anyhow!("Could not determine device index for '{}'", dev_str));
    }
    Ok(idx as u32)
}

#[derive(Parser, Debug)]
#[command(about = "Re-add a device to a running filesystem")]
pub struct OnlineCli {
    /// Device path
    device: String,
}

pub fn cmd_device_online(argv: Vec<String>) -> Result<()> {
    let cli = OnlineCli::parse_from(argv);

    let handle = BcachefsHandle::open(&cli.device)
        .map_err(|e| anyhow!("Failed to open filesystem for '{}': {}", cli.device, e))?;

    let dev_path = path_to_cstr(&cli.device);
    handle.disk_online(&dev_path)
        .map_err(|e| anyhow!("Failed to online device '{}': {}", cli.device, e))
}

#[derive(Parser, Debug)]
#[command(about = "Take a device offline, without removing it")]
pub struct OfflineCli {
    /// Force, if data redundancy will be degraded
    #[arg(short, long)]
    force: bool,

    /// Device path
    device: String,
}

pub fn cmd_device_offline(argv: Vec<String>) -> Result<()> {
    let cli = OfflineCli::parse_from(argv);
    let (handle, dev_idx) = open_dev(&cli.device)?;

    let flags = if cli.force { BCH_FORCE_IF_DEGRADED } else { 0 };
    handle.disk_offline(dev_idx, flags)
        .map_err(|e| anyhow!("Failed to offline device '{}': {}", cli.device, e))
}

#[derive(Parser, Debug)]
#[command(about = "Remove a device from a filesystem")]
pub struct RemoveCli {
    /// Force removal, even if some data couldn't be migrated
    #[arg(short, long)]
    force: bool,

    /// Force removal, even if some metadata couldn't be migrated
    #[arg(short = 'F', long)]
    force_metadata: bool,

    /// Device path or numeric device index
    device: String,

    /// Filesystem path (required when specifying device by index)
    path: Option<String>,
}

pub fn cmd_device_remove(argv: Vec<String>) -> Result<()> {
    let cli = RemoveCli::parse_from(argv);

    let mut flags = BCH_FORCE_IF_DEGRADED;
    if cli.force {
        flags |= BCH_FORCE_IF_DATA_LOST;
    }
    if cli.force_metadata {
        flags |= BCH_FORCE_IF_METADATA_LOST;
    }

    let is_numeric = cli.device.parse::<u32>().is_ok();

    let (handle, dev_idx) = if let Some(ref fs_path) = cli.path {
        let handle = BcachefsHandle::open(fs_path)
            .map_err(|e| anyhow!("Failed to open filesystem '{}': {}", fs_path, e))?;
        let dev_idx = resolve_dev(&handle, &cli.device)?;
        (handle, dev_idx)
    } else if !is_numeric {
        open_dev(&cli.device)?
    } else {
        return Err(anyhow!("Filesystem path required when specifying device by index"));
    };

    handle.disk_remove(dev_idx, flags)
        .map_err(|e| anyhow!("Failed to remove device '{}': {}", cli.device, e))
}

fn data_type_is_empty(t: u32) -> bool {
    t == BCH_DATA_free as u32 ||
    t == BCH_DATA_need_gc_gens as u32 ||
    t == BCH_DATA_need_discard as u32
}

fn data_type_is_hidden(t: u32) -> bool {
    t == BCH_DATA_sb as u32 ||
    t == BCH_DATA_journal as u32
}

#[derive(Parser, Debug)]
#[command(about = "Migrate data off a device")]
pub struct EvacuateCli {
    /// Device path
    device: String,
}

pub fn cmd_device_evacuate(argv: Vec<String>) -> Result<()> {
    let cli = EvacuateCli::parse_from(argv);

    if bcachefs_kernel_version() < bcachefs_metadata_version_reconcile as u64 {
        return Err(anyhow!(
            "Kernel too old for Rust evacuate path; \
             need bcachefs metadata version >= {} (reconcile)",
            bcachefs_metadata_version_reconcile as u64
        ));
    }

    let (handle, dev_idx) = open_dev(&cli.device)?;

    let usage = handle.dev_usage(dev_idx)
        .map_err(|e| anyhow!("Failed to query device usage: {}", e))?;

    if usage.state == BCH_MEMBER_STATE_rw as u8 {
        println!("Setting {} readonly", cli.device);
        handle.disk_set_state(dev_idx, BCH_MEMBER_STATE_ro as u32, BCH_FORCE_IF_DEGRADED)
            .map_err(|e| anyhow!("Failed to set device readonly: {}", e))?;
    }

    println!("Setting {} evacuating", cli.device);
    handle.disk_set_state(dev_idx, BCH_MEMBER_STATE_evacuating as u32, BCH_FORCE_IF_DEGRADED)
        .map_err(|e| anyhow!("Failed to set device evacuating: {}", e))?;

    loop {
        let usage = handle.dev_usage(dev_idx)
            .map_err(|e| anyhow!("Failed to query device usage: {}", e))?;

        let mut data_sectors: u64 = 0;
        for (i, dt) in usage.data_types.iter().enumerate() {
            if !data_type_is_empty(i as u32) && !data_type_is_hidden(i as u32) {
                data_sectors += dt.sectors;
            }
        }

        print!("\x1b[2K\r{}", fmt_bytes_human(data_sectors << 9));
        io::stdout().flush().ok();

        if data_sectors == 0 {
            println!();
            return Ok(());
        }

        thread::sleep(Duration::from_secs(1));
    }
}
