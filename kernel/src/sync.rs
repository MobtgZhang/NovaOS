//! Non-recursive spin locks for short kernel critical sections.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::capabilities::InterruptControl;

const UNOWNED: usize = usize::MAX;

pub struct SpinLock<T> {
    owner: AtomicUsize,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { owner: AtomicUsize::new(UNOWNED), value: UnsafeCell::new(value) }
    }

    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let was_masked = crate::arch::interrupts().mask();
        let cpu = crate::arch::current_cpu();
        loop {
            let owner = self.owner.load(Ordering::Relaxed);
            assert_ne!(owner, cpu, "recursive spin lock acquisition at {}", core::panic::Location::caller());
            if owner == UNOWNED && self.acquire(cpu) {
                return SpinLockGuard { lock: self, was_masked, local: PhantomData };
            }
            #[cfg(target_arch = "x86_64")]
            crate::arch::x86_64::tlb::poll(cpu);
            core::hint::spin_loop();
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let was_masked = crate::arch::interrupts().mask();
        if self.acquire(crate::arch::current_cpu()) {
            Some(SpinLockGuard { lock: self, was_masked, local: PhantomData })
        } else {
            crate::arch::interrupts().restore(was_masked);
            None
        }
    }

    fn acquire(&self, cpu: usize) -> bool {
        self.owner.compare_exchange(UNOWNED, cpu, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    pub fn held_by_current(&self) -> bool {
        self.owner.load(Ordering::Relaxed) == crate::arch::current_cpu()
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    was_masked: bool,
    local: PhantomData<*mut ()>,
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(UNOWNED, Ordering::Release);
        crate::arch::interrupts().restore(self.was_masked);
    }
}
