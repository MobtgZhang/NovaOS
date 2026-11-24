use nova_abi::error::ErrorCode;
use nova_abi::layout::{PAGE_SIZE, USER_HEAP_BASE, USER_HEAP_SIZE};
use nova_abi::syscall::ObjectType;
use nova_cap::{ObjectRef, RevocationScope};
use nova_core::address_space::{Protection, VmmError};

use super::{KernelState, object::{MAX_OBJECTS, ObjectKind}};
use crate::mm::address_space::{self, AddressSpace};

const MAX_MAPPINGS: usize = 256;

#[derive(Clone, Copy)]
struct Mapping {
    object: ObjectRef,
    task: usize,
    space: AddressSpace,
    start: u64,
    length: u64,
}

pub(super) struct Mappings([Option<Mapping>; MAX_MAPPINGS]);

impl Mappings {
    pub(super) const fn new() -> Self {
        Self([None; MAX_MAPPINGS])
    }
}

pub(super) fn release_frames(base: u64, size: u64) {
    let start = (base / PAGE_SIZE) as usize;
    let count = (size / PAGE_SIZE) as usize;
    let allocator = unsafe { crate::mm::frame_allocator() };
    for frame in start..start + count {
        unsafe { allocator.dealloc(frame) };
    }
}

impl KernelState {
    pub(crate) fn map_memory(
        &mut self,
        task: usize,
        object: ObjectRef,
        space: AddressSpace,
        start: u64,
        length: u64,
        protection: Protection,
    ) -> Result<u64, ErrorCode> {
        let Some(&ObjectKind::MemoryObject { base, size }) = self.objects.get(object) else {
            return Err(ErrorCode::Invalid);
        };
        if length == 0 || length > size || length % PAGE_SIZE != 0 || start % PAGE_SIZE != 0 {
            return Err(ErrorCode::Invalid);
        }
        let free_slot = self.mappings.0.iter().position(Option::is_none).ok_or(ErrorCode::Quota)?;
        let _access = address_space::user_access();
        let selected = if start == 0 {
            find_heap_range(&space, length).ok_or(ErrorCode::NoMemory)?
        } else {
            start
        };
        let end = selected.checked_add(length).ok_or(ErrorCode::Invalid)?;
        let window = address_space::user_window();
        if selected < window.start || end > window.end {
            return Err(ErrorCode::Invalid);
        }
        if (selected..end).step_by(PAGE_SIZE as usize).any(|page| {
            address_space::translate(&space, page).is_some()
        }) {
            return Err(ErrorCode::Busy);
        }
        for page in (selected..end).step_by(PAGE_SIZE as usize) {
            if let Err(error) = address_space::map(&space, page, base + (page - selected), protection) {
                address_space::revoke_user_range(&space, selected, page - selected, base);
                return Err(match error {
                    VmmError::OutOfMemory => ErrorCode::NoMemory,
                    VmmError::AlreadyMapped => ErrorCode::Busy,
                    _ => ErrorCode::Invalid,
                });
            }
        }
        self.mappings.0[free_slot] = Some(Mapping { object, task, space, start: selected, length });
        Ok(if start == 0 { selected } else { length / PAGE_SIZE })
    }

    pub(super) fn destroy_memory(&mut self, object: ObjectRef) {
        let Some(&ObjectKind::MemoryObject { base, size }) = self.objects.get(object) else {
            return;
        };
        let _access = address_space::user_access();
        self.compositor.detach_buffer(object);
        for entry in &mut self.mappings.0 {
            if let Some(mapping) = entry.filter(|mapping| mapping.object == object) {
                address_space::revoke_user_range(&mapping.space, mapping.start, mapping.length, base);
                *entry = None;
            }
        }
        super::sync::cancel_range(base, size);
        for table in &mut self.caps {
            table.revoke_object(object, RevocationScope::Descendants);
            table.remove_object(object);
        }
        assert!(self.objects.destroy(object));
        release_frames(base, size);
    }

    pub(super) fn remove_task_mappings(&mut self, task: usize) {
        let _access = address_space::user_access();
        for entry in &mut self.mappings.0 {
            if let Some(mapping) = entry.filter(|mapping| mapping.task == task) {
                let Some(&ObjectKind::MemoryObject { base, .. }) = self.objects.get(mapping.object) else {
                    panic!("mapping outlived its memory object");
                };
                address_space::revoke_user_range(&mapping.space, mapping.start, mapping.length, base);
                *entry = None;
            }
        }
    }

    pub(super) fn collect_unused_memory(&mut self) {
        for slot in 0..MAX_OBJECTS {
            let object = ObjectRef::new(ObjectType::MemoryObject, slot as u32);
            if self.objects.get(object).is_some()
                && !self.mappings.0.iter().flatten().any(|mapping| mapping.object == object)
                && !self.compositor.has_buffer(object)
                && !self.caps.iter().any(|table| table.has_live_object(object))
            {
                self.destroy_memory(object);
            }
        }
    }

    pub(super) fn reset_memory(&mut self) {
        for slot in 0..MAX_OBJECTS {
            self.destroy_memory(ObjectRef::new(ObjectType::MemoryObject, slot as u32));
        }
    }
}

fn find_heap_range(space: &AddressSpace, length: u64) -> Option<u64> {
    let mut first = USER_HEAP_BASE;
    for page in (USER_HEAP_BASE..USER_HEAP_BASE + USER_HEAP_SIZE).step_by(PAGE_SIZE as usize) {
        if address_space::translate(space, page).is_some() {
            first = page + PAGE_SIZE;
        } else if page + PAGE_SIZE - first == length {
            return Some(first);
        }
    }
    None
}
