//! Window icon embedded in the binary.
//!
//! The PNG is produced by `brand/scripts/generate_assets.py` from the obsidian
//! NEO-3 tile. `include_bytes!` keeps the taskbar icon working no matter what
//! the process's working directory is.

use egui::viewport::IconData;

/// Decode a PNG into the RGBA buffer [`egui::ViewportBuilder::with_icon`] wants.
pub fn decode_icon_png(bytes: &[u8]) -> Result<IconData, image::ImageError> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

/// The 256×256 obsidian badge compiled into the executable.
pub fn window_icon() -> IconData {
    decode_icon_png(include_bytes!("../assets/icons/floki-256.png"))
        .expect("embedded Floki window icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_exact(bytes: &[u8], rgba: [u8; 4]) -> usize {
        let (chunks, _rest) = bytes.as_chunks::<4>();
        chunks.iter().filter(|px| **px == rgba).count()
    }

    /// Brand planes on a hinted icon must not share an edge. A 1 px badge gap
    /// is what keeps orange, green, and blue distinct at taskbar size.
    fn assert_planes_separated(icon: &IconData) {
        let w = icon.width as i32;
        let h = icon.height as i32;
        let at = |x: i32, y: i32| -> [u8; 4] {
            let i = ((y as u32 * icon.width + x as u32) * 4) as usize;
            [
                icon.rgba[i],
                icon.rgba[i + 1],
                icon.rgba[i + 2],
                icon.rgba[i + 3],
            ]
        };
        let plane = |px: [u8; 4]| -> Option<u8> {
            match px {
                [0xFF, 0x55, 0x00, 255] => Some(0),
                [0x38, 0xFF, 0x4F, 255] => Some(1),
                [0x00, 0xAA, 0xFF, 255] => Some(2),
                _ => None,
            }
        };
        for y in 0..h {
            for x in 0..w {
                let Some(id) = plane(at(x, y)) else {
                    continue;
                };
                for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                    if nx >= w || ny >= h {
                        continue;
                    }
                    if let Some(other) = plane(at(nx, ny)) {
                        assert_eq!(id, other, "brand planes touch at ({x},{y}) and ({nx},{ny})");
                    }
                }
            }
        }
    }

    #[test]
    fn embedded_window_icon_decodes_as_a_square_obsidian_badge() {
        let icon = window_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(
            count_exact(&icon.rgba, [0x14, 0x15, 0x18, 255]) > 1000,
            "obsidian badge missing"
        );
        assert!(count_exact(&icon.rgba, [0xFF, 0x55, 0x00, 255]) > 100);
        assert!(count_exact(&icon.rgba, [0x38, 0xFF, 0x4F, 255]) > 50);
        assert!(count_exact(&icon.rgba, [0x00, 0xAA, 0xFF, 255]) > 20);
    }

    #[test]
    fn hinted_16px_and_24px_icons_keep_integer_planes() {
        let small = decode_icon_png(include_bytes!("../assets/icons/floki-16.png")).unwrap();
        assert_eq!((small.width, small.height), (16, 16));
        assert_eq!(count_exact(&small.rgba, [0xFF, 0x55, 0x00, 255]), 20);
        assert_eq!(count_exact(&small.rgba, [0x38, 0xFF, 0x4F, 255]), 14);
        assert_eq!(count_exact(&small.rgba, [0x00, 0xAA, 0xFF, 255]), 8);
        assert_eq!(&small.rgba[0..4], &[0, 0, 0, 0], "squircle corner");
        assert_planes_separated(&small);

        let medium = decode_icon_png(include_bytes!("../assets/icons/floki-24.png")).unwrap();
        assert_eq!((medium.width, medium.height), (24, 24));
        assert_eq!(count_exact(&medium.rgba, [0xFF, 0x55, 0x00, 255]), 36);
        assert_eq!(count_exact(&medium.rgba, [0x38, 0xFF, 0x4F, 255]), 27);
        assert_eq!(count_exact(&medium.rgba, [0x00, 0xAA, 0xFF, 255]), 15);
        assert_eq!(&medium.rgba[0..4], &[0, 0, 0, 0], "squircle corner");
        assert_planes_separated(&medium);
    }
}
