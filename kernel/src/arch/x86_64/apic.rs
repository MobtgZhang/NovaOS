//! Local APIC setup, timer, and identification.
//!
//! # One physical block, one per CPU
//!
//! Every processor's local APIC is exposed at the same physical base address.
//! The kernel maps that address once, then configures each processor through
//! the same virtual page. The timer is per-CPU state, not a property of the
//! mapping: setting the LVT, divider, and initial count on an AP arms that AP's
//! own timer.
//!
//! # Why CPUID is the CPU identity
//!
//! The interrupt handler runs on whichever processor delivered the interrupt.
//! It needs to choose the per-CPU scheduler, TSS, stack, and thread pool without
//! a per-CPU register the kernel can name in ordinary Rust. CPUID leaf 1 carries
//! the processor's initial local APIC identity in `EBX[31:24]`, and `BootInfo`
//! maps that value to the kernel's CPU slot. Reading CPUID rather than the xAPIC
//! ID register matters on emulated machines whose ID register is not populated:
//! the architectural identity is still reported by CPUID.

use core::ptr::{read_volatile, write_volatile};

use super::msr;
use crate::klog;
use crate::logger::Level;

const MSR_APIC_BASE: u32 = 0x1B;
const APIC_ENABLE: u64 = 1 << 11;
const APIC_BASE_MASK: u64 = 0xFFFF_F000;

const TIMER_LVT: u32 = 0x320;
const TIMER_DIVIDE: u32 = 0x3E0;
const TIMER_INITIAL_COUNT: u32 = 0x380;
const TIMER_CURRENT_COUNT: u32 = 0x390;
// The LVT timer's mode field occupies bits 18:17: `00` is one-shot,
// `01` is periodic, and `10` is TSC-deadline. Setting bit 18 alone selects
// TSC-deadline, which ignores the initial-count register entirely — the
// timer then never counts down and never delivers a vector. Periodic mode
// is therefore bit 17, not bit 18.
const TIMER_PERIODIC: u32 = 1 << 17;
const TIMER_VECTOR: u32 = 0x20;

/// The mapped local APIC block.
///
/// It is the same page for every processor, so one pointer is enough. The
/// per-CPU identities come from the hardware's APIC ID register, not from the
/// address.
static mut APIC_BASE: *mut u32 = core::ptr::null_mut();

/// Configure one processor's local APIC and timer.
///
/// # Safety
///
/// Must run with interrupts masked, after the direct map exists, and before the
/// first APIC timer interrupt is unmasked.
pub unsafe fn init_for_cpu(cpu: usize, direct_map_offset: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let base_value = msr::read(MSR_APIC_BASE);
        let enabled = base_value | APIC_ENABLE;
        msr::write(MSR_APIC_BASE, enabled);

        let physical = (enabled & APIC_BASE_MASK) as u64;
        let virtual_base = physical.wrapping_add(direct_map_offset);

        // The BSP has already mapped this physical block. The APs share it, so
        // only the first caller performs the mapping.
        if APIC_BASE.is_null() {
            let _ = crate::mm::address_space::map_kernel_mmio(virtual_base, physical);
            APIC_BASE = virtual_base as *mut u32;
        }

        let base = APIC_BASE;
        write_volatile(
            base.wrapping_add((0xF0 / 4).try_into().unwrap()),
            0xFF | (1 << 8),
        );
        write_volatile(
            base.wrapping_add((TIMER_DIVIDE / 4).try_into().unwrap()),
            0x03,
        );
        write_volatile(
            base.wrapping_add((TIMER_LVT / 4).try_into().unwrap()),
            TIMER_VECTOR | TIMER_PERIODIC,
        );
        write_volatile(
            base.wrapping_add((TIMER_INITIAL_COUNT / 4).try_into().unwrap()),
            0x1000,
        );

        if cpu == 0 {
            let lvt = read_volatile(base.wrapping_add((TIMER_LVT / 4).try_into().unwrap()));
            let divide = read_volatile(base.wrapping_add((TIMER_DIVIDE / 4).try_into().unwrap()));
            let count =
                read_volatile(base.wrapping_add((TIMER_CURRENT_COUNT / 4).try_into().unwrap()));
            klog!(
                Level::Info,
                "apic: base={:#x} lvt={:#x} divide={:#x} count={:#x}",
                virtual_base,
                lvt,
                divide,
                count
            );
        }
    }
}

/// Read the CPU's initial APIC identity from CPUID leaf 1.
pub fn cpuid_apic_id() -> u32 {
    // SAFETY: `cpuid` is an unprivileged instruction that writes only its four
    // documented result registers.
    unsafe { core::arch::x86_64::__cpuid(1) }.ebx >> 24
}

/// Configure the bootstrap processor.
pub unsafe fn init(direct_map_offset: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe { init_for_cpu(0, direct_map_offset) }
}

/// The kernel CPU slot of the processor currently delivering an interrupt.
///
/// # Safety
///
/// The local APIC must already be mapped. Called from interrupt handlers after
/// APIC setup.
pub fn cpu_index() -> usize {
    // CPUID leaf 1 carries the processor's initial APIC identity in EBX bits
    // 24-31. Reading CPUID rather than the APIC ID register keeps the identity
    // usable on emulated machines whose xAPIC ID register is not populated, and
    // the value is still the architectural identity the bootloader snapshot maps.
    let lapic_id = cpuid_apic_id();
    // SAFETY: `read_boot_info` is called after `boot::capture`, and the snapshot
    // is immutable from then on.
    let info = unsafe { crate::boot::read_boot_info() };
    if let Some(index) = info.cpu_index_of_lapic_id(lapic_id) {
        index
    } else if info.processor_count() == 1 {
        // A bootloader without MP support still booted this one processor.
        // The APIC ID is not part of the snapshot in that fallback, but the
        // only possible kernel slot is zero.
        0
    } else {
        panic!("apic: lapic id {lapic_id} is not in the boot snapshot");
    }
}

/// Acknowledge the current processor's timer interrupt.
///
/// # Safety
///
/// Must be called while servicing an APIC interrupt on the same processor.
pub unsafe fn eoi() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        if APIC_BASE.is_null() {
            return;
        }
        write_volatile(APIC_BASE.wrapping_add((0xB0 / 4).try_into().unwrap()), 0);
    }
}

/// The current processor's remaining timer count.
pub fn current_count() -> u32 {
    // SAFETY: reading a mapped APIC register is safe after `init_for_cpu`.
    unsafe {
        if APIC_BASE.is_null() {
            return 0;
        }
        read_volatile(APIC_BASE.wrapping_add((TIMER_CURRENT_COUNT / 4).try_into().unwrap()))
    }
}
