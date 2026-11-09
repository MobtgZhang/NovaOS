//! Assembling a bootable ESP image.
//!
//! # The shape of the artifact
//!
//! NovaOS boots through NovaBoot — a UEFI application that renders the OS
//! selection menu, loads the kernel ELF and initramfs, builds early page
//! tables, and transfers control to the kernel. The "image" is a FAT-formatted
//! ESP disk image containing:
//!
//! ```text
//! nova.img
//!  └── /EFI/BOOT/BOOTX64.EFI     the NovaBoot boot manager (UEFI app)
//!  └── /EFI/NovaOS/nova-kernel.elf the kernel itself
//!  └── /EFI/NovaOS/initramfs     the initramfs archive
//! ```
//!
//! QEMU then boots it under OVMF with the image as the first (and only)
//! drive. OVMF's boot manager finds `BOOTX64.EFI` on the ESP and launches it.
//!
//! # Why FAT, not ISO
//!
//! An ISO requires `xorriso`/`grub-mkrescue` and El Torito quirks; a FAT image
//! is what a real ESP looks like, and `mtools` lets us build one deterministically
//! without needing loopback mounts or root.

use std::fs;
use std::path::{Path, PathBuf};

/// The bare-metal target triple the kernel is built for.
pub const TARGET: &str = "x86_64-unknown-none";

/// The kernel's file name inside the ESP.
pub const KERNEL_NAME: &str = "nova-kernel.elf";

/// The initramfs's file name inside the ESP.
pub const INITRAMFS_NAME: &str = "initramfs";

/// The NovaBoot boot manager's file name inside the ESP.
///
/// UEFI fixed removable-media path: `\EFI\BOOT\BOOTX64.EFI`.
pub const BOOT_MANAGER_NAME: &str = "BOOTX64.EFI";

/// The NovaBoot boot manager's package name, as Cargo knows it.
pub const BOOT_MANAGER_PACKAGE: &str = "nova-boot-menu";

/// The application package name, as Cargo knows it.
pub const APP_PACKAGE: &str = "nova-apps";

/// The user-space runtime's package name, as Cargo knows it.
pub const USER_PACKAGE: &str = "nova-rt";

/// Programs packaged into the initramfs, in deterministic order.
pub const APP_NAMES: &[&str] = &[
    "init",
    "echo-server",
    "file-server",
    "console-server",
    "shell",
    "terminal",
    "editor",
    "file-manager",
    "widgets-demo",
    "desktop-shell",
];

/// Build a bounded `NOVAFS1` archive from named ELF files.
pub fn build_initramfs(directory: &Path) -> Result<Vec<u8>, String> {
    const HEADER_SIZE: usize = 64;
    const NAME_LIMIT: usize = 31;
    const MAX_FILES: usize = 16;

    if APP_NAMES.len() > MAX_FILES {
        return Err(format!("too many initramfs entries: {}", APP_NAMES.len()));
    }

    let mut archive = Vec::new();
    for name in APP_NAMES {
        let path = directory.join(name);
        let data = fs::read(&path)
            .map_err(|error| format!("cannot read app {}: {error}", path.display()))?;
        if name.len() > NAME_LIMIT {
            return Err(format!("app name {name:?} exceeds the initramfs limit"));
        }
        let size = u64::try_from(data.len())
            .map_err(|_| format!("app {name:?} is too large for the initramfs"))?;

        let mut header = [0u8; HEADER_SIZE];
        header[..8].copy_from_slice(b"NOVAFS1\0");
        header[8..8 + name.len()].copy_from_slice(name.as_bytes());
        header[40..48].copy_from_slice(&size.to_le_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(&data);
    }
    Ok(archive)
}

/// The ESP directory name for the boot manager.
const ESP_BOOT_DIR: &str = "EFI/BOOT";
/// The ESP directory name for NovaOS payloads.
const ESP_NOVAOS_DIR: &str = "EFI/NovaOS";

/// Where per-run scratch files live, relative to the target directory.
///
/// Deliberately a *sibling* of the staging directory, not a child. Everything
/// under `stage/` becomes the ESP's filesystem root, so a per-run file written
/// there — a serial log, an OVMF variable store — ends up as a file inside the
/// shipped image. Keeping scratch files outside means the image contains only
/// what was staged deliberately.
const RUN_SUBDIR: &str = "novaos/run";

/// Directory for the current run's scratch files.
pub fn run_dir() -> PathBuf {
    target_dir().join(RUN_SUBDIR)
}

/// The repository root, derived from this file's location at compile time.
///
/// `CARGO_MANIFEST_DIR` is `tools/xtask`, so the root is two levels up. Doing it
/// this way rather than from the current working directory means `cargo xtask`
/// behaves the same whether it is invoked from the root, from `tools/xtask`, or
/// from a CI job that happens to `cd` somewhere first.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask lives at <root>/tools/xtask, so two parents up is the root")
        .to_path_buf()
}

/// The directory Cargo is writing artifacts into.
///
/// Honours `CARGO_TARGET_DIR` so a CI job that redirects the target directory
/// does not leave `xtask` writing somewhere else.
pub fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(value) => PathBuf::from(value),
        None => repo_root().join("target"),
    }
}

/// The built kernel ELF for `profile`.
pub fn kernel_artifact(profile: Option<&str>) -> Result<PathBuf, String> {
    let directory = match profile {
        None | Some("dev") | Some("debug") => "debug",
        Some(other) => other,
    };

    let path = target_dir()
        .join(TARGET)
        .join(directory)
        .join("nova-kernel");

    if !path.exists() {
        return Err(format!(
            "kernel artifact missing at {}; run `cargo xtask build` first",
            path.display()
        ));
    }
    Ok(path)
}

/// Locate the built application binaries for `profile`.
pub fn application_directory(profile: Option<&str>) -> Result<PathBuf, String> {
    let directory = match profile {
        None | Some("dev") | Some("debug") => "debug",
        Some(other) => other,
    };
    let path = target_dir().join(TARGET).join(directory);
    path.is_dir()
        .then_some(path)
        .ok_or_else(|| format!("the applications in {APP_PACKAGE} were not built"))
}

/// Where the ESP image is written.
pub fn esp_path() -> PathBuf {
    target_dir().join("novaos").join("nova.img")
}

/// Where a test disk image is written.
pub fn disk_path(name: &str) -> PathBuf {
    target_dir().join("novaos").join(name)
}

/// Where the staging directory is.
pub fn stage_dir() -> PathBuf {
    target_dir().join("novaos/stage")
}

/// Copy the boot artifacts into a staging directory that becomes the ESP root.
///
/// Returns the staging directory. Everything is copied rather than symlinked.
pub fn stage_esp(kernel: &Path, initramfs: &[u8], boot_manager: &Path) -> Result<PathBuf, String> {
    let stage = stage_dir();
    if stage.exists() {
        fs::remove_dir_all(&stage)
            .map_err(|error| format!("cannot clear {}: {error}", stage.display()))?;
    }

    let efi_boot = stage.join(ESP_BOOT_DIR);
    let efi_novaos = stage.join(ESP_NOVAOS_DIR);
    fs::create_dir_all(&efi_boot)
        .map_err(|error| format!("cannot create {}: {error}", efi_boot.display()))?;
    fs::create_dir_all(&efi_novaos)
        .map_err(|error| format!("cannot create {}: {error}", efi_novaos.display()))?;

    // The NovaBoot boot manager at the fixed removable-media path.
    let boot_dest = efi_boot.join(BOOT_MANAGER_NAME);
    fs::copy(boot_manager, &boot_dest).map_err(|error| {
        format!(
            "cannot copy boot manager {} -> {}: {error}",
            boot_manager.display(),
            boot_dest.display()
        )
    })?;

    // The kernel under the exact name the boot manager looks for.
    let kernel_dest = efi_novaos.join(KERNEL_NAME);
    fs::copy(kernel, &kernel_dest).map_err(|error| {
        format!(
            "cannot copy kernel {} -> {}: {error}",
            kernel.display(),
            kernel_dest.display()
        )
    })?;

    // The initramfs archive.
    let initramfs_dest = efi_novaos.join(INITRAMFS_NAME);
    fs::write(&initramfs_dest, initramfs)
        .map_err(|error| format!("cannot write initramfs: {error}"))?;

    Ok(stage)
}

/// Build a FAT-formatted ESP image from a staging directory.
///
/// Uses `mkfs.fat` to create a blank image and `mcopy` to populate it, so no
/// loopback mounts or root privileges are needed.
pub fn build_esp_image(stage: &Path, image: &Path) -> Result<(), String> {
    // Size the image: 64 MiB is plenty for the kernel + initramfs + boot manager.
    const IMAGE_MIB: u64 = 64;
    let size_sectors = IMAGE_MIB * 2048; // 1 MiB = 2048 sectors of 512 bytes

    // Create a blank FAT12/FAT16 image.
    let _ = fs::remove_file(image);
    run_command(
        &[
            "mkfs.fat",
            "-C",
            image.to_str().ok_or("invalid image path")?,
            &size_sectors.to_string(),
        ],
        "mkfs.fat",
    )?;

    // Copy the staged files onto the image. `mcopy -s` copies recursively; `::/`
    // is the root of the image. No shell is involved, so we name the EFI
    // directory directly rather than relying on a glob.
    run_command(
        &[
            "mcopy",
            "-i",
            image.to_str().ok_or("invalid image path")?,
            "-s",
            &format!("{}", stage.join("EFI").display()),
            "::/",
        ],
        "mcopy",
    )?;

    Ok(())
}

/// Stamp `\EFI\NovaOS\boot-mode` on the built ESP image with `mode`
/// ("desktop" / "safe" / "test"), so the boot manager's menu selects that
/// entry directly and skips the interactive wait.
///
/// `cargo xtask image` regenerates the image from a fresh `mkfs.fat` every
/// run, so a marker stamped here can never leak into a later, markerless
/// build: the next image build starts from an empty filesystem.
pub fn inject_boot_mode_marker(image: &Path, mode: &str) -> Result<(), String> {
    // The local holder for the marker's content, written into the target
    // directory so `mcopy` has a real file to read from.
    let holder = target_dir().join("novaos").join("boot-mode-marker");
    fs::write(&holder, mode).map_err(|error| format!("cannot write marker: {error}"))?;

    run_command(
        &[
            "mcopy",
            "-i",
            image.to_str().ok_or("invalid image path")?,
            holder.to_str().ok_or("invalid marker path")?,
            ESP_BOOT_MODE_MARKER_PATH,
        ],
        "stamp boot-mode marker",
    )
}

/// The marker file's path on the ESP, in mtools' `::`-prefixed spelling.
const ESP_BOOT_MODE_MARKER_PATH: &str = "::EFI/NovaOS/boot-mode";

/// Run a command, inheriting stdio so the user sees compiler output.
pub fn run_command(program: &[&str], what: &str) -> Result<(), String> {
    crate::log(&format!("{}: {}", what, program.join(" ")));

    let status = std::process::Command::new(program[0])
        .args(&program[1..])
        .status()
        .map_err(|error| format!("failed to run `{}`: {error}", program[0]))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed with {status}"))
    }
}
