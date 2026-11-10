//! Rust port of `ede-timedate-conf`, EDE's date-and-time settings.
//!
//! Upstream shows a clock, a calendar, the timezone, and switches for
//! setting the time manually or keeping it via NTP; writing the hardware
//! clock needs privileges. NovaOS keeps the model: a broken-down civil
//! date/time, its conversion to and from a Unix-style timestamp, the
//! timezone offset the user picked, and the NTP switch the system's time
//! service honours. The clock face itself is the shell's job.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// The longest timezone name kept (`Europe/Lisbon`).
pub const ZONE_MAX: usize = 40;

/// A broken-down civil time: what the dialog's fields edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CivilTime {
    /// Year (full, e.g. 2026).
    pub year: i32,
    /// Month, 1–12.
    pub month: u32,
    /// Day of month, 1–31.
    pub day: u32,
    /// Hour, 0–23.
    pub hour: u32,
    /// Minute, 0–59.
    pub minute: u32,
    /// Second, 0–59.
    pub second: u32,
}

impl Default for CivilTime {
    fn default() -> Self {
        Self {
            year: 2026,
            month: 1,
            day: 1,
            hour: 12,
            minute: 0,
            second: 0,
        }
    }
}

impl CivilTime {
    /// Whether the fields form a real date — the dialog's validation.
    pub const fn is_valid(&self) -> bool {
        if self.month < 1 || self.month > 12 {
            return false;
        }
        if self.day < 1 || self.day > days_in_month(self.year, self.month) {
            return false;
        }
        self.hour <= 23 && self.minute <= 59 && self.second <= 59
    }

    /// The day of the week: 0 = Monday … 6 = Sunday. Valid for the
    /// Gregorian calendar from 1970 on.
    pub fn weekday(self) -> u32 {
        let days = days_from_civil(self.year, self.month, self.day);
        // 1970-01-01 was a Thursday (day 3 counting from Monday).
        (((days + 3) % 7 + 7) % 7) as u32
    }

    /// Convert to a timestamp, applying `offset_minutes` (east positive).
    pub fn to_timestamp(self, offset_minutes: i64) -> i64 {
        let days = days_from_civil(self.year, self.month, self.day);
        let seconds =
            days * 86_400 + self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64;
        seconds - offset_minutes * 60
    }

    /// Build from a timestamp plus the zone's `offset_minutes`.
    pub fn from_timestamp(timestamp: i64, offset_minutes: i64) -> Self {
        let local = timestamp + offset_minutes * 60;
        let mut days = local.div_euclid(86_400);
        let mut rem = local.rem_euclid(86_400);
        let hour = (rem / 3600) as u32;
        rem %= 3600;
        let minute = (rem / 60) as u32;
        let second = (rem % 60) as u32;
        // Walk back from 1970-03-01 to find February-end years cheaply.
        days += 719_468;
        let era = days.div_euclid(146_097);
        let day_of_era = days.rem_euclid(146_097);
        let year_of_era = (day_of_era - day_of_era / 1460 + era - era / 4) / 365;
        let year = (year_of_era as i32).wrapping_add((era * 400) as i32);
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let mp = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { year + 1 } else { year };
        Self {
            year,
            month: month as u32,
            day: day as u32,
            hour,
            minute,
            second,
        }
    }
}

/// The timedate configuration: the zone and the NTP switch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimeDateSettings {
    /// The IANA-style zone name.
    pub zone: [u8; ZONE_MAX],
    zone_len: usize,
    /// The zone's standard offset from UTC, east positive, in minutes.
    pub offset_minutes: i32,
    /// Whether the system time service keeps the clock via NTP.
    pub ntp: bool,
}

impl Default for TimeDateSettings {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeDateSettings {
    /// UTC, NTP on — the factory default.
    pub fn new() -> Self {
        let mut settings = Self {
            zone: [0; ZONE_MAX],
            zone_len: 0,
            offset_minutes: 0,
            ntp: true,
        };
        settings.set_zone("UTC");
        settings
    }

    /// Set the zone by name, with its offset in minutes.
    pub fn set_zone(&mut self, name: &str) -> bool {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > ZONE_MAX {
            return false;
        }
        self.zone[..bytes.len()].copy_from_slice(bytes);
        self.zone_len = bytes.len();
        self.offset_minutes = 0;
        true
    }

    /// The zone name.
    pub fn zone(&self) -> &str {
        core::str::from_utf8(&self.zone[..self.zone_len]).unwrap_or("UTC")
    }

    /// Read from the config store; unknown keys keep the defaults.
    pub fn from_config(config: &EdeConfig) -> Self {
        let mut settings = Self::new();
        if let Some(zone) = config.get("DateTime", "Timezone")
            && !settings.set_zone(zone)
        {
            // An empty or overlong zone name falls back to UTC.
            settings.set_zone("UTC");
        }
        if let Some(offset) = config.get_i32("DateTime", "OffsetMinutes") {
            settings.offset_minutes = offset;
        }
        settings.ntp = config.get_bool("DateTime", "NTP").unwrap_or(settings.ntp);
        settings
    }

    /// Write the settings back as a `[DateTime]` section.
    pub fn to_config(&self, out: &mut [u8]) -> usize {
        let mut cursor = copy_into(out, 0, b"[DateTime]\n");
        cursor = write_i32(out, cursor, b"OffsetMinutes", self.offset_minutes);
        cursor = copy_into(out, cursor, b"NTP = ");
        cursor = copy_into(out, cursor, if self.ntp { b"true\n" } else { b"false\n" });
        cursor = copy_into(out, cursor, b"Timezone = ");
        cursor = copy_into(out, cursor, &self.zone[..self.zone_len]);
        copy_into(out, cursor, b"\n")
    }
}

/// Days in a month, Gregorian.
pub const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Gregorian leap-year rule.
pub const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Howard days-from-civil: the proleptic Gregorian day number.
pub const fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year as i64;
    let month = month as i64;
    let day = day as i64;
    let year_adjusted = if month <= 2 { year - 1 } else { year };
    let era = year_adjusted.div_euclid(400);
    let year_of_era = year_adjusted.rem_euclid(400);
    let month_shift = if month > 2 { month - 3 } else { month + 9 };
    let day_of_era =
        365 * year_of_era + year_of_era / 4 - year_of_era / 100 + (153 * month_shift + 2) / 5 + day
            - 1;
    era * 146_097 + day_of_era - 719_468
}

/// Copy `value` into `out` at `cursor`, returning the new cursor.
fn copy_into(out: &mut [u8], cursor: usize, value: &[u8]) -> usize {
    let take = value.len().min(out.len().saturating_sub(cursor));
    out[cursor..cursor + take].copy_from_slice(&value[..take]);
    cursor + take
}

/// Append `key = value\n` with a signed number.
fn write_i32(out: &mut [u8], cursor: usize, key: &[u8], value: i32) -> usize {
    let mut buffer = [0u8; 14];
    let mut length = 0usize;
    let mut magnitude = value.unsigned_abs();
    if magnitude == 0 {
        buffer[0] = b'0';
        length = 1;
    }
    while magnitude > 0 {
        buffer[length] = b'0' + (magnitude % 10) as u8;
        length += 1;
        magnitude /= 10;
    }
    if value < 0 {
        buffer[length] = b'-';
        length += 1;
    }
    for index in 0..length / 2 {
        buffer.swap(index, length - index - 1);
    }
    let mut next = copy_into(out, cursor, key);
    next = copy_into(out, next, b" = ");
    next = copy_into(out, next, &buffer[..length]);
    copy_into(out, next, b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_validate() {
        let good = CivilTime {
            year: 2026,
            month: 2,
            day: 28,
            hour: 23,
            minute: 59,
            second: 59,
        };
        assert!(good.is_valid());
        let leap_day = CivilTime {
            year: 2024,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert!(leap_day.is_valid(), "2024 is a leap year");
        let non_leap = CivilTime {
            year: 2026,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert!(!non_leap.is_valid());
        let bad_hour = CivilTime {
            year: 2026,
            month: 2,
            day: 28,
            hour: 24,
            minute: 0,
            second: 0,
        };
        assert!(!bad_hour.is_valid());
    }

    #[test]
    fn timestamps_round_trip() {
        let local = CivilTime {
            year: 2026,
            month: 11,
            day: 9,
            hour: 14,
            minute: 30,
            second: 0,
        };
        // Lisbon winter time: UTC+0. NovaOS's default zone.
        let timestamp = local.to_timestamp(0);
        let back = CivilTime::from_timestamp(timestamp, 0);
        assert_eq!(back, local);
    }

    #[test]
    fn an_offset_shifts_the_civil_time() {
        let local = CivilTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 12,
            minute: 0,
            second: 0,
        };
        // UTC+8: the same instant reads 4 a.m. in Lisbon.
        let timestamp = local.to_timestamp(480);
        let utc = CivilTime::from_timestamp(timestamp, 0);
        assert_eq!(utc.hour, 4);
        assert_eq!(utc.day, 1);
        // And back the other way.
        let timestamp = local.to_timestamp(-300);
        let utc = CivilTime::from_timestamp(timestamp, 0);
        assert_eq!(utc.hour, 17);
    }

    #[test]
    fn the_epoch_is_known() {
        let epoch = CivilTime::from_timestamp(0, 0);
        assert_eq!(epoch.year, 1970);
        assert_eq!(epoch.month, 1);
        assert_eq!(epoch.day, 1);
        assert_eq!(epoch.hour, 0);
    }

    #[test]
    fn weekdays_are_correct() {
        // 2026-11-09 is a Monday (today, per the harness clock).
        let monday = CivilTime {
            year: 2026,
            month: 11,
            day: 9,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(monday.weekday(), 0);
        // 1970-01-01 was a Thursday.
        let epoch = CivilTime::from_timestamp(0, 0);
        assert_eq!(epoch.weekday(), 3);
    }

    #[test]
    fn month_lengths_follow_the_leap_rule() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28, "1900 is not a leap year");
        assert_eq!(days_in_month(2000, 2), 29, "2000 is a leap year");
    }

    #[test]
    fn settings_round_trip_through_the_config() {
        let mut settings = TimeDateSettings::new();
        settings.set_zone("Europe/Lisbon");
        settings.offset_minutes = 0;
        settings.ntp = false;
        let mut buffer = [0u8; 128];
        let length = settings.to_config(&mut buffer);
        let config = EdeConfig::parse(&buffer[..length]);
        let reloaded = TimeDateSettings::from_config(&config);
        assert_eq!(reloaded.zone(), "Europe/Lisbon");
        assert_eq!(reloaded.offset_minutes, 0);
        assert!(!reloaded.ntp);
    }

    #[test]
    fn the_default_zone_is_utc() {
        let settings = TimeDateSettings::new();
        assert_eq!(settings.zone(), "UTC");
        assert!(settings.ntp);
    }
}
