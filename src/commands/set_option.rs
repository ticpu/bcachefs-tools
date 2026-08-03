
use anyhow::{bail, Context, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::fs::{Fs, OpenFs};
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::rcu;
use clap::{Arg, ArgAction, Command};
use rustix::fs::FileType;

use crate::commands::opts::{bch_opt_lookup, bch_option_args, bch_options_from_matches};
use crate::device_scan::OpenedFs;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs;

fn opt_flags() -> u32 {
    c::OptFlags::fs.bits() | c::OptFlags::device.bits()
}

/// Apply option (name, value) pairs to a filesystem, online or offline.
///
/// `devices` locates the filesystem; `dev_idxs`, when non-empty, pins
/// device-scoped options to those indices instead of to every path in
/// `devices`. Shared with `device set-option`, which resolves its
/// positional device to an index and passes it here.
pub(crate) fn apply_options(devices: &[&String], dev_idxs: &[u32], opts: &[(String, String)])
    -> Result<()>
{
    if opts.is_empty() {
        bail!("No options specified");
    }

    let devs: Vec<std::path::PathBuf> = devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = c::bch_opts::default();
    fs_opts.set_nostart(true);
    fs_opts.set_will_not_start(true);

    match crate::device_scan::open_online_or_offline(&devs, fs_opts)
        .with_context(|| format!("opening filesystem for '{}'", devices[0]))?
    {
        OpenedFs::Online(fs)  => set_option_online(fs, devices, dev_idxs, opts),
        OpenedFs::Offline(fs) => set_option_offline(fs, devices, dev_idxs, opts),
    }
}

fn set_option_cmd() -> Command {
    Command::new("set-fs-option")
        .about("Set a filesystem option")
        .long_about("\
Set a filesystem or device option on a running filesystem. Changes \
are persisted to the superblock. Use -d to target a specific device \
for device-scoped options. See <<sec:options>> for the full list of \
available options.")
        .args(bch_option_args(opt_flags(), false))
        .arg(Arg::new("dev-idx")
            .short('d')
            .long("dev-idx")
            .action(ArgAction::Append)
            .value_parser(clap::value_parser!(u32))
            .help("Device index for device-specific options"))
        .arg(Arg::new("devices")
            .required(true)
            .action(ArgAction::Append)
            .help("Device path(s)"))
}

fn cmd_set_option(argv: Vec<String>) -> Result<()> {
    let matches = set_option_cmd().get_matches_from(argv);

    let devices: Vec<&String> = matches.get_many::<String>("devices").unwrap().collect();
    let dev_idxs: Vec<u32> = matches.get_many::<u32>("dev-idx")
        .map(|v| v.copied().collect())
        .unwrap_or_default();

    let opts = bch_options_from_matches(&matches, opt_flags());

    apply_options(&devices, &dev_idxs, &opts)
}

fn set_option_online(
    fs: BcachefsHandle,
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    for dev in &devices[1..] {
        let fs2 = BcachefsHandle::open(dev.as_str())?;
        if fs.uuid() != fs2.uuid() {
            bail!("Filesystem mounted, but not all devices are members");
        }
    }

    let mut failed = std::collections::BTreeSet::new();

    for (name, value) in opts {

        let Some((_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            failed.insert(name.as_str());
            continue;
        };
        let flags = opt.flags.bits();

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            failed.insert(name.as_str());
            continue;
        }

        // The online path can only write sysfs, and the kernel creates an
        // option's attribute 0444 unless it is a runtime option (OPT_FILES,
        // fs/debug/sysfs.rs). So a
        // non-runtime option has a file that cannot be opened for writing,
        // and testing fs|device above lets it through to fail.
        // Many such options *can* be set with the filesystem unmounted, so
        // say that rather than just refusing.
        if flags & c::OptFlags::runtime.bits() == 0 {
            eprintln!("{name} cannot be set while the filesystem is mounted \
                       (unmount and set it offline)");
            failed.insert(name.as_str());
            continue;
        }

        let is_fs_opt = flags & c::OptFlags::fs.bits() != 0;
        let is_device_opt = flags & c::OptFlags::device.bits() != 0;

        if is_fs_opt && !is_device_opt {
            if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("options/{name}"), value) {
                eprintln!("Error setting {name}: {e}");
                failed.insert(name.as_str());
            }
        }

        if is_device_opt {
            if !dev_idxs.is_empty() {
                for dev_idx in dev_idxs {
                    if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                        eprintln!("Error setting {name} on device {dev_idx}: {e}");
                        failed.insert(name.as_str());
                    }
                }
                continue;
            }

            for dev in devices {
                let fs2 = BcachefsHandle::open(dev.as_str())?;
                let dev_idx = fs2.dev_idx();
                if dev_idx < 0 {
                    eprintln!("Couldn't determine device index for {dev}; use --dev-idx");
                    failed.insert(name.as_str());
                    continue;
                }

                if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                    eprintln!("Error setting {name} on device {dev_idx}: {e}");
                    failed.insert(name.as_str());
                }
            }
        }
    }

    // A configuration command that could not do what it was asked must not
    // report success: scripts have no other way to tell.
    if !failed.is_empty() {
        bail!("{} of {} option(s) could not be set: {}",
              failed.len(), opts.len(),
              failed.into_iter().collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

fn set_option_offline(
    fs: OpenFs,
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    let mut modified = false;
    let mut failed = std::collections::BTreeSet::new();

    for (name, value) in opts {

        let Some((opt_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            failed.insert(name.as_str());
            continue;
        };
        let flags = opt.flags.bits();

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            failed.insert(name.as_str());
            continue;
        }

        let mut err = Printbuf::new();
        let Ok(val) = bcachefs_kernel::opts::opt_parse(Some(&fs), opt, Some(value.as_bytes()), Some(&mut err)) else {
            match err.as_str().trim_end() {
                ""  => eprintln!("Error parsing {name}={value}"),
                msg => eprintln!("Error parsing {name}={value}: {msg}"),
            }
            failed.insert(name.as_str());
            continue;
        };

        if flags & c::OptFlags::fs.bits() != 0 {
            if let Err(e) = fs.opt_hook_pre_set(None, opt_id, val) {
                eprintln!("Error setting {name}: {e}");
                failed.insert(name.as_str());
                continue;
            }
            if let Err(e) = fs.opt_set_sb(None, opt, val, Some(value.as_bytes())) {
                eprintln!("Error setting {name}={value}: {e}");
                failed.insert(name.as_str());
                continue;
            }
            modified = true;
        }

        if flags & c::OptFlags::device.bits() != 0 {
            let indices: Vec<u32> = if !dev_idxs.is_empty() {
                dev_idxs.to_vec()
            } else {
                // Don't silently drop a device we couldn't resolve: with every
                // name unresolved this list is empty, the loop below never
                // runs, and the command reports success having done nothing.
                let mut idxs = Vec::with_capacity(devices.len());
                for dev in devices {
                    match path_to_dev_idx(&fs, dev) {
                        Ok(i) => idxs.push(i),
                        Err(e) => {
                            eprintln!("{e:#}");
                            failed.insert(name.as_str());
                        }
                    }
                }
                idxs
            };

            if indices.is_empty() {
                eprintln!("No devices to set {name} on");
                failed.insert(name.as_str());
            }

            for idx in indices {
                let Some(ca) = fs.dev_get(idx) else {
                    eprintln!("Couldn't look up device {idx}");
                    failed.insert(name.as_str());
                    continue;
                };

                if let Err(e) = fs.opt_hook_pre_set(Some(&*ca), opt_id, val) {
                    eprintln!("Error setting {name}: {e}");
                    failed.insert(name.as_str());
                    continue;
                }
                if let Err(e) = fs.opt_set_sb(Some(&*ca), opt, val, Some(value.as_bytes())) {
                    eprintln!("Error setting {name}={value} on device {idx}: {e}");
                    failed.insert(name.as_str());
                    continue;
                }
                modified = true;
            }
        }
    }

    if modified {
        fs.write_super_ret(&mut fs.sb_lock())
            .map_err(|e| anyhow::anyhow!("error writing superblock: {e}"))?;
    }

    if !failed.is_empty() {
        bail!("{} of {} option(s) could not be set: {}",
              failed.len(), opts.len(),
              failed.into_iter().collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

/// The online member whose block device is the file at @path. Matched by
/// identity: a member's name is only its path's last component.
fn path_to_dev_idx(fs: &Fs, path: &str) -> Result<u32> {
    let st = rustix::fs::stat(path)
        .with_context(|| format!("looking up device {path}"))?;

    let online: Vec<u32> = fs.online_devs_rcu(&rcu::read_lock())
        .map(|ca| ca.dev_idx as u32)
        .collect();

    for idx in online {
        let Some(ca) = fs.dev_get_ioref::<c::DevReadRefKind>(idx) else {
            continue;
        };
        let member = rustix::fs::fstat(ca.bdev().fd())
            .with_context(|| format!("stat of device {idx}'s open file"))?;
        if same_device(&st, &member) {
            return Ok(idx);
        }
    }

    bail!("{path} is not an online member of this filesystem")
}

/// A block device is its device number, whatever node names it; any other
/// file is its inode.
fn same_device(a: &rustix::fs::Stat, b: &rustix::fs::Stat) -> bool {
    let is_blk = |st: &rustix::fs::Stat| FileType::from_raw_mode(st.st_mode) == FileType::BlockDevice;

    match (is_blk(a), is_blk(b)) {
        (true, true)   => a.st_rdev == b.st_rdev,
        (false, false) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
        _              => false,
    }
}

pub const CMD: super::CmdDef = raw_cmd!("set-fs-option", "Set filesystem options", cmd_set_option);

#[cfg(test)]
mod tests {
    use super::same_device;
    use std::fs;

    fn stat(p: &std::path::Path) -> rustix::fs::Stat {
        rustix::fs::stat(p).unwrap()
    }

    #[test]
    fn file_matches_by_inode_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let (img_a, img_b) = (a.join("dev.img"), b.join("dev.img"));
        fs::write(&img_a, b"").unwrap();
        fs::write(&img_b, b"").unwrap();
        fs::hard_link(&img_a, dir.path().join("link.img")).unwrap();

        let spelled_differently = a.join("..").join("a").join("dev.img");
        assert!(same_device(&stat(&img_a), &stat(&spelled_differently)));
        assert!(same_device(&stat(&img_a), &stat(&dir.path().join("link.img"))));
        assert!(!same_device(&stat(&img_a), &stat(&img_b)));
    }

    #[test]
    fn block_device_never_matches_a_file() {
        let Some(blk) = fs::read_dir("/dev").unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| rustix::fs::FileType::from_raw_mode(stat(p).st_mode)
                == rustix::fs::FileType::BlockDevice)
        else {
            eprintln!("no block device in /dev, skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dev.img");
        fs::write(&file, b"").unwrap();

        assert!(same_device(&stat(&blk), &stat(&blk)));
        assert!(!same_device(&stat(&blk), &stat(&file)));
    }
}
