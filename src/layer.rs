//! Comp **layer model** — the review-player spine (#103).
//!
//! Pure data model: no GPU, no UI, no decode, no IO. It describes an ordered
//! stack of placed pixel sources and resolves, for a given global timeline frame,
//! the concrete sequence of composite steps a renderer should perform. Every
//! decision here is unit-testable headlessly, ahead of any wiring — the same
//! pure-logic-first discipline as the playback contracts in `docs/playback/`.
//!
//! # Terminology (important)
//!
//! A [`Layer`] here is a **composite layer** — a placed source in an ordered
//! stack, à la Chaos Player / Nuke. This is **distinct** from
//! [`crate::exr_loader::LogicalLayer`], which is an AOV / channel-group *within*
//! one EXR. A comp [`Layer`] *selects* one such AOV of its source through the
//! `aov` field of [`LayerSource::Image`]. Throughout the review-player code,
//! "layer" without qualification means the comp layer; the EXR AOV is always the
//! "logical layer".
//!
//! # The structural idea
//!
//! floki's current `Slot::{A,B}` + [`crate::viewer::CompareMode`] are a hardcoded
//! two-input special case. This module generalizes them to a `Vec<Layer>`, and in
//! doing so splits the **two axes that `CompareMode` conflates**:
//!
//! - **Arrangement** — *where* visible layers are placed: one viewport, a wipe, or
//!   a grid. This is [`Layout`].
//! - **Blend** — *how* stacked layers combine: [`crate::viewer::BlendMode`]
//!   (reused unchanged; it is the single source of truth shared with the shader).
//!
//! Once separated, the existing modes and the new features become *arrangements*
//! of one model (see `docs/review-player/layer-model.md` for the full mapping):
//!
//! | Today / target                | Layer-model expression                         |
//! |-------------------------------|------------------------------------------------|
//! | `SingleA` / `SingleB`         | one `solo` layer                               |
//! | `Composite` + `BlendMode`     | ≥2 enabled layers, `Layout::Stack`, per blend  |
//! | `Wipe`                        | 2 visible layers, `Layout::Wipe`               |
//! | `SideBySide`                  | 2 visible layers, `Layout::Grid { cols: 2 }`   |
//! | N-way compare (#104)          | N visible layers, `Layout::Grid`               |
//! | Locked-step A/B (#98)         | 2 layers sharing the global frame (offset 0)   |
//! | Back-to-Beauty                | additive AOV layers of one source (`Add`)      |
//! | Adjustment / Brush / Text     | non-image [`LayerSource`] variants             |
//!
//! Caches generalize too: the T1/T2 `(Slot, frame)` key (`src/cache.rs`) becomes
//! `(LayerId, source_frame)` — see [`Layer::cache_key`]. `LayerId` is stable
//! across reordering, so moving a layer in the stack never invalidates its cache.

// Design-spike module: this lands the pure model + its tests ahead of consumers.
// Wiring follows in the build phases — N-way compare (#104), locked-step A/B
// (#98), the CC/adjustment suite (#102), and the per-layer thumbnail cache (#112)
// — so most public items are intentionally unused for now.
#![allow(dead_code)]

use crate::viewer::BlendMode;

/// Stable identity for a comp layer. Caches key on `(LayerId, source_frame)`, so
/// reordering or renaming a layer never invalidates its decoded/uploaded frames.
/// Allocated monotonically by the owning [`LayerStack`]; never reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct LayerId(u64);

/// Opaque handle to a pixel source (an image sequence or a still) owned elsewhere
/// — the app's loader / frame cache. The layer model never decodes or touches
/// disk; it only *references* a source and selects frames/AOVs from it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SourceId(pub u64);

/// Infinite-canvas placement of a layer: independent x/y scale (aspect) and a
/// canvas-space offset. Identity by default, so a freshly added layer fills the
/// frame exactly as the legacy single-image path does. Rotation is deferred.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transform {
    /// Canvas-space offset, in normalized units (0,0 = centered).
    pub position: (f32, f32),
    /// Independent per-axis scale; `(1.0, 1.0)` is unscaled.
    pub scale: (f32, f32),
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: (0.0, 0.0),
            scale: (1.0, 1.0),
        }
    }
}

/// Per-layer time mapping. A layer's frames live in **source-frame space**
/// `[in_point, out_point]`; `offset` shifts the source along the global timeline,
/// so `source_frame = global + offset`. Locked-step A/B is `offset == 0` on both
/// layers (they advance together); a staggered compare gives one layer a nonzero
/// offset. Outside the trimmed span the layer is blank (the renderer holds or
/// shows nothing) — never an out-of-range decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trim {
    pub in_point: u32,
    pub out_point: u32,
    /// Added to the global frame to reach the source frame. Signed: a negative
    /// offset makes the layer lag the global playhead.
    pub offset: i64,
}

impl Trim {
    /// A trim spanning `[in_point, out_point]` with no global offset.
    #[must_use]
    pub fn full(in_point: u32, out_point: u32) -> Self {
        Self {
            in_point,
            out_point,
            offset: 0,
        }
    }

    /// Map a global timeline frame to this layer's source frame, or `None` when
    /// the global frame falls outside the trimmed span (the layer is blank there).
    #[must_use]
    pub fn source_frame(&self, global: u32) -> Option<u32> {
        let s = i64::from(global) + self.offset;
        if s < i64::from(self.in_point) || s > i64::from(self.out_point) {
            return None;
        }
        u32::try_from(s).ok()
    }
}

/// What a layer draws.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LayerSource {
    /// A pixel source (sequence or still), showing one of its AOV / logical
    /// layers. `aov` indexes [`crate::exr_loader::ExrData::logical_layers`] of the
    /// referenced source — generalizing today's `active_layer`.
    Image { source: SourceId, aov: usize },
    /// A color correction applied to the composited result of the layers *below*
    /// it. Parameters are supplied by the CC suite (#102); the model carries only
    /// identity + opacity so the composite order is fully expressible today.
    Adjustment,
}

/// One composite layer: a placed source plus its blend/opacity/visibility/time.
#[derive(Clone, PartialEq, Debug)]
pub struct Layer {
    pub id: LayerId,
    pub name: String,
    pub source: LayerSource,
    pub transform: Transform,
    /// How this layer combines with the accumulated result below it.
    pub blend: BlendMode,
    /// Layer opacity in `[0, 1]` (not enforced here; the renderer clamps).
    pub opacity: f32,
    /// Whether the layer participates when no layer is soloed.
    pub enabled: bool,
    /// Solo overrides `enabled`: if *any* layer is soloed, only soloed layers
    /// render. Mirrors the A/B `SingleA`/`SingleB` isolation.
    pub solo: bool,
    pub trim: Trim,
}

impl Layer {
    /// The cache key for this layer at a resolved source frame — the
    /// generalization of the T1/T2 `(Slot, frame)` key (`src/cache.rs`).
    #[must_use]
    pub fn cache_key(&self, source_frame: u32) -> (LayerId, u32) {
        (self.id, source_frame)
    }
}

/// Spatial arrangement of the visible layers — the axis `CompareMode` conflates
/// with blend. `Stack` composites in one viewport; `Wipe`/`Grid` lay visible
/// layers out side by side. Arrangement is independent of each layer's
/// [`BlendMode`].
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Layout {
    /// Composite bottom-to-top in a single viewport (today's Single / Composite).
    #[default]
    Stack,
    /// Split-screen wipe between the visible layers at `position` ∈ `[0, 1]`.
    Wipe { position: f32 },
    /// Tile the visible layers in a grid `cols` wide (SideBySide / N-way / sheet).
    Grid { cols: u32 },
}

/// One step of the resolved composite for a given global frame, bottom-to-top.
/// A renderer walks these in order, accumulating the result. Splitting image
/// draws from adjustments keeps the pipeline expressible without the renderer
/// re-deriving visibility/trim.
#[derive(Clone, PartialEq, Debug)]
pub enum Step {
    /// Draw an image layer over the accumulated result.
    Draw(Draw),
    /// Apply a color correction (#102) to the accumulated result. Params are
    /// resolved by the CC suite; the model carries identity + opacity only.
    Adjust { id: LayerId, opacity: f32 },
}

/// A concrete image draw for one visible image layer at one global frame: the
/// exact source, AOV, and source frame to fetch, with how to place and blend it.
#[derive(Clone, PartialEq, Debug)]
pub struct Draw {
    pub id: LayerId,
    pub source: SourceId,
    pub aov: usize,
    pub source_frame: u32,
    pub transform: Transform,
    pub blend: BlendMode,
    pub opacity: f32,
}

/// An ordered comp stack: index `0` is the **bottom** layer (drawn first), higher
/// indices draw over it. Owns `LayerId` allocation and the global [`Layout`].
#[derive(Clone, Debug, Default)]
pub struct LayerStack {
    layers: Vec<Layer>,
    layout: Layout,
    next_id: u64,
}

impl LayerStack {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    #[must_use]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    /// Layers bottom-to-top.
    pub fn iter(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter()
    }

    #[must_use]
    pub fn get(&self, id: LayerId) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }

    pub fn get_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }

    /// Append an image layer on top, returning its stable [`LayerId`]. Defaults
    /// mirror the legacy single-image path: identity transform, `Over` blend,
    /// full opacity, enabled, not soloed.
    pub fn push_image(
        &mut self,
        name: impl Into<String>,
        source: SourceId,
        aov: usize,
        trim: Trim,
    ) -> LayerId {
        let id = self.alloc_id();
        self.layers.push(Layer {
            id,
            name: name.into(),
            source: LayerSource::Image { source, aov },
            transform: Transform::default(),
            blend: BlendMode::default(),
            opacity: 1.0,
            enabled: true,
            solo: false,
            trim,
        });
        id
    }

    /// Append an adjustment layer on top, returning its stable [`LayerId`].
    pub fn push_adjustment(&mut self, name: impl Into<String>) -> LayerId {
        let id = self.alloc_id();
        self.layers.push(Layer {
            id,
            name: name.into(),
            source: LayerSource::Adjustment,
            transform: Transform::default(),
            blend: BlendMode::default(),
            opacity: 1.0,
            enabled: true,
            solo: false,
            trim: Trim::full(0, u32::MAX),
        });
        id
    }

    /// Remove a layer by id, returning whether it was present.
    pub fn remove(&mut self, id: LayerId) -> bool {
        let before = self.layers.len();
        self.layers.retain(|l| l.id != id);
        self.layers.len() != before
    }

    /// Move a layer to absolute stack index `to` (clamped), preserving its id and
    /// cache. Returns whether the layer was found.
    pub fn move_to(&mut self, id: LayerId, to: usize) -> bool {
        let Some(from) = self.layers.iter().position(|l| l.id == id) else {
            return false;
        };
        let layer = self.layers.remove(from);
        let to = to.min(self.layers.len());
        self.layers.insert(to, layer);
        true
    }

    /// Whether any layer is soloed (solo overrides `enabled`).
    #[must_use]
    pub fn solo_active(&self) -> bool {
        self.layers.iter().any(|l| l.solo)
    }

    /// The layers that actually render, bottom-to-top: when any layer is soloed,
    /// only soloed layers; otherwise every `enabled` layer.
    pub fn visible(&self) -> impl Iterator<Item = &Layer> {
        let solo = self.solo_active();
        self.layers
            .iter()
            .filter(move |l| if solo { l.solo } else { l.enabled })
    }

    /// Resolve the composite for a global timeline frame: the bottom-to-top
    /// sequence of [`Step`]s to render. Layers blank at this frame are dropped:
    /// an image layer outside its [`Trim`] span produces no fetch, and an
    /// adjustment layer outside its span applies no grade — trim bounds *where*
    /// every layer kind is active, not just image sources.
    #[must_use]
    pub fn composite_at(&self, global: u32) -> Vec<Step> {
        self.visible()
            .filter_map(|l| {
                // A layer is active only within its trimmed span; for image
                // layers this is also the source frame to fetch.
                let source_frame = l.trim.source_frame(global)?;
                Some(match &l.source {
                    LayerSource::Image { source, aov } => Step::Draw(Draw {
                        id: l.id,
                        source: *source,
                        aov: *aov,
                        source_frame,
                        transform: l.transform,
                        blend: l.blend,
                        opacity: l.opacity,
                    }),
                    LayerSource::Adjustment => Step::Adjust {
                        id: l.id,
                        opacity: l.opacity,
                    },
                })
            })
            .collect()
    }

    fn alloc_id(&mut self) -> LayerId {
        let id = LayerId(self.next_id);
        self.next_id += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(n: u64) -> SourceId {
        SourceId(n)
    }

    #[test]
    fn transform_and_trim_defaults_are_identity_and_unshifted() {
        assert_eq!(
            Transform::default(),
            Transform {
                position: (0.0, 0.0),
                scale: (1.0, 1.0)
            }
        );
        let t = Trim::full(10, 20);
        assert_eq!(t.offset, 0);
        // In range maps 1:1; the bounds are inclusive.
        assert_eq!(t.source_frame(10), Some(10));
        assert_eq!(t.source_frame(20), Some(20));
        // Outside the span is blank, not clamped.
        assert_eq!(t.source_frame(9), None);
        assert_eq!(t.source_frame(21), None);
    }

    #[test]
    fn trim_offset_shifts_source_against_the_global_timeline() {
        // Layer lags the playhead by 5 frames: source = global - 5.
        let t = Trim {
            in_point: 0,
            out_point: 100,
            offset: -5,
        };
        assert_eq!(t.source_frame(5), Some(0));
        assert_eq!(t.source_frame(50), Some(45));
        // global 4 -> source -1 -> before in_point -> blank (no underflow panic).
        assert_eq!(t.source_frame(4), None);
    }

    #[test]
    fn ids_are_stable_across_reordering_so_cache_keys_survive() {
        let mut s = LayerStack::new();
        let a = s.push_image("A", src(1), 0, Trim::full(0, 10));
        let b = s.push_image("B", src(2), 0, Trim::full(0, 10));
        assert_ne!(a, b);
        let key_before = s.get(a).unwrap().cache_key(3);
        // Send the bottom layer to the top; ids (and thus cache keys) are unchanged.
        assert!(s.move_to(a, 1));
        assert_eq!(s.iter().map(|l| l.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(s.get(a).unwrap().cache_key(3), key_before);
    }

    #[test]
    fn remove_reports_presence_and_drops_the_layer() {
        let mut s = LayerStack::new();
        let a = s.push_image("A", src(1), 0, Trim::full(0, 1));
        assert!(s.remove(a));
        assert!(!s.remove(a), "second remove reports absent");
        assert!(s.is_empty());
    }

    #[test]
    fn visible_filters_enabled_and_solo_overrides() {
        let mut s = LayerStack::new();
        let a = s.push_image("A", src(1), 0, Trim::full(0, 10));
        let b = s.push_image("B", src(2), 0, Trim::full(0, 10));
        let c = s.push_image("C", src(3), 0, Trim::full(0, 10));
        // Disable B: A and C render.
        s.get_mut(b).unwrap().enabled = false;
        assert_eq!(
            s.visible().map(|l| l.id).collect::<Vec<_>>(),
            vec![a, c],
            "enabled filtering"
        );
        // Solo C: only C renders, even though A is enabled and B is disabled.
        s.get_mut(c).unwrap().solo = true;
        assert!(s.solo_active());
        assert_eq!(s.visible().map(|l| l.id).collect::<Vec<_>>(), vec![c]);
    }

    #[test]
    fn composite_at_emits_visible_image_draws_bottom_to_top() {
        let mut s = LayerStack::new();
        let a = s.push_image("bg", src(1), 0, Trim::full(0, 100));
        let b = s.push_image("fg", src(2), 2, Trim::full(0, 100));
        s.get_mut(b).unwrap().blend = BlendMode::Add;
        s.get_mut(b).unwrap().opacity = 0.5;

        let steps = s.composite_at(7);
        assert_eq!(steps.len(), 2);
        // Bottom (A) first.
        let Step::Draw(d0) = &steps[0] else {
            panic!("expected draw")
        };
        assert_eq!(
            (d0.id, d0.source, d0.aov, d0.source_frame),
            (a, src(1), 0, 7)
        );
        assert_eq!(d0.blend, BlendMode::Over);
        // Top (B): carries AOV, blend, opacity.
        let Step::Draw(d1) = &steps[1] else {
            panic!("expected draw")
        };
        assert_eq!((d1.id, d1.aov, d1.source_frame), (b, 2, 7));
        assert_eq!(d1.blend, BlendMode::Add);
        assert!((d1.opacity - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn composite_at_drops_layers_that_are_blank_at_this_frame() {
        let mut s = LayerStack::new();
        // A spans the whole range; B only [20, 40].
        s.push_image("A", src(1), 0, Trim::full(0, 100));
        s.push_image("B", src(2), 0, Trim::full(20, 40));
        // At global 10, only A is live.
        assert_eq!(s.composite_at(10).len(), 1);
        // At global 30, both.
        assert_eq!(s.composite_at(30).len(), 2);
    }

    #[test]
    fn adjustment_layer_resolves_to_an_adjust_step_over_the_stack() {
        let mut s = LayerStack::new();
        s.push_image("plate", src(1), 0, Trim::full(0, 10));
        let adj = s.push_adjustment("grade");
        s.get_mut(adj).unwrap().opacity = 0.8;
        let steps = s.composite_at(0);
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[0], Step::Draw(_)), "plate draws first");
        match steps[1] {
            Step::Adjust { id, opacity } => {
                assert_eq!(id, adj);
                assert!((opacity - 0.8).abs() < f32::EPSILON);
            }
            Step::Draw(_) => panic!("expected an adjust step on top"),
        }
    }

    #[test]
    fn adjustment_layer_trim_bounds_where_the_grade_applies() {
        let mut s = LayerStack::new();
        s.push_image("plate", src(1), 0, Trim::full(0, 100));
        let adj = s.push_adjustment("grade");
        // Restrict the grade to frames [20, 40].
        s.get_mut(adj).unwrap().trim = Trim::full(20, 40);

        // Outside the grade's span: only the plate draws, no adjust step.
        let before = s.composite_at(10);
        assert_eq!(before.len(), 1);
        assert!(matches!(before[0], Step::Draw(_)));

        // Inside the span: the adjust step is emitted over the plate.
        let inside = s.composite_at(30);
        assert_eq!(inside.len(), 2);
        assert!(matches!(inside[1], Step::Adjust { id, .. } if id == adj));
    }

    #[test]
    fn back_to_beauty_is_additive_aov_layers_of_one_source() {
        // The Back-to-Beauty case from the roadmap: several AOVs of the SAME
        // source, stacked with additive blend, all live at the frame.
        let mut s = LayerStack::new();
        let beauty_src = src(42);
        for (i, aov) in ["diffuse", "specular", "sss"].iter().enumerate() {
            let id = s.push_image(*aov, beauty_src, i, Trim::full(0, 48));
            s.get_mut(id).unwrap().blend = BlendMode::Add;
        }
        let steps = s.composite_at(12);
        assert_eq!(steps.len(), 3);
        for (i, step) in steps.iter().enumerate() {
            let Step::Draw(d) = step else {
                panic!("expected draw")
            };
            assert_eq!(d.source, beauty_src, "same source");
            assert_eq!(d.aov, i, "distinct AOV per layer");
            assert_eq!(d.blend, BlendMode::Add);
        }
    }

    #[test]
    fn layout_defaults_to_stack_and_is_settable() {
        let mut s = LayerStack::new();
        assert_eq!(s.layout(), Layout::Stack);
        // N-way compare arrangement: a grid, independent of per-layer blend.
        s.set_layout(Layout::Grid { cols: 2 });
        assert_eq!(s.layout(), Layout::Grid { cols: 2 });
    }
}
