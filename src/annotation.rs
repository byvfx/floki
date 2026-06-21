//! Lightweight annotation overlay (#45).
//!
//! Transient (per-session) markup — arrows, boxes, freehand strokes, and text —
//! drawn over the viewport for review feedback and baked into snapshots (#19) via
//! the framebuffer screenshot, so there is no separate flattening step.
//!
//! Geometry is stored in **image space** (pixels) so annotations stay locked to
//! the image under pan/zoom; [`crate::viewer`] projects to screen space when
//! drawing and back when handling input. None of this is persisted.

use eframe::egui::Color32;

/// A single annotation shape. All coordinates are image-space pixels.
#[derive(Clone, PartialEq, Debug)]
pub enum AnnotationKind {
    /// Directional arrow from `a` to `b`.
    Arrow { a: [f32; 2], b: [f32; 2] },
    /// Outlined rectangle spanning the `a`–`b` diagonal.
    Rect { a: [f32; 2], b: [f32; 2] },
    /// Freehand polyline.
    Freehand { points: Vec<[f32; 2]> },
    /// Text label anchored at `pos` (top-left).
    Text { pos: [f32; 2], text: String },
}

/// One annotation: its geometry plus stroke colour and width (screen points).
#[derive(Clone, PartialEq, Debug)]
pub struct Annotation {
    pub kind: AnnotationKind,
    pub color: Color32,
    pub width: f32,
}

/// The active drawing tool. `None` means normal viewport interaction (pan/zoom);
/// the others intercept canvas drags/clicks.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum AnnotationTool {
    #[default]
    None,
    Arrow,
    Rect,
    Freehand,
    Text,
}

impl AnnotationTool {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Off",
            Self::Arrow => "Arrow",
            Self::Rect => "Box",
            Self::Freehand => "Pen",
            Self::Text => "Text",
        }
    }

    /// The tools shown as selectable buttons in the toolbar (excludes `None`,
    /// which is the "Off" toggle).
    pub const DRAW_TOOLS: [Self; 4] = [Self::Arrow, Self::Rect, Self::Freehand, Self::Text];

    pub fn is_active(self) -> bool {
        self != Self::None
    }
}
