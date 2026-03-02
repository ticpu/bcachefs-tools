// SPDX-License-Identifier: GPL-2.0
//
// IO operations on a bcachefs filesystem.
//
// ReadOp is a genuine async Future: the C shim submits the read and
// returns immediately; the bio endio callback wakes the Rust waker
// when IO completes (from the libaio completion thread in userspace).
//
// WriteOp is still synchronous pending the same treatment.

use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use crate::c;
use crate::errcode::{self, BchError};
use crate::fs::Fs;

/// Maximum single IO size (must match RUST_IO_MAX in rust_shims.h).
pub const MAX_IO_SIZE: usize = 1 << 20;
const PAGE_SIZE: usize = 4096;
const MAX_BVECS: usize = MAX_IO_SIZE / PAGE_SIZE;

extern "C" {
    fn rust_write_data(
        c: *mut c::bch_fs,
        inum: u64,
        offset: u64,
        buf: *const std::ffi::c_void,
        len: usize,
        subvol: u32,
        replicas: u32,
        sectors_delta: *mut i64,
    ) -> i32;

    fn rust_read_submit(
        c: *mut c::bch_fs,
        rbio: *mut c::bch_read_bio,
        bvecs: *mut c::bio_vec,
        nr_bvecs: u32,
        buf: *mut std::ffi::c_void,
        len: usize,
        offset: u64,
        opts: c::bch_inode_opts,
        inum: c::subvol_inum,
        endio: c::bio_end_io_t,
    );
}

/// Result of a write operation.
pub struct WriteResult {
    pub sectors_delta: i64,
}

/// A bcachefs write operation (still synchronous).
pub struct WriteOp<'a> {
    fs: &'a Fs,
    inum: u64,
    offset: u64,
    subvol: u32,
    replicas: u32,
}

impl<'a> WriteOp<'a> {
    pub fn new(fs: &'a Fs, inum: u64, offset: u64, subvol: u32, replicas: u32) -> Self {
        Self { fs, inum, offset, subvol, replicas }
    }

    /// Submit the write. Data must be block-aligned and <= MAX_IO_SIZE.
    pub async fn submit(self, data: &[u8]) -> Result<WriteResult, BchError> {
        let mut sectors_delta: i64 = 0;
        let ret = unsafe {
            rust_write_data(
                self.fs.raw,
                self.inum,
                self.offset,
                data.as_ptr() as *const _,
                data.len(),
                self.subvol,
                self.replicas,
                &mut sectors_delta,
            )
        };
        errcode::ret_to_result(ret)?;
        Ok(WriteResult { sectors_delta })
    }
}

// Heap-allocated state for an in-flight read. The bch_read_bio is at
// offset 0 so the endio callback can cast bio → bch_read_bio → ReadState.
#[repr(C)]
struct ReadState {
    rbio:       c::bch_read_bio,
    bvecs:      [c::bio_vec; MAX_BVECS],
    completed:  AtomicBool,
    waker:      UnsafeCell<Option<Waker>>,
}

// Safety: the waker is only written from poll (single-threaded) and
// read from the endio callback. AtomicBool provides the ordering
// guarantee: the endio stores completed=true with Release, and poll
// loads with Acquire, so the waker write is visible before we read
// completed=true.
unsafe impl Send for ReadState {}
unsafe impl Sync for ReadState {}

/// Endio callback — called from the IO completion path (libaio thread).
/// Gets the bio pointer, walks up to ReadState via container_of, signals
/// completion and wakes the Rust future.
unsafe extern "C" fn read_endio(bio: *mut c::bio) {
    // container_of(bio, bch_read_bio, bio) — bio is the last field
    let rbio = (bio as *mut u8)
        .sub(std::mem::offset_of!(c::bch_read_bio, bio))
        as *mut c::bch_read_bio;
    // ReadState has rbio at offset 0
    let state = rbio as *mut ReadState;

    // Take and wake before setting completed, so the waker is consumed
    // before any future poll sees completed=true.
    let waker = (*(*state).waker.get()).take();
    (*state).completed.store(true, Ordering::Release);
    if let Some(w) = waker {
        w.wake();
    }
}

/// Async read operation on a bcachefs filesystem.
///
/// IO is submitted in new(); poll checks for completion.
pub struct ReadOp {
    state: Pin<Box<ReadState>>,
}

impl ReadOp {
    pub fn new(
        fs: &Fs,
        inum: c::subvol_inum,
        offset: u64,
        inode: &c::bch_inode_unpacked,
        buf: &mut [u8],
    ) -> Self {
        let mut opts: c::bch_inode_opts = unsafe { std::mem::zeroed() };
        unsafe {
            c::bch2_inode_opts_get_inode(
                fs.raw,
                inode as *const _ as *mut _,
                &mut opts,
            );
        }

        let state = Box::pin(ReadState {
            rbio:       unsafe { std::mem::zeroed() },
            bvecs:      unsafe { std::mem::zeroed() },
            completed:  AtomicBool::new(false),
            waker:      UnsafeCell::new(None),
        });

        unsafe {
            let state_ptr = &*state as *const ReadState as *mut ReadState;
            rust_read_submit(
                fs.raw,
                &raw mut (*state_ptr).rbio,
                (*state_ptr).bvecs.as_mut_ptr(),
                MAX_BVECS as u32,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                offset,
                opts,
                inum,
                Some(read_endio),
            );
        }

        Self { state }
    }
}

impl Future for ReadOp {
    type Output = Result<(), BchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.state.completed.load(Ordering::Acquire) {
            let ret = this.state.rbio.ret as i32;
            Poll::Ready(errcode::ret_to_result(ret).map(|_| ()))
        } else {
            unsafe {
                *this.state.waker.get() = Some(cx.waker().clone());
            }
            // Re-check after storing waker to avoid missed wakeup
            if this.state.completed.load(Ordering::Acquire) {
                let ret = this.state.rbio.ret as i32;
                Poll::Ready(errcode::ret_to_result(ret).map(|_| ()))
            } else {
                Poll::Pending
            }
        }
    }
}

/// Simple executor for futures — polls in a loop until Ready.
///
/// Handles both synchronous completion (ready on first poll) and
/// async completion (endio wakes from another thread).
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |p| RawWaker::new(p, &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
