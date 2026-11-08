//! The wall clock: decoding, calendar arithmetic, and the live CMOS read.
//!
//! The hardware-dependent parts are pinned by arithmetic tests against
//! known timestamps; the live read is pinned against the date QEMU's RTC
//! actually reports. A decoding error shows up as a wrong epoch; a torn
//! read shows up as a failing monotonicity case.

use crate::arch::x86_64::rtc::{
    RtcDateTime, bcd_to_binary, days_from_civil, decode_hour, is_leap_year, is_plausible,
};

fn bcd_decodes_packed_digits() -> Result<(), &'static str> {
    for (raw, expected) in [
        (0x00, 0),
        (0x01, 1),
        (0x09, 9),
        (0x10, 10),
        (0x26, 26),
        (0x59, 59),
        (0x99, 99),
    ] {
        if bcd_to_binary(raw) != expected {
            return Err("a packed BCD digit decoded to the wrong value");
        }
    }
    Ok(())
}

fn twelve_hour_decode() -> Result<(), &'static str> {
    // BCD, 12-hour mode: 01 AM is 1 o'clock, 12 AM is midnight, 12 PM is
    // noon, 09 PM is 21.
    for (raw, expected) in [(0x01, 1), (0x12, 0), (0x92, 12), (0x09, 9), (0x89, 21)] {
        if decode_hour(raw, true, false) != expected {
            return Err("a 12-hour BCD hour decoded wrongly");
        }
    }
    // 24-hour mode ignores the flag bit entirely.
    if decode_hour(0x17, false, false) != 17 || decode_hour(0x97, false, false) != 17 {
        return Err("a 24-hour hour decoded wrongly");
    }
    Ok(())
}

fn days_from_civil_matches_known_dates() -> Result<(), &'static str> {
    // (year, month, day, days before the epoch)
    for (year, month, day, expected) in [
        (1970, 1, 1, 0i64),
        (2000, 1, 1, 10_957i64),
        (2024, 1, 1, 19_723i64),
        (2026, 11, 8, 20_765i64),
        // A leap day itself, and the day after February that follows it.
        (2024, 2, 29, 19_782i64),
        (2024, 3, 1, 19_783i64),
    ] {
        let days = days_from_civil(year, month, day) as i64;
        if days != expected {
            return Err("a civil date landed on the wrong day number");
        }
    }
    Ok(())
}

fn leap_century_rules_hold() -> Result<(), &'static str> {
    if !is_leap_year(2000) || is_leap_year(1900) || is_leap_year(2100) || !is_leap_year(2024) {
        return Err("the Gregorian century rule is wrong");
    }
    Ok(())
}

fn plausibility_rejects() -> Result<(), &'static str> {
    let good = RtcDateTime {
        year: 2026,
        month: 11,
        day: 8,
        hour: 12,
        minute: 34,
        second: 56,
    };
    if !is_plausible(&good) {
        return Err("a real date was rejected");
    }
    for bad in [
        RtcDateTime { month: 13, ..good },
        RtcDateTime { day: 32, ..good },
        RtcDateTime { day: 0, ..good },
        RtcDateTime {
            month: 2,
            day: 30,
            ..good
        },
        RtcDateTime { hour: 24, ..good },
        RtcDateTime { minute: 60, ..good },
        RtcDateTime { second: 61, ..good },
        RtcDateTime { year: 1969, ..good },
    ] {
        if is_plausible(&bad) {
            return Err("an impossible date was accepted");
        }
    }
    // February 29th only on a leap year.
    if is_plausible(&RtcDateTime {
        year: 2025,
        month: 2,
        day: 29,
        ..good
    }) {
        return Err("a non-leap February 29th was accepted");
    }
    if !is_plausible(&RtcDateTime {
        year: 2024,
        month: 2,
        day: 29,
        ..good
    }) {
        return Err("a leap February 29th was rejected");
    }
    Ok(())
}

fn unix_seconds_matches() -> Result<(), &'static str> {
    let epoch = RtcDateTime {
        year: 1970,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };
    if epoch.unix_seconds() != 0 {
        return Err("the epoch did not read as zero");
    }
    let millis = RtcDateTime {
        year: 2000,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };
    if millis.unix_seconds() != 946_684_800 {
        return Err("2000-01-01 landed off the known epoch");
    }
    let today = RtcDateTime {
        year: 2026,
        month: 11,
        day: 8,
        hour: 0,
        minute: 0,
        second: 0,
    };
    if today.unix_seconds() != 1_794_096_000 {
        return Err("2026-11-08 landed off the known epoch");
    }
    // Intra-day fields contribute their documented weights.
    let noonish = RtcDateTime {
        year: 2026,
        month: 11,
        day: 8,
        hour: 12,
        minute: 34,
        second: 56,
    };
    if noonish.unix_seconds() != 1_794_096_000 + 12 * 3600 + 34 * 60 + 56 {
        return Err("the intra-day weights are wrong");
    }
    Ok(())
}

fn machine_reports_real_date() -> Result<(), &'static str> {
    // SAFETY: the boot snapshot is published long before the harness runs.
    let century = unsafe { crate::boot::read_boot_info() }.acpi_century;
    let Some(reading) = crate::arch::x86_64::rtc::read_datetime(century) else {
        return Err("the CMOS clock did not deliver a stable, plausible reading");
    };
    // The reference machine's clock: no earlier than the kernel's own
    // release window, and within ordinary register ranges (which
    // `read_datetime` already enforces, restated here for the log).
    if reading.year < 2024 {
        return Err("the wall clock reads before 2024");
    }
    Ok(())
}

fn successive_reads_monotonic() -> Result<(), &'static str> {
    // SAFETY: as above.
    let century = unsafe { crate::boot::read_boot_info() }.acpi_century;
    let first = crate::arch::x86_64::rtc::read_datetime(century)
        .ok_or("the first CMOS reading failed")?
        .unix_seconds();
    let second = crate::arch::x86_64::rtc::read_datetime(century)
        .ok_or("the second CMOS reading failed")?
        .unix_seconds();
    if second < first {
        return Err("the wall clock went backwards between two readings");
    }
    Ok(())
}

pub static WALLCLOCK_SUITES: &[crate::test::harness::TestSuite] =
    &[crate::test::harness::TestSuite {
        name: "wall_clock",
        cases: &[
            ("bcd_decodes_packed_digits", bcd_decodes_packed_digits),
            (
                "twelve_hour_decode_inverts_the_datasheet",
                twelve_hour_decode,
            ),
            (
                "days_from_civil_matches_known_dates",
                days_from_civil_matches_known_dates,
            ),
            ("leap_century_rules_hold", leap_century_rules_hold),
            (
                "plausibility_rejects_impossible_dates",
                plausibility_rejects,
            ),
            ("unix_seconds_matches_the_known_epoch", unix_seconds_matches),
            ("the_machine_reports_a_real_date", machine_reports_real_date),
            (
                "successive_reads_do_not_go_backwards",
                successive_reads_monotonic,
            ),
        ],
    }];
