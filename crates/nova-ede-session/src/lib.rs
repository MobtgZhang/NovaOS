//! Rust port of `evoke`, EDE's session manager — the part that decides
//! *what starts, in what order*, and *how the session ends*.
//!
//! Upstream `evoke` reads `ede-startup.conf`, starts each component in
//! `start_order` order with a splash step per entry, hosts the typed
//! XSETTINGS store, and coordinates logout/restart/shutdown. NovaOS has
//! no X11, so the window-manager and X-settings specifics collapse into
//! their portable cores: an ordered startup list, a splash state
//! machine, a typed settings store, and explicit session-end intents
//! that a privileged caller must still carry out.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// How many startup entries one configuration may list.
pub const MAX_ENTRIES: usize = 12;
/// How many typed settings one session keeps.
pub const MAX_SETTINGS: usize = 32;
/// The longest startup-entry or setting name kept.
pub const NAME_MAX: usize = 32;
/// The longest command or string value kept.
pub const VALUE_MAX: usize = 64;

/// One component the session starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StartupEntry {
    /// The entry's key in `[Startup] start_order`, e.g. `wm`.
    pub name: [u8; NAME_MAX],
    name_len: usize,
    /// The command to run.
    pub exec: [u8; VALUE_MAX],
    exec_len: usize,
    /// The one-line description the splash shows.
    pub description: [u8; VALUE_MAX],
    description_len: usize,
}

impl StartupEntry {
    /// The entry's key.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    /// The command to run.
    pub fn exec(&self) -> &str {
        core::str::from_utf8(&self.exec[..self.exec_len]).unwrap_or("")
    }

    /// The splash description.
    pub fn description(&self) -> &str {
        core::str::from_utf8(&self.description[..self.description_len]).unwrap_or("")
    }
}

/// The session's startup plan, parsed from `ede-startup.conf`.
#[derive(Clone, Copy)]
pub struct StartupPlan {
    entries: [Option<StartupEntry>; MAX_ENTRIES],
    count: usize,
    /// The names in `start_order`, in order.
    order: [Option<[u8; NAME_MAX]>; MAX_ENTRIES],
    order_count: usize,
    /// The splash theme; `none` disables the splash.
    pub splash_theme: [u8; NAME_MAX],
    splash_theme_len: usize,
}

impl StartupPlan {
    /// An empty plan.
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_ENTRIES],
            count: 0,
            order: [None; MAX_ENTRIES],
            order_count: 0,
            splash_theme: [0; NAME_MAX],
            splash_theme_len: 0,
        }
    }

    /// Parse a startup configuration: `start_order` names the sequence;
    /// every other section is one entry with `exec` and `description`.
    pub fn parse(config: &EdeConfig) -> Self {
        let mut plan = Self::new();
        if let Some(order) = config.get("Startup", "start_order") {
            for name in order.split(',') {
                let name = name.trim();
                if name.is_empty() || plan.order_count >= MAX_ENTRIES {
                    continue;
                }
                let mut slot = [0u8; NAME_MAX];
                let bytes = name.as_bytes();
                let take = bytes.len().min(NAME_MAX);
                slot[..take].copy_from_slice(&bytes[..take]);
                plan.order[plan.order_count] = Some(slot);
                plan.order_count += 1;
            }
        }
        if let Some(theme) = config.get("Startup", "splash_theme") {
            let bytes = theme.as_bytes();
            let take = bytes.len().min(NAME_MAX);
            plan.splash_theme[..take].copy_from_slice(&bytes[..take]);
            plan.splash_theme_len = take;
        }
        for section in config.section_names() {
            if section == "Startup" {
                continue;
            }
            let Some(exec) = config.get(section, "exec") else {
                continue; // not a startup entry
            };
            if plan.count >= MAX_ENTRIES {
                break;
            }
            let mut entry = StartupEntry {
                name: [0; NAME_MAX],
                name_len: 0,
                exec: [0; VALUE_MAX],
                exec_len: 0,
                description: [0; VALUE_MAX],
                description_len: 0,
            };
            let name_bytes = section.as_bytes();
            let name_take = name_bytes.len().min(NAME_MAX);
            entry.name[..name_take].copy_from_slice(&name_bytes[..name_take]);
            entry.name_len = name_take;
            let exec_bytes = exec.as_bytes();
            let exec_take = exec_bytes.len().min(VALUE_MAX);
            entry.exec[..exec_take].copy_from_slice(&exec_bytes[..exec_take]);
            entry.exec_len = exec_take;
            if let Some(description) = config.get(section, "description") {
                let bytes = description.as_bytes();
                let take = bytes.len().min(VALUE_MAX);
                entry.description[..take].copy_from_slice(&bytes[..take]);
                entry.description_len = take;
            }
            plan.entries[plan.count] = Some(entry);
            plan.count += 1;
        }
        plan
    }

    /// How many entries the plan carries.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// How many names `start_order` listed.
    pub const fn order_count(&self) -> usize {
        self.order_count
    }

    /// The `start_order` name at `index`.
    pub fn order_name(&self, index: usize) -> Option<&str> {
        let slot = self.order.get(index)?.as_ref()?;
        core::str::from_utf8(&slot[..slot.iter().position(|&b| b == 0).unwrap_or(slot.len())]).ok()
    }

    /// The entry named `name`, when one was configured.
    pub fn entry(&self, name: &str) -> Option<StartupEntry> {
        self.entries
            .iter()
            .take(self.count)
            .flatten()
            .copied()
            .find(|entry| entry.name() == name)
    }

    /// The entry at position `index` of `start_order` — the session's
    /// launch sequence.
    pub fn entry_in_order(&self, index: usize) -> Option<StartupEntry> {
        self.entry(self.order_name(index)?)
    }
}

impl Default for StartupPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// The splash's step state — upstream's per-entry icon and description.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplashStep {
    /// Nothing is loading yet.
    Idle,
    /// Entry `index` is starting.
    Starting(usize),
    /// Every entry has been started.
    Done,
}

/// The splash state machine: one step per entry, skippable.
#[derive(Clone, Copy)]
pub struct Splash {
    entries: usize,
    step: SplashStep,
}

impl Splash {
    /// A splash over `entries` startup steps.
    pub const fn new(entries: usize) -> Self {
        Self {
            entries,
            step: SplashStep::Idle,
        }
    }

    /// Whether the splash is disabled entirely (`splash_theme = none`).
    pub const fn disabled(&self) -> bool {
        self.entries == 0
    }

    /// The current step.
    pub const fn step(&self) -> SplashStep {
        self.step
    }

    /// Advance to the next entry.
    pub fn advance(&mut self) {
        self.step = match self.step {
            SplashStep::Idle => {
                if self.entries == 0 {
                    SplashStep::Done
                } else {
                    SplashStep::Starting(0)
                }
            }
            SplashStep::Starting(index) => {
                if index + 1 >= self.entries {
                    SplashStep::Done
                } else {
                    SplashStep::Starting(index + 1)
                }
            }
            SplashStep::Done => SplashStep::Done,
        };
    }
}

/// A typed setting: the XSETTINGS `int`/`string`/`color` triple, without
/// the X11 transport.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingValue {
    /// An integer setting.
    Int(i32),
    /// A string setting.
    String([u8; VALUE_MAX]),
    /// An RGBA colour.
    Color(u8, u8, u8, u8),
}

/// The typed settings store, keyed by dotted name (`Net/ThemeName`).
#[derive(Clone, Copy)]
pub struct SettingsStore {
    names: [[u8; NAME_MAX]; MAX_SETTINGS],
    name_lens: [usize; MAX_SETTINGS],
    values: [Option<SettingValue>; MAX_SETTINGS],
    count: usize,
}

impl SettingsStore {
    /// An empty store.
    pub const fn new() -> Self {
        Self {
            names: [[0; NAME_MAX]; MAX_SETTINGS],
            name_lens: [0; MAX_SETTINGS],
            values: [None; MAX_SETTINGS],
            count: 0,
        }
    }

    /// Set a value, overwriting in place when the name exists.
    pub fn set(&mut self, name: &str, value: SettingValue) -> bool {
        let bytes = name.as_bytes();
        if bytes.len() > NAME_MAX {
            return false;
        }
        for index in 0..self.count {
            if &self.names[index][..self.name_lens[index]] == bytes {
                self.values[index] = Some(value);
                return true;
            }
        }
        if self.count >= MAX_SETTINGS {
            return false;
        }
        self.names[self.count][..bytes.len()].copy_from_slice(bytes);
        self.name_lens[self.count] = bytes.len();
        self.values[self.count] = Some(value);
        self.count += 1;
        true
    }

    /// The value stored under `name`.
    pub fn get(&self, name: &str) -> Option<SettingValue> {
        let bytes = name.as_bytes();
        for index in 0..self.count {
            if &self.names[index][..self.name_lens[index]] == bytes {
                return self.values[index];
            }
        }
        None
    }

    /// How many settings are stored.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Remove a setting.
    pub fn remove(&mut self, name: &str) -> bool {
        let bytes = name.as_bytes();
        for index in 0..self.count {
            if &self.names[index][..self.name_lens[index]] == bytes {
                let tail = index + 1;
                self.names.copy_within(tail..self.count, index);
                self.name_lens.copy_within(tail..self.count, index);
                self.values.copy_within(tail..self.count, index);
                self.count -= 1;
                return true;
            }
        }
        false
    }
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// How the user asked the session to end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionIntent {
    /// Continue running.
    None,
    /// Log out: close the session's windows and exit the shell.
    Logout,
    /// Restart the machine.
    Restart,
    /// Shut the machine down.
    Shutdown,
    /// Lock the screen.
    Lock,
}

/// The session controller: what the user asked for, and whether the
/// system can honour it.
#[derive(Clone, Copy)]
pub struct Session {
    /// The pending intent, cleared once it has been carried out.
    pub intent: SessionIntent,
    /// Whether the platform reports it can power off.
    pub can_shutdown: bool,
    /// Whether the platform reports it can reboot.
    pub can_restart: bool,
}

impl Session {
    /// A session with the platform's reported capabilities.
    pub const fn new(can_shutdown: bool, can_restart: bool) -> Self {
        Self {
            intent: SessionIntent::None,
            can_shutdown,
            can_restart,
        }
    }

    /// Request an intent. Returns false only for intents the platform
    /// cannot honour — upstream falls back to `/sbin/shutdown`, which a
    /// capability-secure kernel has no equivalent of.
    pub fn request(&mut self, intent: SessionIntent) -> bool {
        match intent {
            SessionIntent::Shutdown if !self.can_shutdown => return false,
            SessionIntent::Restart if !self.can_restart => return false,
            _ => {}
        }
        self.intent = intent;
        true
    }

    /// Take the pending intent, clearing it — the caller now owns it.
    pub fn take(&mut self) -> SessionIntent {
        core::mem::replace(&mut self.intent, SessionIntent::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTUP_CONF: &[u8] = b"\
[Startup]
start_order = wm,ede-desktop,ede-panel,ede-notify-daemon,autostart
splash_theme = scape

[wm]
exec = nova-wm
description = Starting window manager...

[ede-desktop]
exec = ede-desktop
description = Starting desktop...

[ede-panel]
exec = ede-panel
description = Starting panel...

[ede-notify-daemon]
exec = ede-notify-daemon
description = Starting notification service...

[autostart]
exec = ede-autostart --safe
description = Loading local programs...
";

    fn plan() -> StartupPlan {
        StartupPlan::parse(&EdeConfig::parse(STARTUP_CONF))
    }

    #[test]
    fn start_order_is_parsed_in_order() {
        let plan = plan();
        assert_eq!(plan.order_count(), 5);
        assert_eq!(plan.order_name(0), Some("wm"));
        assert_eq!(plan.order_name(4), Some("autostart"));
        assert_eq!(plan.order_name(5), None);
    }

    #[test]
    fn entries_carry_their_exec_and_description() {
        let plan = plan();
        assert_eq!(plan.count(), 5);
        let wm = plan.entry("wm").unwrap();
        assert_eq!(wm.exec(), "nova-wm");
        assert_eq!(wm.description(), "Starting window manager...");
    }

    #[test]
    fn the_launch_sequence_follows_start_order() {
        let plan = plan();
        let first = plan.entry_in_order(0).unwrap();
        assert_eq!(first.name(), "wm");
        let second = plan.entry_in_order(1).unwrap();
        assert_eq!(second.exec(), "ede-desktop");
        assert!(plan.entry_in_order(9).is_none());
    }

    #[test]
    fn the_splash_theme_is_read() {
        let plan = plan();
        assert_eq!(plan.splash_theme_len, 5);
    }

    #[test]
    fn the_splash_walks_one_step_per_entry() {
        let mut splash = Splash::new(3);
        assert_eq!(splash.step(), SplashStep::Idle);
        splash.advance();
        assert_eq!(splash.step(), SplashStep::Starting(0));
        splash.advance();
        splash.advance();
        assert_eq!(splash.step(), SplashStep::Starting(2));
        splash.advance();
        assert_eq!(splash.step(), SplashStep::Done);
        splash.advance();
        assert_eq!(splash.step(), SplashStep::Done, "done is sticky");
    }

    #[test]
    fn a_zero_entry_splash_skips_to_done() {
        let mut splash = Splash::new(0);
        splash.advance();
        assert_eq!(splash.step(), SplashStep::Done);
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let mut store = SettingsStore::new();
        assert!(store.set("Net/ThemeName", SettingValue::Int(0)));
        assert!(store.set("Net/CursorBlink", SettingValue::Int(1)));
        assert!(store.set("Net/CursorBlink", SettingValue::Int(7)));
        assert_eq!(store.count(), 2, "overwrite, never duplicate");
        match store.get("Net/CursorBlink") {
            Some(SettingValue::Int(value)) => assert_eq!(value, 7),
            other => panic!("expected the int, got {other:?}"),
        }
    }

    #[test]
    fn settings_remove_compacts() {
        let mut store = SettingsStore::new();
        store.set("a", SettingValue::Int(1));
        store.set("b", SettingValue::Int(2));
        store.set("c", SettingValue::Int(3));
        assert!(store.remove("b"));
        assert_eq!(store.count(), 2);
        assert!(store.get("b").is_none());
        match store.get("c") {
            Some(SettingValue::Int(value)) => assert_eq!(value, 3),
            other => panic!("expected c, got {other:?}"),
        }
    }

    #[test]
    fn intents_respect_platform_capabilities() {
        let mut session = Session::new(true, false);
        assert!(session.request(SessionIntent::Logout));
        assert_eq!(session.take(), SessionIntent::Logout);
        assert!(!session.request(SessionIntent::Restart), "cannot restart");
        assert_eq!(session.take(), SessionIntent::None);
        assert!(session.request(SessionIntent::Shutdown));
        assert_eq!(session.take(), SessionIntent::Shutdown);
    }

    #[test]
    fn take_clears_the_pending_intent() {
        let mut session = Session::new(true, true);
        session.request(SessionIntent::Lock);
        assert_eq!(session.take(), SessionIntent::Lock);
        assert_eq!(session.take(), SessionIntent::None);
    }
}
