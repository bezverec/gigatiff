use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=LIBTIFF_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        link_windows_libtiff();
    } else if let Some(libtiff_dir) = env::var_os("LIBTIFF_DIR").map(PathBuf::from) {
        link_libtiff_dir(&libtiff_dir);
    } else {
        link_pkg_config_libtiff();
    }
}

fn link_windows_libtiff() {
    let libtiff_dir = env::var_os("LIBTIFF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\temp\libtiff\install"));
    let bin_dir = libtiff_dir.join("bin");
    let dll = bin_dir.join("tiff.dll");

    println!("cargo:rerun-if-changed={}", dll.display());
    link_libtiff_dir(&libtiff_dir);

    if dll.exists() {
        if let Some(profile_dir) = profile_dir() {
            let _ = fs::copy(&dll, profile_dir.join("tiff.dll"));
        }
    }
}

fn link_libtiff_dir(libtiff_dir: &PathBuf) {
    let lib_dir = if libtiff_dir.join("lib").exists() {
        libtiff_dir.join("lib")
    } else {
        libtiff_dir.join("lib64")
    };
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=tiff");
}

fn link_pkg_config_libtiff() {
    if pkg_config("libtiff-4") || pkg_config("libtiff") {
        return;
    }

    panic!(
        "could not find libtiff through pkg-config; install libtiff development files or set LIBTIFF_DIR"
    );
}

fn pkg_config(package: &str) -> bool {
    let pkg_config = env::var_os("PKG_CONFIG").unwrap_or_else(|| "pkg-config".into());
    let output = Command::new(pkg_config).args(["--libs", package]).output();

    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for token in stdout.split_whitespace() {
        if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if let Some(lib) = token.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }

    true
}

fn profile_dir() -> Option<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    out_dir.ancestors().nth(3).map(|path| path.to_path_buf())
}
