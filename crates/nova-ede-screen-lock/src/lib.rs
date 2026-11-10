//! Rust port of `ede-screen-lock` — the screen lock dialog's model.
//!
//! Upstream locks the screen behind a password prompt: type the user's
//! password, get the session back; Escape restarts the field; enough
//! wrong attempts introduce a delay. NovaOS keeps the same interaction
//! as a pure model — the password lives in the caller's authentication
//! service, so this crate only tracks the attempt state machine and the
//! lockout timer.

#![no_std]
#![forbid(unsafe_code)]

/// How many failed attempts before the delay engages.
pub const ATTEMPTS_BEFORE_DELAY: u32 = 3;
/// The lockout delay, in ticks, after too many failures.
pub const DELAY_TICKS: u32 = 30;
/// The longest password the field keeps.
pub const PASSWORD_MAX: usize = 32;

/// The lock's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockState {
    /// The prompt is open and accepting input.
    Prompt,
    /// The attempt failed; the prompt stays open.
    Failed,
    /// Too many failures: input is frozen for a while.
    Delay(u32),
    /// The right password was typed; the shell may unlock.
    Unlocked,
}

/// The screen lock's model.
#[derive(Clone, Copy)]
pub struct ScreenLock {
    /// The typed characters so far — the caller echoes dots over them.
    typed: [u8; PASSWORD_MAX],
    typed_len: usize,
    /// Failed attempts since the last success.
    failures: u32,
    state: LockState,
}

impl ScreenLock {
    /// A freshly locked screen.
    pub const fn new() -> Self {
        Self {
            typed: [0; PASSWORD_MAX],
            typed_len: 0,
            failures: 0,
            state: LockState::Prompt,
        }
    }

    /// The current state.
    pub const fn state(&self) -> LockState {
        self.state
    }

    /// How many failures are on record.
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// Type one character into the password field.
    pub fn type_char(&mut self, byte: u8) -> bool {
        if self.state == LockState::Delay(0) || matches!(self.state, LockState::Delay(_)) {
            return false;
        }
        if self.state == LockState::Unlocked {
            return false;
        }
        if byte.is_ascii_graphic() && self.typed_len < PASSWORD_MAX {
            self.typed[self.typed_len] = byte;
            self.typed_len += 1;
            return true;
        }
        false
    }

    /// Backspace one character.
    pub fn backspace(&mut self) -> bool {
        if self.typed_len == 0 {
            return false;
        }
        self.typed_len -= 1;
        self.typed[self.typed_len] = 0;
        true
    }

    /// How many characters are typed.
    pub const fn typed_len(&self) -> usize {
        self.typed_len
    }

    /// Submit the typed password. `check` decides whether it is right —
    /// the authentication service's job, not this crate's.
    pub fn submit(&mut self, check: impl FnOnce(&[u8]) -> bool) -> bool {
        if matches!(self.state, LockState::Delay(_)) {
            return false;
        }
        let password = &self.typed[..self.typed_len];
        if check(password) {
            self.state = LockState::Unlocked;
            self.clear_typed();
            return true;
        }
        self.failures = self.failures.saturating_add(1);
        self.clear_typed();
        if self.failures >= ATTEMPTS_BEFORE_DELAY {
            self.state = LockState::Delay(DELAY_TICKS);
        } else {
            self.state = LockState::Failed;
        }
        false
    }

    /// Advance one tick; the lockout delay counts down to `Prompt`.
    pub fn tick(&mut self) {
        if let LockState::Delay(remaining) = self.state {
            if remaining <= 1 {
                self.state = LockState::Prompt;
            } else {
                self.state = LockState::Delay(remaining - 1);
            }
        }
    }

    /// Clear the typed field — Escape, and every failed attempt.
    fn clear_typed(&mut self) {
        self.typed_len = 0;
        self.typed = [0; PASSWORD_MAX];
    }
}

impl Default for ScreenLock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "nova";

    /// The authentication service's check, as the tests see it.
    fn check(password: &[u8]) -> bool {
        password == PASSWORD.as_bytes()
    }

    #[test]
    fn the_right_password_unlocks() {
        let mut lock = ScreenLock::new();
        for byte in PASSWORD.bytes() {
            assert!(lock.type_char(byte));
        }
        assert!(lock.submit(check));
        assert_eq!(lock.state(), LockState::Unlocked);
    }

    #[test]
    fn a_wrong_password_fails_and_clears_the_field() {
        let mut lock = ScreenLock::new();
        for byte in b"wrong" {
            lock.type_char(*byte);
        }
        assert_eq!(lock.typed_len(), 5);
        assert!(!lock.submit(check));
        assert_eq!(lock.typed_len(), 0, "the field clears on failure");
        assert_eq!(lock.state(), LockState::Failed);
    }

    #[test]
    fn backspace_edits_the_field() {
        let mut lock = ScreenLock::new();
        lock.type_char(b'a');
        lock.type_char(b'b');
        assert!(lock.backspace());
        assert_eq!(lock.typed_len(), 1);
        assert!(lock.backspace());
        assert_eq!(lock.typed_len(), 0);
        assert!(!lock.backspace(), "backspacing an empty field is a no-op");
    }

    #[test]
    fn too_many_failures_freeze_input() {
        let mut lock = ScreenLock::new();
        for _ in 0..ATTEMPTS_BEFORE_DELAY {
            lock.type_char(b'x');
            assert!(!lock.submit(check));
        }
        assert!(matches!(lock.state(), LockState::Delay(_)));
        assert!(!lock.type_char(b'a'), "input is frozen");
        assert!(!lock.submit(check), "submit is frozen too");
    }

    #[test]
    fn the_delay_counts_down_to_the_prompt() {
        let mut lock = ScreenLock::new();
        for _ in 0..ATTEMPTS_BEFORE_DELAY {
            lock.type_char(b'x');
            lock.submit(check);
        }
        for _ in 0..DELAY_TICKS {
            lock.tick();
        }
        assert_eq!(lock.state(), LockState::Prompt);
        assert!(lock.type_char(PASSWORD.as_bytes()[0]));
    }

    #[test]
    fn unlock_survives_a_full_cycle() {
        let mut lock = ScreenLock::new();
        for _ in 0..ATTEMPTS_BEFORE_DELAY {
            lock.type_char(b'x');
            lock.submit(check);
        }
        for _ in 0..DELAY_TICKS {
            lock.tick();
        }
        for byte in PASSWORD.bytes() {
            lock.type_char(byte);
        }
        assert!(lock.submit(check));
        assert_eq!(lock.state(), LockState::Unlocked);
        // After unlocking, typing is dead — the dialog is gone.
        assert!(!lock.type_char(b'a'));
    }
}
