use std::io;

use anyhow::Result;
use crossterm::{cursor, execute, terminal};

pub fn fmt_bytes_human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P"];
    if bytes == 0 { return "0B".to_string() }
    let mut val = bytes as f64;
    for unit in UNITS {
        if val < 1024.0 || *unit == "P" {
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

pub fn fmt_num_human(n: u64) -> String {
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

pub fn run_tui<F>(f: F) -> Result<()>
where F: FnOnce(&mut io::Stdout) -> Result<()>
{
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = f(&mut stdout);

    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    result
}
