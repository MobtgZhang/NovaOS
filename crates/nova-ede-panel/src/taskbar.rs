//! The taskbar's window list — the model behind the taskbar applet.
//!
//! Upstream's taskbar enumerates mapped windows from the window manager
//! and renders one button each: icon, title, active highlight. NovaOS's
//! compositor publishes the same facts — surface ids, titles, focus,
//! minimized state — through its window-list op, and this model renders
//! them. Clicking an active window minimizes it; clicking a minimized or
//! background window activates it, exactly like upstream.

/// How many windows one taskbar tracks.
pub const MAX_WINDOWS: usize = 8;
/// The longest window title kept.
pub const TITLE_MAX: usize = 24;

/// One window in the taskbar's list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskEntry {
    /// The compositor surface id — the window's identity.
    pub id: u32,
    /// The window title, as the compositor published it.
    pub title: [u8; TITLE_MAX],
    title_len: usize,
    /// Whether this window has focus.
    pub active: bool,
    /// Whether this window is minimized.
    pub minimized: bool,
}

impl TaskEntry {
    /// A window entry with a title.
    pub fn new(id: u32, title: &str, active: bool, minimized: bool) -> Self {
        let mut entry = Self {
            id,
            title: [0; TITLE_MAX],
            title_len: 0,
            active,
            minimized,
        };
        let bytes = title.as_bytes();
        let take = bytes.len().min(TITLE_MAX);
        entry.title[..take].copy_from_slice(&bytes[..take]);
        entry.title_len = take;
        entry
    }

    /// The window's title.
    pub fn title(&self) -> &str {
        core::str::from_utf8(&self.title[..self.title_len]).unwrap_or("")
    }
}

/// What a click on a taskbar button means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskClick {
    /// Activate the window with this id.
    Activate(u32),
    /// Minimize the active window with this id.
    Minimize(u32),
    /// Nothing — the click landed between buttons.
    Nothing,
}

/// The window list.
#[derive(Clone, Copy, Debug)]
pub struct TaskbarModel {
    entries: [Option<TaskEntry>; MAX_WINDOWS],
    count: usize,
}

impl TaskbarModel {
    /// An empty list.
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_WINDOWS],
            count: 0,
        }
    }

    /// Replace the whole list from the compositor's window inventory.
    pub fn sync(&mut self, windows: &[TaskEntry]) {
        self.count = 0;
        for window in windows.iter().take(MAX_WINDOWS) {
            self.entries[self.count] = Some(*window);
            self.count += 1;
        }
    }

    /// How many windows the taskbar shows.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The window at `index`.
    pub fn entry(&self, index: usize) -> Option<&TaskEntry> {
        self.entries.get(index)?.as_ref()
    }

    /// The focused window, if any.
    pub fn active(&self) -> Option<&TaskEntry> {
        self.entries
            .iter()
            .take(self.count)
            .flatten()
            .find(|entry| entry.active)
    }

    /// Resolve a click at `x` within a row of `button_width`-wide buttons
    /// starting at `x0`. The active window minimizes on click; any other
    /// window activates.
    pub fn click(&self, x: i32, x0: i32, button_width: i32) -> TaskClick {
        if button_width <= 0 {
            return TaskClick::Nothing;
        }
        let index = (x - x0) / button_width;
        if index < 0 || index as usize >= self.count {
            return TaskClick::Nothing;
        }
        let Some(entry) = self.entries[index as usize].as_ref() else {
            return TaskClick::Nothing;
        };
        if entry.active && !entry.minimized {
            TaskClick::Minimize(entry.id)
        } else {
            TaskClick::Activate(entry.id)
        }
    }
}

impl Default for TaskbarModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> TaskbarModel {
        let mut model = TaskbarModel::new();
        model.sync(&[
            TaskEntry::new(1, "Terminal", false, false),
            TaskEntry::new(2, "Editor", true, false),
            TaskEntry::new(3, "Files", false, true),
        ]);
        model
    }

    #[test]
    fn sync_replaces_the_list() {
        let model = model();
        assert_eq!(model.count(), 3);
        assert_eq!(model.entry(0).unwrap().title(), "Terminal");
        assert_eq!(model.entry(2).unwrap().id, 3);
        assert!(model.entry(2).unwrap().minimized);
    }

    #[test]
    fn the_active_window_is_found() {
        let model = model();
        assert_eq!(model.active().unwrap().id, 2);
    }

    #[test]
    fn clicking_the_active_window_minimizes() {
        let model = model();
        // Buttons start at x=90 and are 88 wide: button 1 (Editor, active)
        // spans 178..266.
        assert_eq!(model.click(200, 90, 88), TaskClick::Minimize(2));
    }

    #[test]
    fn clicking_any_other_window_activates() {
        let model = model();
        assert_eq!(model.click(10, 90, 88), TaskClick::Activate(1));
        assert_eq!(model.click(270, 90, 88), TaskClick::Activate(3));
    }

    #[test]
    fn a_click_between_buttons_is_nothing() {
        let model = model();
        assert_eq!(model.click(500, 90, 88), TaskClick::Nothing);
        assert_eq!(model.click(-5, 90, 88), TaskClick::Nothing);
    }

    #[test]
    fn a_minimized_active_window_activates_instead() {
        let mut model = TaskbarModel::new();
        model.sync(&[TaskEntry::new(9, "Docked", true, true)]);
        assert_eq!(model.click(0, 0, 88), TaskClick::Activate(9));
    }

    #[test]
    fn sync_caps_at_max_windows() {
        let mut model = TaskbarModel::new();
        let windows: [TaskEntry; 12] =
            core::array::from_fn(|index| TaskEntry::new(index as u32 + 1, "w", false, false));
        model.sync(&windows);
        assert_eq!(model.count(), MAX_WINDOWS);
    }
}
