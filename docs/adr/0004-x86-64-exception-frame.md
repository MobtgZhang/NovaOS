# x86-64 Exception Frame — Verified Layout

Measured on QEMU (`-cpu qemu64`, x86_64-unknown-none, nightly 1.99) by probing
the words adjacent to the CPU-pushed frame at the entry of the IDT dispatch
path. Reproduce with `cargo xtask image` + the probe in `nova_kernel_idt_dispatch`.

## The measurement

`frame` is the address of `RSP` as the stub captured it. Words at
`frame + n*8`, for `n = -1 ..= 6`:

```
v=0x0  #DE   [-1]=0xffffffff8010c378  [0]=0xffffffff8010000d  [1]=0x8  [2]=0x10046  [3]=0xffffffff80125b60  [4]=0x10  [5]=0xffffffff8010367e  [6]=0x15
v=0x6  #UD   [-1]=0x367d01             [0]=0xffffffff8010367e  [1]=0x8  [2]=0x10046  [3]=0xffffffff80125b68  [4]=0x10  [5]=0xffffffff8010367e  [6]=0x15
v=0xe  #PF   [-1]=0xffffffffffffff01   [0]=0x10                [1]=0x15 [2]=0x8      [3]=0x10046            [4]=0xffffffff80125b70  [5]=0x10  [6]=0xffffffff801072c7
v=0xd  #GP   [-1]=0x367d01             [0]=0x14                [1]=0xffffffff801000cf [2]=0x8 [3]=0x10002      [4]=0xffffffff80125b40  [5]=0x10  [6]=0xffffffff80100014
```

Cross-checked against QEMU's own `-d int` trace, which reports
`IP=0008:ffffffff8010000d` for the `#DE` and `pc=0000000000000015` for the
`#PF` — i.e. QEMU's `pc` equals word `[1]` for `#PF`, confirming the `#PF`
frame's word `[1]` is RIP.

## The layout

Reading from the **lowest** address upward, which is the direction `iretq`
consumes the frame:

| word | no error code (`#DE`, `#UD`) | with error code (`#PF`, `#GP`) |
| --- | --- | --- |
| `[0]` | RIP | **error code** |
| `[1]` | CS | RIP |
| `[2]` | RFLAGS | CS |
| `[3]` | RSP | RFLAGS |
| `[4]` | SS | RSP |
| `[5]` | (caller's stack) | SS |

So the error code, when present, is at the **lowest** address of the frame —
`RSP` on entry — and the five unconditional words sit *above* it.

## Why the original code was wrong in two independent ways

1. `ExceptionFrame` assumed `RIP` was always word `[0]`. For the
   error-code-carrying vectors that reads the error code as RIP, and the
   following words shift by one.
2. `Exception::has_error_code` listed `#DE`, `#OF` and `#BR` as
   error-code-carrying. They do not push one. The Intel SDM's list is
   `#DF`, `#TS`, `#NP`, `#SS`, `#GP`, `#PF`, `#AC`, `#VE`, `#CP`.

The measured `#DE` frame has RIP at word `[0]`, so the original code's
assumption happened to hold for `#DE` — but only because it *also*
assumed `#DE` pushed an error code, which made it read a word that was
never part of the frame.

## The chosen design: uniformise the frame in the stub

The stub pushes a zero placeholder for the vectors that push no error code,
so every vector presents the same six-word frame to the handler:

```
[0] error code   (CPU-pushed, or the zero placeholder)
[1] RIP
[2] CS
[3] RFLAGS
[4] RSP
[5] SS
```

This puts RIP at a **fixed** offset of 8 for every vector, which makes the
resume trampoline vector-independent: `add rsp, 8; mov [rsp], r12; iretq`.

## Stack alignment

The CPU aligns `RSP` to 16 bytes before pushing its frame, then pushes 5
words (no error code) or 6 words (error code). With the placeholder the
frame is always 6 words, so `RSP` on entry is always `aligned16 - 48`,
i.e. **0 mod 16**.

The SysV ABI requires `RSP ≡ 8 (mod 16)` at a function's first instruction,
which is what a `call` produces. The stub therefore uses `call`, not `jmp`,
to enter the dispatcher: the pushed return address supplies the missing
8 bytes. (`jmp` leaves `RSP ≡ 0 (mod 16)` and the callee's prologue then
misaligns every SSE spill.)

The return address is pushed *below* the frame, so it does not disturb it,
and the resume path discards it by resetting `RSP` to the frame pointer.

## Resume arithmetic

`iretq` pops RIP, CS, RFLAGS, RSP, SS — five words starting at `frame+8`.

* no-error-code vector: the stub's placeholder occupies `[0]`, the CPU's
  five words occupy `[1..5]`. `RSP` at fault time was `orig - 48`;
  after `iretq` it is `orig`. Balanced.
* error-code vector: the CPU's six words occupy `[0..5]`. `RSP` at fault
  time was `orig - 48`; after `iretq` it is `orig`. Balanced.

Both are `orig - 48`, so the arithmetic is identical for every vector.

## AMD `#UD` divergence (not implemented — documented)

AMD's APM specifies that `#UD` pushes an error code holding the opcode
table index, where the Intel SDM specifies none. QEMU's `qemu64` follows
the Intel behaviour (measured above: word `[0]` of the `#UD` frame is RIP,
not a table id).

NovaOS implements the Intel SDM's behaviour. On an AMD-family CPU the `#UD`
frame would have one extra word at the bottom, so `RIP` would be at word
`[2]` instead of `[1]`. The fix is localised: the stub for vector 6 must
skip the placeholder push and the dispatcher must select the `RIP` offset
per vector instead of assuming 8.

## Harness bug found alongside this one

`provoke_divide_by_zero` entered the faulting function with

```rust
core::arch::asm!("call {entry}", entry = in(reg) entry, options(noreturn));
```

`options(noreturn)` tells the compiler the call never returns, so it emits
a `ud2` immediately after it. The resume target is a naked `ret`, which pops
the `call`'s return address — which is that `ud2`. The machine therefore
re-entered `ud2` and raised a second, spurious `#UD`.

That is the `#UD` at `0xffffffff8010367e` in the trace: not a deliberate
test, but the compiler's unreachable marker being executed by the resume.
`provoke_invalid_opcode` already used `options(nostack)` and was correct.

## The resume trampoline's off-by-one (the `#PF` in `core::fmt::write`)

The frame above is only half the contract. The stub also saves the fifteen
general-purpose registers *below* the frame, and the resume has to put them
back exactly where they were.

The stub's layout, from the frame base downwards, is:

```text
[rbx + 0x00] .. [rbx + 0x28]   the six-word frame
[rbx - 0x08] .. [rbx - 0x78]   the fifteen saved registers (r15 lowest)
[rbx - 0x80]                    the stub's eight-byte alignment pad
```

The trampoline reaches the save area with `sub rsp, 120` — fifteen words —
and the fifteen `pop`s then restore the registers in reverse push order.
`iretq` starts at `frame + 8`, one word above the frame base, because the
error code occupies the frame's first word.

The bug this record exists for is that the code said `sub rsp, 128` and
`add rsp, 16`. Both numbers are off by one word, and they are off in
*opposite* directions, which is why the failure is so hard to read:

* `sub rsp, 128` lands on the alignment pad, which sits *below* `r15`. Every
  `pop` then reads one word too low: `r15` receives the pad's stale bytes,
  and every other register receives its neighbour's value. The register file
  ends up shifted by one word.
* `add rsp, 16` skips the error code *and* one word too many — but the
  extra word it skips is precisely the one the misplaced `sub rsp` left
  unaccounted for.

The two errors therefore cancel in the *stack position* and nowhere else.
`iretq` still lands on the right place, control still transfers to the
resume target, and the machine looks fine. But the register file is
corrupted, so the resumed code reads a pointer it never wrote and
dereferences it.

## Why the symptom pointed somewhere else entirely

The corruption is only visible in whatever the resumed code does *next*.
The fault tests resume into `divide_by_zero_resume`, whose `ret` returns
into the harness's own frame, and the harness then formats a report. The
report is the first thing that dereferences a pointer the compiler stored
in a register — and the register was one word off, so the vtable pointer
read as zero.

The result was a `#PF` at `core::fmt::write`'s `mov (%rbx,%rax,1),%rdi`
with `rbx = 0`, several frames away from the exception that actually went
wrong. The `#DE` that preceded it was reported perfectly, so the log read
as "the handler diagnosed the divide correctly, then something unrelated
faulted".

The diagnostic rule that falls out of this: **a fault whose faulting
address is zero, in a function that did not dereference a null pointer, is
evidence of register corruption rather than of a null pointer.** The
offending code did not have a null pointer to dereference; it had a
pointer that had been stored one slot to the left.

## The `#UD` error code is zero on this CPU

The harness originally asserted that `#UD` reports opcode table id `1`,
which is AMD's documented behaviour. The measurement above contradicts it:
on QEMU's `qemu64` (Intel-family), the word at the bottom of the `#UD`
frame is `RIP`, not a table id — `#UD` pushes no error code.

The test was asserting a premise the platform does not satisfy. It now
asserts that a vector pushing no error code reports `0`, which is the
property that actually holds and is the one that catches a frame read at
the wrong displacement.

## The resume restores the *interrupted* register file, not the compiler's

The frame layout above is not the only contract. The stub saves fifteen
general-purpose registers and the trampoline restores them — so after a
recoverable fault, the register file is the one that existed **at the
moment of the fault**, not the one the compiler had before it called into
the faulting code.

That distinction is invisible for callee-saved registers: the stub saves
them and the trampoline puts them back unchanged, so a value the compiler
parked in `RBX` survives the fault intact. It is decisive for
caller-saved ones. `divide_by_zero` sets `RCX` to zero before its `idiv`,
so the restored `RCX` is zero — whatever the compiler had there before is
gone, with no diagnostic and no trap.

The harness's `asm!("call {entry}", ...)` block declared **no clobbers**,
so LLVM was free to keep a live value in a caller-saved register across
it. `faults_are_repeatable` did exactly that: it held the first fault's
`ip` live in `RCX` across the second provoke. The restored `RCX` was
zero, so `first.ip != second.ip` compared `0` against a real instruction
address and reported a spurious difference.

The failure was **layout-sensitive**, which is what made it so hard to
read. Which register LLVM chose for `first.ip` depended on the surrounding
code layout, so the bug appeared in some builds and not others, and any
unrelated edit — including `cargo fmt` — could hide it. The right
diagnostic was therefore to watch the register at the comparison, not the
fault record: the record was correct, and the *caller's* copy of it was
not.

The fix is to state the truth about the block. `clobber_abi("C")` tells
the compiler that the block destroys every caller-saved register, exactly
as a real `call` would. The value must then be spilled or held in a
callee-saved register — and callee-saved registers *are* preserved across
the fault, because the stub saves them and the trampoline restores them
unchanged.

The general rule this record exists for: **an inline-assembly block that
can fault and resume is not an ordinary `call`.** Its register effects
are the register effects of the *faulting code*, not of the block, and the
compiler must be told so.

