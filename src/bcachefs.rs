mod commands;
mod copy_fs;
mod device_multipath;
mod device_scan;
mod key;
mod dump_stack;
mod logging;
mod qcow2;
mod util;
mod wrappers;
mod http;

use std::process::{ExitCode, Termination};

use bch_bindgen::c;

#[derive(Debug)]
pub struct ErrnoError(pub errno::Errno);
impl std::fmt::Display for ErrnoError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

impl std::error::Error for ErrnoError {}

/// Read the running kernel's .config from /boot or /proc/config.gz.
fn read_kernel_config() -> Option<String> {
    // Try /boot/config-$(uname -r) first (most distros)
    let release = std::process::Command::new("uname").arg("-r")
        .output().ok()?;
    let release = std::str::from_utf8(&release.stdout).ok()?.trim();
    let path = format!("/boot/config-{release}");
    if let Ok(config) = std::fs::read_to_string(&path) {
        return Some(config);
    }

    // Fallback: /proc/config.gz (NixOS, CONFIG_IKCONFIG_PROC=y kernels)
    let output = std::process::Command::new("zcat")
        .arg("/proc/config.gz")
        .output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn kernel_config_has(config: &str, key: &str) -> bool {
    let needle = format!("{key}=y");
    config.lines().any(|l| l == needle)
}

/// Print warnings about kernel configuration issues that affect bcachefs.
/// Called before every command so users can't miss them.
fn check_kernel_warnings() {
    let Some(config) = read_kernel_config() else { return };

    if !kernel_config_has(&config, "CONFIG_RUST") {
        eprintln!("WARNING: kernel does not have CONFIG_RUST enabled; \
                   this will be required for bcachefs in the near future");
        eprintln!("         please alert your distribution or kernel developers \
                   if your kernel does not support CONFIG_RUST");
    }
}

/// Print main bcachefs usage, with commands grouped by category.
/// Descriptions are pulled from the clap command tree (build_cli).
fn bcachefs_usage() {
    let cmd = commands::build_cli();

    println!("bcachefs - tool for managing bcachefs filesystems");
    println!("usage: bcachefs <command> [<args>]\n");

    for (heading, names) in commands::COMMAND_GROUPS {
        println!("{heading}:");
        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            let children: Vec<_> = sub.get_subcommands()
                .filter(|c| c.get_name() != "help")
                .collect();
            if !children.is_empty() {
                for child in children {
                    let about = child.get_about().map(|s| s.to_string()).unwrap_or_default();
                    let full = format!("{name} {}", child.get_name());
                    println!("  {full:<26}{about}");
                }
            } else {
                let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
                println!("  {:<26}{about}", name);
            }
        }
        println!();
    }
}


fn escape_latex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // --- is an em-dash in prose: leave intact
        if b.get(i) == Some(&b'-') && b.get(i+1) == Some(&b'-') && b.get(i+2) == Some(&b'-') {
            out.push_str("---");
            i += 3;
            continue;
        }
        // -- in text mode becomes an en-dash; break ligature with empty group
        if b.get(i) == Some(&b'-') && b.get(i+1) == Some(&b'-') {
            out.push_str("-{}-");
            i += 2;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        match ch {
            '_' => out.push_str("\\_"),
            '#' => out.push_str("\\#"),
            '%' => out.push_str("\\%"),
            '&' => out.push_str("\\&"),
            '$' => out.push_str("\\$"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            _ => out.push(ch),
        }
        i += ch.len_utf8();
    }
    out
}

/// Convert long_about text to LaTeX: escape special chars, convert
/// `<<sec:LABEL>>` to `Section~\ref{sec:LABEL}`, and preserve paragraph breaks.
fn about_to_latex(text: &str) -> String {
    use std::fmt::Write;

    let mut result = String::new();
    for para in text.split("\n\n") {
        if !result.is_empty() {
            result.push('\n');
        }
        let escaped = escape_latex(para.trim());
        // Convert <<sec:label>> cross-references
        let mut s = escaped.as_str();
        while let Some(start) = s.find("<<") {
            result.push_str(&s[..start]);
            let rest = &s[start + 2..];
            if let Some(end) = rest.find(">>") {
                let label = &rest[..end];
                write!(result, "Section~\\ref{{{label}}}").unwrap();
                s = &rest[end + 2..];
            } else {
                result.push_str("<<");
                s = rest;
            }
        }
        result.push_str(s);
        result.push('\n');
    }
    result
}

/// Generate doc/generated/cli-reference.tex from the clap command tree.
fn generate_cli_doc() -> ExitCode {
    use std::fmt::Write;
    use std::path::Path;

    let cmd = commands::build_cli();
    let mut out = String::new();

    for (heading, names) in commands::COMMAND_GROUPS {
        writeln!(out, "\\subsection{{{heading}}}").unwrap();

        // Emit group-level long_about as intro text before the command list
        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            if sub.get_subcommands().count() > 0 {
                if let Some(long_about) = sub.get_long_about() {
                    writeln!(out).unwrap();
                    write!(out, "{}", about_to_latex(&long_about.to_string())).unwrap();
                }
            }
        }

        writeln!(out, "\\begin{{description}}").unwrap();

        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            let children: Vec<_> = sub.get_subcommands()
                .filter(|c| c.get_name() != "help")
                .collect();

            if !children.is_empty() {
                // Group command (device, fs, image, etc.) — list children
                for child in children {
                    let full = format!("bcachefs {name} {}", child.get_name());
                    write!(out, "\\item[{{\\tt {}}}]", escape_latex(&full)).unwrap();

                    let aliases: Vec<_> = child.get_visible_aliases().collect();
                    if !aliases.is_empty() {
                        let alias_str = aliases.into_iter()
                            .map(|a| format!("{{\\tt {}}}", escape_latex(a)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(out, " (alias: {alias_str})").unwrap();
                    }

                    let about = child.get_about().map(|s| s.to_string());
                    let long_about = child.get_long_about().map(|s| s.to_string());

                    if let Some(ref text) = about {
                        writeln!(out, " {}", escape_latex(text)).unwrap();
                    } else {
                        writeln!(out).unwrap();
                    }
                    if let Some(ref text) = long_about {
                        writeln!(out).unwrap();
                        write!(out, "{}", about_to_latex(text)).unwrap();
                    }
                }
            } else {
                // Leaf command
                let full = format!("bcachefs {name}");
                write!(out, "\\item[{{\\tt {}}}]", escape_latex(&full)).unwrap();

                let aliases: Vec<_> = sub.get_visible_aliases().collect();
                if !aliases.is_empty() {
                    let alias_str = aliases.into_iter()
                        .map(|a| format!("{{\\tt {}}}", escape_latex(a)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(out, " (alias: {alias_str})").unwrap();
                }

                let about = sub.get_about().map(|s| s.to_string());
                let long_about = sub.get_long_about().map(|s| s.to_string());

                if let Some(ref text) = about {
                    writeln!(out, " {}", escape_latex(text)).unwrap();
                } else {
                    writeln!(out).unwrap();
                }
                if let Some(ref text) = long_about {
                    writeln!(out).unwrap();
                    write!(out, "{}", about_to_latex(text)).unwrap();
                }
            }
        }

        writeln!(out, "\\end{{description}}").unwrap();
        writeln!(out).unwrap();
    }

    let dir = Path::new("doc/generated");
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("error creating {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }

    let path = dir.join("cli-reference.tex");
    if let Err(e) = std::fs::write(&path, &out) {
        eprintln!("error writing {}: {e}", path.display());
        return ExitCode::FAILURE;
    }

    eprintln!("wrote {}", path.display());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    use commands::{Subcommands as S, DeviceCmd, FsCmd, ImageCmd, ReconcileCmd, DataCmd};

    // glibc and Rust stdlib buffer stdout independently; when piped, both
    // switch to block buffering which can reorder or lose output.
    // Set both to line-buffered.
    unsafe {
        extern "C" { static stdout: *mut libc::FILE; }
        libc::setvbuf(stdout, std::ptr::null_mut(), libc::_IOLBF, 0);
    }

    let args: Vec<String> = std::env::args().collect();

    // Handle symlink invocations (mkfs.bcachefs, fsck.bcachefs, mount.bcachefs, etc.)
    let symlink_cmd: Option<&str> = if args[0].contains("mkfs") {
        Some("format")
    } else if args[0].contains("fsck") {
        Some("fsck")
    } else if args[0].contains("mount.fuse") {
        Some("fusemount")
    } else if args[0].contains("mount") {
        Some("mount")
    } else {
        None
    };

    // Handle top-level help and missing command before clap parsing
    if symlink_cmd.is_none() {
        match args.get(1).map(|s| s.as_str()) {
            None => {
                println!("missing command");
                bcachefs_usage();
                return ExitCode::from(1);
            }
            Some("--help" | "-h" | "help") => {
                bcachefs_usage();
                return ExitCode::SUCCESS;
            }
            Some("_doc_gen") => return generate_cli_doc(),
            _ => {}
        }
    }

    unsafe { c::raid_init() };

    // Build effective argv for clap: insert subcommand name for symlink invocations
    let effective_args: Vec<String> = if let Some(cmd) = symlink_cmd {
        let mut v = vec!["bcachefs".to_string(), cmd.to_string()];
        v.extend_from_slice(&args[1..]);
        v
    } else {
        args
    };

    use clap::Parser;

    let cli = commands::Cli::parse_from(&effective_args);

    // fuse will call this after daemonizing, we can't create threads before
    // note that mount may invoke fusemount, via -t bcachefs.fuse
    if !matches!(cli.cmd, S::Mount(_) | S::Fusemount(_)) {
        unsafe { c::linux_shrinkers_init() };
    }

    check_kernel_warnings();

    match cli.cmd {
        // RawArgs commands — manual parsing, pass argv through
        S::Format(raw)                  => commands::format::cmd_format(raw.argv("format")).report(),
        S::SetFsOption(raw)             => commands::set_option::cmd_set_option(raw.argv("set-fs-option")).report(),
        S::StripAlloc(cli)              => commands::strip_alloc::cmd_strip_alloc(cli).report(),
        S::Mount(cli)                   => commands::mount::mount(cli),
        S::Migrate(raw)                 => commands::migrate::cmd_migrate(raw.argv("migrate")).report(),
        S::SetFileOption(raw)           => commands::attr::cmd_setattr(raw.argv("set-file-option")).report(),
        S::ReflinkOptionPropagate(raw)  => commands::attr::cmd_reflink_option_propagate(raw.argv("reflink-option-propagate")).report(),
        S::Fusemount(cli)               => commands::fusemount::cmd_fusemount(cli).report(),

        // Typed commands — clap already parsed, just dispatch
        S::ShowSuper(cli)               => commands::super_cmd::cmd_show_super(cli).report(),
        S::RecoverSuper(cli)            => commands::recover_super::cmd_recover_super(cli).report(),
        S::ResetCounters(cli)           => commands::counters::cmd_reset_counters(cli).report(),
        S::Fsck(cli)                    => commands::fsck::cmd_fsck(cli).report(),
        S::RecoveryPass(cli)            => commands::recovery_pass::cmd_recovery_pass(cli).report(),
        S::Subvolume(cli)               => commands::subvolume::subvolume(cli).report(),
        S::Scrub(cli)                   => commands::scrub::scrub(cli).report(),
        S::Unlock(cli)                  => commands::key::cmd_unlock(cli).report(),
        S::SetPassphrase(cli)           => commands::key::cmd_set_passphrase(cli).report(),
        S::RemovePassphrase(cli)        => commands::key::cmd_remove_passphrase(cli).report(),
        S::MigrateSuperblock(cli)       => commands::migrate::cmd_migrate_superblock(cli).report(),
        S::Dump(cli)                    => commands::dump::cmd_dump(cli).report(),
        S::Undump(cli)                  => commands::dump::cmd_undump(cli).report(),
        S::List(cli)                    => commands::list::list(cli).report(),
        S::ListJournal(cli)             => commands::list_journal::cmd_list_journal(cli).report(),
        S::KillBtreeNode(cli)           => commands::kill_btree_node::cmd_kill_btree_node(cli).report(),
        S::DataRead(cli)                => commands::data_read::cmd_data_read(cli).report(),
        S::Unpoison(cli)                => commands::unpoison::cmd_unpoison(cli).report(),
        S::Completions(cli)             => { commands::completions::completions(cli); ExitCode::SUCCESS },

        // Group commands
        S::Image { cmd }               => match cmd {
            ImageCmd::Create(raw)       => commands::image::cmd_image_create(raw.argv("create")).report(),
            ImageCmd::Update(cli)       => commands::image::cmd_image_update(cli).report(),
        },
        S::Fs { cmd }                   => match cmd {
            FsCmd::Usage(cli)           => commands::fs_usage::fs_usage(cli).report(),
            FsCmd::Top(cli)             => commands::top::top(cli).report(),
            FsCmd::Timestats(cli)       => commands::timestats::timestats(cli).report(),
        },
        S::Device { cmd }              => match cmd {
            DeviceCmd::Add(raw)         => commands::device::cmd_device_add(raw.argv("add")).report(),
            DeviceCmd::Online(cli)      => commands::device::cmd_device_online(cli).report(),
            DeviceCmd::Offline(cli)     => commands::device::cmd_device_offline(cli).report(),
            DeviceCmd::Remove(cli)      => commands::device::cmd_device_remove(cli).report(),
            DeviceCmd::Evacuate(cli)    => commands::device::cmd_device_evacuate(cli).report(),
            DeviceCmd::SetState(cli)    => commands::device::cmd_device_set_state(cli).report(),
            DeviceCmd::Resize(cli)      => commands::device::cmd_device_resize(cli).report(),
            DeviceCmd::ResizeJournal(cli) => commands::device::cmd_device_resize_journal(cli).report(),
        },
        S::Reconcile { cmd }            => match cmd {
            ReconcileCmd::Status(cli)   => commands::reconcile::cmd_reconcile_status(cli).report(),
            ReconcileCmd::Wait(cli)     => commands::reconcile::cmd_reconcile_wait(cli).report(),
        },
        S::Data { cmd }                 => match cmd {
            DataCmd::Scrub(cli)         => commands::scrub::scrub(cli).report(),
        },
        S::Version                      => {
            let vh = include_str!("../version.h");
            println!("{}", vh.split('"').nth(1).unwrap_or("unknown"));
            let config = read_kernel_config();
            let rust_status = match config.as_deref().map(|c| kernel_config_has(c, "CONFIG_RUST")) {
                Some(true)  => "CONFIG_RUST=y",
                Some(false) => "CONFIG_RUST is not enabled",
                None        => "unable to read kernel config",
            };
            println!("kernel: {rust_status}");
            ExitCode::SUCCESS
        }
    }
}
