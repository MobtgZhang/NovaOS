//! x86-64 backend.
//!
//! Everything here is x86-64-specific and nothing outside `arch/x86_64/` may
//! name it. The module's job is to expose four things — `NAME`,
//! `ADDRESS_SPACE`, `ConsoleImpl`, `ExitImpl`, `Interrupts` — and to run
//! [`arch_init`] in the order this architecture requires.
//!
//! # Boot-time ordering
//!
//! The order is not stylistic. Each step depends on the previous one being able
//! to report failure:
//!
//! 1. IDT — must exist before anything that can fault, or a fault becomes a
//!    triple fault and the machine resets with no output at all. This includes
//!    the segment instructions in step 2, since `ltr` itself can fault.
//! 2. GDT/TSS — needed before any ring 3 code runs, and before interrupts can
//!    be unmasked.
//! 3. Interrupt routing — after the IDT, before any unmask.
//!
//! Interrupts stay masked for the whole sequence. Enabling them before the IDT is
//! in place is the classic way to get a machine that boots and dies.

pub mod apic;
pub mod gdt;
pub mod idt;
pub mod intr;
pub mod msr;
pub mod paging;
pub mod pit;
pub mod port_io;

pub mod console;
pub mod exit;

pub use console::ConsoleImpl;
pub use exit::ExitImpl;

use crate::arch::shared::cpu::InterruptControl;
use crate::arch::shared::hw::AddressSpace;

/// Architecture name used in the boot banner and log tags.
pub const NAME: &str = "x86_64";

/// x86-64 with 4-level paging.
///
/// 52 physical bits is the architectural maximum for a four-level translation:
/// 48 bits of page table plus the reserved bits the PTE format cannot express.
/// 48 virtual bits matches LA57 being off, which is what Limine configures.
pub const ADDRESS_SPACE: AddressSpace = match AddressSpace::new(0x1000, Some(52), Some(48)) {
    Some(space) => space,
    None => unreachable!(),
};

/// x86-64 interrupt mask, backed by `cli`/`sti`.
///
/// # Concurrency
///
/// `cli`/`sti` do not push or pop flags; they set and clear `IF` in place. There
/// is therefore no atomic read-and-modify-write of the interrupt state, so a
/// caller can only learn "was it masked?" by reading `RFLAGS` first. That read is
/// not atomic with respect to a concurrent unmask, which is the same limitation
/// every x86-64 kernel has and the reason SMP porting needs per-CPU `RFLAGS`
/// saves.
pub struct Interrupts;

impl Interrupts {
    /// Read the current interrupt flag out of `RFLAGS`.
    ///
    /// # Safety
    ///
    /// None; `RFLAGS` is readable at any privilege level.
    fn read_flag() -> bool {
        let flags: u64;
        // SAFETY: `pushfq`/`pop` on a local is a plain register read. The `push`
        // is balanced by the `pop` in the same asm block, and `readonly` is
        // deliberately omitted because the instruction writes to the stack.
        unsafe {
            core::arch::asm!(
                "pushfq",
                "pop {0}",
                out(reg) flags,
                options(nostack, preserves_flags),
            );
        }
        flags & (1 << 9) != 0
    }
}

impl InterruptControl for Interrupts {
    fn mask(&self) -> bool {
        // `cli`/`sti` are safe wrappers: they encode the architectural fact that
        // neither instruction can fault at any privilege level.
        port_io::cli();
        Self::read_flag()
    }

    fn restore(&self, was_masked: bool) {
        if was_masked {
            port_io::cli();
        } else {
            port_io::sti();
        }
    }

    fn is_masked(&self) -> bool {
        Self::read_flag()
    }
}

/// Bring up the x86-64 machine state.
///
/// # Safety
///
/// Must run exactly once from the boot entry point, with interrupts masked.
pub unsafe fn arch_init() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        // The GDT goes up *first*, before the IDT.
        //
        // Every IDT gate names the kernel code selector (`0x08`). A gate is only
        // valid if that selector names a present, executable code segment *in
        // the GDT that is installed when the gate is used* — and `gdt::init`
        // ends in `ltr`, which is itself a faulting instruction.
        //
        // So the two initialisations have to be ordered around both of those
        // facts, and each ordering is wrong in a way that is invisible:
        //
        //   IDT first — `ltr` raises `#GP` if its selector does not name a
        //   present TSS, and with no IDT installed there is no handler, so the
        //   fault escalates to `#DF`, also unhandled, and the machine
        //   triple-faults and resets. The reset takes under a second and, with
        //   `-no-reboot`, QEMU exits *successfully* — so a harness reports a
        //   clean pass for a kernel that never ran.
        //
        //   GDT first — the gates are then filled against a valid selector, but
        //   `lgdt` and `ltr` run with the bootloader's IDT still installed. A
        //   fault during that window lands in Limine's flat handlers, which
        //   cannot report anything in NovaOS's format.
        //
        // Neither order is safe on its own. The resolution is that the GDT is
        // built and loaded first, and `gdt::init` does its `ltr` only after the
        // GDT is in place; the IDT is installed immediately afterwards, before
        // anything else can fault. The window between the two is exactly the
        // GDT/TSS setup, which is written to be non-faulting once `lgdt` has
        // succeeded.
        gdt::init();

        idt::init();
    }

    // Interrupt routing has no safety requirement of its own but must not run
    // before the IDT: an IRQ arriving in between would land on a zeroed gate.
    intr::init();
}
