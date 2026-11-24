//! EL0 context switching and address-space probes.

use core::ptr::addr_of;

use nova_abi::cap::CapHandle;
use nova_abi::error::ErrorCode;
use nova_abi::{Syscall, nr};
use nova_core::scheduler::{Scheduler, ThreadState};
use nova_core::syscall::{SyscallArgs, ThreadEffect, dispatch_stateless};
use nova_ipc::OpResult;

use super::{Timer, VIRTUAL_TIMER_IRQ, cycles, paging};
use crate::{early_init, early_ipc};

const EL0_INTERRUPTS: u64 = 0x340;
const SVC64: u64 = (0x15 << 26) | (1 << 25);
const DATA_ABORT_FROM_EL0: u64 = (0x24 << 26) | (1 << 25);
const INST_ABORT_FROM_EL0: u64 = 0x20;
const PERMISSION_WRITE_L3: u64 = (1 << 6) | 0x0f;
const ABORT_SYNDROME_MASK: u64 = (0x7f << 25) | (1 << 10) | 0x7f;
const IRQ: u64 = u64::MAX;
const FIRST_COOKIE: u64 = 0x51;
const SECOND_COOKIE: u64 = 0x52;

#[derive(Default)]
#[repr(C, align(16))]
struct Context {
    gpr: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
    vectors: [u128; 32],
    fpcr: u64,
    fpsr: u64,
    tpidr: u64,
}

const _: () = {
    assert!(core::mem::offset_of!(Context, sp) == 248);
    assert!(core::mem::offset_of!(Context, pc) == 256);
    assert!(core::mem::offset_of!(Context, pstate) == 264);
    assert!(core::mem::offset_of!(Context, vectors) == 272);
    assert!(core::mem::offset_of!(Context, fpcr) == 784);
    assert!(core::mem::offset_of!(Context, fpsr) == 792);
    assert!(core::mem::offset_of!(Context, tpidr) == 800);
};

impl Context {
    fn new(cookie: u64, cooperates: bool, endpoint: CapHandle) -> Self {
        let mut gpr = [0; 31];
        gpr[0] = cookie;
        gpr[1] = u64::from(cooperates);
        gpr[3] = endpoint.raw();
        Self {
            gpr,
            sp: addr_of!(__user_stack_end) as u64,
            pc: addr_of!(nova_aarch64_user_main) as u64,
            pstate: EL0_INTERRUPTS,
            ..Self::default()
        }
    }

    fn syscall(&self) -> Result<Syscall, &'static str> {
        u32::try_from(self.gpr[8])
            .map(Syscall)
            .map_err(|_| "invalid AArch64 syscall number")
    }

    fn arguments(&self) -> SyscallArgs {
        let [arg0, arg1, arg2, arg3, arg4, arg5, ..] = self.gpr;
        SyscallArgs {
            arg0,
            arg1,
            arg2,
            arg3,
            arg4,
            arg5,
        }
    }

    fn finish_ipc(&mut self, outcome: OpResult<u64>) -> bool {
        self.gpr[0] = match outcome {
            OpResult::Ready(value) => value,
            OpResult::Error(error) => error.negated() as u64,
            OpResult::Blocked => return false,
        };
        true
    }
}

unsafe extern "C" {
    static __user_stack_end: u8;
    static nova_aarch64_user_main: u8;
    static nova_aarch64_user_fault: u8;
    fn nova_aarch64_enter_user(context: *mut Context) -> u64;
}

pub(super) fn run_init(
    frames: &mut crate::arch_bringup::EarlyFrames,
    spaces: [paging::AddressSpace; early_init::PROCESSES],
    images: &[early_init::Image],
    timer: &Timer,
    check: bool,
) -> Result<(), &'static str> {
    early_init::run(Init { frames, spaces, images, timer }, check, images.len())
}

struct Init<'a> {
    frames: &'a mut crate::arch_bringup::EarlyFrames,
    spaces: [paging::AddressSpace; early_init::PROCESSES],
    images: &'a [early_init::Image],
    timer: &'a Timer,
}

impl early_init::Platform for Init<'_> {
    type Context = Context;

    fn start(&self, slot: usize, role: u64, endpoint: u64) -> Context {
        let mut gpr = [0; 31];
        gpr[0] = role;
        gpr[1] = endpoint;
        Context {
            gpr,
            sp: early_init::STACK_TOP as u64,
            pc: self.images[slot].entry as u64,
            pstate: EL0_INTERRUPTS,
            ..Context::default()
        }
    }

    fn select(&self, slot: usize) {
        self.spaces[slot].activate();
    }

    fn enter(&self, _: usize, context: &mut Context) -> Result<early_init::Trap, &'static str> {
        self.timer.arm();
        let trap = unsafe { nova_aarch64_enter_user(context) };
        if trap == IRQ {
            return match self.timer.acknowledge() {
                VIRTUAL_TIMER_IRQ => Ok(early_init::Trap::Timer),
                crate::arch::virt_smp::WAKE_SGI => Ok(early_init::Trap::WakeIpi),
                _ => Err("unexpected AArch64 init IRQ"),
            };
        }
        Timer::stop();
        if context.pstate & 0xf != 0 {
            return Err("AArch64 init trap did not originate at EL0");
        }
        if trap == SVC64 {
            return Ok(early_init::Trap::Syscall(context.syscall()?, context.arguments()));
        }
        if trap >> 26 == INST_ABORT_FROM_EL0 && trap & 0x3f == 0x0f {
            return Ok(early_init::Trap::ExecuteFault(context.pc as usize));
        }
        if trap >> 26 == DATA_ABORT_FROM_EL0 >> 26 && trap & (1 << 10) == 0 {
            let fault_address: usize;
            unsafe {
                core::arch::asm!("mrs {address}, far_el1", address = out(reg) fault_address, options(nostack, nomem));
            }
            if trap & ABORT_SYNDROME_MASK == DATA_ABORT_FROM_EL0 | PERMISSION_WRITE_L3 {
                return Ok(early_init::Trap::WriteFault(fault_address));
            }
            return Ok(early_init::Trap::Fault(fault_address));
        }
        Ok(early_init::Trap::Fault(context.pc as usize))
    }

    fn set_result(&self, context: &mut Context, value: u64) {
        context.gpr[0] = value;
    }

    fn image(&self, slot: usize) -> &early_init::Image {
        &self.images[slot]
    }

    fn user_buffer(&self, slot: usize, address: usize, length: usize, write: bool) -> Option<*mut u8> {
        self.images[slot].user_buffer(address, length, write)?;
        paging::contains_user_range(address, length, write).then_some(address as *mut u8)
    }

    fn retire(&mut self, slot: usize) -> Result<(), &'static str> {
        self.spaces[slot].retire(self.frames)?;
        self.frames.release(self.images[slot].frames())
    }
}

pub(super) fn run(timer: &Timer) -> Result<(), &'static str> {
    let mut scheduler = Scheduler::new(1);
    let faulting = scheduler.create_thread(3, 0).map_err(|error| error.tag())?;
    let first = scheduler.create_thread(1, 0).map_err(|error| error.tag())?;
    let second = scheduler.create_thread(2, 0).map_err(|error| error.tag())?;
    let ipc = unsafe { early_ipc::local_probe_ipc() };
    let [send, receive] = ipc.install_endpoints().map_err(|error| error.name())?;
    let mut contexts = [
        Context::new(FIRST_COOKIE, false, send),
        Context::new(SECOND_COOKIE, true, receive),
    ];
    if scheduler.schedule() != Some(faulting) {
        return Err("AArch64 scheduler did not start the fault probe");
    }
    let mut fault_context = Context::new(0, false, CapHandle::INVALID);
    fault_context.pc = addr_of!(nova_aarch64_user_fault) as u64;
    fault_context.gpr[0] = addr_of!(nova_aarch64_user_main) as u64;
    paging::select(false);
    let trap = unsafe { nova_aarch64_enter_user(&mut fault_context) };
    let fault_address: u64;
    unsafe {
        core::arch::asm!("mrs {address}, far_el1", address = out(reg) fault_address, options(nostack, nomem));
    }
    if trap & ABORT_SYNDROME_MASK != DATA_ABORT_FROM_EL0 | PERMISSION_WRITE_L3
        || fault_address != addr_of!(nova_aarch64_user_main) as u64
        || fault_context.pc != addr_of!(nova_aarch64_user_fault) as u64
        || fault_context.pstate & 0xf != 0
    {
        return Err("AArch64 RX write failed to report the expected EL0 permission fault");
    }
    scheduler.exit(faulting).map_err(|error| error.tag())?;
    if scheduler.thread(faulting).map(|thread| thread.state) != Some(ThreadState::Exited) {
        return Err("AArch64 scheduler did not stop the faulting task");
    }
    let mut next = scheduler.schedule();
    if next != Some(first) {
        return Err("AArch64 scheduler did not run the surviving task after the fault");
    }
    let mut version_calls = [false; 2];
    let mut blocked_receive = [false; 2];
    let mut saw_sender_block = false;
    let mut saw_receiver_block = false;
    let mut preempted = false;
    let mut yielded = false;
    let translate = |address, length, write| {
        paging::contains_user_range(address, length, write).then_some(address as *mut u8)
    };

    for _ in 0..24 {
        let Some(thread) = next else { break };
        let slot = usize::from(thread == second);
        let context = &mut contexts[slot];
        paging::select(slot == 1);
        if blocked_receive[slot] {
            let outcome = unsafe { ipc.recv(slot, thread, &context.arguments(), &translate) };
            if !context.finish_ipc(outcome) {
                return Err("AArch64 IPC receiver woke without a message");
            }
            blocked_receive[slot] = false;
            ipc.wake_waiters(&mut scheduler)?;
        }
        if thread == first && version_calls[slot] && !preempted {
            context.gpr[2] = cycles() + timer.frequency / 5;
            timer.arm();
        }
        let trap = unsafe { nova_aarch64_enter_user(context) };
        if context.pstate & 0xf != 0 {
            return Err("AArch64 exception did not originate at EL0");
        }
        if trap == IRQ {
            if timer.acknowledge() != VIRTUAL_TIMER_IRQ
                || thread != first
                || preempted
                || !scheduler.tick()
            {
                return Err("unexpected timer interrupt during AArch64 scheduling test");
            }
            preempted = true;
            next = scheduler.schedule();
            if next != Some(second) {
                return Err("timer did not switch to the other AArch64 address space");
            }
            continue;
        }
        Timer::stop();
        if trap != SVC64 {
            return Err("unexpected synchronous EL0 exception");
        }
        let number = context.syscall()?;
        if matches!(number, nr::SEND | nr::RECV) && preempted && yielded {
            let args = context.arguments();
            let outcome = unsafe {
                if number == nr::SEND {
                    ipc.send(slot, thread, &args, &translate, false)
                } else {
                    ipc.recv(slot, thread, &args, &translate)
                }
            };
            if !context.finish_ipc(outcome) {
                if number == nr::SEND {
                    saw_sender_block = true;
                    context.gpr[0] = 0;
                } else {
                    saw_receiver_block = true;
                    blocked_receive[slot] = true;
                }
                scheduler.block(thread).map_err(|error| error.tag())?;
                ipc.wake_waiters(&mut scheduler)?;
                next = scheduler.schedule();
            } else {
                ipc.wake_waiters(&mut scheduler)?;
            }
            continue;
        }
        let dispatch = dispatch_stateless(number, &context.arguments());
        context.gpr[0] = dispatch.result.to_raw() as u64;
        match (number, dispatch.effect) {
            (nr::GET_VERSION, ThreadEffect::Continue) if !version_calls[slot] => {
                version_calls[slot] = true;
            }
            (nr::YIELD, ThreadEffect::Yield) if thread == second && preempted && !yielded => {
                yielded = true;
                next = scheduler.yield_current();
                if next != Some(first) {
                    return Err("AArch64 yield did not resume the preempted context");
                }
            }
            (nr::THREAD_EXIT, ThreadEffect::Exit) if preempted && yielded => {
                let expected = if thread == first {
                    FIRST_COOKIE
                } else {
                    SECOND_COOKIE
                };
                if context.gpr[0] != expected {
                    return Err("AArch64 process lost its context or private stack data");
                }
                scheduler.exit(thread).map_err(|error| error.tag())?;
                next = scheduler.schedule();
            }
            _ => return Err("unexpected syscall during the AArch64 scheduling test"),
        }
    }

    if next.is_some()
        || scheduler.exited_count() != 3
        || !version_calls.into_iter().all(|called| called)
    {
        return Err("AArch64 scheduling test did not run both processes to completion");
    }
    if !saw_sender_block || !saw_receiver_block || !ipc.is_drained() {
        return Err("AArch64 IPC did not complete both rendezvous paths");
    }
    Ok(())
}

core::arch::global_asm!(
    include_str!("user.S"),
    get_version = const nr::GET_VERSION.number(),
    version_major = const nova_abi::ABI_VERSION.major,
    version_minor = const nova_abi::ABI_VERSION.minor,
    yield_thread = const nr::YIELD.number(),
    thread_exit = const nr::THREAD_EXIT.number(),
    send = const nr::SEND.number(),
    recv = const nr::RECV.number(),
    invalid = const ErrorCode::Invalid.code(),
    denied = const ErrorCode::Denied.code(),
    max_inline_payload = const nova_ipc::MAX_INLINE_PAYLOAD,
);
