//! LoongArch 64 entry: the instructions the firmware jumps to.
//!
//! QEMU loads the kernel's ELF at a fixed address and enters at the image's
//! `e_entry`. This stub sets the stack pointer to the kernel's own 32 KiB
//! stack and jumps to the Rust entry point; the stack lives in `.bss` and
//! is not relocated.

#[repr(C, align(16))]
pub struct KernelStack(pub [u8; 32 * 1024]);

/// The stack buffer.
///
/// Written only through the CPU's stack pointer, never through Rust, so
/// there is nothing safe to offer here: the field is private, the type is
/// module-private, and the entry asm is the only consumer.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".bss.kernel_stack")]
pub static KERNEL_STACK: KernelStack = KernelStack([0; 32 * 1024]);

core::arch::global_asm!(
    ".section .text._start, \"ax\", @progbits",
    ".global _start",
    "_start:",
    "la.local $sp, KERNEL_STACK + 32768",
    "b bringup_entry",
);
