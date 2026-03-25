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
        S::Format(raw)                  => commands::format::cmd_format(raw.argv("format")).report(),
        S::ShowSuper(raw)               => commands::super_cmd::cmd_show_super(raw.argv("show-super")).report(),
        S::RecoverSuper(raw)            => commands::recover_super::cmd_recover_super(raw.argv("recover-super")).report(),
        S::SetFsOption(raw)             => commands::set_option::cmd_set_option(raw.argv("set-fs-option")).report(),
        S::ResetCounters(raw)           => commands::counters::cmd_reset_counters(raw.argv("reset-counters")).report(),
        S::StripAlloc(raw)              => commands::strip_alloc::cmd_strip_alloc(raw.argv("strip-alloc")).report(),

        S::Image { cmd }               => match cmd {
            ImageCmd::Create(raw)       => commands::image::cmd_image_create(raw.argv("create")).report(),
            ImageCmd::Update(raw)       => commands::image::cmd_image_update(raw.argv("update")).report(),
        },

        S::Mount(raw)                   => commands::mount::mount(raw.argv("mount"), symlink_cmd),
        S::Fsck(raw)                    => commands::fsck::cmd_fsck(raw.argv("fsck")).report(),
        S::RecoveryPass(raw)            => commands::recovery_pass::cmd_recovery_pass(raw.argv("recovery-pass")).report(),

        S::Fs { cmd }                   => match cmd {
            FsCmd::Usage(raw)           => commands::fs_usage::fs_usage(raw.argv("usage")).report(),
            FsCmd::Top(raw)             => commands::top::top(raw.argv("top")).report(),
            FsCmd::Timestats(raw)       => commands::timestats::timestats(raw.argv("timestats")).report(),
        },

        S::Device { cmd }              => match cmd {
            DeviceCmd::Add(raw)         => commands::device::cmd_device_add(raw.argv("add")).report(),
            DeviceCmd::Online(raw)      => commands::device::cmd_device_online(raw.argv("online")).report(),
            DeviceCmd::Offline(raw)     => commands::device::cmd_device_offline(raw.argv("offline")).report(),
            DeviceCmd::Remove(raw)      => commands::device::cmd_device_remove(raw.argv("remove")).report(),
            DeviceCmd::Evacuate(raw)    => commands::device::cmd_device_evacuate(raw.argv("evacuate")).report(),
            DeviceCmd::SetState(raw)    => commands::device::cmd_device_set_state(raw.argv("set-state")).report(),
            DeviceCmd::Resize(raw)      => commands::device::cmd_device_resize(raw.argv("resize")).report(),
            DeviceCmd::ResizeJournal(raw) => commands::device::cmd_device_resize_journal(raw.argv("resize-journal")).report(),
        },

        S::Subvolume(raw)               => commands::subvolume::subvolume(raw.argv("subvolume")).report(),

        S::Reconcile { cmd }            => match cmd {
            ReconcileCmd::Status(raw)   => commands::reconcile::cmd_reconcile_status(raw.argv("status")).report(),
            ReconcileCmd::Wait(raw)     => commands::reconcile::cmd_reconcile_wait(raw.argv("wait")).report(),
        },
        S::Scrub(raw)                   => commands::scrub::scrub(raw.argv("scrub")).report(),
        S::Data { cmd }                 => match cmd {
            DataCmd::Scrub(raw)         => commands::scrub::scrub(raw.argv("scrub")).report(),
        },

        S::Unlock(raw)                  => commands::key::cmd_unlock(raw.argv("unlock")).report(),
        S::SetPassphrase(raw)           => commands::key::cmd_set_passphrase(raw.argv("set-passphrase")).report(),
        S::RemovePassphrase(raw)        => commands::key::cmd_remove_passphrase(raw.argv("remove-passphrase")).report(),

        S::Migrate(raw)                 => commands::migrate::cmd_migrate(raw.argv("migrate")).report(),
        S::MigrateSuperblock(raw)       => commands::migrate::cmd_migrate_superblock(raw.argv("migrate-superblock")).report(),

        S::SetFileOption(raw)           => commands::attr::cmd_setattr(raw.argv("set-file-option")).report(),
        S::ReflinkOptionPropagate(raw)  => commands::attr::cmd_reflink_option_propagate(raw.argv("reflink-option-propagate")).report(),

        S::Dump(raw)                    => commands::dump::cmd_dump(raw.argv("dump")).report(),
        S::Undump(raw)                  => commands::dump::cmd_undump(raw.argv("undump")).report(),
        S::List(raw)                    => commands::list::list(raw.argv("list")).report(),
        S::ListJournal(raw)             => commands::list_journal::cmd_list_journal(raw.argv("list_journal")).report(),
        S::KillBtreeNode(raw)           => commands::kill_btree_node::cmd_kill_btree_node(raw.argv("kill_btree_node")).report(),
        S::DataRead(raw)                => commands::data_read::cmd_data_read(raw.argv("data-read")).report(),
        S::Unpoison(raw)                => commands::unpoison::cmd_unpoison(raw.argv("unpoison")).report(),

        S::Completions(raw)             => { commands::completions::completions(raw.argv("completions")); ExitCode::SUCCESS },
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
        S::Fusemount(raw)              => commands::fusemount::cmd_fusemount(raw.args).report(),
    }
}
