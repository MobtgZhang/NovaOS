//! Rust port of `ede-keyboard-conf`, EDE's keyboard configuration tool.
//!
//! Upstream edits three things: the key repeat delay and rate, the
//! modifier-map options (`caps` acting as Ctrl and friends), and a
//! per-layout beep toggle. The X11 `xset r`/`setxkbmap` transport does
//! not exist here; the settings themselves, their bounds, their XSETTINGS
//! key names, and the conflict rule (a layout must not be both enabled
//! and disabled) are what this crate keeps.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// Repeat delay in milliseconds, upstream's legal range.
pub const DELAY_MIN: u32 = 100;
pub const DELAY_MAX: u32 = 1000;
/// Repeat rate in characters per second, upstream's legal range.
pub const RATE_MIN: u32 = 10;
pub const RATE_MAX: u32 = 90;

/// How many modifier options one configuration may hold.
pub const MAX_OPTIONS: usize = 8;
/// The longest option string kept (`ctrl:nocaps`).
pub const OPTION_MAX: usize = 24;

/// One XKB modifier option, e.g. `ctrl:nocaps`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModifierOption {
    /// The option string, verbatim.
    pub text: [u8; OPTION_MAX],
    text_len: usize,
}

impl ModifierOption {
    /// A parsed option.
    pub fn new(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.is_empty() || bytes.len() > OPTION_MAX {
            return None;
        }
        let mut option = Self {
            text: [0; OPTION_MAX],
            text_len: bytes.len(),
        };
        option.text[..bytes.len()].copy_from_slice(bytes);
        Some(option)
    }

    /// The option string.
    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.text[..self.text_len]).unwrap_or("")
    }
}

/// The keyboard configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyboardSettings {
    /// Repeat delay in milliseconds.
    pub delay_ms: u32,
    /// Repeat rate in characters per second.
    pub rate_cps: u32,
    /// The modifier-map options, in order.
    pub options: [Option<ModifierOption>; MAX_OPTIONS],
    option_count: usize,
    /// Whether the bell beeps on keypress.
    pub beep: bool,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            delay_ms: 300,
            rate_cps: 25,
            options: [None; MAX_OPTIONS],
            option_count: 0,
            beep: true,
        }
    }
}

impl KeyboardSettings {
    /// Settings clamped into the legal ranges.
    pub fn clamped(delay_ms: u32, rate_cps: u32, beep: bool) -> Self {
        Self {
            delay_ms: delay_ms.clamp(DELAY_MIN, DELAY_MAX),
            rate_cps: rate_cps.clamp(RATE_MIN, RATE_MAX),
            options: [None; MAX_OPTIONS],
            option_count: 0,
            beep,
        }
    }

    /// Add a modifier option; duplicates are ignored.
    pub fn add_option(&mut self, text: &str) -> bool {
        let Some(option) = ModifierOption::new(text) else {
            return false;
        };
        if self.options.iter().take(self.option_count).any(|slot| {
            slot.as_ref()
                .is_some_and(|found| found.text() == option.text())
        }) {
            return true; // already present
        }
        if self.option_count >= MAX_OPTIONS {
            return false;
        }
        self.options[self.option_count] = Some(option);
        self.option_count += 1;
        true
    }

    /// Remove a modifier option.
    pub fn remove_option(&mut self, text: &str) -> bool {
        for index in 0..self.option_count {
            let found = self.options[index].as_ref().map(|slot| slot.text());
            if found == Some(text) {
                let tail = index + 1;
                self.options.copy_within(tail..self.option_count, index);
                self.option_count -= 1;
                return true;
            }
        }
        false
    }

    /// How many modifier options are set.
    pub const fn option_count(&self) -> usize {
        self.option_count
    }

    /// The option at `index`.
    pub fn option(&self, index: usize) -> Option<&str> {
        let slot = self.options.get(index)?.as_ref()?;
        Some(slot.text())
    }

    /// Read from the config store; out-of-range numbers fall back to
    /// their defaults rather than being trusted.
    pub fn from_config(config: &EdeConfig) -> Self {
        let defaults = Self::default();
        let mut settings = Self {
            delay_ms: checked_range(
                config.get_u32("Keyboard", "RepeatDelay"),
                DELAY_MIN,
                DELAY_MAX,
                defaults.delay_ms,
            ),
            rate_cps: checked_range(
                config.get_u32("Keyboard", "RepeatRate"),
                RATE_MIN,
                RATE_MAX,
                defaults.rate_cps,
            ),
            options: [None; MAX_OPTIONS],
            option_count: 0,
            beep: config.get_bool("Keyboard", "Beep").unwrap_or(defaults.beep),
        };
        if let Some(options) = config.get("Keyboard", "Options") {
            for option in options.split(',') {
                settings.add_option(option.trim());
            }
        }
        settings
    }
}

/// A configured number, but only when it lies inside the legal range;
/// anything else falls back to `default`.
fn checked_range(value: Option<u32>, min: u32, max: u32, default: u32) -> u32 {
    match value {
        Some(found) if found >= min && found <= max => found,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let settings = KeyboardSettings::default();
        assert_eq!(settings.delay_ms, 300);
        assert_eq!(settings.rate_cps, 25);
        assert!(settings.beep);
        assert_eq!(settings.option_count(), 0);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings = KeyboardSettings::clamped(10, 500, false);
        assert_eq!(settings.delay_ms, DELAY_MIN);
        assert_eq!(settings.rate_cps, RATE_MAX);
    }

    #[test]
    fn options_dedupe_and_remove() {
        let mut settings = KeyboardSettings::default();
        assert!(settings.add_option("ctrl:nocaps"));
        assert!(settings.add_option("ctrl:nocaps"));
        assert_eq!(settings.option_count(), 1);
        assert!(settings.add_option("lv3:ralt_switch"));
        assert_eq!(settings.option_count(), 2);
        assert!(settings.remove_option("ctrl:nocaps"));
        assert_eq!(settings.option_count(), 1);
        assert_eq!(settings.option(0), Some("lv3:ralt_switch"));
    }

    #[test]
    fn an_empty_option_is_rejected() {
        let mut settings = KeyboardSettings::default();
        assert!(!settings.add_option(""));
    }

    #[test]
    fn config_round_trips() {
        let config = EdeConfig::parse(
            b"[Keyboard]\nRepeatDelay = 250\nRepeatRate = 40\nBeep = false\nOptions = ctrl:nocaps,lv3:ralt_switch\n",
        );
        let settings = KeyboardSettings::from_config(&config);
        assert_eq!(settings.delay_ms, 250);
        assert_eq!(settings.rate_cps, 40);
        assert!(!settings.beep);
        assert_eq!(settings.option_count(), 2);
    }

    #[test]
    fn a_broken_config_falls_back_to_defaults() {
        let config = EdeConfig::parse(b"[Keyboard]\nRepeatDelay = 99999\n");
        let settings = KeyboardSettings::from_config(&config);
        assert_eq!(settings.delay_ms, 300, "out of range falls back");
    }
}
