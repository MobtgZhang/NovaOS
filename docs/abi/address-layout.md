# NovaOS virtual address layout

This document freezes the virtual address space layout. It is the contract
between the kernel, the user-space linker script (`user/linker.ld`), and every
user binary built against the ABI.

**Status:** frozen for M2 (v0.0.2).

## Why the layout is frozen and not merely described

A user binary and the kernel that loads it must agree *exactly* on where the
image goes, where the stack is, and where the kernel lives. If either side
derives an address differently — a different page size, a different half-open
interval convention — the result is not a crash but a process that runs and
misbehaves. Writing the layout down once, and having both sides reference it,
is the only way to make that disagreement impossible.

The constants below fall into three classes, and the distinction matters:

| Class | Where it comes from | Can it move? |
| --- | --- | --- |
| **Derived** | Computed from the architecture's address-space geometry or from the bootloader's answers | Yes — no code change, because nothing hardcodes it |
| **ABI** | Agreed with `user/linker.ld`; a shared constant | Only by changing both sides in the same commit |
| **Physical** | Where the firmware put the kernel image | No — the linker script fixes it |

## The kernel half

```
0xFFFF_8000_0000_0000  ─────────────────────────────  kernel half start
0xFFFF_8000_0000_0000  ┬ HHDM (higher-half direct map)
                       │   virtual = physical + direct_map_offset
0xFFFF_FFFF_8010_0000  ┬ kernel image (.text)
0xFFFF_FFFF_8012_9000  ┘ kernel image end
0xFFFF_FFFF_FFFF_FFFF  ─────────────────────────────  last canonical address
```

### Kernel half start — **derived**

`0xFFFF_8000_0000_0000` is *not* written into the kernel as a literal. It is
`arch::ADDRESS_SPACE.kernel_half_start()`, which is computed from the target's
declared virtual-address width (48 bits, LA57 off) by
`arch::shared::hw::AddressSpace::kernel_half_start`. The same value reaches the
ELF validator (`nova_elf::Elf::validate`) and the user-window derivation, so
"kernel space" means exactly one thing in the whole tree.

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
where the null page of a user address space cannot alias it).

The image spans, by section:

| Region | Linker symbols | Protection |
| --- | --- | --- |
| `.text` | `__text_start` .. `__text_end` | read + execute, **not writable** |
| `.rodata` | `__rodata_start` .. `__rodata_end` | read-only |
| `.data` | `__data_start` .. `__data_end` | read + write |
| `.bss` | `__bss_start` .. `__bss_end` | read + write, zero-filled |

The image end (`__image_end_vma`) is the frame allocator's floor: a frame
below it is still partly occupied by the running kernel.

## The user half

```
0x0000_0000_0000_0000  ┬ null page — never mapped
0x0000_0000_0000_1000  ┬ user window start
                       │   program image, heap, stack
0x0000_0000_7FFF_F000  ┬ user stack top (initial RSP)
0x0000_7FFF_FFFF_F000  ┘ user window end
0x0000_8000_0000_0000  ── first non-canonical address
```

### User window — **derived**

```
start = 0x1000
end   = kernel_half_start - page_size = 0x0000_7FFF_FFFF_F000
```

Both bounds are computed by `nova_core::address_space::UserWindow::derive`,
from the page size and the kernel-half start. The lower bound is the page
after the null page: address zero must fault, not resolve to the first page of
the process. The upper bound is one page below the kernel half, so a user
mapping can never share a page with the kernel's own.

The window is *half-open*: `start` is inclusive, `end` is exclusive. The last
byte a user mapping may occupy is `0x0000_7FFF_FFFF_EFFF`.

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

### User stack — **ABI constant, shared with `user/linker.ld`**

```
stack top (initial RSP)   0x0000_0000_7FFF_F000
stack size                64 KiB  (16 pages)
stack base                0x0000_0000_7FFF_F000 - 64 KiB
guard page                the page immediately below the stack base
```

`user/linker.ld` defines `USER_STACK_TOP = 0x000000007FFFF000`, and the
kernel's stack setup uses the same number. This is an **ABI** constant, not a
derived one: the user binary's linker script and the kernel must agree, and
neither computes the other. It is well inside the derived user window, so the
two rules do not conflict.

The **guard page** is the page immediately below the stack's lowest page. It
is deliberately left unmapped, so a stack overflow raises `#PF` at the guard
rather than silently corrupting whatever lies below the stack. The guard is
not a mapping — it is the *absence* of one — and `nova_core::address_space::GuardPage`
names the range so the kernel and the tests agree on which page it is.

### User heap — **ABI constant, shared with `user/linker.ld`**

`user/linker.ld` reserves a 1 MiB `.heap` region at `0x0000_0000_4000_0000`
(`NOLOAD`). The kernel does not map it eagerly; it is the region a user
program's allocator draws from once M3 provides `MAP`. Reserving it here keeps
two processes' heaps from overlapping.

## W^X

A user page is **never** both writable and executable. The rule is enforced in
the mapping API (`nova_core::address_space::Protection::check_map`), not by
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
writable, `.data` and `.bss` are not executable).

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

### The HHDM mapping

The kernel's own tables must map the HHDM, because the frame allocator
reaches physical memory through it and the loader writes into user frames
through it. The mapping covers exactly the firmware's *usable* regions — not
the whole physical range — so a process cannot be handed a frame the firmware
said was not RAM.

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

The entry instruction is `syscall`; the return instruction is `iretq`. See
`crates/nova-abi/src/syscall.rs` for the reasoning, and
`kernel/src/syscall/mod.rs` for the mechanism.

`iretq` is used rather than `sysretq` on purpose: `sysretq` checks the value in
`RCX` against the kernel's canonical addresses and raises `#GP` *after* the
stack switch is partly set up, so a mistake there loses all context. `iretq`
is one instruction slower and dramatically easier to reason about.

## Segment selectors

| Selector | Descriptor | RPL |
| --- | --- | --- |
| `0x08` | kernel code | 0 |
| `0x10` | kernel data | 0 |
| `0x18 \| 3` = `0x1B` | user code | 3 |
| `0x30 \| 3` = `0x33` | user data | 3 |
| `0x20` | TSS | 0 |

`STAR`'s kernel half must name a *pair* of descriptors: the kernel code
segment at `STAR[47:32]` and the kernel data segment at `STAR[47:32] + 8`.
That is why the kernel code and data descriptors occupy adjacent GDT slots
(`0x08` and `0x10`).

## See also

- `docs/adr/0004-x86-64-exception-frame.md` — the exception-frame contract
  this layout's `iretq` usage must not break.
- `crates/nova-abi/src/syscall.rs` — the syscall calling convention.
- `user/linker.ld` — the user-side half of this contract.
