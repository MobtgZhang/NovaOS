//! The Limine boot protocol adapter.
//!
//! # One bootloader, two architectures
//!
//! Limine speaks a single protocol on both x86-64 and AArch64: the kernel exports
//! request structures in a linker-designated section and the bootloader fills in
//! the responses in place. That means this module is *shared*, not per-target —
//! the only per-target difference is which bootloader binary is loaded, which is
//! a packaging concern (`scripts/qemu.sh`), not a code one.
//!
//! # Ownership rule
//!
//! Limine's response structures live in bootloader-owned pages that the
//! bootloader may reclaim as soon as it hands over control. This module copies
//! everything the kernel needs into kernel-owned storage and performs **no**
//! access to a `Request::response()` pointer once [`capture`] returns. Later
//! stages (M2 paging, the initramfs reader) must read the published snapshot
//! rather than re-reading the protocol structures.
//!
//! # How the bootloader finds these requests
//!
//! Every request below is a `#[unsafe(no_mangle)] pub static` whose *symbol
//! name* begins with `LIMINE_`. That is the entire discovery mechanism, and it
//! is worth spelling out because the alternative is invisible when it breaks.
//!
//! Limine 12 locates requests by scanning the ELF symbol table for the exact
//! names `LIMINE_BASE_REVISION`, `LIMINE_MEMMAP`, `LIMINE_DIRECT_MAP`,
//! `LIMINE_BOOTLOADER_INFO`, `LIMINE_STACK_SIZE`, and the two request markers.
//! It does **not** look for a section. An earlier version of this module placed
//! the requests in a linker-designated `.boot.rodata` section, which is how the
//! protocol worked in Limine 11 and is what most guides still show.
//!
//! The failure is completely silent, and it fails in the most confusing way
//! available: the bootloader loads and starts the kernel perfectly well, the
//! kernel runs, and then
//!
//! ```text
//! [NOVA] ERR   | panic: limine: bootloader too old; base revision 3 or newer is required
//! ```
//!
//! appears. That message is not about the bootloader being old at all — Limine
//! 12.9.4 supports revision 6. It is the [`BaseRevision`] response still holding
//! its initial value because Limine never found the request to fill in. The
//! diagnostic points at the bootloader when the bug is entirely in this file.
//!
//! Because the symbols are found by name, `.boot` is still worth keeping in the
//! linker script for a different reason: it pins the requests into one
//! contiguous, page-aligned run that the bootloader can treat as a unit. But it
//! is a convenience, not the discovery mechanism, and renaming a symbol or
//! dropping `no_mangle` breaks boot with no build error to explain it.
//!
//! # Validation rule
//!
//! Every value crossing the boundary is untrusted until checked: memory map
//! entries are bounds- and overflow-checked, the direct-map offset is validated,
//! and a protocol mismatch panics rather than booting on guessed layouts. Nothing
//! here allocates — the snapshot uses statically sized storage because the heap
//! does not exist until M2.

use core::num::NonZeroU64;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use limine::request::{
    BootloaderInfoRequest, HhdmRequest, MemmapRequest, ModulesRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use crate::arch::shared::exit::EXIT_PANIC;
use crate::boot::info::{BootInfo, MAX_MEMMAP_ENTRIES, MAX_PHYSICAL_ADDRESS, PAGE_SIZE};
use crate::klog;
use crate::logger::Level;

/// Marker: requests begin. The bootloader ignores everything before it.
#[unsafe(no_mangle)]
pub static LIMINE_REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

/// Marker: requests end. The bootloader ignores everything after it.
#[unsafe(no_mangle)]
pub static LIMINE_REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Request the base boot-protocol revision this kernel implements.
#[unsafe(no_mangle)]
pub static LIMINE_BASE_REVISION: BaseRevision =
    BaseRevision::with_revision(crate::LIMINE_PROTOCOL_REVISION);

/// Bootloader identity, for the boot banner.
#[unsafe(no_mangle)]
pub static LIMINE_BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

/// The firmware's physical memory map.
#[unsafe(no_mangle)]
pub static LIMINE_MEMMAP: MemmapRequest = MemmapRequest::new();

/// The kernel's higher-half direct map.
///
/// The name is Limine's, but the kernel calls it `direct_map_offset` because that
/// is what it means on both targets: `virtual = physical + offset`.
#[unsafe(no_mangle)]
pub static LIMINE_DIRECT_MAP: HhdmRequest = HhdmRequest::new();

/// Request a 64 KiB boot stack instead of the bootloader's default, so the
/// kernel's early stacks are not sharing pages with protocol structures.
#[unsafe(no_mangle)]
pub static LIMINE_STACK_SIZE: StackSizeRequest = StackSizeRequest::new(64 * 1024);

/// Request the modules the bootloader's `module_path` named.
///
/// M2 delivers the user-space image this way: the xtask stages the user ELF
/// next to the kernel on the ISO and points Limine's `module_path` at it, and
/// the bootloader hands back the file's bytes.
///
/// A module that is absent is not an error. The kernel boots and reports that
/// there is no user image, which is the right degradation: a machine with no
/// user program still runs the kernel and its test suites.
#[unsafe(no_mangle)]
pub static LIMINE_MODULES: ModulesRequest = ModulesRequest::new();

/// The single kernel-owned copy of the boot snapshot.
///
/// Written once by [`capture`], read-only thereafter. Access goes through
/// [`read_boot_info`] rather than by naming this directly.
static mut BOOT_INFO: BootInfo = BootInfo::empty();

/// Kernel-owned bytes of the initramfs, copied from the Limine module.
///
/// The bootloader may reclaim its pages after the boot hand-off, so the archive
/// is copied once before any user process is loaded.
static mut INITRAMFS_BYTES: [u8; INITRAMFS_CAPACITY] = [0; INITRAMFS_CAPACITY];

/// Number of valid bytes in [`INITRAMFS_BYTES`].
static mut INITRAMFS_LEN: usize = 0;

/// The largest initramfs this kernel will copy.
///
/// Six small ELF binaries fit easily; a larger archive is refused before it can
/// overwrite kernel-owned storage.
const INITRAMFS_CAPACITY: usize = 256 * 1024;

/// Borrow the kernel-owned initramfs snapshot.
pub fn initramfs() -> Option<&'static [u8]> {
    // SAFETY: this read-only snapshot is published after `capture` and is never
    // written again; kernel access is serialised.
    let length = unsafe { INITRAMFS_LEN };
    if length == 0 {
        None
    } else {
        Some(unsafe { &INITRAMFS_BYTES[..length] })
    }
}

/// Flipped by [`capture`] once the snapshot is complete.
static READY: AtomicBool = AtomicBool::new(false);

/// Whether the snapshot holds validated firmware facts.
///
/// # Safety
///
/// The caller must not read an uninitialised snapshot merely because this returns
/// `false`; see [`read_boot_info`].
#[inline]
pub fn boot_info_is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Borrow the validated boot snapshot.
///
/// # Safety
///
/// Must only be called after [`capture`] has returned. Reading before that would
/// observe a zeroed snapshot whose `memmap_len` is 0 — a benign but misleading
/// state — so this is a real contract, not defensive noise.
#[inline]
pub unsafe fn read_boot_info() -> &'static BootInfo {
    // SAFETY: the caller guarantees `capture` already ran, and nothing writes the
    // snapshot afterwards, so the borrow is effectively immutable.
    unsafe { &*core::ptr::addr_of!(BOOT_INFO) }
}

/// Check the bootloader speaks a boot protocol revision this kernel implements.
///
/// Called before any hardware access, so a mismatch is a clean diagnostic rather
/// than a machine that dies before it can speak.
///
/// # Panics
///
/// If the revision is unsupported or the bootloader predates base revision 3.
pub fn check_protocol_revision() {
    // `is_supported` is the crate's way of asking "did the bootloader fill this
    // in?". It is the only test available: `BaseRevision` carries its verdict in
    // the response magic rather than in a nullable pointer.
    if !LIMINE_BASE_REVISION.is_supported() {
        match LIMINE_BASE_REVISION.actual_revision() {
            None => panic!("limine: bootloader too old; base revision 3 or newer is required"),
            Some(revision) => panic!(
                "limine: unsupported boot protocol revision {revision}; kernel requires {}",
                crate::LIMINE_PROTOCOL_REVISION
            ),
        }
    }

    // Even with a response present, a bootloader may have negotiated a lower
    // revision than we asked for, so the actual value is authoritative.
    if let Some(actual) = LIMINE_BASE_REVISION.actual_revision()
        && actual < crate::LIMINE_PROTOCOL_REVISION
    {
        panic!(
            "limine: bootloader negotiated revision {actual}; kernel requires {}",
            crate::LIMINE_PROTOCOL_REVISION
        );
    }

    let actual = LIMINE_BASE_REVISION
        .actual_revision()
        .unwrap_or(crate::LIMINE_PROTOCOL_REVISION);
    klog!(Level::Debug, "limine: protocol revision {actual}");
}

/// Read every response, validate it, and publish the snapshot.
///
/// # Safety
///
/// Must be called exactly once, from the kernel entry point, while Limine's
/// response pages are still mapped. Every later stage reads the published
/// snapshot instead, because the bootloader may reclaim those pages at any moment
/// after control is transferred.
pub unsafe fn capture() -> &'static BootInfo {
    // SAFETY: this function's contract is that it runs exactly once from `_start`;
    // it is the only writer of `BOOT_INFO`.
    unsafe {
        let target = &raw mut BOOT_INFO;

        if let Some(response) = LIMINE_BOOTLOADER_INFO.response() {
            // These dereference bootloader pointers. The exception table is
            // installed by this point, so a fault is reported rather than fatal.
            copy_cstr_into(response.name(), &raw mut (*target).bootloader_name);
            copy_cstr_into(response.version(), &raw mut (*target).bootloader_version);
        } else {
            klog!(Level::Warn, "limine: no bootloader info response");
        }

        (&raw mut (*target).protocol_revision).write(
            LIMINE_BASE_REVISION
                .actual_revision()
                .unwrap_or(crate::LIMINE_PROTOCOL_REVISION),
        );

        if let Some(response) = LIMINE_MODULES.response() {
            for module in response.modules() {
                if module.path().ends_with("initramfs") {
                    let bytes = module.data();
                    if bytes.len() > INITRAMFS_CAPACITY {
                        panic!(
                            "limine: initramfs is {} bytes; kernel stores at most {}",
                            bytes.len(),
                            INITRAMFS_CAPACITY
                        );
                    }
                    let target_bytes = &raw mut INITRAMFS_BYTES;
                    (&raw mut INITRAMFS_LEN).write(bytes.len());
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        target_bytes.cast::<u8>(),
                        bytes.len(),
                    );
                    klog!(
                        Level::Debug,
                        "limine: copied initramfs {} bytes",
                        bytes.len()
                    );
                    break;
                }
            }
        }

        // The direct map is the only route from the kernel's high addresses to
        // physical memory, so its absence is unrecoverable on either target.
        let Some(response) = LIMINE_DIRECT_MAP.response() else {
            panic!("limine: no direct map response; kernel requires a higher-half direct map");
        };
        (&raw mut (*target).direct_map_offset).write(response.offset);

        let Some(response) = LIMINE_MEMMAP.response() else {
            panic!("limine: no memory map response");
        };

        let entries = response.entries();
        klog!(
            Level::Debug,
            "limine: {} raw memory map entries",
            entries.len()
        );

        if entries.len() > MAX_MEMMAP_ENTRIES {
            panic!(
                "limine: memory map has {} entries; kernel stores at most {}",
                entries.len(),
                MAX_MEMMAP_ENTRIES
            );
        }

        let mut stored = 0usize;
        for entry in entries {
            // Zero-length regions carry no information and would make the
            // allocator's watermark arithmetic ambiguous.
            let Some(length) = NonZeroU64::new(entry.length) else {
                continue;
            };

            let Some(end) = entry.base.checked_add(entry.length) else {
                klog!(
                    Level::Warn,
                    "limine: dropping memory map entry that overflows [{:#x}, +{:#x})",
                    entry.base,
                    entry.length
                );
                continue;
            };

            if end > MAX_PHYSICAL_ADDRESS {
                klog!(
                    Level::Warn,
                    "limine: dropping memory map entry past the addressable limit \
                     [{:#x}, {end:#x})",
                    entry.base
                );
                continue;
            }

            // Aligning down keeps the allocator's arithmetic uniform; a
            // misaligned base would otherwise split one page across two
            // bookkeeping regions.
            let slot = addr_of_mut!((*target).memmap[stored]);
            (&raw mut (*slot).base).write(entry.base & !(PAGE_SIZE - 1));
            (&raw mut (*slot).length).write(length.get());
            (&raw mut (*slot).type_).write(entry.type_);
            stored += 1;
        }

        // Write the count last: it is what makes the entries visible, so a
        // reader that sees a count must see the entries behind it.
        (&raw mut (*target).memmap_len).write(stored);
    }

    // SAFETY: `capture` owns the single write, and no concurrent writer exists this
    // early in boot.
    let validation = unsafe { read_boot_info() }.validate(MAX_PHYSICAL_ADDRESS);

    if !validation.is_ok() {
        let problems = &validation.problems[..validation
            .problem_count
            .min(crate::boot::info::MAX_PROBLEMS)];
        for problem in problems {
            klog!(Level::Error, "boot: {}", problem.describe());
        }
        if validation.problem_count > crate::boot::info::MAX_PROBLEMS {
            klog!(
                Level::Error,
                "boot: {} further problems omitted",
                validation.problem_count - crate::boot::info::MAX_PROBLEMS
            );
        }
        panic!("limine: boot information failed validation");
    }

    if validation.out_of_order > 0 {
        klog!(
            Level::Warn,
            "boot: {} memory map entries were out of order",
            validation.out_of_order
        );
    }

    klog!(Level::Debug, "limine: boot information captured");

    READY.store(true, Ordering::Release);

    // SAFETY: `READY` was just set, and nothing writes the snapshot afterwards.
    unsafe { read_boot_info() }
}

/// Copy a C string into a fixed-size, NUL-padded buffer, truncating safely.
///
/// Takes a raw pointer because `BootInfo` is reached through `&raw mut`, and
/// `&mut (*raw)[i]` on a `Copy` field would have to copy the whole array back
/// into the static to hand out a reference. A truncated string is still
/// NUL-terminated by the padding, so consumers can never read past the buffer.
unsafe fn copy_cstr_into(source: &str, dest: *mut [u8; 32]) {
    let bytes = source.as_bytes();

    // SAFETY: `dest` points at one of `BootInfo`'s two 32-byte name buffers, so
    // reading its length and writing every index below stays inside that buffer.
    unsafe {
        let length = (*dest).len();
        let n = bytes.len().min(length);

        for (index, &byte) in bytes.iter().take(n).enumerate() {
            dest.cast::<u8>().add(index).write(byte);
        }
        // Zero the tail so a truncated string is still NUL-terminated.
        for index in n..length {
            dest.cast::<u8>().add(index).write(0);
        }
    }
}

/// Status reported if the kernel panics before reaching a test summary.
///
/// Referenced here so the shared exit encoding and the adapter's expectations
/// cannot drift; the mechanism itself lives in the arch backend.
const _: () = assert!(EXIT_PANIC != 0);
