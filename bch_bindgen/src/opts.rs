use crate::c;
use crate::fs::Fs;
use std::ffi::{CString, c_char};

#[macro_export]
macro_rules! opt_set {
    ($opts:ident, $n:ident, $v:expr) => {
        bch_bindgen::paste! {
            $opts.$n = $v;
            $opts.[<set_ $n _defined>](1)
        }
    };
}

#[macro_export]
macro_rules! opt_defined {
    ($opts:ident, $n:ident) => {
        bch_bindgen::paste! {
            $opts.[< $n _defined>]()
        }
    };
}

#[macro_export]
macro_rules! opt_get {
    ($opts:ident, $n:ident) => {
        if bch_bindgen::opt_defined!($opts, $n) == 0 {
            bch_bindgen::paste! {
                unsafe {
                    bch_bindgen::bcachefs::bch2_opts_default.$n
                }
            }
        } else {
            bch_bindgen::paste! {
                $opts.$n
            }
        }
    };
}

pub fn parse_mount_opts(fs: Option<&mut Fs>, optstr: Option<&str>, ignore_unknown: bool)
        -> Result<c::bch_opts, c::bch_errcode> {
    let mut opts: c::bch_opts = Default::default();

    if let Some(optstr) = optstr {
        let optstr = CString::new(optstr).unwrap();
        let optstr_ptr = optstr.as_ptr();

        let ret = unsafe {
            c::bch2_parse_mount_opts(fs.map_or(std::ptr::null_mut(), |f| f.raw),
                                     &mut opts as *mut c::bch_opts,
                                     std::ptr::null_mut(),
                                     optstr_ptr as *mut c_char,
                                     ignore_unknown)
        };

        drop(optstr);

        if ret != 0 {
            let err: c::bch_errcode = unsafe { std::mem::transmute(-ret) };
            return Err(err);
        }
    }
    Ok(opts)
}
