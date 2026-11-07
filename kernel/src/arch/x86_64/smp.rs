//! The INIT-SIPI trampoline that wakes application processors.
//!
//! # What this module is for
//!
//! ACPI told the boot manager *which* processors exist (the snapshot's
//! `cpu_ids`); this module is the part that actually starts them. An x86-64
//! application processor comes out of reset in 16-bit real mode at whatever
//! page a startup IPI names, so waking one means placing a small program at a
//! page the kernel's own page tables also map, having it build long mode from
//! the kernel's CR3, and then jumping into the kernel's per-CPU startup path.
//!
//! # The page layout
//!
//! Everything lives in one 4 KiB page, chosen at boot time from the snapshot's
//! usable regions (below 1 MiB — the startup IPI's vector is 8 bits):
//!
//! ```text
//! 0x0000  16-bit real-mode entry: segments, PAE, CR3, EFER.LME, PG, far jump
//! 0x0400  64-bit stub: load mailbox, jump to the kernel's AP entry
//! 0x0780  64-bit GDT (null + flat long-mode code)
//! 0x0790  GDTR image (limit + base) the 16-bit entry loads
//! 0x0800  mailbox: CR3 (u32), entry (u64), cpu index (u32)
//! ```
//!
//! The mailbox is the hand-off's contract with the 16-bit entry and the
//! 64-bit stub: the bootstrap processor writes it once per AP (the CPU index
//! changes), and the woken processor reads it *after* paging is on, through
//! the identity mapping this page has in the kernel's own tables.
//!
//! # The identity mapping
//!
//! After the entry enables paging with the kernel's CR3, every address it
//! touches must translate: the trampoline page itself, the GDT, and later the
//! kernel image. The kernel half (PML4 256..512) covers the kernel image but
//! *not* this page, so `bring_up` maps the page identically into PML4[0] of
//! the kernel space before waking anyone. Process spaces clone only 256..512,
//! so user space never sees the identity map.
//!
//! # Why one AP at a time
//!
//! The mailbox is shared, so the bootstrap processor wakes processors
//! serially: write the index, INIT, SIPI, wait until the AP reports online,
//! then move to the next. Slower than broadcasting, and impossible to get
//! subtly wrong.

use core::ptr::{write_bytes, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::apic;
use crate::klog;
use crate::logger::Level;
use crate::mm::{address_space, direct_map};

/// The startup IPI's vector is 8 bits, so the trampoline page is below 1 MiB.
const TRAMPOLINE_LIMIT: u64 = 0x10_0000;

/// One past the last byte of a 4 KiB page, i.e. the page's size.
const PAGE_BYTES: u64 = 0x1000;

/// Offsets within the trampoline page.
const OFFSET_STUB: u64 = 0x0400;
const OFFSET_GDT: u64 = 0x0780;
const OFFSET_GDTR: u64 = 0x0790;
const MAIL_CR3: u64 = 0x0800;
const MAIL_ENTRY: u64 = 0x0808;
const MAIL_CPU: u64 = 0x0810;

/// The 16-bit real-mode entry.
///
/// Hand-assembled because it runs *before* any assembler the kernel can call:
/// the AP starts at CS:IP `base:0` after the startup IPI, and these bytes are
/// the whole program until long mode is on. The encoding notes name each
/// instruction so a reader can verify the bytes against an encoder.
///
/// Sequence: mask interrupts, point DS at the page's paragraph so the fixed
/// 16-bit displacements below reach the GDTR image and the mailbox, load the
/// 64-bit GDT, enable PAE, read CR3 from the mailbox, set `EFER.LME` and
/// `EFER.NXE` (the kernel's tables carry the no-execute bit), enable paging —
/// with protection enable, because the architecture faults `PG` while `PE` is
/// clear and an INIT-reset AP starts with `CR0.PE = 0` — and far-jump into
/// the 64-bit stub with `66 EA` — the operand-size prefix
/// that widens the far-jump offset to 32 bits, which a 16-bit `IP` alone
/// cannot express. The offset is patched to `base + 0x400` at install time;
/// the code segment's base is zero, so the *offset* is the whole linear
/// address and lands exactly on the stub through the identity mapping.
///
/// Every 32-bit immediate carries an explicit `66` prefix. This code runs in
/// 16-bit real mode, where an unprefixed `0D` decodes as `or ax, imm16`: the
/// tail of the intended 32-bit immediate is then fetched as instructions —
/// stray `add [bx+si], al` writes through `DS:[0]`, and `or eax, 0x80000001`
/// silently reduces to `or ax, 0x0001`, which never enables paging. The
/// `mov ecx, imm32` EFER index needs the same prefix (`66 B9`).
const REAL_MODE_ENTRY: [u8; 0x42] = [
    0xFA, // cli
    0x8C, 0xC8, // mov ax, cs
    0x8E, 0xD8, // mov ds, ax
    0x0F, 0x01, 0x16, 0x90, 0x07, // lgdt [0x0790]
    0x0F, 0x20, 0xE0, // mov eax, cr4
    0x66, 0x0D, 0x20, 0x00, 0x00, 0x00, // or eax, 0x20 (PAE)
    0x0F, 0x22, 0xE0, // mov cr4, eax
    0x66, 0x8B, 0x06, 0x00, 0x08, // mov eax, [0x0800] (mailbox CR3)
    0x0F, 0x22, 0xD8, // mov cr3, eax
    0x66, 0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, 0xC0000080 (EFER)
    0x0F, 0x32, // rdmsr
    0x66, 0x0D, 0x01, 0x01, 0x00, 0x00, // or eax, 0x101 (LME | NXE)
    0x0F, 0x30, // wrmsr
    0x0F, 0x20, 0xC0, // mov eax, cr0
    0x66, 0x0D, 0x01, 0x00, 0x00, 0x80, // or eax, 0x80000001 (PG | PE)
    0x0F, 0x22, 0xC0, // mov cr0, eax
    0x66, 0xEA, 0x40, 0x00, 0x00, 0x00, 0x08, 0x00, // jmp far 0x0008:base+0x400
];

/// The 64-bit stub the far jump lands on.
///
/// RIP-relative loads pick the mailbox up (entry at page+0x808, CPU index at
/// page+0x810) and `jmp rax` enters the kernel's per-CPU startup path, which
/// installs the AP's own stack from the CPU index in `edi`. The displacements
/// are fixed by the layout constants: each is the mailbox field's offset minus
/// the *next* instruction's RIP.
const STUB: [u8; 15] = [
    // 0x400: mov rax, [rip+0x401]  → page+0x808, the entry address.
    0x48, 0x8B, 0x05, 0x01, 0x04, 0x00, 0x00,
    // 0x407: mov edi, [rip+0x403]  → page+0x810, the CPU index.
    0x8B, 0x3D, 0x03, 0x04, 0x00, 0x00, // 0x40D: jmp rax.
    0xFF, 0xE0,
];

/// The GDT the 16-bit entry loads: a null descriptor and one flat long-mode
/// code segment (`L=1`, `D=0`, base 0, limit 0xFFFF), so selector `0x08`
/// names it.
const GDT_CODE: [u8; 8] = [
    0xFF, 0xFF, // limit 15:0
    0x00, 0x00, 0x00, // base 23:0
    0x9A, // present, ring 0, code, execute/read
    0x20, // flags: L=1, D=0, G=0; limit 19:16 = 0
    0x00, // base 31:24
];

/// The physical base of the installed trampoline page, `0` until one is built.
static TRAMPOLINE_BASE: AtomicU64 = AtomicU64::new(0);

/// Whether an AP bring-up is in progress — the mailbox is single-tenant.
static WAKE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Wake the application processor in kernel slot `cpu`.
///
/// Runs the full INIT–SIPI sequence: the INIT IPI puts the processor into the
/// wait-for-SIPI state, the first startup IPI points it at the trampoline
/// page, and — if the AP has not reported online after a bounded wait — a
/// second startup IPI, as the Intel-recommended retry.
///
/// Returns whether the AP reached [`crate::boot::smp::mark_online`] within the
/// wait. A `false` is a boot-time report, not a reason to stop: the caller
/// decides whether a machine with a missing processor continues.
pub fn bring_up(cpu: usize) -> bool {
    // SAFETY: `capture` ran during boot, so the snapshot is published and
    // immutable.
    let info = unsafe { crate::boot::read_boot_info() };
    let Some(lapic_id) = info.processor_id(cpu) else {
        klog!(Level::Error, "smp: cpu {cpu} has no snapshot identity");
        return false;
    };

    let Some(base) = trampoline_base(&info) else {
        klog!(Level::Error, "smp: no page below 1 MiB for the trampoline");
        return false;
    };

    // Serial wakeups only: two CPUs in the bring-up path would share the
    // mailbox. Re-entrancy here means a caller bug, reported rather than
    // corrupted around.
    if WAKE_IN_PROGRESS
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        klog!(Level::Error, "smp: a bring-up is already in progress");
        return false;
    }
    let started = bring_up_one(cpu, lapic_id, base);
    WAKE_IN_PROGRESS.store(false, Ordering::Release);
    started
}

/// The serial body of [`bring_up`], guarded by `WAKE_IN_PROGRESS`.
fn bring_up_one(cpu: usize, lapic_id: u32, base: u64) -> bool {
    // SAFETY: the boot path runs with interrupts masked, the kernel space is
    // published, and `install_once` maps the trampoline before anything wakes.
    unsafe {
        if !install_once(base) {
            klog!(
                Level::Error,
                "smp: cannot install the trampoline at {base:#x}"
            );
            return false;
        }

        let pml4 = address_space::kernel_space().pml4_phys();
        // The mailbox stores CR3 in 32 bits, which every page table the
        // allocator can produce satisfies: frames come from regions whose
        // bases are 32-bit on any machine this kernel boots on. A larger one
        // is reported rather than truncated silently.
        if pml4 >= 0x1_0000_0000 {
            klog!(
                Level::Error,
                "smp: kernel PML4 {pml4:#x} exceeds the 32-bit mailbox"
            );
            return false;
        }

        let entry = super::gdt::switch_to_kernel_stack_for_cpu_and_continue as unsafe fn(usize) -> !
            as *const () as u64;
        write_mailbox(base, pml4 as u32, entry, cpu as u32);

        // INIT, then the required settle delay, then the startup IPI. The
        // delay is a bounded pause-spin rather than a timer read: the boot
        // path has no tick source it can sleep on, and "long enough" here is
        // a hardware courtesy, not a correctness bound — the AP latches
        // whatever arrives after its INIT.
        apic::send_init(lapic_id);
        spin(INIT_DELAY_SPINS);

        let vector = (base >> 12) as u8;
        apic::send_startup(lapic_id, vector);
        if wait_online(cpu, FIRST_SIPI_SPINS) {
            return true;
        }

        // The Intel-recommended retry: a second startup IPI, then a much
        // longer wait.
        apic::send_startup(lapic_id, vector);
        let online = wait_online(cpu, SECOND_SIPI_SPINS);
        if online {
            klog!(
                Level::Info,
                "smp: cpu {cpu} (lapic {lapic_id:#x}) online after retry"
            );
        }
        online
    }
}

/// The bounded spin lengths, in `pause` iterations.
///
/// A `pause` is a few tens of cycles natively and one instruction under
/// emulation, so these are order-of-magnitude budgets: the INIT delay is the
/// ~10 ms courtesy the Intel sequence calls for, the first wait is long
/// enough for a healthy AP, and the retry wait is generous enough to cover a
/// slow virtual machine.
const INIT_DELAY_SPINS: u64 = 2_000_000;
const FIRST_SIPI_SPINS: u64 = 1_000_000;
const SECOND_SIPI_SPINS: u64 = 50_000_000;

/// Install the trampoline image at `base`, once per boot.
///
/// Maps the page identically into the kernel space (PML4[0]) and writes the
/// entry, stub, GDT, GDTR image, and mailbox. The mailbox's CPU index is
/// rewritten per wakeup; nothing else changes.
///
/// # Safety
///
/// The kernel space must be published and the direct map live; runs on the
/// bootstrap processor with interrupts masked.
unsafe fn install_once(base: u64) -> bool {
    if TRAMPOLINE_BASE.load(Ordering::Acquire) == base {
        return true;
    }
    // The identity mapping: PML4[0] in the kernel space. The walk allocates
    // its intermediate tables from the frame allocator, which is why this
    // runs on the bootstrap processor and not on an AP.
    if address_space::map_kernel_mmio(base, base).is_err() {
        return false;
    }

    // SAFETY: the direct map covers every firmware-reported region, and the
    // frame at `base` is inside one — `trampoline_base` chose it that way.
    let page = direct_map::phys_to_virt(base);
    // SAFETY: the page is the trampoline's alone; the frame allocator never
    // hands out frames below the kernel image, and the identity map makes
    // this the page an AP executes.
    unsafe {
        write_image(page);
    }
    TRAMPOLINE_BASE.store(base, Ordering::Release);
    true
}

/// Write the trampoline image into the page at direct-map `page` pointer.
///
/// # Safety
///
/// `page` must be a writable pointer to one 4 KiB page, and the caller must
/// be the only writer.
unsafe fn write_image(page: *mut u8) {
    let mut entry = REAL_MODE_ENTRY;
    // Patch the far jump's 32-bit offset: bytes 0x3C..0x40 are the operand,
    // little-endian, and the target is the stub's linear address.
    let target = (page as u64) + OFFSET_STUB;
    let offset_bytes = (target as u32).to_le_bytes();
    entry[0x3C] = offset_bytes[0];
    entry[0x3D] = offset_bytes[1];
    entry[0x3E] = offset_bytes[2];
    entry[0x3F] = offset_bytes[3];

    // SAFETY: the page is 4 KiB, and every write below is within it.
    unsafe {
        // Zero the whole page first so no stale firmware byte survives in a
        // region the layout does not name.
        write_bytes(page, 0, PAGE_BYTES as usize);

        let mut cursor = page;
        for byte in entry {
            write_volatile(cursor, byte);
            cursor = cursor.add(1);
        }

        cursor = page.add(OFFSET_STUB as usize);
        for byte in STUB {
            write_volatile(cursor, byte);
            cursor = cursor.add(1);
        }

        cursor = page.add(OFFSET_GDT as usize);
        // The null descriptor is eight zero bytes.
        for _ in 0..8 {
            write_volatile(cursor, 0);
            cursor = cursor.add(1);
        }
        for byte in GDT_CODE {
            write_volatile(cursor, byte);
            cursor = cursor.add(1);
        }

        // The GDTR image: a 16-bit limit (two descriptors minus one) and the
        // 32-bit base. The 16-bit entry loads it with `lgdt [0x0790]`
        // relative to DS at the page's paragraph.
        cursor = page.add(OFFSET_GDTR as usize);
        for byte in 0x0Fu16.to_le_bytes() {
            write_volatile(cursor, byte);
            cursor = cursor.add(1);
        }
        for byte in (((page as u64) + OFFSET_GDT) as u32).to_le_bytes() {
            write_volatile(cursor, byte);
            cursor = cursor.add(1);
        }

        // The mailbox fields CR3, entry, and CPU index are written by
        // `write_mailbox`; the zeroed page already guarantees a benign
        // default.
    }
}

/// Publish one AP's hand-off values in the mailbox.
///
/// # Safety
///
/// The trampoline must be installed at `base`, and no AP may be executing
/// its image while the mailbox is rewritten.
unsafe fn write_mailbox(base: u64, pml4: u32, entry: u64, cpu: u32) {
    // SAFETY: the page is installed and quiescent (the caller wakes at most
    // one AP after this and waits for it to report online).
    unsafe {
        let page = direct_map::phys_to_virt(base);
        write_volatile(page.add(MAIL_CR3 as usize) as *mut u32, pml4);
        write_volatile(page.add(MAIL_ENTRY as usize) as *mut u64, entry);
        write_volatile(page.add(MAIL_CPU as usize) as *mut u32, cpu);
    }
}

/// Choose the trampoline page: the highest fully-usable page below 1 MiB that
/// no boot image claims.
///
/// The startup IPI's 8-bit vector is what forces "below 1 MiB"; the frame
/// allocator never hands out frames below the kernel image, so a page there
/// stays exclusive for as long as the machine runs. The highest candidate is
/// preferred because the lowest pages carry firmware debris (the IVT, the
/// BDA) that no memory map models precisely.
fn trampoline_base(info: &crate::boot::BootInfo) -> Option<u64> {
    let mut best: Option<u64> = None;
    for region in info.regions() {
        if !region.is_usable() {
            continue;
        }
        // The candidate is the last whole page inside the region's part
        // below 1 MiB. Starting at one page in keeps the IVT and BIOS data
        // area out of play even when the map calls them usable.
        let start = region.base.max(PAGE_BYTES);
        let end = region.end().min(TRAMPOLINE_LIMIT);
        if end < start + PAGE_BYTES {
            continue;
        }
        let candidate = end - PAGE_BYTES;
        if best.map_or(true, |current| candidate > current) {
            best = Some(candidate);
        }
    }

    // The initramfs was placed by the boot manager, not by the frame
    // allocator, so it is the one image this walk must dodge explicitly.
    let Some(base) = best else {
        return None;
    };
    // SAFETY: `initramfs` returns a direct-map slice published by `capture`.
    if let Some(bytes) = unsafe { crate::boot::initramfs() } {
        // SAFETY: the slice's pointer is a direct-map address.
        let start = direct_map::virt_to_phys(bytes.as_ptr() as u64);
        let end = start.saturating_add(bytes.len() as u64);
        let base_end = base + PAGE_BYTES;
        if start < base_end && base < end {
            klog!(
                Level::Error,
                "smp: the only page below 1 MiB holds the initramfs"
            );
            return None;
        }
    }
    Some(base)
}

/// Bounded `pause` spin.
fn spin(count: u64) {
    for _ in 0..count {
        // SAFETY: `pause` is a yield hint; it changes no memory state and is
        // valid while interrupts are masked.
        unsafe {
            core::arch::asm!("pause", options(nomem, nostack));
        }
    }
}

/// Poll an AP's online flag for `spins` bounded iterations.
fn wait_online(cpu: usize, spins: u64) -> bool {
    for _ in 0..spins {
        if crate::boot::smp::is_online(cpu) {
            return true;
        }
        // SAFETY: as in `spin`.
        unsafe {
            core::arch::asm!("pause", options(nomem, nostack));
        }
    }
    false
}

/// Diagnostic mirror of the installed trampoline base, for tests and logs.
pub fn installed_base() -> u64 {
    TRAMPOLINE_BASE.load(Ordering::Acquire)
}
