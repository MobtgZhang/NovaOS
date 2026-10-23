/* NovaOS kernel linker script (x86_64, 4-level paging).
 *
 * The kernel is linked in the higher half and loaded low. Distinguishing the
 * virtual address (VMA) from the load address (LMA) means the kernel always
 * knows its own physical base at link time via `__kernel_phys_base`, which is
 * what lets M2 build recursive page tables and switch CR3 without asking the
 * bootloader where it put us.
 *
 * The virtual layout itself is frozen in docs/abi/address-layout.md.
 *
 * Section placement rule: every output section that the running kernel will
 * *dereference* must be explicitly placed in the higher half. Anything left to
 * the linker's default placement lands at the current location counter — which
 * here is the low LMA — and is then either unreachable at runtime or silently
 * overlaps `.boot`. `.got` is the case that bites in practice, because
 * `code-model=kernel` routes absolute references through it.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* Higher-half virtual base for the kernel image. */
KERNEL_VMA_BASE = 0xFFFFFFFF80100000;

/* Physical load address. Low memory, above the legacy BIOS/BDA region and
 * below 1 MiB so that no conventional BIOS area is clobbered. */
KERNEL_LMA_BASE = 0x00100000;

/* The constant offset between VMA and LMA. Every `AT(...)` below is written as
 * `ADDR(section) - KERNEL_VMA_BASE + KERNEL_LMA_BASE` rather than as a literal,
 * so changing either base cannot silently desynchronise the two. */
KERNEL_VMA_LMA_DELTA = KERNEL_VMA_BASE - KERNEL_LMA_BASE;

SECTIONS
{
    /* Start laying out at the load address... */
    . = KERNEL_LMA_BASE;
    __kernel_phys_base = .;

    /* ...but all symbols below refer to the virtual address. */
    . = KERNEL_VMA_BASE;
    __kernel_vma_base = KERNEL_VMA_BASE;

    /* The Limine protocol requests, kept contiguous and page-aligned.
     *
     * NOTE: this section is a *convenience*, not the discovery mechanism.
     * Limine 12 finds the requests by their exported `LIMINE_*` symbol names,
     * not by section; an earlier revision of this script relied on the
     * `.boot.rodata` placement that Limine 11 used, and the kernel then booted
     * and reported "bootloader too old" — a misleading error caused entirely
     * by the section being the wrong mechanism. See `kernel/src/boot/limine.rs`.
     *
     * The section is still worth having: it groups the requests and their
     * alignment in one place, so the linker cannot interleave unrelated data
     * between a marker and the requests it delimits.
     */
    .boot ALIGN(4K) : AT(ADDR(.boot) - KERNEL_VMA_BASE + KERNEL_LMA_BASE)
    {
        __boot_start = .;
        KEEP(*(.boot.text))
        KEEP(*(.boot.rodata))
        __boot_end = .;
    }

    /* The requests themselves. They must live in a writable section, because
     * the bootloader overwrites them in place with its responses; `.data.rel.ro`
     * is writable at load time and read-only afterwards, which is exactly the
     * contract, so `READONLY` on the containing segment is fine but `WA` on the
     * section is required. */

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_VMA_BASE + KERNEL_LMA_BASE)
    {
        __text_start = .;
        *(.text .text.*)
        __text_end = .;
    }

    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_VMA_BASE + KERNEL_LMA_BASE)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    }

    /* Data that needs relocation, including the GOT. These are read *before* any
     * paging is installed, so they must live in the image the bootloader loaded
     * rather than in a section that would need a relocation loop. */
    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_VMA_BASE + KERNEL_LMA_BASE)
    {
        __data_start = .;

        *(.got .got.plt)
        *(.data.rel.ro .data.rel.ro.*)
        *(.data .data.*)

        . = ALIGN(8);
        __data_end = .;
    }
    __data_load_start = LOADADDR(.data);

    /* Uninitialised data. Zeroed by the boot code, never loaded. */
    .bss ALIGN(4K) : AT(ADDR(.bss) - KERNEL_VMA_BASE + KERNEL_LMA_BASE)
    {
        __bss_start = .;
        *(COMMON)
        *(.bss .bss.*)
        . = ALIGN(8);
        __bss_end = .;
    }
    __bss_size = __bss_end - __bss_start;

    /* End of the image. The frame allocator's floor is derived from this, so it
     * must be page-aligned: a partial trailing page is still partly occupied and
     * rounding down would hand it out. */
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