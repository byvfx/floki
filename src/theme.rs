//! Studio chrome and the locked brand constants.
//!
//! Chrome C is the shipping egui theme: an obsidian chassis and a silver
//! focus, on both the Dark and Light choices. The viewport checker and the
//! functional timeline / readout colors are not part of that chrome. They are
//! declared here so a theme edit cannot quietly repaint them.
//!
//! The About mark is the same three paths as `brand/svg/floki-mark-dark.svg`.

use egui::Color32;

/// Locked product line. Also `CARGO_PKG_DESCRIPTION`.
pub const TAGLINE: &str =
    "Inspect, compare, and play multi-layer image sequences with color-accurate OCIO viewing.";

pub const LICENSE_NAME: &str = "MIT";
pub const URL_REPOSITORY: &str = "https://github.com/byvfx/floki";
pub const URL_RELEASES: &str = "https://github.com/byvfx/floki/releases";
pub const URL_DOCS: &str = "https://byvfx.github.io/floki/";

pub const CANVAS: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
pub const PANEL: Color32 = Color32::from_rgb(0x1C, 0x1D, 0x22);
pub const ELEVATED: Color32 = Color32::from_rgb(0x26, 0x28, 0x30);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2E, 0x31, 0x3B);
pub const BONE: Color32 = Color32::from_rgb(0xD8, 0xDC, 0xE4);
pub const PAPER: Color32 = Color32::from_rgb(0xF4, 0xF5, 0xF7);
pub const PAPER_WHITE: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
pub const INK: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
/// Secondary copy on paper (`#4A4E59`). Not a neon ink.
pub const INK_MUTED: Color32 = Color32::from_rgb(0x4A, 0x4E, 0x59);
/// Muted copy on the obsidian chassis (`#8C919E`).
pub const MUTED: Color32 = Color32::from_rgb(0x8C, 0x91, 0x9E);
pub const LINK_BLUE: Color32 = Color32::from_rgb(0x00, 0x70, 0xB8);

/// Dark-mode Parallax planes. Top, middle, bottom.
pub const MARK_ORANGE: Color32 = Color32::from_rgb(0xFF, 0x55, 0x00);
pub const MARK_GREEN_DARK: Color32 = Color32::from_rgb(0x38, 0xFF, 0x4F);
pub const MARK_BLUE_DARK: Color32 = Color32::from_rgb(0x00, 0xAA, 0xFF);
/// Light-mode planes. Orange is unchanged; green and blue drop to the 3.1:1 inks.
pub const MARK_GREEN_LIGHT: Color32 = Color32::from_rgb(0x00, 0xAA, 0x14);
pub const MARK_BLUE_LIGHT: Color32 = Color32::from_rgb(0x00, 0x99, 0xE6);

/// Timeline cache strip. Proxy / beauty resident, then full-res on top.
pub const CACHE_PROXY_FILL: Color32 = Color32::from_rgb(0x34, 0x68, 0x48);
pub const CACHE_FULL_FILL: Color32 = Color32::from_rgb(0x40, 0xA8, 0x60);
/// Missing-frame tick on the ruler.
pub const MISSING_FRAME: Color32 = Color32::from_rgb(0xCE, 0x5C, 0x3C);
/// Decode-bound hint and the data-window overlay. Functional amber, not a brand plane.
pub const DECODE_WARN_AMBER: Color32 = Color32::from_rgb(0xFF, 0xC8, 0x64);
/// Pixel-readout channels. These are instrument colors, not the mark.
pub const READOUT_R: Color32 = Color32::from_rgb(0xFF, 0x50, 0x50);
pub const READOUT_G: Color32 = Color32::from_rgb(0x50, 0xFF, 0x50);
pub const READOUT_B: Color32 = Color32::from_rgb(0x64, 0x96, 0xFF);

/// Plane paths, identical in the dark and light mark SVGs.
const PATH_TOP: &str = "M 224.9 384.9 Q 220.0 398.0 234.0 398.0 L 766.0 398.0 Q 780.0 398.0 784.9 384.9 L 815.1 303.1 Q 820.0 290.0 806.0 290.0 L 274.0 290.0 Q 260.0 290.0 255.1 303.1 Z";
const PATH_MIDDLE: &str = "M 234.9 552.9 Q 230.0 566.0 244.0 566.0 L 616.0 566.0 Q 630.0 566.0 634.9 552.9 L 665.1 471.1 Q 670.0 458.0 656.0 458.0 L 284.0 458.0 Q 270.0 458.0 265.1 471.1 Z";
const PATH_BOTTOM: &str = "M 244.9 720.9 Q 240.0 734.0 254.0 734.0 L 436.0 734.0 Q 450.0 734.0 454.9 720.9 L 485.1 639.1 Q 490.0 626.0 476.0 626.0 L 294.0 626.0 Q 280.0 626.0 275.1 639.1 Z";

pub fn about_version() -> String {
    format!("Version {}", env!("CARGO_PKG_VERSION"))
}

/// Secondary ink for the About dialog: muted bone on the chassis, `#4A4E59` on paper.
pub fn secondary_ink(dark_mode: bool) -> Color32 {
    if dark_mode { MUTED } else { INK_MUTED }
}

/// Obsidian (dark) or paper (light) visuals. Selection and focus stay achromatic.
pub fn chrome_visuals(theme: egui::Theme) -> egui::Visuals {
    let dark = matches!(theme, egui::Theme::Dark);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let (text, panel, canvas, elevated, hairline, focus, link) = if dark {
        (BONE, PANEL, CANVAS, ELEVATED, HAIRLINE, BONE, PAPER)
    } else {
        (INK, PAPER_WHITE, PAPER, PAPER, BONE, INK, LINK_BLUE)
    };

    visuals.hyperlink_color = link;
    visuals.faint_bg_color = elevated;
    visuals.extreme_bg_color = canvas;
    visuals.code_bg_color = if dark { ELEVATED } else { PAPER };
    visuals.window_fill = panel;
    visuals.window_stroke = egui::Stroke::new(1.0_f32, hairline);
    visuals.panel_fill = panel;
    visuals.selection.bg_fill = if dark { HAIRLINE } else { BONE };
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, focus);

    visuals.widgets.noninteractive.fg_stroke.color = text;
    visuals.widgets.inactive.fg_stroke.color = text;
    visuals.widgets.hovered.fg_stroke.color = text;
    visuals.widgets.active.fg_stroke.color = text;
    visuals.widgets.open.fg_stroke.color = text;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    visuals.widgets.inactive.bg_fill = elevated;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    visuals.widgets.hovered.bg_fill = if dark { HAIRLINE } else { PAPER };
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, focus);
    visuals.widgets.active.bg_fill = if dark { HAIRLINE } else { BONE };
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, focus);
    visuals.widgets.open.bg_fill = if dark { HAIRLINE } else { PAPER };
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, focus);

    visuals
}

/// Install Chrome C on both egui themes. Safe to call every frame.
pub fn install_studio_chrome(ctx: &egui::Context) {
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals = chrome_visuals(egui::Theme::Dark);
    });
    ctx.style_mut_of(egui::Theme::Light, |style| {
        style.visuals = chrome_visuals(egui::Theme::Light);
    });
}

/// Draw the Parallax mark into `rect`. Dark mode uses the chassis inks; light
/// mode uses the 3.1:1 inks. The paths are sampled from the canonical SVG.
pub fn paint_parallax_mark(painter: &egui::Painter, rect: egui::Rect, dark_mode: bool) {
    let (top, middle, bottom) = if dark_mode {
        (MARK_ORANGE, MARK_GREEN_DARK, MARK_BLUE_DARK)
    } else {
        (MARK_ORANGE, MARK_GREEN_LIGHT, MARK_BLUE_LIGHT)
    };
    paint_plane(painter, rect, PATH_TOP, top);
    paint_plane(painter, rect, PATH_MIDDLE, middle);
    paint_plane(painter, rect, PATH_BOTTOM, bottom);
}

fn paint_plane(painter: &egui::Painter, rect: egui::Rect, path: &str, color: Color32) {
    let points = sample_path(path)
        .into_iter()
        .map(|(x, y)| {
            egui::pos2(
                rect.left() + x / 1024.0 * rect.width(),
                rect.top() + y / 1024.0 * rect.height(),
            )
        })
        .collect();
    painter.add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
}

fn sample_path(path: &str) -> Vec<(f32, f32)> {
    let mut tokens = path.split_whitespace();
    let mut points = Vec::new();
    let mut cx = 0.0;
    let mut cy = 0.0;
    while let Some(op) = tokens.next() {
        match op {
            "M" | "L" => {
                cx = tokens
                    .next()
                    .expect("path x")
                    .parse()
                    .expect("path x number");
                cy = tokens
                    .next()
                    .expect("path y")
                    .parse()
                    .expect("path y number");
                points.push((cx, cy));
            }
            "Q" => {
                let x1: f32 = tokens.next().expect("qx").parse().expect("qx number");
                let y1: f32 = tokens.next().expect("qy").parse().expect("qy number");
                let x2: f32 = tokens.next().expect("x").parse().expect("x number");
                let y2: f32 = tokens.next().expect("y").parse().expect("y number");
                const STEPS: usize = 8;
                for step in 1..=STEPS {
                    let t = step as f32 / STEPS as f32;
                    let u = 1.0 - t;
                    points.push((
                        u * u * cx + 2.0 * u * t * x1 + t * t * x2,
                        u * u * cy + 2.0 * u * t * y1 + t * t * y2,
                    ));
                }
                cx = x2;
                cy = y2;
            }
            "Z" => {}
            other => panic!("unexpected mark path token {other}"),
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chroma(color: Color32) -> u8 {
        let max = color.r().max(color.g()).max(color.b());
        let min = color.r().min(color.g()).min(color.b());
        max - min
    }

    fn hex(color: Color32) -> String {
        format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
    }

    #[test]
    fn about_copy_matches_package_metadata() {
        assert_eq!(env!("CARGO_PKG_DESCRIPTION"), TAGLINE);
        assert_eq!(env!("CARGO_PKG_HOMEPAGE"), URL_DOCS);
        assert_eq!(env!("CARGO_PKG_REPOSITORY"), URL_REPOSITORY);
        assert_eq!(env!("CARGO_PKG_LICENSE"), LICENSE_NAME);
        assert!(about_version().starts_with("Version "));
        assert!(about_version().contains(env!("CARGO_PKG_VERSION")));
        assert!(!TAGLINE.contains(" we "));
        assert!(URL_DOCS.starts_with("https://byvfx.github.io/floki"));
        assert!(URL_RELEASES.contains("/releases"));
    }

    #[test]
    fn mark_paths_match_the_canonical_svgs() {
        let dark = include_str!("../brand/svg/floki-mark-dark.svg");
        let light = include_str!("../brand/svg/floki-mark-light.svg");
        for svg in [dark, light] {
            assert!(svg.contains(PATH_TOP));
            assert!(svg.contains(PATH_MIDDLE));
            assert!(svg.contains(PATH_BOTTOM));
        }
        assert!(dark.contains(&hex(MARK_ORANGE)));
        assert!(dark.contains(&hex(MARK_GREEN_DARK)));
        assert!(dark.contains(&hex(MARK_BLUE_DARK)));
        assert!(light.contains(&hex(MARK_GREEN_LIGHT)));
        assert!(light.contains(&hex(MARK_BLUE_LIGHT)));
        let top = sample_path(PATH_TOP);
        assert_eq!(top[0], (224.9, 384.9));
        assert!(top.len() > 8);
        for (x, y) in top {
            assert!((0.0..1024.0).contains(&x) && (0.0..1024.0).contains(&y));
        }
    }

    #[test]
    fn token_file_carries_the_same_hexes() {
        let tokens = include_str!("../brand/tokens.json");
        for color in [
            CANVAS,
            PANEL,
            ELEVATED,
            HAIRLINE,
            BONE,
            PAPER,
            INK_MUTED,
            LINK_BLUE,
            MUTED,
            MARK_ORANGE,
            MARK_GREEN_DARK,
            MARK_BLUE_DARK,
            MARK_GREEN_LIGHT,
            MARK_BLUE_LIGHT,
            CACHE_PROXY_FILL,
            CACHE_FULL_FILL,
            MISSING_FRAME,
            DECODE_WARN_AMBER,
            READOUT_R,
            READOUT_G,
            READOUT_B,
        ] {
            assert!(
                tokens.contains(&hex(color)),
                "{} missing from tokens.json",
                hex(color)
            );
        }
        assert!(tokens.contains(TAGLINE));
        assert!(tokens.contains("Orange 021 C"));
        assert!(tokens.contains("802 C"));
        assert!(tokens.contains("299 C"));
        assert!(tokens.contains("physical Pantone Formula Guide"));
    }

    #[test]
    fn chrome_c_dark_is_an_achromatic_obsidian_chassis() {
        let visuals = chrome_visuals(egui::Theme::Dark);
        let stock = egui::Visuals::dark();
        assert_eq!(visuals.extreme_bg_color, CANVAS);
        assert_eq!(visuals.panel_fill, PANEL);
        assert_eq!(visuals.window_fill, PANEL);
        assert_eq!(visuals.selection.stroke.color, BONE);
        assert_eq!(visuals.widgets.active.bg_stroke.color, BONE);
        assert_eq!(visuals.hyperlink_color, PAPER);
        assert_eq!(secondary_ink(true), MUTED);
        assert_eq!(visuals.warn_fg_color, stock.warn_fg_color);
        assert_eq!(visuals.error_fg_color, stock.error_fg_color);
        for color in [
            visuals.panel_fill,
            visuals.window_fill,
            visuals.extreme_bg_color,
            visuals.faint_bg_color,
            visuals.selection.bg_fill,
            visuals.selection.stroke.color,
            visuals.hyperlink_color,
            visuals.widgets.hovered.bg_stroke.color,
            visuals.widgets.active.bg_fill,
        ] {
            assert!(chroma(color) <= 16, "{color:?} carries a hue");
            assert_ne!(color, MARK_ORANGE);
            assert_ne!(color, MARK_GREEN_DARK);
            assert_ne!(color, MARK_BLUE_DARK);
            assert_ne!(color, CACHE_FULL_FILL);
            assert_ne!(color, MISSING_FRAME);
            assert_ne!(color, DECODE_WARN_AMBER);
        }
    }

    #[test]
    fn chrome_c_light_keeps_paper_and_a_neutral_focus() {
        let visuals = chrome_visuals(egui::Theme::Light);
        assert_eq!(visuals.panel_fill, PAPER_WHITE);
        assert_eq!(visuals.extreme_bg_color, PAPER);
        assert_eq!(visuals.widgets.noninteractive.fg_stroke.color, INK);
        assert_eq!(visuals.selection.stroke.color, INK);
        assert_eq!(visuals.hyperlink_color, LINK_BLUE);
        assert_eq!(secondary_ink(false), INK_MUTED);
        assert_ne!(visuals.hyperlink_color, MARK_BLUE_LIGHT);
        assert_ne!(visuals.hyperlink_color, MARK_BLUE_DARK);
        assert!(chroma(visuals.panel_fill) <= 16);
        assert!(chroma(visuals.selection.bg_fill) <= 16);
    }

    #[test]
    fn functional_colors_and_the_viewport_stay_put() {
        assert_eq!(CACHE_PROXY_FILL, Color32::from_rgb(52, 104, 72));
        assert_eq!(CACHE_FULL_FILL, Color32::from_rgb(64, 168, 96));
        assert_eq!(MISSING_FRAME, Color32::from_rgb(206, 92, 60));
        assert_eq!(DECODE_WARN_AMBER, Color32::from_rgb(255, 200, 100));
        assert_eq!(READOUT_R, Color32::from_rgb(255, 80, 80));
        assert_eq!(READOUT_G, Color32::from_rgb(80, 255, 80));
        assert_eq!(READOUT_B, Color32::from_rgb(100, 150, 255));
        assert_ne!(MARK_GREEN_DARK, CACHE_FULL_FILL);
        assert_ne!(MARK_GREEN_DARK, CACHE_PROXY_FILL);
        assert_ne!(MARK_ORANGE, DECODE_WARN_AMBER);
        assert_ne!(MARK_BLUE_DARK, READOUT_B);

        let backdrop = crate::background::Background::default();
        assert_eq!(backdrop.checker_dark, [0.1, 0.1, 0.1]);
        assert_eq!(backdrop.checker_light, [0.2, 0.2, 0.2]);
        assert_eq!(backdrop.checker_size, 16.0);
        for stop in crate::background::default_gradient().stops {
            let [r, g, b] = stop.color;
            assert_eq!(r, g);
            assert_eq!(g, b);
        }
    }

    fn qss_code(sheet: &str) -> String {
        let mut code = String::new();
        let mut chars = sheet.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                while let Some(d) = chars.next() {
                    if d == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            } else {
                code.push(c);
            }
        }
        code
    }

    #[test]
    fn qt_direction_c_sheets_match_the_token_block() {
        let tokens = include_str!("../brand/tokens.json");
        for needle in [
            "\"qt\"",
            "floki-qt.qss",
            "floki-qt-light.qss",
            "floki-qt-light-studio.qss",
            "lightNeon",
            "lightStudio",
            "timelineTracks",
            "surfaces",
            "#101114",
            "#22242B",
            "#2B2D33",
            "#FF6A1A",
            "#E64800",
            "#FF2D87",
            "#00B8FF",
            "#00E65A",
            "#FF6B00",
            "#D92BFF",
        ] {
            assert!(tokens.contains(needle), "{needle} missing from tokens.json");
        }

        let dark = include_str!("../brand/floki-qt.qss");
        let neon = include_str!("../brand/floki-qt-light.qss");
        let studio = include_str!("../brand/floki-qt-light-studio.qss");
        for (name, sheet) in [("dark", dark), ("neon", neon), ("studio", studio)] {
            let code = qss_code(sheet);
            assert_eq!(
                code.matches('{').count(),
                code.matches('}').count(),
                "{name} braces do not balance"
            );
            for banned in [
                "box-shadow",
                "backdrop-filter",
                "var(",
                "calc(",
                "@font-face",
                "outline:",
            ] {
                assert!(!code.contains(banned), "{name} still has {banned}");
            }
            for selector in [
                "QPushButton#playButton",
                "QPushButton[role=\"play\"]",
                "QWidget#viewportSurround",
                "QWidget#viewport",
                "QWidget#trackLane[pass=\"beauty\"]",
                "QWidget#trackLane[pass=\"diffuse\"]",
                "QWidget#trackLane[pass=\"spec\"]",
                "QWidget#trackLane[pass=\"volume\"]",
                "QWidget[surface=\"well\"]",
                "QFrame#annotateToolbar",
                "QFrame#sheetTile",
                "QDialog#settingsDialog",
                "QDialog#aboutDialog",
                "QWidget#layerRow[selected=\"true\"]",
                "QFrame#spectralTopBar",
                "QListView#layerList",
                "QTreeView#layerList",
                "QSlider#strokeWidth",
                "QProgressBar#opacityBar",
                "QTabBar#dockTabs",
                "QScrollBar:vertical",
            ] {
                assert!(sheet.contains(selector), "{name} missing {selector}");
            }
        }
        assert!(dark.contains("#101114"));
        assert!(dark.contains("#FF6A1A"));
        assert!(dark.contains("#2D5270"));
        assert!(dark.contains("background: transparent"));
        assert!(neon.contains("#22242B"));
        assert!(neon.contains("#FF2D87"));
        assert!(neon.contains("#00B8FF"));
        assert!(studio.contains("#2B2D33"));
        assert!(studio.contains("#4E7A98"));
        assert!(studio.contains("#F3F4F6"));
    }
}
