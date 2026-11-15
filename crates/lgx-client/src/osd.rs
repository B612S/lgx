//! Software rasterizer for the in-window OSD card, styled after luma's
//! `OsdToast.qml`: 40 px tall, 8 px radius, translucent theme background,
//! a 20% text-colored border, 16 px centered text, top-center of the window.
//!
//! Text is rendered by FreeType with the system fontconfig hinting style
//! (hintslight), which is much closer to Qt/KDE than a pure-Rust rasterizer.

use std::path::PathBuf;
use std::process::Command;

use freetype::face::LoadFlag;
use freetype::{Face, Library};

const CARD_H: u32 = 40;
const PAD_X: u32 = 16;
const DEFAULT_POINT_SIZE: f32 = 10.0;
const RADIUS: f32 = 8.0;

const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

pub const AUDIO_ON: usize = 0;
pub const AUDIO_OFF: usize = 1;
pub const POINTER_CAPTURED: usize = 2;
pub const POINTER_RELEASED: usize = 3;
pub const FULLSCREEN_ON: usize = 4;
pub const FULLSCREEN_OFF: usize = 5;
pub const CARDS: [&str; 6] = [
    "Background audio: On",
    "Background audio: Off",
    "Pointer captured",
    "Pointer released",
    "Fullscreen: On",
    "Fullscreen: Off",
];

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// (offset, width) of each card in the atlas.
    pub cards: Vec<(u32, u32)>,
    pub rgba: Vec<u8>,
}

struct SystemFont {
    _library: Library,
    face: Face,
    pixels: f32,
    hint_flags: LoadFlag,
}

fn kdeglobals_font() -> Option<(String, f32)> {
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
                    let mut parts = value.split(',');
                    let family = parts.next().unwrap_or_default().trim();
                    if family.is_empty() {
                        continue;
                    }
                    let point = parts
                        .next()
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(DEFAULT_POINT_SIZE);
                    return Some((family.to_string(), point));
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

fn fc_attr(pattern: &str, format: &str) -> Option<String> {
    let output = Command::new("fc-match")
        .args(["-f", format, pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok().map(|s| s.trim().to_string())
}

fn xft_dpi() -> Option<f32> {
    let output = Command::new("xrdb").args(["-query"]).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("Xft.dpi:") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn system_font(scale: f32) -> Option<(SystemFont, String, f32)> {
    let (family, point) = kdeglobals_font()
        .unwrap_or_else(|| ("sans-serif".to_string(), DEFAULT_POINT_SIZE));
    let dpi = xft_dpi().unwrap_or(96.0 * scale.max(0.5));
    let hint_style = fc_attr(&family, "%{hintstyle}")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(1);
    let hint_flags = match hint_style {
        0 => LoadFlag::NO_HINTING,
        1 => LoadFlag::TARGET_LIGHT,
        _ => LoadFlag::TARGET_NORMAL,
    };
    let mut paths = Vec::new();
    if let Some(path) = fc_match(&family) {
        paths.push(path);
    }
    if let Some(path) = fc_match("sans-serif") {
        paths.push(path);
    }
    paths.extend(FALLBACK_FONTS.iter().map(PathBuf::from));

    let library = Library::init().ok()?;
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(face) = library.new_face(&path, 0) else {
            continue;
        };
        let char_height = (point * 64.0).round() as isize;
        if face
            .set_char_size(0, char_height, dpi.round() as u32, dpi.round() as u32)
            .is_err()
        {
            continue;
        }
        let pixels = face
            .size_metrics()
            .map(|m| m.x_ppem as f32)
            .filter(|p| *p > 0.0)
            .unwrap_or(point * dpi / 72.0);
        return Some((
            SystemFont { _library: library, face, pixels, hint_flags },
            family,
            point,
        ));
    }
    None
}

fn glyph(font: &SystemFont, ch: char, render: bool) -> (i32, i32, i32, Vec<u8>) {
    let mut flags = font.hint_flags;
    if render {
        flags |= LoadFlag::RENDER;
    }
    if font.face.load_char(ch as usize, flags).is_err() {
        return (0, 0, 0, Vec::new());
    }
    let slot = font.face.glyph();
    let advance = (slot.advance().x >> 6) as i32;
    let left = slot.bitmap_left();
    let top = slot.bitmap_top();
    (advance, left, top, if render { slot.bitmap().buffer().to_vec() } else { Vec::new() })
}

fn text_width(font: &SystemFont, text: &str) -> f32 {
    text.chars().map(|ch| glyph(font, ch, false).0 as f32).sum()
}

#[allow(clippy::too_many_arguments)]
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
    font: &SystemFont,
    text: &str,
) {
    let mut pen = x;
    for ch in text.chars() {
        let (advance, left, top, bitmap) = glyph(font, ch, true);
        if !bitmap.is_empty() {
            let slot = font.face.glyph();
            let bmp = slot.bitmap();
            let pitch = bmp.pitch();
            let width = bmp.width() as usize;
            let rows = bmp.rows() as usize;
            for row in 0..rows {
                let start = if pitch >= 0 {
                    row * pitch as usize
                } else {
                    (rows - 1 - row) * (-pitch) as usize
                };
                for col in 0..width {
                    let coverage = bitmap[start + col] as f32 / 255.0;
                    if coverage > 0.0 {
                        blend(
                            pixels,
                            stride,
                            pen as i32 + left + col as i32,
                            baseline as i32 - top + row as i32,
                            250,
                            250,
                            252,
                            coverage,
                        );
                    }
                }
            }
        }
        pen += advance as f32;
    }
}

fn rounded_distance(px: f32, py: f32, w: f32, h: f32, radius: f32) -> f32 {
    let qx = (px - w / 2.0).abs() - (w / 2.0 - radius);
    let qy = (py - h / 2.0).abs() - (h / 2.0 - radius);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - radius
}

fn draw_card(pixels: &mut [u8], stride: u32, offset: u32, card_w: u32, text: &str, font: &SystemFont) {
    let h = CARD_H;
    let radius = RADIUS;
    let (bg_r, bg_g, bg_b) = (28u8, 28u8, 33u8);
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

    let x = offset as f32 + PAD_X as f32;
    let baseline = h as f32 / 2.0 + font.pixels * 0.35;
    draw_text(pixels, stride, x, baseline, font, text);
}

fn card_width(font: &SystemFont, text: &str) -> u32 {
    text_width(font, text).ceil() as u32 + PAD_X * 2
}

pub fn atlas(scale: f32) -> Option<Atlas> {
    let (font, family, point) = system_font(scale)?;
    eprintln!("lgx-client: OSD font {family}, {point} pt ({} px)", font.pixels);
    let widths: Vec<u32> = CARDS.iter().map(|t| card_width(&font, t)).collect();
    let card_h = CARD_H;
    let width: u32 = widths.iter().sum();
    let mut rgba = vec![0u8; (width * card_h * 4) as usize];
    let mut cards = Vec::with_capacity(CARDS.len());
    let mut offset = 0;
    for (text, w) in CARDS.iter().zip(widths.iter()) {
        draw_card(&mut rgba, width, offset, *w, text, &font);
        cards.push((offset, *w));
        offset += *w;
    }
    Some(Atlas {
        width,
        height: card_h,
        cards,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_uses_a_system_font() {
        let atlas = atlas(1.0).expect("OSD atlas");
        assert_eq!(atlas.cards.len(), CARDS.len());
        assert_eq!(atlas.width, atlas.cards.iter().map(|c| c.1).sum());
        assert_eq!(atlas.rgba.len(), (atlas.width * atlas.height * 4) as usize);
        assert!(atlas.cards.iter().all(|c| c.1 > 0));
    }
}
