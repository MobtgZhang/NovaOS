//! The `syscall` instruction and the ABI's calling convention.
//!
//! # What the instruction does
//!
//! `syscall` is not a call. It does not push a return address, it does not
//! switch stacks, and it does not consult the TSS. What it does is:
//!
//! - load `CS` and `SS` from `STAR`, which is how the privilege level drops
//!   from ring 3 to ring 0;
//! - put the address of the instruction *after* the `syscall` into `RCX`;
//! - put the caller's `RFLAGS` into `R11`.
//!
//! So `RCX` and `R11` are clobbered by the instruction and cannot be argument
//! registers. The arguments are `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9`, and the
//! number goes in `RAX` — which is also where the result comes back.
//!
//! # Why the result is signed
//!
//! The ABI encodes a failure as a *negative* error code, so the result has to be
//! interpreted as a signed word. [`nova_abi::ErrorCode::from_result`] is the
//! only thing that turns a negative result back into an error, which is what
//! keeps the sign convention in one place.

use nova_abi::{ErrorCode, Syscall, nr};

/// A syscall that failed.
///
/// A distinct type from [`nova_abi::ErrorCode`] because the runtime has one
/// failure that is *not* an ABI error code: a short write, where the kernel
/// copied fewer bytes than asked for. That is a kernel bug rather than a
/// refusal, and lumping it in with `E_DENIED` would report it wrongly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallError {
    /// The kernel refused the call with an ABI error code.
    Refused(ErrorCode),
    /// The kernel wrote fewer bytes than were requested.
    ShortWrite,
}

/// Make a syscall and return its result.
///
/// The result is a signed word: a negative value is an error code, and
/// [`ErrorCode::from_result`] is what recognises one. A non-negative value is
/// the call's return value.
pub fn call(number: u64, args: [u64; 6]) -> Result<i64, SyscallError> {
    let mut result: u64;

    // SAFETY: the `syscall` instruction is valid at ring 3, and the kernel's
    // entry path restores every register it clobbers. `RCX` and `R11` are
    // listed as clobbered because the instruction overwrites them.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }

    let signed = result as i64;
    match ErrorCode::from_result(signed) {
        Some(code) => Err(SyscallError::Refused(code)),
        None => Ok(signed),
    }
}

/// Write `buffer` to the file descriptor `fd`.
///
/// Returns the number of bytes the kernel copied. The kernel copies out of
/// this program's own address space, so the buffer may be anywhere in user
/// space — the kernel validates it rather than trusting it.
pub fn write(fd: u64, buffer: &[u8]) -> Result<u64, SyscallError> {
    let result = call(
        Syscall(nova_abi::nr::WRITE.number()).raw(),
        [fd, buffer.as_ptr() as u64, buffer.len() as u64, 0, 0, 0],
    )?;
    Ok(result as u64)
}

/// Exit the calling thread with `status`.
///
/// Never returns: the kernel does not schedule the thread again, so there is
/// nothing for the caller to come back to.
pub fn exit_thread(status: i64) -> ! {
    // A refusal to exit would strand the thread, so the result is deliberately
    // discarded: the thread is finished either way.
    let _ = call(
        Syscall(nr::THREAD_EXIT.number()).raw(),
        [status as u64, 0, 0, 0, 0, 0],
    );

    // The kernel's exit path does not return. If it somehow did — a kernel
    // that failed to honour the exit — the thread would spin here rather than
    // run off into whatever follows the call.
    loop {
        core::hint::spin_loop();
    }
}
