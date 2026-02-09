use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use rustix::fs::{XattrFlags, setxattr, removexattr};

const BCHFS_IOC_REINHERIT_ATTRS: libc::c_ulong = 0x8008bc40;
const OPT_INODE: u32 = 4; // BIT(2)

fn inode_opt_names() -> Vec<String> {
    let mut names = Vec::new();
    unsafe {
        for i in 0..c::bch_opt_id::bch2_opts_nr as usize {
            let opt = &*c::bch2_opt_table.as_ptr().add(i);
            if opt.flags as u32 & OPT_INODE == 0 { continue }
            if opt.attr.name.is_null() { continue }
            if let Ok(s) = CStr::from_ptr(opt.attr.name).to_str() {
                names.push(s.to_string());
            }
        }
    }
    names
}

fn propagate_recurse(dir_path: &Path) {
    let dir = match std::fs::File::open(dir_path) {
        Ok(f) => f,
        Err(e) => { eprintln!("{}: {e}", dir_path.display()); return }
    };
    let entries = match std::fs::read_dir(dir_path) {
        Ok(e) => e,
        Err(e) => { eprintln!("{}: {e}", dir_path.display()); return }
    };

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() { continue }
        let Ok(name) = CString::new(entry.file_name().as_bytes().to_vec()) else { continue };

        let ret = unsafe { libc::ioctl(dir.as_raw_fd(), BCHFS_IOC_REINHERIT_ATTRS, name.as_ptr()) };
        if ret < 0 {
            eprintln!("{}: {}", entry.path().display(), std::io::Error::last_os_error());
            continue;
        }
        if ret == 0 || !ft.is_dir() { continue }
        propagate_recurse(&entry.path());
    }
}

fn remove_bcachefs_attr(path: &Path, attr_name: &str) {
    if let Err(e) = removexattr(path, attr_name) {
        if e != rustix::io::Errno::NODATA && e != rustix::io::Errno::INVAL {
            eprintln!("error removing attribute {} from {}: {}",
                attr_name, path.display(), e);
        }
    }
}

fn do_setattr(path: &str, opts: &[(String, String)], remove_all: bool) -> Result<()> {
    let path = Path::new(path);

    if remove_all {
        for name in inode_opt_names() {
            // casefold only works on empty directories
            if name == "casefold" { continue }
            remove_bcachefs_attr(path, &format!("bcachefs.{}", name));
        }
    }

    for (name, value) in opts {
        let attr = format!("bcachefs.{}", name);

        if value == "-" {
            remove_bcachefs_attr(path, &attr);
        } else {
            setxattr(path, &attr, value.as_bytes(), XattrFlags::empty())
                .map_err(|e| anyhow!("setxattr error on {}: {}", path.display(), e))?;
        }
    }

    if std::fs::metadata(path)
        .map_err(|e| anyhow!("stat error on {}: {}", path.display(), e))?
        .is_dir()
    {
        propagate_recurse(path);
    }
    Ok(())
}

fn setattr_usage() {
    println!("bcachefs set-file-option - set attributes on files in a bcachefs filesystem");
    println!("Usage: bcachefs set-file-option [OPTION]... <files>\n");
    println!("Options:");
    unsafe { c::bch2_opts_usage(OPT_INODE) };
    println!("      --remove-all             Remove all file options");
    println!("                               To remove specific options, use: --option=-");
    println!("  -h, --help                   Display this help and exit");
}

/// Parse argv, extracting bcachefs inode options and returning (remove_all, opts, files).
fn parse_setattr_args(argv: Vec<String>) -> Result<(bool, Vec<(String, String)>, Vec<String>)> {
    let valid_opts = inode_opt_names();
    let mut remove_all = false;
    let mut opts = Vec::new();
    let mut files = Vec::new();

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        if arg == "-h" || arg == "--help" {
            setattr_usage();
            std::process::exit(0);
        }
        if arg == "--remove-all" {
            remove_all = true;
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            let rest = &arg[2..];
            let (name, value) = if let Some(eq) = rest.find('=') {
                (rest[..eq].to_string(), rest[eq + 1..].to_string())
            } else if i + 1 < argv.len() && !argv[i + 1].starts_with('-') {
                let n = rest.to_string();
                let v = argv[i + 1].clone();
                i += 1;
                (n, v)
            } else {
                // Might be a boolean option
                (rest.to_string(), "1".to_string())
            };

            if valid_opts.iter().any(|o| *o == name) {
                opts.push((name, value));
            } else {
                return Err(anyhow!("invalid option --{}", name));
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            return Err(anyhow!("invalid option {}", arg));
        }

        files.push(arg.clone());
        i += 1;
    }

    Ok((remove_all, opts, files))
}

pub fn cmd_setattr(argv: Vec<String>) -> Result<()> {
    let (remove_all, opts, files) = parse_setattr_args(argv)?;

    if files.is_empty() {
        return Err(anyhow!("Please supply one or more files"));
    }

    for path in &files {
        do_setattr(path, &opts, remove_all)?;
    }
    Ok(())
}
