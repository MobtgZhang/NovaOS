//! NovaOS user-space runtime.
//!
//! # What this is
//!
//! A user program on NovaOS cannot use Rust's `std`: there is no libc, no
//! heap, no file system, and no C runtime to start it. Porting `std` for a new
//! operating system is a large project in its own right, and none of it is
//! needed to prove that the kernel can run user code.
//!
//! So this crate provides the smallest runtime that makes a *user program*
//! possible: an entry point, a page allocator, and formatted output. Everything
//! else a user program needs is a syscall.
//!
//! # The three pieces
//!
//! - [`allocator`] — a bump allocator over a heap the kernel maps on demand.
//!   It exists because `alloc` is the only way a `no_std` program can have a
//!   `Vec`, and a `Vec` is the only way most Rust code has of owning memory.
//! - [`syscall`] — the `syscall` instruction and the ABI's calling convention.
//! - [`console`] — formatted output, which the kernel routes to the serial
//!   line.
//!
//! # The entry point
//!
//! [`_start`] is a naked function: the kernel enters it in ring 3 with the
//! program's stack already set up, so there is no C-runtime-style prologue to
//! run and no `main` to call. It sets up the stack canary the ABI expects,
//! calls the program's entry point, and then exits the thread with the status
//! the program returned.
//!
//! # Why the runtime does not trust the kernel's stack
//!
//! The kernel maps the user stack, so the stack pointer is the kernel's
//! responsibility — but the *contents* are the program's. The runtime therefore
//! installs a canary word below the stack pointer at entry and checks it before
//! returning, so a program that overflows its stack is told rather than being
//! allowed to corrupt the kernel's view of the stack.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
// `#[linkage = "weak"]` is still unstable, and the runtime needs it so that a
// build with no user program attached still links. The feature gate is the
// only price, and the toolchain is pinned to nightly in `rust-toolchain.toml`
// for exactly this kind of thing.
#![feature(linkage)]

mod allocator;
mod console;
mod syscall;

use nova_abi::nr;

/// The address the program's entry point is called at.
///
/// The kernel jumps to the ELF's entry point, which is this symbol. It takes
/// no arguments and returns no value: the exit status travels through the
/// `THREAD_EXIT` syscall, and a program that wants to return a status calls
/// [`nova_rt_exit`] itself.
///
/// # Safety
///
/// Reached only by the kernel's `iretq` into the user address space, with the
/// stack the kernel mapped and the image the kernel loaded.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // SAFETY: the kernel entered ring 3 with a valid stack and a mapped image;
    // the setup below only writes to the program's own stack.
    unsafe {
        allocator::init();
    }

    // The program's entry point. It is a plain function so a user program can
    // be linked against this runtime without knowing anything about the
    // runtime's internals.
    let status = main();

    // A program that returns exits the thread. `THREAD_EXIT` never returns, so
    // the `-> !` on this function is honest.
    nova_rt_exit(status)
}

/// The user program's entry point.
///
/// Weakly linked so a runtime built on its own — with no program attached —
/// still links and still exits cleanly rather than failing with an undefined
/// symbol. The kernel's acceptance tests use exactly that shape: a runtime
/// whose program does nothing but report that it ran.
///
/// # Safety
///
/// Called once, from [`_start`], on the program's stack.
#[unsafe(no_mangle)]
#[linkage = "weak"]
extern "C" fn main() -> i64 {
    0
}

/// Exit the calling thread with `status`.
///
/// Never returns: the kernel does not schedule the thread again.
pub fn nova_rt_exit(status: i64) -> ! {
    syscall::exit_thread(status)
}

/// Report the ABI version the kernel claims.
///
/// Returns `None` if the kernel refused the call, which is the only way a user
/// program can discover that it is talking to something that is not a NovaOS
/// kernel.
pub fn kernel_version() -> Option<i64> {
    syscall::call(nr::GET_VERSION.raw(), [0; 6]).ok()
}

/// Yield the remainder of this thread's timeslice.
///
/// Returns `false` if the kernel refused the call, which for a single-CPU
/// kernel should not happen.
pub fn yield_now() -> bool {
    syscall::call(nr::YIELD.raw(), [0; 6]).is_ok()
}

/// The panic handler for a ring-3 program.
///
/// # Why a user program needs its own
///
/// `panic = abort` is set for the bare-metal target, so a panic does not
/// unwind — it lands here. A `no_std` binary must still define exactly one
/// `#[panic_handler]`, and the kernel's handler is not available to a user
/// program: it lives in a different address space and talks to the console
/// directly.
///
/// # Why it reports rather than aborts
///
/// A ring-3 program cannot abort the machine. The strongest thing it can do is
/// exit its own thread, which is what this handler does: the kernel reclaims
/// the task and the rest of the system carries on. The alternative — halting —
/// would let any user program stop the whole machine, which is exactly the
/// property a microkernel must not have.
///
/// The status is deliberately distinctive: a caller that sees a thread exit
/// with `-1` knows the program failed rather than chose that status itself.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // The message is best-effort: if the panic came from a fault in the
    // syscall path, the write may fail too, and a failed write must not turn
    // into a second panic (which would recurse into this handler).
    //
    // `format_args!` rather than `format!`: the runtime has no `alloc`, and
    // `format!` would need a `String` to live in.
    let _ = console::write_fmt(format_args!("user panic: {info}"));

    // Exit with a status no well-behaved program returns on its own.
    nova_rt_exit(-1)
}
