//! The SMP suites: processor capture, AP startup, per-CPU state, and AP timers.
//!
//! # What these cases pin
//!
//! SMP is easy to claim and hard to observe from outside. The bootloader hands
//! over processors, but a kernel that stores their IDs and never starts them
//! looks identical to one that does, unless the AP-side state is checkable. Each
//! case below names the failure it is designed to catch.
//!
//! The APs host their own idle threads, and newly created user threads are
//! dispatched round-robin across every online CPU (see
//! `created_threads_dispatch_across_online_cpus` below), so an application
//! processor's scheduler can carry user work in this SMP phase.

use super::harness::TestSuite;
use crate::boot::info::MAX_CPUS;
use core::sync::atomic::{AtomicU64, Ordering};

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
        (
            "created_threads_dispatch_across_online_cpus",
            created_threads_dispatch_across_online_cpus,
        ),
        ("revoked_global_translation_is_not_reused", revoked_global_translation_is_not_reused),
    ],
}];

const OLD_CONTENTS: u64 = 0x1122_3344;
const NEW_CONTENTS: u64 = 0x5566_7788;
static PROBE_ADDRESS: AtomicU64 = AtomicU64::new(0);
static PROBE_COMMAND: AtomicU64 = AtomicU64::new(0);
static PROBE_OBSERVED: AtomicU64 = AtomicU64::new(0);

fn wait_for_probe(word: &AtomicU64, value: u64) -> bool {
    let started = crate::arch::x86_64::cycles();
    while word.load(Ordering::Acquire) != value {
        if crate::arch::x86_64::cycles().wrapping_sub(started) >= 5_000_000_000 {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

extern "C" fn read_global_translation_on_ap() -> ! {
    let address = PROBE_ADDRESS.load(Ordering::Acquire) as *const u64;
    PROBE_OBSERVED.store(unsafe { address.read_volatile() }, Ordering::Release);
    if wait_for_probe(&PROBE_COMMAND, 1) {
        PROBE_OBSERVED.store(unsafe { address.read_volatile() }, Ordering::Release);
    }
    crate::arch::x86_64::port_io::cli();
    unsafe { crate::sched::exit_current_and_switch_for_cpu(crate::arch::current_cpu()) }
}

fn revoked_global_translation_is_not_reused() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::PageFlags;
    use crate::mm::{address_space, direct_map, page_table};

    let allocator = unsafe { crate::mm::frame_allocator() };
    let page_size = crate::arch::ADDRESS_SPACE.page_size;
    let address = boot_info().direct_map_offset + 0x0070_0000_0000;
    let space = address_space::kernel_space();
    if address_space::translate(&space, address).is_some() {
        return Err("the shootdown test slot is already mapped");
    }
    let old = allocator.alloc().map_err(|_| "no old test frame")?;
    let new = allocator.alloc().map_err(|_| "no replacement test frame")?;
    let old_phys = old as u64 * page_size;
    let new_phys = new as u64 * page_size;
    unsafe {
        direct_map::phys_to_virt(old_phys).cast::<u64>().write(OLD_CONTENTS);
        direct_map::phys_to_virt(new_phys).cast::<u64>().write(NEW_CONTENTS);
    }
    address_space::map_kernel_mmio(address, old_phys).map_err(|_| "cannot map old test frame")?;
    PROBE_ADDRESS.store(address, Ordering::Release);
    if !crate::task::launch_thread_on(
        1,
        0x2000,
        0,
        space,
        crate::sched::kernel_entry(read_global_translation_on_ap as *const () as u64),
        [0; 2],
    ) {
        return Err("cannot start the shootdown probe on CPU 1");
    }
    if !wait_for_probe(&PROBE_OBSERVED, OLD_CONTENTS) {
        return Err("CPU 1 did not populate the old translation");
    }
    address_space::set_writable(&space, address, false).map_err(|_| "cannot revoke write access")?;
    let read_only = page_table::leaf_entry(&space.pml4(), address).ok_or("test PTE disappeared")?;
    if read_only & PageFlags::WRITABLE.bits() != 0 {
        return Err("write permission remained set");
    }
    if address_space::unmap(&space, address) != Ok(old_phys) {
        return Err("unmap did not return the revoked frame");
    }
    unsafe { allocator.dealloc(old) };
    address_space::map_kernel_mmio(address, new_phys).map_err(|_| "cannot map replacement frame")?;
    PROBE_COMMAND.store(1, Ordering::Release);
    if !wait_for_probe(&PROBE_OBSERVED, NEW_CONTENTS) {
        return Err("CPU 1 did not observe the replacement translation");
    }
    if address_space::unmap(&space, address) != Ok(new_phys) {
        return Err("cannot unmap the replacement frame");
    }
    unsafe { allocator.dealloc(new) };
    Ok(())
}

fn created_threads_dispatch_across_online_cpus() -> Result<(), &'static str> {
    let count = boot_info().processor_count();
    if count < 2 {
        return Err("this case needs at least one application processor");
    }
    let mut first = [false; MAX_CPUS];
    for _ in 0..MAX_CPUS {
        let mut order = crate::task::dispatch_order();
        let cpu = order.next().ok_or("no online CPU to dispatch to")?;
        first[cpu] = true;
        let mut seen = [false; MAX_CPUS];
        seen[cpu] = true;
        for cpu in order {
            if core::mem::replace(&mut seen[cpu], true) {
                return Err("dispatch visits one CPU more than once");
            }
        }
        if (0..MAX_CPUS).any(|cpu| seen[cpu] != (cpu < count)) {
            return Err("dispatch did not cover exactly the online CPUs");
        }
    }
    if !first[..count].iter().all(|seen| *seen) {
        return Err("not every online CPU was preferred at least once");
    }
    Ok(())
}

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
