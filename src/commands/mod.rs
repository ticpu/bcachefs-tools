use clap::{Command, CommandFactory, Subcommand};

pub mod attr;
pub mod completions;
pub mod counters;
pub mod device;
pub mod list;
pub mod mount;
pub mod opts;
pub mod scrub;
pub mod subvolume;
pub mod timestats;
pub mod top;

pub use completions::completions;
pub use attr::cmd_setattr;
pub use attr::cmd_reflink_option_propagate;
pub use counters::cmd_reset_counters;
pub use device::{
    cmd_device_online, cmd_device_offline, cmd_device_remove, cmd_device_evacuate,
    cmd_device_set_state, cmd_device_resize, cmd_device_resize_journal,
};
pub use list::list;
pub use mount::mount;
pub use scrub::scrub;
pub use subvolume::subvolume;
pub use timestats::timestats;
pub use top::top;

#[derive(clap::Parser, Debug)]
#[command(name = "bcachefs")]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    List(list::Cli),
    Mount(mount::Cli),
    Completions(completions::Cli),
    #[command(visible_aliases = ["subvol"])]
    Subvolume(subvolume::Cli),
}

/// Build full command tree for completions and help.
/// Includes both Rust commands (with full arg specs) and C commands (stubs).
pub fn build_cli() -> Command {
    let mut cmd = Cli::command();

    // Rust commands with full Clap specs
    cmd = cmd
        .subcommand(attr::setattr_cmd())
        .subcommand(attr::reflink_option_propagate_cmd())
        .subcommand(Command::new("reset-counters")
            .about("Reset filesystem counters")
            .arg(clap::Arg::new("fs").required(true)))
        .subcommand(Command::new("version")
            .about("Display version"));

    // C commands — stubs for completions/help
    // (list, mount, completions, subvolume already come from the derive-based Cli)
    cmd = cmd
        .subcommand(Command::new("data").about("Manage filesystem data"))
        .subcommand(Command::new("device").about("Manage devices within a filesystem"))
        .subcommand(Command::new("dump").about("Dump filesystem metadata to a qcow2 image"))
        .subcommand(Command::new("format").visible_alias("mkfs")
            .about("Format a new filesystem"))
        .subcommand(Command::new("fs").about("Manage a running filesystem"))
        .subcommand(Command::new("fsck").about("Check an existing filesystem for errors"))
        .subcommand(Command::new("image").about("Filesystem image commands"))
        .subcommand(Command::new("migrate")
            .about("Migrate an existing ext2/3/4 filesystem to bcachefs in place"))
        .subcommand(Command::new("reconcile").about("Reconcile filesystem data"))
        .subcommand(Command::new("recovery-pass")
            .about("Run a specific recovery pass"))
        .subcommand(Command::new("set-fs-option")
            .about("Set a filesystem option"))
        .subcommand(Command::new("set-passphrase")
            .about("Set passphrase on an encrypted filesystem"))
        .subcommand(Command::new("remove-passphrase")
            .about("Remove passphrase from an encrypted filesystem"))
        .subcommand(Command::new("show-super")
            .about("Print superblock information to stdout"))
        .subcommand(Command::new("unlock")
            .about("Unlock an encrypted filesystem"));

    cmd
}
