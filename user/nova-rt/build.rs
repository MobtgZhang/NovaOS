//! Build script for the user-space runtime.
//!
//! # Why this exists
//!
//! A user program on NovaOS is linked at `0x1000` — in the *lower* half of the
//! address space, where the kernel is not — and the layout it must produce is
//! frozen in `docs/abi/address-layout.md` and written out in `user/linker.ld`.
//! `rustc` has no way to learn that from the manifest, so the linker script has
//! to be named to the linker somehow.
//!
//! The two ways to do that are a `-C link-arg` rustflag or a link-arg emitted
//! by a build script. The build script is used here because a rustflag would
//! have to be repeated by every caller, and a caller who forgot it would get a
//! binary linked at the default address — which *links*, and runs, and is
//! wrong. A build script cannot be forgotten: it runs as part of every build
//! of the package.
//!
//! # Why the path is absolute
//!
//! The linker resolves `-T` relative to its *working directory*, which is the
//! workspace root when Cargo drives it but the package directory when
//! something else does. An absolute path is the only spelling that works in
//! both cases, and the manifest directory is the one thing a build script
//! always knows.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("Cargo always sets CARGO_MANIFEST_DIR"),
    );

    let script = manifest
        .parent()
        .expect("the runtime lives in <root>/user/nova-rt")
        .join("linker.ld");

    if !script.exists() {
        panic!(
            "the user-space linker script is missing at {}; the runtime cannot be linked without it",
            script.display()
        );
    }

    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rerun-if-changed=src/main.rs");

    // The linker's own argument, not a rustc flag: the script has to be read
    // by `rust-lld`, and `-C link-arg` is how a package tells the linker
    // about something the manifest cannot express.
    println!("cargo::rustc-link-arg=-T{}", script.display());
}
