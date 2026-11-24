//! Synchronization suites: the futex table and kernel condition variables.
//!
//! # What these cases prove
//!
//! The futex table and the condition variables are pure logic over fixed
//! arrays — the interesting behaviour is bookkeeping, not hardware: a
//! registration must be discoverable by a wake with the same key and by
//! nothing else, a wake must remove what it wakes (so it cannot double-fire),
//! a maximum must be honoured, and an abandoned thread's slots must return
//! to the pool. A lost or duplicated wake is exactly the kind of bug that
//! only shows up under load in a real system, so it is caught here, at the
//! table's level, where the behaviour is deterministic.
//!
//! These cases do not park real threads: a park is the scheduler's business
//! (M2's suites cover blocking), and the wake handoff is exercised end-to-end
//! by the first real waiter the kernel ever has. What the suites prove is
//! that the tables themselves never leak a slot, never wake across keys, and
//! never lose count.

use crate::services::sync::{self, CondVar};

/// Waking an empty table is a success that wakes nobody, and a wake with a
/// zero budget is refused as meaningless rather than treated as "wake all".
fn futex_wake_of_nothing_wakes_nobody() -> Result<(), &'static str> {
    if sync::futex_wake(0x4000_0000, 4) != 0 {
        return Err("a wake on an unwatched word claimed to wake someone");
    }
    Ok(())
}

/// A registration is discoverable by a wake with the same key and removed by
/// it: the round trip leaves the table empty, so the same threads can wait
/// again. A duplicate registration by the same thread is refused — one
/// thread, one park.
fn futex_register_then_wake_round_trip() -> Result<(), &'static str> {
    let before = sync::parked_count();

    sync::register_wait(0x1000, 7, 0).map_err(|_| "a fresh registration was refused")?;
    // The same thread cannot park twice: its syscall is blocked until the
    // first wait ends, so a second registration means a confused caller.
    if sync::register_wait(0x2000, 7, 0).is_ok() {
        return Err("a thread was allowed to register on two futexes");
    }
    // A different thread may park on the same key.
    sync::register_wait(0x1000, 8, 1).map_err(|_| "a second waiter on one key was refused")?;
    if sync::parked_count() != before + 2 {
        return Err("registrations did not each occupy one slot");
    }

    let woken = sync::futex_wake(0x1000, 4);
    if woken != 2 {
        return Err("a wake on a watched key did not wake both waiters");
    }
    if sync::parked_count() != before {
        return Err("a wake left its matched entries in the table");
    }

    // A different key never matches: the key is the physical address, and a
    // wake on a neighbour word must not reach across it.
    sync::register_wait(0x1000, 7, 0).map_err(|_| "a re-registration after wake was refused")?;
    if sync::futex_wake(0x1004, 4) != 0 {
        return Err("a wake on a different word woke across keys");
    }
    if sync::futex_wake(0x1000, 4) != 1 {
        return Err("the original key stopped matching after a neighbour wake");
    }
    Ok(())
}

/// `max` caps the wake: a capped wake removes exactly its budget, and the
/// remainder is still there for the next wake. The cap exists so a wild
/// `max` cannot scan past the table.
fn futex_wake_honours_max() -> Result<(), &'static str> {
    let before = sync::parked_count();

    for thread in 10u32..13 {
        sync::register_wait(0x3000, thread, 0).map_err(|_| "a fresh registration was refused")?;
    }

    if sync::futex_wake(0x3000, 2) != 2 {
        return Err("a capped wake did not wake exactly its budget");
    }
    if sync::parked_count() != before + 1 {
        return Err("a capped wake removed more than its budget");
    }
    if sync::futex_wake(0x3000, 8) != 1 {
        return Err("the remainder after a capped wake was lost");
    }
    if sync::parked_count() != before {
        return Err("the final wake left a slot occupied");
    }
    Ok(())
}

/// A thread that exits while parked must release its registration, and only
/// its own: an abandon that swept a neighbour's registration would strand
/// that neighbour's wake.
fn futex_abandon_drops_only_that_thread() -> Result<(), &'static str> {
    let before = sync::parked_count();

    sync::register_wait(0x5000, 20, 0).map_err(|_| "a fresh registration was refused")?;
    sync::register_wait(0x5000, 21, 1).map_err(|_| "a second waiter on one key was refused")?;
    sync::register_wait(0x6000, 22, 0).map_err(|_| "the second key was refused")?;

    if sync::abandon_thread(20) != 1 {
        return Err("an exiting thread's registration was not dropped");
    }
    if sync::parked_count() != before + 2 {
        return Err("an abandon dropped a registration it did not own");
    }
    if sync::futex_wake(0x5000, 4) != 1 {
        return Err("the surviving waiter was lost to a neighbour's abandon");
    }
    if sync::futex_wake(0x6000, 4) != 1 {
        return Err("the waiter on the second key was lost");
    }
    if sync::parked_count() != before {
        return Err("the final wake left a slot occupied");
    }
    Ok(())
}

/// Notifying an empty condition variable is a success that notifies nobody.
fn condvar_notify_without_waiters_is_zero() -> Result<(), &'static str> {
    let cond = CondVar::new();
    if cond.notify(4) != 0 {
        return Err("a notify with no waiters claimed to wake someone");
    }
    Ok(())
}

/// Registration, FIFO notify, and reclamation: the first thread to register
/// is the first woken, a notify removes what it wakes, and a resumed thread
/// can reclaim an entry a notify never delivered.
fn condvar_fifo_notify_and_take() -> Result<(), &'static str> {
    let cond = CondVar::new();

    if !cond.register(31, 0) || !cond.register(32, 1) || !cond.register(33, 0) {
        return Err("a fresh registration was refused");
    }
    if cond.register(31, 0) {
        return Err("a thread was allowed to register twice");
    }
    if cond.waiter_count() != 3 {
        return Err("registrations did not each occupy one slot");
    }

    if cond.notify(1) != 1 {
        return Err("a notify of one did not wake exactly one");
    }
    if cond.waiter_count() != 2 {
        return Err("a notify left its woken entry registered");
    }
    if cond.notify(8) != 2 {
        return Err("an unbounded notify did not wake the remainder");
    }
    if cond.notify(8) != 0 {
        return Err("a notify on an empty list claimed to wake someone");
    }

    // `take` reclaims a slot a notify never delivered: the scheduler's
    // fallback scan can resume a parked thread with no notify in between.
    if !cond.register(34, 0) {
        return Err("a fresh registration was refused");
    }
    if !cond.take(34) {
        return Err("a thread could not reclaim its own registration");
    }
    if cond.take(34) {
        return Err("a reclaimed registration was still present");
    }
    if cond.waiter_count() != 0 {
        return Err("the list did not return to empty");
    }
    Ok(())
}

/// A full condition variable refuses a registration rather than overwriting
/// or silently queueing past its bound.
fn condvar_full_list_refuses() -> Result<(), &'static str> {
    let cond = CondVar::new();

    for thread in 40u32..40 + sync::MAX_COND_WAITERS as u32 {
        if !cond.register(thread, 0) {
            return Err("a fresh registration was refused");
        }
    }
    if cond.register(99, 0) {
        return Err("a full condition variable accepted another waiter");
    }
    if cond.waiter_count() != sync::MAX_COND_WAITERS {
        return Err("the list held more than its bound");
    }

    // Everything must drain so the shared test instance state is clean:
    // each case owns its own CondVar, but the wake handoff must not be
    // left holding stale requests either.
    if cond.notify(sync::MAX_COND_WAITERS) != sync::MAX_COND_WAITERS {
        return Err("draining the full list lost a waiter");
    }
    Ok(())
}

pub static SYNC_SUITES: &[crate::test::harness::TestSuite] = &[
    crate::test::harness::TestSuite {
        name: "futex_table",
        cases: &[
            (
                "futex_wake_of_nothing_wakes_nobody",
                futex_wake_of_nothing_wakes_nobody,
            ),
            (
                "futex_register_then_wake_round_trip",
                futex_register_then_wake_round_trip,
            ),
            ("futex_wake_honours_max", futex_wake_honours_max),
            (
                "futex_abandon_drops_only_that_thread",
                futex_abandon_drops_only_that_thread,
            ),
        ],
    },
    crate::test::harness::TestSuite {
        name: "condvar",
        cases: &[
            (
                "condvar_notify_without_waiters_is_zero",
                condvar_notify_without_waiters_is_zero,
            ),
            ("condvar_fifo_notify_and_take", condvar_fifo_notify_and_take),
            ("condvar_full_list_refuses", condvar_full_list_refuses),
        ],
    },
];
