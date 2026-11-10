//! RISC-V 64 entry: the instructions the firmware jumps to.
//!
//! QEMU loads the kernel's ELF at `0x8020_0000` and enters at the image's
//! `e_entry` with `a0 = hartid` and `a1 = dtb`. The kernel must not rely on
//! either: this stub sets the stack pointer to the kernel's own 32 KiB stack
//! and jumps to the Rust entry point. The stack lives in `.bss` and is not
//! relocated (the linker script places `.bss` in the image at the load
//! address, so the stack pointer is a link-time constant).

/// The kernel's own stack: 32 KiB, 16-byte aligned at the top.
///
/// The buffer is here (not in `mod.rs`) so the `global_asm!` in the same
/// translation unit sees the symbol. The alignment attribute keeps the
/// buffer's base 16-aligned, and the size is a multiple of 16 so the top is
/// aligned too.
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
    "la sp, KERNEL_STACK + 32768",
    "tail bringup_entry",
);
