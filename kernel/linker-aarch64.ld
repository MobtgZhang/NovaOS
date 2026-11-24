OUTPUT_FORMAT(elf64-littleaarch64)
ENTRY(_start)

SECTIONS
{
    . = 0x40200000;
    .text ALIGN(4K) : {
        __text_start = .;
        KEEP(*(.text._start))
        *(.text .text.*)
    }
    .rodata ALIGN(4K) : {
        __rodata_start = .;
        *(.rodata .rodata.*)
    }
    .data ALIGN(4K) : {
        __data_start = .;
        *(.got .got.plt .data.rel.ro .data.rel.ro.* .data .data.*)
    }
    .bss ALIGN(4K) (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.* COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }
    .user_text ALIGN(4K) : {
        __user_text_start = .;
        KEEP(*(.user_probe.text))
        __user_text_end = .;
    }
    .user_stack ALIGN(4K) (NOLOAD) : {
        __user_stack_start = .;
        . += 4K;
        __user_stack_end = .;
    }
    __image_end_vma = .;
    __image_size = __image_end_vma - __text_start;

    ASSERT(__user_text_end - __user_text_start <= 4K, "EL0 probe exceeds one page")
    ASSERT(__image_end_vma - __text_start <= 2M, "EL1 image exceeds early page table")

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .note.* .comment) }
}
