//! LoongArch 64 entry: the instructions the firmware jumps to.
//!
//! QEMU loads the kernel's ELF at a fixed address and enters at the image's
//! `e_entry`. This stub sets the stack pointer to the kernel's own 64 KiB
//! stack and jumps to the Rust entry point; the stack lives in `.bss` and
//! is not relocated.

pub(super) const BOOT_STACK_SIZE: usize = 64 * 1024;

#[repr(C, align(16))]
pub struct KernelStack(pub [u8; BOOT_STACK_SIZE]);

/// The stack buffer.
///
/// Written only through the CPU's stack pointer, never through Rust, so
/// there is nothing safe to offer here: the field is private, the type is
/// module-private, and the entry asm is the only consumer.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".bss.kernel_stack")]
pub static KERNEL_STACK: KernelStack = KernelStack([0; BOOT_STACK_SIZE]);

core::arch::global_asm!(
    ".section .text._start, \"ax\", @progbits",
    ".global _start",
    "_start:",
    "la.local $sp, KERNEL_STACK + {stack_size}",
    "b bringup_entry",
    stack_size = const BOOT_STACK_SIZE,
);
