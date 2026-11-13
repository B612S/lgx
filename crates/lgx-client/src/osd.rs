//! Software rasterizer for the in-window OSD card, styled after luma's
//! `OsdToast.qml`: 40 px tall, 8 px radius, translucent theme background,
//! a 20% text-colored border, 16 px centered text, top-center of the window.

use fontdue::{Font, FontSettings};

const CARD_H: u32 = 40;
const SS: u32 = 2; // supersampling
const PAD_X: u32 = 16;
const FONT_PX: f32 = 16.0;
const RADIUS: f32 = 8.0;

const FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

pub struct Atlas {
    /// Texture width (both states side by side, supersampled).
    pub width: u32,
    pub height: u32,
    /// Logical size of one card.
    pub card_width: u32,
    pub card_height: u32,
    pub rgba: Vec<u8>,
}

fn load_font() -> Option<Font> {
    for path in FONTS {
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
    color: (u8, u8, u8),
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
                    blend(
                        pixels,
                        stride,
                        gx + col as i32,
                        gy + row as i32,
                        color.0,
                        color.1,
                        color.2,
                        coverage,
                    );
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

fn draw_card(
    pixels: &mut [u8],
    stride: u32,
    offset: u32,
    card_w: u32,
    text: &str,
    font: &Font,
) {
    let h = CARD_H * SS;
    let radius = RADIUS * SS as f32;
    let (bg_r, bg_g, bg_b) = (28u8, 28u8, 33u8); // luma dark background, 85%
    for y in 0..h {
        for x in 0..card_w {
            let d = rounded_distance(x as f32 + 0.5, y as f32 + 0.5, card_w as f32, h as f32, radius);
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
                    (217.0 / 255.0) * coverage,
                );
            }
            if d > -1.5 && d <= 0.0 {
                blend(
                    pixels,
                    stride,
                    (offset + x) as i32,
                    y as i32,
                    250,
                    250,
                    252,
                    (51.0 / 255.0) * (-d).clamp(0.0, 1.0),
                );
            }
        }
    }

    let center = offset as f32 + card_w as f32 / 2.0;
    let baseline = h as f32 / 2.0 + FONT_PX * SS as f32 * 0.35;
    draw_text(
        pixels,
        stride,
        center,
        baseline,
        font,
        FONT_PX * SS as f32,
        (250, 250, 252),
        text,
    );
}

pub fn atlas() -> Option<Atlas> {
    let font = load_font()?;
    let on = "Background audio: On";
    let off = "Background audio: Off";
    let text_w = text_width(&font, FONT_PX * SS as f32, on)
        .max(text_width(&font, FONT_PX * SS as f32, off));
    let card_w = (text_w.ceil() as u32 + PAD_X * 2 * SS).max(180 * SS);
    let card_h = CARD_H * SS;
    let width = card_w * 2;
    let mut rgba = vec![0u8; (width * card_h * 4) as usize];
    draw_card(&mut rgba, width, 0, card_w, on, &font);
    draw_card(&mut rgba, width, card_w, card_w, off, &font);
    Some(Atlas {
        width,
        height: card_h,
        card_width: card_w / SS,
        card_height: CARD_H,
        rgba,
    })
}
