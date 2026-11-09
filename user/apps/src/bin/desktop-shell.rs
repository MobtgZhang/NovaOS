#![no_std]
#![no_main]

//! The desktop shell: the background, the icons, and the panel.
//!
//! # What this is
//!
//! A fullscreen client of the compositor at the back of the z-order. It
//! paints the wallpaper from `nova_desktop`, the four icons on the left,
//! and the panel along the bottom, and it turns clicks on those into
//! process spawns. It is not a window manager — z-order and focus stay
//! with the compositor — and it is not a file manager.
//!
//! # Why commands are spawned, not linked
//!
//! Each icon `exec`s its application by name. The desktop never contains
//! application code, so a broken application cannot take the desktop down,
//! and the icons remain the same launchers the boot menu uses.

use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, process, yield_now};
use nova_desktop::{DesktopCommand, DesktopShell};
use nova_protocols::input::{INPUT_BUTTON_PRIMARY, INPUT_EVENT_POINTER};
use nova_ui::{Canvas, typeface};

/// The GUI face: Source Han Sans, the same as the other desktop apps.
static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

/// The shell's fixed mode. The panel and icons are laid out for this size.
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
const MAX_EVENTS: usize = 8;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] desktop-shell ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!(
                "desktop-shell: window failed {}\r\n",
                error.name()
            ));
            return -1;
        }
    };
    // Ask to be placed at the origin — the server may grant it, which is
    // what a desktop background wants, or clamp it, which still leaves a
    // coherent picture on smaller outputs.
    let _ = nova_apps::graphics::compositor_move(window.surface, 0, 0);

    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);
    let mut desktop = DesktopShell::new(HEIGHT);
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];
    let mut prev_primary = false;

    desktop.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!(
            "desktop-shell: present failed {}\r\n",
            error.name()
        ));
        return -1;
    }

    loop {
        let count = window.drain(&mut events).unwrap_or(0);
        let mut dirty = false;

        for event in &events[..count] {
            match event.kind {
                INPUT_EVENT_POINTER => {
                    let primary = event.buttons & INPUT_BUTTON_PRIMARY != 0;
                    let released = prev_primary && !primary;
                    prev_primary = primary;
                    if let Some(command) = desktop.on_pointer(event.x, event.y, primary, released) {
                        match command {
                            DesktopCommand::Launch(name) => {
                                let _ = console::write_fmt(format_args!(
                                    "desktop-shell: launch {}\r\n",
                                    name
                                ));
                                let _ = process::exec(name);
                            }
                        }
                    }
                    dirty = true;
                }
                _ => {}
            }
        }

        if dirty {
            desktop.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ = console::write_fmt(format_args!(
                    "desktop-shell: present failed {}\r\n",
                    error.name()
                ));
                return -1;
            }
        }

        yield_now();
    }
}
