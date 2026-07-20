use std::ffi;
use std::fmt;

use crate::PLUGIN_NAME;

pub fn obs_log_args(log_level: u32, args: fmt::Arguments) {
    let message = format!("[{PLUGIN_NAME}] {args}");
    let message = ffi::CString::new(message.replace('\0', "\\0")).unwrap();

    unsafe {
        obs_sys::blog(log_level.cast_signed(), message.as_ptr());
    }
}

#[macro_export]
macro_rules! obs_log {
    ($level:expr, $($arg:tt)*) => {{
        $crate::log::obs_log_args($level, format_args!($($arg)*))
    }};
}

#[macro_export]
macro_rules! obs_debug {
    ($($arg:tt)*) => {{
        $crate::obs_log!(obs_sys::LOG_DEBUG.try_into().unwrap(), $($arg)*)
    }};
}

#[macro_export]
macro_rules! obs_info {
    ($($arg:tt)*) => {{
        $crate::obs_log!(obs_sys::LOG_INFO.try_into().unwrap(), $($arg)*)
    }};
}

#[macro_export]
macro_rules! obs_warning {
    ($($arg:tt)*) => {{
        $crate::obs_log!(obs_sys::LOG_WARNING.try_into().unwrap(), $($arg)*)
    }};
}

#[macro_export]
macro_rules! obs_error {
    ($($arg:tt)*) => {{
        $crate::obs_log!(obs_sys::LOG_ERROR.try_into().unwrap(), $($arg)*)
    }};
}
