//! Capability-backed rendezvous for the early user-mode probes.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::syscall::ObjectType;
#[cfg(not(target_arch = "aarch64"))]
use nova_abi::{Syscall, nr};
use nova_cap::{CapabilityTable, ObjectRef};
use nova_core::scheduler::Scheduler;
use nova_core::syscall::SyscallArgs;
use nova_ipc::transfer::TableTransfer;
use nova_ipc::{Channel, ChannelId, Completion, MAX_INLINE_PAYLOAD, Message, OpResult, ReplyToken, Side};

use crate::early_init::PAGE_SIZE;

static mut IPC: [Ipc; crate::arch::virt_smp::MAX_CPUS] =
    [const { Ipc::new() }; crate::arch::virt_smp::MAX_CPUS];

/// # Safety
/// The boot probe may borrow this CPU's channel only once, with IRQs masked.
pub(super) unsafe fn local_probe_ipc() -> &'static mut Ipc {
    let cpu = crate::arch::virt_smp::current_slot();
    unsafe { &mut *(&raw mut IPC).cast::<Ipc>().add(cpu) }
}

pub(super) struct Ipc {
    tables: [CapabilityTable; 2],
    channel: Channel,
}

impl Ipc {
    pub(super) const fn new() -> Self {
        Self {
            tables: [const { CapabilityTable::new() }; 2],
            channel: Channel::new(ChannelId::new(0)),
        }
    }

    pub(super) fn install_endpoints(&mut self) -> Result<[CapHandle; 2], ErrorCode> {
        self.grant_endpoints([Rights::SEND, Rights::RECEIVE])
    }

    pub(super) fn grant_endpoints(&mut self, rights: [Rights; 2]) -> Result<[CapHandle; 2], ErrorCode> {
        Ok([
            self.tables[0].create(endpoint_object(Side::First), rights[0])?,
            self.tables[1].create(endpoint_object(Side::Second), rights[1])?,
        ])
    }

    /// The translation must return a readable kernel address for each requested page fragment.
    pub(super) unsafe fn send(
        &mut self, slot: usize, thread: u32, args: &SyscallArgs, translate: Translate, call: bool,
    ) -> OpResult<u64> {
        let message = (|| {
            let rights = self.resolve(slot, args.arg0, Rights::SEND)?;
            if call {
                output_buffer(args.arg4, args.arg5, translate)?;
            }
            Ok((rights, unsafe { read_message(args.arg1, args, translate)? }))
        })();
        let (rights, message) = match message {
            Ok(value) => value,
            Err(error) => return OpResult::Error(error),
        };
        let endpoint = self.channel.endpoint(side(slot), rights);
        let [first, second] = &mut self.tables;
        let (sender, receiver) = if slot == 0 { (first, second) } else { (second, first) };
        let mut transfer = TableTransfer::new(sender, receiver);
        let outcome = if call {
            self.channel.call(&endpoint, message, &mut transfer, thread)
        } else {
            self.channel.send(&endpoint, message, &mut transfer, thread)
        };
        outcome.map(|()| 0)
    }

    /// The translation must cover each requested writable page fragment for payload and token.
    pub(super) unsafe fn recv(
        &mut self, slot: usize, thread: u32, args: &SyscallArgs, translate: Translate,
    ) -> OpResult<u64> {
        let destination = (|| {
            let rights = self.resolve(slot, args.arg0, Rights::RECEIVE)?;
            let payload = output_buffer(args.arg2, args.arg3, translate)?;
            let token = if args.arg4 == 0 {
                None
            } else {
                Some(UserBuffer::translate(args.arg4 as usize, size_of::<u64>(), true, translate)?)
            };
            Ok((rights, payload, token))
        })();
        let (rights, payload, token) = match destination {
            Ok(value) => value,
            Err(error) => return OpResult::Error(error),
        };
        let endpoint = self.channel.endpoint(side(slot), rights);
        self.channel.recv(&endpoint, thread).map(|received| {
            unsafe { write_payload(payload, &received.message) };
            if let Some(pointer) = token {
                let raw = received.reply_token.map_or(0, ReplyToken::to_raw);
                unsafe { pointer.write(&raw.to_le_bytes()) };
            }
            received.message.opcode()
        })
    }

    pub(super) unsafe fn reply(
        &mut self, slot: usize, args: &SyscallArgs, translate: Translate,
    ) -> OpResult<u64> {
        let request = (|| {
            let rights = self.resolve(slot, args.arg0, Rights::REPLY)?;
            let message = unsafe { read_message(0, args, translate)? };
            Ok((rights, message))
        })();
        let (rights, message) = match request {
            Ok(value) => value,
            Err(error) => return OpResult::Error(error),
        };
        let endpoint = self.channel.endpoint(side(slot), rights);
        self.channel.reply(&endpoint, ReplyToken::from_raw(args.arg1), message).map(|()| 0)
    }

    pub(super) fn kill(&mut self, slot: usize) {
        self.channel.kill_side(side(slot));
    }

    pub(super) fn complete(
        &mut self, mut visit: impl FnMut(Completion) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let completions = self.channel.completions();
        if completions.dropped() != 0 {
            return Err("early IPC completion overflow");
        }
        for index in 0..completions.len() {
            visit(completions.get(index).ok_or("missing early IPC completion")?)?;
        }
        self.channel.take_completions();
        Ok(())
    }

    pub(super) fn is_drained(&self) -> bool {
        self.channel.parked() == 0
            && self.channel.outstanding_calls() == 0
            && self.channel.queued(Side::First) == 0
            && self.channel.queued(Side::Second) == 0
    }

    pub(super) fn wake_waiters(&mut self, scheduler: &mut Scheduler) -> Result<(), &'static str> {
        self.complete(|completion| match completion {
            Completion::SendCompleted { waiter } | Completion::ReceiveReady { waiter } => {
                scheduler.unblock(waiter).map_err(|error| error.tag())
            }
            _ => Err("unexpected assembly probe IPC completion"),
        })
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub(super) unsafe fn probe_message(
        &mut self,
        slot: usize,
        thread: u32,
        number: Syscall,
        args: &SyscallArgs,
        translate: Translate,
        scheduler: &mut Scheduler,
    ) -> Result<u64, &'static str> {
        let result = match (slot, number) {
            (0, nr::SEND) => match unsafe { self.send(slot, thread, args, translate, false) } {
                OpResult::Blocked => {
                    scheduler.block(thread).map_err(|error| error.tag())?;
                    0
                }
                _ => return Err("early IPC sender did not block"),
            },
            (1, nr::RECV) => match unsafe { self.recv(slot, thread, args, translate) } {
                OpResult::Ready(opcode) => opcode,
                _ => return Err("early IPC receiver did not receive the message"),
            },
            _ => return Err("unexpected early IPC syscall"),
        };
        self.wake_waiters(scheduler)?;
        Ok(result)
    }

    fn resolve(&self, slot: usize, raw: u64, needed: Rights) -> Result<Rights, ErrorCode> {
        let entry = self.tables[slot].check_rights(CapHandle::from_raw(raw), needed)?;
        if entry.object_ref != endpoint_object(side(slot)) {
            return Err(ErrorCode::BadHandle);
        }
        Ok(entry.rights)
    }
}

const fn side(slot: usize) -> Side {
    if slot == 0 { Side::First } else { Side::Second }
}

const fn endpoint_object(side: Side) -> ObjectRef {
    ObjectRef::new(ObjectType::Endpoint, side.index() as u32)
}

type Translate<'a> = &'a dyn Fn(usize, usize, bool) -> Option<*mut u8>;

struct UserBuffer {
    first: *mut u8,
    first_length: usize,
    second: Option<*mut u8>,
    length: usize,
}

impl UserBuffer {
    fn translate(address: usize, length: usize, write: bool, translate: Translate) -> Result<Self, ErrorCode> {
        if length == 0 || length > PAGE_SIZE || address.checked_add(length).is_none() {
            return Err(ErrorCode::Invalid);
        }
        let first_length = length.min(PAGE_SIZE - address % PAGE_SIZE);
        let first = translate(address, first_length, write).ok_or(ErrorCode::Invalid)?;
        let second = if first_length < length {
            Some(translate(address + first_length, length - first_length, write).ok_or(ErrorCode::Invalid)?)
        } else {
            None
        };
        Ok(Self { first, first_length, second, length })
    }

    unsafe fn read(&self, bytes: &mut [u8]) {
        assert_eq!(self.length, bytes.len());
        let (first, second) = bytes.split_at_mut(self.first_length);
        unsafe { core::ptr::copy_nonoverlapping(self.first, first.as_mut_ptr(), first.len()) };
        if let Some(pointer) = self.second {
            unsafe { core::ptr::copy_nonoverlapping(pointer, second.as_mut_ptr(), second.len()) };
        }
    }

    unsafe fn write(&self, bytes: &[u8]) {
        assert!(bytes.len() <= self.length);
        let (first, second) = bytes.split_at(self.first_length.min(bytes.len()));
        unsafe { core::ptr::copy_nonoverlapping(first.as_ptr(), self.first, first.len()) };
        if let Some(pointer) = self.second {
            unsafe { core::ptr::copy_nonoverlapping(second.as_ptr(), pointer, second.len()) };
        }
    }
}

unsafe fn read_message(opcode: u64, args: &SyscallArgs, translate: Translate) -> Result<Message, ErrorCode> {
    let length = args.arg3 as usize;
    if length > MAX_INLINE_PAYLOAD {
        return Err(ErrorCode::Invalid);
    }
    let mut bytes = [0; MAX_INLINE_PAYLOAD];
    if length != 0 {
        let buffer = UserBuffer::translate(args.arg2 as usize, length, false, translate)?;
        unsafe { buffer.read(&mut bytes[..length]) };
    }
    Message::with_payload(opcode, &bytes[..length])
}

fn output_buffer(address: u64, capacity: u64, translate: Translate) -> Result<Option<UserBuffer>, ErrorCode> {
    match capacity as usize {
        0 => Ok(None),
        MAX_INLINE_PAYLOAD => UserBuffer::translate(address as usize, MAX_INLINE_PAYLOAD, true, translate).map(Some),
        1..MAX_INLINE_PAYLOAD => Err(ErrorCode::BufferSmall),
        _ => Err(ErrorCode::Invalid),
    }
}

unsafe fn write_payload(buffer: Option<UserBuffer>, message: &Message) {
    if let Some(buffer) = buffer {
        unsafe { buffer.write(message.payload()) };
    }
}

pub(super) unsafe fn write_reply(args: &SyscallArgs, message: &Message, translate: Translate) -> Result<(), ErrorCode> {
    let output = output_buffer(args.arg4, args.arg5, translate)?;
    unsafe { write_payload(output, message) };
    Ok(())
}

#[cfg(not(target_arch = "aarch64"))]
pub(super) fn stack_buffer(
    stack_page: usize, stack_top: usize, address: usize, length: usize,
) -> Option<*mut u8> {
    let stack_base = stack_top - crate::early_init::PAGE_SIZE;
    let offset = address.checked_sub(stack_base)?;
    let end = address.checked_add(length)?;
    (end <= stack_top).then(|| (stack_page + offset) as *mut u8)
}
