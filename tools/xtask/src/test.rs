//! Test orchestration.
//!
//! # Two suites, two runners
//!
//! `cargo test --workspace --exclude nova-kernel --exclude xtask` runs the host
//! tests: pure logic compiled under `std`. The kernel's own tests run inside QEMU
//! and are reached through `cargo xtask run`.
//!
//! The two are deliberately *not* unified. The host tests are fast enough to run
//! on save and are what a developer runs to check a change; the QEMU suite costs
//! ~7 s of emulation and is what proves the kernel links, boots, and reads the
//! bootloader's data structures correctly. Merging them would mean every save
//! either paid the emulation cost or ran a subset without saying so.
//!
//! # Why the excludes are explicit
//!
//! `--exclude nova-kernel` because the kernel is `no_std`/`no_main`: a host
//! build of its `cfg(test)` modules does not link, and if it ever did, it would
//! link a binary that never runs them — a green result that tested nothing.
//! `--exclude xtask` because it is the tool driving the run, not code under
//! test. Naming both is deliberate: a future `--workspace` sweep would then
//! either skip them correctly or fail loudly, never silently skip them.

/// Crates that are not part of the host test run.
pub const EXCLUDED_HOST_PACKAGES: &[&str] = &["nova-kernel", "xtask"];

/// Run the host tests.
///
/// No `--target` is passed: `.cargo/config.toml` keys its rustflags on
/// `target_arch = "x86_64-unknown-none"`, so a host build inherits none of them.
/// That is what lets the host tests share one workspace with the kernel without
/// an environment override, and why adding `--target` here would need an
/// accompanying `RUSTFLAGS` reset.
pub fn host() -> Result<(), String> {
    let mut command = vec![
        "cargo".to_string(),
        "test".to_string(),
        "--workspace".to_string(),
    ];

    for package in EXCLUDED_HOST_PACKAGES {
        command.push("--exclude".to_string());
        command.push((*package).to_string());
    }

    crate::log("running host tests");

    crate::run_command_owned(&command, "run host tests")
}

#[cfg(test)]
mod tests {
    use super::EXCLUDED_HOST_PACKAGES;

    #[test]
    fn the_kernel_is_excluded_from_host_tests() {
        // The kernel is `no_std`/`no_main`; a host `cargo test` of it would
        // either not compile or, worse, compile the test modules into a binary
        // that never runs them.
        assert!(EXCLUDED_HOST_PACKAGES.contains(&"nova-kernel"));
    }

    #[test]
    fn the_driver_is_excluded_from_host_tests() {
        assert!(EXCLUDED_HOST_PACKAGES.contains(&"xtask"));
    }

    #[test]
    fn every_host_build_package_is_also_testable_on_the_host() {
        // A crate in the build list but not runnable under `cargo test` would
        // mean `cargo xtask test` builds something it never checks.
        for package in crate::build::HOST_PACKAGES {
            assert!(
                !EXCLUDED_HOST_PACKAGES.contains(package),
                "{package} is built but excluded from host tests",
            );
        }
    }
}
