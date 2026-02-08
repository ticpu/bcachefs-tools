use clap::Subcommand;

pub mod completions;
pub mod list;
pub mod mount;
pub mod subvolume;
pub mod timestats;
pub mod top;

pub use completions::completions;
pub use list::list;
pub use mount::mount;
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
