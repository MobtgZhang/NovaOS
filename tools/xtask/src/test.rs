//! Test orchestration.
//!
//! # Three suites, three runners
//!
//! `cargo xtask test` runs three suites, in this order:
//!
//! 1. `xtask`'s own tests — the ISO layout, the QEMU argument vector, and the
//!    verdict classification. These are the parts of the harness that decide
//!    whether a boot counts as a pass, so they are checked before anything
//!    depends on them.
//! 2. The host tests — `cargo test --workspace --exclude nova-kernel --exclude
//!    nova-rt --exclude xtask`: pure logic compiled under `std`. The kernel's
//!    own tests run inside QEMU and are reached through `cargo xtask run`.
//! 3. The QEMU boot suite — the kernel, booted for real.
//!
//! The host tests and the QEMU suite are deliberately *not* unified. The host
//! tests are fast enough to run on save and are what a developer runs to check
//! a change; the QEMU suite costs ~7 s of emulation and is what proves the
//! kernel links, boots, and reads the bootloader's data structures correctly.
//! Merging them would mean every save either paid the emulation cost or ran a
//! subset without saying so.
//!
//! # Why the excludes are explicit
//!
//! `--exclude nova-kernel` and `--exclude nova-rt` because both are bare-metal
//! `no_std` / `no_main` binaries with their own `_start`: a host build of
//! either does not link, and if it ever did, it would link a binary that never
//! runs them — a green result that tested nothing.
//!
//! `--exclude xtask` because it is the tool driving the run, not code under
//! test — but see [`driver`]: it is excluded from the *workspace sweep* and
//! run on its own, so that exclusion is a routing decision rather than an
//! exemption. Naming all three is deliberate: a future `--workspace` sweep
//! would then either skip them correctly or fail loudly, never silently skip
//! them.

/// Crates that are not part of the host test run.
///
/// `nova-kernel` and `nova-rt` are bare-metal `no_std` / `no_main` binaries
/// with their own `_start`, and neither can be linked for the host.
///
/// `xtask` is the tool driving the run rather than code under test — but it is
/// not an exemption: [`driver`] runs its own suite.
pub const EXCLUDED_HOST_PACKAGES: &[&str] = &["nova-kernel", "nova-rt", "nova-apps", "xtask"];

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

/// Run `xtask`'s own unit tests.
///
/// `xtask` is excluded from the workspace host-test run because it is the
/// tool driving the run rather than code under test — but that exclusion is
/// not an exemption. The tool has its own test suite covering exactly the
/// things that are easy to get wrong and hard to notice: the El Torito
/// catalog layout, the QEMU argument vector, and the verdict classification
/// that decides pass from fail.
///
/// Excluding it from one test command and never running it anywhere else
/// would leave that suite green by absence rather than by result. It runs
/// here, before the emulation, because it costs milliseconds and it fails
/// with a precise message.
pub fn driver() -> Result<(), String> {
    crate::log("running xtask's own tests");

    crate::run_command_owned(
        &[
            "cargo".to_string(),
            "test".to_string(),
            "--package".to_string(),
            "xtask".to_string(),
        ],
        "run xtask tests",
    )
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
    fn the_user_runtime_is_excluded_from_host_tests() {
        // `nova-rt` is a bare-metal `no_std` / `no_main` binary with its own
        // `_start`, exactly like the kernel. It is excluded for the same
        // reason.
        //
        // It is excluded *explicitly* rather than relying on its manifest's
        // `test = false`: `cargo clippy --all-targets` builds it anyway, and
        // a host build of it fails. An exclusion that depends on another
        // file's setting is an exclusion that quietly stops working.
        assert!(EXCLUDED_HOST_PACKAGES.contains(&"nova-rt"));
    }

    #[test]
    fn the_applications_are_excluded_from_host_tests() {
        assert!(EXCLUDED_HOST_PACKAGES.contains(&"nova-apps"));
    }

    #[test]
    fn every_bare_metal_binary_is_excluded() {
        // The rule, stated once: every workspace member that is a bare-metal
        // binary rather than a library must be excluded from the host test
        // run. A new one that is not would fail here rather than in CI.
        for package in ["nova-kernel", "nova-rt", "nova-apps"] {
            assert!(
                EXCLUDED_HOST_PACKAGES.contains(&package),
                "{package} is a bare-metal binary and must be excluded from the host tests"
            );
        }
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
