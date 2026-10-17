//! 32x32 pixel-art icons for drun's built-in entries (system commands, plugins, ChickenWM apps).
//! Drawn from integer shapes on a 32x32 grid, then given a 1px dark outline so every icon
//! shares the same pixel-art look.
//!
//! Preview: `cargo test -p chickenwm-launcher export_pixel_icon_sheet -- --ignored`
//! writes `pixel_icons_preview.bmp` (8x scale) to the temp directory.

pub const SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelIcon {
    Shutdown,
    Reboot,
    Lock,
    Sleep,
    Reload,
    RestoreExplorer,
    Calculator,
    Shell,
    WindowSwitch,
    Files,
    Settings,
    Browser,
    Snip,
}

impl PixelIcon {
    pub const ALL: [PixelIcon; 13] = [
        PixelIcon::Shutdown,
        PixelIcon::Reboot,
        PixelIcon::Lock,
        PixelIcon::Sleep,
        PixelIcon::Reload,
        PixelIcon::RestoreExplorer,
        PixelIcon::Calculator,
        PixelIcon::Shell,
        PixelIcon::WindowSwitch,
        PixelIcon::Files,
        PixelIcon::Settings,
        PixelIcon::Browser,
        PixelIcon::Snip,
    ];

    /// Straight (unpremultiplied) RGBA, `SIZE * SIZE * 4` bytes.
    pub fn rgba(self) -> Vec<u8> {
        let mut c = Canvas::new();
        match self {
            PixelIcon::Shutdown => draw_shutdown(&mut c),
            PixelIcon::Reboot => draw_reboot(&mut c),
            PixelIcon::Lock => draw_lock(&mut c),
            PixelIcon::Sleep => draw_sleep(&mut c),
            PixelIcon::Reload => draw_reload(&mut c),
            PixelIcon::RestoreExplorer => draw_restore_explorer(&mut c),
            PixelIcon::Calculator => draw_calculator(&mut c),
            PixelIcon::Shell => draw_shell(&mut c),
            PixelIcon::WindowSwitch => draw_window_switch(&mut c),
            PixelIcon::Files => draw_files(&mut c),
            PixelIcon::Settings => draw_settings(&mut c),
            PixelIcon::Browser => draw_browser(&mut c),
            PixelIcon::Snip => draw_snip(&mut c),
        }
        c.outline(OUTLINE);
        c.to_rgba()
    }
}

// Palette (0xRRGGBB; 0 = transparent is never used as a colour).
const OUTLINE: u32 = 0x1B1A24;
const WHITE: u32 = 0xF4F1E8;
const LIGHT: u32 = 0xC9C3B4;
const MID: u32 = 0x8A8577;
const DARK: u32 = 0x3E3A46;
const YELLOW: u32 = 0xF2C14E;
const ORANGE: u32 = 0xD98A36;
const RED: u32 = 0xD1453B;
const RED_DARK: u32 = 0x8E2A2A;
const BLUE: u32 = 0x4F8FD8;
const BLUE_DARK: u32 = 0x2C5A9E;
const BLUE_LIGHT: u32 = 0x9CC8F0;
const GREEN: u32 = 0x5CB85C;
const GREEN_DARK: u32 = 0x2F7D3A;
const PURPLE: u32 = 0x8B6FD8;

struct Canvas {
    px: [[Option<u32>; SIZE]; SIZE],
}

impl Canvas {
    fn new() -> Self {
        Self {
            px: [[None; SIZE]; SIZE],
        }
    }

    fn set(&mut self, x: i32, y: i32, c: Option<u32>) {
        if (0..SIZE as i32).contains(&x) && (0..SIZE as i32).contains(&y) {
            self.px[y as usize][x as usize] = c;
        }
    }

    fn dot(&mut self, x: i32, y: i32, c: u32) {
        self.set(x, y, Some(c));
    }

    /// Filled rectangle, inclusive corners.
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: u32) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.dot(x, y, c);
            }
        }
    }

    fn clear_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.set(x, y, None);
            }
        }
    }

    /// Calls `f` for every pixel whose centre lies within `r` of (`cx`, `cy`).
    fn each_in_disc(cx: f32, cy: f32, r: f32, mut f: impl FnMut(i32, i32)) {
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                if dx * dx + dy * dy <= r * r {
                    f(x, y);
                }
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: u32) {
        Self::each_in_disc(cx, cy, r, |x, y| self.dot(x, y, c));
    }

    fn clear_disc(&mut self, cx: f32, cy: f32, r: f32) {
        Self::each_in_disc(cx, cy, r, |x, y| self.set(x, y, None));
    }

    /// Ring between radii `r_in` and `r_out`, limited to pixels where `keep(dx, dy)` holds
    /// (offsets from the centre), so arcs can be cut out.
    fn ring_where(
        &mut self,
        cx: f32,
        cy: f32,
        r_out: f32,
        r_in: f32,
        c: u32,
        keep: impl Fn(f32, f32) -> bool,
    ) {
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let d2 = dx * dx + dy * dy;
                if d2 <= r_out * r_out && d2 > r_in * r_in && keep(dx, dy) {
                    self.dot(x, y, c);
                }
            }
        }
    }

    /// Bresenham line, `w` pixels thick (square brush).
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, w: i32, c: u32) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.rect(x, y, x + w - 1, y + w - 1, c);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Rows of a solid triangle given as (y, x_from, x_to) spans.
    fn spans(&mut self, spans: &[(i32, i32, i32)], c: u32) {
        for &(y, x0, x1) in spans {
            self.rect(x0, y, x1, y, c);
        }
    }

    /// Paints `c` on every empty pixel 4-adjacent to a filled one.
    fn outline(&mut self, c: u32) {
        let src = self.px;
        for y in 0..SIZE {
            for x in 0..SIZE {
                if src[y][x].is_some() {
                    continue;
                }
                let filled = |xx: i32, yy: i32| {
                    (0..SIZE as i32).contains(&xx)
                        && (0..SIZE as i32).contains(&yy)
                        && src[yy as usize][xx as usize].is_some()
                };
                let (xi, yi) = (x as i32, y as i32);
                if filled(xi - 1, yi) || filled(xi + 1, yi) || filled(xi, yi - 1) || filled(xi, yi + 1)
                {
                    self.px[y][x] = Some(c);
                }
            }
        }
    }

    fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIZE * SIZE * 4);
        for row in &self.px {
            for p in row {
                match p {
                    Some(c) => out.extend_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, *c as u8, 255]),
                    None => out.extend_from_slice(&[0, 0, 0, 0]),
                }
            }
        }
        out
    }
}

/// Round badge with a soft top-left highlight.
fn badge(c: &mut Canvas, fill: u32, shade: u32) {
    c.disc(16.0, 16.0, 14.5, shade);
    c.disc(15.5, 15.5, 13.5, fill);
}

fn draw_shutdown(c: &mut Canvas) {
    badge(c, RED, RED_DARK);
    // ⏻: open ring + bar through the gap
    c.ring_where(16.0, 17.0, 8.5, 5.5, WHITE, |dx, dy| !(dy < -2.0 && dx.abs() < 4.5));
    c.rect(14, 5, 17, 16, RED);
    c.rect(15, 6, 16, 15, WHITE);
}

fn draw_reboot(c: &mut Canvas) {
    badge(c, GREEN, GREEN_DARK);
    // Circular arrow, open at the top right, arrowhead pointing clockwise.
    c.ring_where(16.0, 16.0, 9.0, 6.0, WHITE, |dx, dy| !(dx > -1.0 && dy < 1.0 && dx > -dy * 0.2));
    c.spans(
        &[
            (3, 16, 17),
            (4, 16, 18),
            (5, 16, 19),
            (6, 16, 20),
            (7, 16, 21),
            (8, 16, 20),
            (9, 16, 19),
            (10, 16, 18),
            (11, 16, 17),
        ],
        WHITE,
    );
}

fn draw_lock(c: &mut Canvas) {
    // Shackle
    c.ring_where(16.0, 13.0, 9.0, 5.5, LIGHT, |_, dy| dy < 0.5);
    c.rect(7, 13, 10, 15, LIGHT);
    c.rect(22, 13, 25, 15, LIGHT);
    c.ring_where(16.0, 13.0, 9.0, 8.0, WHITE, |dx, dy| dy < -3.0 && dx < 0.0);
    // Body
    c.rect(5, 15, 27, 29, YELLOW);
    c.rect(5, 26, 27, 29, ORANGE);
    c.rect(6, 16, 26, 16, WHITE);
    // Keyhole
    c.disc(16.0, 20.5, 2.6, DARK);
    c.rect(15, 21, 16, 26, DARK);
}

fn draw_sleep(c: &mut Canvas) {
    // Crescent moon
    c.disc(14.0, 18.0, 11.5, YELLOW);
    c.clear_disc(19.5, 13.5, 9.0);
    c.ring_where(14.0, 18.0, 11.5, 10.5, WHITE, |dx, dy| dx < -4.0 && dy > -2.0 && dy < 6.0);
    // "z z"
    let mut z = |x: i32, y: i32, s: i32| {
        c.rect(x, y, x + s, y, PURPLE);
        c.line(x + s, y, x, y + s, 1, PURPLE);
        c.rect(x, y + s, x + s, y + s, PURPLE);
    };
    z(21, 3, 6);
    z(25, 12, 4);
}

fn draw_reload(c: &mut Canvas) {
    badge(c, BLUE, BLUE_DARK);
    // Two arcs chasing each other, each with an arrowhead.
    c.ring_where(16.0, 16.0, 9.5, 6.5, WHITE, |dx, dy| {
        !((dx > 0.0 && dy < 0.0 && dx < -dy * 1.2) || (dx < 0.0 && dy > 0.0 && -dx < dy * 1.2))
    });
    // Head at the top (pointing right)
    c.spans(&[(3, 16, 16), (4, 16, 17), (5, 16, 18), (6, 16, 19), (7, 16, 18), (8, 16, 17), (9, 16, 16)], WHITE);
    // Head at the bottom (pointing left)
    c.spans(&[(22, 15, 15), (23, 14, 15), (24, 13, 15), (25, 12, 15), (26, 13, 15), (27, 14, 15), (28, 15, 15)], WHITE);
}

fn folder(c: &mut Canvas, back: u32, front: u32, top: i32) {
    c.rect(3, top, 13, top + 2, back);
    c.rect(3, top + 2, 28, 27, back);
    c.rect(3, top + 6, 28, 27, front);
    c.rect(4, top + 7, 27, top + 7, WHITE);
}

fn draw_restore_explorer(c: &mut Canvas) {
    folder(c, ORANGE, YELLOW, 5);
    // Red cross shield in the corner = failsafe
    c.rect(17, 14, 29, 25, RED);
    c.spans(&[(26, 18, 28), (27, 19, 27), (28, 20, 26), (29, 22, 24)], RED);
    c.rect(22, 16, 24, 25, WHITE);
    c.rect(19, 19, 27, 21, WHITE);
}

fn draw_calculator(c: &mut Canvas) {
    c.rect(6, 2, 25, 29, MID);
    c.rect(7, 3, 24, 28, LIGHT);
    c.rect(9, 5, 22, 10, GREEN_DARK);
    c.rect(10, 6, 21, 9, GREEN);
    c.rect(17, 7, 20, 8, GREEN_DARK);
    for row in 0..4 {
        for col in 0..3 {
            let (x, y) = (9 + col * 5, 13 + row * 4);
            let color = match (row, col) {
                (_, 2) => ORANGE,
                (3, 0) => RED,
                _ => WHITE,
            };
            c.rect(x, y, x + 3, y + 2, color);
        }
    }
}

fn draw_shell(c: &mut Canvas) {
    c.rect(2, 4, 29, 27, DARK);
    c.rect(2, 4, 29, 7, MID);
    c.dot(4, 5, RED);
    c.dot(6, 5, YELLOW);
    c.dot(8, 5, GREEN);
    // ">"
    c.line(6, 11, 11, 15, 2, GREEN);
    c.line(11, 15, 6, 19, 2, GREEN);
    // "_"
    c.rect(14, 19, 21, 20, WHITE);
}

fn window(c: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, bar: u32) {
    c.rect(x0, y0, x1, y1, WHITE);
    c.rect(x0, y0, x1, y0 + 3, bar);
    c.dot(x1 - 2, y0 + 1, WHITE);
}

fn draw_window_switch(c: &mut Canvas) {
    window(c, 2, 3, 20, 18, BLUE_DARK);
    c.rect(4, 8, 18, 16, LIGHT);
    c.clear_rect(10, 11, 30, 29);
    window(c, 11, 12, 29, 28, BLUE);
    c.rect(13, 17, 27, 26, BLUE_LIGHT);
}

fn draw_files(c: &mut Canvas) {
    folder(c, ORANGE, YELLOW, 5);
    c.rect(4, 26, 27, 27, ORANGE);
}

fn draw_settings(c: &mut Canvas) {
    // Gear: body + eight teeth, hollow centre.
    c.disc(16.0, 16.0, 10.0, MID);
    for (x0, y0, x1, y1) in [(14, 2, 17, 6), (14, 25, 17, 29), (2, 14, 6, 17), (25, 14, 29, 17)] {
        c.rect(x0, y0, x1, y1, MID);
    }
    for (dx, dy) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
        let (cx, cy) = (16 + dx * 8 - (dx < 0) as i32, 16 + dy * 8 - (dy < 0) as i32);
        c.rect(cx - 2, cy - 2, cx + 1, cy + 1, MID);
    }
    c.disc(16.0, 16.0, 8.0, LIGHT);
    c.ring_where(16.0, 16.0, 8.0, 6.5, WHITE, |dx, dy| dx < 0.0 && dy < 0.0);
    c.clear_disc(16.0, 16.0, 4.0);
}

fn draw_browser(c: &mut Canvas) {
    c.disc(16.0, 16.0, 14.0, BLUE);
    // Continents
    c.disc(11.0, 11.0, 5.0, GREEN);
    c.disc(8.0, 15.0, 3.0, GREEN);
    c.disc(21.0, 21.0, 5.0, GREEN);
    c.disc(24.0, 9.0, 2.5, GREEN);
    // Meridian + equator grid
    c.ring_where(16.0, 16.0, 14.0, 0.0, BLUE_LIGHT, |dx, dy| {
        let e = (dx / 6.0).powi(2) + (dy / 14.0).powi(2);
        (0.82..=1.0).contains(&e) || dy.abs() < 0.6
    });
    c.ring_where(16.0, 16.0, 14.0, 12.5, BLUE_DARK, |dx, dy| dx > 0.0 || dy > 0.0);
}

fn draw_snip(c: &mut Canvas) {
    // Blades
    c.line(8, 3, 21, 20, 2, LIGHT);
    c.line(23, 3, 10, 20, 2, MID);
    c.line(9, 3, 21, 18, 1, WHITE);
    c.dot(15, 12, DARK);
    // Handles
    c.disc(9.0, 24.0, 5.5, RED);
    c.clear_disc(9.0, 24.0, 2.8);
    c.disc(22.5, 24.0, 5.5, RED);
    c.clear_disc(22.5, 24.0, 2.8);
    c.ring_where(9.0, 24.0, 5.5, 4.5, RED_DARK, |dx, dy| dx > 0.0 && dy > 0.0);
    c.ring_where(22.5, 24.0, 5.5, 4.5, RED_DARK, |dx, dy| dx > 0.0 && dy > 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_32x32_and_not_empty() {
        for icon in PixelIcon::ALL {
            let rgba = icon.rgba();
            assert_eq!(rgba.len(), SIZE * SIZE * 4, "{icon:?}");
            assert!(rgba.chunks(4).any(|p| p[3] != 0), "{icon:?} is blank");
        }
    }

    /// Writes all icons side by side at 8x over the drun background to a 24-bit BMP.
    #[test]
    #[ignore]
    fn export_pixel_icon_sheet() {
        const SCALE: usize = 8;
        const GAP: usize = 16;
        let cols = PixelIcon::ALL.len();
        let (w, h) = (cols * (SIZE * SCALE + GAP) + GAP, SIZE * SCALE + GAP * 2);
        let bg = [0xFD_u8, 0xF6, 0xE3];
        let mut img = vec![bg; w * h];
        for (i, icon) in PixelIcon::ALL.iter().enumerate() {
            let rgba = icon.rgba();
            let ox = GAP + i * (SIZE * SCALE + GAP);
            for y in 0..SIZE * SCALE {
                for x in 0..SIZE * SCALE {
                    let p = &rgba[((y / SCALE) * SIZE + x / SCALE) * 4..][..4];
                    if p[3] != 0 {
                        img[(GAP + y) * w + ox + x] = [p[0], p[1], p[2]];
                    }
                }
            }
        }
        let row_bytes = (w * 3).div_ceil(4) * 4;
        let data_len = row_bytes * h;
        let mut bmp = Vec::with_capacity(54 + data_len);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((54 + data_len) as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(w as i32).to_le_bytes());
        bmp.extend_from_slice(&(h as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0u8; 24]);
        for y in (0..h).rev() {
            let start = bmp.len();
            for x in 0..w {
                let [r, g, b] = img[y * w + x];
                bmp.extend_from_slice(&[b, g, r]);
            }
            bmp.resize(start + row_bytes, 0);
        }
        let path = std::env::temp_dir().join("pixel_icons_preview.bmp");
        std::fs::write(&path, bmp).unwrap();
        println!("wrote {}", path.display());
    }
}
