/* NovaOS kernel linker script (riscv64, QEMU virt machine).
 *
 * OpenSBI lands on 0x8000_0000; QEMU's `virt` machine loads the kernel at
 * 0x8020_0000 and jumps to the image's entry with `a0 = hartid`, `a1 = dtb`.
 * The layout mirrors the x86-64 script so that shared Rust code can keep
 * referring to linker symbols (__bss_start etc.) regardless of target.
 */

OUTPUT_FORMAT(elf64-littleriscv)
ENTRY(_start)

SECTIONS
{
    . = 0x80200000;
    __kernel_phys_base = .;
    __kernel_vma_base = 0x80200000;

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
