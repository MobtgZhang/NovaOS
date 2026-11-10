//! Rust port of `ede-screensaver-conf`, EDE's screensaver configuration.
//!
//! Upstream edits three switches and two timers — whether a saver runs
//! at all, after how many minutes of idle it engages, whether it blanks
//! only the screen or also locks it, and whether the saver offers the
//! lock dialog. The X11 `xscreensaver-command` transport does not exist
//! here; the settings, their bounds, and the idle state machine the
//! shell drives are what this crate keeps.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// Idle minutes before the saver engages, upstream's legal range.
pub const TIMEOUT_MIN: u32 = 1;
pub const TIMEOUT_MAX: u32 = 120;

/// The screensaver's configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScreensaverSettings {
    /// Whether a saver runs at all.
    pub enabled: bool,
    /// Idle minutes before the saver engages.
    pub timeout_minutes: u32,
    /// Whether engaging the saver also locks the screen.
    pub lock_with_saver: bool,
}

impl Default for ScreensaverSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_minutes: 10,
            lock_with_saver: false,
        }
    }
}

impl ScreensaverSettings {
    /// Settings with the timeout clamped into range.
    pub fn clamped(enabled: bool, timeout_minutes: u32, lock_with_saver: bool) -> Self {
        Self {
            enabled,
            timeout_minutes: timeout_minutes.clamp(TIMEOUT_MIN, TIMEOUT_MAX),
            lock_with_saver,
        }
    }

    /// Read from the config store; out-of-range timeouts fall back to
    /// the default rather than being trusted.
    pub fn from_config(config: &EdeConfig) -> Self {
        let defaults = Self::default();
        let timeout = match config.get_u32("Screensaver", "Timeout") {
            Some(found @ TIMEOUT_MIN..=TIMEOUT_MAX) => found,
            _ => defaults.timeout_minutes,
        };
        Self {
            enabled: config
                .get_bool("Screensaver", "Enabled")
                .unwrap_or(defaults.enabled),
            timeout_minutes: timeout,
            lock_with_saver: config
                .get_bool("Screensaver", "Lock")
                .unwrap_or(defaults.lock_with_saver),
        }
    }
}

/// The saver's idle state machine, driven by the shell's tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaverState {
    /// The screen is in use.
    Awake,
    /// The saver is showing.
    Engaged,
}

/// The idle tracker: ticks of activity, ticks of idle, and the saver
/// state they produce.
#[derive(Clone, Copy)]
pub struct SaverIdle {
    settings: ScreensaverSettings,
    /// Ticks since the last input event. One tick is one second.
    idle_ticks: u32,
    state: SaverState,
}

impl SaverIdle {
    /// A tracker over the given settings.
    pub const fn new(settings: ScreensaverSettings) -> Self {
        Self {
            settings,
            idle_ticks: 0,
            state: SaverState::Awake,
        }
    }

    /// The current state.
    pub const fn state(&self) -> SaverState {
        self.state
    }

    /// Report a user input event: the saver retreats, and if the
    /// settings ask for it, the shell must unlock first.
    pub fn activity(&mut self) {
        self.idle_ticks = 0;
        self.state = SaverState::Awake;
    }

    /// Advance one second of idle time.
    pub fn idle_second(&mut self) {
        if !self.settings.enabled {
            return;
        }
        if self.state == SaverState::Awake {
            self.idle_ticks = self.idle_ticks.saturating_add(1);
            let threshold = self.settings.timeout_minutes.saturating_mul(60);
            if self.idle_ticks >= threshold {
                self.state = SaverState::Engaged;
            }
        }
    }

    /// Whether waking from the saver now requires a lock dialog: only
    /// when the settings asked to lock with the saver.
    pub const fn needs_unlock(&self) -> bool {
        matches!(self.state, SaverState::Engaged) && self.settings.lock_with_saver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let settings = ScreensaverSettings::default();
        assert!(settings.enabled);
        assert_eq!(settings.timeout_minutes, 10);
        assert!(!settings.lock_with_saver);
    }

    #[test]
    fn the_timeout_is_clamped() {
        let settings = ScreensaverSettings::clamped(true, 0, false);
        assert_eq!(settings.timeout_minutes, TIMEOUT_MIN);
        let settings = ScreensaverSettings::clamped(true, 9999, false);
        assert_eq!(settings.timeout_minutes, TIMEOUT_MAX);
    }

    #[test]
    fn config_round_trips() {
        let config = EdeConfig::parse(b"[Screensaver]\nEnabled = true\nTimeout = 5\nLock = true\n");
        let settings = ScreensaverSettings::from_config(&config);
        assert!(settings.enabled);
        assert_eq!(settings.timeout_minutes, 5);
        assert!(settings.lock_with_saver);
    }

    #[test]
    fn a_broken_timeout_falls_back() {
        let config = EdeConfig::parse(b"[Screensaver]\nTimeout = 0\n");
        let settings = ScreensaverSettings::from_config(&config);
        assert_eq!(settings.timeout_minutes, 10);
    }

    #[test]
    fn the_saver_engages_after_the_timeout() {
        let settings = ScreensaverSettings::clamped(true, 1, false);
        let mut idle = SaverIdle::new(settings);
        assert_eq!(idle.state(), SaverState::Awake);
        for _ in 0..59 {
            idle.idle_second();
        }
        assert_eq!(idle.state(), SaverState::Awake);
        idle.idle_second();
        assert_eq!(idle.state(), SaverState::Engaged);
    }

    #[test]
    fn activity_wakes_it_up() {
        let settings = ScreensaverSettings::clamped(true, 1, true);
        let mut idle = SaverIdle::new(settings);
        for _ in 0..70 {
            idle.idle_second();
        }
        assert_eq!(idle.state(), SaverState::Engaged);
        idle.activity();
        assert_eq!(idle.state(), SaverState::Awake);
    }

    #[test]
    fn a_disabled_saver_never_engages() {
        let settings = ScreensaverSettings::clamped(false, 1, true);
        let mut idle = SaverIdle::new(settings);
        for _ in 0..1000 {
            idle.idle_second();
        }
        assert_eq!(idle.state(), SaverState::Awake);
    }

    #[test]
    fn the_lock_dialog_is_only_offered_when_configured() {
        let unlocked = SaverIdle::new(ScreensaverSettings::clamped(true, 1, false));
        assert!(!unlocked.needs_unlock());
        let locked = SaverIdle::new(ScreensaverSettings::clamped(true, 1, true));
        let mut locked = locked;
        for _ in 0..61 {
            locked.idle_second();
        }
        assert!(locked.needs_unlock());
    }
}
