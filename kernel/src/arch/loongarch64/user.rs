//! PLV3 integer, floating-point and vector context for the boot probe.
//!
//! The exception vector saves the integer registers and trapped PC here,
//! then returns through the trusted kernel frame. The boot driver advances
//! the PC and places the syscall result in `a0` before entering again.

use core::ptr::addr_of;

use nova_abi::{Syscall, nr};
use nova_core::scheduler::Scheduler;
use nova_core::syscall::{SyscallArgs, ThreadEffect, dispatch_stateless, version_word};

use crate::{early_init, early_ipc};

const PAGE_SIZE: u64 = 4096;
const USER_CODE: u64 = crate::early_init::CODE as u64;
const USER_STACK_TOP: u64 = crate::early_init::STACK_TOP as u64;
const INIT_TRAPS: u64 = 1;
const PAGE_MODIFICATION: u64 = 4;
const PAGE_NON_EXECUTABLE: u64 = 6;
const TLB_MISS: u64 = 0x8000;
const SYSCALL_EXCEPTION: u64 = 0x0b;
const PROBE_CHALLENGE: u64 = 0x4e4f_5641_5f50_4c33;
const PROBE_COOKIE_SALT: u64 = 0x5a5;
const TIMER_BUDGET: u64 = 1 << 24;
const ARGUMENTS: [u64; 5] = [0x11, 0x22, 0x33, 0x44, 0x55];
const SP: usize = 3;
const A0: usize = 4;
const A1: usize = 5;
const A6: usize = 10;
const A7: usize = 11;
const S1: usize = 24;
const S3: usize = 26;
const EUEN_FP: u64 = 1 << 0;
const EUEN_LSX: u64 = 1 << 1;
const EUEN_LASX: u64 = 1 << 2;
const CPUCFG_FP: u64 = 1 << 0;
const CPUCFG_LSX: u64 = 1 << 6;
const CPUCFG_LASX: u64 = 1 << 7;

#[derive(Clone, Copy)]
struct AddressSpace([usize; 4]);

impl AddressSpace {
    fn new(
        frames: &mut crate::arch_bringup::EarlyFrames,
        pages: impl IntoIterator<Item = early_init::Page>,
    ) -> Result<Self, &'static str> {
        const ENTRIES: usize = nova_core::ENTRIES_PER_TABLE;
        const USER: usize = 1 | (3 << 2) | (1 << 4);
        const DIRTY: usize = 1 << 1;
        const NX: usize = 1 << 62;
        let tables = unsafe { frames.allocate::<4>()? };
        let [root, upper, middle, leaf] = tables.map(|address| address as *mut [usize; ENTRIES]);
        unsafe {
            (*root)[0] = upper as usize;
            (*upper)[0] = middle as usize;
            (*middle)[0] = leaf as usize;
            for page in pages {
                use early_init::Access;
                let permission = match page.access {
                    Access::Read => NX,
                    Access::ReadWrite => DIRTY | NX,
                    Access::ReadExecute => 0,
                };
                (*leaf)[page.address / early_init::PAGE_SIZE] = page.frame | USER | permission;
            }
        }
        Ok(Self(tables))
    }
}

#[repr(C, align(32))]
struct Context {
    registers: [[u64; 4]; 32],
    gpr: [u64; 32],
    pc: u64,
    exception: u64,
    fault_address: usize,
    fcsr: u64,
    fcc: [u64; 8],
}

impl Context {
    fn new(entry: *const u8) -> Self {
        let mut gpr = [0; 32];
        gpr[SP] = USER_STACK_TOP;
        Self {
            registers: [[0; 4]; 32],
            gpr,
            pc: user_address(entry),
            exception: 0,
            fault_address: 0,
            fcsr: 0,
            fcc: [0; 8],
        }
    }

    fn enter(&mut self, expected: ExpectedTrap, space: AddressSpace) -> Result<(), &'static str> {
        let syscall_pc = match expected {
            ExpectedTrap::Syscall(label) => user_address(label),
            ExpectedTrap::Timer => 0,
            ExpectedTrap::Init => INIT_TRAPS,
        };
        if unsafe { loongarch64_enter_user(self, syscall_pc, space.0[0], user_extensions()) } == 1 {
            Ok(())
        } else {
            Err(match expected {
                ExpectedTrap::Syscall(_) | ExpectedTrap::Init => {
                    "LoongArch privilege probe requires PLV0 direct boot with traps installed"
                }
                ExpectedTrap::Timer => "LoongArch PLV3 timer probe requires a masked, idle timer",
            })
        }
    }

    fn syscall(&self) -> Result<Syscall, &'static str> {
        u32::try_from(self.gpr[A7])
            .map(Syscall)
            .map_err(|_| "invalid LoongArch syscall number")
    }

    fn arguments(&self) -> SyscallArgs {
        let [_, _, _, _, arg0, arg1, arg2, arg3, arg4, arg5, ..] = self.gpr;
        SyscallArgs {
            arg0,
            arg1,
            arg2,
            arg3,
            arg4,
            arg5,
        }
    }
}

fn user_extensions() -> u64 {
    let features: u64;
    unsafe {
        core::arch::asm!("cpucfg {features}, {index}", features = lateout(reg) features,
            index = in(reg) 2_u64, options(nostack, nomem));
    }
    if features & CPUCFG_FP == 0 {
        0
    } else if features & CPUCFG_LSX == 0 {
        EUEN_FP
    } else if features & CPUCFG_LASX == 0 {
        EUEN_FP | EUEN_LSX
    } else {
        EUEN_FP | EUEN_LSX | EUEN_LASX
    }
}

enum ExpectedTrap {
    Syscall(*const u8),
    Init,
    Timer,
}

fn user_address(label: *const u8) -> u64 {
    USER_CODE | (label as u64 & (PAGE_SIZE - 1))
}

unsafe extern "C" {
    static loongarch64_user_start: u8;
    static loongarch64_user_cookie_syscall: u8;
    static loongarch64_user_abi_start: u8;
    static loongarch64_user_version_syscall: u8;
    static loongarch64_user_yield_syscall: u8;
    static loongarch64_user_ipc_syscall: u8;
    static loongarch64_user_exit_syscall: u8;
    fn loongarch64_enter_user(
        context: *mut Context, syscall_pc: u64, root: usize, extensions: u64,
    ) -> u64;
}

pub(super) fn verify(
    frames: &mut crate::arch_bringup::EarlyFrames,
    stack_pages: Option<[usize; 2]>, images: &[early_init::Image],
) -> Result<(), &'static str> {
    let mut spaces = [AddressSpace([0; 4]); early_init::PROCESSES];
    for (space, image) in spaces.iter_mut().zip(images) {
        *space = AddressSpace::new(frames, image.pages())?;
    }
    unsafe { core::arch::asm!("dbar 0", "ibar 0", options(nostack)) };
    early_init::run(Init { frames, images, spaces }, stack_pages.is_some(), images.len())?;
    let Some(stack_pages) = stack_pages else { return Ok(()) };
    let code = addr_of!(loongarch64_user_start) as usize;
    let probe_pages = |stack| [
        early_init::Page { address: early_init::CODE, frame: code, access: early_init::Access::ReadExecute },
        early_init::Page { address: early_init::STACK, frame: stack, access: early_init::Access::ReadWrite },
    ];
    let probe_spaces = [
        AddressSpace::new(frames, probe_pages(stack_pages[0]))?,
        AddressSpace::new(frames, probe_pages(stack_pages[1]))?,
    ];

    let mut context = Context::new(addr_of!(loongarch64_user_start));
    context.gpr[A0] = PROBE_CHALLENGE;
    context.enter(
        ExpectedTrap::Syscall(addr_of!(loongarch64_user_cookie_syscall)),
        probe_spaces[0],
    )?;
    if context.gpr[A0] != PROBE_CHALLENGE ^ PROBE_COOKIE_SALT {
        return Err("LoongArch PLV3 syscall returned the wrong cookie");
    }

    verify_scheduler(stack_pages, probe_spaces)?;
    for space in probe_spaces {
        frames.release(space.0)?;
    }
    Ok(())
}

struct Init<'a> {
    frames: &'a mut crate::arch_bringup::EarlyFrames,
    images: &'a [early_init::Image],
    spaces: [AddressSpace; early_init::PROCESSES],
}

impl early_init::Platform for Init<'_> {
    type Context = Context;

    fn start(&self, slot: usize, role: u64, endpoint: u64) -> Context {
        let mut context = Context::new(core::ptr::null());
        context.pc = self.images[slot].entry as u64;
        context.gpr[A0] = role;
        context.gpr[A1] = endpoint;
        context
    }

    fn select(&self, _: usize) {}

    fn enter(&self, slot: usize, context: &mut Context) -> Result<early_init::Trap, &'static str> {
        context.enter(ExpectedTrap::Init, self.spaces[slot])?;
        if context.exception == 0 {
            return Ok(early_init::Trap::Timer);
        }
        if context.exception == PAGE_MODIFICATION {
            return Ok(early_init::Trap::WriteFault(context.fault_address));
        }
        if context.exception == PAGE_NON_EXECUTABLE {
            return Ok(early_init::Trap::ExecuteFault(context.fault_address));
        }
        if context.exception == SYSCALL_EXCEPTION {
            context.pc += 4;
            return Ok(early_init::Trap::Syscall(context.syscall()?, context.arguments()));
        }
        Ok(early_init::Trap::Fault(context.fault_address))
    }

    fn set_result(&self, context: &mut Context, value: u64) {
        context.gpr[A0] = value;
    }

    fn image(&self, slot: usize) -> &early_init::Image {
        &self.images[slot]
    }

    fn retire(&mut self, slot: usize) -> Result<(), &'static str> {
        self.frames.release(self.spaces[slot].0)?;
        self.frames.release(self.images[slot].frames())
    }
}

fn verify_scheduler(stack_pages: [usize; 2], spaces: [AddressSpace; 2]) -> Result<(), &'static str> {
    let extensions = user_extensions();
    let cookies = [PROBE_CHALLENGE, PROBE_CHALLENGE ^ PROBE_COOKIE_SALT];
    let mut scheduler = Scheduler::new(1);
    let first = scheduler.create_thread(1, 0).map_err(|error| error.tag())?;
    let second = scheduler.create_thread(2, 0).map_err(|error| error.tag())?;
    let ipc = unsafe { early_ipc::local_probe_ipc() };
    let endpoints = ipc.install_endpoints().map_err(|error| error.name())?;
    let mut contexts = core::array::from_fn::<_, 2, _>(|slot| {
        let mut context = Context::new(addr_of!(loongarch64_user_abi_start));
        context.gpr[A0] = cookies[slot];
        context.gpr[A1..A1 + ARGUMENTS.len()].copy_from_slice(&ARGUMENTS);
        context.gpr[A6] = slot as u64;
        context.gpr[S1] = endpoints[slot].raw();
        context.gpr[S3] = extensions;
        context
    });
    let mut next = scheduler.schedule();
    let mut version_calls = [false; 2];
    let mut preempted = false;
    let mut yielded = false;
    let mut ipc_done = [false; 2];

    for _ in 0..8 {
        let Some(thread) = next else { break };
        let slot = usize::from(thread == second);
        let context = &mut contexts[slot];
        if thread == first && version_calls[slot] && !preempted {
            context.enter(ExpectedTrap::Timer, spaces[slot])?;
            if !scheduler.tick() {
                return Err("LoongArch timer did not expire the scheduler quantum");
            }
            preempted = true;
            context.gpr[A6] = 1;
            next = scheduler.schedule();
            if next != Some(second) {
                return Err("LoongArch timer did not switch the user stack mapping");
            }
            continue;
        }

        let label = if !version_calls[slot] {
            addr_of!(loongarch64_user_version_syscall)
        } else if thread == second && !yielded {
            addr_of!(loongarch64_user_yield_syscall)
        } else if !ipc_done[slot] {
            addr_of!(loongarch64_user_ipc_syscall)
        } else {
            addr_of!(loongarch64_user_exit_syscall)
        };
        context.enter(ExpectedTrap::Syscall(label), spaces[slot])?;
        let number = context.syscall()?;
        let args = context.arguments();
        context.pc += 4;
        if preempted && yielded && !ipc_done[slot] {
            let translate = |address, length, _write| {
                early_ipc::stack_buffer(stack_pages[slot], USER_STACK_TOP as usize, address, length)
            };
            context.gpr[A0] = unsafe {
                ipc.probe_message(slot, thread, number, &args, &translate, &mut scheduler)?
            };
            ipc_done[slot] = true;
            if slot == 0 {
                next = scheduler.schedule();
            }
            continue;
        }
        let dispatch = dispatch_stateless(number, &args);
        context.gpr[A0] = dispatch.result.to_raw() as u64;
        match (number, dispatch.effect) {
            (nr::GET_VERSION, ThreadEffect::Continue) if !version_calls[slot] => {
                if args.arg0 != cookies[slot] || args.all()[1..] != ARGUMENTS {
                    return Err("LoongArch syscall lost the user ABI registers");
                }
                version_calls[slot] = true;
            }
            (nr::YIELD, ThreadEffect::Yield) if thread == second && preempted && !yielded => {
                yielded = true;
                next = scheduler.yield_current();
                if next != Some(first) {
                    return Err("LoongArch yield did not resume the preempted thread");
                }
            }
            (nr::THREAD_EXIT, ThreadEffect::Exit) if preempted && yielded => {
                if context.gpr[A0] != cookies[slot] || context.gpr[SP] != USER_STACK_TOP {
                    return Err("LoongArch resume lost register state or private stack data");
                }
                scheduler.exit(thread).map_err(|error| error.tag())?;
                next = scheduler.schedule();
            }
            _ => return Err("unexpected syscall during the LoongArch scheduler probe"),
        }
    }

    if next.is_some() || scheduler.exited_count() != 2 {
        return Err("LoongArch scheduler did not run both processes to completion");
    }
    if version_calls != [true; 2] || ipc_done != [true; 2] {
        return Err("LoongArch scheduler probe missed a user syscall");
    }
    if !ipc.is_drained() {
        return Err("LoongArch scheduler probe left outstanding IPC");
    }
    let vector_bits = if extensions & EUEN_LASX != 0 { 256 } else if extensions & EUEN_LSX != 0 { 128 } else { 0 };
    crate::arch_bringup::report_vector_bits(vector_bits);
    Ok(())
}

core::arch::global_asm!(
    include_str!("privilege.S"),
    cookie_salt = const PROBE_COOKIE_SALT,
    user_code = const USER_CODE,
    page_shift = const PAGE_SIZE.trailing_zeros(),
    context_gpr = const core::mem::offset_of!(Context, gpr),
    context_pc = const core::mem::offset_of!(Context, pc),
    context_exception = const core::mem::offset_of!(Context, exception),
    context_fault_address = const core::mem::offset_of!(Context, fault_address),
    context_fcsr = const core::mem::offset_of!(Context, fcsr),
    context_fcc = const core::mem::offset_of!(Context, fcc),
    euen_fp = const EUEN_FP,
    euen_lsx = const EUEN_LSX,
    euen_lasx = const EUEN_LASX,
    tlb_miss = const TLB_MISS,
    syscall_exception = const SYSCALL_EXCEPTION,
    init_traps = const INIT_TRAPS,
    get_version = const nr::GET_VERSION.number(),
    version_word = const version_word(),
    timer_budget = const TIMER_BUDGET,
    yield_thread = const nr::YIELD.number(),
    thread_exit = const nr::THREAD_EXIT.number(),
    send = const nr::SEND.number(),
    recv = const nr::RECV.number(),
    max_inline_payload = const nova_ipc::MAX_INLINE_PAYLOAD,
);
