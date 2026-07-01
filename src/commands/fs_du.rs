// bcachefs fs du: disk usage statistics via reverse-mapping

use anyhow::{Context, Result};
use bch_bindgen::accounting::data_type_from_u8;
use bch_bindgen::c::{bch_ioctl_rev_map, BCH_REV_MAP_RESOLVE_PATH};
use bcachefs_kernel::btree;
use bcachefs_kernel::opts::prt_data_type;
use bcachefs_kernel::util::printbuf::Printbuf;
use clap::Parser;

use crate::commands::device;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_wr;

const BCH_IOCTL_REV_MAP_NR: u32 = 38;

#[derive(Parser, Debug)]
#[command(name = "du", about = "Disk usage statistics")]
pub struct Cli {
    /// Filesystem path or device
    filesystem: String,

    /// Probe a specific device offset: --probe <DEV> <OFFSET>.
    /// DEV may be a numeric device index or a device path (e.g. /dev/vdb1).
    /// Hidden: single-offset BCH_IOCTL_REV_MAP diagnostic, not part of the
    /// user-facing du surface.
    #[arg(long, value_names = ["DEV", "OFFSET"], num_args = 2, hide = true)]
    probe: Vec<String>,
}

fn fs_du(cli: Cli) -> Result<()> {
    let handle = BcachefsHandle::open(&cli.filesystem)
        .with_context(|| format!("opening filesystem '{}'", cli.filesystem))?;

    if !cli.probe.is_empty() {
        let dev = device::resolve_dev(&handle, &cli.probe[0])?;
        let offset: u64 = cli.probe[1].parse()
            .context("offset must be a number")?;

        probe_offset(&handle, dev, offset)?;
    } else {
        // Future: sampling-based du
        println!("Full sampling-based du not yet implemented. Use --probe <dev> <offset> to test the ioctl.");
    }

    Ok(())
}

fn probe_offset(handle: &BcachefsHandle, dev: u32, offset: u64) -> Result<()> {
    const PATH_BUF_SIZE: usize = 4096;
    let mut path_buf = vec![0u8; PATH_BUF_SIZE];

    let mut arg = bch_ioctl_rev_map {
        dev,
        flags: BCH_REV_MAP_RESOLVE_PATH,
        offset,
        buf: path_buf.as_mut_ptr() as u64,
        buf_size: path_buf.len() as u32,
        ..Default::default()
    };

    handle.rev_map(&mut arg)
        .map_err(|e| anyhow::anyhow!("rev_map ioctl failed: {}", e))?;

    if arg.found == 0 {
        println!("dev {} offset {}: free space", dev, offset);
        return Ok(());
    }

    // Found
    println!("dev {} offset {}: found", dev, offset);
    println!("  data_type: {} ({})",
        arg.data_type,
        data_type_name(arg.data_type));
    println!("  btree_id:  {} ({})",
        arg.btree_id,
        btree::types::btree_id_str(arg.btree_id.into()));
    println!("  ec:        {}", arg.ec);
    println!("  snapshot:  {}", arg.snapshot);
    println!("  inode:     {}", arg.inode);
    println!("  sectors:   {}", arg.sectors);
    println!("  background_target: {}", arg.background_target);

    // Extract path (if resolved)
    if let Some(nul_pos) = path_buf.iter().position(|&b| b == 0) {
        if nul_pos > 0 {
            let path = String::from_utf8_lossy(&path_buf[..nul_pos]);
            println!("  path:      {}", path);
        }
    }

    Ok(())
}

/// Format a raw `data_type` byte via the C `__bch2_data_types[]` table
/// (same formatting `fs usage` uses), so names can't drift from the kernel.
pub(super) fn data_type_name(dt: u8) -> String {
    let mut pb = Printbuf::new();
    prt_data_type(&mut pb, data_type_from_u8(dt));
    pb.as_str().to_string()
}

pub const CMD: super::CmdDef = typed_cmd!("du", "Disk usage statistics", Cli, fs_du);
