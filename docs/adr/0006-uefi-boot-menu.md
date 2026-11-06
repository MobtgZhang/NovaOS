# 0006 — A UEFI boot menu layer over the NovaBoot unified boot protocol

## Context

NovaOS currently boots through Limine (0001): Limine loads the kernel ELF,
publishes its responses by exported symbol name, and transfers control to
`_start`. That path works and is what `make run` exercises, but it ties the
kernel to one bootloader and to Limine's symbol-based discovery.

The longer-term goal is a firmware-to-kernel contract that is the same on
every supported target (x86-64, AArch64, RISC-V 64, LoongArch 64): a boot
program chooses one menu entry, loads the selected kernel image, publishes a
bounded `BootProtocol` snapshot, and hands control to the kernel. The kernel
validates that snapshot before trusting any address or CPU identity.

This record covers the first piece of that contract: the protocol crate and a
UEFI boot-menu application. It deliberately does **not** yet cover handing
control to the kernel through this path.

## Decision

Implement the NovaBoot protocol and a UEFI boot menu as two crates, gated so
they cannot be mistaken for a shipped boot path:

**`crates/nova-boot`** — architecture-neutral protocol, `no_std`, allocation-free,
host-testable:

- `BootProtocol`: a fixed-layout snapshot (magic, protocol version, target
  architecture, kernel entry/load address, direct-map offset, initramfs, CPU
  table, memory map, framebuffer, selected entry) with a strict `validate()`.
- `BootArch`: `x86_64`, `aarch64`, `riscv64`, `loongarch64` — names four target
  families the protocol can carry; naming one does **not** mean its kernel
  backend exists.
- `BootMenu`: a bounded menu model (fixed entry count, bounded name/path,
  unique IDs, selection by stable ID).
- `menu_config`: a parser for a simple text menu config, pure `core`.

**`crates/nova-boot-menu`** — UEFI application, `x86_64-unknown-uefi` only:

- Renders a **graphical** menu through the UEFI GOP framebuffer (no text
  console): a centered panel with title bar, a highlighted selection bar, and a
  hint row, drawn with an embedded 5x7 bitmap font (glyphs verified against a
  rendered preview). Prefers 1024x768 and falls back to the best available mode.
- Two entries ship compiled in: `1. Desktop` (default) and `2. Safe Terminal`.
- Keyboard input is read through *every* `SimpleTextIn` handle the firmware
  exposes — OVMF surfaces several and which one is wired to the real keyboard
  varies, so waiting on all of them is the portable choice. Digits select,
  Enter picks the default, Escape aborts.
- Explicitly **does not** transfer control to the kernel: the kernel-side
  consumer (reading `BootProtocol` instead of Limine's symbols) does not exist
  yet, and jumping to the kernel's higher-half entry without the paging Limine
  sets up would fault.
- Verified under QEMU/OVMF: the menu draws into the GOP framebuffer (confirmed
  by monitor `screendump`), and a monitor-injected `sendkey 1` reaches the
  menu, selects Desktop, and the reboot path prints its status to serial.

This crate is built only for a UEFI target and is excluded from the host test
and clippy sweeps (`nova-test-host`, `nova-clippy`).

## Alternatives considered

- **Keep Limine only.** Simplest and already verified, but it does not progress
  toward a firmware-neutral boot contract or a visible OS-selection menu.
- **Write a full hand-written bootloader now.** Would duplicate Limine's
  paging/entry setup and is out of scope for this milestone; it also risks
  claiming unverified boot behavior.
- **Do the protocol crate and menu now, defer the kernel hand-off.** Chosen:
  the protocol and menu are concrete, host-testable and QEMU-verifiable, and
  the missing hand-off is explicitly documented rather than hidden.

## Consequences

- `make run` and `cargo xtask run` keep booting through Limine and are
  unchanged; they remain the only verified boot path.
- The UEFI menu renders graphically under QEMU/OVMF and accepts keyboard
  selection, but it halts after printing the selected entry's details. It does
  not start NovaOS.
- Four target families appear in the protocol model and the default menu, but
  only x86-64 has a kernel backend; the rest are placeholders.

## Reopening conditions

This decision is reopened when:

- the kernel gains a `BootProtocol` reader (replacing or complementing Limine's
  symbol-based capture) and a validated transfer of control at the entry point,
  or
- the menu is removed in favor of keeping Limine as the sole boot path.
