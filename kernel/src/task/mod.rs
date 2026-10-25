//! Threads, their saved machine state, and the ring 3 entry path.
//!
//! # One context, two privilege levels
//!
//! Every thread in NovaOS — kernel or user — is resumed the same way: the
//! scheduler restores a saved register file and jumps to a saved instruction
//! pointer. There is exactly one context structure, and it holds the fifteen
//! general-purpose registers plus the five words of a return frame.
//!
//! The alternative is a separate "kernel thread" concept and a separate
//! "user thread" concept, each with its own resume mechanism. That doubles
//! the scheduler's branching and — worse — makes the *preemption* path
//! ambiguous: a kernel thread that is preempted and a user thread that is
//! preempted would be saved by different code, and the scheduler would have
//! to know which kind it was switching to.
//!
//! With one structure, the scheduler never asks. It restores a context, and
//! the *content* of the context decides whether control ends up in ring 0 or
//! ring 3 — because the code segment selector is in the context, and the
//! resume path honours it.
//!
//! # Why the *whole* register file is saved
//!
//! A context switch that saved only the callee-saved registers would be
//! correct for a kernel thread and wrong for a user thread.
//!
//! The reason is that the callee-saved convention is a *contract between
//! two pieces of code that both follow the ABI*. The kernel's own code
//! honours it, so a switch that preserves only those registers preserves
//! everything the interrupted kernel code can still rely on. User code
//! honours no such convention: it may hold a live value in `rax` across
//! any instruction, and the CPU's interrupt mechanism saves none of the
//! general-purpose registers at all.
//!
//! So a preempted user thread would resume with a corrupted register file if
//! the switch preserved only the callee-saved set. The timer stub therefore
//! saves all fifteen, and the context holds all fifteen.
//!
//! # The resume block
//!
//! Restoring a thread is done by writing a *block* onto the target thread's
//! kernel stack and then executing `iretq` with the stack pointer pointing
//! into it. The block is laid out as:
//!
//! ```text
//!     [base +   0]  r15   ┐
//!     [base +   8]  r14   │ the fifteen general-purpose registers,
//!     ...                 │ `r15` at the lowest address
//!     [base + 120]  rax   ┘
//!     [base + 120]  rip   ┐
//!     [base + 128]  cs    │ the return frame, in the order `iretq` pops it
//!     [base + 136]  rflags│
//!     [base + 144]  rsp   │
//!     [base + 152]  ss    ┘
//! ```
//!
//! The block's *base* differs between the two kinds of thread, and the
//! difference is the only thing that distinguishes them:
//!
//! - A **user** thread's block sits at the top of its kernel stack, and the
//!   frame's `rsp` slot holds the *user's* stack pointer. `iretq` pops five
//!   words (the privilege level changes) and takes the stack pointer from the
//!   frame, so the thread resumes on the user's own stack in ring 3.
//! - A **kernel** thread's block sits immediately below the stack pointer it
//!   is resuming with, and the frame's `rsp` slot is unused. `iretq` pops
//!   three words (the privilege level does not change), so the stack pointer
//!   after the pop is the one the block was placed under.
//!
//! One trampoline serves both, and the difference is entirely in where the
//! block is written.

use core::cell::UnsafeCell;

use crate::arch::x86_64::gdt::{
    KERNEL_CODE_SELECTOR, KERNEL_DATA_SELECTOR, USER_CODE_SELECTOR, USER_DATA_SELECTOR,
};
use crate::klog;
use crate::logger::Level;
use crate::mm::address_space::AddressSpace;

/// The size of a thread's kernel stack.
///
/// 16 KiB is enough for the syscall path, the fault path, and the scheduler's
/// own frames, and small enough that a pool of eight fits comfortably in
/// `.bss` — which matters, because the kernel stack this sits on is only
/// 64 KiB in total.
pub const KERNEL_STACK_SIZE: usize = 16 * 1024;

/// The maximum number of threads the kernel can hold at once.
///
/// Bounded because the pool is static. A thread that cannot be created fails
/// loudly rather than silently overwriting a live thread's stack.
pub const MAX_THREADS: usize = 8;

/// The number of general-purpose registers in a resume block.
pub const GPR_COUNT: usize = 15;

/// The size of a resume block, in bytes.
pub const BLOCK_BYTES: usize = (GPR_COUNT + 5) * 8;

/// The `RFLAGS` a thread starts with.
///
/// Bit 1 is architecturally reserved and always reads as one; bit 9 is the
/// interrupt-enable flag.
///
/// Setting `IF` matters: a thread that started with interrupts disabled
/// could never be preempted, and the round-robin test would hang rather than
/// fail.
pub const USER_RFLAGS: u64 = 0x202;

/// What a thread is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    /// Ready to run, waiting its turn.
    Runnable,
    /// Currently running on the CPU.
    Running,
    /// Waiting for something — a message, a reply, a timer.
    Blocked,
    /// Finished; awaiting reaping.
    Exited,
}

impl ThreadState {
    /// A stable tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            ThreadState::Runnable => "runnable",
            ThreadState::Running => "running",
            ThreadState::Blocked => "blocked",
            ThreadState::Exited => "exited",
        }
    }
}

/// A thread's saved machine state.
///
/// The field order is not ABI — the resume block is written field by field,
/// so this structure is free to be readable. What *is* fixed is the set of
/// fields: all fifteen general-purpose registers, because a user thread's
/// register file is not covered by any convention.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context {
    /// The saved `rax`.
    pub rax: u64,
    /// The saved `rbx`.
    pub rbx: u64,
    /// The saved `rcx`.
    pub rcx: u64,
    /// The saved `rdx`.
    pub rdx: u64,
    /// The saved `rsi`.
    pub rsi: u64,
    /// The saved `rdi`.
    pub rdi: u64,
    /// The saved `rbp`.
    pub rbp: u64,
    /// The saved `r8`.
    pub r8: u64,
    /// The saved `r9`.
    pub r9: u64,
    /// The saved `r10`.
    pub r10: u64,
    /// The saved `r11`.
    pub r11: u64,
    /// The saved `r12`.
    pub r12: u64,
    /// The saved `r13`.
    pub r13: u64,
    /// The saved `r14`.
    pub r14: u64,
    /// The saved `r15`.
    pub r15: u64,
    /// The instruction pointer to resume at.
    pub rip: u64,
    /// The code segment selector to resume with.
    pub cs: u64,
    /// The flags word to resume with.
    pub rflags: u64,
    /// The stack pointer to resume with.
    pub rsp: u64,
    /// The data segment selector to resume with.
    pub ss: u64,
}

impl Context {
    /// The context of a thread that has never run, starting in ring 3.
    ///
    /// The instruction pointer is the user program's entry point and the
    /// stack pointer is the top of the user stack. The flags word has `IF`
    /// set, because a thread that cannot be interrupted can never be
    /// preempted.
    pub const fn user_start(rip: u64, rsp: u64) -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip,
            cs: USER_CODE_SELECTOR as u64,
            rflags: USER_RFLAGS,
            rsp,
            ss: USER_DATA_SELECTOR as u64,
        }
    }

    /// The context of a thread that has never run, starting in ring 0.
    pub const fn kernel_start(rip: u64, rsp: u64) -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip,
            cs: KERNEL_CODE_SELECTOR as u64,
            rflags: USER_RFLAGS,
            rsp,
            ss: KERNEL_DATA_SELECTOR as u64,
        }
    }

    /// Whether this context resumes in ring 3.
    pub const fn is_user(&self) -> bool {
        self.cs & 0x3 == 3
    }

    /// Write this context into a resume block.
    ///
    /// The block's layout is fixed by the resume trampoline: the fifteen
    /// registers first, `r15` at the lowest address, then the five frame
    /// words in the order `iretq` pops them.
    ///
    /// # Safety
    ///
    /// `block` must point at [`BLOCK_BYTES`] writable bytes.
    pub unsafe fn write_block(&self, block: *mut u64) {
        // SAFETY: delegated to this function's contract. The block is a
        // 160-byte region on the target thread's kernel stack, and every
        // write below stays inside it.
        unsafe {
            // The registers, in the order the trampoline pops them: `r15`
            // first (lowest address) and `rax` last.
            block.add(0).write(self.r15);
            block.add(1).write(self.r14);
            block.add(2).write(self.r13);
            block.add(3).write(self.r12);
            block.add(4).write(self.r11);
            block.add(5).write(self.r10);
            block.add(6).write(self.r9);
            block.add(7).write(self.r8);
            block.add(8).write(self.rbp);
            block.add(9).write(self.rsi);
            block.add(10).write(self.rdi);
            block.add(11).write(self.rdx);
            block.add(12).write(self.rcx);
            block.add(13).write(self.rbx);
            block.add(14).write(self.rax);

            // The return frame, in the order `iretq` pops it.
            block.add(15).write(self.rip);
            block.add(16).write(self.cs);
            block.add(17).write(self.rflags);
            block.add(18).write(self.rsp);
            block.add(19).write(self.ss);
        }
    }
}

/// The kind of entry a thread starts with.
///
/// A thread's *first* run differs from every later one: there is no saved
/// register file to restore, because there was no previous run. What there is
/// instead is either a kernel function to call or a ring 3 state to enter.
#[derive(Clone, Copy, Debug)]
pub enum Entry {
    /// Start by calling this kernel function.
    ///
    /// The function is called with no arguments and must not return; a thread
    /// that returns is treated as an exit.
    Kernel(u64),
    /// Start by entering ring 3 at this instruction point with this stack
    /// pointer.
    User {
        /// The user program's entry point.
        rip: u64,
        /// The initial user stack pointer.
        rsp: u64,
    },
}

/// A thread's kernel stack.
///
/// A fixed-size, 16-byte-aligned buffer. The `Sync` implementation documents
/// the single-CPU discipline: the CPU writes the buffer through the stack
/// pointer it was given, and only one thread is running at a time.
#[repr(align(16))]
struct StackBuffer(UnsafeCell<[u8; KERNEL_STACK_SIZE]>);

// SAFETY: the buffer is written only by the CPU, through the stack pointer the
// kernel hands it, and only while that thread is the running one. Two threads
// never share a buffer: the pool hands each thread its own slot, and a slot is
// not reused until the thread occupying it has been reaped.
//
// The buffer is not read through Rust at all — the kernel only ever computes
// its top address — so there is no Rust-level aliasing to reason about.
unsafe impl Sync for StackBuffer {}

/// The pool of kernel stacks.
///
/// A static array rather than an allocator: the kernel has no general-purpose
/// allocator for objects, and a stack must be a single contiguous region that
/// never moves.
struct StackPool {
    /// The buffers.
    ///
    /// `UnsafeCell` because the CPU — not Rust — writes the bytes.
    buffers: [StackBuffer; MAX_THREADS],
    /// Which buffers are in use.
    ///
    /// A plain array rather than a bitmap: the pool is small, and the search
    /// is a short scan on a path that runs once per thread creation.
    used: [bool; MAX_THREADS],
}

impl StackPool {
    const fn new() -> Self {
        Self {
            buffers: [const { StackBuffer(UnsafeCell::new([0; KERNEL_STACK_SIZE])) }; MAX_THREADS],
            used: [false; MAX_THREADS],
        }
    }

    /// Claim a stack, returning its slot.
    ///
    /// Returns `None` when the pool is exhausted.
    fn acquire(&mut self) -> Option<usize> {
        let slot = self.used.iter().position(|taken| !taken)?;
        self.used[slot] = true;
        Some(slot)
    }

    /// Release a stack.
    fn release(&mut self, slot: usize) {
        if slot < MAX_THREADS {
            self.used[slot] = false;
        }
    }

    /// The top address of a stack.
    ///
    /// The CPU pushes downwards from here. The alignment is asserted rather
    /// than assumed: the buffer is 16-byte aligned and the size is a multiple
    /// of 16, so the sum is aligned too — but a linker or attribute change
    /// could break that, and a misaligned stack pointer faults in ring 0.
    fn top_of(&self, slot: usize) -> u64 {
        let base = self.buffers[slot].0.get() as *const u8 as u64;
        let top = base + KERNEL_STACK_SIZE as u64;
        debug_assert_eq!(top & 0xF, 0, "a kernel stack top must be 16-byte aligned");
        top
    }

    /// The lowest address of a stack.
    fn base_of(&self, slot: usize) -> u64 {
        self.buffers[slot].0.get() as *const u8 as u64
    }
}

/// The kernel's stack pool.
///
/// Behind an [`UnsafeCell`] because the pool is read and written in pieces:
/// a thread is claimed by a writer, and the *running* thread's stack
/// bounds are read by the fault path while the pool is otherwise quiescent.
/// A `static mut` cannot lend either of those without the borrow checker
/// (correctly) objecting that a shared reference to a mutable static is
/// undefined behaviour.
struct StackPoolCell(UnsafeCell<StackPool>);

// SAFETY: the pool is only accessed with interrupts masked, which is the
// kernel's discipline for every access to shared state. The CPU writes
// through a stack pointer the kernel hands it, and only while that thread
// is the running one.
unsafe impl Sync for StackPoolCell {}

/// The kernel's stack pool.
static STACKS: StackPoolCell = StackPoolCell(UnsafeCell::new(StackPool::new()));

/// A thread.
///
/// Every field is fixed-size and `Copy`, so a thread's metadata lives in a
/// static array with no allocator involved.
#[derive(Clone, Copy)]
pub struct Thread {
    /// The thread's identifier.
    pub id: u32,
    /// The slot of the process this thread belongs to.
    pub process: usize,
    /// The address space the thread runs in.
    pub space: AddressSpace,
    /// The thread's kernel stack slot in the pool.
    pub stack_slot: usize,
    /// The thread's saved machine state.
    pub context: Context,
    /// What the thread is doing.
    pub state: ThreadState,
    /// The thread's entry point, for the *first* run.
    pub entry: Entry,
    /// The thread's exit status, for a thread that exited.
    pub exit_status: i64,
    /// How many times the thread has been scheduled.
    pub runs: u64,
    /// How many times the thread was preempted by the timer.
    pub preemptions: u64,
    /// The address a user thread faulted at, if it was killed by a fault.
    pub fault_address: u64,
}

/// The kernel's thread pool.
///
/// A static array rather than an allocator: the kernel has no general-purpose
/// allocator for objects.
///
/// Behind an [`UnsafeCell`] because the pool is read and written in pieces:
/// the scheduler reads a thread's record while the fault path reads the
/// *running* thread's, and a `static mut` cannot lend a shared reference to
/// either without the borrow checker (correctly) objecting that a shared
/// reference to a mutable static is undefined behaviour.
struct ThreadPoolCell(UnsafeCell<[Option<Thread>; MAX_THREADS]>);

// SAFETY: the pool is only accessed with interrupts masked, which is the
// kernel's discipline for every access to shared state. The kernel runs on a
// single CPU, so a read of a thread's record never overlaps a write of it.
unsafe impl Sync for ThreadPoolCell {}

/// The kernel's thread pool.
///
/// A static array rather than an allocator: the kernel has no general-purpose
/// allocator for objects.
static THREADS: ThreadPoolCell = ThreadPoolCell(UnsafeCell::new([None; MAX_THREADS]));

/// What the current syscall did to the calling thread.
///
/// The dispatcher reports this so the *scheduler* can act on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadEffect {
    /// The thread continues.
    Continue,
    /// The thread yielded; schedule another one.
    Yield,
    /// The thread exited; it must be reaped.
    Exit,
    /// The thread blocked; wake it when the awaited event happens.
    Block,
}

/// The thread pool, as a raw slice.
///
/// # Safety
///
/// Must only be accessed with interrupts masked.
unsafe fn thread_pool() -> &'static mut [Option<Thread>; MAX_THREADS] {
    // SAFETY: delegated to this function's contract. The pool is a static
    // whose only writer is the kernel, and every access is made with
    // interrupts masked.
    unsafe { &mut *THREADS.0.get() }
}

/// The effect the most recent syscall asked for.
///
/// A single slot rather than a per-thread field: only one syscall can be in
/// flight at a time, because the entry path runs with interrupts masked.
static mut PENDING_EFFECT: ThreadEffect = ThreadEffect::Continue;

/// The exit status the most recent `THREAD_EXIT` carried.
static mut PENDING_EXIT_STATUS: i64 = 0;

/// Record what a syscall did to the calling thread.
///
/// # Safety
///
/// Must only be called from the syscall entry path, with interrupts masked.
/// The value is consumed by the scheduler before the next syscall can be
/// dispatched.
pub unsafe fn note_syscall_effect(effect: ThreadEffect) {
    // SAFETY: delegated to this function's contract. The slot is written by
    // the entry path and read by the scheduler, both with interrupts masked.
    unsafe {
        PENDING_EFFECT = effect;
    }
}

/// Take the pending syscall effect, clearing it.
///
/// # Safety
///
/// Must only be called by the scheduler, with interrupts masked.
pub unsafe fn take_syscall_effect() -> ThreadEffect {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let effect = PENDING_EFFECT;
        PENDING_EFFECT = ThreadEffect::Continue;
        effect
    }
}

/// Record the exit status a `THREAD_EXIT` carried.
///
/// # Safety
///
/// Must only be called from the syscall entry path, with interrupts masked.
pub unsafe fn note_exit_status(status: i64) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        PENDING_EXIT_STATUS = status;
    }
}

/// Take the pending exit status, clearing it.
///
/// # Safety
///
/// Must only be called by the scheduler, with interrupts masked.
pub unsafe fn take_exit_status() -> i64 {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let status = PENDING_EXIT_STATUS;
        PENDING_EXIT_STATUS = 0;
        status
    }
}

/// The context a syscall is dispatched in.
///
/// The syscall path needs the caller's *identity* to reach the caller's
/// capability table. The kernel's boot task owns slot 0, and every user
/// thread the M2 loader creates belongs to that task until M3 gives each
/// process its own slot.
pub fn current_syscall_context() -> crate::services::dispatch::SyscallContext {
    // SAFETY: the syscall entry path is serialised and runs with interrupts
    // masked, so the scheduler's current thread cannot change while it reads.
    let current =
        unsafe { crate::sched::current_slot().and_then(|slot| (unsafe { thread_in_slot(slot) })) };

    // A user syscall must always have a running thread. The sentinel keeps the
    // function callable from boot-time diagnostics without inventing a task.
    match current {
        Some(thread) => crate::services::dispatch::SyscallContext::new(thread.process, thread.id),
        None => crate::services::dispatch::SyscallContext::new(0, 1),
    }
}

/// The address space of the thread that is running.
///
/// The syscall path needs it to copy bytes *out of* the caller's memory: the
/// kernel cannot dereference a user pointer directly, because the kernel's own
/// address space is not necessarily the caller's, and the page that holds the
/// user's bytes may not even be mapped in the kernel's tables.
///
/// Returns `None` when no thread is running — which happens on the boot
/// thread, before the scheduler has been given anything to switch to.
pub fn current_thread_space() -> Option<AddressSpace> {
    // SAFETY: reading the pool is safe with interrupts masked, which is the
    // kernel's discipline for every access to shared state. The thread's
    // `space` is a `Copy` handle to a table the kernel owns.
    unsafe {
        crate::sched::current_slot()
            .and_then(|slot| (unsafe { thread_pool() }).get(slot).copied().flatten())
            .map(|thread| thread.space)
    }
}

/// Create a thread with a given identifier.
///
/// # Errors
///
/// Returns `None` when the thread pool or the stack pool is exhausted.
pub fn create_thread(id: u32, process: usize, space: AddressSpace, entry: Entry) -> Option<Thread> {
    // SAFETY: `create_thread` is called from the kernel with interrupts
    // masked, and it is the only writer of `THREADS`.
    unsafe {
        let slot = (unsafe { thread_pool() })
            .iter()
            .position(|entry| entry.is_none())?;
        let stack_slot = unsafe { (*STACKS.0.get()).acquire()? };

        let thread = Thread {
            id,
            process,
            space,
            stack_slot,
            context: Context::default(),
            state: ThreadState::Runnable,
            entry,
            exit_status: 0,
            runs: 0,
            preemptions: 0,
            fault_address: 0,
        };

        (unsafe { thread_pool() })[slot] = Some(thread);
        Some(thread)
    }
}

/// Look up a thread by identifier.
///
/// # Safety
///
/// Must only be called with interrupts masked, and must not be called while
/// a `&mut` to a thread is live.
pub unsafe fn thread_by_id(id: u32) -> Option<Thread> {
    // SAFETY: delegated to this function's contract.
    unsafe {
        (unsafe { thread_pool() })
            .iter()
            .flatten()
            .find(|thread| thread.id == id)
            .copied()
    }
}

/// The thread in a pool slot.
///
/// # Safety
///
/// Must only be called with interrupts masked.
pub unsafe fn thread_in_slot(slot: usize) -> Option<Thread> {
    // SAFETY: delegated to this function's contract.
    unsafe { (thread_pool()).get(slot).copied().flatten() }
}

/// Replace a thread's record.
///
/// # Safety
///
/// Must only be called with interrupts masked, and the slot must hold a live
/// thread.
pub unsafe fn update_thread(slot: usize, thread: Thread) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        (unsafe { thread_pool() })[slot] = Some(thread);
    }
}

/// The pool slot a thread's identifier lives in.
pub fn slot_of(id: u32) -> Option<usize> {
    // SAFETY: reading the pool is safe with interrupts masked.
    unsafe {
        (unsafe { thread_pool() })
            .iter()
            .position(|entry| entry.is_some_and(|thread| thread.id == id))
    }
}

/// The number of live threads.
pub fn thread_count() -> usize {
    // SAFETY: reading the pool is safe with interrupts masked, which is the
    // kernel's discipline for every access to shared state.
    unsafe { (thread_pool()).iter().flatten().count() }
}

/// Reap a thread, releasing its stack slot.
///
/// # Safety
///
/// Must only be called with interrupts masked, and the thread must have
/// exited.
pub unsafe fn reap_thread(slot: usize) {
    // SAFETY: delegated to this function's contract. The thread is not
    // running, so its stack slot is not in use.
    unsafe {
        if let Some(thread) = (unsafe { thread_pool() })
            .get_mut(slot)
            .and_then(|entry| *entry)
            && thread.state == ThreadState::Exited
        {
            unsafe { (*STACKS.0.get()).release(thread.stack_slot) };
            (unsafe { thread_pool() })[slot] = None;
        }
    }
}

/// The kernel stack bounds of a thread, as `(low, high)`.
///
/// The bounds are used to tell "an interrupt arrived on this thread's stack"
/// from "an interrupt arrived on the scheduler's stack". The distinction
/// matters: only the former is a preemption of the current thread.
pub fn stack_bounds(thread: &Thread) -> (u64, u64) {
    // SAFETY: the pool hands each thread its own slot, and a slot is not
    // released until the thread is reaped, so the slot is still the thread's.
    let pool = unsafe { &*STACKS.0.get() };
    (
        pool.base_of(thread.stack_slot),
        pool.top_of(thread.stack_slot),
    )
}

/// Whether `address` lies within a thread's kernel stack.
pub fn stack_contains(thread: &Thread, address: u64) -> bool {
    let (low, high) = stack_bounds(thread);
    address >= low && address < high
}

/// The thread's kernel stack top address.
///
/// This is what the TSS's `RSP0` must name while the thread is running:
/// the CPU uses it to land an interrupt, and the syscall entry path uses it
/// to switch to the kernel stack.
pub fn kernel_stack_top(thread: &Thread) -> u64 {
    // SAFETY: `STACKS` is a static pool whose slots are claimed exclusively
    // by one thread each. A thread's slot is not released until the thread is
    // reaped, so the slot is still the thread's while the thread exists.
    let pool = unsafe { &*STACKS.0.get() };
    pool.top_of(thread.stack_slot)
}

/// The pool slot of the thread with `id`, if it exists.
///
/// The scheduler policy identifies threads by the pool's identifier, so the
/// mechanism needs this reverse mapping to reach the thread's saved context.
///
/// # Safety
///
/// Must only be accessed with interrupts masked.
pub unsafe fn slot_of_thread(id: u32) -> Option<usize> {
    // SAFETY: delegated to this function's contract. The single-CPU kernel
    // accesses the pool only from serialised scheduler or syscall paths.
    unsafe {
        thread_pool()
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|thread| thread.id == id))
    }
}

/// Write a thread's context into a resume block on its kernel stack, and
/// return the block's address.
///
/// # Where the block goes
///
/// The block's placement is what distinguishes a kernel thread from a user
/// thread, and it is worth spelling out why:
///
/// - A **user** thread's block sits at the top of its kernel stack. The
///   frame's `rsp` slot holds the *user's* stack pointer, and `iretq` — which
///   pops five words because the privilege level changes — takes the stack
///   pointer from the frame. The thread therefore resumes in ring 3, on the
///   user's own stack.
/// - A **kernel** thread's block sits at the top of its kernel stack, exactly
///   where the timer stub saved the register file and interrupt frame.
///
/// # Safety
///
/// The thread must own a kernel stack, and the thread must not be running.
pub unsafe fn build_resume_block(thread: &Thread) -> u64 {
    let base = kernel_stack_top(thread) - BLOCK_BYTES as u64;

    // SAFETY: delegated to this function's contract. The block is
    // `BLOCK_BYTES` long and sits inside the thread's own kernel stack: for a
    // user thread at the top of it, for a kernel thread immediately below
    // its stack pointer. Both regions are mapped and writable, and the
    // thread is not running.
    unsafe {
        thread.context.write_block(base as *mut u64);
    }

    base
}

/// Restore a thread from a resume block and continue at it.
///
/// The trampoline restores the fifteen general-purpose registers and then
/// executes `iretq`, which pops the frame. The distinction between the two
/// privilege levels is made by the frame's contents and by where the block
/// was placed — not by the code here.
///
/// # Safety
///
/// `block` must point at a resume block written by
/// [`build_resume_block`], and the thread it describes must not be running.
#[unsafe(naked)]
pub unsafe extern "C" fn resume_from_block(block: u64) -> ! {
    core::arch::naked_asm!(
        // SAFETY: by contract, `block` addresses a resume block whose fifteen
        // leading words are the general-purpose registers, `r15` at the
        // lowest address. The pops below therefore restore the registers
        // correctly, and leave the stack pointer at the block's frame.
        "mov rsp, rdi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rsi",
        "pop rdi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        // SAFETY: the stack pointer now addresses the block's return frame,
        // whose five words are the values `iretq` pops. For a user thread the
        // privilege level changes and five words are consumed, taking the
        // stack pointer from the frame; for a kernel thread three words are
        // consumed and the stack pointer is the one the block was placed
        // under.
        "iretq",
    )
}

/// Prepare a thread's *first* context.
///
/// A thread's first run differs from every later one: there is no saved
/// register file to restore, because there was no previous run. What there is
/// instead is either a kernel function to call or a ring 3 state to enter, and
/// this function builds the [`Context`] that switches into it.
///
/// # Why the two cases look different
///
/// A *kernel* thread is entered by jumping to a Rust function with the stack
/// pointer set to the thread's kernel stack. The function's `ret` has to go
/// somewhere, so the exit landing address is written just below the stack top
/// and the stack pointer starts there — which is also what gives the stack the
/// alignment the SysV ABI wants (`RSP ≡ 8 (mod 16)` at a function's first
/// instruction).
///
/// A *user* thread is entered with the user program's own entry point and
/// stack pointer; the `iretq` that restores it is what performs the
/// privilege change.
///
/// # Safety
///
/// The thread must own a kernel stack, and `entry` must describe a valid
/// destination: a kernel function that does not return, or a user entry point
/// in a mapped, executable page with a mapped stack.
pub unsafe fn prepare_initial_context(thread: &mut Thread) {
    let stack_top = kernel_stack_top(thread);

    match thread.entry {
        Entry::Kernel(entry_point) => {
            // The resume block owns the top of the kernel stack. Put the
            // function's return address just below the block: the block is
            // transient scheduling state, while the return address has to
            // survive until the function actually returns.
            let landing = thread_exit_landing();
            let stack_pointer = stack_top - (BLOCK_BYTES as u64 + 8);

            // SAFETY: the landing address is written into the thread's own
            // kernel stack, one word below the top, which is mapped and has
            // room for it.
            unsafe {
                core::arch::asm!(
                    "mov qword ptr [{stack_pointer}], {landing}",
                    stack_pointer = in(reg) stack_pointer,
                    landing = in(reg) landing,
                    options(nostack),
                );
            }

            thread.context = Context::kernel_start(entry_point, stack_pointer);
        }
        Entry::User { rip, rsp } => {
            thread.context = Context::user_start(rip, rsp);
        }
    }
}

/// The address the kernel puts in a user thread's `RIP` when it exits.
///
/// A user program that returns has no caller to return to, so the kernel
/// treats the return as a thread exit rather than as a fault.
pub fn thread_exit_landing() -> u64 {
    thread_exit_landing_stub as *const () as u64
}

/// Where a user thread lands when it returns from its entry point.
///
/// # Safety
///
/// Reached only by the `iretq` path, with the user's stack in use.
#[unsafe(naked)]
unsafe extern "C" fn thread_exit_landing_stub() -> ! {
    core::arch::naked_asm!(
        // SAFETY: the syscall to exit is issued with the ABI's convention:
        // the number in `RAX`, no arguments. The syscall entry path will
        // dispatch it and the scheduler will reap the thread.
        //
        // The `syscall` instruction clobbers `RCX` and `R11`; nothing here
        // depends on them.
        "mov rax, 13",
        "syscall",
        // SAFETY: `THREAD_EXIT` never returns to its caller — the thread is
        // reaped. The `ud2` is only there because the naked function's type
        // demands a terminator.
        "ud2",
    )
}

/// Log a summary of the thread pool, for the boot log.
pub fn describe_threads() {
    klog!(
        Level::Info,
        "task: {} threads live, {} stack slots",
        thread_count(),
        MAX_THREADS
    );
}

/// The number of stack slots still free.
pub fn free_stack_slots() -> usize {
    // SAFETY: reading the pool is safe with interrupts masked.
    unsafe { (*STACKS.0.get()).used.iter().filter(|used| !**used).count() }
}
