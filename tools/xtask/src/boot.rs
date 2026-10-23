//! Assembling a bootable image.
//!
//! # The shape of the artifact
//!
//! NovaOS boots through Limine, which means the "image" is not one file: it is an
//! ISO containing a bootloader, a configuration, and the kernel ELF. QEMU then
//! boots it under OVMF.
//!
//! ```text
//! nova.iso
//!  └── /boot/limine-uefi-cd.bin    the EFI application
//!  └── /boot/limine-bios-cd.bin    the BIOS boot sector
//!  └── /boot/limine.cfg            what to load, and how long to wait
//!  └── /boot/nova-kernel.elf       the kernel itself
//! ```
//!
//! # Why `boot():` appears in every Limine path
//!
//! Limine 12.x resolves each path through a resource prefix. Writing
//! `/boot/nova-kernel.elf` works only if the ISO was built so that that prefix
//! resolves, and it does not reliably — the failure is `PANIC: Failed to open
//! executable` with no kernel output at all. Writing `boot():/nova-kernel.elf`
//! names the resource prefix explicitly and works for both BIOS and UEFI builds.
//! This is one of the three "kernel prints nothing" traps documented in
//! [`crate`]'s module docs, and it is the reason [`LIMINE_CONFIG`] writes the
//! prefix rather than a bare path.

use std::path::{Path, PathBuf};

/// The bare-metal target triple the kernel is built for.
pub const TARGET: &str = "x86_64-unknown-none";

/// The kernel's file name inside the ISO.
///
/// Must match the name in [`LIMINE_CONFIG`]. These two are written in different
/// files by design — the config is a build artifact's sibling, not something
/// derived from the crate name — so a rename has to be made twice, and the ISO
/// build is what catches a miss.
pub const KERNEL_NAME: &str = "nova-kernel.elf";

/// The initramfs's file name inside the ISO, when one is staged.
///
/// Like [`KERNEL_NAME`], this must match the name in [`LIMINE_CONFIG`].
pub const INITRAMFS_NAME: &str = "initramfs";

/// The Limine menu entry's title, without its opening `/`.
///
/// Named once so [`LIMINE_CONFIG`]'s opener and its `default_entry` cannot
/// disagree — the two are a matching pair, and a mismatch between them is the
/// silent "boots to menu forever" failure documented on [`LIMINE_CONFIG`].
pub const ENTRY_NAME: &str = "NovaOS";

/// The removable-media fallback name for the EFI application.
///
/// The name the UEFI specification fixes for a non-removable default. OVMF finds
/// the application through the ISO's El Torito catalog and ignores this path
/// entirely, but firmware booting the same image from a USB stick looks for
/// exactly this file and nothing else.
pub const EFI_APPLICATION: &str = "BOOTX64.EFI";

/// The configuration file's name, and where Limine 12.x looks for it.
///
/// Limine 12.x renamed this file: the 11.x name was `limine.cfg`, the 12.x name is
/// `limine.conf`. Limine does *not* accept the old name — it searches a fixed list
/// of paths, none of which include it, so a stale `limine.cfg` produces a boot
/// that loads the bootloader, finds no config, and sits at its menu with no
/// entries to show. The observed symptom is exactly `Default entry is not valid,
/// booting to menu.` on the serial line, with no kernel output at all.
///
/// The `boot()` prefix in [`LIMINE_KERNEL_PATH`] is the resource prefix for the
/// same reason: the ISO's own mount is the root, so `/boot/...` inside the
/// bootloader refers to a different directory than it does to the ISO builder.
///
/// Checked against the strings embedded in `limine-uefi-cd.bin`, which lists
/// `/boot/limine.conf` first.
pub const CONFIG_NAME: &str = "limine.conf";

/// The Limine configuration written into the staging directory.
///
/// # Why this looks nothing like a Limine 11.x `limine.cfg`
///
/// Two independent facts about Limine 12.x combine to produce a config that is
/// silently ignored if you get either one wrong.
///
/// 1. **Menu entries open with `/Title`, not `:label`.** A `:label` block is
///    Limine 12's syntax for something else entirely (SMBIOS-provided config
///    nesting), so it is parsed as garbage. An entry is a `/`-prefixed title
///    line followed by indented `option: value` lines, up to the next entry.
/// 2. **The protocol is `limine`, not `elf64`.** `elf64` was never a Limine
///    protocol; it was the shape of the *file*. Limine 12 lists exactly
///    `linux`, `limine`, `multiboot1`, `multiboot2`, `efi`, `bios`. An
///    unrecognised protocol does not abort — the entry is dropped.
///
/// Neither mistake produces an error. Both leave Limine with zero bootable
/// entries, and the whole symptom set is a single line on the serial console:
///
/// ```text
/// Default entry is not valid, booting to menu.Limine 12.9.4 (x86-64, UEFI)
/// [config file contains no valid entries]
///  S Firmware Setup    B Blank Entry
/// ```
///
/// So the kernel is never loaded, `isa-debug-exit` is never reached, and a
/// naive harness reports only "timeout". Treat this message as a config parse
/// error, never as a kernel hang.
///
/// # Why `default_entry` is a bare name, not `/NovaOS`
///
/// `default_entry` accepts a 1-based index or an entry path, and its two forms
/// are *not* symmetric. Per Limine 12's CONFIG.md, entry paths "use `/` as a
/// directory separator" — i.e. the separator is a separator *between*
/// components, not a required leading character. So `NovaOS` and `1` both
/// resolve, while `/NovaOS` is parsed as an empty first component, matches
/// nothing, and disables autoboot.
///
/// That failure is silent and looks exactly like a hang. Limine prints
///
/// ```text
/// Default entry is not valid, booting to menu.
/// ```
///
/// and then sits at the entry picker forever with the kernel fully loaded in
/// memory. Nothing has crashed; the kernel is simply never entered. Since the
/// next stage of a boot test is normally "wait, then declare a timeout", this
/// reads as a kernel that hangs before printing its first line, which sends you
/// hunting through `main.rs` for a fault that is not there.
///
/// `NovaOS` and `1` are both valid today. The bare name is used because it keeps
/// working when a second entry is added above the kernel (an index would
/// silently shift, a name would not).
///
/// # Why `timeout: 0` is load-bearing, and not for the reason it looks
///
/// `0` means "boot the selected entry instantly, *without showing the menu*".
/// `no` disables autoboot and *always* shows the menu — the opposite of what the
/// name suggests.
///
/// The `module_path` line is emitted by [`limine_config`] only when an
/// initramfs is actually staged; see that function for why a dangling
/// `module_path` silently voids the whole entry.
pub const LIMINE_CONFIG: &str = r#"
# NovaOS Limine configuration.
#
# Generated by `cargo xtask image`. Edit `tools/xtask/src/boot.rs`, not this
# copy under `target/`.

timeout: 0
default_entry: NovaOS
quiet: yes
serial: yes

# A menu entry: the `/`-prefixed line opens it and its indented options belong
# to it. `protocol: limine` boots a bare-metal ELF64; `path` is the executable.
#
# The `boot():` prefix names the ISO resource prefix explicitly. Inside the
# bootloader the ISO is mounted at `boot()`, so a bare `/boot/nova-kernel.elf`
# refers to a path that does not exist there. A bad path fails late, as
# `Failed to open executable` after the menu, which is harder to attribute.
/NovaOS
    protocol: limine
    path: boot():/boot/nova-kernel.elf
    module_path: boot():/boot/initramfs
"#;

/// Where the staging directory lives, relative to the target directory.
const STAGE_SUBDIR: &str = "novaos/stage";

/// Where the finished ISO lives, relative to the target directory.
const IMAGE_SUBDIR: &str = "novaos";

/// Where per-run scratch files live, relative to the target directory.
///
/// Deliberately a *sibling* of the staging directory, not a child. Everything
/// under `stage/` becomes the ISO's filesystem root, so a per-run file written
/// there — a serial log, an OVMF variable store — ends up as a 4 MiB
/// `OVMF_VARS.fd` inside the shipped image. Keeping scratch files outside means
/// the ISO contains only what was staged deliberately.
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
fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(value) => PathBuf::from(value),
        None => repo_root().join("target"),
    }
}

/// The built kernel ELF for `profile`.
pub fn kernel_artifact(profile: Option<&str>) -> Result<PathBuf, String> {
    // `debug` rather than `dev`: Cargo's directory name for the default profile
    // is `debug` even though `--profile dev` is its official name.
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

/// Where the ISO is written.
pub fn iso_path() -> PathBuf {
    target_dir().join(IMAGE_SUBDIR).join("nova.iso")
}

/// Where the staging directory is.
pub fn stage_dir() -> PathBuf {
    target_dir().join(STAGE_SUBDIR)
}

/// Copy the kernel and Limine's binaries and config into a staging directory.
///
/// Returns the staging directory. Everything is copied rather than symlinked
/// because `xorriso` follows links inconsistently across invocations, and an ISO
/// that is correct on the author's machine but empty on CI is a miserable thing
/// to debug.
pub fn stage_image(kernel: &Path) -> Result<PathBuf, String> {
    let stage = stage_dir();
    let boot = stage.join("boot");

    // Rebuild from scratch: a stale `limine.cfg` left over from a previous
    // version would otherwise be used when the new one fails to write.
    if stage.exists() {
        std::fs::remove_dir_all(&stage)
            .map_err(|error| format!("cannot clear {}: {error}", stage.display()))?;
    }
    std::fs::create_dir_all(&boot)
        .map_err(|error| format!("cannot create {}: {error}", boot.display()))?;

    // The kernel, under the exact name `LIMINE_CONFIG` refers to.
    let kernel_dest = boot.join(KERNEL_NAME);
    std::fs::copy(kernel, &kernel_dest).map_err(|error| {
        format!(
            "cannot copy kernel {} -> {}: {error}",
            kernel.display(),
            kernel_dest.display()
        )
    })?;

    // Limine's two boot images. Only one is used per boot path, but shipping
    // both means the same ISO works under UEFI and under BIOS, which is what
    // makes `-bios` a viable debugging alternative to the pflash pair.
    for binary in crate::limine::BOOT_BINARIES {
        let source = crate::limine::binary_path(binary)?;
        let destination = boot.join(binary);
        std::fs::copy(&source, &destination).map_err(|error| {
            format!(
                "cannot copy Limine binary {} -> {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
    }

    // The UEFI application is also placed at the conventional
    // `/EFI/BOOT/BOOTX64.EFI`. El Torito alone is enough for OVMF, but the
    // removable-media fallback is what a real machine looks for when firmware
    // has no boot entry — and its absence is exactly what `xorriso` warns about
    // when the ISO is written to a USB stick.
    let efi_dir = stage.join("EFI").join("BOOT");
    std::fs::create_dir_all(&efi_dir)
        .map_err(|error| format!("cannot create {}: {error}", efi_dir.display()))?;

    let uefi_source = crate::limine::binary_path(crate::limine::UEFI_BINARY)?;
    std::fs::copy(&uefi_source, efi_dir.join(EFI_APPLICATION)).map_err(|error| {
        format!(
            "cannot copy {} -> {}: {error}",
            uefi_source.display(),
            efi_dir.join(EFI_APPLICATION).display()
        )
    })?;

    let config = boot.join(CONFIG_NAME);
    let config_text = limine_config(&boot);
    std::fs::write(&config, config_text.trim_start())
        .map_err(|error| format!("cannot write {}: {error}", config.display()))?;

    crate::log(&format!("staged image in {}", stage.display()));
    Ok(stage)
}

/// Render [`LIMINE_CONFIG`] for a staging directory.
///
/// `module_path` is emitted only when [`INITRAMFS_NAME`] exists in the staging
/// tree, which makes this a pure function of what is actually on the ISO.
///
/// # Why the module line cannot simply always be present
///
/// A `module_path` naming a file that is not on the ISO does not produce a
/// warning. Limine 12 **drops the entire entry**, leaving it with zero
/// bootable entries, and reports only:
///
/// ```text
/// [config file contains no valid entries]
/// ```
///
/// which names neither the missing file nor the line that referenced it. Worse,
/// the menu still renders, so the log looks like a boot that hangs on an entry
/// picker rather than a packaging error.
///
/// Omitting the line keeps a missing initramfs a *recoverable* degradation —
/// the kernel boots with no module instead of the bootloader refusing to boot at
/// all — which matters while the initramfs is still being built out.
fn limine_config(boot: &Path) -> String {
    if boot.join(INITRAMFS_NAME).is_file() {
        return LIMINE_CONFIG.to_string();
    }

    LIMINE_CONFIG
        .lines()
        .filter(|line| !line.trim_start().starts_with("module_path:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the ISO from a staging directory.
///
/// # Why `xorriso` and not `grub-mkrescue`
///
/// `grub-mkrescue` would install a second bootloader and a second
/// configuration to load Limine — two more moving parts, and it needs a loop
/// device that may not be available in a container. `xorriso` writes a hybrid
/// BIOS/UEFI ISO directly, so the only bootloader in the chain is Limine.
pub fn build_iso(stage: &Path) -> Result<(), String> {
    // The BIOS boot sector must be the first 2048 bytes of the image and must
    // be at file offset 0, hence `-b boot/limine-bios-cd.bin` together with
    // `-boot-load-size 4`.
    //
    // `-boot-info-table` patches that first sector with the LBA of the ISO, which
    // is what lets BIOS firmware find itself after the file is moved.

    let command = vec![
        "xorriso".to_string(),
        "-as".to_string(),
        "mkisofs".to_string(),
        // ISO 9660 with Rock Ridge and Joliet: Limine reads ISO 9660, and the
        // other two make the image mountable on a host for debugging.
        "-R".to_string(),
        "-r".to_string(),
        "-J".to_string(),
        "-b".to_string(),
        format!("boot/{}", crate::limine::BIOS_BINARY),
        "-no-emul-boot".to_string(),
        "-boot-load-size".to_string(),
        "4".to_string(),
        "-boot-info-table".to_string(),
        // The UEFI path. `-efi-boot-part --efi-boot-image` makes the EFI
        // application a partition-boot image rather than a file, which is what
        // OVMF looks for.
        "--efi-boot".to_string(),
        format!("boot/{}", crate::limine::UEFI_BINARY),
        "-efi-boot-part".to_string(),
        "--efi-boot-image".to_string(),
        // `stage` must be the last argument: everything before it is an option.
        stage.to_string_lossy().into_owned(),
        "-o".to_string(),
        iso_path().to_string_lossy().into_owned(),
    ];

    if let Some(parent) = iso_path().parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    crate::run_command_owned(&command, "iso build")?;
    Ok(())
}

/// Structural checks on the generated Limine config.
///
/// Every assertion here corresponds to a way the config can be *silently*
/// rejected by Limine 12 — the failure mode is a kernel that is loaded into
/// memory and never entered, so the only evidence is an absence of output and a
/// boot that has to be killed on a timer. Each of these was found the hard way,
/// by bisecting the config against QEMU.
///
/// They cannot be caught by a host-side unit test in the general case, because
/// whether Limine accepts an entry is Limine's business. What *can* be pinned is
/// the shape of the config, which is what these do: they assert the specific
/// spellings documented in [`LIMINE_CONFIG`] are present, so a well-meaning
/// "cleanup" that makes the config more idiomatic fails CI instead of the boot.
#[cfg(test)]
mod tests {
    use super::*;

/// The entry must be opened by a `/`-prefixed title line.
///
/// Limine 12 opens a menu entry with `/Title`. A `:Title` line is not an entry
/// opener, so a config written that way yields zero entries and
/// `[config file contains no valid entries]`.
///
/// The assertion matches the *whole* line rather than checking a prefix. A
/// prefix check is what let `:/NovaOS` — a colon glued onto the front — pass this
/// test while the boot stayed broken: the test asserted on a prefix that the
/// bug itself satisfied. Comparing the complete trimmed line means an extra
/// character anywhere fails the test.
#[test]
fn entry_is_opened_by_slash_title() {
    let opener = format!("/{ENTRY_NAME}");

    assert!(
        LIMINE_CONFIG.lines().any(|line| line.trim() == opener),
        "expected a `{opener}` entry opener; an entry is opened by `/Title`, and \
         any other prefix (`:` in particular) silently yields zero entries"
    );

    // Belt and braces: no line may carry a `:` immediately before the `/`.
    assert!(
        !LIMINE_CONFIG.lines().any(|line| line.trim().starts_with(":/")),
        "a `:` before the entry title makes the line a non-opener, not an entry"
    );
}

    /// The protocol must be `limine`.
    ///
    /// `elf64` names a file format, not a Limine protocol. Limine 12 accepts
    /// `linux`, `limine`, `multiboot1`, `multiboot2`, `efi` and `bios`; an
    /// unrecognised protocol drops the entry rather than erroring.
    #[test]
    fn protocol_is_limine_not_elf64() {
        assert!(
            LIMINE_CONFIG
                .lines()
                .any(|line| line.trim() == "protocol: limine"),
            "expected `protocol: limine`; `elf64` is not a Limine protocol"
        );
        assert!(
            !LIMINE_CONFIG.contains("elf64"),
            "`elf64` is not a Limine protocol and will void the entry"
        );
    }

    /// `default_entry` must be the bare entry name, with no leading `/`.
    ///
    /// This is the subtlest of the four, because the leading-slash form is the
    /// intuitive one: `/` is documented as the *separator between* path
    /// components, not as a required first character. `/NovaOS` parses as an
    /// empty leading component, matches no entry, and disables autoboot — after
    /// which Limine waits at the menu with the kernel already loaded.
    #[test]
    fn default_entry_is_bare_name() {
        let line = LIMINE_CONFIG
            .lines()
            .find(|line| line.starts_with("default_entry:"))
            .expect("config should set default_entry so unattended boots are deterministic");

        assert_eq!(
            line,
            format!("default_entry: {ENTRY_NAME}"),
            "a leading `/` in default_entry disables autoboot and hangs the boot at the menu"
        );
    }

    /// `timeout: 0` must be `0`, never `no`.
    ///
    /// `no` disables *autoboot* and always shows the menu — the opposite of the
    /// naive reading — which reintroduces the wait-for-keypress hang that
    /// `timeout: 0` exists to avoid.
    #[test]
    fn timeout_is_zero_not_no() {
        assert!(
            LIMINE_CONFIG
                .lines()
                .any(|line| line.trim() == "timeout: 0"),
            "`timeout: 0` boots instantly without showing the menu; `no` does the reverse"
        );
    }

    /// The kernel path must use the `boot():/` resource prefix.
    ///
    /// Inside the bootloader the ISO is mounted at `boot()`, so a bare absolute
    /// path resolves against the wrong root and fails late, as
    /// `Failed to open executable`, after the menu has already been shown.
    #[test]
    fn kernel_path_uses_iso_resource_prefix() {
        assert!(
            LIMINE_CONFIG.contains(&format!("path: boot():/boot/{KERNEL_NAME}")),
            "the kernel path must be `boot():/boot/{KERNEL_NAME}`"
        );
    }

    /// The staged config drops `module_path` when no initramfs is present.
    ///
    /// A `module_path` pointing at a file that is not on the ISO does not warn:
    /// Limine discards the whole entry and reports only
    /// `[config file contains no valid entries]`, naming neither the file nor
    /// the line. Dropping the line keeps a not-yet-built initramfs a
    /// recoverable degradation instead of an unbootable image.
    #[test]
    fn missing_initramfs_drops_module_path() {
        let stage = Path::new("/nonexistent-stage-for-test");
        let rendered = limine_config(stage);

        assert!(
            !rendered.contains("module_path:"),
            "a module_path with no matching file on the ISO voids the entry"
        );
        // The rest of the entry must survive; this is a surgical removal.
        assert!(
            rendered.lines().any(|line| line.trim() == format!("/{ENTRY_NAME}")),
            "entry opener was lost"
        );
        assert!(rendered.contains("protocol: limine"), "protocol was lost");
        assert!(
            rendered.contains(&format!("path: boot():/boot/{KERNEL_NAME}")),
            "kernel path was lost"
        );
    }

    /// `limine_config` is a pure function of what is on disk.
    ///
    /// With an initramfs present the template is emitted verbatim; without one
    /// only the `module_path` line is filtered out.
    #[test]
    fn config_is_verbatim_when_initramfs_present() {
        let stage = std::env::temp_dir().join(format!("novaos-limcfg-{}", std::process::id()));
        std::fs::create_dir_all(&stage).expect("create temp stage");
        std::fs::write(stage.join(INITRAMFS_NAME), b"").expect("write initramfs");

        assert_eq!(limine_config(&stage), LIMINE_CONFIG);

        std::fs::remove_dir_all(&stage).ok();
    }
}