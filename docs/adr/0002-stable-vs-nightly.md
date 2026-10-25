# Nightly Rust, pinned

- Status: Accepted
- Date: 2026-10-22

## Context

The design report recommends locking a *stable* Rust toolchain, and adds that
when an unstable feature is genuinely needed, it should be the subject of its
own decision record rather than a quiet `#![feature(...)]` in a source file.

That recommendation was written before the reference environment was measured.
The measurement says something different, and the measurement wins: the
`x86_64-unknown-none` target is only available on nightly in this
environment, and the `limine` crate does not compile on stable at all.

The stable/nightly question is not really "how risky is nightly". It is
"what does the toolchain's churn cost, and where does it actually reach?". A
nightly toolchain is a moving target; the question is whether the movement
can reach the parts of the system where a change would be hard to diagnose.

## Decision

The whole workspace builds on **nightly**, pinned in `rust-toolchain.toml`
with a specific channel, and the reason is recorded rather than left to be
rediscovered.

Two things force it:

* **The bare-metal target.** `x86_64-unknown-none` is installed for nightly
  only. There is no stable channel in this environment that can produce a
  kernel at all.
* **The bootloader's binding.** The `limine` crate, version 0.6.5, uses
  `#![feature(ptr_metadata)]`. It fails to compile on stable. The
  bootloader binding is not optional: it is how the kernel receives its
  memory map.

A third reason is narrower and worth distinguishing from the first two,
because it is the one that is really a policy question: the user-space
runtime uses `#![feature(linkage)]` so that a runtime built with no user
program attached still links. That one is a genuine choice rather than a
constraint, and it is called out here because it is the reason the nightly
policy would otherwise be invisible.

### Why not "stable for the host, nightly for the kernel"

That split is the obvious middle position and it was rejected for a specific
reason rather than on principle: the *host test suite* is where the
interesting logic lives. The capability table, the IPC rendezvous, the
address-space model, the ELF parser and the scheduler are all written so that
the exact code the kernel runs can be exercised by `cargo test` on the host.
If the host tests ran on a different channel from the kernel, the tests would
be testing a different compiler's output than the one that ships.

The split would also split the *lints*. The project's quality gates — zero
`clippy` warnings, `fmt` clean — would have to be enforced twice, on two
channels, with two different lint sets, and the nightly channel's lints would
change without anyone changing the code.

### Why the pin is a pin

`rust-toolchain.toml` names the channel, the components and the target
explicitly. This is not a convenience:

* `rust-src` is needed for `core` on the bare-metal target.
* `rustfmt` and `clippy` are needed for the quality gates.
* `llvm-tools` is needed by the tooling.
* `profile = "minimal"` keeps the installed toolchain small, because the
  environment's disk is shared with the build outputs.

A toolchain that floats would let a nightly compiler change the kernel's
behaviour between two runs of the same source. That is the actual cost of
nightly, and it is managed by pinning rather than by wishing the churn away.

### Where the churn can actually reach

The honest answer is: it can reach the kernel's code generation, and it
cannot reach the ABI.

Everything two separately compiled binaries must agree on lives in
`crates/nova-abi`, which is `no_std`, architecture-neutral and
dependency-free. A compiler change that altered a calling convention, a
structure layout or an error code would be a change in that crate, and that
crate has no nightly features, no dependencies and no `unsafe`. The nightly
surface is confined to code generation in the kernel and in the user-space
runtime — code whose behaviour is covered by the boot smoke test, the fault
tests and the acceptance suites.

## Alternatives

**Stable everywhere, with a hand-written boot protocol binding.** Rejected.
The `limine` binding's unstable feature is `ptr_metadata`, which is the
mechanism for reading a dynamically-sized structure out of a fat pointer —
exactly what a boot protocol's response parsing needs. Reimplementing it means
maintaining a second, hand-written view of the protocol structures, which is
the kind of second source of truth this project has been bitten by before.

**Stable for the host, nightly for the kernel.** Rejected for the reason
given above: it splits the tests from the shipped code and splits the quality
gates.

**Nightly everywhere, unpinned.** Rejected. An unpinned nightly means a
compiler update can change the kernel's behaviour with no commit, no diff and
no review — which is precisely the failure mode a decision record is supposed
to prevent.

**Stable everywhere, on a toolchain that has the bare-metal target.** This is
the right long-term answer if it becomes available: the constraint is
environmental, not architectural. Revisit when the stable channel provides
`x86_64-unknown-none` and the bootloader's binding stabilises.

## Consequences

The kernel's build depends on a toolchain that upstream considers unstable.
Every toolchain upgrade is a deliberate act, and the pin makes it a diff
rather than a surprise.

The user-space runtime cannot be built on stable, which means the *entire*
workspace — including the SDK crates that will grow in M4 — inherits the
nightly requirement. That is a real cost, and it is the reason to keep
revisiting this decision rather than treating it as settled.

The upside is that the crates that must be exactly right are not exposed to
it: `nova-abi` is ordinary stable Rust with no dependencies, so the ABI's
stability does not rest on the toolchain's.

## Validation

* `rust-toolchain.toml` names the channel, the components and the target, so
  a clean machine installs the same toolchain the project was last verified
  against.
* `cargo clippy -D warnings` and `cargo fmt --check` run on the pinned
  channel, and CI uses the same pin.
* `cargo test --workspace` runs the host suites on the pinned channel, so the
  code under test is compiled by the same compiler that builds the kernel.

## Revisit When

* The stable channel provides `x86_64-unknown-none` and the `limine` crate's
  `ptr_metadata` use is stabilised — then the split becomes available and
  this decision should be re-examined rather than merely inherited.
* The user-space runtime's `linkage` feature is stabilised and the runtime
  can be linked without it, removing the last reason that is a choice rather
  than a constraint.
* A second nightly-only dependency appears, which would make nightly the
  plan rather than the accident.
