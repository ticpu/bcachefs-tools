//! `bcachefs dump` / `undump`.
//!
//! Dump opens the fs with norecovery + nochanges, so the on-disk btrees are
//! never mutated. Iteration still sees not-yet-replayed journal updates through
//! the journal-keys overlay (btree_trans_peek_journal), so the alloc/btree walks
//! are post-replay-equivalent even on an unclean fs -- a crash image with a
//! committed-but-unflushed journal dumps the same live data a mount would recover.
//! Full journal replay is deliberately not run so a damaged fs still opens.
//!
//! Split by concern: `sanitize` holds the sanitize/de-replication buffer
//! transforms and crypto; `collect` holds the range collection, write path, and
//! `dump_fs` orchestration; this module holds the CLI and command dispatch.

mod collect;
mod sanitize;

use std::os::fd::AsFd;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;

use bcachefs_kernel::c;
use bcachefs_kernel::opt_set;

use crate::qcow2;
use crate::wrappers::accounting::data_type;

// ---- Dump CLI ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
enum IncludeBuckets {
    Free,
    NeedGcGens,
    NeedDiscard,
}

impl IncludeBuckets {
    fn as_c(self) -> c::bch_data_type {
        match self {
            Self::Free        => data_type::free,
            Self::NeedGcGens  => data_type::need_gc_gens,
            Self::NeedDiscard => data_type::need_discard,
        }
    }
}

/// Dump filesystem metadata and data to a qcow2 image (use --metadata-only for metadata-only debugging images)
#[derive(Parser, Debug)]
#[command(about = "Dump filesystem metadata and data to a qcow2 image")]
pub struct DumpCli {
    /// Output filename (without .qcow2 extension)
    #[arg(short = 'o')]
    output: String,

    /// Force; overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Dump only metadata (superblock, journal, btree), not user data
    #[arg(short = 'm', long = "metadata-only")]
    metadata_only: bool,

    /// Sanitize inline data and optionally filenames (data or filenames)
    #[arg(short = 's', long, num_args = 0..=1, default_missing_value = "data")]
    sanitize: Option<String>,

    /// Don't dump entire journal, just dirty entries
    #[arg(long)]
    nojournal: bool,

    /// Dump only the lowest-device-index replica of each btree node, rewriting
    /// the other replica pointers to an invalid device. Produces a much smaller
    /// metadata image that still reads back cleanly (the surviving replica is
    /// the only one the read path will consider).
    #[arg(long)]
    single_replica: bool,

    /// Open devices without O_EXCL
    #[arg(long)]
    noexcl: bool,

    /// Verbose output
    #[arg(short = 'v', long)]
    verbose: bool,

    /// When streaming to stdout (-o -), select which device of a multi-device
    /// filesystem to stream (run once per device)
    #[arg(long = "device")]
    device: Option<u32>,

    /// Dump these normally-empty bucket classes too (debug/forensics; `free` dumps every bucket in bounds)
    #[arg(long = "include", value_delimiter = ',', value_enum)]
    include: Vec<IncludeBuckets>,

    /// Devices to dump
    #[arg(required = true)]
    devices: Vec<String>,
}

// ---- Undump CLI ----

/// Convert a qcow2 image back to a raw device image
#[derive(Parser, Debug)]
#[command(about = "Convert qcow2 dump files back to raw device images")]
pub struct UndumpCli {
    /// Overwrite existing output files
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// qcow2 files to convert
    #[arg(required = true)]
    files: Vec<String>,
}

fn cmd_undump(cli: UndumpCli) -> Result<()> {
    // Handle stdin case: undump - <output>
    if cli.files.first().map(|s| s.as_str()) == Some("-") {
        if cli.files.len() != 2 {
            return Err(anyhow!("stdin form requires exactly two arguments: undump [-f] - <output>"));
        }

        let output = &cli.files[1];
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        if !cli.force {
            open_opts.create_new(true);
        } else {
            open_opts.truncate(false);
        }

        let outfile = open_opts.open(output)
            .map_err(|err| anyhow!("{}: {}", output, err))?;

        qcow2::qcow2_stream_to_raw(std::io::stdin().as_fd(), outfile.as_fd())?;
        return Ok(());
    }

    let suffix = ".qcow2";

    struct FileEntry {
        input: String,
        output: String,
    }

    let mut entries = Vec::new();

    for f in &cli.files {
        if !f.ends_with(suffix) {
            return Err(anyhow!("{} not a qcow2 image?", f));
        }

        let output = f[..f.len() - suffix.len()].to_string();

        if !cli.force && Path::new(&output).exists() {
            return Err(anyhow!("{} already exists", output));
        }

        entries.push(FileEntry { input: f.clone(), output });
    }

    for e in &entries {
        let infile = std::fs::File::open(&e.input)
            .map_err(|err| anyhow!("{}: {}", e.input, err))?;

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        if !cli.force {
            open_opts.create_new(true);
        } else {
            open_opts.truncate(false);
        }

        let outfile = open_opts.open(&e.output)
            .map_err(|err| anyhow!("{}: {}", e.output, err))?;

        qcow2::qcow2_to_raw(infile.as_fd(), outfile.as_fd())?;
    }

    Ok(())
}

fn cmd_dump(cli: DumpCli) -> Result<()> {

    let (sanitize, sanitize_filenames) = match cli.sanitize.as_deref() {
        None => (false, false),
        Some("data") => (true, false),
        Some("filenames") => (true, true),
        Some(other) => return Err(anyhow!("Bad sanitize option: {}", other)),
    };

    if sanitize && !cli.metadata_only {
        return Err(anyhow!("--sanitize requires --metadata-only"));
    }

    if cli.single_replica && !cli.metadata_only {
        return Err(anyhow!("--single-replica requires --metadata-only"));
    }

    if !cli.include.is_empty() && cli.metadata_only {
        return Err(anyhow!("--include requires a full data dump (conflicts with --metadata-only)"));
    }

    if cli.device.is_some() && cli.output != "-" {
        return Err(anyhow!("--device only applies when streaming to stdout (-o -)"));
    }

    // Handle stdout streaming: capture real stdout before fs open pollutes it
    let stream_out = if cli.output == "-" {
        if sanitize {
            return Err(anyhow!("--sanitize is not supported with stdout output"));
        }
        if cli.single_replica {
            return Err(anyhow!("--single-replica is not supported with stdout output"));
        }
        if rustix::termios::isatty(rustix::stdio::stdout()) {
            return Err(anyhow!("refusing to write a qcow2 image to a terminal; redirect or pipe stdout"));
        }

        let real = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdout(), 0)
            .map_err(|e| anyhow!("dup stdout: {}", e))?;
        rustix::stdio::dup2_stdout(rustix::stdio::stderr())
            .map_err(|e| anyhow!("redirect stdout: {}", e))?;
        Some(real)
    } else {
        None
    };

    // Open filesystem in read-only, no-recovery mode
    let devs: Vec<PathBuf> = cli.devices.iter().map(PathBuf::from).collect();
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, direct_io, 0);
    opt_set!(opts, read_only, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, norecovery, 1);
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue as u8);
    opt_set!(opts, fix_errors, c::fsck_err_opts::FSCK_FIX_no as u8);

    if cli.noexcl {
        opt_set!(opts, noexcl, 1);
    }
    if cli.verbose {
        opt_set!(opts, verbose, 1);
    }

    let fs = crate::device_scan::open_scan(&devs, opts)?;
    collect::dump_fs(&fs, &cli, sanitize, sanitize_filenames, stream_out.as_ref().map(|f| f.as_fd()))
}

pub const CMD_DUMP: super::CmdDef = typed_cmd!("dump", "Dump filesystem metadata and data", DumpCli, cmd_dump);
pub const CMD_UNDUMP: super::CmdDef = typed_cmd!("undump", "Restore dumped metadata", UndumpCli, cmd_undump);
