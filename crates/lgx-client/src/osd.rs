//! Software rasterizer for the in-window OSD card, styled after luma's
//! `OsdToast.qml`: 40 px tall, 8 px radius, translucent theme background,
//! a 20% text-colored border, 16 px centered text, top-center of the window.

use std::path::PathBuf;
use std::process::Command;

use fontdue::{Font, FontSettings};

const CARD_H: u32 = 40;
const SS: u32 = 1; // render at the system font size, no downscale blur
const PAD_X: u32 = 16;
const FONT_PX: f32 = 16.0;
const RADIUS: f32 = 8.0;

const FALLBACK_FONTS: &[&str] = &[
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

fn kdeglobals_family() -> Option<String> {
    let mut paths = Vec::new();
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(base).join("kdeglobals"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/kdeglobals"));
    }
    paths.push(PathBuf::from("/etc/xdg/kdeglobals"));
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut general = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                general = line == "[General]";
                continue;
            }
            if general {
                if let Some(value) = line.strip_prefix("font=") {
                    let family = value.split(',').next().unwrap_or_default().trim();
                    if !family.is_empty() {
                        return Some(family.to_string());
                    }
                }
            }
        }
    }
    None
}

fn fc_match(pattern: &str) -> Option<PathBuf> {
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn system_font_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(family) = kdeglobals_family() {
        if let Some(path) = fc_match(&family) {
            paths.push(path);
        }
    }
    if let Some(path) = fc_match("sans-serif") {
        paths.push(path);
    }
    paths.extend(FALLBACK_FONTS.iter().map(PathBuf::from));
    paths
}

fn load_font() -> Option<Font> {
    let mut seen = std::collections::HashSet::new();
    for path in system_font_paths() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
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
    x: f32,
    baseline: f32,
    font: &Font,
    px: f32,
    color: (u8, u8, u8),
    text: &str,
) {
    let mut x = x;
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

    let x = offset as f32 + (PAD_X * SS) as f32;
    let baseline = h as f32 / 2.0 + FONT_PX * SS as f32 * 0.35;
    draw_text(
        pixels,
        stride,
        x,
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_uses_a_system_font() {
        let font = load_font().expect("a system sans font");
        let on = "Background audio: On";
        let off = "Background audio: Off";
        assert!(text_width(&font, 32.0, on) > 0.0);
        assert!(text_width(&font, 32.0, off) > 0.0);
        let atlas = atlas().expect("OSD atlas");
        assert!(atlas.card_width >= 180);
        assert_eq!(atlas.rgba.len(), (atlas.width * atlas.height * 4) as usize);
    }
}
