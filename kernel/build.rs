fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed={dir}/linker.ld");
    if std::env::var("TARGET").unwrap_or_default() == "x86_64-unknown-none" {
        println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
        println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    }
}
