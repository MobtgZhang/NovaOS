/* NovaOS user-space linker script (x86-64, 4-level paging).
 *
 * User programs live in the low half of the address space, at the addresses
 * the ABI freezes in docs/abi/address-layout.md:
 *
 *   0x0000_0000_0000_0000  null page — never mapped, so a null pointer faults
 *   0x0000_0000_0000_1000  program image (text, rodata, data)
 *   0x0000_0000_4000_0000  heap
 *   0x0000_0000_7FFF_F000  user stack top, growing down
 *
 * The layout is deliberately fixed rather than derived, because a user binary
 * and the kernel that loads it must agree on it exactly; the kernel copies the
 * segments to the virtual addresses the ELF header names, and the loader
 * rejects an image whose segments fall outside the user window.
 *
 * The stack is not allocated by this script: the kernel maps it. The script
 * only reserves the address range as a NOLOAD region so that a null or
 * wild pointer lands in a hole rather than in the program's own text.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* The user window's boundaries, mirrored from docs/abi/address-layout.md. */
USER_IMAGE_BASE = 0x0000000000001000;
USER_STACK_TOP = 0x000000007FFFF000;

SECTIONS
{
    /* The null page. A user program must fault on a null dereference, not
     * read the first bytes of its own image. */
    . = USER_IMAGE_BASE;

    .text ALIGN(4K) :
    {
        __text_start = .;
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
        *(.data .data.*)
        *(.got .got.plt)
        *(.data.rel.ro .data.rel.ro.*)
        . = ALIGN(8);
        __data_end = .;
    }

    .bss ALIGN(4K) :
    {
        __bss_start = .;
        *(COMMON)
        *(.bss .bss.*)
        . = ALIGN(8);
        __bss_end = .;
    }

    /* The heap. The kernel maps the pages it needs on demand; the region is
     * reserved here so that two programs cannot have their heaps overlap. */
    .heap ALIGN(4K) (NOLOAD) :
    {
        __heap_start = .;
        . = . + 0x00100000;
        __heap_end = .;
    }

    /* Discard debug info: it is large, and the kernel copies the whole image
     * into user memory. */
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
