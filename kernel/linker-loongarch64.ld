/* NovaOS kernel linker script (loongarch64, QEMU virt machine).
 *
 * QEMU's `virt` machine loads the kernel at 0x9000_0000 when booting with
 * `-kernel`, and the entry runs with interrupts masked. The layout mirrors
 * the riscv64 script so shared Rust code can keep referring to linker
 * symbols regardless of target.
 */

OUTPUT_FORMAT(elf64-loongarch)
ENTRY(_start)

SECTIONS
{
    . = 0x200000;
    __kernel_phys_base = .;
    __kernel_vma_base = 0x200000;

    .text ALIGN(4K) :
    {
        __text_start = .;
        *(.text._start)
        *(.text .text.*)
        __text_end = .;
    }

    .rodata ALIGN(4K) :
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    }

    .data ALIGN(4K) :
    {
        __data_start = .;
        *(.got .got.plt)
        *(.data.rel.ro .data.rel.ro.*)
        *(.data .data.*)
        . = ALIGN(8);
        __data_end = .;
    }

    .bss ALIGN(4K) :
    {
        __bss_start = .;
        *(.bss .bss.*)
        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(4K);
    __image_end_vma = .;

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.debug_*)
        *(.gnu.build.attributes)
    }
}
