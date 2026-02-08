use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::io::{self, Write as IoWrite};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bch_bindgen::c::bch_counters_flags;
use bch_bindgen::c::bch_persistent_counters::BCH_COUNTER_NR;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::dev_name_from_sysfs;

// ioctl constants

const BCH_IOCTL_QUERY_COUNTERS_NR: u32 = 21;
const BCH_IOCTL_QUERY_COUNTERS_MOUNT: u16 = 1 << 0;

const fn bch_ioc_w<T>(nr: u32) -> libc::c_ulong {
    ((1u32 << 30) | ((mem::size_of::<T>() as u32) << 16) | (0xbcu32 << 8) | nr) as libc::c_ulong
}

// Counter info from bindgen

struct CounterInfo {
    names:      Vec<String>,
    flags:      Vec<bch_counters_flags>,
    stable_map: Vec<u16>,
}

fn counter_info() -> CounterInfo {
    let nr = BCH_COUNTER_NR as usize;
    let mut names = Vec::with_capacity(nr);
    let mut flags = Vec::with_capacity(nr);
    let mut stable_map = Vec::with_capacity(nr);

    // bch2_counter_names is a NULL-terminated extern array (bindgen size 0),
    // bch2_counter_flags is a static const array (bindgen size 131),
    // bch2_counter_flags_map and bch2_counter_stable_map are extern arrays (size 0).
    // Access all via raw pointers for uniformity.
    unsafe {
        let names_ptr = bch_bindgen::c::bch2_counter_names.as_ptr();
        let flags_ptr = bch_bindgen::c::bch2_counter_flags_map.as_ptr();
        let stable_ptr = bch_bindgen::c::bch2_counter_stable_map.as_ptr();

        for i in 0..nr {
            let name_ptr = *names_ptr.add(i);
            let name = if name_ptr.is_null() {
                format!("counter_{}", i)
            } else {
                CStr::from_ptr(name_ptr).to_string_lossy().to_string()
            };
            names.push(name);
            flags.push(*flags_ptr.add(i));
            stable_map.push(*stable_ptr.add(i));
        }
    }

    CounterInfo { names, flags, stable_map }
}

// ioctl query

#[repr(C)]
#[derive(Copy, Clone)]
struct BchIoctlQueryCounters {
    nr:    u16,
    flags: u16,
    pad:   u32,
}

fn read_counters(fd: i32, flags: u16, nr_stable: u16) -> Result<Vec<u64>> {
    let hdr_size = mem::size_of::<BchIoctlQueryCounters>();
    let buf_size = hdr_size + (nr_stable as usize) * mem::size_of::<u64>();
    let mut buf = vec![0u8; buf_size];

    let hdr = BchIoctlQueryCounters {
        nr:    nr_stable,
        flags,
        pad:   0,
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const _ as *const u8,
            buf.as_mut_ptr(),
            hdr_size,
        );
    }

    let request = bch_ioc_w::<BchIoctlQueryCounters>(BCH_IOCTL_QUERY_COUNTERS_NR);
    let ret = unsafe { libc::ioctl(fd, request, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let returned_hdr = unsafe { &*(buf.as_ptr() as *const BchIoctlQueryCounters) };
    let actual_nr = returned_hdr.nr as usize;

    let mut values = vec![0u64; actual_nr];
    for i in 0..actual_nr {
        values[i] = unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr().add(hdr_size + i * mem::size_of::<u64>()) as *const u64
            )
        };
    }

    Ok(values)
}

// Per-device IO from sysfs
//
// io_done format (values in bytes):
//   read:
//   sb          :       12345
//   journal     :           0
//   ...
//   write:
//   sb          :       12345
//   ...

struct DevIoEntry {
    label:      String,     // "dev/data_type"
    read_bytes: u64,
    write_bytes: u64,
}

fn parse_io_done(dev_name: &str, text: &str) -> Vec<DevIoEntry> {
    let mut entries: Vec<(String, u64, u64)> = Vec::new();
    let mut in_write = false;

    for line in text.lines() {
        let line = line.trim();
        if line == "read:" {
            in_write = false;
            continue;
        }
        if line == "write:" {
            in_write = true;
            continue;
        }
        if let Some((name, val_str)) = line.split_once(':') {
            let name = name.trim();
            if let Ok(v) = val_str.trim().parse::<u64>() {
                if let Some(entry) = entries.iter_mut().find(|(n, _, _)| n == name) {
                    if in_write { entry.2 = v; } else { entry.1 = v; }
                } else if in_write {
                    entries.push((name.to_string(), 0, v));
                } else {
                    entries.push((name.to_string(), v, 0));
                }
            }
        }
    }

    entries.into_iter()
        .filter(|(_, r, w)| *r != 0 || *w != 0)
        .map(|(dtype, r, w)| DevIoEntry {
            label: format!("{}/{}", dev_name, dtype),
            read_bytes: r,
            write_bytes: w,
        })
        .collect()
}

fn read_device_io(sysfs_path: &Path) -> Vec<DevIoEntry> {
    let mut entries = Vec::new();
    let Ok(dir) = fs::read_dir(sysfs_path) else { return entries };

    for entry in dir.flatten() {
        let dirname = entry.file_name().to_string_lossy().to_string();
        if !dirname.starts_with("dev-") { continue }

        let dev_path = entry.path();
        let dev_name = dev_name_from_sysfs(&dev_path);

        let io_done_path = dev_path.join("io_done");
        let Ok(content) = fs::read_to_string(&io_done_path) else { continue };

        entries.extend(parse_io_done(&dev_name, &content));
    }
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    entries
}

// Human-readable formatting

fn fmt_bytes(bytes: u64, human_readable: bool) -> String {
    if human_readable { fmt_bytes_human(bytes) } else { format!("{}", bytes) }
}

fn is_sectors(flags: bch_counters_flags) -> bool {
    flags == bch_counters_flags::TYPE_SECTORS
}

fn fmt_counter(val: u64, sectors: bool, human_readable: bool) -> String {
    if sectors {
        let bytes = val << 9;
        if human_readable {
            fmt_bytes_human(bytes)
        } else {
            format!("{}", bytes)
        }
    } else if human_readable && val >= 10_000 {
        fmt_num_human(val)
    } else {
        format!("{}", val)
    }
}

fn fmt_bytes_human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes == 0 { return "0B".to_string() }
    let mut val = bytes as f64;
    for unit in UNITS {
        if val < 1024.0 || *unit == "PiB" {
            return if val >= 100.0 {
                format!("{:.0}{}", val, unit)
            } else if val >= 10.0 {
                format!("{:.1}{}", val, unit)
            } else {
                format!("{:.2}{}", val, unit)
            };
        }
        val /= 1024.0;
    }
    format!("{}B", bytes)
}

fn fmt_num_human(n: u64) -> String {
    const UNITS: &[&str] = &["", "K", "M", "G", "T"];
    let mut val = n as f64;
    for unit in UNITS {
        if val < 1000.0 || *unit == "T" {
            return if val >= 100.0 {
                format!("{:.0}{}", val, unit)
            } else if val >= 10.0 {
                format!("{:.1}{}", val, unit)
            } else if unit.is_empty() {
                format!("{}", n)
            } else {
                format!("{:.2}{}", val, unit)
            };
        }
        val /= 1000.0;
    }
    format!("{}", n)
}

// CLI

#[derive(Parser, Debug)]
#[command(about = "Display runtime performance info")]
pub struct Cli {
    /// Human-readable units
    #[arg(short, long)]
    human_readable: bool,

    /// Filesystem path, device, or UUID (default: current directory)
    filesystem: Option<String>,
}

// TUI state

struct TopState {
    info:           CounterInfo,
    ioctl_fd:       i32,
    nr_stable:      u16,
    mount_vals:     Vec<u64>,
    start_vals:     Vec<u64>,
    prev_vals:      Vec<u64>,
    prev_dev_io:    HashMap<String, (u64, u64)>,    // label -> (read, write)
    human_readable: bool,
    show_devices:   bool,
    sysfs_path:     PathBuf,
    interval_secs:  u32,
}

fn sysfs_path_from_fd(fd: i32) -> Result<PathBuf> {
    let link = format!("/proc/self/fd/{}", fd);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", fd))
}

impl TopState {
    fn new(handle: &BcachefsHandle, human_readable: bool) -> Result<Self> {
        let info = counter_info();
        let ioctl_fd = handle.ioctl_fd_raw();
        let nr_stable = info.stable_map.iter().copied().max().unwrap_or(0) + 1;

        let mount_vals = read_counters(ioctl_fd, BCH_IOCTL_QUERY_COUNTERS_MOUNT, nr_stable)?;
        let start_vals = read_counters(ioctl_fd, 0, nr_stable)?;
        let prev_vals  = read_counters(ioctl_fd, 0, nr_stable)?;

        let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;

        Ok(TopState {
            info, ioctl_fd, nr_stable,
            mount_vals, start_vals, prev_vals,
            prev_dev_io: HashMap::new(),
            human_readable, show_devices: true,
            sysfs_path, interval_secs: 1,
        })
    }

    fn get_val(vals: &[u64], stable_id: u16) -> u64 {
        let idx = stable_id as usize;
        if idx < vals.len() { vals[idx] } else { 0 }
    }

    fn render(&self, curr: &[u64], dev_io: &[DevIoEntry], stdout: &mut io::Stdout) -> io::Result<()> {
        execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

        write!(stdout, "All counters have a corresponding tracepoint; for more info on any given event, try e.g.\r\n")?;
        write!(stdout, "  perf trace -e bcachefs:data_update_pred\r\n\r\n")?;
        write!(stdout, "  q:quit  h:human-readable  d:devices  1-9:interval\r\n\r\n")?;

        write!(stdout, "{:<40} {:>14} {:>14} {:>14}\r\n",
            "", format!("{}/s", self.interval_secs), "total", "mount")?;

        let nr = self.info.names.len();
        for i in 0..nr {
            let stable = self.info.stable_map[i];
            let cv = Self::get_val(curr, stable);
            let pv = Self::get_val(&self.prev_vals, stable);
            let sv = Self::get_val(&self.start_vals, stable);
            let mv = Self::get_val(&self.mount_vals, stable);

            let v_mount = cv.wrapping_sub(mv);
            if v_mount == 0 { continue }

            let v_rate  = cv.wrapping_sub(pv);
            let v_total = cv.wrapping_sub(sv);

            let sectors = is_sectors(self.info.flags[i]);

            write!(stdout, "{:<40} {:>12}/s {:>14} {:>14}\r\n",
                &self.info.names[i],
                fmt_counter(v_rate / self.interval_secs as u64, sectors, self.human_readable),
                fmt_counter(v_total, sectors, self.human_readable),
                fmt_counter(v_mount, sectors, self.human_readable))?;
        }

        if self.show_devices && !dev_io.is_empty() {
            write!(stdout, "\r\nPer-device IO:\r\n")?;
            write!(stdout, "{:<40} {:>14} {:>14} {:>14} {:>14}\r\n",
                "", "read/s", "read", "write/s", "write")?;
            for dev in dev_io {
                let (prev_r, prev_w) = self.prev_dev_io
                    .get(&dev.label)
                    .copied()
                    .unwrap_or((dev.read_bytes, dev.write_bytes));
                let rate_r = dev.read_bytes.wrapping_sub(prev_r) / self.interval_secs as u64;
                let rate_w = dev.write_bytes.wrapping_sub(prev_w) / self.interval_secs as u64;

                let h = self.human_readable;
                write!(stdout, "{:<40} {:>14} {:>14} {:>14} {:>14}\r\n",
                    &dev.label,
                    fmt_bytes(rate_r, h), fmt_bytes(dev.read_bytes, h),
                    fmt_bytes(rate_w, h), fmt_bytes(dev.write_bytes, h))?;
            }
        }

        stdout.flush()
    }
}

fn run_interactive(handle: BcachefsHandle, human_readable: bool) -> Result<()> {
    let mut state = TopState::new(&handle, human_readable)?;
    let mut stdout = io::stdout();

    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = (|| -> Result<()> {
        loop {
            let curr = read_counters(state.ioctl_fd, 0, state.nr_stable)?;
            let dev_io = read_device_io(&state.sysfs_path);
            state.render(&curr, &dev_io, &mut stdout)?;
            state.prev_vals = curr;
            state.prev_dev_io = dev_io.into_iter()
                .map(|d| (d.label, (d.read_bytes, d.write_bytes)))
                .collect();

            // Wait for interval or keypress
            if event::poll(Duration::from_secs(state.interval_secs as u64))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        KeyCode::Char('h') => state.human_readable = !state.human_readable,
                        KeyCode::Char('d') => state.show_devices = !state.show_devices,
                        KeyCode::Char(c @ '1'..='9') => {
                            state.interval_secs = (c as u32) - ('0' as u32);
                        }
                        _ => {}
                    }
                }
                // Drain remaining events
                while event::poll(Duration::ZERO)? { let _ = event::read()?; }
            }
        }
    })();

    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    result
}

pub fn top(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let fs_arg = cli.filesystem.as_deref().unwrap_or(".");
    let handle = BcachefsHandle::open(fs_arg)
        .map_err(|e| anyhow!("Failed to open filesystem '{}': {}", fs_arg, e))?;

    run_interactive(handle, cli.human_readable)
}
