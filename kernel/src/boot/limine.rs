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

use limine::framebuffer::{FRAMEBUFFER_RGB, Framebuffer};
use limine::request::{
    BootloaderInfoRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    MpRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use crate::arch::shared::exit::EXIT_PANIC;
use crate::boot::info::{
    BootInfo, FramebufferInfo, MAX_CPUS, MAX_MEMMAP_ENTRIES, MAX_PHYSICAL_ADDRESS, PAGE_SIZE,
};
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

/// Multi-processor handover.
///
/// Flags are zero: the kernel uses the architectural xAPIC path, not x2APIC.
/// The response exposes the bootstrap processor and one record per processor.
#[unsafe(no_mangle)]
pub static LIMINE_MP: MpRequest = MpRequest::new(0);

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

/// Request a linear framebuffer from the firmware's GOP setup.
///
/// Absent on a headless firmware and on any boot where nobody asked for a
/// display; [`capture`] records `None` in that case rather than failing, so
/// M0–M5 boots are unaffected by the M6 request being present.
#[unsafe(no_mangle)]
pub static LIMINE_FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

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
/// The archive holds every packaged program, and the desktop's programs are not
/// the handful of small IPC services this bound started with: eight binaries,
/// three of which link the UI toolkit and the built-in font, run to about
/// 270 KiB today — so the first value of this constant (256 KiB) is already a
/// boot failure rather than a margin.
///
/// It is a guard, not a target. The length is checked *before* the copy, so an
/// oversized archive is refused rather than truncated into a corrupt one, and
/// the storage is `.bss`: raising the bound costs RAM the machine already has
/// (the QEMU VM is 1 GiB) and no image bytes, while lowering the risk of the
/// next application silently pushing the desktop past a boot it used to pass.
const INITRAMFS_CAPACITY: usize = 512 * 1024;

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

        // Capture the processors and start the application processors.
        //
        // The bootloader's `MpInfo` records are not retained. Their identifiers
        // are copied into `BootInfo`, and the APs are handed over while the
        // records are still valid. Each AP waits on `boot::smp`'s gate until the
        // bootstrap processor has completed the scheduler, address-space, and
        // APIC setup it needs.
        if let Some(response) = LIMINE_MP.response() {
            let bsp_lapic_id = response.bsp_lapic_id;
            (&raw mut (*target).bsp_lapic_id).write(bsp_lapic_id);
            (&raw mut (*target).cpu_ids[0]).write(bsp_lapic_id);

            let mut stored = 1usize;
            for processor in response.cpus() {
                if processor.lapic_id == bsp_lapic_id {
                    continue;
                }
                if stored >= MAX_CPUS {
                    panic!(
                        "limine: processor count exceeds the kernel's static CPU table ({MAX_CPUS})"
                    );
                }

                (&raw mut (*target).cpu_ids[stored]).write(processor.lapic_id);
                processor.bootstrap(crate::boot::smp::ap_entry, stored as u64);
                stored += 1;
            }

            (&raw mut (*target).cpu_count).write(stored);
            klog!(
                Level::Debug,
                "limine: {} processors, bsp lapic id {bsp_lapic_id}, {} APs handed over",
                stored,
                stored - 1
            );
        } else {
            // A bootloader without MP support still describes one processor.
            (&raw mut (*target).cpu_count).write(1);
            (&raw mut (*target).bsp_lapic_id).write(0);
            (&raw mut (*target).cpu_ids[0]).write(0);
            klog!(
                Level::Warn,
                "limine: no multi-processor response; treating boot as single-CPU"
            );
        }

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

        // The firmware framebuffer, when one exists. Its absence is the
        // normal case on every headless and M0-M5 boot and is recorded as
        // `None` without a warning; an unusable one is dropped with the
        // reason spelled out, because a GUI that draws into a framebuffer
        // whose geometry was never checked corrupts nothing but the user's
        // understanding of why the screen is wrong.
        let framebuffer = match LIMINE_FRAMEBUFFER.response() {
            None => None,
            Some(response) => match response.framebuffers().first() {
                None => None,
                Some(framebuffer) => {
                    let direct_map_offset = (*target).direct_map_offset;
                    let outcome = (framebuffer.address() as u64)
                        .checked_sub(direct_map_offset)
                        .ok_or("virtual address is below the direct map")
                        .and_then(|phys| normalise_framebuffer(framebuffer, phys));
                    match outcome {
                        Ok(info) => {
                            klog!(
                                Level::Info,
                                "limine: framebuffer {}x{} pitch {} bpp 32 rgb shifts {}/{}/{} at {:#x}",
                                info.width,
                                info.height,
                                info.pitch,
                                info.red_shift,
                                info.green_shift,
                                info.blue_shift,
                                info.physical_address
                            );
                            Some(info)
                        }
                        Err(reason) => {
                            klog!(Level::Warn, "limine: framebuffer unusable: {reason}");
                            None
                        }
                    }
                }
            },
        };
        (&raw mut (*target).framebuffer).write(framebuffer);
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

/// Check a firmware framebuffer against what M6 draws, and normalise it.
///
/// The kernel draws one pixel format — XRGB8888, 32 bits, three eight-bit
/// components — and this function is the gate that keeps that promise honest:
/// a firmware that hands over a 16-bit or planar framebuffer is *reported*,
/// not silently drawn into with the wrong pixel layout. The physical address
/// is taken from the caller, which derived it from the direct-map offset;
/// this function only checks that the derivation produced a page-aligned
/// address inside the addressable range, which is what tells the caller the
/// subtraction was sound.
fn normalise_framebuffer(
    framebuffer: &Framebuffer,
    physical_address: u64,
) -> Result<FramebufferInfo, &'static str> {
    if framebuffer.memory_model != FRAMEBUFFER_RGB {
        return Err("memory model is not RGB");
    }
    if framebuffer.bpp != 32 {
        return Err("bits per pixel is not 32");
    }
    if framebuffer.red_mask_size != 8
        || framebuffer.green_mask_size != 8
        || framebuffer.blue_mask_size != 8
    {
        return Err("colour components are not eight bits each");
    }
    if framebuffer.width == 0 || framebuffer.height == 0 {
        return Err("zero width or height");
    }
    // A mode the address space cannot hold is a mode that cannot be drawn:
    // the visible bytes are `height * pitch`, and both are firmware-supplied.
    if framebuffer.width > MAX_PHYSICAL_ADDRESS / 4
        || framebuffer.height > MAX_PHYSICAL_ADDRESS
        || framebuffer.pitch > MAX_PHYSICAL_ADDRESS
    {
        return Err("geometry exceeds the addressable range");
    }
    // Rows are padded to the display's transfer granularity, so pitch is at
    // least the visible bytes and may be more; less means the firmware's own
    // arithmetic disagrees with its geometry.
    let minimum_pitch = framebuffer.width * 4;
    if framebuffer.pitch < minimum_pitch {
        return Err("pitch is smaller than one row of pixels");
    }
    // The visible bytes must fit the addressable range too, or the last rows
    // land past the end of what the kernel can map. The multiplication is
    // checked rather than performed and then bounded: two firmware-supplied
    // words can overflow u64 before any comparison sees them.
    let Some(visible_bytes) = framebuffer.pitch.checked_mul(framebuffer.height) else {
        return Err("visible size overflows the address space");
    };
    if visible_bytes == 0 || visible_bytes > MAX_PHYSICAL_ADDRESS {
        return Err("visible size exceeds the addressable range");
    }
    if physical_address == 0 || physical_address % PAGE_SIZE != 0 {
        return Err("physical address is null or not page aligned");
    }
    if physical_address.saturating_add(visible_bytes) > MAX_PHYSICAL_ADDRESS {
        return Err("framebuffer extends past the addressable limit");
    }

    Ok(FramebufferInfo {
        virtual_address: framebuffer.address() as u64,
        physical_address,
        width: framebuffer.width as u32,
        height: framebuffer.height as u32,
        pitch: framebuffer.pitch as u32,
        red_shift: framebuffer.red_mask_shift,
        green_shift: framebuffer.green_mask_shift,
        blue_shift: framebuffer.blue_mask_shift,
    })
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
