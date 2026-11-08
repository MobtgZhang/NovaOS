//! The CMOS RTC: the machine's wall clock.
//!
//! The RTC is the only source of *absolute* time in the machine — everything
//! else (PIT, APIC timer) counts elapsed intervals from an arbitrary zero.
//! Filesystem timestamps (§6.2) need absolute time, so this module turns the
//! battery-backed clock into seconds-since-the-Unix-epoch.
//!
//! # The hardware's quirks, all handled here
//!
//! * **BCD versus binary** — status register B's data-mode bit says which;
//!   firmware defaults to BCD and QEMU keeps that default.
//! * **12-hour versus 24-hour** — status register B's hour-form bit, with the
//!   hour register's high bit doubling as the PM flag in 12-hour mode.
//! * **The update cycle** — once a second the RTC updates its registers and
//!   tears any read spanning the update. Status register A's UIP flag warns
//!   about the window; the read below waits it out and re-reads until two
//!   consecutive reads agree.
//! * **The century** — the year register holds 00–99. Which CMOS register
//!   holds the century is machine-specific, and the FADT's `CENTURY` field
//!   names it; the boot manager carries that index through the boot
//!   protocol. With no FADT the index arrives as zero and the century is
//!   inferred as 20xx from the year register.
//! * **Port 0x70's high bit is the NMI control** — every index write must
//!   preserve it, and the caller's original NMI state is restored on the
//!   way out.
//!
//! # Concurrency
//!
//! The index/data port pair is global machine state: two CPUs writing the
//! index in between each other's data reads would read each other's
//! registers. Every access is serialized behind a spinlock, and the read
//! below is the only place that touches the ports.

use crate::arch::x86_64::port_io::{inb, outb};
use crate::sync::SpinLock;

/// The register-index port. Bit 7 is the NMI control, not an address bit.
const INDEX_PORT: u16 = 0x70;
/// The register-data port.
const DATA_PORT: u16 = 0x71;

/// The index-port bit that controls NMI delivery.
const NMI_BIT: u8 = 0x80;

/// Status register A: its low nibble is the rate, and bit 7 is UIP.
const STATUS_A: u8 = 0x0A;
/// Status register B: bit 2 is the data mode, bit 1 the hour form.
const STATUS_B: u8 = 0x0B;

/// Seconds register.
const SECONDS: u8 = 0x00;
/// Minutes register.
const MINUTES: u8 = 0x02;
/// Hours register (with the PM flag in 12-hour mode).
const HOURS: u8 = 0x04;
/// Day-of-month register.
const DAY_OF_MONTH: u8 = 0x07;
/// Month register, 1–12.
const MONTH: u8 = 0x08;
/// Year register, 00–99.
const YEAR: u8 = 0x09;

/// How many UIP-wait iterations the read tolerates before giving up.
///
/// The update cycle lasts at most a couple of hundred microseconds; each
/// iteration here performs two port writes and a port read, which are slow
/// enough that this bound is orders of magnitude past the real window.
const UIP_BUDGET: usize = 200_000;

/// How many full re-reads the torn-read detector tolerates.
///
/// The read spans a dozen registers, far shorter than the update cycle's
/// period, so two agreeing reads land on the first or second attempt; the
/// budget exists so a pathological clock becomes a `None` rather than a
/// hang.
const READ_BUDGET: usize = 8;

/// Serializes every CMOS port access in the machine.
static CMOS: SpinLock<()> = SpinLock::new(());

/// A wall-clock reading, decoded into real units.
///
/// `year` is the full year (2026, not 26); the rest are their ordinary
/// 1-based ranges with hours in 24-hour form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    /// Full year.
    pub year: u16,
    /// 1–12.
    pub month: u8,
    /// 1–31.
    pub day: u8,
    /// 0–23, always 24-hour form.
    pub hour: u8,
    /// 0–59.
    pub minute: u8,
    /// 0–59.
    pub second: u8,
}

/// Read two packed BCD digits.
///
/// `0x42` is forty-two, not sixty-six: the high nibble is the tens digit.
/// Pure arithmetic, so the host tests can pin every case.
pub fn bcd_to_binary(raw: u8) -> u8 {
    (raw & 0x0F).wrapping_add((raw >> 4).wrapping_mul(10))
}

/// Decode the hour register into 24-hour form.
///
/// `raw` is the register's byte *before* BCD decoding, because the PM flag
/// lives in bit 7 of the raw value: a BCD-decoded hour would have lost it.
/// In 12-hour mode the register holds 01–12 and 12 AM is midnight (hour 0).
pub fn decode_hour(raw: u8, twelve_hour: bool, binary: bool) -> u8 {
    let pm = raw & 0x80 != 0;
    let value = if binary {
        raw & 0x7F
    } else {
        bcd_to_binary(raw & 0x7F)
    };
    if !twelve_hour {
        return value;
    }
    match (pm, value) {
        // 12 PM is noon, 12 AM is midnight: the twelve o'clock hour is the
        // one where the flag means the opposite of what it looks like.
        (false, 12) => 0,
        (true, 12) => 12,
        (true, hour) => hour.saturating_add(12),
        (false, hour) => hour,
    }
}

/// Whether `year` counts as a leap year.
pub fn is_leap_year(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days from 1970-01-01 to the start of `year`-`month`-`day`.
///
/// The days-before-month table runs March–February so the leap day falls at
/// the very end and never shifts the earlier months — the same trick every
/// civil-date algorithm uses to avoid a per-month leap correction.
pub fn days_from_civil(year: u16, month: u8, day: u8) -> u64 {
    let year = year as i64;
    let month = month as i64;
    let day = day as i64;
    let era_year = if month <= 2 { year - 1 } else { year };
    // The month shifted so March is 0 and February is 11.
    let shifted_month = if month <= 2 { month + 9 } else { month - 3 };
    // Days from 1970-03-01 to `era_year`-`shifted_month`-01, in the shifted
    // calendar: 400-year eras, then the century remainder, then 4-year
    // cycles, then the months themselves (each 30/31 days in the March-
    // based ordering, which is what the 153/5 encodes).
    let era = era_year.div_euclid(400);
    let era_start = era * 400;
    let year_of_era = era_year - era_start;
    let days = era * 146_097
        + year_of_era * 365
        + year_of_era / 4
        - year_of_era / 100
        + (shifted_month * 153 + 2) / 5
        + day
        // 1970-01-01 is day 719_468 of this era calendar, counted from the
        // March new-year with the day itself 1-based.
        - 719_469;
    days.max(0) as u64
}

/// Days in `month` of `year`.
fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Whether the reading describes a real date.
///
/// The RTC's registers are firmware-writable, so a nonsense reading is not
/// a kernel bug — but a timestamp built from month 17 would be worse than
/// none at all. `None` from [`read_datetime`] is the caller's signal to
/// omit the timestamp.
pub fn is_plausible(dt: &RtcDateTime) -> bool {
    let day_limit = days_in_month(dt.year, dt.month);
    (1970..=2400).contains(&dt.year)
        && (1..=12).contains(&dt.month)
        && dt.day >= 1
        && dt.day <= day_limit
        && dt.hour < 24
        && dt.minute < 60
        && dt.second < 60
}

impl RtcDateTime {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub fn unix_seconds(&self) -> u64 {
        days_from_civil(self.year, self.month, self.day) * 86_400
            + u64::from(self.hour) * 3_600
            + u64::from(self.minute) * 60
            + u64::from(self.second)
    }
}

/// Read the wall clock once, in one atomic CMOS session.
///
/// `century_register` is the FADT's `RTC century` index, or zero for "no
/// century register". Returns `None` when the clock could not be read
/// stably inside the budgets or the result is not a plausible date — a
/// caller wanting a timestamp omits it rather than recording nonsense.
pub fn read_datetime(century_register: u8) -> Option<RtcDateTime> {
    // SAFETY: the CMOS ports are machine-global, so every access runs behind
    // `CMOS`; the lock's own discipline covers the interrupt reentrancy case.
    let _cmos = CMOS.lock();

    // Port 0x70's high bit is the NMI control. Reading the port returns the
    // last index written, so the bit survives the whole session by writing
    // it back with every index.
    let nmi_state = unsafe { inb(INDEX_PORT) } & NMI_BIT;
    let mut session = Session { nmi_state };

    let status_b = session.read(STATUS_B);
    let binary = status_b & 0b100 != 0;
    let twelve_hour = status_b & 0b010 == 0;

    // The torn-read detector: take a reading, wait out any update cycle,
    // take another, and require agreement. A disagreement means the cycle
    // tore one of them; the budgets bound a pathological clock into a
    // `None` rather than a hang.
    let mut last = session.snapshot(century_register, binary, twelve_hour)?;
    for _ in 0..READ_BUDGET {
        if !session.wait_not_updating() {
            return None;
        }
        let next = session.snapshot(century_register, binary, twelve_hour)?;
        match (last, next) {
            (Some(reading), Some(other)) if reading == other => return Some(reading),
            (_, next) => last = next,
        }
    }
    None
}

/// One CMOS session: the NMI bit threaded through every index write.
struct Session {
    /// The caller's NMI state, restored implicitly by preserving it here.
    nmi_state: u8,
}

impl Session {
    /// Read one CMOS register.
    ///
    /// # Safety-free note
    ///
    /// The ports are machine-global; [`read_datetime`] holds the CMOS lock
    /// for the whole session, which is the only authorization this needs.
    fn read(&mut self, register: u8) -> u8 {
        // SAFETY: the CMOS lock serializes the port pair across CPUs, and
        // the NMI bit is preserved by construction.
        unsafe {
            outb(INDEX_PORT, self.nmi_state | register);
            inb(DATA_PORT)
        }
    }

    /// Spin until the update cycle is over, bounded.
    fn wait_not_updating(&mut self) -> bool {
        for _ in 0..UIP_BUDGET {
            if self.read(STATUS_A) & 0x80 == 0 {
                return true;
            }
        }
        false
    }

    /// Read and decode every time register, twice-consistency left to the
    /// caller. `None` on a value no decode can rescue.
    fn snapshot(
        &mut self,
        century_register: u8,
        binary: bool,
        twelve_hour: bool,
    ) -> Option<Option<RtcDateTime>> {
        let second = self.read(SECONDS);
        let minute = self.read(MINUTES);
        let hour_raw = self.read(HOURS);
        let day = self.read(DAY_OF_MONTH);
        let month = self.read(MONTH);
        let year = self.read(YEAR);

        let decode = |raw: u8| -> u8 { if binary { raw } else { bcd_to_binary(raw) } };

        let hour = decode_hour(hour_raw, twelve_hour, binary);
        let minute_decoded = decode(minute);
        let second_decoded = decode(second);
        let day_decoded = decode(day);
        let month_decoded = decode(month);
        let year_decoded = decode(year) as u16;

        // The century comes from the register the FADT named. Without one,
        // the year register's 00–99 range is completed to 20xx: machines
        // whose RTC predates a century register are 19xx hardware, which
        // this kernel does not target.
        let year = if century_register != 0 {
            let century = decode(self.read(century_register)) as u16;
            century * 100 + year_decoded
        } else {
            2000 + year_decoded
        };

        let reading = RtcDateTime {
            year,
            month: month_decoded,
            day: day_decoded,
            hour,
            minute: minute_decoded,
            second: second_decoded,
        };
        if !is_plausible(&reading) {
            return Some(None);
        }
        Some(Some(reading))
    }
}
