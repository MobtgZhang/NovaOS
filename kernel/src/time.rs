//! The wall clock: the kernel's only source of *absolute* time.
//!
//! Everything else — the PIT, the APIC timers — counts intervals from an
//! arbitrary zero. Only the CMOS RTC knows what day it is, and only the
//! filesystem needs to know: timestamps are the one consumer, and this
//! module is the seam they read through.
//!
//! # Cost model
//!
//! Each reading performs a couple of dozen port I/O cycles and may wait out
//! the RTC's update cycle (up to a few hundred microseconds). That is cheap
//! next to any disk I/O a timestamp-bearing operation performs, and it
//! spares the kernel a cached "boot time" whose staleness would grow with
//! every uptime hour. Callers that would read it per-tick must not exist;
//! timestamps are taken once per file operation, if at all.

/// Seconds since 1970-01-01T00:00:00Z, from the machine's real-time clock.
///
/// `None` means the clock could not be read stably or the reading is not a
/// plausible date: a caller building a timestamp omits it rather than
/// recording nonsense. The century register index comes from the boot
/// manager's FADT walk — without it the century is inferred from the year
/// register.
pub fn wall_clock_seconds() -> Option<u64> {
    // SAFETY: the boot snapshot is published by `boot::capture` before any
    // filesystem service can run.
    let century = unsafe { crate::boot::read_boot_info() }.acpi_century;
    let reading = crate::arch::x86_64::rtc::read_datetime(century)?;
    Some(reading.unix_seconds())
}
