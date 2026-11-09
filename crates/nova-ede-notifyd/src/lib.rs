//! Rust port of `ede-notifyd`, EDE's notification daemon.
//!
//! Applications post a notification; the daemon renders a balloon in the
//! corner of the screen for a few seconds. NovaOS's IPC cannot yet carry a
//! string from one arbitrary process to another, so the transport is the
//! filesystem: an application writes `/var/run/notify/<app>` with
//! `summary\nbody`, and the desktop daemon drains the directory on each tick,
//! renders the newest balloon, and deletes the file it consumed.
//!
//! This crate is the model: a bounded queue, expiry by tick, and the most
//! recent message visible. Rendering lives in the shell; transport is the
//! caller's business.

#![no_std]
#![forbid(unsafe_code)]

/// The maximum length of a summary line.
pub const MAX_SUMMARY: usize = 64;
/// The maximum length of a body line.
pub const MAX_BODY: usize = 160;
/// The maximum number of pending notifications.
pub const MAX_PENDING: usize = 8;

/// One notification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Notification {
    /// Which application posted it.
    pub app: [u8; MAX_SUMMARY],
    app_len: usize,
    /// The one-line summary.
    pub summary: [u8; MAX_SUMMARY],
    summary_len: usize,
    /// The longer body text.
    pub body: [u8; MAX_BODY],
    body_len: usize,
    /// A tick at which the notification expires.
    pub expires_at: u32,
}

impl Notification {
    /// The application name.
    pub fn app(&self) -> &str {
        core::str::from_utf8(&self.app[..self.app_len]).unwrap_or("?")
    }

    /// The summary line.
    pub fn summary(&self) -> &str {
        core::str::from_utf8(&self.summary[..self.summary_len]).unwrap_or("")
    }

    /// The body text.
    pub fn body(&self) -> &str {
        core::str::from_utf8(&self.body[..self.body_len]).unwrap_or("")
    }
}

/// The notification queue.
#[derive(Clone, Copy, Debug)]
pub struct NotifyBus {
    pending: [Option<Notification>; MAX_PENDING],
    /// The tick of the last drain; used for expiry.
    pub tick: u32,
    /// How long a notification stays visible, in ticks.
    pub lifetime: u32,
}

impl NotifyBus {
    /// An empty bus with a default lifetime of 300 ticks.
    pub const fn new() -> Self {
        Self {
            pending: [None; MAX_PENDING],
            tick: 0,
            lifetime: 300,
        }
    }

    /// Post a notification. Returns false when the queue is full.
    pub fn post(&mut self, app: &str, summary: &str, body: &str) -> bool {
        let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        let mut notification = Notification {
            app: [0; MAX_SUMMARY],
            app_len: 0,
            summary: [0; MAX_SUMMARY],
            summary_len: 0,
            body: [0; MAX_BODY],
            body_len: 0,
            expires_at: self.tick.saturating_add(self.lifetime),
        };
        copy(
            &mut notification.app,
            &mut notification.app_len,
            app.as_bytes(),
        );
        copy(
            &mut notification.summary,
            &mut notification.summary_len,
            summary.as_bytes(),
        );
        copy(
            &mut notification.body,
            &mut notification.body_len,
            body.as_bytes(),
        );
        *slot = Some(notification);
        true
    }

    /// Advance one tick; expired notifications are dropped.
    pub fn tick(&mut self) {
        self.tick = self.tick.saturating_add(1);
        for slot in self.pending.iter_mut() {
            if slot.is_some_and(|n| n.expires_at <= self.tick) {
                *slot = None;
            }
        }
    }

    /// The number of pending notifications.
    pub fn pending(&self) -> usize {
        self.pending.iter().filter(|n| n.is_some()).count()
    }

    /// The most recent notification, if any.
    pub fn current(&self) -> Option<Notification> {
        self.pending
            .iter()
            .filter_map(|n| *n)
            .reduce(|a, b| if a.expires_at >= b.expires_at { a } else { b })
    }

    /// Parse one wire line `app|summary|body` into a notification.
    pub fn post_wire(&mut self, line: &str) -> bool {
        let mut parts = line.splitn(3, '|');
        let app = parts.next().unwrap_or("?");
        let summary = parts.next().unwrap_or("");
        let body = parts.next().unwrap_or("");
        if summary.is_empty() {
            return false;
        }
        self.post(app, summary, body)
    }
}

impl Default for NotifyBus {
    fn default() -> Self {
        Self::new()
    }
}

fn copy(slot: &mut [u8], len: &mut usize, value: &[u8]) {
    let take = value.len().min(slot.len());
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_and_expires() {
        let mut bus = NotifyBus::new();
        assert!(bus.post("terminal", "Done", "command completed"));
        assert_eq!(bus.pending(), 1);
        for _ in 0..300 {
            bus.tick();
        }
        assert_eq!(bus.pending(), 0);
    }

    #[test]
    fn wire_format_parses() {
        let mut bus = NotifyBus::new();
        assert!(bus.post_wire("editor|Saved|notes.txt written"));
        let current = bus.current().unwrap();
        assert_eq!(current.summary(), "Saved");
        assert!(!bus.post_wire("editor|"));
    }
}
