//! Secondary CPU bootstrap and interprocessor signalling on the QEMU ports.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::capabilities::{ByteSink, Console, InterruptControl};
use crate::arch_bringup::{EarlyFrames, RemoteMutex, run_check, run_resident};
use crate::sync::SpinLock;

pub(crate) const MAX_CPUS: usize = 4;
#[cfg(target_arch = "aarch64")]
pub(crate) const WAKE_SGI: u32 = 1;
pub(super) const STACK_SIZE: usize = 64 * 1024;
const ROUNDS: usize = 256;
const WAIT_LIMIT: usize = 8_000_000;

#[repr(align(16))]
struct ApStacks([[u8; STACK_SIZE]; MAX_CPUS - 1]);

static mut STACKS: ApStacks = ApStacks([[0; STACK_SIZE]; MAX_CPUS - 1]);
#[unsafe(no_mangle)]
static NOVA_AP_STACK_TOP: AtomicUsize = AtomicUsize::new(0);
static BOOT_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static ONLINE: AtomicUsize = AtomicUsize::new(usize::MAX);
static CONTENDED: AtomicUsize = AtomicUsize::new(0);
static FINISHED: AtomicUsize = AtomicUsize::new(0);
static COUNTER: SpinLock<usize> = SpinLock::new(0);
static CPU_IDS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];
enum Work {
    Check(EarlyFrames, &'static [u8], Option<RemoteMutex>),
    Resident(EarlyFrames, &'static [u8]),
}
static CHECKS: [SpinLock<Option<Work>>; MAX_CPUS] = [const { SpinLock::new(None) }; MAX_CPUS];
static RESULTS: [SpinLock<Option<Result<(), &'static str>>>; MAX_CPUS] =
    [const { SpinLock::new(None) }; MAX_CPUS];

unsafe extern "C" {
    fn nova_virt_ap_entry();
}

pub(crate) fn verify(ids: &[u64]) -> Result<(), &'static str> {
    let boot = cpu_id();
    if ids.is_empty() || !ids.contains(&(boot as u64)) || ids.len() > MAX_CPUS {
        return Err("invalid or unsupported firmware CPU topology");
    }
    BOOT_CPU.store(boot, Ordering::Release);
    for (slot, &id) in ids.iter().enumerate() {
        CPU_IDS[slot].store(id as usize, Ordering::Release);
    }
    init_ipi();
    {
        let _guard = COUNTER.lock();
        if !super::interrupts().is_masked() {
            return Err("spin lock did not mask interrupts");
        }
    }
    if !super::interrupts().is_masked() {
        return Err("spin lock did not restore the masked boot state");
    }

    for (slot, id) in ids.iter().copied().filter(|id| *id as usize != boot).enumerate() {
        let stack = unsafe { (&raw mut STACKS.0).cast::<[u8; STACK_SIZE]>().add(slot + 1) };
        NOVA_AP_STACK_TOP.store(stack as usize, Ordering::Release);
        start(id as usize, stack as usize)?;
        wait(|| ONLINE.load(Ordering::Acquire) == id as usize, "secondary CPU failed to reach kernel")?;
    }

    let peers = ids.len() - 1;
    if peers != 0 {
        let held = COUNTER.lock();
        for id in ids.iter().copied().filter(|id| *id as usize != boot) {
            send_ipi(id as usize)?;
        }
        wait(|| CONTENDED.load(Ordering::Acquire) == peers, "secondary CPU missed IPI or spin-lock contention")?;
        drop(held);
        add_rounds();
        wait(|| FINISHED.load(Ordering::Acquire) == peers, "secondary CPU did not finish locked work")?;
        let acknowledgements = if cfg!(target_arch = "aarch64") { peers } else { 1 };
        for _ in 0..acknowledgements {
            wait(take_ipi, "boot CPU missed an acknowledgement IPI")?;
        }
        if *COUNTER.lock() != ROUNDS * ids.len() {
            return Err("SMP spin lock lost updates");
        }
    }

    use core::fmt::Write as _;
    let mut message = ByteSink::<96>::new();
    let _ = write!(message, "[NOVA-ARCH] SMP {} CPUs online, IPI and lock verified\r\n", ids.len());
    super::console().send_bytes(message.as_bytes());
    Ok(())
}

pub(crate) fn slot_for(cpu: usize) -> usize {
    CPU_IDS.iter().position(|id| id.load(Ordering::Acquire) == cpu).expect("unregistered CPU")
}

pub(crate) fn current_slot() -> usize {
    slot_for(cpu_id())
}

pub(crate) fn is_boot_cpu() -> bool {
    BOOT_CPU.load(Ordering::Acquire) == cpu_id()
}

pub(crate) fn start_check(cpu: usize, pool: EarlyFrames, image: &'static [u8], remote: Option<RemoteMutex>) -> Result<(), &'static str> {
    *CHECKS[slot_for(cpu)].lock() = Some(Work::Check(pool, image, remote));
    send_ipi(cpu)
}

pub(crate) fn start_resident(cpu: usize, pool: EarlyFrames, image: &'static [u8]) -> Result<(), &'static str> {
    *CHECKS[slot_for(cpu)].lock() = Some(Work::Resident(pool, image));
    send_ipi(cpu)
}

pub(crate) fn notify_slot(slot: usize) -> Result<(), &'static str> {
    let id = CPU_IDS[slot].load(Ordering::Acquire);
    if id == usize::MAX {
        return Err("cannot notify an unregistered CPU");
    }
    send_ipi(id)
}

pub(crate) fn finish_checks(ids: &[u64]) -> Result<(), &'static str> {
    let peers = ids.iter().map(|id| *id as usize).filter(|id| *id != cpu_id());
    for cpu in peers {
        let mut result = None;
        wait(|| {
            result = RESULTS[slot_for(cpu)].lock().take();
            result.is_some()
        }, "secondary CPU did not finish its user scheduler checks")?;
        result.ok_or("missing secondary result")??;
    }
    use core::fmt::Write as _;
    let mut message = ByteSink::<96>::new();
    let _ = write!(message, "[NOVA-ARCH] SMP {} secondary user schedulers verified\r\n", ids.len() - 1);
    super::console().send_bytes(message.as_bytes());
    Ok(())
}

fn wait(mut predicate: impl FnMut() -> bool, error: &'static str) -> Result<(), &'static str> {
    for _ in 0..WAIT_LIMIT {
        if predicate() {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(error)
}

fn add_rounds() {
    for _ in 0..ROUNDS {
        *COUNTER.lock() += 1;
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_virt_ap_main() -> ! {
    unsafe { super::arch_init() };
    init_ipi();
    ONLINE.store(cpu_id(), Ordering::Release);
    while !take_ipi() {
        core::hint::spin_loop();
    }
    if COUNTER.try_lock().is_none() {
        CONTENDED.fetch_add(1, Ordering::AcqRel);
        add_rounds();
        let _ = send_ipi(BOOT_CPU.load(Ordering::Acquire));
        FINISHED.fetch_add(1, Ordering::AcqRel);
    }
    let slot = current_slot();
    loop {
        take_ipi();
        let work = CHECKS[slot].lock().take();
        match work {
            Some(Work::Check(mut pool, image, remote)) => {
                let result = run_check(&mut pool, image, remote);
                *RESULTS[slot].lock() = Some(result);
            }
            Some(Work::Resident(mut pool, image)) => {
                let result = run_resident(&mut pool, image);
                let reason = result.err().unwrap_or("secondary resident scheduler returned");
                crate::klog!(crate::logger::Level::Error, "secondary resident scheduler: {reason}");
                unsafe { crate::arch::exit(crate::EXIT_TEST_FAILURE) };
            }
            None => idle_for_ipi(),
        }
    }
}

/// Sleep with exceptions masked and the local IPI source enabled. A signal
/// arriving between a scheduler's last queue check and idle remains pending.
pub(crate) fn idle_for_ipi() {
    #[cfg(target_arch = "aarch64")]
    unsafe { asm!("dsb sy", "wfi", options(nostack)) };
    #[cfg(target_arch = "riscv64")]
    unsafe { asm!("csrsi sie, 2", "wfi", "csrci sie, 2", options(nostack)) };
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        asm!("csrwr {mask}, 0x4", "idle 0", "csrwr {mask}, 0x4",
            mask = inout(reg) 1_usize << 12 => _, options(nostack));
    }
}

#[cfg(target_arch = "aarch64")]
global_asm!(
    ".pushsection .text.nova_virt_ap_entry, \"ax\"",
    ".global nova_virt_ap_entry",
    "nova_virt_ap_entry:",
    "msr daifset, #0xf",
    "msr spsel, #1",
    "mov sp, x0",
    "mov x0, #(3 << 20)",
    "msr cpacr_el1, x0",
    "isb",
    "b nova_virt_ap_main",
    ".popsection",
);

#[cfg(target_arch = "riscv64")]
global_asm!(
    ".pushsection .text.nova_virt_ap_entry, \"ax\"",
    ".balign 4",
    ".global nova_virt_ap_entry",
    "nova_virt_ap_entry:",
    "mv tp, a0",
    "mv sp, a1",
    "tail nova_virt_ap_main",
    ".popsection",
);

#[cfg(target_arch = "loongarch64")]
global_asm!(
    ".pushsection .text.nova_virt_ap_entry, \"ax\"",
    ".balign 4",
    ".global nova_virt_ap_entry",
    "nova_virt_ap_entry:",
    "la.local $t0, NOVA_AP_STACK_TOP",
    "ld.d $sp, $t0, 0",
    "dbar 0",
    "b nova_virt_ap_main",
    ".popsection",
);

#[cfg(target_arch = "aarch64")]
pub(super) fn cpu_id() -> usize {
    let mpidr: usize;
    unsafe { asm!("mrs {id}, mpidr_el1", id = out(reg) mpidr, options(nostack, nomem)) };
    mpidr & 0xff_00ff_ffff
}

#[cfg(target_arch = "riscv64")]
pub(super) fn cpu_id() -> usize {
    let id: usize;
    unsafe { asm!("mv {id}, tp", id = out(reg) id, options(nostack, nomem)) };
    id
}

#[cfg(target_arch = "loongarch64")]
pub(super) fn cpu_id() -> usize {
    let id: usize;
    unsafe { asm!("csrrd {id}, 0x20", id = out(reg) id, options(nostack, nomem)) };
    id & 0x3ff
}

#[cfg(target_arch = "aarch64")]
fn start(cpu: usize, stack: usize) -> Result<(), &'static str> {
    let status: isize;
    unsafe {
        asm!("dsb sy", "hvc #0", inlateout("x0") 0xc400_0003_usize => status,
            in("x1") cpu, in("x2") nova_virt_ap_entry as *const (), in("x3") stack,
            clobber_abi("C"), options(nostack));
    }
    (status == 0).then_some(()).ok_or("PSCI CPU_ON rejected secondary CPU")
}

#[cfg(target_arch = "riscv64")]
fn sbi(extension: usize, arg0: usize, arg1: usize, arg2: usize) -> isize {
    let status: isize;
    unsafe {
        asm!("ecall", inlateout("a0") arg0 => status, inlateout("a1") arg1 => _,
            in("a2") arg2, in("a6") 0_usize, in("a7") extension, options(nostack));
    }
    status
}

#[cfg(target_arch = "riscv64")]
fn start(cpu: usize, stack: usize) -> Result<(), &'static str> {
    (sbi(0x48534d, cpu, nova_virt_ap_entry as *const () as usize, stack) == 0)
        .then_some(()).ok_or("SBI HSM rejected secondary hart")
}

#[cfg(target_arch = "loongarch64")]
fn start(cpu: usize, _: usize) -> Result<(), &'static str> {
    if cpu >= 1024 {
        return Err("LoongArch CPU number does not fit the IOCSR mailbox");
    }
    let entry = nova_virt_ap_entry as *const () as u64;
    let command = (1 << 31) | ((cpu as u64) << 16);
    unsafe {
        asm!("dbar 0", "iocsrwr.d {high}, {mail}", "iocsrwr.d {low}, {mail}",
            "iocsrwr.w {wakeup}, {ipi}", high = in(reg) (entry & 0xffff_ffff_0000_0000) | command | 4,
            low = in(reg) (entry << 32) | command, mail = in(reg) 0x1048_usize,
            wakeup = in(reg) command, ipi = in(reg) 0x1040_usize, options(nostack));
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn init_ipi() {
    unsafe {
        core::ptr::write_volatile(0x0800_0000 as *mut u32, 1);
        core::ptr::write_volatile(0x0801_0004 as *mut u32, 0xff);
        core::ptr::write_volatile(0x0801_0000 as *mut u32, 1);
    }
}

#[cfg(target_arch = "loongarch64")]
fn init_ipi() {
    unsafe { asm!("iocsrwr.w {mask}, {reg}", mask = in(reg) 2_usize,
        reg = in(reg) 0x1004_usize, options(nostack)) };
}

#[cfg(target_arch = "riscv64")]
fn init_ipi() {}

#[cfg(target_arch = "aarch64")]
fn send_ipi(cpu: usize) -> Result<(), &'static str> {
    if cpu >= 8 { return Err("GICv2 target out of range"); }
    unsafe {
        asm!("dsb sy", options(nostack));
        core::ptr::write_volatile(0x0800_0f00 as *mut u32, (1 << (16 + cpu)) | WAKE_SGI);
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn send_ipi(cpu: usize) -> Result<(), &'static str> {
    (sbi(0x735049, 1, cpu, 0) == 0).then_some(()).ok_or("SBI IPI rejected target hart")
}

#[cfg(target_arch = "loongarch64")]
fn send_ipi(cpu: usize) -> Result<(), &'static str> {
    unsafe { asm!("iocsrwr.w {action}, {reg}", action = in(reg) ((1 << 31) | (cpu << 16) | 1),
        reg = in(reg) 0x1040_usize, options(nostack)) };
    Ok(())
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn take_ipi() -> bool {
    unsafe {
        let irq = core::ptr::read_volatile(0x0801_000c as *const u32);
        if irq & 0x3ff >= 1020 { return false; }
        core::ptr::write_volatile(0x0801_0010 as *mut u32, irq);
        irq & 0x3ff == WAKE_SGI
    }
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn take_ipi() -> bool {
    let pending: usize;
    unsafe { asm!("csrrc {pending}, sip, {mask}", pending = out(reg) pending,
        mask = in(reg) 2_usize, options(nostack)) };
    pending & 2 != 0
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn take_ipi() -> bool {
    let pending: usize;
    unsafe { asm!("iocsrrd.w {pending}, {status}", "iocsrwr.w {pending}, {clear}",
        pending = out(reg) pending, status = in(reg) 0x1000_usize, clear = in(reg) 0x100c_usize,
        options(nostack)) };
    pending & 2 != 0
}
