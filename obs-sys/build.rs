use std::env;
use std::path;

#[cfg(windows)]
mod build_win;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");

    println!("cargo:rustc-link-lib=obs");
    println!("cargo:rustc-link-lib=obs-frontend-api");

    #[cfg(windows)]
    build_win::find_windows_obs_lib();

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args([
            #[cfg(windows)]
            "-Wno-error=implicit-function-declaration",
            "-I/usr/include/obs",
            "-I./obs-studio/libobs/",
            "-I./obs-studio/build_x64/config/",
            "-I./obs-studio/frontend/api/",
        ])
        .allowlist_item("obs_.*")
        .allowlist_item("OBS_.*")
        .allowlist_var("LIBOBS_.*") // LIBOBS_API_MAJOR_VER, LIBOBS_API_MINOR_VER, LIBOBS_API_PATCH_VER
        .allowlist_var("LOG_.*") // LOG_ERROR, LOG_WARNING, LOG_INFO, LOG_DEBUG
        .allowlist_function("signal_handler_.*")
        .allowlist_function("calldata_.*")
        .allowlist_function("blog")
        .allowlist_function("bmalloc")
        .allowlist_function("brealloc")
        .allowlist_function("bfree")
        .allowlist_function("bcrash")
        .allowlist_function("os_gettime_ns")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = path::PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
