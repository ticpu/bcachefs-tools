use clap::Subcommand;

pub mod attr;
pub mod completions;
pub mod device;
pub mod list;
pub mod mount;
pub mod scrub;
pub mod subvolume;
pub mod timestats;
pub mod top;

pub use completions::completions;
pub use attr::cmd_setattr;
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
