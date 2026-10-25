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

/// The number of times the scheduler has switched threads.
///
/// Exposed for the tests: a scheduler that claims to round-robin but never
/// switches is indistinguishable from one that works, unless the switch count
/// is observable.
static mut SWITCH_COUNT: u64 = 0;

/// The number of timer ticks the kernel has seen.
static mut TICK_COUNT: u64 = 0;

/// The five words an interrupt or syscall return path restores with `iretq`.
#[repr(C)]
struct IretFrame {
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// The scheduler's policy state machine.
///
/// The pure half — queue order, timeslice accounting, exit and reap — lives
/// in `nova_core::scheduler::Scheduler` and is host-tested there. This is the
/// instance the kernel runs.
///
/// It lives behind an [`UnsafeCell`] rather than as a bare `static mut`
/// because the scheduler is a state machine that is read and written in
/// pieces: `tick` needs to read the current thread's remaining quantum and
/// write the tick counter, `schedule` needs to mutate the queue, and the
/// tests need to *read* the queue order while no switch is in flight.
/// A `static mut` cannot lend a `&mut` to any of those without the borrow
/// checker (correctly) objecting that a shared reference to a mutable
/// static is undefined behaviour.
struct SchedulerCell(UnsafeCell<nova_core::scheduler::Scheduler>);

// SAFETY: the scheduler's state is only read or written with interrupts
// masked, which is the kernel's discipline for every access to shared state.
// A tick, a schedule, and a test's read of the queue order never overlap,
// because the scheduler runs on a single CPU and a context switch happens
// only from the scheduler's own code.
unsafe impl Sync for SchedulerCell {}

/// The scheduler's policy state machine.
///
/// The pure half — queue order, timeslice accounting, exit and reap — lives
/// in `nova_core::scheduler::Scheduler` and is host-tested there. This is the
/// instance the kernel runs.
static SCHEDULER: SchedulerCell = SchedulerCell(UnsafeCell::new(
    nova_core::scheduler::Scheduler::new(DEFAULT_TIMESLICE),
));

/// The pool slot of the thread that is running.
///
/// `None` before the scheduler has switched to anything, and `None` again
/// while the *boot* thread is running — the boot thread is not in the pool,
/// because it exists before the pool does.
static mut CURRENT_SLOT: Option<usize> = None;

/// Bring the scheduler up.
///
/// # Safety
///
/// Must be called once, with interrupts masked, after the thread pool exists.
pub unsafe fn init() {
    // SAFETY: delegated to this function's contract. The scheduler's state is
    // written only here, before any reader exists.
    unsafe {
        // SAFETY: the scheduler cell is not read anywhere else yet.
        *SCHEDULER.0.get() = nova_core::scheduler::Scheduler::new(DEFAULT_TIMESLICE);
        SWITCH_COUNT = 0;
        TICK_COUNT = 0;
        CURRENT_SLOT = None;
    }

    klog!(
        Level::Info,
        "sched: round-robin, timeslice {DEFAULT_TIMESLICE} ticks"
    );
}

/// How many times the scheduler has switched threads.
pub fn switch_count() -> u64 {
    // SAFETY: reading the counter is safe with interrupts masked.
    unsafe { SWITCH_COUNT }
}

/// How many timer ticks the kernel has seen.
pub fn tick_count() -> u64 {
    // SAFETY: reading the counter is safe with interrupts masked.
    unsafe { TICK_COUNT }
}

/// The pool slot of the running thread.
///
/// `None` when the boot thread is running — it is not in the pool.
pub fn current_slot() -> Option<usize> {
    // SAFETY: reading the scheduler's state is safe with interrupts masked.
    unsafe { CURRENT_SLOT }
}

/// The identifier of the running thread, if any.
pub fn current_thread_id() -> Option<u32> {
    // SAFETY: reading the scheduler is safe with interrupts masked.
    unsafe {
        CURRENT_SLOT
            .and_then(|slot| task::thread_in_slot(slot))
            .map(|thread| thread.id)
    }
}

/// The state of the running thread.
pub fn current_state() -> Option<ThreadState> {
    // SAFETY: reading the scheduler is safe with interrupts masked.
    unsafe {
        CURRENT_SLOT
            .and_then(|slot| task::thread_in_slot(slot))
            .map(|thread| thread.state)
    }
}

/// Whether the scheduler has a runnable thread to switch to.
pub fn has_runnable_thread() -> bool {
    // SAFETY: the scheduler is only read with interrupts masked, which is the
    // boot path's discipline.
    unsafe { (*SCHEDULER.0.get()).next_runnable().is_some() }
}

/// The number of threads the scheduler tracks.
pub fn thread_count() -> usize {
    // SAFETY: reading the scheduler is safe with interrupts masked.
    unsafe { (*SCHEDULER.0.get()).thread_count() }
}

/// The runnable threads, in queue order.
pub fn runnable_order() -> impl Iterator<Item = u32> + 'static {
    // SAFETY: the scheduler state is only mutated with interrupts masked, and
    // the iterator borrows it immutably for the duration of the call.
    let scheduler = unsafe { &*SCHEDULER.0.get() };
    scheduler.runnable_order()
}

/// The scheduler's policy state machine, for the tests.
pub fn scheduler() -> &'static mut nova_core::scheduler::Scheduler {
    // SAFETY: the scheduler is only mutated with interrupts masked, and the
    // tests call this while nothing else is running.
    unsafe { &mut *SCHEDULER.0.get() }
}

/// Mark the running thread as exited.
///
/// # Safety
///
/// Must be called with interrupts masked, from the scheduler's own context.
pub unsafe fn exit_current_thread() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let Some(slot) = CURRENT_SLOT else {
            return;
        };
        let Some(thread) = task::thread_in_slot(slot) else {
            return;
        };

        let mut thread = thread;
        thread.state = ThreadState::Exited;
        thread.exit_status = task::take_exit_status();
        task::update_thread(slot, thread);

        // SAFETY: the scheduler is only mutated with interrupts masked.
        let _ = (*SCHEDULER.0.get()).exit(thread.id);
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
    // SAFETY: delegated to this function's contract.
    unsafe {
        TICK_COUNT += 1;

        // A syscall may have asked for a state change that the scheduler has
        // to honour before the timeslice is even consulted. An exit is the
        // obvious case: the thread is gone, so asking whether its timeslice
        // expired is meaningless.
        match task::take_syscall_effect() {
            task::ThreadEffect::Exit => {
                exit_current_thread();
                return true;
            }
            task::ThreadEffect::Yield => {
                // The thread gave up the rest of its quantum. It goes back to
                // the queue and the next thread runs.
                // SAFETY: the scheduler is only mutated with interrupts masked.
                let _ = (*SCHEDULER.0.get()).yield_current();
                return true;
            }
            task::ThreadEffect::Block => {
                // A blocked thread cannot run. It leaves the queue.
                // SAFETY: the scheduler is only mutated with interrupts masked.
                if let Some(id) = (*SCHEDULER.0.get()).current() {
                    let _ = (*SCHEDULER.0.get()).block(id);
                }
                return true;
            }
            task::ThreadEffect::Continue => {}
        }

        // SAFETY: the scheduler is only mutated with interrupts masked.
        (*SCHEDULER.0.get()).tick()
    }
}

/// Save the running thread's complete register file and interrupt frame.
///
/// # Safety
///
/// Must be called from an interrupt or syscall return path, with interrupts
/// masked, and with `CURRENT_SLOT` naming the thread being interrupted. The
/// register pointer must address fifteen valid words and the frame pointer
/// must address the CPU's five-word interrupt frame.
pub unsafe fn save_running_context(registers: &UserRegisters, frame: &IretFrame) {
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

    if let Some(slot) = unsafe { CURRENT_SLOT }
        && let Some(mut thread) = unsafe { task::thread_in_slot(slot) }
    {
        thread.context = context;
        unsafe { task::update_thread(slot, thread) };
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
    // SAFETY: the syscall stub wrote this global before switching stacks, and
    // the syscall path is serialised so no later entry can overwrite it yet.
    let user_stack = unsafe { crate::syscall::NOVA_KERNEL_USER_STACK_POINTER };

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

    if let Some(slot) = unsafe { CURRENT_SLOT }
        && let Some(mut thread) = unsafe { task::thread_in_slot(slot) }
    {
        thread.context = context;
        unsafe { task::update_thread(slot, thread) };
    }
}

/// Choose the next runnable thread and resume it.
///
/// # Safety
///
/// Must be called with interrupts masked, after the outgoing thread's complete
/// context has been saved.
pub unsafe fn switch_to_next() -> ! {
    // SAFETY: the scheduler is only mutated with interrupts masked.
    let next_id = unsafe { (*SCHEDULER.0.get()).schedule() };
    let Some(next_id) = next_id else {
        klog!(Level::Error, "sched: no runnable thread");
        unsafe { crate::arch::exit(crate::EXIT_PANIC) }
    };

    unsafe { resume_thread(next_id) }
}

/// Resume a scheduler-selected thread by its pool slot.
///
/// # Safety
///
/// Must be called with interrupts masked, after the outgoing thread's complete
/// context has been saved and after the policy state machine has already set
/// `next_id` as its current thread.
unsafe fn resume_thread(next_id: u32) -> ! {
    let Some(slot) = (unsafe { task::slot_of_thread(next_id) }) else {
        klog!(Level::Error, "sched: scheduler id has no thread slot");
        unsafe { crate::arch::exit(crate::EXIT_PANIC) }
    };

    // SAFETY: the scheduler state and thread records are only mutated with
    // interrupts masked.
    unsafe {
        CURRENT_SLOT = Some(slot);
        SWITCH_COUNT += 1;

        let Some(mut next) = task::thread_in_slot(slot) else {
            klog!(Level::Error, "sched: selected slot is empty");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        next.state = ThreadState::Running;
        task::update_thread(slot, next);

        address_space::activate(&next.space);
        crate::arch::x86_64::gdt::set_privilege_stack(task::kernel_stack_top(&next));
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
    let Some(slot) = (unsafe { CURRENT_SLOT }) else {
        klog!(Level::Error, "sched: resume without a current thread");
        unsafe { crate::arch::exit(crate::EXIT_PANIC) }
    };

    // SAFETY: the current thread and its stack are still live, and the saved
    // context was written into the thread record before this call.
    unsafe {
        let Some(current) = task::thread_in_slot(slot) else {
            klog!(Level::Error, "sched: current slot is empty");
            crate::arch::exit(crate::EXIT_PANIC)
        };

        address_space::activate(&current.space);
        crate::arch::x86_64::gdt::set_privilege_stack(task::kernel_stack_top(&current));
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
    // SAFETY: the scheduler is only mutated with interrupts masked.
    unsafe {
        let next_id = (*SCHEDULER.0.get()).yield_current();
        klog!(Level::Info, "debug: yield next={next_id:?}");

        if next_id.is_none() && (*SCHEDULER.0.get()).current().is_none() {
            klog!(Level::Error, "sched: yield without a current thread");
            crate::arch::exit(crate::EXIT_PANIC)
        }

        // `yield_current` has already selected the next thread and made it
        // current. Rescheduling again would enqueue that new current thread
        // and select a different one, breaking the requested switch.
        match next_id {
            Some(next_id) => resume_thread(next_id),
            None => resume_current(),
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
    // SAFETY: the scheduler is only mutated with interrupts masked.
    unsafe {
        let Some(current) = (*SCHEDULER.0.get()).current() else {
            klog!(Level::Error, "sched: block without a current thread");
            crate::arch::exit(crate::EXIT_PANIC)
        };
        let _ = (*SCHEDULER.0.get()).block(current);
        klog!(Level::Info, "debug: block current={current}");
        switch_to_next()
    }
}

/// Exit the current thread and immediately schedule another one.
///
/// # Safety
///
/// Must be called with interrupts masked, from the current thread's syscall or
/// interrupt path after its context has been saved.
pub unsafe fn exit_current_and_switch() -> ! {
    // SAFETY: the scheduler and thread records are only mutated with
    // interrupts masked.
    unsafe {
        exit_current_thread();
        switch_to_next()
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
    // The CPU frame sits above the saved register file because the stack
    // grows down and the stub pushes the register file first.
    //
    // SAFETY: the timer stub pushes exactly `size_of::<UserRegisters>()`
    // bytes below the CPU's five-word frame, so this pointer arithmetic is
    // within the same live stack frame.
    let frame =
        unsafe { &*(registers.cast::<u8>().add(size_of::<UserRegisters>()) as *const IretFrame) };

    // SAFETY: delegated to this function's contract.
    unsafe { save_running_context(&*registers, frame) };

    // SAFETY: the scheduler is only mutated from the timer or syscall path,
    // both of which run with interrupts masked.
    let preempt = unsafe { tick() };
    klog!(Level::Info, "debug: timer preempt={preempt}");
    if preempt {
        // SAFETY: the outgoing context has already been saved.
        unsafe { switch_to_next() }
    } else {
        // SAFETY: the running thread's context is the one just saved; resume it
        // without disturbing the policy state machine's round-robin order.
        unsafe { resume_current() }
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
    // SAFETY: delegated to this function's contract. The thread owns a kernel
    // stack, and the entry describes a valid destination.
    unsafe {
        task::prepare_initial_context(thread);
    }

    // `prepare_initial_context` mutates the caller's copy of the thread. Write
    // that initial context back into the pool; otherwise the scheduler can
    // select a thread whose saved state is still the pool's zeroed default.
    let thread_slot = unsafe { task::slot_of_thread(thread.id) };
    if let Some(thread_slot) = thread_slot {
        unsafe {
            task::update_thread(thread_slot, *thread);
        }
    }

    let process = thread.process;
    let thread_id = thread.id;

    // SAFETY: delegated to this function's contract. The scheduler is only
    // mutated with interrupts masked.
    unsafe {
        let _ = (*SCHEDULER.0.get()).adopt_thread(thread_id, process as u32, 0);
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
