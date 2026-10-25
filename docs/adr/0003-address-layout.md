# A derived address layout, frozen in one document

- Status: Accepted
- Date: 2026-10-22

## Context

The design report is explicit about this one:

> **Do not hardcode an "always correct" address constant inside the design
> document.** Write `docs/abi/address-layout.md` first, and freeze there the
> user interval, the kernel half, the guard page, the randomised mapping
> region and the MMIO virtual region, all coordinated with the bootloader and
> the HHDM. This is what prevents the mapping Limine returns from conflicting
> with the page-table layout we design ourselves.

The reason is a class of bug that does not announce itself. If the kernel and
the user-space linker script disagree about where the stack is, the result is
not a crash at boot — it is a process that runs and misbehaves. If the kernel
assumes a direct-map offset rather than asking the bootloader, the result is
not a fault on the first access — it is the kernel reading the wrong memory on
a different firmware.

So the layout has to be *agreed*, and it has to be *derived* rather than
guessed, and the two requirements pull against each other: an agreement needs
a document, and a derivation needs no document.

## Decision

The address layout is frozen in `docs/abi/address-layout.md`, and **every
address in the system is either derived or shared, never guessed.**

The classification has three kinds, and it is the classification — not the
numbers — that makes the document safe:

| Kind | Where it comes from | Can it move? |
| --- | --- | --- |
| **Derived** | the address-space geometry, or the bootloader's answers | yes, with no code change |
| **ABI** | agreed with `user/linker.ld` | only by changing both sides in one commit |
| **Physical** | where the firmware put the kernel image | no — the linker script fixes it |

### The derivation chain

The chain is a chain, and each link names the symbol that owns it:

1. **The kernel half's start** is derived from the target's declared virtual
   address width, by `arch::ADDRESS_SPACE.kernel_half_start()`. It is not
   written as a literal anywhere. A LA57 target would move the boundary and
   every dependent computation would follow.
2. **The direct-map offset** is whatever the bootloader reports in its
   `LIMINE_DIRECT_MAP` response, captured once at boot and published through
   `mm::direct_map::offset()`. This is the most important link in the chain:
   the offset is a property of the *bootloader's* address-space layout, not of
   NovaOS, and a kernel that assumed a particular value would read the wrong
   memory on a different firmware or a different Limine configuration.
3. **The kernel image's virtual and physical bases** come from linker
   symbols (`__kernel_vma_base`, `__kernel_phys_base`), not from constants. The
   image is loaded low so the bootloader can read it and linked high so it
   runs in the top canonical half.
4. **The user window's bounds** are derived from the page size and the
   address width. The lower bound is the page after the null page, so a null
   pointer faults rather than resolving to the process's own first page. The
   upper bound is the last canonical user page, so a user mapping can never
   share a page with the kernel's own.
5. **The user stack and heap** are ABI constants shared with
   `user/linker.ld`, because a linker script cannot read a Rust constant. The
   two halves agree by *kind* — both name the same value for the same thing —
   and the kernel checks the stack against the derived window before mapping
   it, so a disagreement shows up as a refusal to start rather than as a
   program running somewhere unexpected.

### Why the guard page is the *absence* of a mapping

The page below the user stack is deliberately not mapped. A guard page is not
a mapping with special permissions; it is a hole, and its presence is
inferred from the absence of one. Naming it (`nova_abi::layout::USER_GUARD_BASE`,
`nova_core::address_space::GuardPage`) is what lets the kernel and the tests
agree on which page it is.

### Why W^X is enforced in the mapping API

The rule — a user page is never both writable and executable — is checked in
`nova_core::address_space::AddressSpace::check_map`, not by the caller. The
caller is exactly the code that would get it wrong: an ELF loader has to
choose between "writable" and "executable" from a segment's flags, and a
`.text` that is writable is a shellcode target.

The rule is asymmetric on purpose. Kernel mappings may be both, because the
kernel is not the adversary. Only the *user* half is restricted.

A segment that is both writable and executable is **refused** by the loader
rather than split. Splitting it would need two mappings of the same physical
page with different protections, which x86-64 does not support, so refusing is
the only honest answer.

### The kernel half is copied, not duplicated

An x86-64 address space is one PML4, and there is no "kernel address space"
register. The only way to have the kernel's mappings visible in a user
process is to put them in the same PML4, which means copying the kernel
half's *top-level entries* into every process's PML4. The entries point at
the *same* intermediate tables, so the kernel's mappings — and the direct map
— cost one page table's worth of memory in total rather than one per process.

The user half is private, which is what makes two processes unable to see
each other's pages even at the same virtual address.

### The kernel's own tables are copied from the bootloader's

The kernel does not re-derive the kernel image's mapping from scratch. It
copies the bootloader's tables, because the bootloader already mapped the
image, the direct map and the boot stack, and reproducing that mapping by hand
would mean hardcoding addresses the bootloader chose.

## Alternatives

**Hardcode the layout, document it, and move on.** This is the common
approach and it is what the plan warns against. It works until the firmware
changes, and it fails in the worst way: the kernel reads plausible addresses
and the failure surfaces somewhere unrelated. The direct-map offset is the
clearest case — it is entirely the bootloader's business, and a hardcoded
value is a guess about another program's address-space layout.

**Derive everything, document nothing.** Rejected. A derivation that nobody
has written down cannot be checked by a reader, and the one thing the
document gives is the ability to state the *rule* rather than the *number* —
"the user window ends one page below the kernel half" is checkable against the
code; the number is not.

**Let the bootloader own the whole layout.** Rejected. The bootloader knows
where physical memory is; it does not know where a NovaOS user program's
stack should be, and it certainly does not know that a guard page must
follow it. The user half is a NovaOS decision that the bootloader has no
opinion about.

**Randomise the user half per process.** Deferred. The design report asks for
a randomised mapping region, and this milestone does not provide one. It is
recorded here as *not* decided rather than as decided against: the user
window is currently a fixed range, and a per-process randomised base is a
change to the derivation, not to the document's structure.

## Consequences

The layout is now the contract between three things that must agree exactly:
the kernel, the user-space linker script, and every user binary built against
the ABI. A disagreement between the kernel and the linker script is a program
that runs and misbehaves, which is exactly the failure this document exists
to make impossible.

The cost is that a reader who wants to know "where is the user stack" must
follow a derivation rather than read a number. That is the intended cost: the
derivation is the thing that stays true, and the number is a consequence of it.

The classification has a second benefit that was not the point but matters: it
makes a *disagreement* visible. If a value that should be derived is written
down, there are now two sources of truth and the classification says which one
is wrong. A value that should be shared between the kernel and the linker
script but is written down in only one place is now visibly an ABI
inconsistency rather than an invisible one.

## Validation

* Host-side tests in `nova-core` exercise the user-window derivation and the
  W^X check without a privilege transition, so the two rules the document
  states are tested against the same code the kernel runs.
* The ELF validator is given the *derived* kernel-half start rather than a
  literal, so a segment that reaches into the kernel half is refused for the
  same reason on every target.
* The acceptance tests `user_cannot_write_kernel` and
  `task_a_cannot_read_task_b` check the properties the layout exists to
  provide: a kernel-half address is not mapped in a user space, and one
  process's pages are invisible to another.
* `invalid_elf_rejected` covers a segment outside the user window.
* The frame allocator's floor is computed from the linker symbols rather than
  a constant, so a change to the load address needs no code change.

## Revisit When

* A per-process randomised user base is introduced — that changes the
  derivation, not the document's structure.
* The MMIO virtual region, or the mapping-randomisation region the design
  report asks for, gets a concrete shape and belongs in this document.
* A second architecture enters the tree and needs a different user window.
  The derivation is written to follow, but the document's example numbers will
  no longer describe the shipped configuration and must be re-derived.
* The direct map is replaced by something else — a different bootloader
  protocol, or a kernel-built direct map. The chain's second link changes,
  and the document must change with it.
