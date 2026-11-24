//! Synchronization services: the futex table and kernel condition variables.
//!
//! # The futex
//!
//! A futex lets a user thread park on a 32-bit word instead of spinning until
//! the word changes. The kernel keeps no per-word allocation: the table below
//! is a fixed array of `(physical address, thread, home CPU)` slots, so a
//! wait can be refused when the table is full rather than failing an
//! allocation in the syscall path.
//!
//! The key is the word's **physical** address, translated through the
//! caller's address space at syscall time. Two processes that map the same
//! memory object therefore wait on the same key and wake each other; two
//! processes that each map their own private page at the same virtual
//! address do not.
//!
//! # The lost-wakeup race and how the wait closes it
//!
//! The classic futex race is: the waiter reads the value, then another task
//! changes it and wakes — and only then does the waiter park, sleeping
//! forever. The value check and insertion share the queue lock with wake.
//! The home CPU keeps interrupts masked until it marks the thread blocked,
//! so a wake queued before that transition is only delivered after the park.
//!
//! # The wake mechanism
//!
//! Waking never touches another CPU's run queue. [`futex_wake`] removes the
//! matched entries and hands each one to
//! [`sched::request_wake`](crate::sched::request_wake), which queues the
//! request for the thread's home CPU; the home CPU drains its queue at its
//! next timer tick. The tick fires even while the home CPU idles in `hlt`,
//! so a parked thread wakes at most one tick late.
//!
//! # The condition variable
//!
//! `CondVar` is the futex idea for kernel code: a fixed waiter list plus
//! notify, delivered through the same wake handoff. It is deliberately
//! **split-phase** rather than a synchronous `wait(&guard)`: blocking in
//! this kernel destroys the calling stack — the block path saves the
//! thread's context and switches away, and the thread resumes from its
//! *saved* context, not from the middle of a Rust function — so a `wait`
//! that blocked and returned could not exist. The caller (a syscall path or
//! kernel service) registers itself, releases whatever it held, returns a
//! `Block` effect so the entry path parks the thread, and reclaims its entry
//! with [`CondVar::take`] once it is resumed. See [`CondVar`] for the
//! lost-wakeup bound this split implies.

use nova_abi::error::ErrorCode;

use crate::sync::SpinLock;

/// How many threads may be parked on futexes at once.
///
/// One slot per parked thread, not one per word: several threads wait on the
/// same word by holding several slots with the same key.
pub const MAX_FUTEX_WAITERS: usize = 32;

/// One parked futex waiter.
struct FutexWaiter {
    used: bool,
    /// The physical address of the watched word: the wait key.
    phys: u64,
    /// The parked thread's identifier.
    thread: u32,
    /// The CPU whose pool owns the thread; the wake is routed there.
    cpu: usize,
}

impl FutexWaiter {
    const fn empty() -> Self {
        Self {
            used: false,
            phys: 0,
            thread: 0,
            cpu: 0,
        }
    }
}

static FUTEX_TABLE: SpinLock<[FutexWaiter; MAX_FUTEX_WAITERS]> =
    SpinLock::new([const { FutexWaiter::empty() }; MAX_FUTEX_WAITERS]);

/// Register a thread as parked on the word at physical address `phys`.
///
/// A thread may park on only one word at a time — its syscall is blocked
/// until it wakes, so a second registration would mean a confused caller —
/// and the table refuses to grow beyond [`MAX_FUTEX_WAITERS`] slots.
///
/// # Errors
///
/// [`ErrorCode::Quota`] when the table is full, [`ErrorCode::Invalid`] when
/// the thread already has a registration.
pub fn register_wait(phys: u64, thread: u32, cpu: usize) -> Result<(), ErrorCode> {
    register_wait_if(phys, thread, cpu, || true)
}

pub fn register_wait_if(
    phys: u64, thread: u32, cpu: usize, matches: impl FnOnce() -> bool,
) -> Result<(), ErrorCode> {
    let mut table = FUTEX_TABLE.lock();

    if !matches() {
        return Err(ErrorCode::WouldBlock);
    }
    if table
        .iter()
        .any(|entry| entry.used && entry.thread == thread)
    {
        return Err(ErrorCode::Invalid);
    }
    let slot = table.iter_mut().find(|entry| !entry.used).ok_or(ErrorCode::Quota)?;
    *slot = FutexWaiter {
        used: true,
        phys,
        thread,
        cpu,
    };
    Ok(())
}

/// Wake up to `max` threads parked on the word at physical address `phys`.
///
/// Each woken entry leaves the table, so a wake never double-fires.
/// Returns how many threads were woken; zero when nobody is parked on the
/// word, which is success, not an error.
pub fn futex_wake(phys: u64, max: usize) -> usize {
    wake_matching(max, |address| address == phys)
}

pub(super) fn cancel_range(base: u64, size: u64) {
    wake_matching(MAX_FUTEX_WAITERS, |address| (base..base + size).contains(&address));
}

fn wake_matching(max: usize, matches: impl Fn(u64) -> bool) -> usize {
    if max == 0 {
        return 0;
    }

    // The woken identities are collected under the lock and delivered after
    // dropping it: `request_wake` takes its own lock, and keeping nested
    // lock orders out of the design needs no justification later.
    let mut woken = [(0u32, 0usize); MAX_FUTEX_WAITERS];
    let mut count = 0usize;

    {
        let mut table = FUTEX_TABLE.lock();
        for entry in table.iter_mut() {
            if count == max {
                break;
            }
            if entry.used && matches(entry.phys) {
                woken[count] = (entry.thread, entry.cpu);
                count += 1;
                *entry = FutexWaiter::empty();
            }
        }
    }

    for &(thread, cpu) in woken[..count].iter() {
        // `request_wake` routes to the home CPU recorded at registration;
        // the waking CPU never touches the sleeping thread's queue.
        crate::sched::request_wake(thread, cpu);
    }
    count
}

/// Drop every registration held by `thread`.
///
/// Called when the thread exits: a wait never outlives its waiter, and a
/// dead thread's slot must return to the pool rather than hold a key nobody
/// can wake into a running thread again. (A stale entry is not fatal — the
/// next wake of its key removes it — but prompt reclamation keeps the table
/// usable under churn.)
///
/// Returns how many registrations were dropped.
pub fn abandon_thread(thread: u32) -> usize {
    let mut table = FUTEX_TABLE.lock();
    let mut dropped = 0usize;
    for entry in table.iter_mut() {
        if entry.used && entry.thread == thread {
            *entry = FutexWaiter::empty();
            dropped += 1;
        }
    }
    dropped
}

/// How many threads are parked right now. Diagnostic; the test suites use it.
pub fn parked_count() -> usize {
    FUTEX_TABLE.lock().iter().filter(|entry| entry.used).count()
}

// ---------------------------------------------------------------------------
// Condition variables
// ---------------------------------------------------------------------------

/// How many threads one condition variable may hold at once.
pub const MAX_COND_WAITERS: usize = 8;

/// One registered condition waiter.
#[derive(Clone, Copy)]
struct CondWaiter {
    thread: u32,
    cpu: usize,
}

/// The waiter list of one condition variable.
struct CondWaiters {
    entries: [Option<CondWaiter>; MAX_COND_WAITERS],
    len: usize,
}

impl CondWaiters {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_COND_WAITERS],
            len: 0,
        }
    }
}

/// A kernel condition variable.
///
/// A fixed waiter list plus notify, delivered through the same
/// [`sched::request_wake`](crate::sched::request_wake) handoff the futex
/// uses. It is deliberately **split-phase** rather than a synchronous
/// `wait(&guard)`: blocking in this kernel destroys the calling stack — the
/// block path saves the thread's context and switches away, and the thread
/// resumes from its *saved* context, not from the middle of a Rust function
/// — so a `wait` that blocked and returned could not exist. The caller (a
/// syscall path or kernel service) registers itself, releases whatever it
/// held, returns a `Block` effect so the entry path parks the thread, and
/// reclaims its entry with [`take`](Self::take) once it is resumed.
///
/// A notify that lands between the registration and the park wakes a thread
/// that has not yet parked. The scheduler's `unblock` is idempotent — waking
/// an already runnable thread merely queues it, and the scheduler's idle
/// fallback re-scans for blocked-but-alive threads — so the window degrades
/// to a late wake, not a lost one. That is the honest bound of a wake
/// mechanism with no runtime IPI; P3 replaces the handoff with one.
pub struct CondVar {
    waiters: SpinLock<CondWaiters>,
}

impl CondVar {
    /// An empty condition variable.
    pub const fn new() -> Self {
        Self {
            waiters: SpinLock::new(CondWaiters::new()),
        }
    }

    /// Register `thread` (on `cpu`) as waiting on this variable.
    ///
    /// Returns `false` when the list is full or the thread is already
    /// registered; the caller must then refuse to park rather than wait
    /// forever.
    pub fn register(&self, thread: u32, cpu: usize) -> bool {
        let mut waiters = self.waiters.lock();
        if waiters.len == MAX_COND_WAITERS {
            return false;
        }
        if waiters
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.thread == thread)
        {
            return false;
        }
        let slot = waiters.len;
        waiters.entries[slot] = Some(CondWaiter { thread, cpu });
        waiters.len += 1;
        true
    }

    /// Remove `thread`'s own registration, if present.
    ///
    /// Called after a parked thread resumes: the notifier removes the entry
    /// when it delivers the wake, so a still-present entry means the thread
    /// resumed without a notify (the scheduler's fallback scan) and must
    /// reclaim its slot itself.
    pub fn take(&self, thread: u32) -> bool {
        let mut waiters = self.waiters.lock();
        let len = waiters.len;
        for index in 0..len {
            let matches = matches!(&waiters.entries[index], Some(entry) if entry.thread == thread);
            if matches {
                // Compact: shift the tail down so registration order is
                // preserved for the next notify.
                for shift in index..len - 1 {
                    waiters.entries[shift] = waiters.entries[shift + 1];
                }
                waiters.entries[len - 1] = None;
                waiters.len -= 1;
                return true;
            }
        }
        false
    }

    /// Wake up to `max` waiters, oldest first.
    ///
    /// Each woken thread leaves the list and is queued for a wake on its
    /// home CPU. Returns how many were woken.
    pub fn notify(&self, max: usize) -> usize {
        let mut woken = [(0u32, 0usize); MAX_COND_WAITERS];
        let mut count = 0usize;

        {
            let mut waiters = self.waiters.lock();
            while count < max && waiters.len > 0 {
                let Some(entry) = waiters.entries[0] else {
                    break;
                };
                woken[count] = (entry.thread, entry.cpu);
                count += 1;
                let len = waiters.len;
                for shift in 0..len - 1 {
                    waiters.entries[shift] = waiters.entries[shift + 1];
                }
                waiters.entries[len - 1] = None;
                waiters.len -= 1;
            }
        }

        for &(thread, cpu) in woken[..count].iter() {
            crate::sched::request_wake(thread, cpu);
        }
        count
    }

    /// How many threads are registered right now. Diagnostic; tests use it.
    pub fn waiter_count(&self) -> usize {
        self.waiters.lock().len
    }
}
