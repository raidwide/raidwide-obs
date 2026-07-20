#![allow(dead_code)]

use std::ffi;
use std::ptr;

pub unsafe fn calldata_int(data: *mut obs_sys::calldata_t, name: &ffi::CStr) -> ffi::c_longlong {
    let mut val = 0;
    unsafe {
        obs_sys::calldata_get_data(
            data,
            name.as_ptr(),
            ptr::from_mut(&mut val).cast(),
            size_of_val(&val),
        )
    };
    val
}

pub unsafe fn calldata_float(data: *mut obs_sys::calldata_t, name: &ffi::CStr) -> ffi::c_double {
    let mut val = 0.0;
    unsafe {
        obs_sys::calldata_get_data(
            data,
            name.as_ptr(),
            ptr::from_mut(&mut val).cast(),
            size_of_val(&val),
        )
    };
    val
}

pub unsafe fn calldata_bool(data: *mut obs_sys::calldata_t, name: &ffi::CStr) -> bool {
    let mut val = false;
    unsafe {
        obs_sys::calldata_get_data(
            data,
            name.as_ptr(),
            ptr::from_mut(&mut val).cast(),
            size_of_val(&val),
        )
    };
    val
}

pub unsafe fn calldata_ptr(data: *mut obs_sys::calldata_t, name: &ffi::CStr) -> *mut ffi::c_void {
    let mut val: *mut ffi::c_void = ptr::null_mut();
    unsafe {
        obs_sys::calldata_get_data(
            data,
            name.as_ptr(),
            ptr::from_mut(&mut val).cast(),
            size_of_val(&val),
        )
    };
    val
}

pub unsafe fn calldata_string(
    data: *mut obs_sys::calldata_t,
    name: &ffi::CStr,
) -> *const ffi::c_char {
    let mut val: *const ffi::c_char = ptr::null_mut();
    unsafe {
        obs_sys::calldata_get_string(data, name.as_ptr(), ptr::from_mut(&mut val).cast());
    }
    val
}
