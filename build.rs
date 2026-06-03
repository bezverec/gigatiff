use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let libtiff_dir = env::var_os("LIBTIFF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\temp\libtiff\install"));

    let lib_dir = libtiff_dir.join("lib");
    let bin_dir = libtiff_dir.join("bin");
    let dll = bin_dir.join("tiff.dll");

    println!("cargo:rerun-if-env-changed=LIBTIFF_DIR");
    println!("cargo:rerun-if-changed={}", dll.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=tiff");

    if dll.exists() {
        if let Some(profile_dir) = profile_dir() {
            let _ = fs::copy(&dll, profile_dir.join("tiff.dll"));
        }
    }
}

fn profile_dir() -> Option<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    out_dir.ancestors().nth(3).map(|path| path.to_path_buf())
}
