//! NovaOS kernel — a capability-secure microkernel.
//!
//! NovaOS is its own operating system, not a Linux derivative: it defines its
//! own syscall ABI, its own capability-based authority model, and its own IPC
//! semantics. NovaBoot — the UEFI boot manager and its snapshot protocol — is
//! the boot boundary, not an OS layer.
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
//! 4. Capture the boot manager's memory map into kernel-owned storage.
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
mod drivers;
mod initramfs;
mod loader;
mod logger;
mod mm;
mod sched;
mod services;
mod syscall;
mod task;
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

/// Kernel entry point.
///
/// The UEFI boot manager (`nova-boot-menu`) calls this after `ExitBootServices`
/// with the direct-map virtual address of the [`nova_boot::BootProtocol`]
/// snapshot in `RDI` (SysV order), running on a boot-manager-provided stack in
/// long mode with the boot manager's own page tables loaded in CR3.
///
/// The first job is to remember where the snapshot lives — `RDI` dies with the
/// frame — and then everything proceeds exactly as the boot sequence documents:
/// console first, kernel stack second, and the rest in
/// [`boot_after_stack_switch`].
///
/// # Safety
///
/// Reached only by the boot manager, which guarantees an initialised
/// environment: long mode, interrupts masked by the jump path, a valid address
/// space that maps the kernel image and the snapshot through the direct map,
/// and a stack the stub abandons immediately.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nova_boot_entry(snapshot: u64) -> ! {
    // The pointer is all this frame receives; store it before anything can
    // clobber `rdi`. SAFETY: the entry path is single-CPU with interrupts
    // masked, and this is the only writer.
    unsafe { boot::novaboot::store_snapshot_pointer(snapshot) };

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

        // Move off the boot manager's stack, and from here on never unwind
        // through the frame this function has already built on it.
        //
        // This ordering is load-bearing for two independent reasons, and both
        // have produced faults whose symptoms pointed elsewhere entirely.
        //
        // First, `arch::init` builds a 4 KiB IDT, fills a TSS, and reloads `CS`,
        // and until the IDT is installed a fault inside that work lands on the
        // boot manager's stack — the stack the faulting code had already pushed
        // past its end. The `#PF` handler then reports a frame with an `RIP`
        // inside the boot manager and an `RSP` in the direct map, which reads as
        // a stack-overflow in unrelated code.
        //
        // Second, and far less obviously, `nova_boot_entry` itself must not
        // return through the frame it has already opened. The switch below is
        // `-> !` precisely so that LLVM emits no epilogue for it; see
        // `arch::switch_to_kernel_stack_and_continue`, whose comment records the
        // `#PF` that came from getting this wrong — a faulting address inside the
        // stack buffer, blamed on a callee that returned perfectly.
        //
        // Nothing is lost by switching before the tables exist: `KERNEL_STACK`
        // is a `.bss` static, and the boot manager has already mapped the
        // kernel image's zero-filled pages, so it is writable before any table
        // is built. Loading `CS` is still the last thing `gdt::init` does.
        // SAFETY: interrupts are still masked. The function abandons this frame,
        // so nothing that lives on the boot manager's stack may be read afterwards.
        arch::switch_to_kernel_stack_and_continue();
    }
}

/// Continue the boot sequence on the kernel's own stack.
///
/// Split out of [`nova_boot_entry`] for one reason that is easy to miss:
/// `nova_boot_entry` has already built a frame on the boot manager's stack, and
/// moving `rsp` abandons it. If the rest of boot lived in the entry function,
/// its epilogue would eventually run and read that abandoned frame back through
/// the *new* stack pointer. Here there is no such epilogue — this function is
/// called fresh, with the stack already switched, and never returns.
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
        // Reject an unsupported boot protocol before touching hardware, so a
        // mismatch is a clean diagnostic rather than a machine that dies before
        // it can print anything.
        boot::check_protocol_revision();

        // Validates and copies the boot manager's snapshot into kernel-owned
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

    // §14.5.2's runtime layout is assembled before anything can open a path:
    // `RamFs` starts with only a root inode, and a file cannot be created
    // under a directory that does not exist. This is directory creation, not
    // an authorization — every task still needs a path capability before the
    // filesystem will let it read or write anything here.
    //
    // It runs before `drivers::init`, because that is where a disk is mounted
    // at `/media/…` and a mount needs its parent directory to exist.
    //
    // SAFETY: `services::init` above has published the state, nothing has
    // taken a borrow of it yet, and this is the boot path with interrupts
    // masked.
    unsafe {
        services::state().files.seed_default_layout();
    }

    // The M2 machinery: the syscall MSRs, the timer, and the scheduler.
    //
    // The order matters. The syscall MSRs need the GDT and TSS, which
    // `arch::init` installed above. The scheduler needs the thread pool,
    // which is a static and so needs no initialisation. The timer is armed
    // last, because a tick that arrives before the scheduler exists would
    // deliver IRQ0 into a handler that has nowhere to put the tick.
    //
    // SAFETY: this runs once, from the boot path, with interrupts masked and
    // before any reader of the state exists.
    unsafe {
        syscall::init();
        sched::init();
        arch::x86_64::apic::init(boot::read_boot_info().direct_map_offset);

        // The APs have been waiting on this gate since `boot::capture` handed
        // them over. Everything they need — the kernel address space, the shared
        // IDT, the per-CPU scheduler and stack tables, and the mapped APIC — now
        // exists, so they can enter their own startup paths.
        boot::smp::mark_ready();
    }

    // Devices, then the filesystems that sit on them. This runs after `mm`
    // (the drivers allocate frames for virtqueues and map BARs) and after
    // `services` (they mount into the file server), and before the init
    // process is started so that a user program's first `open` can already
    // see a disk.
    //
    // SAFETY: runs once, from the boot path, with interrupts masked, with the
    // frame allocator and the service state both published.
    unsafe { drivers::init() };

    if let Some(archive) = crate::boot::initramfs() {
        if crate::initramfs::Archive::parse(archive).is_ok() {
            if let Err(error) = crate::services::process::start_init_image(b"init") {
                klog!(Level::Error, "init: failed with error {}", error as i64);
            }
        }
    }

    // SAFETY: `boot::capture` ran above, so the snapshot is published and
    // immutable from here on.
    let boot_info = unsafe { boot::read_boot_info() };

    // The framebuffer is mapped here, after `mm::init` built and activated the
    // kernel's address space and after `services::init` published the display
    // service's state. `init` records the mode and logs it; a headless boot is
    // normal and leaves the display unavailable rather than failing.
    //
    // SAFETY: runs once from the boot path, with the address space live and no
    // draw call before it.
    unsafe { drivers::display::init(boot_info) };

    // SAFETY: the driver's mode is a boot fact; copying it into the service
    // state is a placement, not an allocation.
    unsafe {
        crate::services::state()
            .display
            .set_mode(crate::drivers::display::mode());
    }

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

    klog!(
        Level::Info,
        "smp: {} processors, bsp lapic id {}",
        boot_info.processor_count(),
        boot_info.bsp_lapic_id
    );
    for cpu in 1..boot_info.processor_count() {
        klog!(
            Level::Info,
            "smp: cpu {} lapic id {} online={} scheduler_threads={} ticks={}",
            cpu,
            boot_info.cpu_ids[cpu],
            boot::smp::is_online(cpu),
            boot::smp::scheduler_threads(cpu),
            boot::smp::tick_count(cpu)
        );
    }

    // Wait for the AP startup paths before the suites assert on them. This is a
    // bounded spin rather than an unbounded block: a machine whose APs fail to
    // start reports a failed SMP suite instead of hanging the boot forever.
    if !boot::smp::wait_for_aps(10_000_000) {
        klog!(
            Level::Error,
            "smp: application processors did not reach their startup path"
        );
    }

    // `run_all` is `-> !`: it terminates the machine via `arch::exit`, so there is
    // deliberately no epilogue here. An `arch::exit` fallback would be dead code
    // that the compiler correctly rejects to write, which is the better outcome —
    // it forces the harness's exit path to stay the single terminating branch.
    test::harness::run_all();
}

/// Continue an application processor on its own kernel stack.
///
/// # Safety
///
/// Reached only from [`crate::arch::x86_64::gdt::switch_to_kernel_stack_for_cpu_and_continue`],
/// with interrupts masked, after the bootstrap processor published `SMP_READY`.
/// The AP abandons the bootloader-provided frame and never unwinds it.
pub unsafe fn boot_after_ap_stack_switch(cpu: usize) -> ! {
    // SAFETY: delegated to this function's contract. The AP needs the kernel's
    // address space, its own tables, its own APIC timer, and its own scheduler.
    unsafe {
        crate::mm::address_space::activate(&crate::mm::address_space::kernel_space());
        crate::arch::init_cpu(cpu);
        crate::boot::smp::record_tss_rsp0(
            cpu,
            crate::arch::x86_64::gdt::privilege_stack_for_cpu(cpu),
        );
        crate::arch::x86_64::apic::init_for_cpu(
            cpu,
            crate::boot::read_boot_info().direct_map_offset,
        );
        crate::sched::init_for_cpu(cpu);

        let space = crate::mm::address_space::kernel_space();
        let entry = crate::boot::smp::ap_idle_entry as *const () as u64;
        let Some(mut thread) = crate::task::create_thread_for_cpu(
            cpu,
            crate::boot::smp::ap_thread_id(cpu),
            0,
            space,
            crate::sched::kernel_entry(entry),
        ) else {
            crate::klog!(
                Level::Error,
                "smp: cpu {cpu} could not create an idle thread"
            );
            crate::arch::exit(EXIT_PANIC)
        };

        crate::sched::spawn_for_cpu(cpu, &mut thread);
        crate::boot::smp::record_scheduler_threads(cpu, crate::sched::thread_count_for_cpu(cpu));
        crate::boot::smp::mark_online(cpu);
        crate::sched::switch_to_next_for_cpu(cpu);
    }
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
