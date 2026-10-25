//! Loading and starting processes from the initramfs.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;

use crate::boot::BootInfo;
use crate::klog;
use crate::loader;
use crate::logger::Level;
use crate::mm::address_space;
use crate::services::{KernelState, MAX_SERVICE_NAME, MAX_TASKS};
use crate::task::{self, Entry};

/// A unique identifier for each thread created by the process layer.
struct ThreadIds(UnsafeCell<u32>);
unsafe impl Sync for ThreadIds {}

static NEXT_THREAD_ID: ThreadIds = ThreadIds(UnsafeCell::new(1));

/// Allocate a unique thread identifier.
fn next_thread_id() -> u32 {
    // SAFETY: the kernel is single-CPU and syscall entry runs with interrupts
    // masked, so this counter has no concurrent access.
    let next = unsafe { &mut *NEXT_THREAD_ID.0.get() };
    let id = *next;
    *next = next.wrapping_add(1);
    id
}

/// Execute `name` from the initramfs as a child of the current task.
pub fn exec_image(name: &[u8]) -> Result<CapHandle, ErrorCode> {
    let context = task::current_syscall_context();
    // SAFETY: service state is installed before user processes can run, and
    // the syscall path holds the only mutable borrow.
    let kernel = unsafe { crate::services::state() };
    crate::services::dispatch::check_spawn_authority(kernel, &context)?;
    exec_image_in(name, context.task_slot)
}

/// Start `name` from the initramfs as a child of `parent_slot`.
pub fn exec_image_in(name: &[u8], parent_slot: usize) -> Result<CapHandle, ErrorCode> {
    // SAFETY: boot capture has run, and syscall entry runs with interrupts
    // masked; this is the same contract as the rest of the syscall path.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    // SAFETY: service state is installed before user processes can run, and
    // the syscall path holds the only mutable borrow.
    let kernel = unsafe { crate::services::state() };
    exec_image_in_state(kernel, name, parent_slot, boot_info)
}

/// Start an image against an explicit kernel state.
///
/// The kernel's real state and the test state are separate static instances.
/// Keeping the execution logic behind this function lets the test suites
/// exercise the same authority and spawn logic without observing the live
/// boot state.
pub(crate) fn exec_image_in_state(
    kernel: &mut KernelState,
    name: &[u8],
    parent_slot: usize,
    boot_info: &'static BootInfo,
) -> Result<CapHandle, ErrorCode> {
    let Some(archive_bytes) = crate::boot::initramfs() else {
        klog!(Level::Error, "process: no initramfs");
        return Err(ErrorCode::Unsupported);
    };
    let archive =
        crate::initramfs::Archive::parse(archive_bytes).map_err(|_| ErrorCode::Invalid)?;
    if name.len() > MAX_SERVICE_NAME {
        klog!(Level::Error, "process: service name overflow");
        return Err(ErrorCode::Invalid);
    }
    let image = archive.get(name).ok_or(ErrorCode::BadHandle)?;

    if parent_slot >= MAX_TASKS {
        klog!(Level::Error, "process: parent slot overflow");
        return Err(ErrorCode::BadHandle);
    }

    crate::services::dispatch::check_spawn_authority_at_slot(kernel, parent_slot)?;

    let Some(child_task) = kernel.create_bare_task() else {
        klog!(Level::Error, "process: no task slot");
        return Err(ErrorCode::NoMemory);
    };
    let Some(child_slot) = kernel.slot_of_newest_task(child_task) else {
        klog!(Level::Error, "process: no new task slot");
        return Err(ErrorCode::NoMemory);
    };

    let space = address_space::create_process_space(boot_info).map_err(|_| ErrorCode::NoMemory)?;
    let loaded = loader::load(&space, image, boot_info).map_err(|error| {
        klog!(Level::Error, "process: load failed {error}");
        ErrorCode::NoMemory
    })?;

    let Some((parent_endpoint, child_endpoint)) = kernel.create_endpoint_pair(
        parent_slot,
        child_slot,
        Rights::SEND | Rights::RECEIVE | Rights::REPLY,
        Rights::SEND | Rights::RECEIVE | Rights::REPLY,
    ) else {
        klog!(Level::Error, "process: no endpoint pair");
        return Err(ErrorCode::NoMemory);
    };

    if let Some(child) = kernel.task_at_slot_mut(child_slot) {
        child.management_endpoint = child_endpoint;
        child.service_name[..name.len()].copy_from_slice(name);
        child.service_name_len = name.len();
    }

    let Some(mut thread) = task::create_thread(
        next_thread_id(),
        child_slot,
        space,
        Entry::User {
            rip: loaded.entry,
            rsp: loaded.stack_pointer,
        },
    ) else {
        klog!(Level::Error, "process: no thread slot");
        return Err(ErrorCode::NoMemory);
    };

    // SAFETY: the thread owns a kernel stack from `create_thread`, and the
    // scheduler is only mutated with interrupts masked.
    unsafe {
        crate::sched::spawn(&mut thread);
    }

    Ok(parent_endpoint)
}

/// Boot-time entry point: create the root supervisor and start `init`.
pub fn start_init_image(name: &[u8]) -> Result<CapHandle, ErrorCode> {
    // SAFETY: service state was installed during boot, before any user thread
    // existed, and the boot path runs serialised with interrupts masked.
    let kernel = unsafe { crate::services::state() };
    let Some(id) = kernel.create_task() else {
        klog!(Level::Error, "process: init has no task slot");
        return Err(ErrorCode::NoMemory);
    };
    let Some(slot) = kernel.slot_of_newest_task(id) else {
        klog!(Level::Error, "process: init has no task slot index");
        return Err(ErrorCode::NoMemory);
    };
    klog!(Level::Info, "process: init task {slot} created");
    let result = exec_image_in(name, slot);
    klog!(Level::Info, "process: init exec result={result:?}");
    return result;
}

/// The number of tasks that are still alive.
pub fn live_task_count() -> usize {
    // SAFETY: the syscall path is the only reader here and runs serialised.
    unsafe {
        crate::services::state()
            .tasks
            .iter()
            .flatten()
            .filter(|task| task.alive)
            .count()
    }
}

/// Remove a dead task and release its slot.
pub fn retire_task(slot: usize) -> bool {
    // SAFETY: syscall path is serialised; exiting thread cannot return.
    let kernel = unsafe { crate::services::state() };
    kernel.retire_task(slot)
}

/// Whether a live task was started from the named initramfs image.
pub fn service_is_alive(name: &[u8]) -> bool {
    // SAFETY: syscall path is serialised.
    let kernel = unsafe { crate::services::state() };
    kernel.service_is_alive(name)
}

/// Create both sides of an endpoint pair in the calling task.
pub fn create_endpoint_pair() -> Result<CapHandle, ErrorCode> {
    // SAFETY: service state is installed before user processes can run.
    let kernel = unsafe { crate::services::state() };
    let context = task::current_syscall_context();
    let slot = context.task_slot;
    if slot >= MAX_TASKS {
        return Err(ErrorCode::BadHandle);
    }
    let rights = Rights::SEND | Rights::RECEIVE | Rights::REPLY;
    let Some((first, second)) = kernel.create_endpoint_pair(slot, slot, rights, rights) else {
        return Err(ErrorCode::NoMemory);
    };
    if let Some(task) = kernel.task_at_slot_mut(slot) {
        task.pending_endpoint = second;
    }
    Ok(first)
}

/// Return the side of a pair created by [`create_endpoint_pair`].
pub fn take_pending_endpoint() -> Result<CapHandle, ErrorCode> {
    // SAFETY: service state is installed before user processes can run.
    let kernel = unsafe { crate::services::state() };
    let context = task::current_syscall_context();
    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return Err(ErrorCode::BadHandle);
    };
    let handle = task.pending_endpoint;
    task.pending_endpoint = CapHandle::INVALID;
    if handle.is_invalid() {
        Err(ErrorCode::BadHandle)
    } else {
        Ok(handle)
    }
}

/// Return the management endpoint this process received at spawn.
pub fn management_endpoint() -> Result<CapHandle, ErrorCode> {
    // SAFETY: service state is installed before user processes can run.
    let kernel = unsafe { crate::services::state() };
    let context = task::current_syscall_context();
    let Some(task) = kernel.task_at_slot(context.task_slot) else {
        return Err(ErrorCode::BadHandle);
    };
    let handle = task.management_endpoint;
    if handle.is_invalid() {
        Err(ErrorCode::BadHandle)
    } else {
        Ok(handle)
    }
}
use core::cell::UnsafeCell;
