//! Load and schedule static user images on the early ports.

use core::sync::atomic::{AtomicU32, Ordering};

use nova_abi::{ErrorCode, Syscall, nr};
use nova_abi::cap::Rights;
use nova_abi::cap::CapHandle;
use nova_core::scheduler::Scheduler;
use nova_core::syscall::{SyscallArgs, ThreadEffect, dispatch_stateless};
use nova_elf::{ET_EXEC, Elf, PF_R, PF_W, PF_X, PT_DYNAMIC, PT_INTERP};
use nova_ipc::{Completion, Message, OpResult};

use crate::early_ipc::Ipc;
use crate::arch::capabilities::Console;
use crate::sync::SpinLock;

pub const PAGE_SIZE: usize = 4096;
pub const CODE: usize = 0x10000;
pub const STACK: usize = CODE + PAGE_SIZE;
pub const STACK_TOP: usize = STACK + PAGE_SIZE;
pub const SHARED: usize = STACK_TOP + 2 * PAGE_SIZE;
const USER_END: usize = PAGE_SIZE * nova_core::ENTRIES_PER_TABLE;
const MAX_IMAGE_PAGES: usize = 8;
pub const PROCESSES: usize = 7;
const FAULT_PROBE: usize = 2;
const TIMER_PROBE: usize = 3;
const GUARD_PROBE: usize = 4;
const EXECUTE_PROBE: usize = 5;
const READ_ONLY_PROBE: usize = 6;
const SERVICE_ROLE_BASE: usize = 16;
pub const SERVICES: usize = 2;
pub const INIT_NAME: &[u8] = b"init";
pub const ECHO_NAME: &[u8] = b"echo-server";
pub const CHECK_NAME: &[u8] = b"init-check";
pub(crate) const PROBE_SECOND_CPU: u64 = 1 << 8;
pub(crate) const PROBE_REMOTE_MUTEX: u64 = 1 << 9;
pub(crate) const REMOTE_MUTEX_OFFSET: usize = 128;

static mut INIT_IPC: [Ipc; crate::arch::virt_smp::MAX_CPUS] =
    [const { Ipc::new() }; crate::arch::virt_smp::MAX_CPUS];
static mut RESIDENT_IPC: Ipc = Ipc::new();
static REMOTE_IPC: SpinLock<RemoteIpc> = SpinLock::new(RemoteIpc {
    ipc: Ipc::new(),
    homes: [usize::MAX; SERVICES],
    endpoints: None,
    mailboxes: [None; SERVICES],
});

struct RemoteIpc {
    ipc: Ipc,
    homes: [usize; SERVICES],
    endpoints: Option<[CapHandle; SERVICES]>,
    mailboxes: [Option<Completion>; SERVICES],
}

fn endpoint_rights() -> [Rights; SERVICES] {
    let rights = Rights::SEND.union(Rights::RECEIVE);
    [rights, rights.union(Rights::REPLY)]
}

pub(crate) fn install_remote_ipc(homes: [usize; SERVICES]) -> Result<(), &'static str> {
    let mut session = REMOTE_IPC.lock();
    if homes[0] == homes[1] || session.endpoints.is_some() {
        return Err("invalid remote IPC homes");
    }
    session.endpoints = Some(session.ipc.grant_endpoints(endpoint_rights()).map_err(|error| error.name())?);
    session.homes = homes;
    Ok(())
}

enum RuntimeIpc<'a> {
    Local(&'a mut Ipc),
    Remote { cpu: usize, role: usize },
}

impl RuntimeIpc<'_> {
    fn operate<T>(&mut self, operation: impl FnOnce(&mut Ipc) -> T) -> Result<T, &'static str> {
        match *self {
            Self::Local(ref mut ipc) => Ok(operation(ipc)),
            Self::Remote { cpu, .. } => {
                let (result, notify) = {
                    let mut session = REMOTE_IPC.lock();
                    let RemoteIpc { ipc, homes, mailboxes, .. } = &mut *session;
                    let result = operation(ipc);
                    let mut notify = None;
                    ipc.complete(|completion| {
                        let target = completion.waiter().checked_sub(1).ok_or("invalid remote IPC waiter")? as usize;
                        let mailbox = mailboxes.get_mut(target).ok_or("unknown remote IPC waiter")?;
                        if mailbox.replace(completion).is_some() {
                            return Err("remote IPC mailbox overwrite");
                        }
                        if homes[target] != cpu {
                            notify = Some(homes[target]);
                        }
                        Ok(())
                    })?;
                    (result, notify)
                };
                if let Some(home) = notify {
                    crate::arch::virt_smp::notify_slot(home)?;
                }
                Ok(result)
            }
        }
    }

    fn poll(&mut self, mut visit: impl FnMut(Completion) -> Result<(), &'static str>) -> Result<(), &'static str> {
        match *self {
            Self::Local(ref mut ipc) => ipc.complete(visit),
            Self::Remote { role, .. } => {
                let completion = REMOTE_IPC.lock().mailboxes[role].take();
                completion.map_or(Ok(()), &mut visit)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FutexSlot {
    Empty,
    Waiting(usize),
    Ready,
}

static FUTEX: crate::sync::SpinLock<[[FutexSlot; PROCESSES]; crate::arch::virt_smp::MAX_CPUS]> =
    crate::sync::SpinLock::new([[FutexSlot::Empty; PROCESSES]; crate::arch::virt_smp::MAX_CPUS]);
static REMOTE_WAKES: AtomicU32 = AtomicU32::new(0);
static RECEIVED_WAKE_IPIS: AtomicU32 = AtomicU32::new(0);

pub(crate) fn verified_remote_wake() -> bool {
    REMOTE_WAKES.load(Ordering::Acquire) != 0 && RECEIVED_WAKE_IPIS.load(Ordering::Acquire) != 0
}

#[derive(Clone, Copy)]
pub struct Image {
    pub entry: usize,
    owned: [Page; MAX_IMAGE_PAGES],
    length: usize,
    pub shared: Option<usize>,
    pub probe_flags: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    ReadWrite,
    ReadExecute,
}

#[derive(Clone, Copy)]
pub struct Page {
    pub address: usize,
    pub frame: usize,
    pub access: Access,
}

pub fn load(
    bytes: &[u8],
    frames: &mut crate::arch_bringup::EarlyFrames,
) -> Result<Image, &'static str> {
    let machine = match crate::arch::arch_name() {
        "aarch64" => nova_elf::EM_AARCH64,
        "riscv64" => nova_elf::EM_RISCV,
        "loongarch64" => nova_elf::EM_LOONGARCH,
        _ => return Err("unknown machine for early init"),
    };
    let elf = Elf::parse_for_machine(bytes, machine).map_err(|error| error.tag())?;
    elf.validate(USER_END as u64).map_err(|error| error.tag())?;
    if elf.header().e_type != ET_EXEC
        || elf.program_headers().any(|header| matches!(header.p_type, PT_DYNAMIC | PT_INTERP))
    {
        return Err("init requires a static executable");
    }
    let entry = elf.entry_point() as usize;
    let mut owned = [Page { address: 0, frame: 0, access: Access::Read }; MAX_IMAGE_PAGES];
    owned[0] = Page { address: STACK, frame: 0, access: Access::ReadWrite };
    let mut length = 1;
    let mut executable_entry = false;
    for segment in elf.loadable_segments() {
        let access = match segment.p_flags {
            PF_R => Access::Read,
            flags if flags == PF_R | PF_W => Access::ReadWrite,
            flags if flags == PF_R | PF_X => Access::ReadExecute,
            _ => return Err("invalid init segment permissions"),
        };
        let start = segment.p_vaddr as usize;
        let end = start + segment.p_memsz as usize;
        if access == Access::ReadExecute && (start..start + segment.p_filesz as usize).contains(&entry) {
            executable_entry = entry.is_multiple_of(4);
        }
        for address in (start & !(PAGE_SIZE - 1)..end).step_by(PAGE_SIZE) {
            if address < CODE || matches!(address, STACK | STACK_TOP | SHARED) {
                return Err("init segment overlaps a reserved page");
            }
            if owned[..length].iter().any(|page| page.address == address) {
                return Err("init segments share a page");
            }
            let page = owned.get_mut(length).ok_or("init image exceeds the page budget")?;
            *page = Page { address, frame: 0, access };
            length += 1;
        }
    }
    if !executable_entry {
        return Err("init entry must be aligned and inside file-backed code");
    }
    let mut physical = [0; MAX_IMAGE_PAGES];
    unsafe { frames.allocate_into(&mut physical[..length])? };
    for (page, frame) in owned[..length].iter_mut().zip(physical) {
        page.frame = frame;
    }
    for segment in elf.loadable_segments() {
        let start = segment.p_vaddr as usize;
        let end = start + segment.p_filesz as usize;
        for page in &owned[1..length] {
            let copy_start = start.max(page.address);
            let copy_end = end.min(page.address + PAGE_SIZE);
            if copy_start < copy_end {
                let source = segment.p_offset as usize + (copy_start - start);
                let target = page.frame + (copy_start - page.address);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(source), target as *mut u8, copy_end - copy_start,
                    );
                }
            }
        }
    }
    Ok(Image { entry, owned, length, shared: None, probe_flags: 0 })
}

impl Image {
    pub fn frames(&self) -> impl Iterator<Item = usize> + '_ {
        self.owned[..self.length].iter().map(|page| page.frame)
    }

    pub fn pages(&self) -> impl Iterator<Item = Page> + '_ {
        self.owned[..self.length].iter().copied().chain(self.shared.map(|frame| Page {
            address: SHARED, frame, access: Access::ReadWrite,
        }))
    }

    pub fn user_buffer(&self, address: usize, length: usize, write: bool) -> Option<*mut u8> {
        let end = address.checked_add(length)?;
        let base = address & !(PAGE_SIZE - 1);
        let page = self.pages().find(|page| page.address == base)?;
        (end <= base + PAGE_SIZE && (!write || page.access == Access::ReadWrite))
            .then(|| (page.frame + (address - base)) as *mut u8)
    }
}

pub trait Platform {
    type Context;

    fn start(&self, slot: usize, role: u64, endpoint: u64) -> Self::Context;
    fn select(&self, slot: usize);
    fn enter(&self, slot: usize, context: &mut Self::Context) -> Result<Trap, &'static str>;
    fn set_result(&self, context: &mut Self::Context, value: u64);
    fn image(&self, slot: usize) -> &Image;
    fn user_buffer(&self, slot: usize, address: usize, length: usize, write: bool) -> Option<*mut u8> {
        self.image(slot).user_buffer(address, length, write)
    }
    fn retire(&mut self, slot: usize) -> Result<(), &'static str>;
}

pub enum Trap {
    Syscall(Syscall, SyscallArgs),
    WriteFault(usize),
    ExecuteFault(usize),
    Fault(usize),
    Timer,
    #[cfg(target_arch = "aarch64")]
    WakeIpi,
}

#[derive(Clone, Copy)]
enum Pending {
    Send,
    Receive(SyscallArgs),
    Call(SyscallArgs),
    Reply(SyscallArgs, Message),
}

pub fn run(mut platform: impl Platform, check: bool, count: usize) -> Result<(), &'static str> {
    let cpu = crate::arch::virt_smp::current_slot();
    let distributed = !check && count == 1;
    let (mut ipc, endpoints, first_role) = if distributed {
        let session = REMOTE_IPC.lock();
        let role = session.homes.iter().position(|&home| home == cpu).ok_or("missing remote service role")?;
        let endpoints = session.endpoints.ok_or("remote service endpoints not installed")?;
        (RuntimeIpc::Remote { cpu, role }, endpoints, role)
    } else {
        let local = if check {
        unsafe { &mut *(&raw mut INIT_IPC).cast::<Ipc>().add(cpu) }
        } else {
            assert!(crate::arch::virt_smp::is_boot_cpu());
            unsafe { &mut *(&raw mut RESIDENT_IPC) }
        };
        let endpoints = local.grant_endpoints(endpoint_rights()).map_err(|error| error.name())?;
        (RuntimeIpc::Local(local), endpoints, 0)
    };
    let remote = (0..count.min(SERVICES)).find_map(|slot| {
        let image = platform.image(slot);
        (image.probe_flags & PROBE_REMOTE_MUTEX != 0)
            .then(|| image.shared.expect("remote mutex must have a shared frame") + REMOTE_MUTEX_OFFSET)
    });
    let mut scheduler = Scheduler::new(1);
    let mut threads = [0; PROCESSES];
    let startup_order: &[usize] = if check {
        &[TIMER_PROBE, FAULT_PROBE, GUARD_PROBE, EXECUTE_PROBE, READ_ONLY_PROBE, 0, 1]
    } else if !distributed {
        &[1, 0]
    } else {
        &[0]
    };
    for &slot in startup_order {
        threads[slot] = scheduler.create_thread(slot as u32 + 1, 0).map_err(|error| error.tag())?;
    }
    let mut contexts = core::array::from_fn::<_, PROCESSES, _>(|slot| (slot < count).then(|| {
        let endpoint = endpoints.get(first_role + slot).map_or(0, |handle| handle.raw());
        let role = if check { slot } else { SERVICE_ROLE_BASE + first_role + slot };
        platform.start(slot, role as u64 | platform.image(slot).probe_flags, endpoint)
    }));
    let mut pending = [None; PROCESSES];
    let mut next = scheduler.schedule();
    let mut received = [false; 2];
    let mut contained_fault = false;
    let mut contained_guard_fault = false;
    let mut contained_execute_fault = false;
    let mut contained_read_only_fault = false;
    let mut timer_ticks = 0;
    let mut remote_replies = 0;
    let mut budget = if remote.is_some() { 65_536 } else { 96 };
    loop {
        if (remote.is_some() || distributed) && crate::arch::virt_smp::take_ipi() && remote.is_some() {
            RECEIVED_WAKE_IPIS.fetch_add(1, Ordering::Release);
        }
        ipc.poll(|completion| {
            let slot = if distributed {
                if completion.waiter() != first_role as u32 + 1 {
                    return Err("remote IPC completion routed to the wrong scheduler");
                }
                0
            } else {
                threads.iter().position(|id| *id == completion.waiter()).ok_or("unknown IPC waiter")?
            };
            let waiter = threads[slot];
            let waiting = pending[slot].take().ok_or("initramfs IPC completion without wait")?;
            let result = match (waiting, completion) {
                (Pending::Send, Completion::SendCompleted { .. }) => 0,
                (Pending::Receive(args), Completion::ReceiveReady { .. }) => {
                    pending[slot] = Some(Pending::Receive(args));
                    return scheduler.unblock(waiter).map_err(|error| error.tag());
                }
                (Pending::Call(args), Completion::ReplyReady { message, .. }) => {
                    pending[slot] = Some(Pending::Reply(args, message));
                    if distributed {
                        remote_replies += 1;
                        if remote_replies == 128 {
                            crate::arch::console().send_bytes(b"[NOVA-ARCH] SMP resident cross-CPU RPC verified\r\n");
                        }
                    }
                    return scheduler.unblock(waiter).map_err(|error| error.tag());
                }
                (_, Completion::Failed { error, .. }) => error.negated() as u64,
                _ => return Err("unexpected initramfs IPC wait completion"),
            };
            let context = contexts[slot].as_mut().ok_or("missing initramfs waiter context")?;
            platform.set_result(context, result);
            scheduler.unblock(waiter).map_err(|error| error.tag())
        })?;
        let remote_waiting = {
            let mut waiters = FUTEX.lock();
            for (slot, waiter) in waiters[cpu].iter_mut().enumerate() {
                if *waiter == FutexSlot::Ready {
                    *waiter = FutexSlot::Empty;
                    scheduler.unblock(threads[slot]).map_err(|error| error.tag())?;
                }
            }
            remote.is_some_and(|key| waiters[cpu].contains(&FutexSlot::Waiting(key)))
        };
        let Some(thread) = next.or_else(|| scheduler.schedule()) else {
            if remote_waiting || distributed && scheduler.exited_count() < count as u32 {
                crate::arch::virt_smp::idle_for_ipi();
                continue;
            }
            break;
        };
        next = Some(thread);
        if check {
            if budget == 0 {
                return Err("initramfs check exhausted its trap budget");
            }
            budget -= 1;
        }
        let slot = threads.iter().position(|id| *id == thread).ok_or("unknown initramfs thread")?;
        let context = contexts[slot].as_mut().ok_or("missing initramfs context")?;
        platform.select(slot);
        let translate = |address, length, write| platform.user_buffer(slot, address, length, write);
        let request = match pending[slot].take() {
            Some(Pending::Receive(args)) => Some((nr::RECV, args)),
            Some(Pending::Reply(args, message)) => {
                unsafe { crate::early_ipc::write_reply(&args, &message, &translate) }
                    .map_err(|error| error.name())?;
                platform.set_result(context, 0);
                continue;
            }
            Some(_) => return Err("initramfs thread resumed without an IPC completion"),
            None => match platform.enter(slot, context)? {
                Trap::Syscall(number, args) if slot < FAULT_PROBE => Some((number, args)),
                Trap::WriteFault(address) if check && slot == FAULT_PROBE && address == platform.image(slot).entry => {
                    contained_fault = true;
                    None
                }
                Trap::Fault(STACK_TOP) if check && slot == GUARD_PROBE => {
                    contained_guard_fault = true;
                    None
                }
                Trap::ExecuteFault(address) if check && slot == EXECUTE_PROBE
                    && platform.image(slot).pages().any(|page| {
                        page.address == address & !(PAGE_SIZE - 1)
                            && page.address != STACK && page.access == Access::ReadWrite
                    }) =>
                {
                    contained_execute_fault = true;
                    None
                }
                Trap::WriteFault(address) if check && slot == READ_ONLY_PROBE
                    && platform.image(slot).pages().any(|page| {
                        page.address == address & !(PAGE_SIZE - 1) && page.access == Access::Read
                    }) =>
                {
                    contained_read_only_fault = true;
                    None
                }
                Trap::Fault(address) | Trap::WriteFault(address) | Trap::ExecuteFault(address) if !check => {
                    crate::klog!(crate::logger::Level::Error, "initramfs slot {slot} fault at {address:#x}");
                    None
                }
                Trap::Fault(_) | Trap::WriteFault(_) | Trap::ExecuteFault(_) => {
                    return Err("unexpected initramfs user fault");
                }
                Trap::Syscall(..) => return Err("initramfs fault or timer probe reached a syscall"),
                #[cfg(target_arch = "aarch64")]
                Trap::WakeIpi => {
                    RECEIVED_WAKE_IPIS.fetch_add(1, Ordering::Release);
                    continue;
                }
                Trap::Timer => {
                    let expired = scheduler.tick();
                    if slot == TIMER_PROBE {
                        if !expired {
                            return Err("initramfs timer did not consume the probe's quantum");
                        }
                        timer_ticks += 1;
                        if timer_ticks == 2 {
                            scheduler.exit(thread).map_err(|error| error.tag())?;
                            platform.retire(slot)?;
                        }
                    }
                    if expired {
                        next = scheduler.schedule();
                    }
                    if slot == TIMER_PROBE && timer_ticks == 1 && (next.is_none() || next == Some(thread)) {
                        return Err("initramfs timer did not switch away from the spinning process");
                    }
                    continue;
                }
            },
        };
        let (number, args) = request.unwrap_or((nr::THREAD_EXIT, SyscallArgs::default()));
        let reschedule = if request.is_none() {
            retire(&mut platform, &mut ipc, &mut scheduler, slot, first_role + slot, thread)?;
            true
        } else if matches!(number, nr::FUTEX_WAIT | nr::FUTEX_WAKE) {
            let address = args.arg0 as usize;
            let word = translate(address, 4, true).filter(|_| address.is_multiple_of(4));
            let key = platform.image(slot).user_buffer(address, 4, true).map(|ptr| ptr as usize);
            let (result, blocked) = match word.zip(key) {
                None => (ErrorCode::Invalid.negated() as u64, false),
                Some((_, _)) if args.arg2 != 0 && number == nr::FUTEX_WAIT => {
                    (ErrorCode::Unsupported.negated() as u64, false)
                }
                Some((word, key)) if number == nr::FUTEX_WAIT => {
                    let mut waiters = FUTEX.lock();
                    let current = unsafe { &*word.cast::<AtomicU32>() }.load(Ordering::Acquire);
                    if current == args.arg1 as u32 {
                        scheduler.block(thread).map_err(|error| error.tag())?;
                        waiters[cpu][slot] = FutexSlot::Waiting(key);
                        (0, true)
                    } else {
                        (ErrorCode::WouldBlock.negated() as u64, false)
                    }
                }
                Some(_) if args.arg1 == 0 => (ErrorCode::Invalid.negated() as u64, false),
                Some((_, key)) => {
                    let mut count = 0;
                    let mut notify = 0usize;
                    for (home, slots) in FUTEX.lock().iter_mut().enumerate() {
                        for waiting in slots {
                            if *waiting == FutexSlot::Waiting(key) && count < args.arg1 {
                                *waiting = FutexSlot::Ready;
                                count += 1;
                                if home != cpu {
                                    notify |= 1 << home;
                                }
                            }
                        }
                    }
                    for home in 0..crate::arch::virt_smp::MAX_CPUS {
                        if notify & (1 << home) != 0 {
                            crate::arch::virt_smp::notify_slot(home)?;
                            REMOTE_WAKES.fetch_add(1, Ordering::Release);
                        }
                    }
                    (count, false)
                }
            };
            platform.set_result(context, result);
            blocked
        } else if matches!(number, nr::SEND | nr::RECV | nr::CALL | nr::REPLY) {
            let role = first_role + slot;
            let waiter = if distributed { role as u32 + 1 } else { thread };
            let outcome = ipc.operate(|ipc| match number {
                nr::SEND | nr::CALL => unsafe { ipc.send(role, waiter, &args, &translate, number == nr::CALL) },
                nr::RECV => unsafe { ipc.recv(role, waiter, &args, &translate) },
                _ => unsafe { ipc.reply(role, &args, &translate) },
            })?;
            match outcome {
                OpResult::Ready(value) => {
                    received[slot] |= number == nr::RECV;
                    platform.set_result(context, value);
                    false
                }
                OpResult::Error(error) => {
                    platform.set_result(context, error.negated() as u64);
                    false
                }
                OpResult::Blocked => {
                    pending[slot] = Some(match number {
                        nr::SEND => Pending::Send,
                        nr::RECV => Pending::Receive(args),
                        nr::CALL => Pending::Call(args),
                        _ => return Err("initramfs reply attempted to block"),
                    });
                    scheduler.block(thread).map_err(|error| error.tag())?;
                    true
                }
            }
        } else if !check && number == nr::WRITE {
            let value = if first_role + slot == 0 { write_console(&args, &translate) } else { Err(ErrorCode::Denied) };
            platform.set_result(context, value.unwrap_or_else(|error| error.negated() as u64));
            false
        } else {
            let dispatched = dispatch_stateless(number, &args);
            let result = dispatched.result.to_raw();
            platform.set_result(context, result as u64);
            match dispatched.effect {
                ThreadEffect::Continue => false,
                ThreadEffect::Yield => true,
                ThreadEffect::Block => return Err("stateless initramfs syscall attempted to block"),
                ThreadEffect::Exit => {
                    if result != 0 {
                        crate::klog!(crate::logger::Level::Error, "initramfs slot {slot} exit status {result}");
                        if check {
                            return Err("initramfs process exited unsuccessfully");
                        }
                    }
                    retire(&mut platform, &mut ipc, &mut scheduler, slot, first_role + slot, thread)?;
                    true
                }
            }
        };

        if reschedule {
            next = scheduler.schedule();
        }
    }
    if !check {
        return Err("no runnable initramfs service");
    }
    if timer_ticks != 2 || !contained_fault || !contained_guard_fault || !contained_execute_fault
        || !contained_read_only_fault || scheduler.exited_count() != PROCESSES as u32
        || pending.iter().any(Option::is_some) || FUTEX.lock()[cpu].iter().any(|slot| *slot != FutexSlot::Empty)
        || !ipc.operate(|ipc| ipc.is_drained())? || !received.into_iter().all(|value| value)
    {
        return Err("initramfs processes did not complete preemption, fault isolation, IPC and exit");
    }
    Ok(())
}

fn retire(
    platform: &mut impl Platform, ipc: &mut RuntimeIpc<'_>, scheduler: &mut Scheduler,
    slot: usize, role: usize, thread: u32,
) -> Result<(), &'static str> {
    scheduler.exit(thread).map_err(|error| error.tag())?;
    if role < SERVICES {
        ipc.operate(|ipc| ipc.kill(role))?;
    }
    platform.retire(slot)
}

fn write_console(
    args: &SyscallArgs,
    translate: &impl Fn(usize, usize, bool) -> Option<*mut u8>,
) -> Result<u64, ErrorCode> {
    if !matches!(args.arg0, 1 | 2) {
        return Err(ErrorCode::Denied);
    }
    let length = args.arg2.min(PAGE_SIZE as u64) as usize;
    if length != 0 {
        let physical = translate(args.arg1 as usize, length, false).ok_or(ErrorCode::Invalid)?;
        let bytes = unsafe { core::slice::from_raw_parts(physical, length) };
        crate::arch::console().send_bytes(bytes);
    }
    Ok(length as u64)
}
