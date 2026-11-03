//! WY-14: the gpu build (`--features gpu`) runs with the Mesa tree's glibc, never the initramfs's: its program
//! interpreter is the tree's loader and its DT_RPATH the tree's lib/ (DT_RPATH, not DT_RUNPATH: it also covers the
//! libraries of what is loaded later, libEGL's own ones; started programs do not inherit it, unlike
//! LD_LIBRARY_PATH). BITSCOMP_GPU_ROOT names another tree (a host test); default the stick's, as V2 shows it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BITSCOMP_GPU_ROOT");
    if std::env::var_os("CARGO_FEATURE_GPU").is_none() {
        return;
    }
    let root = std::env::var("BITSCOMP_GPU_ROOT").unwrap_or_else(|_| "/boot/BITSOS2/mesa".into());
    if root.is_empty() || !root.starts_with('/') || root.contains([',', ' ', '\n']) {
        panic!("BITSCOMP_GPU_ROOT must be an absolute path without commas or spaces, not {root:?}");
    }
    println!("cargo:rustc-link-arg-bins=-Wl,--dynamic-linker={root}/lib/ld-linux-x86-64.so.2");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{root}/lib");
    println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
    // flavour::check_build: every library the gpu build maps must come from here.
    println!("cargo:rustc-env=BITSCOMP_GPU_LIB={root}/lib");
}
