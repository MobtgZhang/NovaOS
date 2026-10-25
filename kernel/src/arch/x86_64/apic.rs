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
const TIMER_PERIODIC: u32 = 1 << 18;

pub struct LocalApic {
    base: *mut u32,
}

impl LocalApic {
    pub unsafe fn init(direct_map_offset: u64) -> Self {
        let base_value = unsafe { msr::read(MSR_APIC_BASE) };
        let enabled = base_value | APIC_ENABLE;
        unsafe { msr::write(MSR_APIC_BASE, enabled) };
        let physical = (enabled & APIC_BASE_MASK) as u64;
        let virtual_base = physical.wrapping_add(direct_map_offset);
        let _ = crate::mm::address_space::map_kernel_mmio(virtual_base, physical);
        let base = virtual_base as *mut u32;
        unsafe {
            write_volatile(
                base.wrapping_add((0xF0 / 4).try_into().unwrap()),
                0xFF | (1 << 8),
            )
        };
        unsafe {
            write_volatile(
                base.wrapping_add((TIMER_DIVIDE / 4).try_into().unwrap()),
                0x03,
            )
        };
        unsafe {
            write_volatile(
                base.wrapping_add((TIMER_LVT / 4).try_into().unwrap()),
                0x20 | TIMER_PERIODIC,
            )
        };
        unsafe {
            write_volatile(
                base.wrapping_add((TIMER_INITIAL_COUNT / 4).try_into().unwrap()),
                0x1000,
            )
        };
        let timer = Self { base };
        let lvt = unsafe { read_volatile(base.wrapping_add((TIMER_LVT / 4).try_into().unwrap())) };
        let divide =
            unsafe { read_volatile(base.wrapping_add((TIMER_DIVIDE / 4).try_into().unwrap())) };
        let count = timer.current_count();
        klog!(
            Level::Info,
            "apic: base={:#x} lvt={:#x} divide={:#x} count={:#x}",
            virtual_base,
            lvt,
            divide,
            count
        );
        timer
    }

    pub unsafe fn eoi(&self) {
        unsafe { write_volatile(self.base.wrapping_add((0xB0 / 4).try_into().unwrap()), 0) };
    }

    pub fn current_count(&self) -> u32 {
        unsafe {
            read_volatile(
                self.base
                    .wrapping_add((TIMER_CURRENT_COUNT / 4).try_into().unwrap()),
            )
        }
    }
}

static mut TIMER: Option<LocalApic> = None;

pub unsafe fn init(direct_map_offset: u64) {
    let timer = unsafe { LocalApic::init(direct_map_offset) };
    unsafe { TIMER = Some(timer) };
}

pub unsafe fn eoi() {
    unsafe {
        if let Some(timer) = (&raw mut TIMER).as_mut().and_then(|slot| slot.as_mut()) {
            timer.eoi();
        }
    }
}
