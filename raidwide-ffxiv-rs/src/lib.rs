mod calldata;
mod log;
mod output_hook;
mod sei;
mod source;
mod ws;

use std::panic;

static mut OBS_MODULE_POINTER: *mut obs_sys::obs_module_t = std::ptr::null_mut();

pub const PLUGIN_NAME: &str = env!("CARGO_PKG_NAME");
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run an OBS callback's body, logging a panic instead of letting it abort OBS
/// (a panic can't unwind out of an `extern "C"` function)
pub fn catch_panic<T>(callback: &str, default: T, body: impl FnOnce() -> T) -> T {
    match panic::catch_unwind(panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown cause");
            obs_error!("Panic in the {callback}: {message}");
            default
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut obs_sys::obs_module_t) {
    unsafe {
        OBS_MODULE_POINTER = module;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    obs_sys::LIBOBS_API_MAJOR_VER << 24
        | obs_sys::LIBOBS_API_MINOR_VER << 16
        | obs_sys::LIBOBS_API_PATCH_VER
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    obs_info!("Running rust plugin {PLUGIN_NAME} version {PLUGIN_VERSION}");

    obs_info!("Registering Raidwide source");
    unsafe {
        obs_sys::obs_register_source_s(
            &raw const source::RAIDWIDE_SOURCE,
            size_of::<obs_sys::obs_source_info>(),
        );
    }
    output_hook::load();

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    catch_panic("module unload", (), output_hook::unload);
    obs_info!("Plugin unloaded");
}
