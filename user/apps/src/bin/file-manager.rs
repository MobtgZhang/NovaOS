#![no_std]
#![no_main]

use nova_apps::{console, graphics, yield_now};
use nova_protocols::surface::SurfaceRect;

const WIDTH: u32 = 360;
const HEIGHT: u32 = 260;
const STRIDE: usize = WIDTH as usize * 4;

fn paint(buffer: *mut u8) {
    graphics::fill_word(
        buffer,
        WIDTH as usize * HEIGHT as usize,
        graphics::colour(26, 34, 42),
    );
    graphics::fill_rect(
        buffer,
        STRIDE,
        0,
        0,
        WIDTH as usize,
        30,
        graphics::colour(88, 116, 148),
    );
    for row in 0..6 {
        let colour = if row % 2 == 0 {
            graphics::colour(58, 74, 90)
        } else {
            graphics::colour(42, 56, 70)
        };
        graphics::fill_rect(buffer, STRIDE, 16, 46 + row * 30, 328, 22, colour);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] file-manager ready\r\n");

    let (surface, buffer) = match graphics::open_window(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!(
                "file-manager: window failed {}\r\n",
                error.name()
            ));
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
        let _ = console::write_str("file-manager: damage failed\r\n");
        return -1;
    }
    if graphics::surface_commit(surface, 1).is_err() {
        let _ = console::write_str("file-manager: commit failed\r\n");
        return -1;
    }

    loop {
        yield_now();
    }
}
