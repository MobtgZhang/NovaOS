# NovaOS virtual address layout

This document freezes the virtual address space layout. It is the contract
between the kernel, the user-space linker script (`user/linker.ld`), and every
user binary built against the ABI.

**Status:** frozen for M2.

## Why the layout is frozen and not merely described

A user binary and the kernel that loads it must agree *exactly* on where the
image goes, where the stack is, and where the kernel lives. If either side
derives an address differently — a different page size, a different half-open
interval convention — the result is not a crash but a process that runs and
misbehaves. Writing the layout down once, and having both sides reference it,
is the only way to make that disagreement impossible.

The rule this document exists to enforce is narrower than "write down the
numbers", and it is the rule that makes the numbers safe to write down:

> **Nothing in the tree hardcodes an address that can be derived.**

The constants below fall into three classes, and the distinction matters:

| Class | Where it comes from | Can it move? |
| --- | --- | --- |
| **Derived** | Computed from the address-space geometry or from the bootloader's answers | Yes — no code change, because nothing hardcodes it |
| **ABI** | Agreed with `user/linker.ld`; a shared constant | Only by changing both sides in the same commit |
| **Physical** | Where the firmware put the kernel image | No — the linker script fixes it |

A derived value that is also written down is not derived any more: it is a
second source of truth that can disagree with the first, and the disagreement
is invisible until an address lands in the wrong page. So each number below
names the symbol that owns it, and that symbol is the only place the number
appears.

## The kernel half

```
0xFFFF_8000_0000_0000  ─────────────────────────────  kernel half start
                        ┬ HHDM (higher-half direct map)
                        │   virtual = physical + direct_map_offset
                        │   the map's own base is the *offset*, which the
                        │   bootloader reports — it is not a constant
0xFFFF_FFFF_8010_0000  ┬ kernel image (.text)
                        ┘ kernel image end (__image_end_vma)
0xFFFF_FFFF_FFFF_FFFF  ─────────────────────────────  last canonical address
```

The kernel image's VMA is the one literal-shaped value in this diagram, and
it is still not a literal in the code: it comes from the linker script's
`KERNEL_VMA_BASE`. The diagram states it because a reader of the layout needs
to see where the kernel sits relative to the direct map.

### Kernel half start — **derived**

`0xFFFF_8000_0000_0000` is *not* written into the kernel as a literal. It is
`arch::ADDRESS_SPACE.kernel_half_start()`, which is computed from the target's
declared virtual-address width (48 bits, LA57 off) by
`arch::shared::hw::AddressSpace::kernel_half_start`. The same value reaches
the ELF validator (`nova_elf::Elf::validate`) and the user-window derivation,
so "kernel space" means exactly one thing in the whole tree.

A LA57 target would move the boundary up to `0xFF00_0000_0000_0000` and every
dependent computation would follow, because none of them hold the number.

### HHDM offset — **derived from the bootloader**

The direct map is `physical + direct_map_offset`. The offset is whatever the
bootloader reports in its `LIMINE_DIRECT_MAP` response, captured at boot into
`boot::BootInfo::direct_map_offset` and published through
`mm::direct_map::offset()`.

**The kernel never hardcodes it.** This is the single most important
"no hardcoded address" rule in the document: the offset is a property of the
*bootloader's* address-space layout, not of NovaOS, and a kernel that assumed a
particular value would read the wrong memory on a different firmware or a
different Limine configuration.

On the x86-64 Limine configuration this tree is tested against, the reported
offset happens to equal the kernel-half start. That is an observation about
one bootloader, not an identity: the two are conceptually unrelated, and the
code must not rely on them coinciding. `boot::BootInfo::validate` requires the
offset to be non-zero and page-aligned, and nothing more.

Because the HHDM is where the frame allocator reaches physical memory, the
kernel's own page tables must map it — see *Recursive paging* below.

### Kernel image — **physical, from the linker script**

```
VMA (virtual)  0xFFFF_FFFF_8010_0000     __kernel_vma_base
LMA (physical) 0x0000_0000_0010_0000     __kernel_phys_base
```

Both come from `kernel/linker.ld` as linker symbols; the kernel reads them
rather than repeating the numbers. The image is loaded low (so the bootloader
can read it) and linked high (so the kernel runs in the top canonical half,
where the user window's last page cannot share a page with the kernel's own).

The image spans, by section:

| Region | Linker symbols | Protection |
| --- | --- | --- |
| `.text` | `__text_start` .. `__text_end` | read + execute, **not writable** |
| `.rodata` | `__rodata_start` .. `__rodata_end` | read-only |
| `.data` | `__data_start` .. `__data_end` | read + write |
| `.bss` | `__bss_start` .. `__bss_end` | read + write, zero-filled |

The image end (`__image_end_vma`) is the frame allocator's floor: a frame
below it is still partly occupied by the running kernel. `kernel/src/mm/mod.rs`
computes that floor from `__kernel_phys_base` and the image's virtual span,
and rounds it *up* — a partial trailing page is still partly occupied, and
rounding down would hand it out.

## The user half

```
0x0000_0000_0000_0000  ┬ null page — never mapped
0x0000_0000_0000_1000  ┬ user window start
                       │   program image, heap, stack
0x0000_0000_7FFF_F000  ┬ user stack top (initial RSP)
0x0000_7FFF_FFFF_F000  ┘ user window end (exclusive)
0x0000_8000_0000_0000  ── first non-canonical address
```

### User window — **derived**

```
start = page_size
end   = 2^(virtual_bits - 1) - page_size
```

The bounds are computed by `nova_core::address_space::UserWindow::derive`,
from the page size and the target's virtual-address width. The lower bound is
the page after the null page: address zero must fault, not resolve to the
first page of the process. The upper bound is the last canonical *user* page —
one page below `2^(virtual_bits - 1)`, the top of the user half — so a user
mapping can never share a page with the kernel's own, and can never land in
the PML4 entries the kernel half occupies.

The window is *half-open*: `start` is inclusive, `end` is exclusive. The last
byte a user mapping may occupy is `0x0000_7FFF_FFFF_EFFF`.

On a 48-bit address space these two derivations produce exactly
`0x1000` and `0x0000_7FFF_FFFF_F000`, which is why the plan can state those
numbers and the code can still derive them: the numbers are a *property* of
the geometry, not a separate choice.

#### Why the bound is not "the page below the kernel half"

The kernel half begins at `0xFFFF_8000_0000_0000`, so "one page below the
kernel half" is `0xFFFF_7FFF_FFFF_F000` — and that address is **not
canonical**. Its bits 63:48 are `0xFFFF` while bit 47 is clear, so the sign
extension disagrees with bit 47 and the hardware does not address it at all.
A window bounded that way is *larger than every canonical address in the
space*, which means it excludes nothing at all.

The bound is not merely too large. `0x0000_8000_0000_0000` and
`0xFFFF_8000_0000_0000` have the **same page-table walk** — PML4 entry 256,
PDPT 0, PD 0, PT 0 — and PML4 entry 256 is the kernel half's own entry, which
`clone_kernel_half` copies into every process's PML4. A user mapping at the
former would therefore land in the kernel's own tables. The corrected bound
(`0x0000_7FFF_FFFF_F000`) excludes every address whose walk reaches PML4
entry 256 or above, and the host tests in `nova-core` pin that property
directly.

The kernel half's start is still passed to `UserWindow::derive` alongside the
address width, but as a **cross-check** rather than as the source of the
bound. The two are related — the kernel half starts at
`2^64 - 2^(virtual_bits - 1)`, the mirror of the user half's top — and a
geometry in which they disagree is a geometry in which one of them was
derived wrongly. `UserWindow::derive` refuses such a geometry rather than
silently picking one of the two.

#### The canonical-address rule

The same rule underlies the bound, so it is worth stating once here. An
address is canonical when the bits above the addressable range **replicate**
the last addressable bit: for a 48-bit space, bits 63:48 must be all-zero
when bit 47 is clear and all-one when bit 47 is set.

That is stricter than "the sign is all-zero or all-one". `0x0000_8000_0000_0000`
has an all-zero sign and bit 47 set, and `0xFFFF_0000_0000_0000` has an all-one
sign and bit 47 clear; neither replicates bit 47, and neither is addressable.
The rule lives in exactly one place — `nova_core::paging::is_canonical` — and
the ELF validator (`nova_elf::is_canonical`) and the kernel's
`arch::x86_64::paging::is_canonical` both delegate to it, because three
copies of the rule are three answers to the same question.

### The null page

`0x0` .. `0xFFF` is never mapped in any user address space. A null pointer
dereference is therefore a page fault that the kernel attributes to the
offending process, rather than a read of the process's own first page.

### User image — **from the ELF**

The program image is *not* at a fixed address in the kernel's terms: the loader
maps each `PT_LOAD` segment at the `p_vaddr` the image declares, and refuses
any segment outside the user window. `user/linker.ld` places the image at
`0x0000_0000_0000_1000` and the kernel's loader honours that, but the kernel
never assumes it.

The loader validates before it maps anything: a segment that runs past the end
of the file, an unaligned `p_align`, a non-canonical address, a segment that
reaches into the kernel half, and two segments that overlap in memory are all
refused. A malformed image is refused whole rather than half-loaded, because a
program running with half its segments mapped has an unpredictable failure
mode.

### User stack — **ABI constant, shared with `user/linker.ld`**

```
stack top (initial RSP)   0x0000_0000_7FFF_F000
stack size                64 KiB  (16 pages)
stack base                0x0000_0000_7FFF_F000 - 64 KiB
guard page                the page immediately below the stack base
```

The constants live in `nova_abi::layout` — `USER_STACK_TOP`, `USER_STACK_SIZE`,
`USER_STACK_BASE`, `USER_GUARD_BASE` — and `user/linker.ld` spells the same
values in its own syntax. This is an **ABI** constant, not a derived one: the
user binary's linker script and the kernel must agree, and neither computes
the other. It is well inside the derived user window, so the two rules do not
conflict.

Concretely, on a 48-bit address space the corrected window is
`0x1000 .. 0x0000_7FFF_FFFF_F000`, and every ABI constant falls inside it:

| Constant | Value | Inside the window |
| --- | --- | --- |
| `USER_IMAGE_BASE` | `0x0000_0000_0000_1000` | yes |
| `USER_HEAP_BASE` | `0x0000_0000_4000_0000` | yes |
| `USER_HEAP_BASE + USER_HEAP_SIZE` | `0x0000_0000_4010_0000` | yes |
| `USER_GUARD_BASE` | `0x0000_0000_7FFE_F000` | yes |
| `USER_STACK_BASE` | `0x0000_0000_7FEF_F000` | yes |
| `USER_STACK_TOP` | `0x0000_0000_7FFF_F000` | yes |

The stack top is the highest of them, and it is still roughly 128 TiB below the
window's upper bound, so the ABI constants are not the constraint on the
window's size — the geometry is.

A linker script has no way to read a Rust constant, so the agreement between
the two halves is maintained by *kind* rather than by a generated header:
both sides name the same value for the same thing, and the kernel's mapping
path checks every stack page against the derived window before it is mapped
(`nova_core::address_space::AddressSpace::check_map` refuses a user mapping
outside the window), so a disagreement shows up as a refusal to start rather
than as a program that runs somewhere unexpected.

The **guard page** is the page immediately below the stack's lowest page. It
is deliberately left unmapped, so a stack overflow raises `#PF` at the guard
rather than silently corrupting whatever lies below the stack. The guard is
not a mapping — it is the *absence* of one — and both
`nova_abi::layout::USER_GUARD_BASE` and
`nova_core::address_space::GuardPage` name the range so that the kernel and
the tests agree on which page it is.

### User heap — **ABI constant, shared with `user/linker.ld`**

`nova_abi::layout::USER_HEAP_BASE` reserves a 1 MiB heap region at
`0x0000_0000_4000_0000`. The kernel does not map it eagerly; it is the region
a user program's allocator draws from once the mapping call exists.
Reserving it here keeps two processes' heaps from overlapping.

> **Resolved.** `user/linker.ld` used to emit `.heap` at the location counter,
> i.e. immediately after `.bss`, rather than at `USER_HEAP_BASE`. The region is
> `NOLOAD` and nothing maps it today, so the mismatch was harmless in itself —
> but it also made the linker refuse the image outright, because a `.heap` that
> overlaps `.text` is a layout the linker rejects rather than one it silently
> accepts. The script now moves the location counter to `USER_HEAP_BASE`
> before emitting the region, and the built image places `.heap` at
> `0x0000_0000_4000_0000` as this section says.

## W^X

A user page is **never** both writable and executable. The rule is enforced in
the mapping API (`nova_core::address_space::AddressSpace::check_map`), not by
the caller, because the caller is exactly the code that would get it wrong: an
ELF loader must choose between "writable" and "executable" from a segment's
`p_flags`, and a `.text` that is writable is a shellcode target.

| Segment flags | Mapping |
| --- | --- |
| `PF_R` | read-only |
| `PF_R \| PF_W` | read + write, not executable |
| `PF_R \| PF_X` | read + execute, not writable |

A segment that is both writable and executable (`PF_R | PF_W | PF_X`) is
**rejected** by the loader rather than silently split. Splitting it would need
two mappings of the same physical page with different protections, which x86-64
does not support.

Kernel mappings are exempt: the kernel is not the adversary, and the kernel
image's sections are mapped according to their own permissions (`.text` is not
writable, `.data` and `.bss` are not executable). The exemption is asymmetric
on purpose — the rule exists to constrain the *user* half.

## Recursive paging

The kernel installs its own page tables, replacing the bootloader's. The
structure is:

```
PML4[0..256]     user half   — private to each address space
PML4[256..512]   kernel half — shared by every address space
```

The kernel half's top-level entries are **copied**, not duplicated, into every
process's PML4. The entries point at the *same* intermediate tables, so the
kernel's own mappings — and the HHDM — are present in every address space and
cost one page table's worth of memory in total rather than one per process.

The user half is private: a process's `PML4[0..256]` subtree is its own, so
two processes cannot see each other's pages even at the same virtual address.

The kernel's own tables are built by copying the *bootloader's* tables rather
than by re-deriving the kernel image's mapping from scratch. The bootloader
already mapped the image, the HHDM and the boot stack; reproducing that
mapping by hand would mean hardcoding addresses the bootloader chose.

### The HHDM mapping

The kernel's own tables must map the HHDM, because the frame allocator
reaches physical memory through it and the loader writes into user frames
through it. The mapping covers exactly the firmware's *usable* regions — not
the whole physical range — so a process cannot be handed a frame the firmware
said was not RAM.

Every page table is itself mapped into the HHDM, so a table named by a
*physical* address is reachable from the kernel's address space. That is what
lets one process's tables be edited while another address space is loaded.

### Page-table hierarchy

| Level | Covers | Entries |
| --- | --- | --- |
| PML4 | 512 GiB | 512 |
| PDPT | 1 GiB | 512 |
| PD | 2 MiB | 512 |
| PT | 4 KiB | 512 |

All four levels are 4 KiB, which is what lets a page table be mapped by a page
table.

## The syscall boundary

```
user ring 3                              kernel ring 0
──────────                                ────────────
RAX  syscall number in, result out
RDI  arg 0
RSI  arg 1
RDX  arg 2
R10  arg 3
R8   arg 4
R9   arg 5
```

`RCX` and `R11` are **not** argument registers: `syscall` overwrites them with
the return address and the saved flags.

The entry instruction is `syscall`; the return instruction is `iretq`. The
full convention — the entry mechanism, the error encoding, the argument rules
per call — is in `docs/abi/syscalls-x86_64.md`.

## Segment selectors

| Selector | Descriptor | RPL | Owning symbol |
| --- | --- | --- | --- |
| `0x08` | kernel code | 0 | `gdt::KERNEL_CODE_SELECTOR` |
| `0x10` | kernel data | 0 | `gdt::KERNEL_DATA_SELECTOR` |
| `0x18 \| 3` = `0x1B` | user code | 3 | `gdt::USER_CODE_SELECTOR` |
| `0x30 \| 3` = `0x33` | user data | 3 | `gdt::USER_DATA_SELECTOR` |
| `0x20` | TSS | 0 | `gdt::KERNEL_TSS_SELECTOR` |

`STAR`'s kernel half must name a *pair* of descriptors: the kernel code
segment at `STAR[47:32]` and the kernel data segment at `STAR[47:32] + 8`.
That is why the kernel code and data descriptors occupy adjacent GDT slots
(`0x08` and `0x10`).

The selectors are *derived* from the GDT slot layout rather than written down,
so a descriptor that moves cannot leave `STAR` or the `iretq` frame pointing at
the wrong one. The table above states the derived values because a reader of
the layout needs to see them; `gdt::star_value` computes `STAR` from the same
slot indices, and the GDT asserts the consistency of the two. The user
selectors are the ones the `iretq` frame carries, and they are the same two
constants the return path uses — the frame cannot name a ring 3 segment the
GDT does not hold.

The TSS selector deserves a note. It is derived from `TSS_SLOT` rather than
written down, because a TSS descriptor is 16 bytes and straddles two 8-byte
slots: a selector written by hand once pointed at a slot the script had left
null, and `ltr` on a null descriptor raises `#GP` — which, on a machine with no
IDT installed, is a triple fault and a silent reset.

## See also

- `docs/adr/0003-address-layout.md` — the decision that this layout is frozen
  and derived rather than hardcoded.
- `docs/adr/0004-x86-64-exception-frame.md` — the exception-frame contract
  this layout's `iretq` usage must not break.
- `docs/abi/syscalls-x86_64.md` — the syscall calling convention.
- `docs/abi/ipc.md` — the IPC semantics.
- `crates/nova-abi/src/layout.rs` — the ABI-side owner of the user layout
  constants.
- `crates/nova-abi/src/syscall.rs` — the syscall numbers and convention.
- `user/linker.ld` — the user-side half of this contract.
