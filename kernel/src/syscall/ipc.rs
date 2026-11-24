use nova_abi::{ErrorCode, Syscall, ipc::MAX_INLINE_PAYLOAD, nr};
use nova_core::syscall::{SyscallResult, ThreadEffect};
use nova_ipc::{Completion, Message, endpoint::Received};

use crate::mm::address_space::AddressSpace;
use crate::services::{KernelState, dispatch::{self, SyscallContext, SyscallOutcome}};
use crate::sync::SpinLock;
use crate::task::{self, Thread};

use super::{Outcome, copy_from_user, copy_to_space, failed, writable_range};

#[derive(Clone, Copy)]
enum Delivery {
    Done(SyscallOutcome),
    Received(Received),
    Reply(Message),
}

#[derive(Clone, Copy)]
struct Wait {
    context: SyscallContext,
    cpu: usize,
    number: Syscall,
    args: [u64; 6],
    delivery: Option<Delivery>,
}

const MAX_WAITS: usize = task::MAX_THREADS * crate::boot::info::MAX_CPUS;
static WAITS: SpinLock<[Option<Wait>; MAX_WAITS]> = SpinLock::new([None; MAX_WAITS]);

pub(super) fn dispatch(number: Syscall, args: [u64; 6]) -> Outcome {
    let Some(space) = task::current_thread_space() else {
        return failed(ErrorCode::BadHandle);
    };
    let context = task::current_syscall_context();
    let message = match prepare(number, args, &space) {
        Ok(message) => message,
        Err(error) => return failed(error),
    };

    let mut kernel = crate::services::state();
    let mut waits = WAITS.lock();
    let Some(slot) = waits.iter_mut().find(|slot| slot.is_none()) else {
        return failed(ErrorCode::Quota);
    };
    let delivery = match number {
        nr::RECV => receive(&mut kernel, &context, args[0]),
        nr::SEND | nr::CALL => Delivery::Done(dispatch::send(
            &mut kernel, &context, args[0], message, number == nr::CALL,
        )),
        nr::REPLY => Delivery::Done(dispatch::reply(&mut kernel, &context, args[0], args[1], message)),
        _ => unreachable!(),
    };
    let blocked = matches!(delivery, Delivery::Done(SyscallOutcome::Blocked));
    if blocked {
        *slot = Some(Wait {
            context,
            cpu: task::current_cpu(),
            number,
            args,
            delivery: None,
        });
    }
    collect(&mut kernel, &mut waits[..]);
    drop(waits);
    drop(kernel);

    if blocked {
        Outcome { result: SyscallResult::Value(0), effect: ThreadEffect::Block }
    } else {
        Outcome { result: deliver(&space, args, delivery), effect: ThreadEffect::Continue }
    }
}

fn prepare(number: Syscall, args: [u64; 6], space: &AddressSpace) -> Result<Message, ErrorCode> {
    let (output, capacity) = match number {
        nr::RECV => (args[2], args[3]),
        nr::CALL => (args[4], args[5]),
        _ => (0, 0),
    };
    if capacity != 0 {
        if capacity > MAX_INLINE_PAYLOAD as u64 {
            return Err(ErrorCode::Invalid);
        }
        if capacity < MAX_INLINE_PAYLOAD as u64 {
            return Err(ErrorCode::BufferSmall);
        }
        writable_range(space, output, capacity)?;
    }
    if number == nr::RECV {
        if args[4] != 0 {
            writable_range(space, args[4], size_of::<u64>() as u64)?;
        }
        return Ok(Message::new(0));
    }
    let mut payload = [0; MAX_INLINE_PAYLOAD];
    if args[3] != 0 {
        copy_from_user(&mut payload, args[2], args[3])?;
    }
    let opcode = if number == nr::REPLY { 0 } else { args[1] };
    Message::with_payload(opcode, &payload[..args[3] as usize])
}

fn receive(kernel: &mut KernelState, context: &SyscallContext, handle: u64) -> Delivery {
    match dispatch::receive(kernel, context, handle) {
        Ok(received) => Delivery::Received(received),
        Err(outcome) => Delivery::Done(outcome),
    }
}

fn collect(kernel: &mut KernelState, waits: &mut [Option<Wait>]) {
    while let Some(completion) = kernel.ipc.pop_completion() {
        let Some(wait) = waits.iter_mut().flatten().find(|wait| {
            wait.context.thread == completion.waiter() && wait.delivery.is_none()
        }) else {
            continue;
        };
        let delivery = match completion {
            Completion::SendCompleted { .. } if wait.number == nr::SEND => {
                Delivery::Done(SyscallOutcome::Value(0))
            }
            Completion::ReceiveReady { .. } if wait.number == nr::RECV => {
                receive(kernel, &wait.context, wait.args[0])
            }
            Completion::ReplyReady { message, .. } if wait.number == nr::CALL => Delivery::Reply(message),
            Completion::Failed { error, .. } => Delivery::Done(SyscallOutcome::Failed(error)),
            _ => continue,
        };
        if !matches!(delivery, Delivery::Done(SyscallOutcome::Blocked)) {
            wait.delivery = Some(delivery);
            crate::sched::request_wake(wait.context.thread, wait.cpu);
        }
    }
}

pub(crate) fn collect_after_exit(kernel: &mut KernelState, process: usize) {
    let mut waits = WAITS.lock();
    for slot in waits.iter_mut() {
        if slot.is_some_and(|wait| wait.context.task_slot == process) {
            *slot = None;
        }
    }
    collect(kernel, &mut waits[..]);
}

pub(crate) fn finish_wait(thread: &mut Thread) {
    let ready = {
        let mut waits = WAITS.lock();
        waits.iter_mut().find_map(|slot| match slot {
            Some(wait) if wait.context.thread == thread.id && wait.cpu == thread.cpu => {
                let delivery = wait.delivery?;
                let args = wait.args;
                *slot = None;
                Some((args, delivery))
            }
            _ => None,
        })
    };
    if let Some((args, delivery)) = ready {
        thread.context.rax = deliver(&thread.space, args, delivery).to_raw() as u64;
    }
}

fn deliver(space: &AddressSpace, args: [u64; 6], delivery: Delivery) -> SyscallResult {
    match delivery {
        Delivery::Done(SyscallOutcome::Failed(error)) => SyscallResult::Failed(error),
        Delivery::Done(SyscallOutcome::Value(value)) => SyscallResult::Value(value),
        Delivery::Done(SyscallOutcome::Blocked) => unreachable!(),
        Delivery::Received(received) => {
            if args[3] != 0 && let Err(error) = copy_to_space(space, args[2], received.message.payload()) {
                return SyscallResult::Failed(error);
            }
            if args[4] != 0 {
                let token = received.reply_token.map_or(0, nova_ipc::ReplyToken::to_raw);
                if let Err(error) = copy_to_space(space, args[4], &token.to_le_bytes()) {
                    return SyscallResult::Failed(error);
                }
            }
            SyscallResult::Value(received.message.opcode() as i64)
        }
        Delivery::Reply(message) => {
            if args[5] != 0 && let Err(error) = copy_to_space(space, args[4], message.payload()) {
                return SyscallResult::Failed(error);
            }
            SyscallResult::Value(0)
        }
    }
}
