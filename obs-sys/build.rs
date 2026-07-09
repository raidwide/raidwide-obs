use std::env;
use std::path;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");

    println!("cargo:rustc-link-lib=obs");
    println!("cargo:rustc-link-lib=obs-frontend-api");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/usr/include/obs")
        .allowlist_item("obs_.*")
        .allowlist_item("OBS_.*")
        .allowlist_var("LIBOBS_.*") // LIBOBS_API_MAJOR_VER, LIBOBS_API_MINOR_VER, LIBOBS_API_PATCH_VER
        .allowlist_var("LOG_.*") // LOG_ERROR, LOG_WARNING, LOG_INFO, LOG_DEBUG
        .allowlist_function("blog")
        .allowlist_function("brealloc")
        // TODO: add missing stuff (like brealloc)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = path::PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
