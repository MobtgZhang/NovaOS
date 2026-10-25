//! The `syscall` entry path and the `iretq` return.
//!
//! # What `syscall` does and does not do
//!
//! The `syscall` instruction is not a call. It does not push a return address,
//! it does not switch stacks, and it does not consult the TSS. What it does is:
//!
//! - load `CS` and `SS` from `STAR` — which is how the privilege level drops
//!   from ring 3 to ring 0;
//! - put the address of the instruction *after* the `syscall` into `RCX`;
//! - put the caller's `RFLAGS` into `R11`;
//! - and clear the `RFLAGS` bits named by `SFMASK`.
//!
//! Everything else — the kernel stack, the saved user registers, the return
//! path — is the kernel's job, and this file is that job.
//!
//! # The three-step entry
//!
//! 1. **Switch to the kernel stack.** The TSS's `RSP0` names the current
//!    thread's kernel stack. The CPU does *not* use it for `syscall` (it
//!    does, for interrupts and exceptions), so the stub does it by hand.
//! 2. **Save the user register file.** The kernel is about to run arbitrary
//!    Rust code, which clobbers every caller-saved register. The user's
//!    values have to be saved before the first call, or they are gone.
//! 3. **Dispatch.** The dispatcher reads the number out of the saved `RAX`
//!    and the arguments out of the saved `RDI`..`R9`.
//!
//! # The return path
//!
//! The return is an `iretq` frame, not a `sysretq`. `sysretq` is one
//! instruction faster and has a famously sharp edge: it checks the value in
//! `RCX` against the kernel's canonical addresses and raises `#GP` *after* the
//! stack switch is partially set up, so a mistake there loses every scrap of
//! context. `iretq` restores the stack pointer from the frame and the segment
//! selectors from the frame, so the whole return state is explicit and
//! checkable. v0.1.0 trades the instruction for the ability to reason about
//! it; see `docs/abi/address-layout.md`.
//!
//! # The frame
//!
//! The return frame is built on the *kernel* stack and is laid out exactly as
//! the CPU expects for `iretq`:
//!
//! ```text
//!     [rsp + 0x00]  RIP
//!     [rsp + 0x08]  CS
//!     [rsp + 0x10]  RFLAGS
//!     [rsp + 0x18]  RSP
//!     [rsp + 0x20]  SS
//! ```
//!
//! The `SS` word is read by the CPU because the privilege level changes here —
//! ring 3 to ring 0 and back. `RSP` is the *user's* stack pointer, not the
//! kernel's: `iretq` restores it verbatim, so the user returns to exactly the
//! stack it was on.
//!
//! # Why the user registers are saved on the kernel stack
//!
//! They have to live somewhere the kernel can write, and the kernel stack is
//! the only place that is guaranteed to be both writable and mapped. Saving
//! them on the *user* stack would mean writing to memory the user controls
//! at an address the user chose, which is precisely the primitive a ring 3
//! attacker needs.

use core::arch::asm;

use crate::arch::shared::console::Console;
use crate::arch::x86_64::gdt::{USER_CODE_SELECTOR, USER_DATA_SELECTOR};
use crate::arch::x86_64::msr;
use crate::klog;
use crate::logger::Level;
use nova_abi::ErrorCode;

/// The `RFLAGS` bits cleared on entry to the syscall handler.
///
/// `IF` is cleared so that a syscall cannot be interrupted by another syscall
/// before the register file is saved — two nested entries would each save a
/// register file, and the inner one's would overwrite the outer one's.
///
/// `TF` is cleared because a trap flag would make the *first* instruction of
/// the handler itself trap, producing an endless chain of `#DB`s.
///
/// `DF` is cleared because the SysV ABI requires it, and because a stray
/// string-direction would corrupt every string operation the kernel performs.
///
/// `AC` is cleared because the kernel does not check alignment, and leaving it
/// set would make the kernel's own misaligned accesses fault.
pub const SFMASK: u64 = (1 << 9) | (1 << 8) | (1 << 10) | (1 << 18);

/// The number of general-purpose registers the entry stub saves.
///
/// Fifteen, not sixteen: `RSP` is not saved with them, because the user's
/// stack pointer is stashed separately and the kernel's own stack pointer is
/// restored from the `iretq` frame.
pub const SAVED_REGISTER_COUNT: usize = 15;

/// The syscall handler's view of the saved user registers.
///
/// The layout matches the order the stub pushes them, so the stub and this
/// struct cannot disagree about which register is where: the stub pushes `rax`
/// first, and `rax` is the *last* field here, because the struct's field
/// order is the push order reversed.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct UserRegisters {
    /// `R15`.
    pub r15: u64,
    /// `R14`.
    pub r14: u64,
    /// `R13`.
    pub r13: u64,
    /// `R12`.
    pub r12: u64,
    /// `R11` — the saved `RFLAGS` the `syscall` instruction put here.
    pub r11: u64,
    /// `R10`.
    pub r10: u64,
    /// `R9`.
    pub r9: u64,
    /// `R8`.
    pub r8: u64,
    /// `RBP`.
    pub rbp: u64,
    /// `RDI` — the first argument.
    pub rdi: u64,
    /// `RSI` — the second argument.
    pub rsi: u64,
    /// `RDX` — the third argument.
    pub rdx: u64,
    /// `RCX` — the return address, which the `syscall` instruction put here.
    pub rcx: u64,
    /// `RBX`.
    pub rbx: u64,
    /// `RAX` — the syscall number in, the result out.
    pub rax: u64,
}

const _: () = {
    use core::mem::offset_of;

    // The stub writes the result back into `rax`, and the dispatcher's
    // argument pointer is the base of this structure. If the layout changed,
    // the write would land in the wrong register and the user would see a
    // stale result.
    assert!(offset_of!(UserRegisters, r15) == 0);
    assert!(offset_of!(UserRegisters, rax) == 112);
    assert!(size_of::<UserRegisters>() == 120);
};

impl UserRegisters {
    /// The syscall arguments, in the ABI's order.
    ///
    /// `RAX` is *not* among them: it carries the syscall number on entry and
    /// the result on exit.
    pub const fn args(&self) -> [u64; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }

    /// The user's instruction pointer — the address after the `syscall`.
    ///
    /// The `syscall` instruction leaves it in `RCX`, and the stub saves `RCX`
    /// with the rest of the register file.
    pub const fn user_rip(&self) -> u64 {
        self.rcx
    }

    /// The user's `RFLAGS`.
    ///
    /// The `syscall` instruction leaves it in `R11`.
    pub const fn user_rflags(&self) -> u64 {
        self.r11
    }
}

/// The user's stack pointer at the moment of the `syscall`.
///
/// The `syscall` instruction does not touch `RSP`, but the stub switches to
/// the kernel stack immediately afterwards, so the value has to be preserved
/// somewhere the kernel can read it back. A dedicated global is that place:
/// the entry path is strictly serialised — interrupts are masked on entry,
/// and `SFMASK` clears `IF` — so one slot is enough.
///
/// # Safety
///
/// Only the syscall entry stub writes it, and only while a syscall is being
/// dispatched.
#[unsafe(no_mangle)]
pub static mut NOVA_KERNEL_USER_STACK_POINTER: u64 = 0;

/// Install the syscall MSRs.
///
/// # Safety
///
/// Must be called with the GDT and TSS installed, because the entry stub
/// loads `CS` and `SS` from `STAR` and switches stacks through the TSS's
/// `RSP0`.
pub unsafe fn init() {
    // SAFETY: delegated to this function's contract. `STAR`, `LSTAR` and
    // `SFMASK` are ordinary MSRs that may be written in ring 0.
    unsafe {
        // `EFER.SCE` is what makes the `syscall` instruction exist at all.
        // Without it, executing `syscall` raises `#UD` — the same symptom as
        // a missing entry point, which makes the two mistakes hard to tell
        // apart.
        msr::set_efer(msr::EFER_SCE);

        // `STAR` names the kernel and user segment selectors. Its value is
        // *derived* from the GDT's slot layout by `gdt::star_value`, so a
        // descriptor that moves cannot leave `STAR` pointing at the wrong one.
        msr::write(msr::MSR_STAR, crate::arch::x86_64::gdt::star_value());

        // `LSTAR` is the entry point. The address is taken from the function
        // item rather than written down, so the two cannot drift apart.
        msr::write(msr::MSR_LSTAR, syscall_entry as *const () as u64);

        // `SFMASK` is the set of `RFLAGS` bits the CPU clears on entry.
        msr::write(msr::MSR_SFMASK, SFMASK);
    }

    // SAFETY: `read` requires the MSR to exist, and `STAR` exists on every
    // processor with the system call extension — which `set_efer` just
    // enabled.
    let star = unsafe { msr::read(msr::MSR_STAR) };

    klog!(
        Level::Info,
        "syscall: lstar={:#x} star={:#x} sfmask={:#x}",
        syscall_entry as *const () as u64,
        star,
        SFMASK
    );
}

/// The `syscall` entry stub.
///
/// # What it does
///
/// 1. Stashes the user's stack pointer.
/// 2. Switches to the kernel stack named by the TSS's `RSP0`.
/// 3. Saves the fifteen user general-purpose registers on that stack.
/// 4. Calls the dispatcher with a pointer to them.
///
/// # Safety
///
/// Reached only through the `syscall` instruction, which means `LSTAR` points
/// here. The TSS's `RSP0` must name a valid kernel stack.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() -> ! {
    core::arch::naked_asm!(
        // SAFETY: the user's stack pointer is stashed in a kernel-owned global
        // before the stack pointer is replaced. The store is a single aligned
        // eight-byte write to a statically allocated word.
        //
        // `NOVA_KERNEL_USER_STACK_POINTER` is `no_mangle` precisely so that
        // the assembler can name it: `naked_asm!` accepts no operands, so a
        // Rust-side symbol has to be reachable by name. The name in the
        // assembler is the *symbol* name, which is the Rust identifier
        // verbatim — Rust's `no_mangle` does not case-fold it.
        "mov qword ptr [rip + NOVA_KERNEL_USER_STACK_POINTER], rsp",
        // SAFETY: the TSS's `RSP0` is at offset 8 of the TSS structure —
        // `privilege_stack_table[0]`. The offset is asserted at compile time
        // in `gdt`, so the two cannot drift apart.
        //
        // `nova_kernel_tss` is the TSS the CPU itself reads on every
        // privilege transition, so the kernel stack it names is the same one
        // the CPU would have used for an interrupt.
        "mov rsp, qword ptr [rip + nova_kernel_tss + 8]",
        // SAFETY: each `push` writes one word of the register file to the
        // kernel stack. The stack is mapped and has room: the kernel reserves
        // a stack per thread, and the boot thread has one of its own.
        //
        // The order is `rax` first, so `rax` ends up at the *highest* address
        // of the save area and `r15` at the lowest. That is the order
        // `UserRegisters` declares its fields in, reversed.
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // SAFETY: an eight-byte alignment pad, so the `call` below happens
        // with `RSP` a multiple of 16 as the ABI requires.
        "sub rsp, 8",
        // SAFETY: the saved register area starts at `rsp + 8` and runs for
        // fifteen words; `lea` computes that address without touching
        // memory.
        //
        // The pointer goes in `rdi`, the SysV first integer argument slot.
        "lea rdi, [rsp + 8]",
        // SAFETY: `nova_kernel_syscall_dispatch` is an ordinary `extern "C"`
        // function, so the `call` establishes the ABI's argument and stack
        // conventions. It never returns: it ends in an `iretq` back to ring 3.
        "call nova_kernel_syscall_dispatch",
        // SAFETY: the dispatcher is declared `-> !`, so control never returns
        // here. The `ud2` is only there because the naked function's type
        // demands a terminator.
        "ud2",
    )
}

/// Dispatch one syscall.
///
/// The pure part of the path: decode the number, run the policy, and report
/// the effect. It is deliberately separate from the entry stub so that the
/// policy can be exercised without a privilege transition.
///
/// # Safety
///
/// Must be called only from the syscall entry stub, with `registers` pointing
/// at the saved user register file on the kernel stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nova_kernel_syscall_dispatch(registers: *mut UserRegisters) -> ! {
    // SAFETY: the stub passes a pointer to the fifteen saved words, which are
    // laid out exactly as `UserRegisters` declares them. The region is on the
    // kernel stack and is writable.
    let registers = unsafe { &*registers };

    let number = registers.rax;
    let args = registers.args();

    // The syscall that needs the *caller's* address space — `WRITE`, which
    // copies bytes out of a user pointer — is handled here rather than by the
    // service layer, because only the entry path knows which address space
    // the caller had. The service layer's dispatcher is used for everything
    // else, so the policy is not duplicated.
    let outcome = dispatch(number, args);

    // The effect is the scheduler's business, not the dispatcher's: a yield
    // or an exit changes *who runs next*, and the scheduler owns that.
    //
    // SAFETY: the entry path is strictly serialised — `SFMASK` cleared `IF` on
    // entry — so this single slot is written by exactly one caller at a time.
    unsafe {
        crate::task::note_syscall_effect(match outcome.effect {
            nova_core::syscall::ThreadEffect::Continue => crate::task::ThreadEffect::Continue,
            nova_core::syscall::ThreadEffect::Yield => crate::task::ThreadEffect::Yield,
            nova_core::syscall::ThreadEffect::Exit => crate::task::ThreadEffect::Exit,
            nova_core::syscall::ThreadEffect::Block => crate::task::ThreadEffect::Block,
        });
    }

    // The result goes back into the saved `RAX`.
    //
    // SAFETY: `rax` is at a fixed offset within the region `registers` points
    // at — the fifteenth word — and the region is on the kernel stack and
    // writable.
    // SAFETY: the return frame has been fully built, and returning to ring 3
    // is the point at which user-mode preemption becomes valid.
    unsafe {
        crate::arch::x86_64::port_io::sti();
        asm!(
            "mov qword ptr [{base} + 112], {result}",
            base = in(reg) registers,
            result = in(reg) outcome.result.to_raw() as u64,
            options(nostack),
        );
    }

    // A syscall can end in any of three scheduler transitions. Save the exact
    // machine state first; the scheduler writes it into a resume block before
    // it picks another thread.
    //
    // SAFETY: the entry path is serialised with interrupts masked, and the
    // scheduler functions never return into this dispatcher.
    match outcome.effect {
        nova_core::syscall::ThreadEffect::Continue => {
            // SAFETY: the user's `RIP`, `RFLAGS` and `RSP` are all known, and
            // the kernel stack has room for the frame.
            unsafe { return_to_user(registers) }
        }
        nova_core::syscall::ThreadEffect::Yield => unsafe {
            crate::sched::save_current_syscall_context(registers);
            crate::sched::yield_current_and_switch()
        },
        nova_core::syscall::ThreadEffect::Block => unsafe {
            crate::sched::save_current_syscall_context(registers);
            crate::sched::block_current_and_switch()
        },
        nova_core::syscall::ThreadEffect::Exit => unsafe {
            crate::sched::save_current_syscall_context(registers);
            crate::sched::exit_current_and_switch()
        },
    }
}

/// The result of dispatching one syscall.
struct Outcome {
    /// The value to return to the caller in `RAX`.
    result: nova_core::syscall::SyscallResult,
    /// What the call did to the calling thread.
    effect: nova_core::syscall::ThreadEffect,
}

/// Dispatch one syscall, with the entry path's own handling for the calls
/// that need the caller's address space.
///
/// # Why the `WRITE` case lives here and not in the service layer
///
/// `WRITE`'s second argument is a *user* pointer. The service layer's
/// dispatcher is pure policy — it never dereferences a user pointer, and
/// `docs` says so explicitly — so handing it a user address would mean the
/// dispatcher had to trust it. The entry path is the only place that knows
/// which address space the caller had, so it is the only place that can copy
/// the bytes out safely.
///
/// Every other number goes to the service layer, so the M3/M4 policy is
/// exercised by exactly one implementation.
fn dispatch(number: u64, args: [u64; 6]) -> Outcome {
    if number == nova_abi::nr::WRITE.number() as u64 {
        return write_to_console(args);
    }

    if number == nova_abi::nr::PROCESS_EXEC.number() as u64 {
        return process_exec(args);
    }

    stage_outgoing_payload(number, args);

    let context = crate::task::current_syscall_context();

    // SAFETY: the service state was installed by `services::init` during boot,
    // before any user thread could exist, and the entry path holds no other
    // borrow of it.
    let outcome = unsafe { crate::services::dispatch::dispatch_syscall(&context, number, args) };

    if number == nova_abi::nr::RECV.number() as u64
        && !matches!(
            outcome,
            crate::services::dispatch::SyscallOutcome::Failed(_)
        )
    {
        copy_received_payload(context.thread as usize, args);
    }

    let mut effect = nova_core::syscall::ThreadEffect::Continue;
    if number == nova_abi::nr::YIELD.number() as u64 {
        effect = nova_core::syscall::ThreadEffect::Yield;
    } else if number == nova_abi::nr::THREAD_EXIT.number() as u64 {
        effect = nova_core::syscall::ThreadEffect::Exit;
    }

    Outcome {
        result: match outcome {
            crate::services::dispatch::SyscallOutcome::Value(value) => {
                nova_core::syscall::SyscallResult::Value(value)
            }
            crate::services::dispatch::SyscallOutcome::Failed(code) => {
                nova_core::syscall::SyscallResult::Failed(code)
            }
            // A parked call blocks the calling thread until a completion
            // arrives. The service layer signals that by returning `Blocked`,
            // and the scheduler honours it by taking the thread out of the
            // runnable queue.
            crate::services::dispatch::SyscallOutcome::Blocked => {
                return Outcome {
                    result: nova_core::syscall::SyscallResult::Value(0),
                    effect: nova_core::syscall::ThreadEffect::Block,
                };
            }
        },
        effect,
    }
}

/// Copy a bounded byte range out of the calling process's address space.
fn copy_from_user(buffer: &mut [u8], pointer: u64, length: u64) -> Result<(), ErrorCode> {
    if length as usize > buffer.len() {
        return Err(ErrorCode::Invalid);
    }
    let Some(space) = crate::task::current_thread_space() else {
        return Err(ErrorCode::BadHandle);
    };
    let page_size = crate::arch::ADDRESS_SPACE.page_size;
    let mut copied = 0usize;

    while copied < length as usize {
        let address = pointer + copied as u64;
        let offset = address & (page_size - 1);
        let chunk = core::cmp::min(page_size - offset, (length as usize - copied) as u64) as usize;
        let Some(phys) = crate::mm::address_space::translate(&space, address & !(page_size - 1))
        else {
            return Err(ErrorCode::Invalid);
        };

        // SAFETY: the translation resolved a user page in the caller's space,
        // and the direct map is a kernel-only mapping over all physical frames.
        unsafe {
            let source = crate::mm::direct_map::phys_to_virt(phys + offset);
            buffer[copied..copied + chunk]
                .copy_from_slice(core::slice::from_raw_parts(source, chunk));
        }
        copied += chunk;
    }

    Ok(())
}

/// Copy bytes into the calling process's address space.
fn copy_to_user(pointer: u64, bytes: &[u8]) -> Result<(), ErrorCode> {
    let Some(space) = crate::task::current_thread_space() else {
        return Err(ErrorCode::BadHandle);
    };
    let page_size = crate::arch::ADDRESS_SPACE.page_size;
    let mut copied = 0usize;

    while copied < bytes.len() {
        let address = pointer + copied as u64;
        let offset = address & (page_size - 1);
        let chunk = core::cmp::min(page_size - offset, (bytes.len() - copied) as u64) as usize;
        let Some(phys) = crate::mm::address_space::translate(&space, address & !(page_size - 1))
        else {
            return Err(ErrorCode::Invalid);
        };

        // SAFETY: the destination is a caller-owned user page, copied through
        // the kernel's direct map with a bounded chunk.
        unsafe {
            let destination = crate::mm::direct_map::phys_to_virt(phys + offset);
            core::ptr::copy_nonoverlapping(
                bytes[copied..copied + chunk].as_ptr(),
                destination,
                chunk,
            );
        }
        copied += chunk;
    }

    Ok(())
}

/// Copy an IPC payload from user memory before the service dispatcher runs.
fn stage_outgoing_payload(number: u64, args: [u64; 6]) {
    let ipc_number = number == nova_abi::nr::SEND.number() as u64
        || number == nova_abi::nr::CALL.number() as u64
        || number == nova_abi::nr::REPLY.number() as u64;
    if !ipc_number || args[3] == 0 || args[3] > nova_abi::ipc::MAX_INLINE_PAYLOAD as u64 {
        return;
    }

    let mut payload = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
    if copy_from_user(&mut payload, args[2], args[3]).is_err() {
        return;
    }
    let opcode = if number == nova_abi::nr::REPLY.number() as u64 {
        0
    } else {
        args[1]
    };
    let message = nova_ipc::Message::with_payload(opcode, &payload[..args[3] as usize]);
    let Ok(message) = message else {
        return;
    };

    // SAFETY: service state is installed before user processes can run.
    unsafe {
        let kernel = crate::services::state();
        let thread = crate::task::current_syscall_context().thread as usize;
        if thread < kernel.outgoing_message.len() {
            kernel.outgoing_message[thread] = Some(message);
        }
    }
}

/// Copy the payload of a successfully received IPC message to user memory.
fn copy_received_payload(thread: usize, args: [u64; 6]) {
    if args[2] == 0 || args[3] == 0 {
        return;
    }

    // SAFETY: service state is installed before user processes can run.
    unsafe {
        let kernel = crate::services::state();
        let Some(message) = kernel.last_message.get(thread).and_then(Option::as_ref) else {
            return;
        };
        let payload = message.payload();
        if payload.len() <= args[3] as usize {
            let _ = copy_to_user(args[2], payload);
        }
    }
}

/// Start an initramfs image as a child process.
fn process_exec(args: [u64; 6]) -> Outcome {
    const MAX_NAME: usize = 32;
    let mut name = [0u8; MAX_NAME];
    if args[1] == 0 || args[1] as usize > MAX_NAME {
        return failed(ErrorCode::Invalid);
    }
    if copy_from_user(&mut name[..args[1] as usize], args[0], args[1]).is_err() {
        return failed(ErrorCode::Invalid);
    }

    match crate::services::process::exec_image(&name[..args[1] as usize]) {
        Ok(handle) => Outcome {
            result: nova_core::syscall::SyscallResult::Value(handle.raw() as i64),
            effect: nova_core::syscall::ThreadEffect::Continue,
        },
        Err(code) => failed(code),
    }
}

/// A dispatch failure with the default `Continue` effect.
const fn failed(code: ErrorCode) -> Outcome {
    Outcome {
        result: nova_core::syscall::SyscallResult::Failed(code),
        effect: nova_core::syscall::ThreadEffect::Continue,
    }
}

/// Handle `WRITE`: copy bytes out of the caller's address space to the console.
///
/// # The argument convention
///
/// `arg0` is the file descriptor, `arg1` the *user* pointer to the buffer, and
/// `arg2` the length. Only descriptors 1 and 2 are accepted; anything else is
/// `E_DENIED`, because there is no other sink a user process has before the
/// console service arrives in M4.
///
/// # Why the copy is bounded
///
/// The length is clamped to a fixed maximum before the copy. A user program
/// that claims a length of `u64::MAX` would otherwise make the kernel walk
/// an arbitrary amount of memory in a loop with no way to notice the claim was
/// absurd. The clamp is what makes the loop's cost a function of the kernel's
/// own constant rather than of the caller's word.
fn write_to_console(args: [u64; 6]) -> Outcome {
    const MAX_WRITE: u64 = 4096;

    let fd = args[0];
    let pointer = args[1];
    let length = args[2];

    if fd != 1 && fd != 2 {
        return Outcome {
            result: nova_core::syscall::SyscallResult::Failed(nova_abi::ErrorCode::Denied),
            effect: nova_core::syscall::ThreadEffect::Continue,
        };
    }

    let length = length.min(MAX_WRITE);
    if length == 0 {
        return Outcome {
            result: nova_core::syscall::SyscallResult::Value(0),
            effect: nova_core::syscall::ThreadEffect::Continue,
        };
    }

    // The caller's address space is the one the running thread has. The copy
    // is done *through the direct map*, page by page, because the kernel may
    // not have the user's address space loaded in CR3 while it is servicing
    // this syscall — and a kernel that dereferenced the user pointer directly
    // would fault on a perfectly valid user address.
    let space = crate::task::current_thread_space();
    let Some(space) = space else {
        return Outcome {
            result: nova_core::syscall::SyscallResult::Failed(nova_abi::ErrorCode::BadHandle),
            effect: nova_core::syscall::ThreadEffect::Continue,
        };
    };

    let page_size = crate::arch::ADDRESS_SPACE.page_size;
    let mut copied: u64 = 0;

    while copied < length {
        let address = pointer + copied;
        let offset = address & (page_size - 1);
        let chunk = core::cmp::min(page_size - offset, length - copied);

        let Some(phys) = crate::mm::address_space::translate(&space, address & !(page_size - 1))
        else {
            // The buffer runs off the end of the caller's mapped memory. The
            // bytes written so far are real, so the count is returned — a
            // short write — rather than the whole call failing.
            break;
        };

        // SAFETY: the translation resolved the page to a physical frame the
        // caller's address space owns, and the chunk is bounded by the page
        // size and by the remaining length. The direct map covers every
        // usable physical frame.
        unsafe {
            let source = crate::mm::direct_map::phys_to_virt(phys + offset);
            crate::arch::console().send_bytes(core::slice::from_raw_parts(source, chunk as usize));
        }

        copied += chunk;
    }

    Outcome {
        result: nova_core::syscall::SyscallResult::Value(copied as i64),
        effect: nova_core::syscall::ThreadEffect::Continue,
    }
}

/// Return to ring 3 with an `iretq` frame.
///
/// The frame is built on the kernel stack, below the saved user registers, and
/// holds the user's `RIP`, `CS`, `RFLAGS`, `RSP` and `SS`.
///
/// # Safety
///
/// The caller must have restored the user's `RIP` and `RSP` into `registers`,
/// and the kernel stack must have room for the five-word frame.
unsafe fn return_to_user(registers: &UserRegisters) -> ! {
    // The user's `RIP` is the address after the `syscall` instruction, which
    // the CPU put in `RCX`.
    let user_rip = registers.user_rip();
    // The user's `RFLAGS`, which the CPU put in `R11` before the entry.
    let user_rflags = registers.user_rflags();
    // The user's `RSP`. The `syscall` instruction did not change it, and the
    // stub stashed it before switching stacks.
    //
    // SAFETY: delegated to this function's contract; the stub wrote it.
    let user_rsp = unsafe { NOVA_KERNEL_USER_STACK_POINTER };
    klog!(Level::Info, "debug: return rflags={user_rflags:#x}");

    // The frame is built *below* the saved registers: the stack grows down,
    // and the saved register file must survive the return path so the caller
    // can inspect it afterwards.
    //
    // Forty-eight bytes are reserved so that the frame's base is 16-byte
    // aligned. The CPU does not require the frame's base to be aligned, but an
    // aligned frame is one less thing to be wrong about, and the kernel's own
    // stack discipline is 16-byte aligned throughout.
    //
    // The user's `RFLAGS` has `IF` restored by the `iretq` — the frame
    // carries the flags the CPU saved in `R11`, which are the flags the user
    // had *before* `SFMASK` cleared `IF`. So the user returns with interrupts
    // enabled, exactly as it had them.
    //
    // SAFETY: the kernel stack is mapped and has room for five more words.
    // The values pushed are the user's own, and the `SS`/`CS` immediates name
    // the ring 3 descriptors the GDT installs.
    unsafe {
        asm!(
            "sub rsp, 48",
            "mov qword ptr [rsp], {rip}",
            "mov qword ptr [rsp + 8], {cs}",
            "mov qword ptr [rsp + 16], {rflags}",
            "mov qword ptr [rsp + 24], {rsp}",
            "mov qword ptr [rsp + 32], {ss}",
            rip = in(reg) user_rip,
            cs = in(reg) USER_CODE_SELECTOR as u64,
            rflags = in(reg) user_rflags,
            rsp = in(reg) user_rsp,
            ss = in(reg) USER_DATA_SELECTOR as u64,
            options(nomem),
        );
    }

    // The `iretq` itself. Everything it needs is in the frame above.
    //
    // The kernel stack pointer is restored from the frame's `RSP` slot, which
    // holds the *user's* stack pointer — so after this instruction the kernel
    // stack is no longer in use, and the next thing that runs is user code on
    // the user's stack.
    //
    // `iretq` is the only way back to ring 3 that this kernel uses.
    // `sysretq` would be faster but validates `RCX` against the kernel's
    // canonical addresses and faults in a way that loses all context; see the
    // module documentation.
    //
    // SAFETY: the frame was just built on the kernel stack, and it names the
    // user's `RIP`, `CS`, `RFLAGS`, `RSP` and `SS`. The `CS` and `SS`
    // selectors name present ring 3 descriptors in the installed GDT.
    unsafe {
        asm!("iretq", options(noreturn),);
    }
}
