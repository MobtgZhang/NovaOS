ENTRY(_start)

PHDRS
{
    code PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text._start) *(.text .text.*) *(.rodata .rodata.*) } :code
    . = 0x13000;
    .data : { *(.data .data.* .sdata .sdata.* .got .got.*) } :data
    .bss (NOLOAD) : { *(.bss .bss.* .sbss .sbss.* COMMON) } :data
    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .note.* .comment) }
    ASSERT(SIZEOF(.text) <= 4096, "early init exceeds one code page")
    ASSERT(. <= 0x14000, "early init exceeds one data page")
}
