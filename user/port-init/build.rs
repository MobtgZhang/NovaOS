fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
        let linker = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
            "linker.ld"
        } else {
            "linker-early.ld"
        };
        println!("cargo::rerun-if-changed={linker}");
        println!("cargo::rustc-link-arg=-T{manifest}/{linker}");
    }
}
