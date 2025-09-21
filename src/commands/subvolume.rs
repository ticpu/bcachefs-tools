use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use bch_bindgen::c::BCH_SUBVOL_SNAPSHOT_RO;
use clap::{Parser, Subcommand};

use crate::wrappers::handle::BcachefsHandle;

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

/// Subvolumes-related commands
#[derive(Subcommand, Debug)]
enum Subcommands {
    #[command(visible_aliases = ["new"])]
    Create {
        /// Paths
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    #[command(visible_aliases = ["del"])]
    Delete {
        /// Path
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    #[command(allow_missing_positional = true, visible_aliases = ["snap"])]
    Snapshot {
        /// Make snapshot read only
        #[arg(long, short)]
        read_only: bool,
        source:    Option<PathBuf>,
        dest:      PathBuf,
    },
}

pub fn subvolume(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    match cli.subcommands {
        Subcommands::Create { targets } => {
            for target in targets {
                let target = if target.is_absolute() {
                    target
                } else {
                    env::current_dir()
                        .map(|p| p.join(target))
                        .context("unable to get current directory")?
                };

                if let Some(dirname) = target.parent() {
                    let fs =
                        BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
                    fs.create_subvolume(target)
                        .context("Failed to create the subvolume")?;
                }
            }
        }
        Subcommands::Delete { targets } => {
            for target in targets {
                let target = target
                    .canonicalize()
                    .context("subvolume path does not exist or can not be canonicalized")?;

                if let Some(dirname) = target.parent() {
                    let fs =
                        BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
                    fs.delete_subvolume(target)
                        .context("Failed to delete the subvolume")?;
                }
            }
        }
        Subcommands::Snapshot {
            read_only,
            source,
            dest,
        } => {
            if let Some(dirname) = dest.parent() {
                let dot = PathBuf::from(".");
                let dir = if dirname.as_os_str().is_empty() {
                    &dot
                } else {
                    dirname
                };
                let fs = BcachefsHandle::open(dir).context("Failed to open the filesystem")?;

                fs.snapshot_subvolume(
                    if read_only {
                        BCH_SUBVOL_SNAPSHOT_RO
                    } else {
                        0x0
                    },
                    source,
                    dest,
                )
                .context("Failed to snapshot the subvolume")?;
            }
        }
    }

    Ok(())
}
