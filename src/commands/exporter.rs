//! Prometheus exporter for a mounted filesystem.
//!
//! Serves the same two sources `fs top` reads: persistent counters over
//! BCH_IOCTL_QUERY_COUNTERS, and per-device IO from sysfs `io_done`. Both
//! are exact.
//!
//! This exists because the per-counter sysfs files are not. Sector-typed
//! counters go through prt_human_readable_u64() there, which rounds to three
//! significant digits — at 56 TB `data_read` only moves in ~50 GB steps, so a
//! scraper sees nothing at all for most workloads, and rate() over a short
//! window can be off by multiples when it does step. The ioctl has no such
//! problem and needs no privileges: BCH_IOCTL_QUERY_COUNTERS is dispatched
//! without a capable() check, and the fd is just an open fd on the mount.
//!
//! Counters are exported cumulative-since-filesystem-creation only. The
//! since-mount values `fs top` also shows would reset on remount, which is
//! exactly what a Prometheus counter must not do.


use anyhow::{Context, Result};
use bcachefs_kernel::sb::io::{COUNTERS, CounterInfo};
use clap::Parser;

use crate::commands::DeviceNameArgs;
use crate::wrappers::counters::read_counters;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::{DeviceNameMode, read_device_io, sysfs_path_from_fd};

/// Counters whose x-macro description reads as a fault counter but is not.
fn help_text(c: &CounterInfo) -> String {
    let desc = match c.name {
        "error_throw" => "Error-code returns, including routine control flow: \
                          transaction restarts, declined promotions, ratelimits",
        _ if c.desc.is_empty() => return format!("bcachefs {} counter", c.name),
        _ => c.desc,
    };

    // Sector-typed counters are described in sectors but exported in bytes.
    if c.is_sectors {
        if let Some(rest) = desc.strip_prefix("Sectors") {
            return format!("Bytes{rest}");
        }
    }
    desc.to_string()
}

fn metric_name(c: &CounterInfo) -> String {
    if c.is_sectors {
        format!("bcachefs_{}_bytes_total", c.name)
    } else {
        format!("bcachefs_{}_total", c.name)
    }
}

fn escape_help(s: &str) -> String {
    s.replace('\\', r"\\").replace('\n', r"\n")
}

fn render(handle: &BcachefsHandle, uuid: &str, name_mode: DeviceNameMode) -> Result<String> {
    let nr_stable = COUNTERS.iter().map(|c| c.stable_id).max().unwrap_or(0) + 1;
    let vals = read_counters(handle.ioctl_fd_raw(), 0, nr_stable)
        .context("querying counters")?;

    let mut out = String::with_capacity(24 * 1024);

    for c in COUNTERS {
        // A kernel older than this build knows fewer counters than we do.
        let Some(&raw) = vals.get(c.stable_id as usize) else { continue };
        let value = if c.is_sectors { raw << 9 } else { raw };
        let name = metric_name(c);

        out.push_str(&format!("# HELP {name} {}\n", escape_help(&help_text(c))));
        out.push_str(&format!("# TYPE {name} counter\n"));
        out.push_str(&format!("{name}{{uuid=\"{uuid}\"}} {value}\n"));
    }

    let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;
    let dev_io = read_device_io(&sysfs_path, name_mode);
    if !dev_io.is_empty() {
        const DEV: &str = "bcachefs_device_io_bytes_total";
        out.push_str(&format!("# HELP {DEV} Bytes transferred, by device and data type\n"));
        out.push_str(&format!("# TYPE {DEV} counter\n"));
        for e in &dev_io {
            for (op, v) in [("read", e.read_bytes), ("write", e.write_bytes)] {
                out.push_str(&format!(
                    "{DEV}{{uuid=\"{uuid}\",device=\"{}\",data_type=\"{}\",operation=\"{op}\"}} {v}\n",
                    e.dev, e.data_type));
            }
        }
    }

    Ok(out)
}

fn serve(handle: BcachefsHandle, listen: &str, name_mode: DeviceNameMode) -> Result<()> {
    let server = tiny_http::Server::http(listen)
        .map_err(|e| anyhow::anyhow!("binding {listen}: {e}"))?;

    // A scraper that disconnects mid-response must not kill the process; the
    // write then fails with EPIPE, which we log and survive.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }

    let uuid = sysfs_path_from_fd(handle.sysfs_fd())?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    eprintln!("bcachefs exporter: serving {uuid} on http://{listen}/metrics");

    for request in server.incoming_requests() {
        let response = match (request.method(), request.url()) {
            (tiny_http::Method::Get, "/metrics") => match render(&handle, &uuid, name_mode) {
                Ok(body) => tiny_http::Response::from_string(body)
                    .with_header(tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/plain; version=0.0.4"[..],
                    ).expect("static header"))
                    .boxed(),
                Err(e) => {
                    eprintln!("bcachefs exporter: collecting metrics: {e:#}");
                    tiny_http::Response::from_string(format!("{e:#}\n"))
                        .with_status_code(500)
                        .boxed()
                }
            },
            (tiny_http::Method::Get, "/") => tiny_http::Response::from_data(
                b"<a href=\"/metrics\">/metrics</a>\n".to_vec(),
            ).boxed(),
            (tiny_http::Method::Get, _) => tiny_http::Response::empty(404).boxed(),
            _ => tiny_http::Response::empty(405).boxed(),
        };

        if let Err(e) = request.respond(response) {
            eprintln!("bcachefs exporter: sending response: {e}");
        }
    }

    Ok(())
}

#[derive(Parser, Debug)]
#[command(about = "Serve counters in Prometheus format")]
pub struct Cli {
    /// Address to listen on
    #[arg(short, long, default_value = "127.0.0.1:9101")]
    listen: String,

    #[command(flatten)]
    device_names: DeviceNameArgs,

    /// Filesystem path, device, or UUID (default: current directory)
    filesystem: Option<String>,
}

fn exporter(cli: Cli) -> Result<()> {
    let path = cli.filesystem.unwrap_or_else(|| ".".to_string());
    let handle = BcachefsHandle::open(&path)
        .map_err(|e| anyhow::anyhow!("opening filesystem '{path}': {}", e))?;

    serve(handle, &cli.listen, cli.device_names.name_mode())
}

pub const CMD: super::CmdDef =
    typed_cmd!("exporter", "Serve counters in Prometheus format", Cli, exporter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_counters_are_exported_as_bytes() {
        let c = COUNTERS.iter().find(|c| c.name == "data_read").unwrap();
        assert!(c.is_sectors);
        assert_eq!(metric_name(c), "bcachefs_data_read_bytes_total");
        assert!(help_text(c).starts_with("Bytes"),
                "sector description not restated in bytes: {}", help_text(c));
    }

    #[test]
    fn event_counters_keep_their_description() {
        let c = COUNTERS.iter().find(|c| c.name == "data_read_nopromote").unwrap();
        assert!(!c.is_sectors);
        assert_eq!(metric_name(c), "bcachefs_data_read_nopromote_total");
        assert_eq!(help_text(c), c.desc);
    }

    #[test]
    fn error_throw_help_is_overridden() {
        let c = COUNTERS.iter().find(|c| c.name == "error_throw").unwrap();
        assert_ne!(help_text(c), c.desc);
        assert!(help_text(c).contains("transaction restarts"));
    }
}
