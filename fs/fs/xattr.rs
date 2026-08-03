// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_void, CStr};

use crate::btree::bkey::BkeyValSC;
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransError,
};
use crate::c;
use crate::errcode::{self, BchError};
use crate::fs::Fs;

/// The value bytes of an xattr key follow its name in the flexible array.
fn xattr_value(x: &c::bch_xattr) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(
            x.x_name_and_value.as_ptr().add(x.x_name_len as usize),
            u16::from_le(x.x_val_len) as usize,
        )
    }
}

/// Mirrors `struct xattr_search_key` (fs/xattr.h), the key bch2_xattr_hash_desc
/// hashes over. Bindgen does not emit it: only the X_SEARCH macro constructs it,
/// so no bound signature references the type.
#[repr(C)]
struct XattrSearchKey {
    type_: u8,
    name:  c::qstr,
}

/// The short (namespace-stripped) name of an xattr key.
fn xattr_name(x: &c::bch_xattr) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(x.x_name_and_value.as_ptr(), x.x_name_len as usize)
    }
}

/// Set an xattr, or remove it when `val` is `None`. bch2_xattr_set selects
/// removal on a NULL value pointer, so an empty slice is a zero-length set
/// rather than a delete — the distinction has to survive in the signature.
pub fn set<'a, 't>(
    t:     TransAttempt<'a, 't>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &CStr,
    val:   Option<&[u8]>,
    typ:   i32,
    flags: i32,
) -> Result<TransAttempt<'a, 't>, TransError> {
    let (ptr, len) = match val {
        Some(v) => (v.as_ptr() as *const c_void, v.len()),
        None    => (core::ptr::null(), 0),
    };
    let ret = unsafe {
        c::bch2_xattr_set(t.raw(), inum, inode, name.as_ptr(), ptr, len, typ, flags)
    };
    t.result(ret)
}

/// Look up one xattr value. Returns `BCH_ERR`/ENOENT when the attribute is
/// not set; callers wanting getxattr(2) semantics map that to ENODATA.
#[cfg(feature = "std")]
pub fn get(
    fs:        &Fs,
    inum:      c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &CStr,
    typ:       u8,
) -> Result<Vec<u8>, BchError> {
    let name_bytes = name.to_bytes();
    let search = XattrSearchKey {
        type_: typ,
        name:  crate::dirent::qstr(name_bytes),
    };

    let trans = BtreeTrans::new(fs);
    crate::btree::iter::lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::uninit();
        let k = unsafe {
            c::bch2_hash_lookup(
                t.raw(),
                iter.raw_mut(),
                core::ptr::read(&raw const c::bch2_xattr_hash_desc),
                hash_info,
                inum,
                &search as *const _ as *const c_void,
                c::btree_iter_update_trigger_flags(0),
            )
        };

        // bkey_s_c carries the error in its k pointer.
        errcode::errptr_to_result_c(k.k)?;

        let val = match unsafe { BkeyValSC::from_raw(&*k.k, &*k.v) } {
            BkeyValSC::xattr(_, x) => xattr_value(x).to_vec(),
            _ => return Err(errcode::bch_err_throw(
                c::bch_errcode::BCH_ERR_ENOENT_str_hash_lookup).into()),
        };
        Ok((t, val))
    })
}

/// Collect every xattr on an inode as (type index, short name) pairs.
/// The caller re-attaches the namespace prefix it understands.
#[cfg(feature = "std")]
pub fn list(fs: &Fs, inum: c::subvol_inum) -> Result<Vec<(u8, Vec<u8>)>, BchError> {
    let mut snapshot = 0u32;
    let trans = BtreeTrans::new(fs);
    crate::btree::iter::lockrestart_do(&trans, |t| {
        let ret = unsafe {
            c::bch2_subvolume_get_snapshot(t.raw(), inum.subvol as u32, &mut snapshot)
        };
        let t = t.result(ret)?;
        Ok((t, ()))
    })?;

    let mut out = Vec::new();
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::BTREE_ID_xattrs,
        c::bpos { inode: inum.inum, offset: 0, snapshot },
        BtreeIterFlags::empty(),
    );

    iter.for_each_max(
        &trans,
        c::bpos { inode: inum.inum, offset: u64::MAX, snapshot },
        |k| {
            if let BkeyValSC::xattr(_, x) = k.v() {
                out.push((x.x_type, xattr_name(x).to_vec()));
            }
            core::ops::ControlFlow::Continue(())
        },
    )?;

    Ok(out)
}
