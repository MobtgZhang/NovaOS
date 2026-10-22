# NovaOS

**An experimental Rust-based desktop operating system featuring a lightweight microkernel, capability-based security, and modular user-space services.**

> 🚧 **Status: early design / pre-alpha.** NovaOS is not yet bootable and does not provide a desktop release. Features and milestones described below are planned unless explicitly marked as implemented.

NovaOS draws inspiration from Symbian OS's resource efficiency, client-server system services, and event-driven programming. It reimagines those ideas for modern 64-bit desktop computers, with an **independent Rust kernel** rather than a Linux-based distribution.

📖 **[Detailed design and implementation plan (中文)](NovaOS.md)**

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
    Apps["Native Desktop Applications"]
    SDK["Nova SDK · Async Runtime · UI Toolkit"]
    GUI["Desktop Shell · Window Compositor"]
    Services["User-Space Services: Files · Network · Audio · Devices"]
    IPC["Capability-Aware IPC · Shared Memory"]
    Kernel["NovaKernel: Threads · Memory · Scheduling · Capabilities"]
    HW["x86-64 Hardware / QEMU"]

    Apps --> SDK
    SDK --> GUI
    SDK --> Services
    GUI --> IPC
    Services --> IPC
    IPC --> Kernel
    Kernel --> HW
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

## Technology Stack

| Area | Initial choice |
| --- | --- |
| Main language | **Rust** (`no_std` in the kernel, plus minimal assembly where needed) |
| CPU architecture | **x86-64** first; AArch64 in the future |
| Development platform | **QEMU** with UEFI/OVMF |
| Boot protocol | **Limine** |
| Executable format | **ELF64** and a NovaOS-specific ABI |
| Initial device targets | Serial, timers, framebuffer, and VirtIO |
| Tooling | Cargo, QEMU, GDB, and a planned `xtask` workflow |

## Roadmap

All milestones are **planned**. They are engineering gates, not guaranteed delivery dates.

- [ ] **M0 — Foundation:** Repository, Rust workspace, architecture decisions, and QEMU test harness.
- [ ] **M1 — Boot:** UEFI boot, serial logging, exception handling, and memory-map validation.
- [ ] **M2 — Isolation:** Paging, user mode, processes, syscalls, threads, and preemptive scheduling.
- [ ] **M3 — Microkernel:** Capability tables, IPC, an `init` process, and an IPC echo service.
- [ ] **M4 — Command line:** Service Manager, RAMFS, File Server, console, and shell.
- [ ] **M5 — Storage:** PCI, VirtIO block devices, and VFS.
- [ ] **M6 — Desktop Alpha:** Input system, window compositor, terminal, editor, and file manager.
- [ ] **M7+ — Expansion:** Networking, SMP, package management, security hardening, and selected real hardware.

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
│   └── nova-async/       # Async runtime
├── user/
│   ├── init/             # First user-space process
│   ├── services/         # System services
│   └── apps/             # Native applications
├── drivers/              # Device drivers
├── boot/                 # Boot configuration
├── tools/xtask/          # Build and QEMU automation
├── tests/                # Unit and integration tests
├── docs/                 # ADRs and technical specifications
├── NovaOS.md             # Detailed design plan
└── README.md
```

This structure is a **target layout**; individual directories will be created as implementation progresses.

## Building and Running

There is **no supported build-and-boot workflow yet**. The following commands describe the *intended future* workflow; they will become available after the build tooling is implemented:

```bash
# Planned commands — not functional in the design-only stage
cargo xtask image
cargo xtask run
cargo xtask test-qemu
```

The build environment will require a pinned Rust toolchain, the x86-64 bare-metal target, QEMU, OVMF, and a compatible Limine version. Producing a bootable image requires packaging the kernel, bootloader, and initial user-space image; `cargo build` alone is not sufficient.

## Contributing

NovaOS welcomes future collaboration on Rust systems programming, operating-system architecture, documentation, drivers, and testing.

Useful early contributions include architecture reviews, ABI and IPC design feedback, QEMU automation, host-side unit tests, and focused Rust components. Before implementing a large subsystem, discuss its scope in a GitHub issue. New `unsafe` code should document its safety assumptions and include relevant tests.

## License

NovaOS is intended to be dual-licensed under either:

- [Apache License 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

**at your option** (`MIT OR Apache-2.0`).

Before publishing under these terms, add both full license texts with accurate copyright notices. Third-party dependencies and assets may use different licenses.

---

**NovaOS — Small kernel. Strong boundaries. Open possibilities.**
