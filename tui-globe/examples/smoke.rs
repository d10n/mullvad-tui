// SPDX-License-Identifier: GPL-3.0-or-later

//! Renders one frame at several yaw angles into an in-memory ratatui Buffer
//! and prints the result with `#` for any non-blank cell. Used to sanity-check
//! the projection without running an interactive terminal.

use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use tui_globe::{Camera, Globe, MapData};

fn main() {
    let map = MapData::embedded();
    let area = Rect::new(0, 0, 80, 30);
    for yaw in [0.0_f32, 1.0, 2.0, 3.0] {
        let camera = Camera {
            yaw,
            ..Camera::default()
        };
        let mut buf = Buffer::empty(area);
        Globe::new(&map, camera).render(area, &mut buf);
        println!("--- yaw = {yaw:.2} rad ---");
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                let sym = buf[(x, y)].symbol();
                line.push(if sym == " " || sym.is_empty() {
                    ' '
                } else {
                    '#'
                });
            }
            println!("{}", line.trim_end());
        }
        println!();
    }
}
