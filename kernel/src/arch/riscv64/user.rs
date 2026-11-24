//! Two-process probe for the RV64 integer syscall and resume path.

use nova_abi::{Syscall, nr};
use nova_core::scheduler::Scheduler;
use nova_core::syscall::{SyscallArgs, ThreadEffect, dispatch_stateless, version_word};

use super::privilege::{
    ARG0, AddressSpace, Context, ECALL_FROM_U, INSTRUCTION_PAGE_FAULT, STORE_PAGE_FAULT, SUPERVISOR_TIMER, TIMER_SPIN_LIMIT,
};
use crate::{early_init, early_ipc};
use crate::arch_bringup::EarlyFrames;

const ARG1: usize = 11;
const ARG2: usize = 12;
const ARG3: usize = 13;
const SYSCALL_NUMBER: usize = 17;
const TIMER_COUNTER: usize = 5;
const COOKIES: [usize; 2] = [0x51, 0x52];

unsafe extern "C" {
    static nova_riscv64_user_main: u8;
    static nova_riscv64_user_spin: u8;
    static nova_riscv64_user_spin_end: u8;
}

fn request(context: &Context) -> Result<(Syscall, SyscallArgs), &'static str> {
    let number = u32::try_from(context.gpr[SYSCALL_NUMBER])
        .map(Syscall)
        .map_err(|_| "RISC-V syscall number does not fit the ABI")?;
    let args = SyscallArgs {
        arg0: context.gpr[ARG0] as u64,
        arg1: context.gpr[ARG0 + 1] as u64,
        arg2: context.gpr[ARG0 + 2] as u64,
        arg3: context.gpr[ARG0 + 3] as u64,
        arg4: context.gpr[ARG0 + 4] as u64,
        arg5: context.gpr[ARG0 + 5] as u64,
    };
    Ok((number, args))
}

pub(super) fn run_init(
    frames: &mut EarlyFrames,
    spaces: [AddressSpace; early_init::PROCESSES],
    tables: [[usize; 6]; early_init::PROCESSES],
    images: &[early_init::Image],
    check: bool,
    timer_ticks: usize,
) -> Result<(), &'static str> {
    early_init::run(Init { frames, spaces, tables, images, timer_ticks }, check, images.len())
}

struct Init<'a> {
    frames: &'a mut EarlyFrames,
    spaces: [AddressSpace; early_init::PROCESSES],
    tables: [[usize; 6]; early_init::PROCESSES],
    images: &'a [early_init::Image],
    timer_ticks: usize,
}

impl early_init::Platform for Init<'_> {
    type Context = Context;

    fn start(&self, slot: usize, role: u64, endpoint: u64) -> Context {
        let mut context = Context::new(self.images[slot].entry, early_init::STACK_TOP);
        context.gpr[ARG0] = role as usize;
        context.gpr[ARG1] = endpoint as usize;
        context
    }

    fn select(&self, _: usize) {}

    fn enter(&self, slot: usize, context: &mut Context) -> Result<early_init::Trap, &'static str> {
        match context.enter(self.spaces[slot], self.timer_ticks)? {
            ECALL_FROM_U => {
                context.pc += 4;
                let (number, args) = request(context)?;
                Ok(early_init::Trap::Syscall(number, args))
            }
            STORE_PAGE_FAULT => Ok(early_init::Trap::WriteFault(context.fault_address)),
            INSTRUCTION_PAGE_FAULT => Ok(early_init::Trap::ExecuteFault(context.fault_address)),
            SUPERVISOR_TIMER => Ok(early_init::Trap::Timer),
            _ => Ok(early_init::Trap::Fault(context.fault_address)),
        }
    }

    fn set_result(&self, context: &mut Context, value: u64) {
        context.gpr[ARG0] = value as usize;
    }

    fn image(&self, slot: usize) -> &early_init::Image {
        &self.images[slot]
    }

    fn retire(&mut self, slot: usize) -> Result<(), &'static str> {
        self.frames.release(self.tables[slot])?;
        self.frames.release(self.images[slot].frames())
    }
}

pub(super) fn run(
    spaces: [AddressSpace; 2], stack_pages: [usize; 2], stack_top: usize,
    timer_ticks: usize,
) -> Result<(), &'static str> {
    let mut scheduler = Scheduler::new(1);
    let first = scheduler.create_thread(1, 0).map_err(|error| error.tag())?;
    let second = scheduler.create_thread(2, 0).map_err(|error| error.tag())?;
    let ipc = unsafe { early_ipc::local_probe_ipc() };
    let endpoints = ipc.install_endpoints().map_err(|error| error.name())?;
    let mut contexts = core::array::from_fn::<_, 2, _>(|slot| {
        let mut context = Context::new(&raw const nova_riscv64_user_main as usize, stack_top);
        context.gpr[ARG0] = COOKIES[slot];
        context.gpr[ARG1] = slot;
        context.gpr[ARG3] = endpoints[slot].raw() as usize;
        context.gpr[ARG0 + 4] = context.vector_bytes;
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
        let arm_timer = thread == first && version_calls[slot] && !preempted;
        let cause = context.enter(spaces[slot], if arm_timer { timer_ticks } else { 0 })?;
        if cause == SUPERVISOR_TIMER {
            let spin = &raw const nova_riscv64_user_spin as usize;
            let spin_end = &raw const nova_riscv64_user_spin_end as usize;
            if !arm_timer
                || !(spin..spin_end).contains(&context.pc)
                || context.gpr[TIMER_COUNTER] >= TIMER_SPIN_LIMIT
            {
                super::trap_entry();
            }
            if !scheduler.tick() {
                return Err("RISC-V timer did not expire the scheduler quantum");
            }
            preempted = true;
            context.gpr[ARG2] = 1;
            next = scheduler.schedule();
            if next != Some(second) {
                return Err("RISC-V timer did not switch to the second address space");
            }
            continue;
        }
        if cause != ECALL_FROM_U {
            super::trap_entry();
        }

        let (number, args) = request(context)?;
        context.pc += 4;
        if preempted && yielded && !ipc_done[slot] {
            let translate = |address, length, _write| {
                early_ipc::stack_buffer(stack_pages[slot], stack_top, address, length)
            };
            context.gpr[ARG0] = unsafe {
                ipc.probe_message(slot, thread, number, &args, &translate, &mut scheduler)?
            } as usize;
            ipc_done[slot] = true;
            if slot == 0 {
                next = scheduler.schedule();
            }
            continue;
        }
        let dispatch = dispatch_stateless(number, &args);
        context.gpr[ARG0] = dispatch.result.to_raw() as usize;
        match (number, dispatch.effect) {
            (nr::GET_VERSION, ThreadEffect::Continue) if !version_calls[slot] => {
                version_calls[slot] = true;
            }
            (nr::YIELD, ThreadEffect::Yield) if thread == second && preempted && !yielded => {
                yielded = true;
                next = scheduler.yield_current();
                if next != Some(first) {
                    return Err("RISC-V yield did not resume the preempted context");
                }
            }
            (nr::THREAD_EXIT, ThreadEffect::Exit) => {
                if !preempted || !yielded || context.gpr[ARG0] != COOKIES[slot] {
                    return Err("RISC-V user result, context, or private stack check failed");
                }
                scheduler.exit(thread).map_err(|error| error.tag())?;
                next = scheduler.schedule();
            }
            _ => return Err("unexpected syscall during the RISC-V scheduler probe"),
        }
    }

    if next.is_some()
        || scheduler.exited_count() != 2
        || !version_calls.into_iter().all(|called| called)
        || !ipc_done.into_iter().all(|done| done)
        || !ipc.is_drained()
    {
        return Err("RISC-V scheduler probe did not complete both contexts");
    }
    crate::arch_bringup::report_vector_bits(contexts[0].vector_bytes * 8);
    Ok(())
}

core::arch::global_asm!(
    include_str!("user.S"),
    get_version = const nr::GET_VERSION.number(),
    version = const version_word(),
    yield_thread = const nr::YIELD.number(),
    thread_exit = const nr::THREAD_EXIT.number(),
    spin_limit = const TIMER_SPIN_LIMIT,
    send = const nr::SEND.number(),
    recv = const nr::RECV.number(),
    max_inline_payload = const nova_ipc::MAX_INLINE_PAYLOAD,
);
