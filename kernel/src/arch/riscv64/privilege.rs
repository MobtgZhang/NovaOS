//! U-mode context switching and four-level Sv48 address spaces.

use crate::arch_bringup::EarlyFrames;

const PAGE_SIZE: usize = 4096;
const ENTRIES: usize = 512;
const WINDOW_SIZE: usize = PAGE_SIZE * ENTRIES;
const SATP_SV48: usize = 9 << 60;

const VALID: usize = 1 << 0;
const READ: usize = 1 << 1;
const WRITE: usize = 1 << 2;
const EXECUTE: usize = 1 << 3;
const USER: usize = 1 << 4;
const ACCESSED: usize = 1 << 6;
const DIRTY: usize = 1 << 7;

const SSTATUS_SPIE: usize = 1 << 5;
const SSTATUS_SPP: usize = 1 << 8;
const SSTATUS_VS: usize = 3 << 9;
const SSTATUS_FS: usize = 3 << 13;
const SSTATUS_SUM: usize = 1 << 18;
const SSTATUS_MXR: usize = 1 << 19;
const SSTATUS_USER_CLEAR: usize =
    SSTATUS_SPIE | SSTATUS_SPP | SSTATUS_VS | SSTATUS_FS | SSTATUS_SUM | SSTATUS_MXR;
pub(super) const ECALL_FROM_U: usize = 8;
pub(super) const STORE_PAGE_FAULT: usize = 15;
pub(super) const INSTRUCTION_PAGE_FAULT: usize = 12;
pub(super) const SUPERVISOR_TIMER: usize = (1 << 63) | 5;
pub(super) const ARG0: usize = 10;
const SIE_STIE: usize = 1 << 5;
const SBI_EXT_TIME: usize = 0x5449_4d45;
const SBI_SET_TIMER: usize = 0;
/// Ten milliseconds at the QEMU `virt` timebase.
const QUANTA_PER_SECOND: usize = 100;
/// Fail the gate through a separate ecall if the timer never preempts this loop.
pub(super) const TIMER_SPIN_LIMIT: usize = 1 << 26;
const NO_SV48: usize = usize::MAX;
const BAD_BOOT_STATE: usize = usize::MAX - 1;
const TIMER_ARM_FAILED: usize = usize::MAX - 2;
const TIMER_STOP_FAILED: usize = usize::MAX - 3;
const COOKIE: usize = 0x4e4f_5641;
const MAX_VECTOR_BYTES: usize = 32;

#[repr(C, align(4096))]
struct Table([usize; ENTRIES]);

#[derive(Clone, Copy)]
pub(super) struct AddressSpace(usize);

/// RV64GC user state with registers indexed by hardware register number.
#[repr(C)]
pub(super) struct Context {
    pub(super) gpr: [usize; 32],
    pub(super) pc: usize,
    pub(super) fault_address: usize,
    fpr: [u64; 32],
    fcsr: u64,
    pub(super) vector_bytes: usize,
    vector_start: usize,
    vector_control: usize,
    vector_length: usize,
    vector_type: usize,
    vector: [u8; 32 * MAX_VECTOR_BYTES],
}

impl Context {
    pub(super) fn new(pc: usize, stack_top: usize) -> Self {
        let mut gpr = [0; 32];
        gpr[2] = stack_top;
        Self {
            gpr,
            pc,
            fault_address: 0,
            fpr: [0; 32],
            fcsr: 0,
            vector_bytes: supported_vector_bytes(),
            vector_start: 0,
            vector_control: 0,
            vector_length: 0,
            vector_type: 1 << 63,
            vector: [0; 32 * MAX_VECTOR_BYTES],
        }
    }

    /// Run until a U-mode ecall or an armed supervisor timer interrupts it.
    /// The saved PC names the trapping instruction or the interrupted one.
    pub(super) fn enter(
        &mut self,
        space: AddressSpace,
        timer_ticks: usize,
    ) -> Result<usize, &'static str> {
        // SAFETY: The boot hart owns the trap path and the roots map the
        // trap vector, boot stack, and context.
        let cause = unsafe { nova_riscv64_enter_user(space.0, self, timer_ticks) };
        match cause {
            NO_SV48 => Err("RISC-V hart did not enable Sv48"),
            BAD_BOOT_STATE => {
                Err("RISC-V privilege probe requires Bare mode and masked interrupt sources")
            }
            TIMER_ARM_FAILED => Err("RISC-V supervisor timer could not be enabled via SBI TIME"),
            TIMER_STOP_FAILED => Err("RISC-V supervisor timer could not be disarmed via SBI TIME"),
            _ => Ok(cause),
        }
    }
}

fn supported_vector_bytes() -> usize {
    let bytes: usize;
    unsafe {
        core::arch::asm!(
            "csrrs {saved}, sstatus, {mask}",
            "csrr {bytes}, sstatus",
            "and {bytes}, {bytes}, {mask}",
            "beqz {bytes}, 2f",
            "csrr {bytes}, 0xc22",
            "2:",
            "csrw sstatus, {saved}",
            mask = in(reg) SSTATUS_VS,
            saved = out(reg) _,
            bytes = out(reg) bytes,
            options(nostack),
        );
    }
    if bytes <= MAX_VECTOR_BYTES { bytes } else { 0 }
}

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __image_end_vma: u8;
    static __nova_user_text_start: u8;
    static __nova_user_stack_start: u8;
    static __nova_user_stack_end: u8;

    static nova_riscv64_cookie_entry: u8;
    static nova_riscv64_cookie_ecall: u8;
    static nova_riscv64_timer_entry: u8;
    static nova_riscv64_timer_spin: u8;
    static nova_riscv64_timer_timeout: u8;

    fn nova_riscv64_enter_user(satp: usize, context: *mut Context, timer_ticks: usize) -> usize;
}

core::arch::global_asm!(
    include_str!("privilege.S"),
    context_pc = const core::mem::offset_of!(Context, pc),
    context_fault_address = const core::mem::offset_of!(Context, fault_address),
    context_fpr = const core::mem::offset_of!(Context, fpr),
    context_fcsr = const core::mem::offset_of!(Context, fcsr),
    context_vector_bytes = const core::mem::offset_of!(Context, vector_bytes),
    context_vector_start = const core::mem::offset_of!(Context, vector_start),
    context_vector_control = const core::mem::offset_of!(Context, vector_control),
    context_vector_length = const core::mem::offset_of!(Context, vector_length),
    context_vector_type = const core::mem::offset_of!(Context, vector_type),
    context_vector = const core::mem::offset_of!(Context, vector),
    sie = const super::SSTATUS_SIE,
    spp = const SSTATUS_SPP,
    stie = const SIE_STIE,
    user_status_clear = const SSTATUS_USER_CLEAR,
    fs = const SSTATUS_FS,
    vs = const SSTATUS_VS,
    supervisor_timer = const SUPERVISOR_TIMER as i64,
    sbi_time = const SBI_EXT_TIME,
    sbi_set_timer = const SBI_SET_TIMER,
    timer_spin_limit = const TIMER_SPIN_LIMIT,
    no_sv48 = const NO_SV48 as i64,
    bad_boot_state = const BAD_BOOT_STATE as i64,
    timer_arm_failed = const TIMER_ARM_FAILED as i64,
    timer_stop_failed = const TIMER_STOP_FAILED as i64,
    cookie = const COOKIE,
    fatal = sym super::trap_entry,
);

fn pte(address: usize, flags: usize) -> usize {
    ((address >> 12) << 10) | VALID | flags
}

impl Table {
    fn map_page(&mut self, address: usize, physical: usize, flags: usize) {
        self.0[(address >> 12) & (ENTRIES - 1)] = pte(physical, flags | ACCESSED);
    }

    fn map(&mut self, start: usize, end: usize, flags: usize) {
        for address in (start..end).step_by(PAGE_SIZE) {
            self.map_page(address, address, flags);
        }
    }
}

pub(super) fn verify(
    frames: &mut EarlyFrames,
    stack_pages: Option<[usize; 2]>, init: &[crate::early_init::Image],
) -> Result<(), &'static str> {
    let frequency = frames.timebase_frequency().ok_or("missing RISC-V CPU timebase frequency")?;
    let timer_ticks = (frequency.get() as usize).div_ceil(QUANTA_PER_SECOND);
    let text = &raw const __text_start as usize;
    let image_end = &raw const __image_end_vma as usize;
    let user_text = &raw const __nova_user_text_start as usize;
    let user_stack = &raw const __nova_user_stack_start as usize;
    let user_stack_top = &raw const __nova_user_stack_end as usize;

    if text % WINDOW_SIZE != 0 || image_end - text > WINDOW_SIZE {
        return Err("RISC-V boot image exceeds the Sv48 port's 2 MiB window");
    }
    let mut spaces = [AddressSpace(0); 2 + crate::early_init::PROCESSES];
    let mut probe_tables = [[0; 4]; 2];
    let mut init_tables = [[0; 6]; crate::early_init::PROCESSES];
    for (slot, space) in spaces[..2 + init.len()].iter_mut().enumerate() {
        if slot < 2 && stack_pages.is_none() {
            continue;
        }
        let [root, upper, middle, leaf_address] = if slot < 2 {
            let tables = unsafe { frames.allocate::<4>()? };
            probe_tables[slot] = tables;
            tables
        } else {
            let [root, upper, middle, leaf, low_middle, low_leaf] = unsafe { frames.allocate::<6>()? };
            init_tables[slot - 2] = [root, upper, middle, leaf, low_middle, low_leaf];
            [root, upper, middle, leaf]
        };
        unsafe {
            (*(root as *mut Table)).0[(text >> 39) & (ENTRIES - 1)] = pte(upper, 0);
            (*(upper as *mut Table)).0[(text >> 30) & (ENTRIES - 1)] = pte(middle, 0);
            (*(middle as *mut Table)).0[(text >> 21) & (ENTRIES - 1)] = pte(leaf_address, 0);
            let leaf = &mut *(leaf_address as *mut Table);
            if slot >= 2 {
                let [.., low_middle, low_leaf] = init_tables[slot - 2];
                (*(upper as *mut Table)).0[0] = pte(low_middle, 0);
                (*(low_middle as *mut Table)).0[0] = pte(low_leaf, 0);
                let low_leaf = &mut *(low_leaf as *mut Table);
                for page in init[slot - 2].pages() {
                    use crate::early_init::Access;
                    let permission = match page.access {
                        Access::Read => 0,
                        Access::ReadWrite => WRITE | DIRTY,
                        Access::ReadExecute => EXECUTE,
                    };
                    low_leaf.map_page(page.address, page.frame, READ | USER | permission);
                }
            } else {
                leaf.map_page(user_text, user_text, READ | EXECUTE | USER);
                let stack = stack_pages.ok_or("missing RISC-V probe stack")?[slot];
                leaf.map_page(user_stack, stack, READ | WRITE | USER | DIRTY);
            }
            leaf.map(text, &raw const __text_end as usize, READ | EXECUTE);
            leaf.map(
                &raw const __bss_start as usize,
                &raw const __bss_end as usize,
                READ | WRITE | DIRTY,
            );
        }
        *space = AddressSpace(SATP_SV48 | (root >> 12));
    }
    let [first, second, init_spaces @ ..] = spaces;

    unsafe { core::arch::asm!("fence.i", options(nostack)) };
    super::user::run_init(frames, init_spaces, init_tables, init, stack_pages.is_some(), timer_ticks)?;
    if let Some(stacks) = stack_pages {
        verify_probes(first, user_stack_top, timer_ticks)?;
        super::user::run([first, second], stacks, user_stack_top, timer_ticks)?;
        frames.release(probe_tables.into_iter().flatten())?;
    }
    Ok(())
}

fn verify_probes(space: AddressSpace, stack_top: usize, timer_ticks: usize) -> Result<(), &'static str> {
    let mut ecall = Context::new(&raw const nova_riscv64_cookie_entry as usize, stack_top);
    if ecall.enter(space, 0)? != ECALL_FROM_U
        || ecall.pc != &raw const nova_riscv64_cookie_ecall as usize
    {
        super::trap_entry();
    }
    if ecall.gpr[ARG0] != COOKIE {
        return Err("RISC-V user trampoline returned a bad cookie");
    }

    let mut timer = Context::new(&raw const nova_riscv64_timer_entry as usize, stack_top);
    let cause = timer.enter(space, timer_ticks)?;
    let spin = &raw const nova_riscv64_timer_spin as usize;
    let timeout = &raw const nova_riscv64_timer_timeout as usize;
    if cause == ECALL_FROM_U && timer.pc == timeout && timer.gpr[ARG0] == 0 {
        return Err("RISC-V supervisor timer did not preempt the user loop");
    }
    if cause != SUPERVISOR_TIMER
        || !(spin..timeout).contains(&timer.pc)
        || timer.gpr[ARG0] >= TIMER_SPIN_LIMIT
    {
        super::trap_entry();
    }
    Ok(())
}
