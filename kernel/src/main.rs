//! NovaOS kernel — a capability-secure microkernel.
//!
//! NovaOS is its own operating system, not a Linux derivative: it defines its
//! own syscall ABI, its own capability-based authority model, and its own IPC
//! semantics. Limine is used strictly as a boot protocol, not as an OS layer.
//!
//! # Boot sequence
//!
//! The order below is the kernel's core invariant, and it is the *same* on every
//! supported architecture — each backend's `arch::init` handles the parts that
//! genuinely differ (GDT/TSS/IDT on x86-64, VBAR and the exception vector table on
//! AArch64), but the sequence itself is shared:
//!
//! 1. Bring up the architecture: exception tables first, so anything below can
//!    fault safely.
//! 2. Bring up the console, so a panic anywhere below is reportable.
//! 3. Validate the boot protocol, so a mismatch is a clean message rather than a
//!    machine that dies before it can print.
//! 4. Capture the bootloader's memory map into kernel-owned storage.
//! 5. Bring up physical memory, which needs the direct map from step 4.
//! 6. Run the test suites, which need everything above.
//!
//! Every step after 1 assumes a fault can be reported rather than fatal, which is
//! why step 1 is where it has to be. Step 2 is equally load-bearing and just as
//! easy to get wrong: the panic handler writes through the console, and a console
//! that has not been initialised silently *drops* what it is given, so a
//! protocol failure raised before step 2 exits on the panic status with an empty
//! serial log and no explanation at all.

#![no_std]
#![no_main]
// Kernel-internal invariants enforced at compile time. `main.rs` is the crate
// root, so these live here rather than in a lint table that a future module
// could opt out of.
#![deny(unsafe_op_in_unsafe_fn)]

mod arch;
mod boot;
mod logger;
mod mm;
// The service layer (M3 and M4): the capability tables, the object registry,
// the IPC plumbing, the file server, the supervisor, and the syscall
// dispatch. It is wired into the boot sequence below — `services::init` runs
// before the test suites, so the suites can create tasks and dispatch
// syscalls against real kernel state.
//
// The service layer lives outside `kernel/src/syscall/` on purpose: M2 owns
// that directory for the trap entry and the `iretq` trampoline, and two
// agents editing one `mod.rs` is a merge conflict waiting to happen. The
// integration point is `services::dispatch::syscall_dispatch`.
mod services;
mod test;

use crate::arch::capabilities::Console as _;
use crate::boot::BootInfo;
use crate::logger::Level;

/// Exit statuses shared across all targets. The mechanism for delivering them
/// differs per architecture (see `arch::shared::exit`), but the encoding the
/// harness sees is the same.
pub use crate::arch::shared::exit::{EXIT_PANIC, EXIT_SUCCESS, EXIT_TEST_FAILURE};

/// Kernel version, reported in the boot banner.
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Kernel name used in the boot banner and log tags.
pub const KERNEL_NAME: &str = "NovaOS";

/// Boot protocol revision this kernel requires.
pub const LIMINE_PROTOCOL_REVISION: u64 = 6;

/// Kernel entry point.
///
/// Limine jumps here in whatever mode it configured: long mode on x86-64, EL1h
/// with MMU on on AArch64. Both are described by `docs/abi/address-layout.md`.
///
/// # Safety
///
/// Reached only by the bootloader, which guarantees an initialised environment
/// (stack, console-visible state, and populated response areas for every request
/// the backend issued) and a valid address space.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    // SAFETY: this is the kernel's entry point; every call below is made with
    // interrupts masked and the exception table installed first where required.
    unsafe {
        // The console is programmed *first*, before anything else.
        //
        // Port I/O needs no segment registers in long mode, so this is safe to
        // run before `arch::init`, and running it first is what makes the rest
        // of the boot diagnosable. `arch::init` installs the GDT/TSS/IDT and
        // reloads `CS`; that is the most fault-prone code in the kernel, and a
        // fault there with no console yet is unreportable.
        //
        // `check_protocol_revision` and `capture` both `panic!` on a malformed
        // or unsupported boot protocol, and the panic handler prints through this
        // console. A console that has not been initialised yet *drops* those
        // bytes (see `ConsoleImpl::send_byte`), so running it afterwards turns a
        // precise diagnostic into a silent `EXIT_PANIC` with an empty log.
        arch::console_init();

        // Move off the bootloader's stack, and from here on never unwind through
        // the frame this function has already built on it.
        //
        // This ordering is load-bearing for two independent reasons, and both
        // have produced faults whose symptoms pointed elsewhere entirely.
        //
        // First, `arch::init` builds a 4 KiB IDT, fills a TSS, and reloads `CS`,
        // and until the IDT is installed a fault inside that work lands on the
        // bootloader's stack — the stack the faulting code had already pushed
        // past its end. The `#PF` handler then reports a frame with an `RIP`
        // inside the bootloader and an `RSP` in the direct map, which reads as
        // a stack-overflow in unrelated code.
        //
        // Second, and far less obviously, `_start` itself must not return
        // through the frame it has already opened. The switch below is `-> !`
        // precisely so that LLVM emits no epilogue for it; see
        // `arch::switch_to_kernel_stack_and_continue`, whose comment records the
        // `#PF` that came from getting this wrong — a faulting address inside the
        // stack buffer, blamed on a callee that returned perfectly.
        //
        // Nothing is lost by switching before the tables exist: `KERNEL_STACK`
        // is a `.bss` static, and the bootloader has already mapped the kernel
        // image's zero-filled pages, so it is writable before any table is
        // built. Loading `CS` is still the last thing `gdt::init` does.
        // SAFETY: interrupts are still masked. The function abandons this frame,
        // so nothing that lives on the bootloader's stack may be read afterwards.
        arch::switch_to_kernel_stack_and_continue();
    }
}

/// Continue the boot sequence on the kernel's own stack.
///
/// Split out of [`_start`] for one reason that is easy to miss: `_start` has
/// already built a frame on the bootloader's stack, and moving `rsp` abandons
/// it. If the rest of boot lived in `_start`, its epilogue would eventually run
/// and read that abandoned frame back through the *new* stack pointer. Here
/// there is no such epilogue — this function is called fresh, with the stack
/// already switched, and never returns.
///
/// # Safety
///
/// Must run after the console is up and on the kernel's own stack, with
/// interrupts still masked and the exception table installed.
pub unsafe fn boot_after_stack_switch() -> ! {
    // Order matters within the backend: the exception table must exist before
    // anything that can fault.
    unsafe { arch::init() };

    // Diagnostics: dump the GDT and IDT now that the console exists and the
    // kernel owns the stack the dump runs on.
    arch::dump_tables();

    logger::init();

    // Now that output exists, the checks that can panic are safe to run.
    // SAFETY: as above; every call runs with interrupts masked and the
    // exception table installed.
    unsafe {
        // Reject an unsupported bootloader before touching hardware, so a
        // mismatch is a clean diagnostic rather than a machine that dies before
        // it can print anything.
        boot::check_protocol_revision();

        // Validates and copies the bootloader responses into kernel-owned
        // storage. Nothing reads the protocol structures again after this.
        let boot_info: &'static BootInfo = boot::capture();

        mm::init(boot_info);
    }

    logger::print_banner();

    // The service layer's state is installed here, before anything can
    // dispatch a syscall. It is a fixed-size structure in a static, so this
    // is a placement rather than an allocation, and it must happen before the
    // test suites run — the suites create tasks and dispatch syscalls against
    // this state.
    //
    // SAFETY: this runs once, from the boot path, with interrupts masked and
    // before any reader of the state exists. See `services::init`.
    unsafe { services::init() };

    // SAFETY: `boot::capture` ran above, so the snapshot is published and
    // immutable from here on.
    let boot_info = unsafe { boot::read_boot_info() };

    klog!(
        Level::Info,
        "boot: {} {} protocol rev {}",
        boot::bootloader_name(boot_info),
        boot::bootloader_version(boot_info),
        boot_info.protocol_revision
    );
    klog!(
        Level::Info,
        "arch: {} page {} B",
        arch::arch_name(),
        arch::ADDRESS_SPACE.page_size
    );
    klog!(
        Level::Info,
        "memory: {} usable of {} mapped total",
        describe_bytes(boot_info.usable_bytes()),
        describe_bytes(boot_info.physical_limit())
    );

    // The ABI version is printed at boot so a serial log from a failing run can
    // be matched against the kernel that produced it. It comes from the shared
    // `nova-abi` crate rather than a kernel-local constant precisely so that the
    // number here cannot disagree with the one `nr::GET_VERSION` returns to user
    // space -- which is the disagreement that makes an ABI mismatch undiagnosable
    // after the fact.
    klog!(
        Level::Info,
        "abi: {}.{} ({} syscalls)",
        nova_abi::ABI_VERSION.major,
        nova_abi::ABI_VERSION.minor,
        nova_abi::nr::ALL.len()
    );

    // `run_all` is `-> !`: it terminates the machine via `arch::exit`, so there is
    // deliberately no epilogue here. An `arch::exit` fallback would be dead code
    // that the compiler correctly rejects to write, which is the better outcome —
    // it forces the harness's exit path to stay the single terminating branch.
    test::harness::run_all();
}

/// Render a byte count in binary units for the boot log.
pub fn describe_bytes(bytes: u64) -> ByteDescription {
    ByteDescription(bytes)
}

/// Formatted byte count, printed via `Display`.
pub struct ByteDescription(pub u64);

impl core::fmt::Display for ByteDescription {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const KIB: u64 = 1024;
        const MIB: u64 = KIB * 1024;
        const GIB: u64 = MIB * 1024;

        let bytes = self.0;
        if bytes >= GIB {
            write!(f, "{}.{:02} GiB", bytes / GIB, (bytes % GIB) * 100 / GIB)
        } else if bytes >= MIB {
            write!(f, "{}.{:02} MiB", bytes / MIB, (bytes % MIB) * 100 / MIB)
        } else if bytes >= KIB {
            write!(f, "{}.{:02} KiB", bytes / KIB, (bytes % KIB) * 100 / KIB)
        } else {
            write!(f, "{bytes} B")
        }
    }
}

/// Panic handler.
///
/// A panic means an unrecoverable invariant violation. Report it in full, then
/// halt with a distinct status so the test runner can tell a panic apart from an
/// ordinary assertion failure.
///
/// # Note on architecture
///
/// This runs on whatever machine the kernel booted on and must not depend on the
/// architecture beyond the facade: it writes through the console capability and
/// exits through the exit capability. If it ever needs `panic = "unwind"`
/// support per target, that belongs in the backend, not here.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // Write straight to the console: re-entering the level-filtered logger during a
    // panic risks recursing back into the fault that caused it. Formatting goes
    // into a bounded stack sink rather than a `String`, because there is no
    // allocator and a fault report must not be able to fault itself.
    let mut sink = arch::capabilities::ByteSink::<256>::new();
    logger::format_panic(&mut sink, info);

    let console = arch::console();
    console.send_bytes(sink.as_bytes());
    console.send_bytes(b"\r\n");

    // SAFETY: halting on panic is always acceptable.
    unsafe { arch::exit(EXIT_PANIC) }
}
