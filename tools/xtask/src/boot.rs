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
/// # Why the config contains no slash above its entry opener
///
/// This is the subtlest of all of them, and the one that produced a boot which
/// reached the firmware, loaded the bootloader, and then sat there with the
/// kernel never entered.
///
/// Limine does not find a menu entry by looking for a line that *starts* with a
/// slash. It finds one by scanning the raw config bytes for the first slash
/// character **anywhere in the file**. In `init_config` this is the tail of
/// Limine's own config loader:
///
/// ```c
/// menu_tree = create_menu_tree(NULL, 1, 0);
///
/// size_t s;
/// char *c = config_get_entry(&s, 0);
/// if (c != NULL) {
///     while (*c != '/' && c > config_addr) {
///         c--;
///     }
///     if (*c == '/' && c > config_addr) {
///         c[-1] = 0;
///     }
/// }
/// ```
///
/// Comments are not stripped before this scan. They are only skipped later, when
/// an individual option is looked up by name — so a slash in a comment is
/// indistinguishable from the slash that opens the entry.
///
/// The observable consequence is that the *first* slash in the file wins, and
/// whatever surrounds it becomes the entry title. A config whose preamble
/// mentions `tools/xtask/src/boot.rs` produces an entry the menu will not
/// display, no entry that matches `default_entry`, and one serial line:
///
/// ```text
/// Default entry is not valid, booting to menu.
/// ```
///
/// Nothing else is printed. The globals above the entry — `timeout`,
/// `default_entry`, `quiet`, `serial` — are parsed by name and survive intact,
/// which is what makes this so confusing: the config is demonstrably being read,
/// it is demonstrably being applied, and the entry is still absent.
///
/// The fix is a rule rather than a fix, because it constrains every future edit
/// to this template: **no slash anywhere above the entry opener.** The preamble
/// spells out `boot()` without a slash, spells paths as "a bare absolute path",
/// and the enforcement lives in `no_slash_precedes_the_entry_opener` so that a
/// later edit which reintroduces one fails CI rather than the boot.
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
///
/// [`ENTRY_NAME`], [`KERNEL_NAME`] and [`INITRAMFS_NAME`] are interpolated rather
/// than spelled out, so renaming one cannot leave the other two stale — and a
/// stale name here is silent, because Limine drops the entry and only says
/// `[config file contains no valid entries]`.
pub fn limine_config_template() -> String {
    format!(
        r#"
# NovaOS Limine configuration.
#
# Generated by `cargo xtask image`. Edit the xtask crate, not this copy under
# the build directory.
#
# READ THIS BEFORE EDITING: do not write a slash anywhere above the menu entry.
#
# Limine 12 locates a menu entry by scanning the raw config bytes for the
# first slash character *anywhere in the file*, not the first line that begins
# with one. It has no notion of "this slash is inside a comment" -- comments are
# not stripped before the entry scan, they are only skipped when an option is
# looked up by name.
#
# So a single slash in a comment above the entry is enough to consume the entry
# that follows it. Limine then finds an entry titled after whatever text
# surrounded that slash, matches it against nothing, and reports exactly one
# line on the serial port:
#
#     Default entry is not valid, booting to menu.
#
# Everything above this point is then dead weight: the boot is not slow, the
# kernel is not hanging, and no kernel line ever appears because the kernel is
# never entered. The whole symptom set is indistinguishable from a fault in
# `main.rs`, which is where it sends you looking.
#
# That is why this preamble spells out paths without slashes, and why
# `boot_is_entry_slash_safe` in the tests enforces it. A slash is safe only on
# and after the entry opener line below.

timeout: 0
default_entry: {ENTRY_NAME}
quiet: yes
serial: yes

# A menu entry: the slash-prefixed line opens it, and the indented options below
# belong to it. `protocol: limine` boots a bare-metal ELF64, and `path` is the
# executable. Indentation is optional and is kept here for readability.
#
# The `boot():` prefix names the ISO resource prefix explicitly. Inside the
# bootloader the ISO is mounted at `boot()`, so a bare absolute path refers to a
# path that does not exist there. A bad path fails late, as
# `Failed to open executable` after the menu, which is harder to attribute.
/{ENTRY_NAME}
    protocol: limine
    path: boot():/boot/{KERNEL_NAME}
    module_path: boot():/boot/{INITRAMFS_NAME}
"#
    )
}

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
    let template = limine_config_template();
    if boot.join(INITRAMFS_NAME).is_file() {
        return template;
    }

    template
        .lines()
        .filter(|line| !line.trim_start().starts_with("module_path:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the ISO from a staging directory.
///
/// # Why the image is written in-process
///
/// The obvious implementation shells out to `xorriso`. It was not used, for
/// two reasons that both showed up in the reference environment.
///
/// The first is availability: `xorriso` is not installed there, and the
/// fallback — `mtools` — can write an ISO filesystem but cannot produce the
/// El Torito boot catalog, which is the whole point of the image. An image
/// without a boot catalog does not boot.
///
/// The second is that `xorriso`'s diagnostics do not survive the trip. It
/// writes a hybrid BIOS/UEFI ISO whose exact layout depends on a long
/// option list, and it warns about things that need not matter
/// (`-boot-info-table` is deprecated in favour of `-boot_image_any`). The
/// result is an image whose correctness is a property of a command line that
/// is not covered by any test.
///
/// [`crate::iso`] writes the ISO 9660 filesystem and the El Torito catalog
/// directly, so the layout is a property of code that *is* covered by tests —
/// and the boot entries are named as data rather than as flags.
///
/// # Why not `grub-mkrescue`
///
/// `grub-mkrescue` would install a second bootloader and a second
/// configuration to load Limine — two more moving parts, and it needs a loop
/// device that may not be available in a container. The only bootloader in the
/// chain here is Limine.
pub fn build_iso(stage: &Path) -> Result<(), String> {
    let output = iso_path();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    // The BIOS image is the one a legacy BIOS loads; the UEFI image is a
    // partition image for OVMF. Both are marked as El Torito entries, and the
    // UEFI one is the default so that a UEFI firmware starts it while a BIOS
    // firmware — which ignores EFI entries — falls through to the BIOS one.
    let boots = vec![
        crate::iso::BootEntry {
            file: format!("boot/{}", crate::limine::BIOS_BINARY),
            platform: 0,
            default: false,
        },
        crate::iso::BootEntry {
            file: format!("boot/{}", crate::limine::UEFI_BINARY),
            platform: crate::iso::PLATFORM_EFI,
            default: true,
        },
    ];

    crate::iso::write_iso(stage, &boots, &output).map_err(|error| error.to_string())?;
    crate::log(&format!("wrote {}", output.display()));
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

    /// The rendered config every assertion below inspects.
    ///
    /// A `format!` call rather than a constant, because the names it interpolates
    /// are themselves constants: one binding keeps the tests and the shipped
    /// template reading the same string.
    fn config() -> String {
        limine_config_template()
    }

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
            config().lines().any(|line| line.trim() == opener),
            "expected a `{opener}` entry opener; an entry is opened by `/Title`, and \
         any other prefix (`:` in particular) silently yields zero entries"
        );

        // Belt and braces: no line may carry a `:` immediately before the `/`.
        assert!(
            !config().lines().any(|line| line.trim().starts_with(":/")),
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
            config()
                .lines()
                .any(|line| line.trim() == "protocol: limine"),
            "expected `protocol: limine`; `elf64` is not a Limine protocol"
        );
        assert!(
            !config().contains("elf64"),
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
        let config = config();
        let line = config
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
            config().lines().any(|line| line.trim() == "timeout: 0"),
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
            config().contains(&format!("path: boot():/boot/{KERNEL_NAME}")),
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
            rendered
                .lines()
                .any(|line| line.trim() == format!("/{ENTRY_NAME}")),
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

        assert_eq!(limine_config(&stage), config());

        std::fs::remove_dir_all(&stage).ok();
    }

    /// No slash may appear before the entry opener.
    ///
    /// This is the test that pins the trap documented on [`limine_config_template`]
    /// as a rule instead of a comment. Limine finds a menu entry by scanning the
    /// raw config bytes for the first slash *anywhere in the file* — comments are
    /// not stripped before that scan — so a single slash in the preamble silently
    /// consumes the entry that follows it, and the only symptom is
    /// `Default entry is not valid, booting to menu.`
    ///
    /// Every other test in this module passes on a config that this one rejects.
    /// The entry opener is still `/NovaOS`, the protocol is still `limine`,
    /// `default_entry` is still the bare name, the path still carries its
    /// resource prefix: every other assertion is satisfied while the boot sits at
    /// the menu forever. That is what makes this assertion necessary rather than
    /// redundant — it is the only one that can tell the difference between a
    /// correct config and a config that merely looks correct.
    ///
    /// Both the rendered template and the *staged* text are checked, because
    /// [`limine_config`] rewrites the text on the way to disk and is where a
    /// slash could in principle be introduced.
    #[test]
    fn no_slash_precedes_the_entry_opener() {
        let opener = format!("/{ENTRY_NAME}");

        for (label, rendered) in [
            ("template", config()),
            (
                "staged",
                limine_config(Path::new("/nonexistent-stage-for-test")),
            ),
        ] {
            let position = rendered.find(&opener).unwrap_or_else(|| {
                panic!("the {label} config has no `{opener}` entry opener to check against")
            });

            let (before, at) = rendered.split_at(position);
            let offending = before
                .char_indices()
                .filter(|(_, character)| *character == '/')
                .map(|(index, _)| {
                    // The line number, not the byte offset: a failing assertion
                    // should point at something a person can go and edit.
                    let line = before[..index].matches('\n').count() + 1;
                    format!(
                        "line {line}: {}",
                        rendered.lines().nth(line - 1).unwrap_or("").trim()
                    )
                })
                .collect::<Vec<_>>();

            assert!(
                offending.is_empty(),
                "the {label} config has a slash above the entry opener, and Limine will \
                 treat what surrounds it as the entry title instead of booting NovaOS. \
                 Offending line(s):\n{}",
                offending.join("\n")
            );

            // The opener itself must be the *first* non-space character on its
            // line. A leading space makes it an option line, not an opener, and
            // `create_menu_tree` skips it — which is the same silent failure with
            // an even smaller trigger.
            let line = at.lines().next().unwrap_or_default();
            assert!(
                line.starts_with(&opener),
                "the entry opener must start its own line; leading whitespace makes Limine \
                 read it as an option rather than an entry"
            );
        }
    }
}
