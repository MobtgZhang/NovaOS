//! An x86-64 process-private blocking mutex. This mutex is not recursive or
//! owner-death recoverable and does not implement priority inheritance.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use nova_abi::{ErrorCode, nr};

use crate::syscall::{SyscallError, call};

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const CONTENDED: u32 = 2;

pub struct Mutex<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { state: AtomicU32::new(UNLOCKED), value: UnsafeCell::new(value) }
    }

    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        self.state.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .ok().map(|_| self.guard())
    }

    /// Wait for exclusive access. A full kernel waiter queue returns `Quota`.
    pub fn lock(&self) -> Result<Guard<'_, T>, SyscallError> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        while self.state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
            let args = [self.word(), CONTENDED as u64, 0, 0, 0, 0];
            match call(nr::FUTEX_WAIT.raw(), args) {
                Ok(_) | Err(SyscallError::Refused(ErrorCode::WouldBlock)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.guard())
    }

    fn guard(&self) -> Guard<'_, T> {
        Guard { mutex: self, local: PhantomData }
    }

    fn word(&self) -> u64 {
        self.state.as_ptr() as u64
    }
}

pub struct Guard<'a, T> {
    mutex: &'a Mutex<T>,
    local: PhantomData<*mut ()>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            let _ = call(nr::FUTEX_WAKE.raw(), [self.mutex.word(), 1, 0, 0, 0, 0]);
        }
    }
}
