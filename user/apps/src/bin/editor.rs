#![no_std]
#![no_main]

use nova_apps::{console, graphics, yield_now};
use nova_protocols::surface::SurfaceRect;

const WIDTH: u32 = 400;
const HEIGHT: u32 = 300;
const STRIDE: usize = WIDTH as usize * 4;

fn paint(buffer: *mut u8) {
    graphics::fill_word(
        buffer,
        WIDTH as usize * HEIGHT as usize,
        graphics::colour(236, 232, 218),
    );
    graphics::fill_rect(
        buffer,
        STRIDE,
        0,
        0,
        WIDTH as usize,
        34,
        graphics::colour(120, 148, 176),
    );
    for line in 0..8 {
        graphics::fill_rect(
            buffer,
            STRIDE,
            24,
            58 + line * 24,
            320,
            6,
            graphics::colour(70, 78, 92),
        );
    }
    graphics::fill_rect(
        buffer,
        STRIDE,
        24,
        252,
        120,
        6,
        graphics::colour(180, 120, 80),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] editor ready\r\n");

    let (surface, buffer) = match graphics::open_window(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!("editor: window failed {}\r\n", error.name()));
            return -1;
        }
    };

    paint(buffer);

    let rect = SurfaceRect {
        x: 0,
        y: 0,
        width: WIDTH,
        height: HEIGHT,
    };
    if graphics::surface_damage(surface, rect).is_err() {
        let _ = console::write_str("editor: damage failed\r\n");
        return -1;
    }
    if graphics::surface_commit(surface, 1).is_err() {
        let _ = console::write_str("editor: commit failed\r\n");
        return -1;
    }

    loop {
        yield_now();
    }
}
