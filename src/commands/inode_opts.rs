use anyhow::Result;
use std::fs;

// Re-export for clap
pub use crate::commands::inode_opts_device::Cli;

pub fn inode_opts(argv: Vec<String>) -> Result<()> {
    // Check first positional arg to determine mode
    // If it's a directory, use mounted (debugfs) mode
    // Otherwise use device (native btree) mode
    if argv.len() > 1 {
        let path = &argv[argv.len() - 1];
        if let Ok(meta) = fs::metadata(path) {
            if meta.is_dir() {
                return crate::commands::inode_opts_mounted::inode_opts_mounted(argv);
            }
        }
    }
    crate::commands::inode_opts_device::inode_opts_device(argv)
}
