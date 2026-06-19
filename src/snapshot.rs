//! Snapshot-to-clipboard support (issue #19).
//!
//! The displayed frame is captured via egui's framebuffer screenshot
//! (`ViewportCommand::Screenshot` → `Event::Screenshot`, driven from
//! [`crate::app`]); this module holds the pure, side-effect-light helpers that
//! turn the resulting [`egui::ColorImage`] into a clipboard image or a PNG on
//! disk. Capturing the framebuffer means the snapshot already includes the
//! background, every compare mode, OCIO, and the annotation overlay (#45) — no
//! per-pixel re-rendering here.

use eframe::egui::ColorImage;
use std::path::PathBuf;

/// Crop `img` (physical-pixel framebuffer) to the on-screen `rect` (egui points),
/// scaling by `pixels_per_point` and clamping to the image bounds. Returns the
/// whole image if the resulting region is empty (defensive — the canvas rect is
/// always inside the window in practice).
pub fn crop_to_rect(
    img: &ColorImage,
    rect: eframe::egui::Rect,
    pixels_per_point: f32,
) -> ColorImage {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let x0 = ((rect.min.x * pixels_per_point).floor() as i64).clamp(0, iw);
    let y0 = ((rect.min.y * pixels_per_point).floor() as i64).clamp(0, ih);
    let x1 = ((rect.max.x * pixels_per_point).ceil() as i64).clamp(0, iw);
    let y1 = ((rect.max.y * pixels_per_point).ceil() as i64).clamp(0, ih);
    let w = (x1 - x0).max(0) as usize;
    let h = (y1 - y0).max(0) as usize;
    if w == 0 || h == 0 {
        return img.clone();
    }
    img.region_by_pixels([x0 as usize, y0 as usize], [w, h])
}

/// Pack a [`ColorImage`] into a tightly-packed RGBA8 byte buffer (`w*h*4`). The
/// captured framebuffer is opaque over the canvas, so premultiplied == straight
/// alpha here.
pub fn to_rgba_bytes(img: &ColorImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.width() * img.height() * 4);
    for px in &img.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
    }
    out
}

/// Copy the image to the system clipboard. A fresh `arboard::Clipboard` is created
/// per call — arboard discourages holding a context long-term.
pub fn copy_to_clipboard(img: &ColorImage) -> Result<(), String> {
    let data = arboard::ImageData {
        width: img.width(),
        height: img.height(),
        bytes: to_rgba_bytes(img).into(),
    };
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_image(data)
        .map_err(|e| e.to_string())
}

/// `~/.floki/snapshots`. `Err` if the home directory can't be resolved.
pub fn snapshots_dir() -> Result<PathBuf, String> {
    home::home_dir()
        .map(|h| h.join(".floki").join("snapshots"))
        .ok_or_else(|| "could not determine home directory".to_string())
}

/// Encode `img` to a PNG under [`snapshots_dir`] named `snapshot-<unix_secs>.png`,
/// creating the directory if needed. `unix_secs` is passed in (not read from the
/// clock) so callers stay in control and this stays unit-testable.
pub fn save_png(img: &ColorImage, unix_secs: u64) -> Result<PathBuf, String> {
    let dir = snapshots_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("snapshot-{unix_secs}.png"));
    let buf =
        image::RgbaImage::from_raw(img.width() as u32, img.height() as u32, to_rgba_bytes(img))
            .ok_or_else(|| "snapshot buffer size mismatch".to_string())?;
    buf.save(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Color32, ColorImage};

    fn solid(w: usize, h: usize, c: Color32) -> ColorImage {
        ColorImage {
            size: [w, h],
            source_size: eframe::egui::vec2(w as f32, h as f32),
            pixels: vec![c; w * h],
        }
    }

    #[test]
    fn rgba_bytes_are_tightly_packed() {
        let img = solid(2, 3, Color32::from_rgba_premultiplied(10, 20, 30, 255));
        let bytes = to_rgba_bytes(&img);
        assert_eq!(bytes.len(), 2 * 3 * 4);
        assert_eq!(&bytes[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn crop_extracts_subregion() {
        // 4x4 image; crop the bottom-right 2x2 at ppp=1.
        let img = solid(4, 4, Color32::WHITE);
        let rect = eframe::egui::Rect::from_min_max(
            eframe::egui::pos2(2.0, 2.0),
            eframe::egui::pos2(4.0, 4.0),
        );
        let out = crop_to_rect(&img, rect, 1.0);
        assert_eq!(out.size, [2, 2]);
    }

    #[test]
    fn crop_clamps_out_of_bounds_rect() {
        let img = solid(4, 4, Color32::WHITE);
        // Rect extends past the image; result is clamped, not a panic.
        let rect = eframe::egui::Rect::from_min_max(
            eframe::egui::pos2(2.0, 2.0),
            eframe::egui::pos2(100.0, 100.0),
        );
        let out = crop_to_rect(&img, rect, 1.0);
        assert_eq!(out.size, [2, 2]);
    }

    #[test]
    fn crop_respects_pixels_per_point() {
        // 8x8 physical image, ppp=2 → a 0..2pt rect maps to 0..4px.
        let img = solid(8, 8, Color32::WHITE);
        let rect = eframe::egui::Rect::from_min_max(
            eframe::egui::pos2(0.0, 0.0),
            eframe::egui::pos2(2.0, 2.0),
        );
        let out = crop_to_rect(&img, rect, 2.0);
        assert_eq!(out.size, [4, 4]);
    }

    #[test]
    fn snapshots_dir_ends_with_floki_snapshots() {
        if let Ok(dir) = snapshots_dir() {
            assert!(dir.ends_with("snapshots"));
            assert!(dir.to_string_lossy().contains(".floki"));
        }
    }
}
