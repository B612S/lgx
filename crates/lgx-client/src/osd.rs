//! Software rasterizer for the in-window OSD card. The resulting RGBA atlas is
//! uploaded once and drawn by the Vulkan renderer as a transparent overlay.

use fontdue::{Font, FontSettings};

const CARD_W: u32 = 360;
const CARD_H: u32 = 84;
const SS: u32 = 2; // supersampling for crisp text and corners

const REGULAR_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];
const BOLD_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
];

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn load_font(paths: &[&str]) -> Option<Font> {
    for path in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
            return Some(font);
        }
    }
    None
}

fn text_width(font: &Font, px: f32, text: &str) -> f32 {
    text.chars()
        .map(|c| font.metrics(c, px).advance_width)
        .sum()
}

fn blend(pixels: &mut [u8], stride: u32, x: i32, y: i32, r: u8, g: u8, b: u8, a: f32) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    let idx = ((y * stride + x) * 4) as usize;
    if idx + 3 >= pixels.len() {
        return;
    }
    let a = a.clamp(0.0, 1.0);
    let dst_a = pixels[idx + 3] as f32 / 255.0;
    let out_a = a + dst_a * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for (i, src) in [r, g, b].into_iter().enumerate() {
        let dst = pixels[idx + i] as f32 / 255.0;
        let src = src as f32 / 255.0;
        let out = (src * a + dst * dst_a * (1.0 - a)) / out_a;
        pixels[idx + i] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    pixels[idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn draw_text(
    pixels: &mut [u8],
    stride: u32,
    center_x: f32,
    baseline: f32,
    font: &Font,
    px: f32,
    (r, g, b): (u8, u8, u8),
    text: &str,
) {
    let width = text_width(font, px, text);
    let mut x = center_x - width / 2.0;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px);
        let gx = (x + metrics.xmin as f32).round() as i32;
        let gy = (baseline - metrics.ymin as f32 - metrics.height as f32).round() as i32;
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let coverage = bitmap[row * metrics.width + col] as f32 / 255.0;
                if coverage > 0.0 {
                    blend(pixels, stride, gx + col as i32, gy + row as i32, r, g, b, coverage);
                }
            }
        }
        x += metrics.advance_width;
    }
}

fn rounded_distance(px: f32, py: f32, w: f32, h: f32, radius: f32) -> f32 {
    let qx = (px - w / 2.0).abs() - (w / 2.0 - radius);
    let qy = (py - h / 2.0).abs() - (h / 2.0 - radius);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - radius
}

fn draw_card(pixels: &mut [u8], stride: u32, offset: u32, enabled: bool, bold: &Font, regular: &Font) {
    let w = CARD_W * SS;
    let h = CARD_H * SS;
    let radius = 18.0 * SS as f32;
    let (bg_r, bg_g, bg_b) = (30u8, 30u8, 36u8);
    for y in 0..h {
        for x in 0..w {
            let d = rounded_distance(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
            let coverage = (-d).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend(
                    pixels,
                    stride,
                    (offset + x) as i32,
                    y as i32,
                    bg_r,
                    bg_g,
                    bg_b,
                    (232.0 / 255.0) * coverage,
                );
            }
            if d > -1.5 && d <= 0.0 {
                blend(
                    pixels,
                    stride,
                    (offset + x) as i32,
                    y as i32,
                    255,
                    255,
                    255,
                    (34.0 / 255.0) * (-d).clamp(0.0, 1.0),
                );
            }
        }
    }

    let center = offset as f32 + w as f32 / 2.0;
    draw_text(
        pixels,
        stride,
        center,
        34.0 * SS as f32,
        bold,
        20.0 * SS as f32,
        (247, 247, 250),
        "Background audio",
    );
    if enabled {
        draw_text(
            pixels,
            stride,
            center,
            61.0 * SS as f32,
            regular,
            15.0 * SS as f32,
            (126, 229, 158),
            "On",
        );
    } else {
        draw_text(
            pixels,
            stride,
            center,
            61.0 * SS as f32,
            regular,
            14.0 * SS as f32,
            (238, 179, 104),
            "Off — muted when unfocused",
        );
    }
}

pub fn atlas() -> Option<Atlas> {
    let regular = load_font(REGULAR_FONTS)?;
    let bold = load_font(BOLD_FONTS).unwrap_or_else(|| {
        // Reuse the regular face if no bold file is installed.
        load_font(REGULAR_FONTS).expect("regular font")
    });
    let w = CARD_W * SS;
    let h = CARD_H * SS;
    let width = w * 2;
    let mut rgba = vec![0u8; (width * h * 4) as usize];
    draw_card(&mut rgba, width, 0, true, &bold, &regular);
    draw_card(&mut rgba, width, w, false, &bold, &regular);
    Some(Atlas { width, height: h, rgba })
}
