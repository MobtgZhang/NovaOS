//! Arch-neutral fault injection.
//!
//! # Why this is a capability
//!
//! The in-kernel test suite proves that the exception machinery works by
//! deliberately raising the exceptions it claims to handle. That needs two
//! things from the architecture, and neither of them is portable:
//!
//! 1. an instruction that provokes a given exception, and
//! 2. a known address to resume at, so the handler can skip the trapping
//!    instruction instead of re-executing it.
//!
//! Each backend supplies its own pair. The *intent* — "raise an illegal
//! instruction" — is architecture-neutral, so it is expressed as a trait here
//! and the backend decides which instruction to use.
//!
//! # A caveat about "divide by zero"
//!
//! Only x86-64 traps on integer division by zero. AArch64 and RISC-V define
//! the result as zero (and `-1` respectively) with no exception at all, and
//! LoongArch *does* trap. A backend whose architecture has no divide trap
//! therefore raises a different exception for the same request, and the
//! [`TrapSource`] documentation says which one. Callers that assert on a
//! specific [`crate::arch::shared::fault::FaultKind`] must ask the backend
//! what it raises rather than assuming.

use crate::arch::shared::fault::FaultKind;

/// A source of deliberate CPU traps.
///
/// Implemented once per architecture. All the methods are pure address
/// queries except the two that raise the trap.
pub trait TrapSource {
    /// The address of the instruction that raises an illegal-instruction fault.
    ///
    /// The faulting instruction pointer the handler reports must fall inside
    /// this address and the corresponding resume address, which is what lets a
    /// test assert that the handler read the right register.
    fn illegal_instruction_entry(&self) -> u64;

    /// The address the handler resumes at after an illegal-instruction fault.
    ///
    /// Strictly past [`TrapSource::illegal_instruction_entry`], so resuming
    /// there cannot re-execute the trapping instruction.
    fn illegal_instruction_resume_entry(&self) -> u64;

    /// The address of the instruction that raises a divide-by-zero fault.
    ///
    /// On architectures where integer division by zero does not trap, this is
    /// the address of an instruction that raises the closest equivalent
    /// exception; see [`TrapSource::divide_by_zero_kind`].
    fn divide_by_zero_entry(&self) -> u64;

    /// The address the handler resumes at after a divide-by-zero fault.
    fn divide_by_zero_resume_entry(&self) -> u64;

    /// The fault kind the backend reports for
    /// [`TrapSource::raise_divide_by_zero`].
    ///
    /// [`FaultKind::DivideByZero`] on an architecture that traps on division by
    /// zero, and [`FaultKind::IllegalInstruction`] on one that does not.
    fn divide_by_zero_kind(&self) -> FaultKind;

    /// Raise an illegal-instruction fault.
    ///
    /// # Safety
    ///
    /// The caller must be able to receive the fault: the exception table must
    /// be installed and the fault handler must have somewhere to report it.
    /// The call does not return through the trapping instruction; it returns
    /// only if the handler chooses to resume.
    unsafe fn raise_illegal_instruction(&self);

    /// Raise a divide-by-zero fault.
    ///
    /// # Safety
    ///
    /// The caller must be able to receive the fault: the exception table must
    /// be installed and the fault handler must have somewhere to report it.
    /// The call does not return through the trapping instruction; it returns
    /// only if the handler chooses to resume.
    unsafe fn raise_divide_by_zero(&self);
}
