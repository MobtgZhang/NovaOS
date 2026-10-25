# The NovaOS syscall calling convention (x86-64)

This document freezes how a ring-3 program calls into the kernel and how the
answer comes back. It is the half of the ABI that is not about *where* memory
lives; that half is in `docs/abi/address-layout.md`.

**Status:** frozen for M2, extended by M3/M4 as calls are added.

The numbers — syscall numbers, error codes, limits — belong to
`crates/nova-abi`. This document names the owning symbol for each of them and
does not repeat their values, because a value copied into prose is a value
that can drift from the code that enforces it.

## The shape of a call

```
ring 3                              ring 0
──────                              ──────
mov rax, <number>
mov rdi, <arg0>
mov rsi, <arg1>
mov rdx, <arg2>
mov r10, <arg3>
mov r8,  <arg4>
mov r9,  <arg5>
syscall                  ──────►    dispatch
                                  ◄──────  rax = result
```

| Register | Direction | Meaning |
| --- | --- | --- |
| `RAX` | in | the syscall number |
| `RDI` | in | argument 0 |
| `RSI` | in | argument 1 |
| `RDX` | in | argument 2 |
| `R10` | in | argument 3 |
| `R8` | in | argument 4 |
| `R9` | in | argument 5 |
| `RAX` | out | the result |
| `RCX` | out | clobbered: the return address |
| `R11` | out | clobbered: the saved `RFLAGS` |
| `RSP` | — | the user stack; the kernel switches to its own |

The omission of `RCX` and `R11` is not a NovaOS choice but a property of the
instruction: `syscall` loads the return address into `RCX` and the caller's
flags into `R11`, so neither can carry an argument. The argument registers are
therefore `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9` — the same order as the SysV
integer-argument convention, minus the two the instruction has taken.

The number lives in `nova_abi::syscall::nr`; each number is a
`nova_abi::Syscall`, a `#[repr(transparent)]` newtype over a `u32`, so it can
be passed in `RAX` without a wrapper and without a conversion at the boundary.
`nova_abi::nr::ALL` is the list of assigned numbers, and `nova_abi::nr::name`
gives a number a stable label for logs.

### The numbers are append-only

A syscall number is a cross-binary contract. A retired number stays reserved
rather than being reused, so a stale binary calling it receives
`E_UNSUPPORTED` instead of silently landing on a different operation.
`nova_abi::nr::name` returns `"reserved"` for an unassigned number precisely
so that the log says something a reader can act on.

The same discipline covers error codes (`nova_abi::ErrorCode`): never
renumbered, never reused, a retired code stays documented.

## Arguments

Arguments are six unsigned words, in the register order above. There is no
"argument count" field: a call that uses fewer arguments leaves the rest
undefined, and a call that wants more has no way to say so. Every number's
documented argument list is therefore exhaustive.

The kernel's entry path reads the arguments out of the saved register file:
`kernel/src/syscall/mod.rs::UserRegisters::args` returns them in register order.
`nova_core::syscall::SyscallArgs` names the same six slots individually, for
the parts of the dispatch policy that are written against the core crate
rather than against the kernel's own register save area —
`nova_core::syscall::dispatch_stateless` is the pure form of the dispatcher
that the host tests exercise.

### User pointers

A syscall argument that names a buffer is a **user virtual address**, never a
physical one and never a kernel address. The kernel does not dereference it
directly. Two rules make that safe:

1. The kernel translates the pointer through the *calling thread's* address
   space and copies the bytes out through the direct map, page by page. A
   pointer that is not mapped in the caller's space ends the copy, and the
   call reports what it managed to copy rather than reading whatever happens
   to be at that physical address.
2. The length is clamped to a kernel-owned maximum before the copy begins, so
   the cost of a call is a function of the kernel's constant rather than of a
   number the caller chose.

The reason is that only the entry path knows which address space the caller
had. The dispatch logic is therefore written against bytes the kernel already
owns: the entry path stages the caller's message and payload into the
kernel's bounded buffers before dispatching, and stages the received message
and reply token back out afterwards.

`nova_abi::nr::WRITE` is the one call whose argument is a user pointer and
whose handling lives in the entry path rather than in the service layer, for
exactly this reason. The service layer's dispatcher never dereferences a user
address.

## Results and errors

A syscall's result travels in one register, `RAX`, interpreted as a signed
64-bit word:

* `RAX >= 0` — success; the value is the result.
* `RAX < 0` — failure; the value is `-E_*`.

There is deliberately no separate "success" flag. A flag and a value would
need two registers, and the single-register form is what makes the fast path
free. The consequence is that the sign is load-bearing, and the two places
that encode and decode it own the convention:

* `nova_core::syscall::SyscallResult::to_raw` encodes a result into the raw
  word: a value as itself, a failure as the negated code.
* `nova_abi::ErrorCode::from_result` decodes it: a non-negative word is
  success, a negative word is the error code.

An unknown negative value decodes as `E_UNSUPPORTED` rather than
`E_INVALID`. That is a deliberate reading: a syscall made against a newer ABI
than the kernel implements is a version mismatch, and reporting it as a bad
argument would blame the caller for a value the caller never chose.

`nova_abi::ErrorCode::is_retryable` marks the transient codes — the ones a
caller may usefully retry — and the rest are permanent. Being wrong there
costs latency rather than correctness, so it errs toward "retryable".

A user-space stub turns the raw word into a `Result`. The reference stub is
`user/nova-rt/src/syscall.rs`, whose `call` is the one place in the tree that
performs the instruction itself; the kernel's counterpart is
`kernel/src/syscall/mod.rs`.

### A note on the version word

`nova_abi::nr::GET_VERSION` returns the ABI version encoded as a single word,
`major << 16 | minor`. The careful comparison is
`nova_abi::AbiVersion::is_compatible_with`: the major version must match
exactly, and the kernel's minor version may be *newer* than the binary's — the
binary simply cannot use the newer calls and receives `E_UNSUPPORTED` for
them. The reverse is refused: a binary cannot discover the kernel's age, so an
older kernel is refused up front rather than discovered by a failed call
later.

`GET_VERSION` is the only call every user process may make without holding any
capability. A process that cannot identify its environment cannot report an
error usefully.

## The entry mechanism

### What `syscall` does by itself

`syscall` is not a call. It does not push a return address, it does not switch
stacks, and it does not consult the TSS. It:

* loads `CS` and `SS` from `STAR` — which is how the privilege level drops
  from ring 3 to ring 0;
* puts the address of the instruction *after* the `syscall` into `RCX`;
* puts the caller's `RFLAGS` into `R11`;
* clears the `RFLAGS` bits named by `SFMASK`.

Everything else — the kernel stack, the saved user registers, the return path
— is the kernel's job.

### The three-step entry

The entry stub in `kernel/src/syscall/mod.rs` does three things, in this order:

1. **Switch to the kernel stack.** The TSS's `RSP0` names the current thread's
   kernel stack. The CPU does *not* use it for `syscall` (it does, for
   interrupts and exceptions), so the stub does it by hand, by reading the TSS
   field directly — `privilege_stack_table[0]`, at offset 8, which the GDT
   asserts at compile time. The value is not a constant: the scheduler
   rewrites `RSP0` with `gdt::set_privilege_stack` on every context switch,
   so the stack the stub lands on is the one belonging to the thread that
   made the call.
2. **Save the user register file.** The kernel is about to run arbitrary Rust
   code, which clobbers every caller-saved register. The user's values have
   to be saved before the first call, or they are gone. The save area is the
   `UserRegisters` structure, whose field order is the stub's push order
   reversed; compile-time assertions pin the offsets the dispatcher and the
   result write depend on.
3. **Dispatch.** The dispatcher reads the number out of the saved `RAX` and
   the arguments out of the saved `RDI`..`R9`.

The user's stack pointer is stashed in a kernel-owned global during the
switch, because `syscall` does not touch `RSP` and the kernel stack replaces
it immediately. One slot suffices: the entry path is strictly serialised —
`SFMASK` clears `IF`, so no second entry can interleave.

### `SFMASK`

`kernel/src/syscall/mod.rs::SFMASK` names the `RFLAGS` bits cleared on entry.
Each one is there because of a specific hazard:

* `IF` — so that a syscall cannot be interrupted by another syscall before
  the register file is saved. Two nested entries would each save a register
  file, and the inner one's would overwrite the outer one's.
* `TF` — a trap flag would make the *first* instruction of the handler itself
  trap, producing an endless chain of `#DB`s.
* `DF` — the SysV ABI requires it clear, and a stray string direction would
  corrupt every string operation the kernel performs.
* `AC` — the kernel does not check alignment, so leaving it set would make
  the kernel's own misaligned accesses fault.

### `STAR`

`STAR` names the kernel and user segment selectors, and its value is *derived*
from the GDT's slot layout by `kernel/src/arch/x86_64/gdt.rs::star_value`
rather than written down. A descriptor that moves therefore cannot leave
`STAR` pointing at the wrong one.

`STAR`'s kernel half names a *pair*: the kernel code segment at `STAR[47:32]`
and the kernel data segment at `STAR[47:32] + 8`. That is why the kernel code
and data descriptors occupy adjacent GDT slots.

`EFER.SCE` must be set for the `syscall` instruction to exist at all. Without
it, executing `syscall` raises `#UD` — the same symptom as a missing entry
point, which is what makes the two mistakes hard to tell apart.

### The return path

The return is an `iretq` frame, not a `sysretq`. The frame is built on the
*kernel* stack and is laid out exactly as the CPU expects:

```
[rsp + 0x00]  RIP      the user's, from RCX
[rsp + 0x08]  CS       the ring 3 code selector
[rsp + 0x10]  RFLAGS   the user's, from R11
[rsp + 0x18]  RSP      the *user's* stack pointer
[rsp + 0x20]  SS       the ring 3 data selector
```

`RSP` in the frame is the user's stack pointer, not the kernel's: `iretq`
restores it verbatim, so the user returns to exactly the stack it was on.

**Why `iretq` and not `sysretq`.** `sysretq` is one instruction faster and has
a sharp edge: the value in `RCX` is checked against the kernel's canonical
addresses and a non-canonical one raises `#GP` *after* the stack switch is
partly set up, so a mistake there loses every scrap of context. `iretq`
restores the stack pointer from the frame and the segment selectors from the
frame, so the whole return state is explicit and checkable. v0.1.0 trades the
instruction for the ability to reason about it.

The user's `RFLAGS` in the frame is the value the CPU saved in `R11` — that
is, the flags the user had *before* `SFMASK` cleared `IF`. The user therefore
returns with interrupts enabled, exactly as it had them.

## Effects on the calling thread

Most syscalls return a value and leave the thread running. Three do not, and
the dispatcher reports which:

`nova_core::syscall::ThreadEffect` names the four cases — continue, yield,
exit, block. The value and the effect are separate types because they answer
different questions: the value goes in `RAX`, the effect is what the
scheduler has to do afterwards. Keeping them apart means the dispatcher does
not need to know about the scheduler.

A parked call returns `0` in `RAX`. The thread is not going to read it until
it is woken, and the woken path re-reads the completion.

## Per-call arguments

The argument lists below are the ones the ABI's `nr` module documents. The
numbers are owned by `nova_abi::nr`; this table names them so a reader can
find the contract without reading the crate.

| Number | Arguments (in register order) | Result |
| --- | --- | --- |
| `GET_VERSION` | none | the ABI version word |
| `OBJECT_CREATE` | object type, requested rights | a capability handle |
| `OBJECT_DESTROY` | handle | `0` |
| `CAP_DEREGATE` | parent handle, requested rights | a new, narrower handle |
| `CAP_REVOKE` | handle | the number of capabilities withdrawn |
| `SEND` | endpoint, opcode, payload pointer, payload length | `0` |
| `RECV` | endpoint | the opcode of the received message |
| `CALL` | endpoint, opcode, payload pointer, payload length | `0`, then the reply |
| `REPLY` | endpoint, reply token | `0` |
| `MAP` | address space, virtual address, length | — |
| `UNMAP` | address space, virtual address, length | — |
| `THREAD_SPAWN` | address space, entry, stack pointer | a thread identifier |
| `YIELD` | none | `0` |
| `THREAD_EXIT` | status | the status |
| `CANCEL_WAIT` | handle | — |
| `WRITE` | file descriptor, buffer pointer, length | the number of bytes written |

A number this kernel does not implement returns `E_UNSUPPORTED`. It never
falls through to another operation: a number reserved for a later milestone
ending up meaning something else is how a caller's request becomes a different
request without anybody deciding that it should.

> **Spot to re-check.** The argument lists above are the ABI's shape, as
> documented by `nova_abi::nr`. The kernel's dispatcher currently reads
> `arg0` as the endpoint handle and `arg1` as the opcode, and takes the
> message *body* from a message the entry path has already staged into a
> per-thread buffer (`kernel/src/services/dispatch.rs::send` and `::reply`
> read `KernelState::outgoing_message`; `::recv` writes the received message
> and its reply token into `KernelState::last_message` and
> `KernelState::last_token`). The ABI crate's `nr` documentation owns the
> argument list; if the entry path's staging is the real contract, the two
> need to be brought into agreement — in the crate and the dispatcher, not in
> this document. The same applies to the `RECV` result: the dispatcher
> returns the received message's opcode, and the message itself is left in the
> kernel-owned buffer rather than written back to a user pointer.

### The `WRITE` argument convention

`WRITE` takes a file descriptor, a user pointer to the bytes, and a length.
Only descriptors 1 and 2 are accepted, and both go to the kernel's serial
console — the only sink a user process has before the console service exists.
Anything else is `E_DENIED`.

The write is *unbuffered* from the caller's point of view: the bytes are in
the console by the time the call returns. That matters for a program that is
about to exit, where the last line must not be lost in a buffer that is never
flushed.

The kernel copies the caller's bytes out through the direct map, page by page,
because the caller's address space may not be the one loaded in CR3 while the
kernel is servicing the syscall — a kernel that dereferenced the user pointer
directly would fault on a perfectly valid user address.

The copy is also *bounded*: the length is clamped to a kernel-owned maximum
before the loop begins — the constant is `MAX_WRITE` in
`kernel/src/syscall/mod.rs::write_to_console`, not an ABI constant, because it
bounds one call's copy cost rather than anything two binaries must agree on.
A user program that claims a length of `u64::MAX` would otherwise make the
kernel walk an arbitrary amount of memory with no way to notice the claim was
absurd, and the cost of the call would be a function of the caller's word
rather than of the kernel's own constant.

If the buffer runs off the end of the caller's mapped memory, the copy stops
and the call reports how many bytes it *did* copy — a short write, rather
than a whole-call failure. The bytes already written are real, and reporting
them as a failure would make the caller retry them.

## The ABI crate is the single source of truth

Every number this document refers to lives in `crates/nova-abi`, which is
`no_std`, architecture-neutral and dependency-free. The kernel imports it, the
user-space runtime imports it, and the host-side test suite imports it — so
the syscall name in a panic message and the number the CPU dispatches on
cannot drift apart.

`nova_abi::ABI_VERSION` is the version the kernel reports from `GET_VERSION`,
and `nova_abi::AbiVersion` is the comparison a process makes against it.

## See also

- `docs/abi/address-layout.md` — the address layout this convention assumes.
- `docs/abi/ipc.md` — the IPC semantics built on top of these calls.
- `crates/nova-abi/src/syscall.rs` — the numbers and the convention's owner.
- `crates/nova-abi/src/error.rs` — the error-code table and the encoding.
- `crates/nova-core/src/syscall.rs` — the dispatch policy as pure logic.
- `kernel/src/syscall/mod.rs` — the entry stub and the `iretq` return.
- `user/nova-rt/src/syscall.rs` — the user-space side of the instruction.
