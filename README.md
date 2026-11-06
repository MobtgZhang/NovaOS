# NovaOS

**An experimental Rust-based desktop operating system featuring a lightweight microkernel, capability-based security, and modular user-space services.**

> ✅ **Status: bootable in QEMU (M0–M8).** The kernel boots under UEFI/OVMF, runs its in-kernel suites, and reaches the implemented desktop, network, and SMP paths. It is still an experimental system, not a production desktop: IPv6, package management, audio, user accounts, and real-hardware support are not implemented. TCP, ICMP, DHCP, and a bounded socket API are implemented for the emulated QEMU network. SMP has a verified per-CPU foundation, but user processes, services, and drivers remain owned by the bootstrap processor, so concurrent service scheduling is not claimed. The roadmap below marks what is actually built.

NovaOS draws inspiration from Symbian OS's resource efficiency, client-server system services, and event-driven programming. It reimagines those ideas for modern 64-bit desktop computers, with an **independent Rust kernel** rather than a Linux-based distribution.

## Design Principles

- **Minimal kernel:** Keep scheduling, virtual memory, IPC, interrupt handling, and capability enforcement in the kernel.
- **Isolated system services:** Move file, network, device, audio, and display functionality into user-space processes where practical.
- **Capability-based security:** Grant access to specific objects through explicit, restricted handles instead of ambient authority.
- **Async-first programming:** Combine preemptive threads with event loops, efficient IPC, shared memory, and Rust `async`/`await`.
- **Resource efficiency:** Measure memory usage, latency, and idle overhead rather than relying on performance claims.
- **Desktop-focused evolution:** Start with a reliable command-line system, then build a modern graphical desktop.

These are engineering goals, not claims of formally verified security.

## Planned Architecture

```mermaid
flowchart TB
    APP["Applications: terminal / editor / file manager"]
    SDK["Nova SDK: async runtime / GUI toolkit / C ABI"]
    GUI["Desktop: shell / window compositor"]
    SYS["User-space servers: file / net / audio / device / security"]
    SUP["Service manager + capability broker"]
    IPC["Kernel IPC + capability validation"]
    K["NovaKernel: address spaces / threads / scheduler / IRQ / timers"]
    HW["Hardware or QEMU: CPU / RAM / PCI / display / storage"]

    APP --> SDK
    SDK --> GUI
    SDK --> SYS
    GUI --> IPC
    SYS --> IPC
    SUP --> IPC
    IPC --> K
    K --> HW
```

| Component | Responsibility |
| --- | --- |
| **NovaKernel** | Threads, processes, scheduling, address spaces, interrupts, and kernel objects |
| **Capability System** | Fine-grained permissions, handle validation, delegation, and revocation |
| **Nova IPC** | Endpoints, messages, request/reply, notifications, and shared buffers |
| **Service Manager** | Service startup, dependencies, supervision, and recovery |
| **File Server** | In-memory filesystem first; virtual filesystems and persistent storage later |
| **Device Manager** | Controlled MMIO, IRQ, PCI, and virtual device access |
| **Nova Desktop** | Display service, compositing, input routing, windows, and UI components |

## Boot workflow chain

```mermaid
sequenceDiagram
    participant B as UEFI + Limine
    participant K as NovaKernel
    participant I as init / ServiceManager
    participant F as FileServer
    participant C as Console
    B->>K: kernel ELF + memory map + modules
    K->>K: paging / interrupts / scheduler
    K->>I: start first user process + bootstrap caps
    I->>F: launch service + delegate caps
    F-->>I: ready / endpoint
    I->>C: start console
    C->>F: IPC read a file
    F-->>C: result
```

## Technology Stack

| Area | Initial choice |
| --- | --- |
| Main language | **Rust** (`no_std` in the kernel, plus minimal assembly where needed) |
| CPU architecture | **x86-64** first; AArch64 in the future |
| Development platform | **QEMU** with UEFI/OVMF |
| Boot protocol | **Limine** (`make run`, current); **NovaBoot** UEFI menu layer (protocol crate implemented, kernel hand-off planned) |
| Executable format | **ELF64** and a NovaOS-specific ABI |
| Initial device targets | Serial, timers, framebuffer, and VirtIO |
| Tooling | Cargo, `make`, QEMU, GDB, and the `xtask` workflow |

## Roadmap

All listed milestones are engineering gates. M0–M8 are implemented in the repository; the later ones are not.

- [x] **M0 — Foundation:** Repository, Rust workspace, architecture decisions, and QEMU test harness.
- [x] **M1 — Boot:** UEFI boot, serial logging, exception handling, and memory-map validation.
- [x] **M2 — Isolation:** Paging, user mode, processes, syscalls, threads, and preemptive scheduling.
- [x] **M3 — Microkernel:** Capability tables, IPC, an `init` process, and an IPC echo service.
- [x] **M4 — Command line:** Service Manager, RAMFS, File Server, console, and shell.
- [x] **M5 — Storage:** PCI, VirtIO block devices, and VFS.
- [x] **M6 — Desktop Alpha:** Input system, window compositor, terminal, editor, and file manager.
- [x] **M7 — Networking (phase 1):** VirtIO-net over QEMU user networking, ARP, IPv4, UDP, DNS, TCP, ICMP, DHCP, and a versioned bounded socket API. IPv6 and real-hardware networking are not implemented.
- [x] **M8 — SMP foundation:** per-CPU schedulers, thread pools and stacks, GDT/TSS pairs, local APIC timers, AP startup, and kernel-only AP idle threads. CPU identity is taken from CPUID leaf 1 `EBX[31:24]`. The bootstrap processor still owns user processes, services, and drivers; concurrent service/driver SMP scheduling is not claimed.
- [ ] **M9+ — Expansion:** concurrent multi-CPU service scheduling, package management, audio, user accounts, security hardening, and selected real hardware.

**First major architecture milestone:** two isolated Rust processes communicate through a capability-protected IPC endpoint.

See [NovaOS.md](NovaOS.md) for subsystem specifications, interface sketches, risks, testing strategy, and milestone acceptance criteria.

## Planned Repository Layout

```text
NovaOS/
├── kernel/               # Microkernel and architecture-specific code
├── crates/
│   ├── nova-abi/         # Shared syscall and ABI definitions
│   ├── nova-ipc/         # User-space IPC library
│   ├── nova-cap/         # Typed capability handles
│   ├── nova-boot/        # NovaBoot unified boot protocol (protocol snapshot + bounded boot menu)
│   ├── nova-boot-menu/   # UEFI application rendering the NovaBoot boot menu (x86_64-unknown-uefi)
│   ├── nova-net/         # Host-testable Ethernet, ARP, IPv4, UDP, DNS formats
│   ├── nova-protocols/   # Versioned service wire contracts
│   └── nova-async/       # Async runtime
├── user/
│   ├── init/             # First user-space process
│   ├── services/         # System services
│   └── apps/             # Native applications
├── boot/                 # Boot configuration
├── tools/xtask/          # Build and QEMU automation
├── docs/                 # ADRs and technical specifications
├── scripts/              # POSIX build/run helpers
├── Makefile              # POSIX make entry points
├── reports/NovaOS.md     # Detailed design plan
└── README.md
```

The layout above is the current tree for the implemented milestones; later directories are added as those milestones are implemented.

## Building and Running

NovaOS builds with a pinned nightly toolchain, the `x86_64-unknown-none` bare-metal target, QEMU, and OVMF. `cargo xtask` owns the build/image/run sequencing, and the `Makefile` exposes POSIX-style entry points over it.

```bash
# The POSIX build/run entry point: build the image and boot it in QEMU's GUI.
make run

# Headless boot for servers, SSH sessions, and CI-like environments.
make run-headless

# Build the kernel, the user runtime, the ISO, and the test disks.
make build

# The full gate: xtask tests, host unit tests, then the QEMU boot suite.
make test
```

The lower-level commands remain available:

```bash
# Fetch and verify the pinned Limine release (needed once per checkout)
cargo xtask fetch-limine

# Build the kernel and the user-space runtime for x86_64-unknown-none
cargo nova-build-bare

# Build a bootable ISO, then boot it under QEMU/OVMF with a serial console
cargo xtask image
cargo xtask run

# The full test suite: xtask's tests, the host unit tests, then the QEMU
# boot suite. Exits non-zero on a failure *or* a timeout.
cargo xtask test
```

`make run` is the interactive path: it opens QEMU's graphical window and mirrors the kernel's serial console to the terminal and to `target/novaos/serial-interactive.log`. `cargo xtask run` remains the automated path and is headless.

Both `Makefile`, `scripts/qemu.sh`, and `scripts/gates.sh` use POSIX conventions: `make` recipes, `#!/bin/sh`, no bash arrays, and no `[[ ]]` tests.

### M7 networking

The M7 network path is a bounded phase-one stack, not a general TCP/IP implementation:

- the kernel enumerates a `virtio-net-pci` device and negotiates `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`, and `VIRTIO_NET_F_STATUS`;
- the driver uses two virtqueues, a 12-byte VirtIO 1.0 net header, bounded Ethernet frames, ARP, IPv4, UDP, TCP, ICMP, DHCP, and a DNS A-record query;
- QEMU's `user` backend supplies the fixed `10.0.2.0/24` guest network: guest `10.0.2.15`, gateway `10.0.2.2`, DNS `10.0.2.3`;
- TCP handshake, request/response, and close are verified against a `guestfwd` endpoint at `10.0.2.100:80`;
- DHCP acquisition returns a bounded lease and acknowledgement from the emulated server;
- the network service protocol is versioned and bounds names, datagrams, socket payloads, ICMP payloads, and reply sizes; socket handles are owned by the caller's task slot.

Deliberately not implemented or not asserted: IPv6, a blocking socket API, concurrent service/driver SMP scheduling, arbitrary external-host TCP reachability, and real-hardware networking. Those limits are documented rather than hidden behind a passing suite.

### M8 SMP foundation

The SMP phase is a tested foundation, not a claim that the whole system is concurrently scheduled:

- Limine's MP handover captures the processor IDs; each AP waits on `SMP_READY` before entering its startup path.
- Each AP gets its own kernel stack, fault stack, GDT/TSS pair, local APIC timer, scheduler, thread pool, and one kernel-only idle thread.
- The bootstrap processor remains the only owner of user processes, services, and drivers. APs never enter ring 3 and never touch the network, file, or display service state.
- CPU identity is read from CPUID leaf 1 `EBX[31:24]`, not from the xAPIC ID register. On QEMU the ID register reads as zero for every processor, while CPUID reports the captured IDs.
- The APIC timer uses periodic mode; its handler acknowledges the interrupt on the delivering CPU and accounts that CPU's own scheduler tick.
- The timer's no-current-thread path restores the saved `r15..rax` register file before returning, rather than resuming through the frame alone.
- `make run` and `cargo xtask run` both boot with `-smp 4`; the `smp_cpu_state` suite checks processor capture, AP online state, separate idle pools, per-CPU stacks/TSS, and per-AP scheduler ticks.

### UEFI boot menu (NovaBoot protocol) — planned

A separate UEFI boot-menu layer is implemented on top of the [`nova-boot`] protocol crate. It is **not** yet wired to hand control to the kernel, so `make run` (Limine) remains the only verified boot path; this section documents the design boundary rather than a shipped feature.

- `crates/nova-boot` defines the architecture-neutral [`BootProtocol`] snapshot (magic, version, target architecture, kernel entry/load address, CPU table, memory map, framebuffer) and a bounded [`BootMenu`] model, all validated on the host.
- `crates/nova-boot-menu` is a UEFI application (`x86_64-unknown-uefi`) that reads a menu config from the ESP, renders the menu through the UEFI text console, navigates it with the keyboard, and on selection loads the chosen kernel ELF and prints its entry point.
- The menu covers four target families (`x86_64`, `aarch64`, `riscv64`, `loongarch64`) in the protocol model, but only x86-64 has a kernel backend today; the other entries are placeholders.
- The missing piece to actually boot through the menu is the kernel-side consumer: reading and validating the `BootProtocol` snapshot (instead of Limine's exported symbols) and transferring control at the entry point. That hand-off is deliberately not claimed here.

[`nova-boot`]: crates/nova-boot
[`BootProtocol`]: crates/nova-boot#the-snapshot-a-bootloader-publishes-to-the-kernel
[`BootMenu`]: crates/nova-boot#a-bounded-menu-model

### Quality gates

`scripts/gates.sh` runs the four gates the plan requires, in cheapest-first order:

```bash
scripts/gates.sh          # every gate
scripts/gates.sh fmt      # cargo fmt --all -- --check
scripts/gates.sh clippy   # cargo clippy -D warnings (host crates)
scripts/gates.sh test     # host unit tests
scripts/gates.sh kernel   # the bare-metal build
scripts/gates.sh boot     # the QEMU boot suite
```

The same commands are available individually:

| Command | What it does |
| --- | --- |
| `cargo nova-test-host` | The host unit tests, without the bare-metal binaries |
| `cargo nova-fmt` | `cargo fmt --all -- --check`, over the whole workspace |
| `cargo nova-clippy` | `cargo clippy -D warnings` over the host crates |
| `cargo nova-clippy-kernel` | The same, for the bare-metal binaries, on the bare-metal target |
| `cargo nova-build-bare` | The kernel and the user runtime, for `x86_64-unknown-none` |
| `cargo nova-build` | The kernel, for `x86_64-unknown-none` |

> **The kernel is not a host program.** `nova-kernel` and `nova-rt` are `no_std` / `no_main` bare-metal binaries with their own `_start` and their own linker scripts. Building either for the host fails with `unwinding panics are not supported without std` — an error that reads like a broken toolchain and is really a bare-metal binary being built for the wrong target. They are excluded from the host gates and built by `nova-build-bare` instead.

Every QEMU run writes its serial log to `target/novaos/run/serial.log` and archives a timestamped copy under `target/novaos/run/archive/`.

## Contributing

NovaOS welcomes future collaboration on Rust systems programming, operating-system architecture, documentation, drivers, and testing.

Useful early contributions include architecture reviews, ABI and IPC design feedback, QEMU automation, host-side unit tests, and focused Rust components. Before implementing a large subsystem, discuss its scope in a GitHub issue. New `unsafe` code should document its safety assumptions and include relevant tests.

## License

Apache License 2.0 

---

**NovaOS — Small kernel. Strong boundaries. Open possibilities.**
