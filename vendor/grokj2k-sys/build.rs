use std::env;
use std::path::{Path, PathBuf};

fn main() {
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    let lib = pkg_config::Config::new()
        .atleast_version("12.0.0")
        .probe("libgrokj2k")
        .expect("failed to find libgrokj2k with pkg-config");

    let header = lib
        .include_paths
        .iter()
        .find_map(|include| find_header(include))
        .expect("failed to find grok.h in libgrokj2k include paths");

    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_function("grk_.*")
        .allowlist_type("grk_.*")
        .allowlist_type("GRK_.*")
        .allowlist_var("GRK_.*");

    for include in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", include.display()));
    }

    let bindings = builder
        .generate()
        .expect("failed to generate Grok bindings");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write Grok bindings");

    for path in lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for name in lib.libs {
        println!("cargo:rustc-link-lib={name}");
    }
}

fn find_header(include: &Path) -> Option<PathBuf> {
    [include.join("grok.h"), include.join("grok").join("grok.h")]
        .into_iter()
        .find(|path| path.exists())
}
