//! The SMP suites: processor capture, AP startup, per-CPU state, and AP timers.
//!
//! # What these cases pin
//!
//! SMP is easy to claim and hard to observe from outside. The bootloader hands
//! over processors, but a kernel that stores their IDs and never starts them
//! looks identical to one that does, unless the AP-side state is checkable. Each
//! case below names the failure it is designed to catch.
//!
//! The APs run kernel-only idle threads on the bootstrap processor's behalf.
//! User processes, services, and drivers remain bootstrap-owned, so these tests
//! verify the SMP foundation rather than claiming concurrent service scheduling.

use super::harness::TestSuite;
use crate::boot::info::MAX_CPUS;

/// The SMP suites, in registration order.
pub static SMP_SUITES: &[TestSuite] = &[TestSuite {
    name: "smp_cpu_state",
    cases: &[
        (
            "the_boot_snapshot_reports_multiple_processors",
            boot_snapshot_reports_multiple_processors,
        ),
        ("processor_ids_are_unique", processor_ids_are_unique),
        (
            "application_processors_are_online",
            application_processors_are_online,
        ),
        (
            "application_processors_have_separate_idle_pools",
            application_processors_have_separate_idle_pools,
        ),
        (
            "application_processors_have_their_own_stacks_and_tss",
            application_processors_have_their_own_stacks_and_tss,
        ),
        (
            "application_processors_take_their_own_scheduler_ticks",
            application_processors_take_their_own_scheduler_ticks,
        ),
    ],
}];

fn boot_info() -> &'static crate::boot::BootInfo {
    // SAFETY: the boot harness runs after `boot::capture` published the snapshot.
    unsafe { crate::boot::read_boot_info() }
}

fn boot_snapshot_reports_multiple_processors() -> Result<(), &'static str> {
    let info = boot_info();
    if info.processor_count() < 2 {
        return Err("the boot snapshot reports fewer than two processors");
    }
    if info.processor_count() > MAX_CPUS {
        return Err("the boot snapshot reports more processors than the static CPU table");
    }
    if info.bsp_lapic_id != info.cpu_ids[0] {
        return Err("the bootstrap processor id does not match the first captured id");
    }
    Ok(())
}

fn processor_ids_are_unique() -> Result<(), &'static str> {
    let info = boot_info();
    for index in 0..info.processor_count() {
        for other in 0..index {
            if info.cpu_ids[index] == info.cpu_ids[other] {
                return Err("two captured processors share a local APIC id");
            }
        }
    }
    Ok(())
}

fn application_processors_are_online() -> Result<(), &'static str> {
    let info = boot_info();
    for cpu in 1..info.processor_count() {
        if !crate::boot::smp::is_online(cpu) {
            return Err("an application processor did not complete its startup path");
        }
    }
    Ok(())
}

fn application_processors_have_separate_idle_pools() -> Result<(), &'static str> {
    let info = boot_info();
    for cpu in 1..info.processor_count() {
        if crate::boot::smp::scheduler_threads(cpu) != 1 {
            return Err("an application processor's scheduler has no idle thread");
        }

        let id = crate::boot::smp::ap_thread_id(cpu);
        if id <= 0x1000 {
            return Err("the application processor thread id is not in the AP range");
        }
        if unsafe { crate::task::thread_by_id(id) }.is_some() {
            return Err("an AP idle thread leaked into the bootstrap processor's pool");
        }
        if crate::task::slot_of(id).is_some() {
            return Err("an AP idle thread occupies a bootstrap processor slot");
        }
    }
    Ok(())
}

fn application_processors_have_their_own_stacks_and_tss() -> Result<(), &'static str> {
    let info = boot_info();
    let bsp_top = crate::arch::x86_64::gdt::kernel_stack_top_for_cpu(0);
    let mut previous_top = bsp_top;

    for cpu in 1..info.processor_count() {
        let top = crate::arch::x86_64::gdt::kernel_stack_top_for_cpu(cpu);
        let fault_top = crate::arch::x86_64::gdt::fault_stack_top_for_cpu(cpu);

        if top == 0 || fault_top == 0 {
            return Err("an application processor has no kernel or fault stack");
        }
        if top % 16 != 0 || fault_top % 16 != 0 {
            return Err("an application processor stack top is not 16-byte aligned");
        }
        if top == previous_top {
            return Err("two processors share the same kernel boot stack top");
        }
        if crate::boot::smp::tss_rsp0(cpu) != top {
            return Err("an application processor's TSS does not name its own stack");
        }

        previous_top = top;
    }
    Ok(())
}

fn application_processors_take_their_own_scheduler_ticks() -> Result<(), &'static str> {
    let info = boot_info();

    for cpu in 1..info.processor_count() {
        if crate::boot::smp::scheduler_tick_count(cpu) == 0 {
            // The AP's timer is periodic; a short bounded spin lets the first
            // tick arrive before this case declares the AP scheduler dead.
            let mut spins = 0u64;
            while crate::boot::smp::scheduler_tick_count(cpu) == 0 && spins < 10_000_000 {
                spins += 1;
                // SAFETY: `pause` changes no memory state and is valid while the
                // bootstrap processor is running with interrupts masked.
                unsafe {
                    core::arch::asm!("pause", options(nomem, nostack));
                }
            }
        }

        if crate::boot::smp::scheduler_tick_count(cpu) == 0 {
            return Err("an application processor's scheduler never took a timer tick");
        }
    }
    Ok(())
}
