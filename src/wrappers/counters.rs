//! Persistent counter queries over BCH_IOCTL_QUERY_COUNTERS.
//!
//! The ioctl returns the raw u64 counters, indexed by stable id. Sysfs
//! exposes the same values but runs the sector-typed ones through
//! prt_human_readable_u64(), which rounds to three significant digits — so
//! anything that needs exact values reads them here instead.
//!
//! Values for counters flagged TYPE_SECTORS are in sectors; callers shift
//! them to bytes for display. Everything else is an event count.

use std::os::fd::AsFd;

use anyhow::Result;
use bch_bindgen::c::bch_ioctl_query_counters;

use crate::wrappers::ioctl::{BCH_IOCTL_QUERY_COUNTERS, IoctlBuf, ioctl_ptr};

/// Report each counter's value at mount time rather than now.
pub const QUERY_COUNTERS_MOUNT: u16 = 1 << 0;

/// Read the counter array, indexed by stable id.
///
/// `nr_stable` bounds the reply; the kernel may return fewer entries than
/// asked for when it knows fewer counters than this build does.
pub fn read_counters(fd: impl AsFd, flags: u16, nr_stable: u16) -> Result<Vec<u64>> {
    let mut buf = IoctlBuf::<bch_ioctl_query_counters>::new::<u64>(nr_stable as usize);
    let hdr = buf.hdr_mut();
    hdr.nr = nr_stable;
    hdr.flags = flags;

    unsafe { ioctl_ptr::<BCH_IOCTL_QUERY_COUNTERS>(fd, buf.as_mut_ptr())? };

    let nr = (buf.hdr().nr as usize).min(nr_stable as usize);
    Ok(unsafe { buf.hdr().d.as_slice(nr) }.to_vec())
}
