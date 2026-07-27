//! Persistent counter queries over BCH_IOCTL_QUERY_COUNTERS.
//!
//! The ioctl returns the raw u64 counters, indexed by stable id. Sysfs
//! exposes the same values but runs the sector-typed ones through
//! prt_human_readable_u64(), which rounds to three significant digits — so
//! anything that needs exact values reads them here instead.
//!
//! Values for counters flagged TYPE_SECTORS are in sectors; callers shift
//! them to bytes for display. Everything else is an event count.

use std::mem;

use anyhow::Result;
use bch_bindgen::c::bch_ioctl_query_counters;

use crate::wrappers::ioctl::bch_ioc_w;

const BCH_IOCTL_QUERY_COUNTERS_NR: u32 = 21;

/// Report each counter's value at mount time rather than now.
pub const QUERY_COUNTERS_MOUNT: u16 = 1 << 0;

/// Read the counter array, indexed by stable id.
///
/// `nr_stable` bounds the reply; the kernel may return fewer entries than
/// asked for when it knows fewer counters than this build does.
pub fn read_counters(fd: i32, flags: u16, nr_stable: u16) -> Result<Vec<u64>> {
    let hdr_size = mem::size_of::<bch_ioctl_query_counters>();
    let buf_size = hdr_size + (nr_stable as usize) * mem::size_of::<u64>();
    let mut buf = vec![0u8; buf_size];

    unsafe {
        let hdr = &mut *(buf.as_mut_ptr() as *mut bch_ioctl_query_counters);
        hdr.nr = nr_stable;
        hdr.flags = flags;
    }

    let request = bch_ioc_w::<bch_ioctl_query_counters>(BCH_IOCTL_QUERY_COUNTERS_NR);
    let ret = unsafe { libc::ioctl(fd, request, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let actual_nr = unsafe { (*(buf.as_ptr() as *const bch_ioctl_query_counters)).nr } as usize;
    let data = unsafe { buf.as_ptr().add(hdr_size) as *const u64 };
    Ok((0..actual_nr).map(|i| unsafe { std::ptr::read_unaligned(data.add(i)) }).collect())
}
