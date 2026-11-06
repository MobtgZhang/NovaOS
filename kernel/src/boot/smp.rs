//! Multi-processor handover and AP startup.
//!
//! # Two stages
//!
//! The bootloader hands application processors to the kernel during the boot
//! protocol handover. Those processors cannot use the kernel's scheduler or
//! local APIC until the bootstrap processor has built the shared tables and the
//! per-CPU storage. This module therefore starts the APs early but makes each
//! one wait on a single atomic gate before it continues.
//!
//! The wait is deliberate: starting an AP after the bootloader has reclaimed
//! the protocol pages would leave no valid `MpInfo` pointer, and continuing an
//! AP before `mm`, the scheduler, and the first APIC mapping exist would fault
//! in code that has no diagnostic yet.
//!
//! # What the AP owns
//!
//! Each AP gets its own kernel stack, GDT/TSS pair, local APIC timer, scheduler,
//! thread pool, and one kernel-only idle thread. It does not enter ring 3 and it
//! does not use the bootstrap processor's user-service state. That keeps the SMP
//! foundation real without pretending that the service layer is concurrency-safe.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use limine::mp::MpInfo;

use crate::EXIT_PANIC;
use crate::boot::info::MAX_CPUS;

/// Set after the bootstrap processor has completed the state every AP needs.
static SMP_READY: AtomicBool = AtomicBool::new(false);

/// Whether each AP has completed its own local startup.
static AP_ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Diagnostic mirrors for the AP's scheduler and timer.
///
/// The bootstrap processor must not read an AP's live scheduler cell: the AP's
/// timer handler writes that cell while the AP is running. These atomics are the
/// safe cross-CPU snapshot for tests and boot diagnostics.
static AP_SCHEDULER_THREADS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static AP_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static AP_SCHED_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static AP_TSS_RSP0: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Publish that the bootstrap-side state is ready for APs.
pub fn mark_ready() {
    SMP_READY.store(true, Ordering::Release);
}

/// Whether the bootstrap-side state is ready.
pub fn is_ready() -> bool {
    SMP_READY.load(Ordering::Acquire)
}

/// Mark an application processor as online.
pub fn mark_online(cpu: usize) {
    if cpu < MAX_CPUS {
        AP_ONLINE[cpu].store(true, Ordering::Release);
    }
}

/// Whether an application processor has completed startup.
pub fn is_online(cpu: usize) -> bool {
    cpu < MAX_CPUS && AP_ONLINE[cpu].load(Ordering::Acquire)
}

/// Record that an AP's scheduler has one idle thread.
pub fn record_scheduler_threads(cpu: usize, count: usize) {
    if cpu < MAX_CPUS {
        AP_SCHEDULER_THREADS[cpu].store(count, Ordering::Release);
    }
}

/// The diagnostic thread count published by an AP.
pub fn scheduler_threads(cpu: usize) -> usize {
    if cpu < MAX_CPUS {
        AP_SCHEDULER_THREADS[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Record one AP timer tick.
pub fn record_tick(cpu: usize) {
    if cpu < MAX_CPUS {
        AP_TICKS[cpu].fetch_add(1, Ordering::Relaxed);
    }
}

/// The diagnostic timer-tick count published by an AP.
pub fn tick_count(cpu: usize) -> u64 {
    if cpu < MAX_CPUS {
        AP_TICKS[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Record one AP scheduler tick.
pub fn record_scheduler_tick(cpu: usize) {
    if cpu < MAX_CPUS {
        AP_SCHED_TICKS[cpu].fetch_add(1, Ordering::Relaxed);
    }
}

/// The diagnostic scheduler-tick count published by an AP.
pub fn scheduler_tick_count(cpu: usize) -> u64 {
    if cpu < MAX_CPUS {
        AP_SCHED_TICKS[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Record the AP's installed TSS `RSP0`.
pub fn record_tss_rsp0(cpu: usize, rsp0: u64) {
    if cpu < MAX_CPUS {
        AP_TSS_RSP0[cpu].store(rsp0, Ordering::Release);
    }
}

/// The diagnostic TSS `RSP0` published by an AP.
pub fn tss_rsp0(cpu: usize) -> u64 {
    if cpu < MAX_CPUS {
        AP_TSS_RSP0[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}

/// Wait until every non-bootstrap processor has completed startup.
///
/// The wait is a bounded spin, not an unbounded block: a machine whose APs
/// never reach their startup path fails the SMP suite rather than silently
/// hanging the boot.
pub fn wait_for_aps(max_spins: u64) -> bool {
    // SAFETY: the boot path calls this after `boot::capture` published the snapshot.
    let info = unsafe { crate::boot::read_boot_info() };
    let count = info.processor_count();
    if count <= 1 {
        return true;
    }

    let mut spins = 0u64;
    while spins < max_spins {
        let mut all_online = true;
        for cpu in 1..count {
            if !is_online(cpu) {
                all_online = false;
                break;
            }
        }
        if all_online {
            return true;
        }
        spins += 1;
        // SAFETY: `pause` is a yield hint. It changes no memory state and is
        // valid while interrupts are masked in the boot path.
        unsafe {
            core::arch::asm!("pause", options(nomem, nostack));
        }
    }
    false
}

/// The stable thread identifier used by an AP's kernel-only idle thread.
///
/// IDs are per-CPU scheduler identities because each CPU owns its own thread
/// pool. The range starts well above the bootstrap processor's user thread IDs,
/// so a diagnostic cannot confuse the two pools.
pub fn ap_thread_id(cpu: usize) -> u32 {
    0x1000 + cpu as u32
}

/// The kernel-only idle thread an application processor runs.
///
/// It never returns. The timer interrupt preempts it, and the AP scheduler
/// resumes it, so this is a real per-CPU scheduling loop rather than a parked
/// CPU. It deliberately touches no service or driver state.
#[unsafe(naked)]
pub unsafe extern "C" fn ap_idle_entry() -> ! {
    core::arch::naked_asm!(
        // SAFETY: `sti` states the interrupt enable explicitly, and `hlt`
        // stops the CPU until its own timer interrupt arrives. The AP has no
        // service or driver state, so the loop touches nothing shared.
        "sti",
        "hlt",
        "jmp {entry}",
        entry = sym ap_idle_entry,
    )
}

/// Entry point the bootloader calls on an application processor.
///
/// # Safety
///
/// Reached only through Limine's MP handover, with `info` pointing at the
/// bootloader-owned processor record. The function reads the extra argument
/// immediately and then waits for the bootstrap-side gate; it does not retain
/// the bootloader pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_entry(info: &MpInfo) -> ! {
    let cpu = info.extra_argument() as usize;
    if cpu == 0 || cpu >= MAX_CPUS {
        // SAFETY: the machine must stop rather than run an AP with no table.
        unsafe { crate::arch::exit(EXIT_PANIC) }
    }

    // SAFETY: this runs before the AP has installed its own exception table.
    // Masking interrupts here prevents a bootloader-provided timer from landing
    // on a gate that is not the kernel's.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }

    while !SMP_READY.load(Ordering::Acquire) {
        // SAFETY: `pause` is a yield hint. It changes no memory state and is
        // valid before the AP's GDT/TSS/IDT are installed.
        unsafe {
            core::arch::asm!("pause", options(nomem, nostack));
        }
    }

    // SAFETY: the bootstrap-side state is ready, and the AP owns its own kernel
    // stack. This abandons the bootloader-provided frame and never unwinds it.
    unsafe { crate::arch::x86_64::gdt::switch_to_kernel_stack_for_cpu_and_continue(cpu) }
}
