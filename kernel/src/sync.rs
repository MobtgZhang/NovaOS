//! An interrupt-safe recursive spin lock.
//!
//! # Why recursive
//!
//! The lock guards the kernel's shared mutable state (the service layer, the
//! frame allocator). Its call graph is *not* flat: the syscall dispatcher
//! holds the service state and calls helpers that historically acquired the
//! same state themselves. A plain spin lock would deadlock on that
//! re-entrancy the first time a dispatch path reached for the state it
//! already held. Recursion keyed by the holding *CPU slot* is what makes the
//! re-entrancy detectable: the owner is not a thread (this kernel has no
//! thread identity for ring-0 code), it is the processor itself.
//!
//! # Why interrupt-safe
//!
//! A holder runs with interrupts masked (every caller does), so an interrupt
//! on the holding CPU can never try to re-take the lock the holder already
//! owns — the classic interrupt-deadlock of non-recursive spin locks. The
//! mask discipline is the callers' contract; this type only needs the
//! recursion to make the same-CPU re-entry case safe.
//!
//! # Fairness
//!
//! None, deliberately: contention is short (service dispatch runs to
//! completion without blocking inside the lock), and a ticket queue would
//! cost more than the contention it prevents.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The slot index meaning "no CPU holds this lock". Slot 0 is a valid CPU, so
/// the sentinel is one past the last index.
const UNOWNED: usize = usize::MAX;

/// A recursive spin lock, safe under interrupts.
///
/// `T` is the guarded value. [`lock`](Self::lock) returns a guard that
/// dereferences to `T`; dropping the guard releases one level of the
/// recursion.
pub struct SpinLock<T> {
    value: UnsafeCell<T>,
    /// `UNOWNED`, or the CPU slot of the (possibly re-entrant) holder.
    owner: AtomicUsize,
    /// How many times the owning CPU has acquired without releasing.
    depth: AtomicUsize,
}

// SAFETY: the lock disciplines all access: a `&mut T` escapes only through a
// guard held by the owning CPU, and every other CPU spins until the guard is
// dropped.
unsafe impl<T> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// A lock over `value`, unheld.
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            owner: AtomicUsize::new(UNOWNED),
            depth: AtomicUsize::new(0),
        }
    }

    /// Acquire the lock, re-entrantly when the current CPU already holds it.
    ///
    /// Spins with interrupts *as the caller left them*: every caller in this
    /// kernel runs with interrupts masked, and a masked spinner cannot
    /// preempt the holder into releasing — the holder is always making
    /// progress on another CPU.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let cpu = crate::arch::current_cpu();

        if self.owner.load(Ordering::Relaxed) == cpu {
            // Re-entrant acquire: the same processor is already inside. The
            // depth is what makes the final release drop the lock only when
            // the outermost guard falls.
            self.depth.fetch_add(1, Ordering::Relaxed);
            return SpinLockGuard {
                lock: self,
                recursive: true,
            };
        }

        // Compare-exchange so the owner slot names exactly one CPU.
        while self
            .owner
            .compare_exchange_weak(UNOWNED, cpu, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        self.depth.store(1, Ordering::Relaxed);

        SpinLockGuard {
            lock: self,
            recursive: false,
        }
    }

    /// Whether the current CPU holds this lock (at any depth).
    ///
    /// A diagnostic for `debug_assert!`s at call sites whose contracts are
    /// otherwise invisible.
    pub fn held_by_current(&self) -> bool {
        self.owner.load(Ordering::Relaxed) == crate::arch::current_cpu()
    }

    /// Release one level of the recursion. Internal; the guard's `Drop` calls
    /// this.
    fn release(&self, recursive: bool) {
        if recursive {
            let left = self.depth.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(left > 1, "a recursive guard released below depth one");
            return;
        }
        debug_assert_eq!(
            self.depth.load(Ordering::Relaxed),
            1,
            "the outermost guard must outlive every recursive one"
        );
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(UNOWNED, Ordering::Release);
    }
}

/// A held [`SpinLock`], dereferencing to the guarded value.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// Whether this guard is a re-entrant acquisition — it must not clear the
    /// owner slot on drop.
    recursive: bool,
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard's CPU owns the lock, so no other CPU can be
        // writing through its own guard.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `Deref`, with exclusivity proven by the owner slot.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release(self.recursive);
    }
}
