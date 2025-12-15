use crate::bcachefs;
use crate::c;
use std::ffi::{c_int, CStr};
use std::fmt;

pub use crate::c::bch_errcode;

impl fmt::Display for bch_errcode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = unsafe { CStr::from_ptr(bcachefs::bch2_err_str(*self as i32)) };
        write!(f, "{:?}", s)
    }
}

pub fn ret_to_result(ret: c_int) -> Result<c_int, bch_errcode> {
    let max_err: c_int = -4096;
    if ret < 0 && ret > max_err {
        let err: bch_errcode = unsafe { std::mem::transmute(-ret) };
        Err(err)
    } else {
        Ok(ret)
    }
}

/* Can we make a function generic over ptr constness? */

pub fn errptr_to_result<T>(p: *mut T) -> Result<*mut T, bch_errcode> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        let addr = addr as i32;
        let err: bch_errcode = unsafe { std::mem::transmute(-addr) };
        Err(err)
    } else {
        Ok(p)
    }
}

pub fn errptr_to_result_c<T>(p: *const T) -> Result<*const T, bch_errcode> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        let addr = addr as i32;
        let err: bch_errcode = unsafe { std::mem::transmute(-addr) };
        Err(err)
    } else {
        Ok(p)
    }
}

impl std::error::Error for bch_errcode {}

pub fn bch2_err_matches(err: bch_errcode, class: bch_errcode) -> bool {
    if err as i32 != 0 {
        unsafe { c::__bch2_err_matches(err as i32, class as i32) }
    } else {
        false
    }
}
