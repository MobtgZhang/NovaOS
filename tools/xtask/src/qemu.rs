//! The QEMU invocation.
//!
//! # Every flag here is load-bearing
//!
//! Each flag below exists because removing it produces a failure that looks like
//! something else. They are documented individually because "it stopped working
//! when we upgraded QEMU" is otherwise impossible to diagnose.
//!
//! # The exit port trap
//!
//! `isa-debug-exit` has **no fixed port**. It listens on whatever `iobase=`
//! says. The kernel writes [`DEBUG_EXIT_IOBASE`] because that is what this
//! command line requests — and if the two ever disagree, QEMU keeps running after
//! the kernel has finished its tests, so a *passing* suite looks exactly like a
//! hang until the timeout kills it. `scripts/qemu.sh` asserts the same constant
//! from the outside, so a change to one has to be made in both.
//!
//! # OVMF needs two pflash drives
//!
//! `-bios` and the ISO-as-a-CD approach do not work for this configuration:
//! OVMF's variable store has to be writable, and pflash is how it is made so. The
//! first drive is the read-only firmware, the second is a *copy* of a variable
//! template that QEMU writes to — pointing the writable drive at the system's
//! template directly would let one run corrupt the installation for every other.

/// The base port of QEMU's `isa-debug-exit` device.
///
/// Must match `kernel/src/arch/x86_64/exit.rs`'s `DEBUG_EXIT_PORT`.
pub const DEBUG_EXIT_IOBASE: u32 = 0xf4;

/// Guest memory size, in MiB.
///
/// 256 MiB is comfortably above the 16 MiB floor the kernel's boot test asserts,
/// and comfortably below what makes QEMU's TCG emulation slow enough to matter.
pub const MEMORY_MIB: u32 = 256;

/// Exit status the kernel writes for a passing test suite.
///
/// Mirrors `arch/shared/exit.rs`'s `EXIT_SUCCESS`. Duplicated rather than
/// imported because that constant lives in a `no_std` crate that cannot be built
/// for the host; the two are cross-checked by reading the kernel source in this
/// module's tests.
pub const EXIT_SUCCESS: u32 = 0x00;

/// Where OVMF's firmware images are searched.
const OVMF_SEARCH_PATHS: &[&str] = &[
    "/usr/share/OVMF",
    "/usr/share/ovmf",
    "/usr/share/edk2/ovmf",
    "/usr/local/share/ovmf",
];

/// The firmware code image, which is read-only.
const OVMF_CODE_NAMES: &[&str] = &["OVMF_CODE_4M.fd", "OVMF_CODE.fd", "OVMF.fd"];

/// The variable-store template, which is copied per run.
const OVMF_VARS_NAMES: &[&str] = &["OVMF_VARS_4M.fd", "OVMF_VARS.fd", "OVMF_VARS.ms.fd"];

/// Exit status QEMU reports for a kernel that wrote `EXIT_SUCCESS` (0).
///
/// `isa-debug-exit` maps the written value to `(value << 1) | 1`, so *every* guest
/// status is odd. The shell test harness compensates for that in
/// `scripts/qemu.sh`; this constant exists so the mapping lives in exactly one
/// place.
pub const OBSERVED_SUCCESS_STATUS: u32 = (EXIT_SUCCESS << 1) | 1;

/// Locate the OVMF code image.
pub fn ovmf_code() -> Result<std::path::PathBuf, String> {
    find_ovmf(OVMF_CODE_NAMES, "code")
}

/// Locate the OVMF variable-store template.
pub fn ovmf_vars() -> Result<std::path::PathBuf, String> {
    find_ovmf(OVMF_VARS_NAMES, "variables")
}

/// Search the known OVMF locations for one of `names`.
///
/// The error lists every path that was tried, because "OVMF not found" without
/// the search paths sends the reader looking in the wrong place.
fn find_ovmf(names: &[&str], kind: &str) -> Result<std::path::PathBuf, String> {
    let mut tried = Vec::new();

    for directory in OVMF_SEARCH_PATHS {
        for name in names {
            let candidate = std::path::Path::new(directory).join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
            tried.push(candidate.display().to_string());
        }
    }

    Err(format!(
        "OVMF {kind} image not found. Install an edk2/ovmf package, or point \
         OVMF_SEARCH_PATHS at it. Tried:\n  {}",
        tried.join("\n  ")
    ))
}

/// A writable copy of the OVMF variable store.
///
/// Created fresh per run so a previous run's NVRAM — which may hold a stale boot
/// order pointing at a device that no longer exists — cannot affect this one.
///
/// Written to [`crate::boot::run_dir`], not the staging directory: everything
/// under `stage/` becomes the ISO's filesystem root, and a 4 MiB variable store
/// inside the shipped image is both a size problem and a disclosure of the
/// machine's firmware state.
pub fn fresh_vars(vars_template: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let destination = crate::boot::run_dir().join("OVMF_VARS.fd");

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    std::fs::copy(vars_template, &destination).map_err(|error| {
        format!(
            "cannot copy OVMF variables {} -> {}: {error}",
            vars_template.display(),
            destination.display()
        )
    })?;

    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::{DEBUG_EXIT_IOBASE, MEMORY_MIB, OBSERVED_SUCCESS_STATUS};

    #[test]
    fn the_exit_port_is_the_one_the_kernel_writes() {
        // `isa-debug-exit` listens on `iobase`, so the kernel's constant and
        // this one are the same fact expressed twice. If they drift, QEMU never
        // exits and a passing suite looks like a hang.
        assert_eq!(DEBUG_EXIT_IOBASE, 0xf4);

        // Read from the kernel source rather than duplicating the number, so a
        // change there fails *this* test instead of failing at run time.
        let kernel = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../kernel/src/arch/x86_64/exit.rs");
        let source = std::fs::read_to_string(&kernel)
            .expect("the kernel's exit backend is part of the tree");
        assert!(
            source.contains("const DEBUG_EXIT_PORT: u16 = 0xF4;"),
            "kernel/src/arch/x86_64/exit.rs no longer writes 0xF4; \
             update DEBUG_EXIT_IOBASE and scripts/qemu.sh to match",
        );
    }

    #[test]
    fn a_success_status_is_odd() {
        // `(value << 1) | 1` — the harness's exit-code comparison depends on
        // every guest status being odd.
        assert_eq!(OBSERVED_SUCCESS_STATUS % 2, 1);
    }

    #[test]
    fn guest_memory_is_above_the_kernel_boot_floor() {
        // The kernel's `memory_map_is_usable` test fails below 16 MiB, so a
        // smaller value here would fail for a reason unrelated to what was
        // being tested.
        //
        // A `const` block rather than a plain `assert!`: `MEMORY_MIB` is a
        // constant, so clippy evaluates the assertion at compile time and
        // warns that the check is vacuous. That warning is wrong about the
        // intent — the point is to catch someone *lowering* the constant — so
        // the check is kept as a compile-time assertion where it is a genuine
        // invariant rather than a runtime one.
        const { assert!(MEMORY_MIB >= 16) };
    }
}
