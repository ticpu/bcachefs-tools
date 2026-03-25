use clap::{Args, Command, CommandFactory, Parser, Subcommand};

/// Command groups for help output and documentation generation.
/// Each entry: (group heading, list of top-level subcommand names).
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Superblock commands", &[
        "format", "show-super", "recover-super",
        "set-fs-option", "reset-counters", "strip-alloc",
    ]),
    ("Images", &["image"]),
    ("Mount", &["mount"]),
    ("Repair", &["fsck", "recovery-pass"]),
    ("Running filesystem", &["fs"]),
    ("Devices", &["device"]),
    ("Subvolumes and snapshots", &["subvolume"]),
    ("Filesystem data", &["reconcile", "scrub"]),
    ("Encryption", &["unlock", "set-passphrase", "remove-passphrase"]),
    ("Migrate", &["migrate", "migrate-superblock"]),
    ("File options", &["set-file-option", "reflink-option-propagate"]),
    ("Debug", &["dump", "undump", "list", "list_journal", "kill_btree_node", "data-read", "unpoison"]),
    ("Miscellaneous", &["completions", "version"]),
];

pub mod attr;
pub mod completions;
pub mod counters;
pub mod device;
pub mod dump;
pub mod format;
pub mod format_util;
pub mod fs_usage;
pub mod image;
pub mod fsck;
pub mod key;
pub mod kill_btree_node;
pub mod list;
pub mod list_journal;
pub mod migrate;
pub mod mount;
pub mod opts;
pub mod data_read;
pub mod unpoison;
pub mod reconcile;
pub mod recover_super;
pub mod recovery_pass;
pub mod scrub;
pub mod set_option;
pub mod strip_alloc;
pub mod subvolume;
pub mod super_cmd;
pub mod timestats;
pub mod top;
pub mod fusemount;

/// Passthrough for commands that do their own argument parsing.
#[derive(Args, Debug)]
#[command(disable_help_flag = true)]
pub struct RawArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

impl RawArgs {
    /// Reconstruct argv with command name prepended, as parse_from expects.
    pub fn argv(self, cmd_name: &str) -> Vec<String> {
        let mut v = vec![cmd_name.to_string()];
        v.extend(self.args);
        v
    }
}

#[derive(Parser, Debug)]
#[command(name = "bcachefs", disable_help_subcommand = true)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Subcommands,
}

#[derive(Subcommand, Debug)]
pub enum Subcommands {
    // RawArgs: commands with manual arg parsing (dynamic C opts, etc.)
    #[command(name = "format", visible_alias = "mkfs",
              about = "Format a new filesystem")]
    Format(RawArgs),
    #[command(name = "set-fs-option", about = "Set filesystem options")]
    SetFsOption(RawArgs),
    #[command(name = "strip-alloc", about = "Strip alloc info for read-only use")]
    StripAlloc(strip_alloc::Cli),
    #[command(name = "mount", about = "Mount a filesystem")]
    Mount(mount::Cli),
    #[command(name = "migrate", about = "Migrate existing filesystem to bcachefs")]
    Migrate(RawArgs),
    #[command(name = "set-file-option", about = "Set file-level options")]
    SetFileOption(RawArgs),
    #[command(name = "reflink-option-propagate",
              about = "Propagate options to reflinked files")]
    ReflinkOptionPropagate(RawArgs),
    #[command(name = "fusemount", hide = true)]
    Fusemount(fusemount::Cli),

    // Typed Cli structs: clap parses args directly
    #[command(name = "show-super", about = "Print superblock information")]
    ShowSuper(super_cmd::ShowSuperCli),
    #[command(name = "recover-super", about = "Recover damaged superblock")]
    RecoverSuper(recover_super::RecoverSuperCli),
    #[command(name = "reset-counters", about = "Reset filesystem counters")]
    ResetCounters(counters::Cli),
    #[command(name = "fsck", about = "Check filesystem consistency")]
    Fsck(fsck::FsckCli),
    #[command(name = "recovery-pass", about = "Manage recovery passes")]
    RecoveryPass(recovery_pass::RecoveryPassCli),
    #[command(name = "subvolume", visible_alias = "subvol",
              about = "Manage subvolumes and snapshots")]
    Subvolume(subvolume::Cli),
    #[command(name = "scrub", about = "Verify data checksums")]
    Scrub(scrub::Cli),
    #[command(name = "unlock", about = "Unlock an encrypted filesystem")]
    Unlock(key::UnlockCli),
    #[command(name = "set-passphrase", about = "Set or change passphrase")]
    SetPassphrase(key::SetPassphraseCli),
    #[command(name = "remove-passphrase", about = "Remove passphrase")]
    RemovePassphrase(key::RemovePassphraseCli),
    #[command(name = "migrate-superblock",
              about = "Move superblock to standard location")]
    MigrateSuperblock(migrate::MigrateSuperblockCli),
    #[command(name = "dump", about = "Dump filesystem metadata")]
    Dump(dump::DumpCli),
    #[command(name = "undump", about = "Restore dumped metadata")]
    Undump(dump::UndumpCli),
    #[command(name = "list", about = "List filesystem metadata")]
    List(list::Cli),
    #[command(name = "list_journal", about = "List journal entries")]
    ListJournal(list_journal::Cli),
    #[command(name = "kill_btree_node", about = "Remove a btree node")]
    KillBtreeNode(kill_btree_node::KillBtreeNodeCli),
    #[command(name = "data-read", about = "Read data with extended error info")]
    DataRead(data_read::Cli),
    #[command(name = "unpoison", about = "Clear poison flags on file extents")]
    Unpoison(unpoison::Cli),
    #[command(name = "completions", about = "Generate shell completions")]
    Completions(completions::Cli),
    #[command(name = "version", about = "Display version")]
    Version,

    // Group commands with nested subcommands
    #[command(name = "image", about = "Filesystem image commands")]
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    #[command(name = "fs", about = "Manage a running filesystem")]
    Fs {
        #[command(subcommand)]
        cmd: FsCmd,
    },
    #[command(name = "device", about = "Manage devices within a filesystem")]
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
    #[command(name = "reconcile", about = "Reconcile filesystem data")]
    Reconcile {
        #[command(subcommand)]
        cmd: ReconcileCmd,
    },
    #[command(name = "data", about = "Manage filesystem data")]
    Data {
        #[command(subcommand)]
        cmd: DataCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum DeviceCmd {
    #[command(name = "add", about = "Add a device to a filesystem")]
    Add(RawArgs),
    #[command(name = "online", about = "Bring a device online")]
    Online(device::OnlineCli),
    #[command(name = "offline", about = "Take a device offline")]
    Offline(device::OfflineCli),
    #[command(name = "remove", about = "Remove a device")]
    Remove(device::RemoveCli),
    #[command(name = "evacuate", about = "Evacuate data from a device")]
    Evacuate(device::EvacuateCli),
    #[command(name = "set-state", about = "Set device state")]
    SetState(device::SetStateCli),
    #[command(name = "resize", about = "Resize filesystem on a device")]
    Resize(device::ResizeCli),
    #[command(name = "resize-journal", about = "Resize journal on a device")]
    ResizeJournal(device::ResizeJournalCli),
}

#[derive(Subcommand, Debug)]
pub enum FsCmd {
    #[command(name = "usage", about = "Show filesystem disk usage")]
    Usage(fs_usage::Cli),
    #[command(name = "top", about = "Show live performance counters")]
    Top(top::Cli),
    #[command(name = "timestats", about = "Show operation latency statistics")]
    Timestats(timestats::Cli),
}

#[derive(Subcommand, Debug)]
pub enum ImageCmd {
    #[command(name = "create", about = "Create a filesystem image")]
    Create(RawArgs),
    #[command(name = "update", about = "Update a filesystem image")]
    Update(image::ImageUpdateCli),
}

#[derive(Subcommand, Debug)]
pub enum ReconcileCmd {
    #[command(name = "status", about = "Show reconcile status")]
    Status(reconcile::StatusCli),
    #[command(name = "wait", about = "Wait for reconcile to complete")]
    Wait(reconcile::WaitCli),
}

#[derive(Subcommand, Debug)]
pub enum DataCmd {
    #[command(name = "scrub", about = "Verify data checksums")]
    Scrub(scrub::Cli),
}

/// Build the full command tree for completions and help.
pub fn build_cli() -> Command {
    Cli::command()
}
