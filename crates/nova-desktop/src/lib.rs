//! NovaOS's desktop environment: the things that make a machine *feel* like
//! a desktop rather than a stack of windows.
//!
//! # What this crate is
//!
//! The desktop shell is a client of the compositor like any other window —
//! with one difference: it owns the *background*. It paints the wallpaper,
//! the icons, and the panel, and it routes clicks on those things to the
//! actions they represent (launch an application). It never owns other
//! windows; applications are still applications, just reachable by clicking.
//!
//! # What this crate is not
//!
//! A window manager. The compositor already decides z-order, focus, and
//! placement, and this crate deliberately stays out of those decisions.
//! `nova-desktop` is *chrome*: the parts of the experience that are not any
//! one application's responsibility.
//!
//! # The three modules
//!
//! - [`wallpaper`] — the deep-sea gradient with a horizon glow and a
//!   deterministic star field. Drawn once per repaint, from pure math.
//! - [`icons`] — the desktop icon set, hit testing, and activation.
//! - [`panel`] — the bottom bar: the NovaOS wordmark and the task chips.
//!
//! # Design notes Wayland users will recognise
//!
//! - The background is a surface like any other, painted by a client; the
//!   server (compositor) keeps it at the back of the z-order.
//! - Icons are not "files" — they are actions (launch an application). The
//!   old desktop metaphor mapped icons to files; modern shells map them to
//!   launchers, which is what these are.
//! - Everything is drawn with solid-colour primitives and vector text;
//!   there is no bitmap asset anywhere in the environment.

#![no_std]
#![forbid(unsafe_code)]

pub mod icons;
pub mod panel;
pub mod wallpaper;

use nova_ui::Canvas;

pub use icons::Icon;

/// The colour of the desktop background, deepest tone.
pub const DESKTOP_DEEP: u32 = 0x000B1420;
/// The colour at the horizon of the gradient.
pub const DESKTOP_HORIZON: u32 = 0x001A2A3A;
/// The accent colour used for highlights and the panel wordmark dot.
pub const ACCENT: u32 = 0x0068AAC4;
/// An icon's label colour on the wallpaper.
pub const LABEL: u32 = 0x00E6ECF2;
/// The drop-shadow colour under the label text, for legibility on the glow.
pub const LABEL_SHADOW: u32 = 0x00000000;
/// The panel's background, a shade of the deep tone.
pub const PANEL_BG: u32 = 0x00101A26;
/// The panel's separator line.
pub const PANEL_LINE: u32 = 0x00253444;

/// One action the desktop shell decided a click means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DesktopCommand {
    /// Launch an application by name.
    Launch(&'static str),
}

/// The desktop shell's drawing context and interaction state.
///
/// The shell is a plain value: it knows where its icons and panel buttons
/// are, tracks which one is under the pointer, and paints everything on
/// demand. It allocates nothing and keeps no history.
#[derive(Clone, Copy, Debug)]
pub struct DesktopShell {
    /// Which icon, if any, is currently pressed by the pointer.
    pub pressed: Option<u8>,
    /// Which panel chip, if any, is currently pressed.
    pub panel_pressed: Option<u8>,
    /// The surface height in pixels, needed to convert a global y into a
    /// panel-local one for hit testing.
    height: u32,
}

impl DesktopShell {
    /// A shell with nothing pressed, serving a surface `height` pixels tall.
    pub const fn new(height: u32) -> Self {
        Self {
            pressed: None,
            panel_pressed: None,
            height,
        }
    }

    /// Draw the wallpaper.
    pub fn draw_background(&self, canvas: &mut Canvas) {
        wallpaper::paint(canvas);
    }

    /// Draw the icons.
    pub fn draw_icons(&self, canvas: &mut Canvas) {
        icons::draw_all(canvas, self.pressed);
    }

    /// Draw the panel.
    pub fn draw_panel(&self, canvas: &mut Canvas, width: u32, height: u32) {
        panel::draw(canvas, width, height, self.panel_pressed);
    }

    /// Repaint everything this shell owns.
    pub fn draw(&self, canvas: &mut Canvas) {
        let width = canvas.width();
        let height = canvas.height();
        self.draw_background(canvas);
        self.draw_icons(canvas);
        self.draw_panel(canvas, width as u32, height as u32);
    }

    /// Handle one pointer press/release; returns a command if one was
    /// activated.
    ///
    /// `released` is true for the release edge, false for the press edge.
    /// An icon fires on release inside its bounds — the usual, correct
    /// contract — and only if the press started on it.
    pub fn on_pointer(
        &mut self,
        x: i32,
        y: i32,
        primary_down: bool,
        released: bool,
    ) -> Option<DesktopCommand> {
        // The panel strip occupies the bottom PANEL_HEIGHT pixels of the
        // surface; its hit test works in panel-local coordinates.
        let panel_y = y - self.height as i32 + panel::PANEL_HEIGHT as i32;

        // Icons
        if primary_down {
            self.pressed = icons::hit(x, y);
            self.panel_pressed = panel::hit(x, panel_y);
            return None;
        }
        if released {
            let icon = self.pressed.take();
            let panel_chip = self.panel_pressed.take();
            if let Some(id) = icon
                && icons::hit(x, y) == Some(id)
            {
                return Some(DesktopCommand::Launch(icons::APP[id as usize]));
            }
            if let Some(id) = panel_chip
                && panel::hit(x, panel_y) == Some(id)
            {
                return Some(DesktopCommand::Launch(panel::APP[id as usize]));
            }
        }
        None
    }
}

impl Default for DesktopShell {
    fn default() -> Self {
        Self::new(768)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons;
    use crate::panel;

    #[test]
    fn icon_hit_testing_finds_each_icon() {
        for index in 0..icons::ICONS.len() {
            let (x0, y0, _x1, _y1) = icons::cell(index).unwrap();
            let centre_x = x0 + 30;
            let centre_y = y0 + 20;
            assert_eq!(icons::hit(centre_x, centre_y), Some(index as u8));
        }
        assert_eq!(icons::hit(1000, 700), None);
    }

    #[test]
    fn icon_launch_names_match_the_boot_services() {
        assert_eq!(
            icons::APP,
            &["terminal", "file-manager", "editor", "widgets-demo"]
        );
        assert_eq!(panel::APP, icons::APP);
    }

    #[test]
    fn panel_chips_do_not_overlap() {
        for i in 0..panel::CHIP_LABELS.len() {
            for j in (i + 1)..panel::CHIP_LABELS.len() {
                let (a0, _, a1, _) = panel::chip_bounds(i);
                let (b0, _, b1, _) = panel::chip_bounds(j);
                assert!(a1 <= b0 || b1 <= a0, "chips {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn panel_hit_testing_finds_chips() {
        for index in 0..panel::CHIP_LABELS.len() {
            let (x0, _y0, x1, _y1) = panel::chip_bounds(index);
            assert_eq!(panel::hit((x0 + x1) / 2, 10), Some(index as u8));
        }
        assert_eq!(panel::hit(8, 10), None);
    }

    #[test]
    fn a_press_arms_and_release_fires() {
        let mut shell = DesktopShell::new(768);
        let target = icons::hit(30, 20).unwrap();
        // Press inside the icon arms it.
        assert_eq!(shell.on_pointer(30, 20, true, false), None);
        assert_eq!(shell.pressed, Some(target));
        // Release inside fires Launch.
        assert_eq!(
            shell.on_pointer(30, 20, false, true),
            Some(DesktopCommand::Launch(icons::APP[target as usize]))
        );
        assert_eq!(shell.pressed, None);
    }

    #[test]
    fn a_release_outside_cancels() {
        let mut shell = DesktopShell::new(768);
        assert_eq!(shell.on_pointer(30, 20, true, false), None);
        // Drag off the icon, then release: nothing fires.
        assert_eq!(shell.on_pointer(900, 700, false, true), None);
        assert_eq!(shell.pressed, None);
    }

    #[test]
    fn panel_press_fires_on_release_inside() {
        let mut shell = DesktopShell::new(768);
        // First chip's centre.
        let (x0, _, x1, _) = panel::chip_bounds(0);
        let centre = (x0 + x1) / 2;
        // Global y of the panel strip: 768 - 32 + 10.
        let global_y = 768 - panel::PANEL_HEIGHT as i32 + 10;
        assert_eq!(shell.on_pointer(centre, global_y, true, false), None);
        assert_eq!(
            shell.on_pointer(centre, global_y, false, true),
            Some(DesktopCommand::Launch(panel::APP[0]))
        );
    }
}
