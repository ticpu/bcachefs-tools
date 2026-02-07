use crate::bcachefs;
use crate::c;
use std::ffi::{c_int, CStr};
use std::fmt;

pub use crate::c::bch_errcode;

/// Safe wrapper for bcachefs/errno error codes.
/// Stores the positive error code — either a standard errno (1..2047)
/// or a bcachefs-specific code (BCH_ERR_START=2048..).
///
/// Unlike `bch_errcode` (a repr(u32) enum), this never creates an invalid
/// enum discriminant from a raw errno value.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BchError(i32);

impl BchError {
    pub fn from_raw(code: i32) -> Self { Self(code) }
    pub fn raw(&self) -> i32 { self.0 }

    pub fn matches(&self, class: bch_errcode) -> bool {
        if self.0 != 0 {
            unsafe { c::__bch2_err_matches(self.0, class as i32) }
        } else {
            false
        }
    }
}

impl fmt::Display for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = unsafe { CStr::from_ptr(bcachefs::bch2_err_str(self.0)) };
        write!(f, "{}", s.to_str().unwrap_or("unknown error"))
    }
}

impl fmt::Debug for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BchError({}, {})", self.0, self)
    }
}

impl std::error::Error for BchError {}

pub fn ret_to_result(ret: c_int) -> Result<c_int, BchError> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(ret)
    }
}

pub fn errptr_to_result<T>(p: *mut T) -> Result<*mut T, BchError> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}

pub fn errptr_to_result_c<T>(p: *const T) -> Result<*const T, BchError> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}
