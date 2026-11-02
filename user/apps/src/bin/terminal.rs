#![no_std]
#![no_main]

use nova_apps::{console, graphics, yield_now};
use nova_protocols::surface::SurfaceRect;

const WIDTH: u32 = 420;
const HEIGHT: u32 = 260;
const STRIDE: usize = WIDTH as usize * 4;

fn paint(buffer: *mut u8) {
    graphics::fill_word(
        buffer,
        WIDTH as usize * HEIGHT as usize,
        graphics::colour(14, 18, 24),
    );
    graphics::fill_rect(
        buffer,
        STRIDE,
        0,
        0,
        WIDTH as usize,
        32,
        graphics::colour(54, 82, 116),
    );
    for line in 0..6 {
        graphics::fill_rect(
            buffer,
            STRIDE,
            20,
            54 + line * 28,
            360,
            8,
            graphics::colour(92, 158, 96),
        );
    }
    graphics::fill_rect(
        buffer,
        STRIDE,
        20,
        220,
        360,
        4,
        graphics::colour(180, 190, 120),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] terminal ready\r\n");

    let (surface, buffer) = match graphics::open_window(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ =
                console::write_fmt(format_args!("terminal: window failed {}\r\n", error.name()));
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
        let _ = console::write_str("terminal: damage failed\r\n");
        return -1;
    }
    if graphics::surface_commit(surface, 1).is_err() {
        let _ = console::write_str("terminal: commit failed\r\n");
        return -1;
    }

    loop {
        yield_now();
    }
}
