//! The NovaBoot boot protocol adapter.
//!
//! # One protocol, one producer
//!
//! The boot manager (`nova-boot-menu`, a UEFI application) renders the OS
//! selection menu, loads the kernel ELF and the initramfs off the ESP, builds
//! early page tables, and publishes a [`nova_boot::BootProtocol`] snapshot. It
//! then calls the kernel's exported `nova_boot_entry` with the snapshot's
//! direct-map virtual address in `RDI`. This module is the kernel's reader for
//! that snapshot.
//!
//! # Ownership rule
//!
//! The snapshot lives in loader-allocated pages the firmware may reclaim after
//! `ExitBootServices`. [`capture`] validates and copies everything the kernel
//! needs into kernel-owned storage before any allocation happens, and performs
//! **no** access to the snapshot pointer once it returns. Later stages (M2
//! paging, the initramfs reader) must read the published snapshot rather than
//! re-reading protocol structures.
//!
//! # Validation rule
//!
//! Every value crossing the boundary is untrusted until checked. The snapshot
//! is validated by the *same code* the boot manager used to build it
//! ([`nova_boot::BootProtocol::validate`]), so a field the producer cannot
//! produce is a field the consumer cannot see.

use core::num::NonZeroU64;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use nova_boot::{
    BootFramebuffer, BootProtocol, MAGIC, MAX_CPUS, MAX_MEMORY_REGIONS, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};

use crate::arch::shared::exit::EXIT_PANIC;
use crate::boot::info::{
    BootInfo, FramebufferInfo, MAX_CPUS as KERNEL_MAX_CPUS, MAX_MEMMAP_ENTRIES,
    MAX_PHYSICAL_ADDRESS, MEMMAP_USABLE, PAGE_SIZE,
};
use crate::klog;
use crate::logger::Level;

/// The direct-map virtual address of the snapshot, stored by the entry stub.
///
/// The snapshot is read exactly twice: once by [`check_protocol_revision`] for
/// the early version gate, and once by [`capture`]. Both run before any
/// allocation and before the kernel switches CR3, so the boot manager's page
/// tables — which map the snapshot through the direct map — are still live.
static mut SNAPSHOT_POINTER: u64 = 0;

/// The single kernel-owned copy of the boot snapshot.
///
/// Written once by [`capture`], read-only thereafter. Access goes through
/// [`read_boot_info`] rather than by naming this directly.
static mut BOOT_INFO: BootInfo = BootInfo::empty();

/// Kernel-owned bytes of the initramfs, copied from the loader-allocated pages.
///
/// The loader's pages may be reclaimed after the hand-off, so the archive is
/// copied once before any user process is loaded.
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
/// and no image bytes, while lowering the risk of the next application silently
/// pushing the desktop past a boot it used to pass.
const INITRAMFS_CAPACITY: usize = 512 * 1024;

/// Record the snapshot pointer the entry stub received.
///
/// # Safety
///
/// Must be called exactly once, from the kernel entry point, before anything
/// else reads the snapshot.
pub unsafe fn store_snapshot_pointer(snapshot: u64) {
    // SAFETY: the entry path is single-CPU with interrupts masked; this is the
    // only writer of the static.
    unsafe {
        addr_of_mut!(SNAPSHOT_POINTER).write(snapshot);
    }
}

/// The snapshot pointer, or `None` when the entry stub never ran.
fn snapshot_pointer() -> Option<u64> {
    // SAFETY: written once by the entry stub before any reader exists.
    let pointer = unsafe { addr_of_mut!(SNAPSHOT_POINTER).read() };
    if pointer == 0 { None } else { Some(pointer) }
}

/// Borrow the snapshot the boot manager published.
///
/// # Safety
///
/// The snapshot's pages are mapped through the boot manager's direct map, which
/// is live until the kernel switches CR3. Callers must run before `mm::init`.
unsafe fn snapshot() -> &'static BootProtocol {
    let pointer = snapshot_pointer().expect("novaboot: the entry stub never stored a snapshot");
    // SAFETY: the pointer was produced by the boot manager, which guarantees
    // the snapshot is whole and page aligned; the entry stub is the only
    // writer of the static holding it.
    unsafe { &*(pointer as *const BootProtocol) }
}

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

/// Check the boot manager published a snapshot this kernel understands.
///
/// Called before any hardware access, so a mismatch is a clean diagnostic rather
/// than a machine that dies before it can speak.
///
/// # Panics
///
/// If no snapshot was published, or its magic, version, or field invariants
/// fail validation.
pub fn check_protocol_revision() {
    let Some(_pointer) = snapshot_pointer() else {
        panic!("novaboot: no boot protocol snapshot; the kernel must be booted by nova-boot-menu");
    };

    // SAFETY: the boot manager's page tables are still live — this runs before
    // `mm::init` switches CR3.
    let snapshot = unsafe { snapshot() };

    if snapshot.magic != MAGIC {
        panic!("novaboot: snapshot magic is not the NovaBoot magic");
    }
    if snapshot.major != PROTOCOL_MAJOR {
        panic!(
            "novaboot: snapshot major {} != kernel major {}",
            snapshot.major, PROTOCOL_MAJOR
        );
    }
    if snapshot.minor > PROTOCOL_MINOR {
        panic!(
            "novaboot: snapshot minor {} is newer than the kernel's {}",
            snapshot.minor, PROTOCOL_MINOR
        );
    }
    if let Err(error) = snapshot.validate() {
        panic!("novaboot: snapshot failed validation: {}", error.tag());
    }

    klog!(
        Level::Debug,
        "novaboot: protocol {}.{} accepted",
        snapshot.major,
        snapshot.minor
    );
}

/// Read the snapshot, validate it, and publish the kernel-owned copy.
///
/// # Safety
///
/// Must be called exactly once, from the boot path, while the boot manager's
/// page tables are still live — before `mm::init` switches CR3. Every later
/// stage reads the published snapshot instead, because the loader's pages may
/// be reclaimed at any moment after control transferred.
pub unsafe fn capture() -> &'static BootInfo {
    // SAFETY: this function's contract is that it runs exactly once from the
    // boot path; it is the only writer of `BOOT_INFO`.
    unsafe {
        let snapshot = snapshot();
        let target = &raw mut BOOT_INFO;

        // The version gate already ran in `check_protocol_revision`; re-running
        // `validate` here is cheap and catches a snapshot that changed between
        // the two reads — which cannot happen, but costs one walk to prove.
        if let Err(error) = snapshot.validate() {
            panic!("novaboot: snapshot failed validation: {}", error.tag());
        }

        // The protocol crate is the single source of the version numbers; the
        // published revision is the *accepted* one, not a bootloader report.
        (&raw mut (*target).protocol_revision)
            .write((PROTOCOL_MAJOR as u64) * 100 + PROTOCOL_MINOR as u64);

        // Bootloader identity: the boot manager is not Limine, and the banner
        // should say so.
        copy_str_into("NovaBoot", &raw mut (*target).bootloader_name);
        copy_str_into("nova-boot-menu", &raw mut (*target).bootloader_version);

        // Single-CPU boot. The protocol can describe up to `MAX_CPUS`
        // processors, but no MP hand-over exists yet: the kernel boots the
        // bootstrap processor only, and the SMP suites skip themselves when
        // the snapshot reports one CPU. Bringing APs up needs the kernel's own
        // ACPI MADT walk plus an INIT-SIPI trampoline, which is documented as
        // the next NovaBoot milestone.
        (&raw mut (*target).cpu_count).write(1);
        (&raw mut (*target).bsp_lapic_id).write(0);
        (&raw mut (*target).cpu_ids[0]).write(0);
        klog!(
            Level::Info,
            "novaboot: single-processor hand-over (SMP trampoline pending)"
        );

        // The direct map is the only route from the kernel's high addresses to
        // physical memory, so its absence is unrecoverable.
        if snapshot.direct_map_offset == 0 {
            panic!("novaboot: no direct map offset; the kernel requires a higher-half direct map");
        }
        (&raw mut (*target).direct_map_offset).write(snapshot.direct_map_offset);

        // The boot manager publishes the firmware's memory map with the
        // protocol's own row kinds. A usable row is stored with the kernel's
        // `MEMMAP_USABLE` sentinel; every other kind is stored as-is, which
        // keeps it non-usable to `MemoryRegion::is_usable`.
        if snapshot.memory_count as usize > MAX_MEMORY_REGIONS.max(MAX_MEMMAP_ENTRIES) {
            panic!(
                "novaboot: memory map has {} rows; kernel stores at most {}",
                snapshot.memory_count, MAX_MEMMAP_ENTRIES
            );
        }

        let mut stored = 0usize;
        for row in snapshot.memory.iter().take(snapshot.memory_count as usize) {
            // Zero-length rows carry no information and would make the
            // allocator's watermark arithmetic ambiguous.
            let Some(length) = NonZeroU64::new(row.length) else {
                continue;
            };

            let Some(end) = row.start.checked_add(row.length) else {
                klog!(
                    Level::Warn,
                    "novaboot: dropping memory map row that overflows [{:#x}, +{:#x})",
                    row.start,
                    row.length
                );
                continue;
            };

            if end > MAX_PHYSICAL_ADDRESS {
                klog!(
                    Level::Warn,
                    "novaboot: dropping memory map row past the addressable limit \
                     [{:#x}, {end:#x})",
                    row.start
                );
                continue;
            }

            // Aligning down keeps the allocator's arithmetic uniform; a
            // misaligned base would otherwise split one page across two
            // bookkeeping regions.
            let slot = addr_of_mut!((*target).memmap[stored]);
            (&raw mut (*slot).base).write(row.start & !(PAGE_SIZE - 1));
            (&raw mut (*slot).length).write(length.get());
            (&raw mut (*slot).type_).write(usable_type(row.kind));
            stored += 1;
        }

        // Write the count last: it is what makes the entries visible, so a
        // reader that sees a count must see the entries behind it.
        (&raw mut (*target).memmap_len).write(stored);

        // The initramfs: loader-allocated pages the snapshot names by physical
        // address. Copied into kernel-owned `.bss` before anything can reclaim
        // the loader's pages.
        if snapshot.initramfs_length != 0 {
            let bytes = snapshot.initramfs_length as usize;
            if bytes > INITRAMFS_CAPACITY {
                panic!(
                    "novaboot: initramfs is {bytes} bytes; kernel stores at most {INITRAMFS_CAPACITY}"
                );
            }

            let source = snapshot
                .direct_map_offset
                .checked_add(snapshot.initramfs_start);
            match source {
                Some(source) => {
                    let target_bytes = &raw mut INITRAMFS_BYTES;
                    (&raw mut INITRAMFS_LEN).write(bytes);
                    core::ptr::copy_nonoverlapping(
                        source as *const u8,
                        target_bytes.cast::<u8>(),
                        bytes,
                    );
                    klog!(Level::Debug, "novaboot: copied initramfs {bytes} bytes");
                }
                None => {
                    klog!(
                        Level::Warn,
                        "novaboot: initramfs address overflows the direct map; booting without one"
                    );
                }
            }
        }

        // The firmware framebuffer, when the boot manager set one up. Its
        // absence is the normal case on every headless boot and is recorded as
        // `None` without a warning; an unusable one is dropped with the reason
        // spelled out, because a GUI that draws into a framebuffer whose
        // geometry was never checked corrupts nothing but the user's
        // understanding of why the screen is wrong.
        let framebuffer = if snapshot.framebuffer.present == 0 {
            None
        } else {
            let physical = snapshot.framebuffer.address;
            match normalise_framebuffer(&snapshot.framebuffer, physical, snapshot.direct_map_offset)
            {
                Ok(info) => {
                    klog!(
                        Level::Info,
                        "novaboot: framebuffer {}x{} pitch {} bpp 32 rgb shifts {}/{}/{} at {:#x}",
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
                    klog!(Level::Warn, "novaboot: framebuffer unusable: {reason}");
                    None
                }
            }
        };
        (&raw mut (*target).framebuffer).write(framebuffer);

        // The mode the menu selected, carried through to `init` by syscall.
        (&raw mut (*target).boot_mode).write(snapshot.boot_mode);
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
        panic!("novaboot: boot information failed validation");
    }

    if validation.out_of_order > 0 {
        klog!(
            Level::Warn,
            "boot: {} memory map entries were out of order",
            validation.out_of_order
        );
    }

    klog!(Level::Debug, "novaboot: boot information captured");

    READY.store(true, Ordering::Release);

    // SAFETY: `READY` was just set, and nothing writes the snapshot afterwards.
    unsafe { read_boot_info() }
}

/// Map a protocol row kind onto the kernel's memory-type encoding.
///
/// The protocol's `USABLE` kind is `0`, which is the kernel's `MEMMAP_USABLE`
/// sentinel — the identity mapping is deliberate, so a row passes through
/// unchanged and any future kind added to the protocol is non-usable by
/// default rather than silently allocatable.
const fn usable_type(kind: u16) -> u64 {
    if kind == nova_boot::region::USABLE {
        MEMMAP_USABLE
    } else {
        kind as u64
    }
}

/// Check a boot-manager framebuffer against what the kernel draws, and
/// normalise it.
///
/// The kernel draws one pixel format — XRGB8888, 32 bits, three eight-bit
/// components — and this function is the gate that keeps that promise honest.
/// The boot manager reduces the firmware's pixel format to a protocol code;
/// this function turns the code back into the shifts the kernel stores, and
/// refuses anything it cannot address.
fn normalise_framebuffer(
    framebuffer: &BootFramebuffer,
    physical_address: u64,
    direct_map_offset: u64,
) -> Result<FramebufferInfo, &'static str> {
    // The pixel layout is a protocol code, not a firmware mask: the boot
    // manager already checked the firmware's format and reduced it, so an
    // unknown code here means the producer and consumer disagree about the
    // protocol.
    let (red_shift, green_shift, blue_shift) = match framebuffer.format {
        nova_boot::fb_format::XRGB8888 => (16u8, 8u8, 0u8),
        nova_boot::fb_format::RGB888X => (0u8, 8u8, 16u8),
        _ => return Err("pixel format is not one the kernel draws"),
    };

    if framebuffer.width == 0 || framebuffer.height == 0 {
        return Err("zero width or height");
    }

    // A mode the address space cannot hold is a mode that cannot be drawn:
    // the visible bytes are `height * pitch`, and both are producer-supplied.
    if framebuffer.width > MAX_PHYSICAL_ADDRESS as u32 / 4
        || framebuffer.height > MAX_PHYSICAL_ADDRESS as u32
        || framebuffer.stride > MAX_PHYSICAL_ADDRESS as u32
    {
        return Err("geometry exceeds the addressable range");
    }

    // Rows are padded to the display's transfer granularity, so stride is at
    // least the visible bytes and may be more; less means the producer's own
    // arithmetic disagrees with its geometry. Some firmwares (QEMU's GOP among
    // them) report a stride in pixels rather than bytes; when that happens we
    // normalise it to width * 4 rather than refusing a perfectly usable framebuffer.
    let minimum_stride = framebuffer.width * 4;
    let stride = if framebuffer.stride < minimum_stride {
        klog!(
            Level::Warn,
            "novaboot: firmware-reported stride {} < width*4={}; normalising",
            framebuffer.stride,
            minimum_stride
        );
        minimum_stride
    } else {
        framebuffer.stride
    };

    // The visible bytes must fit the addressable range too, or the last rows
    // land past the end of what the kernel can map. The multiplication is
    // checked rather than performed and then bounded: two producer-supplied
    // words can overflow u64 before any comparison sees them.
    let Some(visible_bytes) = (stride as u64).checked_mul(framebuffer.height as u64) else {
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
        virtual_address: physical_address.saturating_add(direct_map_offset),
        physical_address,
        width: framebuffer.width,
        height: framebuffer.height,
        pitch: stride,
        red_shift,
        green_shift,
        blue_shift,
    })
}

/// Copy a string into a fixed-size, NUL-padded buffer, truncating safely.
///
/// Takes a raw pointer because `BootInfo` is reached through `&raw mut`, and
/// `&mut (*raw)[i]` on a `Copy` field would have to copy the whole array back
/// into the static to hand out a reference. A truncated string is still
/// NUL-terminated by the padding, so consumers can never read past the buffer.
///
/// # Safety
///
/// `dest` must point at one of `BootInfo`'s 32-byte name buffers.
unsafe fn copy_str_into(source: &str, dest: *mut [u8; 32]) {
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

/// The kernel stores at most this many processors; the protocol's table bound
/// must not silently exceed it.
const _: () = assert!(MAX_CPUS as usize <= KERNEL_MAX_CPUS);

/// Compile-time cross-checks the adapter's assumptions rest on.
const _: () = {
    // The snapshot's memory table is the bound on how many rows the kernel
    // ever sees; the kernel's own table must be at least as wide, or a valid
    // snapshot could not be copied.
    assert!(MAX_MEMORY_REGIONS <= MAX_MEMMAP_ENTRIES);
};
