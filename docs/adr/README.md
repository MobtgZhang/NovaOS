# Architecture decision records

Each record captures one decision that was made, the reasoning behind it, and
what would change it. The records are written to be argued with: a decision
that has no recorded alternative is a decision that cannot be reviewed.

The records share a shape — the context, the decision, the alternatives, the
consequences, and the conditions that would reopen the question — but they are
not mechanical. **0003** is a decision about a rule. **0004** is largely a
*measurement*, and reads as one: it probes the machine, records what it found,
and then explains why the code's original assumption was wrong. Both are
records of the same kind of thing, and both are written to be argued with.

## The records

| ADR | Decision | Status |
| --- | --- | --- |
| [0001-boot.md](0001-boot.md) | Boot through Limine, pinned and checksum-verified | Accepted |
| [0002-stable-vs-nightly.md](0002-stable-vs-nightly.md) | Nightly Rust, pinned in `rust-toolchain.toml` | Accepted |
| [0003-address-layout.md](0003-address-layout.md) | A derived address layout, frozen in one document | Accepted |
| [0004-x86-64-exception-frame.md](0004-x86-64-exception-frame.md) | The measured x86-64 exception-frame layout, and the uniformised frame | Accepted |

## What each one is about

**0001** records the boot chain: why Limine rather than a hand-written loader
or GRUB, why the release is pinned and its checksum verified, and why the
kernel's protocol requests are discovered by exported symbol name rather than
by section. It also records the three boot failures that fail *silently*,
because a boot chain whose failures are silent has to be defended with
assertions rather than with diagnostics.

**0002** records the toolchain. The stable-versus-nightly question was
settled by measurement rather than by preference: the bare-metal target and
the bootloader's binding are nightly-only in this environment. It also
records why the split — stable for the host, nightly for the kernel — was
rejected: the host test suite runs the exact code the kernel runs, so
splitting the channels would split the tests from the shipped code.

**0003** records the address layout. The classification of every address as
*derived*, *ABI* or *physical* is the decision; the numbers are a consequence
of it. It also records why W^X is enforced in the mapping API rather than by
the caller, and why the kernel half of a process's page tables is copied
rather than duplicated.

**0004** records the x86-64 exception frame. It is the one record in the set
that is primarily *measurement*: the frame layout was probed on QEMU and
cross-checked against the emulator's own interrupt trace, because the code's
original assumption — that `RIP` is always the lowest word — is wrong for
every error-code-carrying vector. It is also the longest, because the
failure it exists to prevent produced symptoms that pointed nowhere near the
cause.

## Reading order

The records are not independent. 0001 and 0002 decide what the kernel is
built from and with; 0003 and 0004 decide what it looks like once running.
A reader new to the tree should read 0003 and 0004 first — they describe the
machine the kernel actually programs — and then 0001 and 0002, which explain
how it got here.

## Adding a record

The next number in sequence, a filename of the form `NNNN-<slug>.md`, and the
same structure as the existing four. A decision that supersedes an earlier
one gets a *new* record that names the one it replaces; the older record is
left in place with its status changed to `Superseded`. History is not
rewritten.
