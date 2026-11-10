//! Rust port of `ede-bell-conf`, EDE's system-bell configuration.
//!
//! Upstream keeps three values — volume (0–100), pitch (100–1000), and
//! duration in milliseconds (0–1000) — applies them to the X server's
//! keyboard bell, and persists them as XSETTINGS `Bell/*` keys. NovaOS's
//! bell is a kernel console beep, so the same three values travel with
//! the same bounds and the same XSETTINGS names, minus the X11 part.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// The XSETTINGS key the volume rides under.
pub const KEY_VOLUME: &str = "Bell/Volume";
/// The XSETTINGS key the pitch rides under.
pub const KEY_PITCH: &str = "Bell/Pitch";
/// The XSETTINGS key the duration rides under.
pub const KEY_DURATION: &str = "Bell/Duration";

/// The bell's configuration, with upstream's bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BellSettings {
    /// 0–100 percent.
    pub volume: u32,
    /// 100–1000 Hz.
    pub pitch: u32,
    /// 0–1000 milliseconds.
    pub duration_ms: u32,
}

impl Default for BellSettings {
    fn default() -> Self {
        // Upstream's slider defaults.
        Self {
            volume: 50,
            pitch: 400,
            duration_ms: 100,
        }
    }
}

impl BellSettings {
    /// Clamp every field into its legal range — a slider cannot leave
    /// the range, but a config file can.
    pub fn clamped(volume: u32, pitch: u32, duration_ms: u32) -> Self {
        Self {
            volume: volume.min(100),
            pitch: pitch.clamp(100, 1000),
            duration_ms: duration_ms.min(1000),
        }
    }

    /// Read from the XSETTINGS-shaped config store; unknown keys keep
    /// their defaults.
    pub fn from_config(config: &EdeConfig) -> Self {
        let defaults = Self::default();
        Self::clamped(
            config.get_u32("Bell", "Volume").unwrap_or(defaults.volume),
            config.get_u32("Bell", "Pitch").unwrap_or(defaults.pitch),
            config
                .get_u32("Bell", "Duration")
                .unwrap_or(defaults.duration_ms),
        )
    }

    /// Write the settings back as `Bell/*` keys for the settings store.
    pub fn to_config(&self, out: &mut [u8]) -> usize {
        let mut used = write_line(out, 0, b"[Bell]\n");
        used = write_key(out, used, b"Volume", self.volume);
        used = write_key(out, used, b"Pitch", self.pitch);
        write_key(out, used, b"Duration", self.duration_ms)
    }

    /// Whether a test beep with these settings would actually sound:
    /// zero volume or zero duration is silence, and the Test button
    /// reports that honestly instead of a silent success.
    pub const fn audible(&self) -> bool {
        self.volume > 0 && self.duration_ms > 0
    }
}

/// Append `key = value\n` for a number.
fn write_key(out: &mut [u8], used: usize, key: &[u8], value: u32) -> usize {
    let mut used = write(out, used, key);
    used = write(out, used, b" = ");
    used = write_number(out, used, value);
    write(out, used, b"\n")
}

/// Append a `[Section]` line.
fn write_line(out: &mut [u8], used: usize, text: &[u8]) -> usize {
    write(out, used, text)
}

fn write(out: &mut [u8], used: usize, bytes: &[u8]) -> usize {
    let take = bytes.len().min(out.len().saturating_sub(used));
    out[used..used + take].copy_from_slice(&bytes[..take]);
    used + take
}

fn write_number(out: &mut [u8], used: usize, value: u32) -> usize {
    let mut buffer = [0u8; 10];
    let mut length = 0usize;
    if value == 0 {
        buffer[0] = b'0';
        length = 1;
    }
    let mut magnitude = value;
    while magnitude > 0 {
        buffer[length] = b'0' + (magnitude % 10) as u8;
        length += 1;
        magnitude /= 10;
    }
    for index in 0..length / 2 {
        buffer.swap(index, length - index - 1);
    }
    write(out, used, &buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let settings = BellSettings::default();
        assert_eq!(settings.volume, 50);
        assert_eq!(settings.pitch, 400);
        assert_eq!(settings.duration_ms, 100);
        assert!(settings.audible());
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings = BellSettings::clamped(500, 5000, 5000);
        assert_eq!(settings.volume, 100);
        assert_eq!(settings.pitch, 1000);
        assert_eq!(settings.duration_ms, 1000);
        let settings = BellSettings::clamped(0, 10, 0);
        assert_eq!(settings.pitch, 100, "pitch floors at 100");
    }

    #[test]
    fn silence_is_reported_honestly() {
        let mute = BellSettings::clamped(0, 400, 100);
        assert!(!mute.audible());
        let forever = BellSettings::clamped(80, 400, 0);
        assert!(!forever.audible());
    }

    #[test]
    fn config_round_trip() {
        let settings = BellSettings::clamped(80, 880, 250);
        let mut buffer = [0u8; 128];
        let length = settings.to_config(&mut buffer);
        let config = EdeConfig::parse(&buffer[..length]);
        let reloaded = BellSettings::from_config(&config);
        assert_eq!(reloaded, settings);
    }

    #[test]
    fn unknown_config_keys_keep_the_defaults() {
        let config = EdeConfig::parse(b"[Bell]\n");
        let settings = BellSettings::from_config(&config);
        assert_eq!(settings, BellSettings::default());
    }
}
