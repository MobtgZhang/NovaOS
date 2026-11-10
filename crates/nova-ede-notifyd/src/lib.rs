//! Rust port of `ede-notifyd`, EDE's notification daemon.
//!
//! Applications post a notification; the daemon renders balloons stacked
//! from the bottom-right corner for a few seconds. NovaOS's IPC cannot yet
//! carry a string from one arbitrary process to another, so the transport
//! is the filesystem: an application writes `/var/run/notify/<app>` with
//! `summary\nbody`, and the desktop daemon drains the directory on each
//! tick, renders the balloons, and deletes the files it consumed.
//!
//! This crate is the model: a bounded queue with ids, replace semantics,
//! urgency levels, sticky (`timeout = 0`) notifications, hover pausing,
//! dismissal, and the stacking layout the renderer paints. Transport is
//! the caller's business.

#![no_std]
#![forbid(unsafe_code)]

/// The maximum length of an application name.
pub const MAX_APP: usize = 48;
/// The maximum length of a summary line.
pub const MAX_SUMMARY: usize = 64;
/// The maximum length of a body line.
pub const MAX_BODY: usize = 160;
/// The maximum number of pending notifications.
pub const MAX_PENDING: usize = 8;

/// The urgency classes of the freedesktop notification spec.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Urgency {
    /// Low: may be dropped under pressure.
    Low,
    /// Normal: the default.
    Normal,
    /// Critical: never auto-expired.
    Critical,
}

/// One notification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Notification {
    /// The spec's notification id; 0 means "assign me one".
    pub id: u32,
    /// Which application posted it.
    pub app: [u8; MAX_APP],
    app_len: usize,
    /// The one-line summary.
    pub summary: [u8; MAX_SUMMARY],
    summary_len: usize,
    /// The longer body text.
    pub body: [u8; MAX_BODY],
    body_len: usize,
    /// The urgency class.
    pub urgency: Urgency,
    /// The tick at which the notification expires; `u32::MAX` for sticky.
    pub expires_at: u32,
    /// Whether the pointer is hovering: while true, expiry is paused.
    pub hovered: bool,
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

/// The balloon stack's layout: where each visible balloon sits.
///
/// Balloons stack upward from the bottom-right, `width` pixels wide with
/// an 8-pixel gutter, the newest at the bottom.
#[derive(Clone, Copy, Debug)]
pub struct StackLayout {
    /// The x of the stack's right edge.
    pub right: i32,
    /// The y of the stack's bottom edge.
    pub bottom: i32,
    /// Each visible balloon's rectangle, newest first.
    pub rects: [Option<Rect>; MAX_PENDING],
}

/// A pixel rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// The notification queue.
#[derive(Clone, Copy, Debug)]
pub struct NotifyBus {
    pending: [Option<Notification>; MAX_PENDING],
    /// The tick of the last drain; used for expiry.
    pub tick: u32,
    /// How long a notification stays visible, in ticks.
    pub lifetime: u32,
    /// The next id to hand out; 0 is reserved for "assign me one".
    next_id: u32,
}

impl NotifyBus {
    /// An empty bus with a default lifetime of 300 ticks.
    pub const fn new() -> Self {
        Self {
            pending: [None; MAX_PENDING],
            tick: 0,
            lifetime: 300,
            next_id: 1,
        }
    }

    /// Post a notification. Returns false when the queue is full.
    pub fn post(&mut self, app: &str, summary: &str, body: &str) -> bool {
        self.post_with(app, summary, body, Urgency::Normal, self.lifetime)
    }

    /// Post a notification with explicit urgency and timeout.
    ///
    /// `timeout` of zero means sticky: the balloon stays until it is
    /// dismissed, exactly as the spec's `expire_timeout = 0` says.
    pub fn post_with(
        &mut self,
        app: &str,
        summary: &str,
        body: &str,
        urgency: Urgency,
        timeout: u32,
    ) -> bool {
        let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut notification = Notification {
            id,
            app: [0; MAX_APP],
            app_len: 0,
            summary: [0; MAX_SUMMARY],
            summary_len: 0,
            body: [0; MAX_BODY],
            body_len: 0,
            urgency,
            // Sticky notifications get the sentinel: `tick` never reaches
            // `u32::MAX` on its own.
            expires_at: if timeout == 0 {
                u32::MAX
            } else {
                self.tick.saturating_add(timeout)
            },
            hovered: false,
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

    /// Post from the wire line `app|summary|body|urgency|timeout`.
    ///
    /// The original two-field form still parses: urgency defaults to
    /// Normal and timeout to the bus lifetime.
    pub fn post_wire(&mut self, line: &str) -> bool {
        let mut parts = line.splitn(5, '|');
        let app = parts.next().unwrap_or("?");
        let summary = parts.next().unwrap_or("");
        let body = parts.next().unwrap_or("");
        if summary.is_empty() {
            return false;
        }
        let urgency = match parts.next() {
            Some("critical") => Urgency::Critical,
            Some("low") => Urgency::Low,
            _ => Urgency::Normal,
        };
        // A parsed `0` is sticky — the spec's expire_timeout = 0. Only a
        // missing or unreadable field falls back to the bus lifetime.
        let timeout = match parts.next().and_then(|text| text.parse::<u32>().ok()) {
            Some(timeout) => timeout,
            None => self.lifetime,
        };
        self.post_with(app, summary, body, urgency, timeout)
    }

    /// Replace the notification with `replaces_id`, keeping its slot; a
    /// zero or unknown id posts a new one. This is the spec's
    /// `replaces_id` argument.
    pub fn replace(
        &mut self,
        replaces_id: u32,
        app: &str,
        summary: &str,
        body: &str,
        urgency: Urgency,
        timeout: u32,
    ) -> bool {
        if replaces_id == 0 {
            return self.post_with(app, summary, body, urgency, timeout);
        }
        let Some(slot) = self
            .pending
            .iter_mut()
            .find(|slot| slot.is_some_and(|note| note.id == replaces_id))
        else {
            return self.post_with(app, summary, body, urgency, timeout);
        };
        let Some(note) = slot.as_mut() else {
            return false;
        };
        copy(&mut note.summary, &mut note.summary_len, summary.as_bytes());
        copy(&mut note.body, &mut note.body_len, body.as_bytes());
        copy(&mut note.app, &mut note.app_len, app.as_bytes());
        note.urgency = urgency;
        note.expires_at = if timeout == 0 {
            u32::MAX
        } else {
            self.tick.saturating_add(timeout)
        };
        true
    }

    /// Advance one tick; expired notifications are dropped. Sticky ones
    /// and hovered ones survive.
    pub fn tick(&mut self) {
        self.tick = self.tick.saturating_add(1);
        for slot in self.pending.iter_mut() {
            let expire = slot.is_some_and(|note| {
                !note.hovered && note.urgency != Urgency::Critical && note.expires_at <= self.tick
            });
            if expire {
                *slot = None;
            }
        }
    }

    /// Report the pointer entering the balloon with `id`: its expiry is
    /// paused for as long as the hover lasts.
    pub fn hover(&mut self, id: u32) -> bool {
        for slot in self.pending.iter_mut() {
            if slot.is_some_and(|note| note.id == id) {
                slot.as_mut().unwrap().hovered = true;
                return true;
            }
        }
        false
    }

    /// Report the pointer leaving: expiry resumes.
    pub fn unhover(&mut self, id: u32) -> bool {
        for slot in self.pending.iter_mut() {
            if slot.is_some_and(|note| note.id == id) {
                slot.as_mut().unwrap().hovered = false;
                return true;
            }
        }
        false
    }

    /// Dismiss the notification with `id` — the spec's CloseNotification.
    pub fn dismiss(&mut self, id: u32) -> bool {
        for slot in self.pending.iter_mut() {
            if slot.is_some_and(|note| note.id == id) {
                *slot = None;
                return true;
            }
        }
        false
    }

    /// The number of pending notifications.
    pub fn pending(&self) -> usize {
        self.pending.iter().filter(|n| n.is_some()).count()
    }

    /// The newest notification, if any — the one the balloon shows.
    pub fn current(&self) -> Option<Notification> {
        self.pending.iter().flatten().next_back().copied()
    }

    /// Every pending notification, oldest first.
    pub fn all(&self) -> impl Iterator<Item = Notification> + '_ {
        self.pending.iter().flatten().copied()
    }

    /// The layout of the balloon stack for a `width`-wide surface whose
    /// balloons sit `height` pixels tall each, 8 pixels apart, anchored to
    /// the bottom-right at (`right`, `bottom`).
    pub fn layout(&self, right: i32, bottom: i32, width: u32, height: u32) -> StackLayout {
        let mut rects = [None; MAX_PENDING];
        let mut y = bottom;
        for (index, _note) in self.pending.iter().flatten().rev().enumerate() {
            let top = y - height as i32;
            rects[index] = Some(Rect {
                x: right - width as i32,
                y: top,
                width,
                height,
            });
            y = top - 8;
        }
        StackLayout {
            right,
            bottom,
            rects,
        }
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

    #[test]
    fn ids_are_unique_and_nonzero() {
        let mut bus = NotifyBus::new();
        bus.post("a", "one", "");
        bus.post("b", "two", "");
        let mut seen = [0u32; MAX_PENDING];
        let mut count = 0;
        for note in bus.all() {
            assert!(note.id != 0, "zero is the 'assign me' id");
            assert!(!seen.contains(&note.id), "ids must be unique");
            seen[count] = note.id;
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn replace_updates_in_place() {
        let mut bus = NotifyBus::new();
        bus.post("downloader", "0%", "starting");
        let id = bus.current().unwrap().id;
        assert!(bus.replace(id, "downloader", "50%", "halfway", Urgency::Normal, 300));
        assert_eq!(bus.pending(), 1, "replace must not add a second balloon");
        assert_eq!(bus.current().unwrap().summary(), "50%");
        // An unknown id posts a new one, as the spec says.
        assert!(bus.replace(9999, "x", "y", "", Urgency::Normal, 300));
        assert_eq!(bus.pending(), 2);
    }

    #[test]
    fn zero_replaces_id_posts_new() {
        let mut bus = NotifyBus::new();
        assert!(bus.replace(0, "a", "b", "", Urgency::Normal, 300));
        assert_eq!(bus.pending(), 1);
    }

    #[test]
    fn critical_notifications_survive_their_timeout() {
        let mut bus = NotifyBus::new();
        bus.post_with("battery", "Low battery", "plug in", Urgency::Critical, 1);
        for _ in 0..500 {
            bus.tick();
        }
        assert_eq!(bus.pending(), 1, "critical balloons are never auto-expired");
    }

    #[test]
    fn zero_timeout_is_sticky_until_dismissed() {
        let mut bus = NotifyBus::new();
        bus.post_with("a", "b", "", Urgency::Normal, 0);
        for _ in 0..1000 {
            bus.tick();
        }
        assert_eq!(bus.pending(), 1);
        let id = bus.current().unwrap().id;
        assert!(bus.dismiss(id));
        assert_eq!(bus.pending(), 0);
    }

    #[test]
    fn hover_pauses_expiry() {
        let mut bus = NotifyBus::new();
        bus.post_with("a", "b", "", Urgency::Normal, 3);
        let id = bus.current().unwrap().id;
        bus.tick();
        bus.tick();
        assert!(bus.hover(id));
        bus.tick();
        bus.tick();
        bus.tick();
        assert_eq!(bus.pending(), 1, "hovered balloons do not expire");
        assert!(bus.unhover(id));
        bus.tick();
        assert_eq!(bus.pending(), 0);
    }

    #[test]
    fn current_is_the_newest() {
        let mut bus = NotifyBus::new();
        bus.post("a", "first", "");
        bus.tick();
        bus.post("b", "second", "");
        assert_eq!(bus.current().unwrap().summary(), "second");
    }

    #[test]
    fn dismiss_removes_only_the_target() {
        let mut bus = NotifyBus::new();
        bus.post("a", "one", "");
        bus.post("b", "two", "");
        let second_id = bus.current().unwrap().id;
        assert!(bus.dismiss(second_id));
        assert_eq!(bus.pending(), 1);
        assert_eq!(bus.current().unwrap().summary(), "one");
        assert!(!bus.dismiss(second_id), "double dismiss is a no-op");
    }

    #[test]
    fn layout_stacks_upward_from_the_corner() {
        let mut bus = NotifyBus::new();
        bus.post("a", "one", "");
        bus.post("b", "two", "");
        let layout = bus.layout(1000, 700, 256, 88);
        let bottom = layout.rects[0].unwrap();
        let top = layout.rects[1].unwrap();
        assert_eq!(bottom.x + bottom.width as i32, 1000);
        assert_eq!(bottom.y + bottom.height as i32, 700);
        assert_eq!(top.y + top.height as i32 + 8, bottom.y);
    }

    #[test]
    fn wire_carries_urgency_and_timeout() {
        let mut bus = NotifyBus::new();
        assert!(bus.post_wire("battery|Low|plug in|critical|0"));
        let note = bus.current().unwrap();
        assert_eq!(note.urgency, Urgency::Critical);
        assert_eq!(note.expires_at, u32::MAX, "zero timeout is sticky");
    }
}
