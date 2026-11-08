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
    ],
}];

/// Newly created user threads must not be pinned to the bootstrap processor.
///
/// §3.2's failure mode was structural: every creation path named CPU 0, so the
/// application processors could never host user work no matter how online they
/// were. This case creates threads through the round-robin dispatcher and
/// checks they land on more than one CPU — and that every landing spot is a
/// CPU whose startup completed.
///
/// The threads are created with a kernel entry they never reach: they are
/// never spawned, so no scheduler picks them up. They are marked `Exited` and
/// reaped before the case returns, leaving the pools as it found them.
fn created_threads_dispatch_across_online_cpus() -> Result<(), &'static str> {
    let info = boot_info();
    if info.processor_count() < 2 {
        return Err("this case needs at least one application processor");
    }

    const CREATED: usize = 4;
    let space = crate::mm::address_space::kernel_space();
    let mut landed = [false; MAX_CPUS];
    let mut ids = [0u32; CREATED];
    for (index, id_slot) in ids.iter_mut().enumerate() {
        let id = 0x2000 + index as u32;
        // The entry must be a real park loop, not a placeholder address: a
        // thread created here is runnable, so the next tick on its CPU can
        // dispatch it before the cleanup below marks it exited — and a
        // dispatched kernel thread that jumps to address zero faults in low
        // memory, where the trampoline's 16-bit bytes decode as garbage in
        // 64-bit mode. The AP idle entry parks instead, so a thread that
        // slips into the window simply idles until it is reaped.
        let entry = crate::boot::smp::ap_idle_entry as *const () as u64;
        let Some(thread) =
            crate::task::create_thread(id, 0, space, crate::task::Entry::Kernel(entry))
        else {
            return Err("the round-robin dispatcher could not create a thread");
        };
        if thread.cpu != 0 && !crate::boot::smp::is_online(thread.cpu) {
            return Err("a thread was dispatched to a CPU that never came online");
        }
        landed[thread.cpu] = true;
        *id_slot = id;
    }

    let distinct = landed.iter().filter(|seen| **seen).count();
    if distinct < 2 {
        return Err("every created thread landed on the same CPU");
    }

    // Clean up: mark each created thread `Exited`, then let every CPU reap
    // its own pool — the bootstrap processor reaps directly here, and an AP
    // reaps from its scheduler path on its own tick. The reap contract skips
    // the CPU's current slot, so a thread that was dispatched before the mark
    // is first switched out (it is parked in the idle entry) and reaped on a
    // later tick. The mark is repeated because a tick that dispatched a
    // thread after the first mark writes `Running` back into the record; the
    // loop converges once every slot is empty, and the assertion below turns
    // a stuck cleanup into a test failure rather than a hung machine.
    let cpus = info.processor_count();
    let mut attempts = 0;
    while attempts < 8 {
        attempts += 1;
        for id in ids {
            for cpu in 0..cpus {
                if let Some(slot) = crate::task::slot_of_for_cpu(cpu, id)
                    && let Some(mut thread) =
                        (unsafe { crate::task::thread_in_slot_for_cpu(cpu, slot) })
                {
                    thread.state = crate::task::ThreadState::Exited;
                    unsafe { crate::task::update_thread_for_cpu(cpu, slot, thread) };
                }
            }
        }
        unsafe { crate::sched::reap_exited_threads_for_cpu(0) };

        let all_reaped = ids
            .iter()
            .all(|id| (0..cpus).all(|cpu| crate::task::slot_of_for_cpu(cpu, *id).is_none()));
        if all_reaped {
            break;
        }
        if attempts >= 8 {
            break;
        }

        // Bounded spin: give the APs' periodic ticks time to switch the
        // parked threads out and reap them. Same bound as the other cases
        // that wait for an AP tick.
        let mut spins = 0u64;
        while spins < 10_000_000 {
            spins += 1;
            // SAFETY: `pause` changes no memory state and is valid while the
            // interrupts this CPU masked are still masked.
            unsafe {
                core::arch::asm!("pause", options(nomem, nostack));
            }
        }
    }
    for id in ids {
        for cpu in 0..info.processor_count() {
            if crate::task::slot_of_for_cpu(cpu, id).is_some() {
                return Err("a reaped test thread still occupies a pool slot");
            }
        }
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
