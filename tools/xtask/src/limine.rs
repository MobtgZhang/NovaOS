//! Fetching and verifying the pinned Limine release.
//!
//! # Why the version is pinned, and checked
//!
//! Three things move together and must not drift: the Limine *boot protocol
//! revision* the kernel implements, the Limine *release* that speaks it, and the
//! bootloader's own *configuration format*. Limine 12.x reads 12.x paths and
//! writes 12.x protocol structures; an unpinned "latest" would silently change
//! both the path syntax and the response layout, and the symptom would be a
//! kernel that boots and then reads garbage.
//!
//! So the release is pinned, and [`RELEASE_SHA256`] is verified after download. A
//! bootloader is the first thing to execute with full privilege; an unverified
//! download would make every other check in this system advisory.

use std::path::{Path, PathBuf};

/// The pinned Limine release.
pub const RELEASE: &str = "12.9.4";

/// The architecture-specific binaries NovaOS ships.
///
/// Both are staged into the same ISO so one image boots under either firmware.
/// The BIOS one is required for `xorriso`'s `-b` boot sector; the UEFI one is
/// what OVMF executes.
pub const UEFI_BINARY: &str = "limine-uefi-cd.bin";
/// The BIOS boot image.
pub const BIOS_BINARY: &str = "limine-bios-cd.bin";
/// Both binaries, in the order [`binary_path`] documents.
pub const BOOT_BINARIES: &[&str] = &[UEFI_BINARY, BIOS_BINARY];

/// SHA-256 of the pinned `limine-binary.tar.xz`.
///
/// This is the value from the release's own checksum file, recorded here so that
/// a compromised or moved download is detected rather than trusted. It is
/// intentionally *not* fetched at runtime: verifying against a file fetched from
/// the same host as the artifact would verify nothing.
///
/// A bootloader is the first thing to execute with full privilege. If this value
/// is ever wrong, `fetch-limine` fails closed with a checksum mismatch rather
/// than booting something it did not verify.
pub const RELEASE_SHA256: &str = "9880cc04681262214db9b824ed8a5c55296955027d8caa9f55ab52a6dd85d6a0";

/// A local cache of release archives, checked before downloading.
///
/// Mirrors the layout used by the environment spike so a machine that already
/// fetched Limine during setup does not download it again. The archive is
/// checksum-verified whether it came from here or from the network, so a stale
/// cache entry can only cause a mismatch, never an unverified boot.
const LOCAL_CACHE: &str = ".cache/novaos/limine";

/// The download URL for the pinned release.
fn release_url() -> String {
    format!("https://github.com/limine-bootloader/limine/releases/download/limine-{RELEASE}/limine-binary.tar.xz")
}

/// Where the release is unpacked, under the target directory.
pub fn release_dir() -> PathBuf {
    crate::boot::stage_dir().parent().map_or_else(
        || crate::boot::stage_dir().join("..").join("limine"),
        |parent| parent.join("limine"),
    )
}

/// Path to one of the staged bootloader binaries.
///
/// Limine unpacks into `limine-binary/`, which is why the constant below is part
/// of the path rather than of the release layout.
pub fn binary_path(name: &str) -> Result<PathBuf, String> {
    let path = release_dir()
        .join("limine-binary")
        .join(name);

    if !path.exists() {
        return Err(format!(
            "Limine binary {} not found at {}; run `cargo xtask fetch-limine`",
            name,
            path.display()
        ));
    }
    Ok(path)
}

/// Fail with an actionable message if Limine is not present.
///
/// Called before staging rather than inside [`binary_path`] so the error can say
/// *how* to get it rather than only what is missing.
pub fn require() -> Result<(), String> {
    for name in BOOT_BINARIES {
        binary_path(name)?;
    }
    Ok(())
}

/// Download and verify the pinned Limine release.
///
/// Idempotent: if the binaries are already present, this returns without
/// touching the network, so a CI job that caches `target/limine` does not
/// re-download on every build.
pub fn fetch() -> Result<(), String> {
    if require().is_ok() {
        crate::log(&format!("Limine {RELEASE} already present"));
        return Ok(());
    }

    crate::log(&format!("fetching Limine {RELEASE}"));

    let directory = release_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;

    let archive = directory.join(format!("limine-binary-{RELEASE}.tar.xz"));

    // A cached archive is preferred over a download, but it is verified on the
    // same path as a downloaded one, so a stale cache can only cause a
    // checksum mismatch — never an unverified bootloader.
    let cached = home_dir().join(LOCAL_CACHE).join("limine-binary.tar.xz");
    if cached.exists() && cached != archive {
        std::fs::copy(&cached, &archive).map_err(|error| {
            format!("cannot copy cached Limine {}: {error}", cached.display())
        })?;
        crate::log(&format!("using cached Limine archive {}", cached.display()));
    } else {
        download(&release_url(), &archive)?;
    }

    verify(&archive)?;

    unpack(&archive, &directory)?;
    require()?;

    crate::log(&format!("Limine {RELEASE} ready in {}", directory.display()));
    Ok(())
}

/// The user's home directory.
///
/// Returns an empty path rather than panicking when `HOME` is unset: the caller
/// then treats the cache as absent and downloads, which is the correct fallback.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::new(), PathBuf::from)
}

/// Download `url` to `destination`.
///
/// Uses `curl` rather than an HTTP library: this is a one-shot fetch of a
/// release artifact during setup, and `curl` is already a build prerequisite on
/// every machine that can run the rest of this toolchain.
fn download(url: &str, destination: &Path) -> Result<(), String> {
    let url = url.to_string();
    let destination = destination.to_string_lossy().into_owned();

    // `--fail` turns an HTTP error into a non-zero exit instead of writing the
    // error page into the archive, which would then fail checksum verification
    // with a confusing message instead of a clear one.
    crate::run_command(
        &[
            "curl",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
            &destination,
            &url,
        ],
        "download Limine",
    )
}

/// Verify the archive's SHA-256.
///
/// The checksum is compared here rather than by fetching a `.sha256` file from
/// the same host: a checksum served alongside the artifact it describes detects
/// corruption and nothing else.
fn verify(archive: &Path) -> Result<(), String> {
    let archive = archive.to_string_lossy().into_owned();
    let output = std::process::Command::new("sha256sum")
        .arg(&archive)
        .output()
        .map_err(|error| format!("cannot run sha256sum: {error}"))?;

    if !output.status.success() {
        return Err(format!("sha256sum failed on {archive}"));
    }

    let printed = String::from_utf8_lossy(&output.stdout);
    let actual = printed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if actual != RELEASE_SHA256 {
        return Err(format!(
            "Limine {RELEASE} checksum mismatch\n  expected {RELEASE_SHA256}\n  actual   {actual}\n\
             refusing to use a bootloader whose contents do not match the pinned release"
        ));
    }

    crate::log("Limine checksum verified");
    Ok(())
}

/// Unpack the archive into `directory`.
fn unpack(archive: &Path, directory: &Path) -> Result<(), String> {
    let archive = archive.to_string_lossy().into_owned();
    let directory = directory.to_string_lossy().into_owned();

    crate::run_command(&["tar", "-xJf", &archive, "-C", &directory], "unpack Limine")
}

#[cfg(test)]
mod tests {
    use super::{BOOT_BINARIES, BIOS_BINARY, RELEASE, UEFI_BINARY};

    #[test]
    fn both_boot_paths_are_staged() {
        // One ISO has to work under UEFI and under BIOS; shipping only one
        // makes the other boot path fail at the very last step.
        assert_eq!(BOOT_BINARIES.len(), 2);
        assert!(BOOT_BINARIES.contains(&UEFI_BINARY));
        assert!(BOOT_BINARIES.contains(&BIOS_BINARY));
    }

    #[test]
    fn the_release_is_pinned_to_a_specific_version() {
        // An unpinned "latest" would change both the path syntax and the
        // protocol structures under a kernel that has already frozen its side
        // of the contract.
        assert_eq!(RELEASE, "12.9.4");
        assert!(RELEASE.split('.').count() == 3);
    }

    #[test]
    fn the_binary_names_match_what_limine_ships() {
        assert_eq!(UEFI_BINARY, "limine-uefi-cd.bin");
        assert_eq!(BIOS_BINARY, "limine-bios-cd.bin");
    }
}