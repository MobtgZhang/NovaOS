# The NovaOS ABI

Three documents, one subject: the boundary between a NovaOS user program and
the kernel.

| Document | What it freezes |
| --- | --- |
| [address-layout.md](address-layout.md) | Where memory lives: the kernel half, the direct map, the user window, the stack and its guard page, and the W^X rule |
| [syscalls-x86_64.md](syscalls-x86_64.md) | How a ring-3 program calls into the kernel: register mapping, the error encoding, the entry and return mechanism |
| [ipc.md](ipc.md) | What a message is, how a capability travels with it, how a reply is authorised, and what happens when a peer dies |

## How the three fit together

The address layout says where a user program's memory is. The syscall
convention says how the program asks the kernel to act on it. The IPC
semantics say what the kernel does when one program talks to another.

They are separate because they change for different reasons and on different
schedules. A new syscall is a change to the convention document and to the
ABI crate. A new IPC operation is a change to the IPC document and to
`nova-ipc`. A change to where the stack lives is a change to the layout
document and to the linker script — and to both at once, because they must
agree.

## The rule the three documents share

**Nothing in the tree hardcodes a value that the ABI crate owns.**

Every number that two separately compiled binaries must agree on lives in
`crates/nova-abi`: the syscall numbers, the error codes, the capability
handle layout, the message header, the payload and capability limits, and the
user layout constants. The documents name the owning symbol for each of them
rather than repeating the value, because a value copied into prose is a value
that can drift from the code that enforces it.

The documents do say what the numbers *are*, where the number is a property
of the architecture rather than of the ABI — a page size, an address width, a
register's role. Those are not duplicated in the ABI crate precisely because
they are not ABI: they are properties of the target, and a document is the
right place to state them.

## Where the values actually live

| Value | Owner |
| --- | --- |
| syscall numbers | `nova_abi::nr` |
| error codes | `nova_abi::ErrorCode` |
| capability handle layout, rights | `nova_abi::cap` |
| message header, payload and capability limits | `nova_abi::ipc` |
| user stack and heap constants | `nova_abi::layout` |
| the ABI version | `nova_abi::ABI_VERSION` |
| the rendezvous state machine | `nova_ipc::channel` |
| the atomic capability transfer | `nova_ipc::transfer` |
| the capability table and its invariants | `nova_cap::CapabilityTable` |
| the address-space model and the W^X rule | `nova_core::address_space` |
| the user window's derivation | `nova_core::address_space::UserWindow` |
| the kernel-half boundary | `arch::ADDRESS_SPACE.kernel_half_start` |
| the direct-map offset | `boot::BootInfo::direct_map_offset`, published by `mm::direct_map::offset` |
| the kernel image's bases | the linker symbols `__kernel_vma_base`, `__kernel_phys_base` |

## Stability

The ABI crate's constants are **append-only**. Values are never renumbered and
never reused. A retired syscall or error code stays reserved and documented,
so an old binary calling it receives `E_UNSUPPORTED` rather than silently
reaching a different operation.

The ABI version is reported by `nova_abi::nr::GET_VERSION` and is compared
with `nova_abi::AbiVersion::is_compatible_with`: the major version must
match exactly, and the kernel's minor version may be newer than the binary's
— the binary simply cannot use the newer calls and gets `E_UNSUPPORTED` for
them. The reverse is refused up front, because a binary cannot discover the
kernel's age by calling anything.

## Relationship to the decision records

`docs/adr/0003-address-layout.md` records *why* the layout is derived rather
than hardcoded, and `docs/adr/0004-x86-64-exception-frame.md` records the
exception-frame contract that the layout's `iretq` usage must not break.
`docs/adr/0002-stable-vs-nightly.md` records the property that makes this
whole arrangement possible: the crate that owns these values is ordinary
stable Rust with no dependencies, so a toolchain change cannot move them.
