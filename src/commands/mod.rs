use clap::Subcommand;

pub mod completions;
pub mod inode_opts;
mod inode_opts_device;
mod inode_opts_mounted;
pub mod list;
pub mod mount;
pub mod subvolume;
pub mod subvol_diff;

pub use completions::completions;
pub use inode_opts::inode_opts;
pub use list::list;
pub use mount::mount;
pub use subvolume::subvolume;
pub use subvol_diff::subvol_diff;

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
    #[command(name = "subvol-diff", visible_aliases = ["diff"])]
    SubvolDiff(subvol_diff::Cli),
    #[command(name = "inode-opts")]
    InodeOpts(inode_opts::Cli),
}
