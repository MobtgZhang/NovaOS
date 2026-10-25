//! Build orchestration: the kernel, the workspace, and the host tests.
//!
//! # Why this is not a single `cargo build`
//!
//! Three properties of this tree force a split:
//!
//! - The kernel and xtask live in the same workspace, but a normal
//!   `cargo build --workspace` compiles *both* — including a host build of the
//!   kernel's `cfg(test)` modules, which cannot be linked bare-metal and whose
//!   tests are not meant to run there. `build_kernel` builds only the kernel,
//!   for the bare-metal target only.
//! - The host tests are *not* "build the kernel for the host and run it". They
//!   are ordinary `cargo test` runs over crates that intentionally compile under
//!   `std`. Running them on every build would either be too eager or hide a
//!   regression, so they are a separate command.
//! - The kernel's `.cargo/config.toml` rustflags are keyed on
//!   `target_arch = "x86_64-unknown-none"`, so a host build inherits none of
//!   them. That is what makes the host test run work without an override, and it
//!   is why the two builds can share one workspace.

use crate::boot::{APP_PACKAGE, USER_PACKAGE};
use crate::{log, run_command_owned};

/// Cargo target triple for the kernel.
pub const KERNEL_TARGET: &str = "x86_64-unknown-none";

/// The profile used when the caller named none.
///
/// `release` because the kernel's `dev` profile inherits `debug = true` and the
/// workspace's dev `panic` setting, and a debug-info-bearing kernel is several
/// times larger for no benefit in a boot that must complete in seconds.
pub const DEFAULT_PROFILE: &str = "release";

/// Crates built and tested for the host.
///
/// Named explicitly rather than `--workspace`: the kernel is a workspace member
/// but has no host test target, so a workspace sweep would fail on it
/// confusingly. `xtask` is included in the build (it is the tool doing the
/// building) but excluded from `cargo test`.
///
/// Only `nova-abi` exists in the M0 tree. The rest are listed here as they are
/// created, because a package named in this list and missing from the workspace
/// fails with a clear Cargo error rather than being silently skipped — which is
/// the failure mode that lets a crate rot unnoticed.
pub const HOST_PACKAGES: &[&str] = &["nova-abi"];

/// Build the kernel for the bare-metal target with `profile`.
///
/// # Panics
///
/// Never. Every failure is a returned `Err` carrying the command that failed,
/// because a panic here would print a backtrace instead of the thing worth
/// reading.
pub fn build_kernel(profile: &str) -> Result<(), String> {
    log(&format!(
        "building the kernel (--package nova-kernel --target {KERNEL_TARGET} --profile {profile})"
    ));

    run_command_owned(
        &[
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            "nova-kernel".to_string(),
            "--target".to_string(),
            KERNEL_TARGET.to_string(),
            "--profile".to_string(),
            profile.to_string(),
        ],
        "kernel build",
    )
}

/// Build the user-space runtime for the bare-metal target with `profile`.
///
/// # Panics
///
/// Never. Every failure is a returned `Err` carrying the command that failed,
/// because a panic here would print a backtrace instead of the thing worth
/// reading.
pub fn build_user(profile: &str) -> Result<(), String> {
    build_applications(profile)?;
    build_runtime(profile)
}

fn build_runtime(profile: &str) -> Result<(), String> {
    log(&format!(
        "building the user-space runtime (--package {USER_PACKAGE} --target {KERNEL_TARGET} --profile {profile})"
    ));

    run_command_owned(
        &[
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            USER_PACKAGE.to_string(),
            "--target".to_string(),
            KERNEL_TARGET.to_string(),
            "--profile".to_string(),
            profile.to_string(),
        ],
        "user-space runtime build",
    )
}

fn build_applications(profile: &str) -> Result<(), String> {
    log(&format!(
        "building user-space applications (--package {APP_PACKAGE} --target {KERNEL_TARGET} --profile {profile})"
    ));

    run_command_owned(
        &[
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            APP_PACKAGE.to_string(),
            "--target".to_string(),
            KERNEL_TARGET.to_string(),
            "--profile".to_string(),
            profile.to_string(),
        ],
        "user-space application build",
    )
}

/// Build the host-side crates, `xtask` included.
///
/// `xtask` is in the list so that a `cargo xtask` invocation always runs code
/// that has been rebuilt from the current source, rather than a stale binary
/// from a previous invocation.
pub fn build_host(profile: &str) -> Result<(), String> {
    log("building host crates");

    let mut command = vec!["cargo".to_string(), "build".to_string()];
    for package in HOST_PACKAGES {
        command.push("--package".to_string());
        command.push((*package).to_string());
    }
    command.push("--package".to_string());
    command.push("xtask".to_string());
    command.push("--profile".to_string());
    command.push(profile.to_string());

    run_command_owned(&command, "host build")
}

/// Build everything, resolving the profile the caller asked for.
///
/// Returns the profile actually used, so a caller can locate the artifact in
/// the right `target/<triple>/<profile>/` directory without repeating the
/// resolution rules.
///
/// # Why the host build is not part of the kernel path
///
/// [`crate::image`] needs only the two bare-metal binaries: an ISO with the
/// kernel on it boots, and userspace is a module the kernel may or may not
/// find. Folding the host build in here would mean a kernel test run fails for
/// an unrelated reason the moment a `crates/` crate stops compiling — which is
/// exactly the wrong time to learn it.
pub fn build_for_profile(requested: Option<&str>) -> Result<String, String> {
    let profile = requested.unwrap_or(DEFAULT_PROFILE).to_string();
    build_kernel(&profile)?;
    build_user(&profile)?;
    build_host(&profile)?;
    Ok(profile)
}

/// Build only what an ISO needs: the kernel and the user-space runtime.
///
/// The user runtime is a separate package with its own linker script and its
/// own target flags, and the ISO carries it as a Limine module. It is built
/// here rather than on demand so that a missing user image is a *build*
/// failure — with a message naming the package — rather than a boot that
/// silently comes up with no program to run.
pub fn build_kernel_only(requested: Option<&str>) -> Result<String, String> {
    let profile = requested.unwrap_or(DEFAULT_PROFILE).to_string();
    build_kernel(&profile)?;
    build_user(&profile)?;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PROFILE, HOST_PACKAGES, KERNEL_TARGET};

    #[test]
    fn the_kernel_target_is_pinned() {
        // Multi-arch means the target has to be explicit, not whatever the
        // current host happens to be.
        assert_eq!(KERNEL_TARGET, "x86_64-unknown-none");
    }

    #[test]
    fn the_default_profile_is_release() {
        assert_eq!(DEFAULT_PROFILE, "release");
    }

    #[test]
    fn the_kernel_is_not_a_host_package() {
        // `nova-kernel` is `no_std`/`no_main`; a host build of it would either
        // not link or would compile test modules that never run.
        assert!(!HOST_PACKAGES.contains(&"nova-kernel"));
    }

    #[test]
    fn the_host_packages_are_the_abi_and_the_pure_libraries() {
        // These are the crates that are `no_std` but testable under `std`.
        //
        // `nova-utils` is asserted to be *absent* rather than present: it was
        // named here before any crate by that name existed, so the assertion
        // failed against a workspace that is otherwise correct. The workspace
        // is the specification, and it has no `nova-utils` — the shared helpers
        // live in `nova-abi` for now. If a `nova-utils` crate is added, this
        // test should start requiring it here *and* adding it to
        // `HOST_PACKAGES`, so the two never drift again.
        assert!(HOST_PACKAGES.contains(&"nova-abi"));
        assert!(!HOST_PACKAGES.contains(&"nova-utils"));
    }

    /// Every host package must actually exist in the workspace.
    ///
    /// `cargo build --package <name>` fails loudly on a typo, but only once
    /// someone runs a host build; this catches it in `cargo test` instead.
    ///
    /// Each package's own manifest is checked rather than the workspace root. The
    /// root manifest is a *virtual* manifest — it lists members by path and contains
    /// no `name =` lines at all — so searching it for package names can never
    /// match.
    #[test]
    fn every_host_package_exists_in_the_workspace() {
        let root = crate::boot::repo_root();
        let members = std::fs::read_to_string(root.join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("cannot read workspace Cargo.toml: {error}"));

        for package in super::HOST_PACKAGES {
            let found = members.lines().any(|line| {
                let line = line.trim().trim_end_matches(',').trim_matches('"');
                line == *package || line.ends_with(&format!("/{package}"))
            });

            assert!(
                found,
                "HOST_PACKAGES names `{package}`, which is not a member of the workspace"
            );
        }
    }
}
