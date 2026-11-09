//! Round-robin scheduling, preemption, and the context switch.
//!
//! # Policy and mechanism
//!
//! Two things are called "the scheduler" and they are not the same thing:
//!
//! 1. **The policy** — which thread runs next, when a timeslice expires, when
//!    a thread exits and who reaps it.
//! 2. **The mechanism** — saving registers, switching stacks, loading a new
//!    CR3, updating the TSS.
//!
//! The policy lives in `nova_core::scheduler`, where the host test suite
//! exercises it directly. This module is the mechanism, and it calls into that
//! state machine to ask "who runs next?".
//!
//! # The context switch
//!
//! The switch is a naked assembly trampoline, and it has to be one for the
//! same reason the exception stubs are: the compiler must not be allowed to
//! allocate registers around instructions that move `RSP` or that replace the
//! whole register file.
//!
//! # The CR3 and the TSS
//!
//! Two pieces of machine state are per-thread and must be updated on every
//! switch:
//!
//! - **CR3** — the address space. Loaded from the incoming thread's space.
//!   The kernel half is shared, so the switch costs a TLB flush for the user
//!   half only.
//! - **TSS `RSP0`** — the stack an interrupt arriving from ring 3 lands on.
//!   It must name the *incoming* thread's kernel stack, or a timer interrupt
//!   would land on the wrong thread's stack.
//!
//! # Why the resume is an `iretq` and not a `jmp`
//!
//! Both kinds of thread resume through the same instruction, but for different
//! reasons, and the difference is the whole reason one structure suffices:
//!
//! - A **user** thread's saved `cs` names a ring 3 descriptor, so `iretq`
//!   performs the privilege change. The frame's `rsp` slot holds the *user's*
//!   stack pointer, and `iretq` restores it — so the thread resumes on the
//!   user's own stack, in ring 3.
//! - A **kernel** thread's saved `cs` names the ring 0 code descriptor, so
//!   `iretq` does not change the privilege level. The frame's `rsp` slot is
//!   the kernel stack pointer the thread should continue with.
//!
//! A `jmp` would work for the kernel case and would *silently* fail for the
//! user case: the thread would resume in ring 0, execute the user's
//! instructions as kernel code, and the first user instruction that touched
//! a user page would fault with a fault address in the user half — which
//! reads as a page-table bug rather than as a missing privilege change.

use core::cell::UnsafeCell;
use core::mem::size_of;

use crate::boot::info::MAX_CPUS;
use crate::mm::address_space;

use crate::klog;
use crate::logger::Level;
use crate::syscall::UserRegisters;
use crate::task::{self, Context, Entry, Thread, ThreadState};

/// How many ticks a thread runs before it is preempted.
///
/// At the PIT's 100 Hz this is a 40 ms quantum. Long enough that a thread
/// doing real work is not preempted constantly, short enough that a busy loop
/// is observed to be preempted promptly.
pub const DEFAULT_TIMESLICE: u64 = 4;

/// The five words an interrupt or syscall return path restores with `iretq`.
#[repr(C)]
struct IretFrame {
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// Every piece of scheduler state a single CPU owns.
///
/// The policy state machine, the current pool slot, and the tick/switch
/// counters are per-CPU because two processors must not share a queue or a
/// current thread. The counters are still exposed for diagnostics.
struct CpuSchedulerState {
    scheduler: nova_core::scheduler::Scheduler,
    current_slot: Option<usize>,
    switch_count: u64,
    tick_count: u64,
}

/// One CPU's scheduler state, under an [`UnsafeCell`].
///
/// A state machine is read and written in pieces: `tick` reads the current
/// thread's quantum and writes the tick counter, `schedule` mutates the queue,
/// and tests read the queue order while no switch is in flight. A bare
/// `static mut` cannot lend a `&mut` to any of those without the borrow checker
/// correctly objecting that a shared reference to a mutable static is undefined
/// behaviour.
struct SchedulerCell(UnsafeCell<CpuSchedulerState>);

// SAFETY: each CPU's state is read or written only by that CPU, with interrupts
// masked. The bootstrap processor never mutates an AP's scheduler; it only reads
// diagnostic atomics published by the AP.
unsafe impl Sync for SchedulerCell {}

/// Per-CPU scheduler state.
static SCHEDULERS: [SchedulerCell; MAX_CPUS] = [const {
    SchedulerCell(UnsafeCell::new(CpuSchedulerState {
        scheduler: nova_core::scheduler::Scheduler::new(DEFAULT_TIMESLICE),
        current_slot: None,
        switch_count: 0,
        tick_count: 0,
    }))
}; MAX_CPUS];

/// The scheduler state of one CPU.
/// # Safety
///
/// Must only be accessed by that CPU with interrupts masked.
unsafe fn state_for_cpu(cpu: usize) -> &'static mut CpuSchedulerState {
    assert!(
        cpu < MAX_CPUS,
        "CPU index exceeds the static scheduler table"
    );
    // SAFETY: delegated to this function's contract. The cell is owned by this
    // CPU and no other CPU writes it.
    unsafe { &mut *SCHEDULERS[cpu].0.get() }
}

/// Bring the bootstrap processor's scheduler up.
///
/// # Safety
///
/// Must be called once, with interrupts masked, after the thread pool exists.
pub unsafe fn init() {
    unsafe { init_for_cpu(0) }
}

/// Bring a specific CPU's scheduler up.
///
/// # Safety
///
/// Must be called on that CPU, with interrupts masked, after its thread pool
/// exists.
pub unsafe fn init_for_cpu(cpu: usize) {
    // SAFETY: delegated to this function's contract. The scheduler state is
    // written only here, before any reader exists.
    unsafe {
        *state_for_cpu(cpu) = CpuSchedulerState {
            scheduler: nova_core::scheduler::Scheduler::new(DEFAULT_TIMESLICE),
            current_slot: None,
            switch_count: 0,
            tick_count: 0,
        };
    }

    // Only the bootstrap processor's scheduler line is boot information. An AP
    // would repeat the same fact and interleave with the boot log while the
    // bootstrap processor is still bringing devices up.
    if cpu == 0 {
        klog!(
            Level::Info,
            "sched: cpu {cpu} round-robin, timeslice {DEFAULT_TIMESLICE} ticks"
        );
    }
}

/// How many times the bootstrap processor's scheduler has switched threads.
pub fn switch_count() -> u64 {
    switch_count_for_cpu(crate::task::current_cpu())
}

/// How many times a specific CPU's scheduler has switched threads.
pub fn switch_count_for_cpu(cpu: usize) -> u64 {
    // SAFETY: reading the counter is safe with interrupts masked.
    unsafe { state_for_cpu(cpu).switch_count }
}

/// How many timer ticks the bootstrap processor has seen.
pub fn tick_count() -> u64 {
    tick_count_for_cpu(crate::task::current_cpu())
}

/// How many timer ticks a specific CPU has seen.
pub fn tick_count_for_cpu(cpu: usize) -> u64 {
    // SAFETY: reading the counter is safe with interrupts masked.
    unsafe { state_for_cpu(cpu).tick_count }
}

/// The pool slot of the running thread on the bootstrap processor.
///
/// `None` when the boot thread is running — it is not in the pool.
pub fn current_slot() -> Option<usize> {
    current_slot_for_cpu(crate::task::current_cpu())
}

/// The pool slot of the running thread on a specific CPU.
pub fn current_slot_for_cpu(cpu: usize) -> Option<usize> {
    // SAFETY: reading the scheduler's state is safe with interrupts masked.
    unsafe { state_for_cpu(cpu).current_slot }
}

/// The identifier of the running thread on the bootstrap processor, if any.
pub fn current_thread_id() -> Option<u32> {
    current_thread_id_for_cpu(crate::task::current_cpu())
}

/// The identifier of the running thread on a specific CPU, if any.
pub fn current_thread_id_for_cpu(cpu: usize) -> Option<u32> {
    // SAFETY: reading the scheduler and thread pool is safe with interrupts masked.
    unsafe {
        current_slot_for_cpu(cpu)
            .and_then(|slot| task::thread_in_slot_for_cpu(cpu, slot))
            .map(|thread| thread.id)
    }
}

/// The state of the running thread on the bootstrap processor.
pub fn current_state() -> Option<ThreadState> {
    current_state_for_cpu(crate::task::current_cpu())
}

/// The state of the running thread on a specific CPU.
pub fn current_state_for_cpu(cpu: usize) -> Option<ThreadState> {
    // SAFETY: reading the scheduler and thread pool is safe with interrupts masked.
    unsafe {
        current_slot_for_cpu(cpu)
            .and_then(|slot| task::thread_in_slot_for_cpu(cpu, slot))
            .map(|thread| thread.state)
    }
}

/// Whether the bootstrap processor's scheduler has a runnable thread.
pub fn has_runnable_thread() -> bool {
    has_runnable_thread_for_cpu(crate::task::current_cpu())
}

/// Whether a specific CPU's scheduler has a runnable thread.
pub fn has_runnable_thread_for_cpu(cpu: usize) -> bool {
    // SAFETY: the scheduler is only read with interrupts masked, on that CPU.
    unsafe { state_for_cpu(cpu).scheduler.next_runnable().is_some() }
}

/// The number of threads the bootstrap processor's scheduler tracks.
pub fn thread_count() -> usize {
    thread_count_for_cpu(crate::task::current_cpu())
}

/// The number of threads a specific CPU's scheduler tracks.
pub fn thread_count_for_cpu(cpu: usize) -> usize {
    // SAFETY: reading the scheduler is safe with interrupts masked.
    unsafe { state_for_cpu(cpu).scheduler.thread_count() }
}

/// The bootstrap processor's runnable threads, in queue order.
pub fn runnable_order() -> impl Iterator<Item = u32> + 'static {
    runnable_order_for_cpu(crate::task::current_cpu())
}

/// A specific CPU's runnable threads, in queue order.
pub fn runnable_order_for_cpu(cpu: usize) -> impl Iterator<Item = u32> + 'static {
    // SAFETY: the scheduler state is only mutated with interrupts masked, and
    // the iterator borrows it immutably for the duration of the call.
    let scheduler = unsafe { &state_for_cpu(cpu).scheduler };
    scheduler.runnable_order()
}

/// The bootstrap processor's policy state machine, for the tests.
pub fn scheduler() -> &'static mut nova_core::scheduler::Scheduler {
    scheduler_for_cpu(crate::task::current_cpu())
}

/// A specific CPU's policy state machine.
pub fn scheduler_for_cpu(cpu: usize) -> &'static mut nova_core::scheduler::Scheduler {
    // SAFETY: the scheduler is only mutated with interrupts masked by its CPU.
    unsafe { &mut state_for_cpu(cpu).scheduler }
}

/// Mark the running thread as exited.
///
/// # Safety
///
/// Must be called with interrupts masked, from the scheduler's own context.
pub unsafe fn exit_current_thread() {
    unsafe { exit_current_thread_for_cpu(crate::task::current_cpu()) }
}

/// Mark the running thread on a specific CPU as exited.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, from the scheduler's own
/// context.
pub unsafe fn exit_current_thread_for_cpu(cpu: usize) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let Some(slot) = state_for_cpu(cpu).current_slot else {
            return;
        };
        let Some(thread) = task::thread_in_slot_for_cpu(cpu, slot) else {
            return;
        };

        let mut thread = thread;
        thread.state = ThreadState::Exited;
        thread.exit_status = task::take_exit_status();
        task::update_thread_for_cpu(cpu, slot, thread);

        // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
        let _ = state_for_cpu(cpu).scheduler.exit(thread.id);
    }
}

/// Kill the current user thread after an unhandled ring-3 fault.
///
/// # Safety
///
/// Must only be called from the exception dispatch path, with interrupts
/// masked, while the faulting user thread is still the scheduler's current
/// thread. The faulting CPU is the one whose scheduler state is updated: a
/// user thread is owned by the CPU that resumed it, which is not necessarily
/// the bootstrap processor.
pub unsafe fn fault_current_user_and_switch(fault_address: u64) -> ! {
    let cpu = crate::task::current_cpu();
    unsafe { fault_current_user_and_switch_for_cpu(cpu, fault_address) }
}

/// Kill the current user thread on one CPU after an unhandled ring-3 fault.
///
/// # Safety
///
/// Must only be called on that CPU from the exception dispatch path, with
/// interrupts masked, while the faulting user thread is still the scheduler's
/// current thread.
pub unsafe fn fault_current_user_and_switch_for_cpu(cpu: usize, fault_address: u64) -> ! {
    // SAFETY: exception entry masks interrupts and this is the scheduler path.
    unsafe {
        let Some(slot) = state_for_cpu(cpu).current_slot else {
            klog!(Level::Error, "sched: user fault without current thread");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        let Some(mut thread) = task::thread_in_slot_for_cpu(cpu, slot) else {
            klog!(Level::Error, "sched: user fault has no thread record");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        thread.fault_address = fault_address;
        task::update_thread_for_cpu(cpu, slot, thread);
        klog!(
            Level::Info,
            "dbg: kill cpu={cpu} slot={slot} cr3={:#x} space={:#x} | faultcpu={} fa={:#x}",
            crate::arch::x86_64::paging::current_page_directory(),
            thread.space.pml4_phys(),
            crate::task::current_cpu(),
            fault_address,
        );

        let process = thread.process;
        exit_current_thread_for_cpu(cpu);
        crate::services::process::retire_task(process);
        switch_to_next_for_cpu(cpu)
    }
}

/// Reap exited threads on the bootstrap processor.
///
/// # Safety
///
/// Must only be called from the scheduler path, with interrupts masked.
pub unsafe fn reap_exited_threads() {
    unsafe { reap_exited_threads_for_cpu(crate::task::current_cpu()) }
}

/// Reap exited threads on a specific CPU.
///
/// The exiting thread is still running on its stack until the context switch
/// returns to another thread, so that thread is deliberately skipped here. It
/// is reaped on the following switch, once a different thread owns the CPU.
///
/// # Safety
///
/// Must only be called from that CPU's scheduler path, with interrupts masked.
pub unsafe fn reap_exited_threads_for_cpu(cpu: usize) {
    // SAFETY: scheduler and thread-pool access are serialised by this contract.
    unsafe {
        let current_slot = state_for_cpu(cpu).current_slot;
        for slot in 0..task::MAX_THREADS {
            if Some(slot) == current_slot {
                continue;
            }

            let Some(thread) = task::thread_in_slot_for_cpu(cpu, slot) else {
                continue;
            };
            if thread.state != ThreadState::Exited {
                continue;
            }

            let thread_id = thread.id;
            task::reap_thread_for_cpu(cpu, slot);
            let _ = state_for_cpu(cpu).scheduler.reap(thread_id);
        }
    }
}

/// Account one timer tick and preempt if the timeslice expired.
///
/// Returns `true` when the running thread should be preempted.
///
/// # Safety
///
/// Must be called from the timer interrupt handler, with interrupts masked.
pub unsafe fn tick() -> bool {
    unsafe { tick_for_cpu(crate::task::current_cpu()) }
}

/// Account one timer tick on a specific CPU and preempt if the timeslice
/// expired.
///
/// Returns `true` when the running thread should be preempted.
///
/// # Safety
///
/// Must be called from that CPU's timer interrupt handler, with interrupts
/// masked.
pub unsafe fn tick_for_cpu(cpu: usize) -> bool {
    // SAFETY: delegated to this function's contract.
    unsafe {
        state_for_cpu(cpu).tick_count += 1;
        if cpu > 0 {
            crate::boot::smp::record_scheduler_tick(cpu);
        }

        // A syscall may have asked for a state change that the scheduler has
        // to honour before the timeslice is even consulted. An exit is the
        // obvious case: the thread is gone, so asking whether its timeslice
        // expired is meaningless.
        match task::take_syscall_effect() {
            task::ThreadEffect::Exit => {
                exit_current_thread_for_cpu(cpu);
                return true;
            }
            task::ThreadEffect::Yield => {
                // The thread gave up the rest of its quantum. It goes back to
                // the queue and the next thread runs.
                // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
                let _ = state_for_cpu(cpu).scheduler.yield_current();
                return true;
            }
            task::ThreadEffect::Block => {
                // A blocked thread cannot run. It leaves the queue.
                // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
                if let Some(id) = state_for_cpu(cpu).scheduler.current() {
                    let _ = state_for_cpu(cpu).scheduler.block(id);
                }
                return true;
            }
            task::ThreadEffect::Continue => {}
        }

        // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
        state_for_cpu(cpu).scheduler.tick()
    }
}

/// Save the running thread's complete register file and interrupt frame.
///
/// # Safety
///
/// Must be called from an interrupt or syscall return path, with interrupts
/// masked, and with the CPU's current slot naming the thread being interrupted.
/// The register pointer must address fifteen valid words and the frame pointer
/// must address the CPU's five-word interrupt frame.
pub unsafe fn save_running_context(registers: &UserRegisters, frame: &IretFrame) {
    // The interrupted thread belongs to the CPU that took the interrupt.
    unsafe { save_running_context_for_cpu(crate::task::current_cpu(), registers, frame) }
}

/// Save a specific CPU's running thread.
///
/// # Safety
///
/// Must be called on that CPU from an interrupt or syscall return path, with
/// interrupts masked, and with that CPU's current slot naming the thread being
/// interrupted.
pub unsafe fn save_running_context_for_cpu(
    cpu: usize,
    registers: &UserRegisters,
    frame: &IretFrame,
) {
    let context = Context {
        rax: registers.rax,
        rbx: registers.rbx,
        rcx: registers.rcx,
        rdx: registers.rdx,
        rsi: registers.rsi,
        rdi: registers.rdi,
        rbp: registers.rbp,
        r8: registers.r8,
        r9: registers.r9,
        r10: registers.r10,
        r11: registers.r11,
        r12: registers.r12,
        r13: registers.r13,
        r14: registers.r14,
        r15: registers.r15,
        rip: frame.rip,
        cs: frame.cs,
        rflags: frame.rflags,
        rsp: frame.rsp,
        ss: frame.ss,
    };

    if let Some(slot) = unsafe { state_for_cpu(cpu).current_slot }
        && let Some(mut thread) = unsafe { task::thread_in_slot_for_cpu(cpu, slot) }
    {
        thread.context = context;
        unsafe { task::update_thread_for_cpu(cpu, slot, thread) };
    }
}

/// Save the calling thread's state at a `syscall` instruction.
///
/// # Safety
///
/// Must be called only from the syscall entry path, with interrupts masked,
/// and with `CURRENT_SLOT` naming the calling thread. The caller must still be
/// on the kernel stack entered by the syscall stub.
pub unsafe fn save_current_syscall_context(registers: &UserRegisters) {
    // The syscall runs on whichever processor executed the `syscall`
    // instruction; its own per-CPU scheduler owns the interrupted thread.
    let cpu = crate::task::current_cpu();

    // SAFETY: the syscall stub wrote this CPU's per-CPU cell before switching
    // stacks, and the syscall path is serialised on this CPU so no later entry
    // can overwrite it yet.
    let user_stack = unsafe { crate::arch::x86_64::percpu::user_stack_pointer() };

    let context = Context {
        rax: registers.rax,
        rbx: registers.rbx,
        rcx: registers.rcx,
        rdx: registers.rdx,
        rsi: registers.rsi,
        rdi: registers.rdi,
        rbp: registers.rbp,
        r8: registers.r8,
        r9: registers.r9,
        r10: registers.r10,
        r11: registers.r11,
        r12: registers.r12,
        r13: registers.r13,
        r14: registers.r14,
        r15: registers.r15,
        rip: registers.user_rip(),
        cs: crate::arch::x86_64::gdt::USER_CODE_SELECTOR as u64,
        rflags: registers.user_rflags(),
        rsp: user_stack,
        ss: crate::arch::x86_64::gdt::USER_DATA_SELECTOR as u64,
    };

    if let Some(slot) = unsafe { state_for_cpu(cpu).current_slot }
        && let Some(mut thread) = unsafe { task::thread_in_slot_for_cpu(cpu, slot) }
    {
        thread.context = context;
        unsafe { task::update_thread_for_cpu(cpu, slot, thread) };
    }
}

/// Choose the next runnable thread and resume it.
///
/// # Safety
///
/// Must be called with interrupts masked, after the outgoing thread's complete
/// context has been saved.
pub unsafe fn switch_to_next() -> ! {
    // SAFETY: the caller runs on the CPU whose scheduler must pick; the
    // hard-coded zero here used to route an AP's switch onto the bootstrap
    // processor's state.
    unsafe { switch_to_next_for_cpu(crate::task::current_cpu()) }
}

/// Choose the next runnable thread on a specific CPU and resume it.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, after the outgoing
/// thread's complete context has been saved.
pub unsafe fn switch_to_next_for_cpu(cpu: usize) -> ! {
    // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
    unsafe { reap_exited_threads_for_cpu(cpu) };
    let next_id = unsafe { state_for_cpu(cpu).scheduler.schedule() };
    let Some(next_id) = next_id else {
        // No runnable thread: every user thread is blocked (on IPC, a timer,
        // or similar). As a last resort, scan every thread record for one
        // that is not exited and make it runnable — this covers the case
        // where the policy state machine and the thread pool disagree about
        // who is runnable. Failing that, halt and wait for an interrupt.
        let fallback = unsafe { state_for_cpu(cpu).scheduler.any_non_exited() };
        if let Some(id) = fallback {
            // The policy queue is empty but a live thread exists (the
            // scheduler's `current` slot was cleared by a earlier block and
            // never restored). Put it back on the runnable queue and let the
            // scheduler pick it normally.
            let _ = unsafe { state_for_cpu(cpu).scheduler.unblock(id) };
            let next_id = unsafe { state_for_cpu(cpu).scheduler.schedule() };
            if let Some(next_id) = next_id {
                unsafe { resume_thread_for_cpu(cpu, next_id) };
            } else {
                klog!(Level::Info, "sched: cpu {cpu} idle, no runnable thread");
                loop {
                    unsafe { core::arch::asm!("hlt", options(nostack)) };
                    if let Some(id) = unsafe { state_for_cpu(cpu).scheduler.next_runnable() } {
                        unsafe { resume_thread_for_cpu(cpu, id) };
                    }
                }
            }
        } else {
            klog!(Level::Info, "sched: cpu {cpu} idle, no runnable thread");
            loop {
                unsafe { core::arch::asm!("hlt", options(nostack)) };
                if let Some(id) = unsafe { state_for_cpu(cpu).scheduler.next_runnable() } {
                    unsafe { resume_thread_for_cpu(cpu, id) };
                }
            }
        }
    };

    unsafe { resume_thread_for_cpu(cpu, next_id) }
}

/// Resume a scheduler-selected thread by its pool slot.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, after the outgoing
/// thread's complete context has been saved and after the policy state machine
/// has already set `next_id` as its current thread.
unsafe fn resume_thread_for_cpu(cpu: usize, next_id: u32) -> ! {
    let Some(slot) = (unsafe { task::slot_of_thread_for_cpu(cpu, next_id) }) else {
        klog!(
            Level::Error,
            "sched: cpu {cpu} scheduler id has no thread slot"
        );
        unsafe { crate::arch::exit(crate::EXIT_PANIC) }
    };

    // SAFETY: this CPU's scheduler state and thread records are only mutated
    // with interrupts masked.
    unsafe {
        state_for_cpu(cpu).current_slot = Some(slot);
        state_for_cpu(cpu).switch_count += 1;

        let Some(mut next) = task::thread_in_slot_for_cpu(cpu, slot) else {
            klog!(Level::Error, "sched: cpu {cpu} selected slot is empty");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        next.state = ThreadState::Running;
        task::update_thread_for_cpu(cpu, slot, next);

        address_space::activate(&next.space);
        // Flush and invalidate all caches after loading a new page table so
        // every CPU sees the same page-table contents. Without this an
        // application processor can walk stale page-table entries cached from
        // a previous address space, faulting on perfectly valid user pages.
        unsafe { core::arch::asm!("wbinvd", options(nostack)) };
        crate::arch::x86_64::gdt::set_privilege_stack_for_cpu(cpu, task::kernel_stack_top(&next));
        // The syscall stub switches stacks through this CPU's per-CPU cell,
        // not through a global TSS: two processors must be able to enter
        // ring 3→0 transitions at overlapping times without sharing a stack.
        crate::arch::x86_64::percpu::set_kernel_stack(task::kernel_stack_top(&next));
        let block = task::build_resume_block(&next);
        task::resume_from_block(block)
    }
}

/// Resume the already-selected current thread from its saved context.
///
/// # Safety
///
/// Must be called with interrupts masked, after the current thread's complete
/// context has been saved.
pub unsafe fn resume_current() -> ! {
    unsafe { resume_current_for_cpu(crate::task::current_cpu()) }
}

/// Resume the already-selected current thread on a specific CPU.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, after the current
/// thread's complete context has been saved.
pub unsafe fn resume_current_for_cpu(cpu: usize) -> ! {
    let Some(slot) = (unsafe { state_for_cpu(cpu).current_slot }) else {
        klog!(
            Level::Error,
            "sched: cpu {cpu} resume without a current thread"
        );
        unsafe { crate::arch::exit(crate::EXIT_PANIC) }
    };

    // SAFETY: the current thread and its stack are still live, and the saved
    // context was written into the thread record before this call.
    unsafe {
        let Some(current) = task::thread_in_slot_for_cpu(cpu, slot) else {
            klog!(Level::Error, "sched: cpu {cpu} current slot is empty");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        address_space::activate(&current.space);
        crate::arch::x86_64::gdt::set_privilege_stack_for_cpu(
            cpu,
            task::kernel_stack_top(&current),
        );
        // As in `resume_thread_for_cpu`: the syscall stub reads this CPU's own
        // cell.
        crate::arch::x86_64::percpu::set_kernel_stack(task::kernel_stack_top(&current));
        let block = task::build_resume_block(&current);
        task::resume_from_block(block)
    }
}

/// Yield the current thread and immediately schedule another one.
///
/// # Safety
///
/// Must be called with interrupts masked, from the current thread's syscall or
/// interrupt path after its context has been saved.
pub unsafe fn yield_current_and_switch() -> ! {
    unsafe { yield_current_and_switch_for_cpu(crate::task::current_cpu()) }
}

/// Yield the current thread on a specific CPU and immediately schedule another.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, from the current thread's
/// syscall or interrupt path after its context has been saved.
pub unsafe fn yield_current_and_switch_for_cpu(cpu: usize) -> ! {
    // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
    unsafe {
        let next_id = state_for_cpu(cpu).scheduler.yield_current();

        if next_id.is_none() && state_for_cpu(cpu).scheduler.current().is_none() {
            klog!(
                Level::Error,
                "sched: cpu {cpu} yield without a current thread"
            );
            crate::arch::exit(crate::EXIT_PANIC)
        }

        // `yield_current` has already selected the next thread and made it
        // current. Rescheduling again would enqueue that new current thread
        // and select a different one, breaking the requested switch.
        match next_id {
            Some(next_id) => resume_thread_for_cpu(cpu, next_id),
            None => resume_current_for_cpu(cpu),
        }
    }
}

/// Block the current thread and immediately schedule another one.
///
/// # Safety
///
/// Must be called with interrupts masked, from the current thread's syscall or
/// interrupt path after its context has been saved.
pub unsafe fn block_current_and_switch() -> ! {
    unsafe { block_current_and_switch_for_cpu(crate::task::current_cpu()) }
}

/// Block the current thread on a specific CPU and immediately schedule another.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, from the current thread's
/// syscall or interrupt path after its context has been saved.
pub unsafe fn block_current_and_switch_for_cpu(cpu: usize) -> ! {
    // SAFETY: this CPU's scheduler is only mutated with interrupts masked.
    unsafe {
        let Some(current) = state_for_cpu(cpu).scheduler.current() else {
            klog!(
                Level::Error,
                "sched: cpu {cpu} block without a current thread"
            );
            crate::arch::exit(crate::EXIT_PANIC)
        };
        let _ = state_for_cpu(cpu).scheduler.block(current);
        klog!(Level::Info, "debug: cpu {cpu} block current={current}");
        switch_to_next_for_cpu(cpu)
    }
}

/// Exit the current thread and immediately schedule another one.
///
/// # Safety
///
/// Must be called with interrupts masked, from the current thread's syscall or
/// interrupt path after its context has been saved.
pub unsafe fn exit_current_and_switch() -> ! {
    unsafe { exit_current_and_switch_for_cpu(crate::task::current_cpu()) }
}

/// Exit the current thread on a specific CPU and immediately schedule another.
///
/// # Safety
///
/// Must be called on that CPU with interrupts masked, from the current thread's
/// syscall or interrupt path after its context has been saved.
pub unsafe fn exit_current_and_switch_for_cpu(cpu: usize) -> ! {
    // SAFETY: this CPU's scheduler and thread records are only mutated with
    // interrupts masked.
    unsafe {
        exit_current_thread_for_cpu(cpu);
        switch_to_next_for_cpu(cpu)
    }
}

/// Save the interrupted thread and act on one timer tick.
///
/// # Safety
///
/// Must be called from the timer interrupt handler, with interrupts masked.
/// The register pointer must address fifteen valid words and the frame pointer
/// must address the CPU's five-word interrupt frame.
pub unsafe fn handle_timer_interrupt(registers: *const UserRegisters) -> ! {
    unsafe { handle_timer_interrupt_for_cpu(0, registers) }
}

/// Save the interrupted thread on a specific CPU and act on one timer tick.
///
/// # Safety
///
/// Must be called from that CPU's timer interrupt handler, with interrupts
/// masked. The register pointer must address fifteen valid words and the frame
/// pointer must address the CPU's five-word interrupt frame.
pub unsafe fn handle_timer_interrupt_for_cpu(cpu: usize, registers: *const UserRegisters) -> ! {
    // A timer can fire while the boot thread is still running, before the
    // scheduler has switched to any pool thread. In that window there is no
    // current slot to save or resume, so the interrupt must return to the
    // interrupted boot code. It cannot resume through the frame alone: the
    // timer stub pushed `r15..rax` below that frame, and `iretq` does not pop
    // them. The saved register file has the same layout as a resume block's
    // leading words, so the trampoline restores the registers and the frame.
    if unsafe { state_for_cpu(cpu).current_slot.is_none() } {
        // SAFETY: the timer stub saves fifteen register words followed by the
        // interrupt frame; the trampoline restores the registers and pops the
        // frame from the same stack region.
        unsafe { task::resume_from_block(registers as u64) }
    }

    // The CPU frame sits above the saved register file because the stack
    // grows down and the stub pushes the register file first.
    //
    // SAFETY: the timer stub pushes exactly `size_of::<UserRegisters>()`
    // bytes below the CPU's five-word frame, so this pointer arithmetic is
    // within the same live stack frame.
    let frame =
        unsafe { &*(registers.cast::<u8>().add(size_of::<UserRegisters>()) as *const IretFrame) };

    // SAFETY: delegated to this function's contract.
    unsafe { save_running_context_for_cpu(cpu, &*registers, frame) };

    // SAFETY: this CPU's scheduler is only mutated from its timer or syscall
    // path, both of which run with interrupts masked.
    let preempt = unsafe { tick_for_cpu(cpu) };
    if preempt {
        // SAFETY: the outgoing context has already been saved.
        unsafe { switch_to_next_for_cpu(cpu) }
    } else {
        // SAFETY: the running thread's context is the one just saved; resume it
        // without disturbing the policy state machine's round-robin order.
        unsafe { resume_current_for_cpu(cpu) }
    }
}

/// Prepare a thread's first context and register it with the scheduler.
///
/// The scheduler's own record is separate from the kernel's thread pool: the
/// policy state machine knows nothing about stacks or address spaces, and the
/// pool knows nothing about queue order. They are joined by the thread
/// identifier.
///
/// # Safety
///
/// Must be called with interrupts masked, and the thread must own a stack.
pub unsafe fn spawn(thread: &mut Thread) {
    unsafe { spawn_for_cpu(thread.cpu, thread) }
}

/// Prepare a thread's first context and register it with its CPU's scheduler.
///
/// # Safety
///
/// Must be called on the thread's CPU with interrupts masked, and the thread
/// must own a stack.
pub unsafe fn spawn_for_cpu(cpu: usize, thread: &mut Thread) {
    assert_eq!(thread.cpu, cpu, "thread belongs to a different CPU");

    // SAFETY: delegated to this function's contract. The thread owns a kernel
    // stack, and the entry describes a valid destination.
    unsafe {
        task::prepare_initial_context(thread);
    }

    // `prepare_initial_context` mutates the caller's copy of the thread. Write
    // that initial context back into the pool; otherwise the scheduler can
    // select a thread whose saved state is still the pool's zeroed default.
    let thread_slot = unsafe { task::slot_of_thread_for_cpu(cpu, thread.id) };
    if let Some(thread_slot) = thread_slot {
        unsafe {
            task::update_thread_for_cpu(cpu, thread_slot, *thread);
        }
    }

    let process = thread.process;
    let thread_id = thread.id;

    // SAFETY: delegated to this function's contract. This CPU's scheduler is
    // only mutated with interrupts masked.
    unsafe {
        let _ = state_for_cpu(cpu)
            .scheduler
            .adopt_thread(thread_id, process as u32, 0);
    }
}

/// The entry a kernel thread starts with, for the tests.
pub fn kernel_entry(function: u64) -> Entry {
    Entry::Kernel(function)
}

/// The context a user thread starts with, for the tests.
pub fn user_entry(rip: u64, rsp: u64) -> Entry {
    Entry::User { rip, rsp }
}
