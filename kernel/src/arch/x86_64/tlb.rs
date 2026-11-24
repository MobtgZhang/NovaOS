//! Synchronous x86-64 TLB invalidation with one request per initiating CPU.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::capabilities::InterruptControl;
use crate::boot::MAX_CPUS;

struct Request {
    address: AtomicU64,
    pending: AtomicUsize,
}

static REQUESTS: [Request; MAX_CPUS] = [const {
    Request {
        address: AtomicU64::new(0),
        pending: AtomicUsize::new(0),
    }
}; MAX_CPUS];
static INBOXES: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
const FLUSH_USER: u64 = u64::MAX;

pub(crate) fn flush_user() {
    invalidate(FLUSH_USER);
}

fn invalidate_local(address: u64) {
    if address == FLUSH_USER {
        unsafe { super::paging::load_page_directory(super::paging::current_page_directory()) };
    } else {
        unsafe { super::paging::invlpg(address) };
    }
}

/// Complete a leaf change before the old mapping's backing can be reclaimed.
///
/// CR3 switches flush private translations (PCID is disabled); an unconditional
/// INVLPG also drops a global translation even when a different root is active.
/// The boot CPU set is fixed before concurrent mapping changes begin.
pub(crate) fn invalidate(address: u64) {
    let interrupts = crate::arch::interrupts();
    let was_masked = interrupts.mask();
    let cpu = crate::arch::current_cpu();
    let mut targets = 0;
    for target in 0..MAX_CPUS {
        if target != cpu && (target == 0 || crate::boot::smp::is_online(target)) {
            targets |= 1 << target;
        }
    }

    let request = &REQUESTS[cpu];
    assert_eq!(request.pending.load(Ordering::Relaxed), 0);
    request.address.store(address, Ordering::Relaxed);
    request.pending.store(targets, Ordering::Relaxed);
    for (target, inbox) in INBOXES.iter().enumerate() {
        if targets & (1 << target) != 0 {
            inbox.fetch_or(1 << cpu, Ordering::Release);
            super::apic::notify_tlb(target);
        }
    }
    invalidate_local(address);

    let started = super::cycles();
    while request.pending.load(Ordering::Acquire) != 0 {
        poll(cpu);
        assert!(
            super::cycles().wrapping_sub(started) < 10_000_000_000,
            "TLB invalidation was not acknowledged on all CPUs"
        );
        core::hint::spin_loop();
    }
    interrupts.restore(was_masked);
}

/// Service pending requests with IRQs masked, without taking any locks.
///
/// The IPI handler calls this normally. A CPU spinning with IRQs masked for
/// a lock or another shootdown also calls it to avoid circular waits.
pub(crate) fn poll(cpu: usize) -> bool {
    let inbox = &INBOXES[cpu];
    if inbox.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let initiators = inbox.swap(0, Ordering::Acquire);
    for (initiator, request) in REQUESTS.iter().enumerate() {
        if initiators & (1 << initiator) != 0 {
            let address = request.address.load(Ordering::Relaxed);
            invalidate_local(address);
            request.pending.fetch_and(!(1 << cpu), Ordering::Release);
        }
    }
    initiators != 0
}
