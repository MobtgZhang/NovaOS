ENTRY(_start)

PHDRS
{
    code PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x20000;
    .text : { *(.text._start) *(.text .text.*) } :code
    . = ALIGN(4096);
    .rodata : { KEEP(*(.rodata.page_probe)) *(.rodata .rodata.* .srodata .srodata.*) } :rodata
    . = ALIGN(4096);
    .data : { *(.data .data.* .sdata .sdata.* .got .got.*) } :data
    .bss (NOLOAD) : { *(.bss .bss.* .sbss .sbss.* COMMON) } :data
    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .note.* .comment) }
}
