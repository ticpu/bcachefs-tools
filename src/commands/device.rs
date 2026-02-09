use anyhow::{anyhow, Result};
use bch_bindgen::c::BCH_FORCE_IF_DEGRADED;
use bch_bindgen::path_to_cstr;
use clap::Parser;

use crate::wrappers::handle::BcachefsHandle;

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

    let handle = BcachefsHandle::open(&cli.device)
        .map_err(|e| anyhow!("Failed to open filesystem for '{}': {}", cli.device, e))?;

    let dev_idx = handle.dev_idx();
    if dev_idx < 0 {
        return Err(anyhow!("'{}' does not appear to be a block device member", cli.device));
    }

    let flags = if cli.force { BCH_FORCE_IF_DEGRADED } else { 0 };
    handle.disk_offline(dev_idx as u32, flags)
        .map_err(|e| anyhow!("Failed to offline device '{}': {}", cli.device, e))
}
