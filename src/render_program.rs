//! Resolve viewer state into a render program via the [`crate::layer`] model
//! (#114, the first build phase on the layer-model spine #103).
//!
//! Today's A/B viewer is a hardcoded two-input special case: [`ExrViewer`] holds
//! `compare_mode` / `blend_mode` / `active_layer` and the draw paths branch on
//! [`CompareMode`] inline to decide which texture(s) to bind and how. This module
//! is the seam that routes those decisions through the pure layer model instead:
//! the viewer keeps a two-layer [`LayerStack`] (slot A at the bottom, slot B on
//! top), and every UI repaint [`resolve`] configures it from the live viewer
//! state and reads [`LayerStack::composite_at`] to produce a [`RenderProgram`]
//! the draw paths consume. (It resolves at global timeline frame 0 this phase —
//! see [`resolve`] for why that is exact, not a placeholder.)
//!
//! [`ExrViewer`]: crate::viewer::ExrViewer
//!
//! # What moves into the model, and what stays viewer-side
//!
//! `CompareMode` conflates *three* axes, only one of which belongs in the pure
//! model this phase:
//!
//! - **Which inputs render + each input's source / AOV / blend / opacity** —
//!   resolved by the model (`composite_at` over the configured stack). This is the
//!   part #114 asks the renderer to read.
//! - **Spatial arrangement** (wipe split + line, side-by-side rects/normalize) —
//!   stays viewer-side: [`Arrangement`] is only a *tag* saying which geometry path
//!   to take; the geometry itself lives in the draw functions unchanged.
//! - **Inspection** (the `DiffMatte` `|A-B|` heat map with its colormap / metric /
//!   floor) — also viewer-side. It is neither a blend nor a layout, so it never
//!   enters [`crate::layer`]; the renderer flags it via [`Arrangement::Diff`].
//!
//! Keeping arrangement + inspection out of the pure model is deliberate: encoding
//! wipe geometry and the diff heat map into [`crate::layer::Layout`] would be a
//! speculative, untested extension. They land with N-way compare (#104), which is
//! also where the N-input iterative render replaces today's 2-input shader. This
//! phase is purely additive: the shader, the uniforms, and every draw branch keep
//! their exact behavior; only the *dispatch* that decides which draw to make now
//! flows through the model.
//!
//! # Cache-key correspondence (move toward `(LayerId, source_frame)`)
//!
//! The two layers carry **stable** [`LayerId`]s (allocated once when the viewer is
//! built). [`slot_of`] / [`layer_id_for`] pin the `Slot ⇄ LayerId` correspondence
//! so a future cache keyed on `(LayerId, source_frame)` is a drop-in extension of
//! today's `(Slot, frame)` ring (`src/cache.rs`) rather than a rewrite. The ring
//! itself is untouched this phase.

use crate::cache::Slot;
use crate::layer::{LayerId, LayerSource, LayerStack, SourceId, Step, Trim};
use crate::viewer::{BlendMode, CompareMode};

/// Source handle for the slot-A (bottom) layer of the A/B stack.
const SRC_A: SourceId = SourceId(0);
/// Source handle for the slot-B (top) layer of the A/B stack.
const SRC_B: SourceId = SourceId(1);

/// Which viewer image slot a resolved draw pulls from. `SourceId(0)` is slot A,
/// `SourceId(1)` is slot B — the [`slot_of`] correspondence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramInput {
    A,
    B,
}

/// One concrete draw the renderer must emit, resolved from a [`Step::Draw`]. Holds
/// only what the existing draw paths already act on; `aov` is the logical-layer
/// index (today's `active_layer`), unclamped — the renderer clamps to the source's
/// layer count when binding, exactly as before.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ProgramDraw {
    pub input: ProgramInput,
    pub aov: usize,
    pub blend: BlendMode,
    pub opacity: f32,
}

/// How the resolved draws are arranged on screen / inspected. Only a selector: the
/// wipe/side-by-side geometry and the diff heat map stay in the draw paths.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Arrangement {
    /// One viewport. One draw (Single) or, when [`RenderProgram::is_composite`],
    /// the top draw composited over the bottom via the blend shader (Composite).
    Stacked,
    /// Split-screen wipe at `position` ∈ `[0, 1]` (`wipe_center[0]`).
    Wipe { position: f32 },
    /// The two draws tiled left/right.
    SideBySide,
    /// `|A − B|` heat map over the two draws (the `DiffMatte` inspection).
    Diff,
}

/// The resolved program the draw paths consume instead of matching `compare_mode`
/// inline. `draws` are bottom-to-top (≤2 this phase).
#[derive(Clone, PartialEq, Debug)]
pub struct RenderProgram {
    pub draws: Vec<ProgramDraw>,
    pub arrangement: Arrangement,
    /// Drives `Uniforms.is_composite` for the top draw (the `Composite` mode).
    pub is_composite: bool,
}

/// Everything [`resolve`] needs from the viewer — plain data, so resolution is a
/// pure function testable headlessly.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ResolveInput {
    pub compare_mode: CompareMode,
    pub blend_mode: BlendMode,
    /// The active logical layer (AOV) index — selects each layer's `aov`.
    pub active_layer: usize,
    /// Wipe split position (`wipe_center[0]`), carried into [`Arrangement::Wipe`].
    pub wipe_position: f32,
}

/// Build the canonical two-layer A/B stack: slot A at the bottom, slot B on top,
/// both spanning all frames with identity transform / `Over` blend. Returns the
/// stack and the two **stable** [`LayerId`]s (A, B) the viewer holds for the life
/// of the session, so cache keys derived from them never churn.
#[must_use]
pub fn new_ab_stack() -> (LayerStack, (LayerId, LayerId)) {
    let mut stack = LayerStack::new();
    let a = stack.push_image("A", SRC_A, 0, Trim::full(0, u32::MAX));
    let b = stack.push_image("B", SRC_B, 0, Trim::full(0, u32::MAX));
    (stack, (a, b))
}

/// The stable `Slot` for a program input — A→`Slot::A`, B→`Slot::B`. Part of the
/// cache-key seam consumed when the ring generalizes to `(LayerId, source_frame)`
/// in #104; unused by the render path this phase.
#[allow(dead_code)]
#[must_use]
pub fn slot_of(input: ProgramInput) -> Slot {
    match input {
        ProgramInput::A => Slot::A,
        ProgramInput::B => Slot::B,
    }
}

/// The stable `LayerId` of a cache slot in the A/B stack. Cache-key seam for #104
/// (see [`slot_of`]); unused by the render path this phase.
#[allow(dead_code)]
#[must_use]
pub fn layer_id_for(slot: Slot, ids: (LayerId, LayerId)) -> LayerId {
    match slot {
        Slot::A => ids.0,
        Slot::B => ids.1,
    }
}

/// The spatial / inspection arrangement for a compare mode.
fn arrangement_of(compare_mode: CompareMode, wipe_position: f32) -> Arrangement {
    match compare_mode {
        CompareMode::SingleA | CompareMode::SingleB | CompareMode::Composite => {
            Arrangement::Stacked
        }
        CompareMode::Wipe => Arrangement::Wipe {
            position: wipe_position,
        },
        CompareMode::SideBySide => Arrangement::SideBySide,
        CompareMode::DiffMatte => Arrangement::Diff,
    }
}

fn input_of(source: SourceId) -> ProgramInput {
    if source == SRC_B {
        ProgramInput::B
    } else {
        ProgramInput::A
    }
}

/// Apply the viewer state onto the two-layer stack (Decision: the existing fields
/// remain the source of truth; the stack is a derived view rebuilt each frame).
///
/// - `Single{A,B}` solos that slot, so only it is visible.
/// - Every other mode leaves both layers enabled (no solo), so both render.
/// - `blend_mode` reaches the **top (B)** layer only in `Composite`; otherwise the
///   shader's blend is inert, so the model carries `Over` to stay truthful.
/// - The global [`Layout`] mirrors the arrangement (cosmetic this phase —
///   `composite_at` ignores layout — but keeps the stack self-describing for #104).
///
/// [`Layout`]: crate::layer::Layout
fn configure(stack: &mut LayerStack, ids: (LayerId, LayerId), input: &ResolveInput) {
    let (a, b) = ids;
    if let Some(la) = stack.get_mut(a) {
        la.source = LayerSource::Image {
            source: SRC_A,
            aov: input.active_layer,
        };
        la.blend = BlendMode::Over;
        la.enabled = true;
        la.solo = input.compare_mode == CompareMode::SingleA;
    }
    if let Some(lb) = stack.get_mut(b) {
        lb.source = LayerSource::Image {
            source: SRC_B,
            aov: input.active_layer,
        };
        lb.blend = if input.compare_mode == CompareMode::Composite {
            input.blend_mode
        } else {
            BlendMode::Over
        };
        lb.enabled = true;
        lb.solo = input.compare_mode == CompareMode::SingleB;
    }
    stack.set_layout(
        match arrangement_of(input.compare_mode, input.wipe_position) {
            Arrangement::Stacked | Arrangement::Diff => crate::layer::Layout::Stack,
            Arrangement::Wipe { position } => crate::layer::Layout::Wipe { position },
            Arrangement::SideBySide => crate::layer::Layout::Grid { cols: 2 },
        },
    );
}

/// Configure the two-layer stack from `input` and resolve it — via
/// [`LayerStack::composite_at`] — into the [`RenderProgram`] the draw paths
/// consume. This is the single place `composite_at` drives the render.
///
/// Note on B presence: resolution is independent of whether slot B is actually
/// loaded. A mode that needs B (e.g. `Composite`, `DiffMatte`, `SingleB`) still
/// resolves to a B draw; the renderer binds nothing for an absent B and so draws
/// blank — exactly the pre-existing `if let Some(bg_b)` behavior. Keeping the gate
/// in the renderer (not here) preserves that behavior precisely.
#[must_use]
pub fn resolve(
    stack: &mut LayerStack,
    ids: (LayerId, LayerId),
    input: &ResolveInput,
) -> RenderProgram {
    configure(stack, ids, input);
    // Resolve at global frame 0 deliberately. This phase's two layers are always
    // `Trim::full(0, u32::MAX)` with offset 0 and nothing mutates their trim, so
    // every global frame yields the same visible draws; the only thing that varies
    // with the frame is each `Draw`'s `source_frame`, which `ProgramDraw` does not
    // carry — the renderer still fetches by `active_layer` through the existing
    // texture caches. So the program is frame-invariant and `0` is exact, not a
    // placeholder. When per-layer trim/offset become real (#104 / locked-step A/B
    // #98), thread the live global frame through `ResolveInput` and pass it here.
    let draws = stack
        .composite_at(0)
        .into_iter()
        .filter_map(|step| match step {
            Step::Draw(d) => Some(ProgramDraw {
                input: input_of(d.source),
                aov: d.aov,
                blend: d.blend,
                opacity: d.opacity,
            }),
            // No adjustment layers in the A/B phase; they arrive with #102.
            Step::Adjust { .. } => None,
        })
        .collect();
    RenderProgram {
        draws,
        arrangement: arrangement_of(input.compare_mode, input.wipe_position),
        is_composite: input.compare_mode == CompareMode::Composite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(compare_mode: CompareMode) -> ResolveInput {
        ResolveInput {
            compare_mode,
            blend_mode: BlendMode::Over,
            active_layer: 0,
            wipe_position: 0.5,
        }
    }

    /// Resolve against a fresh A/B stack, the way the viewer does each frame.
    fn run(input: &ResolveInput) -> RenderProgram {
        let (mut stack, ids) = new_ab_stack();
        resolve(&mut stack, ids, input)
    }

    #[test]
    fn single_a_solos_slot_a() {
        let p = run(&input(CompareMode::SingleA));
        assert_eq!(p.arrangement, Arrangement::Stacked);
        assert!(!p.is_composite);
        assert_eq!(p.draws.len(), 1);
        assert_eq!(p.draws[0].input, ProgramInput::A);
    }

    #[test]
    fn single_b_solos_slot_b() {
        let p = run(&input(CompareMode::SingleB));
        assert_eq!(p.arrangement, Arrangement::Stacked);
        assert!(!p.is_composite);
        assert_eq!(p.draws.len(), 1);
        assert_eq!(p.draws[0].input, ProgramInput::B);
    }

    #[test]
    fn composite_stacks_b_over_a_with_the_blend_on_top() {
        let mut i = input(CompareMode::Composite);
        i.blend_mode = BlendMode::Add;
        let p = run(&i);
        assert_eq!(p.arrangement, Arrangement::Stacked);
        assert!(p.is_composite);
        assert_eq!(p.draws.len(), 2);
        // Bottom is A with Over; the blend rides only the top (B) draw.
        assert_eq!(
            (p.draws[0].input, p.draws[0].blend),
            (ProgramInput::A, BlendMode::Over)
        );
        assert_eq!(
            (p.draws[1].input, p.draws[1].blend),
            (ProgramInput::B, BlendMode::Add)
        );
    }

    #[test]
    fn wipe_is_two_draws_with_the_split_position() {
        let mut i = input(CompareMode::Wipe);
        i.wipe_position = 0.25;
        let p = run(&i);
        assert_eq!(p.arrangement, Arrangement::Wipe { position: 0.25 });
        assert!(!p.is_composite);
        assert_eq!(
            p.draws.iter().map(|d| d.input).collect::<Vec<_>>(),
            vec![ProgramInput::A, ProgramInput::B]
        );
        // Wipe never composites: the top draw keeps Over.
        assert_eq!(p.draws[1].blend, BlendMode::Over);
    }

    #[test]
    fn side_by_side_is_two_draws_tiled() {
        let p = run(&input(CompareMode::SideBySide));
        assert_eq!(p.arrangement, Arrangement::SideBySide);
        assert!(!p.is_composite);
        assert_eq!(p.draws.len(), 2);
    }

    #[test]
    fn diff_matte_is_diff_arrangement_over_both_inputs_not_a_blend() {
        let mut i = input(CompareMode::DiffMatte);
        // The diff inspection ignores blend entirely; setting it must not leak in.
        i.blend_mode = BlendMode::Multiply;
        let p = run(&i);
        assert_eq!(p.arrangement, Arrangement::Diff);
        assert!(!p.is_composite, "diff is an inspection, not a composite");
        assert_eq!(
            p.draws.iter().map(|d| d.input).collect::<Vec<_>>(),
            vec![ProgramInput::A, ProgramInput::B]
        );
        // The colormap/metric/floor live on the viewer, never in the program — by
        // construction RenderProgram has no diff fields to carry them.
        assert_eq!(p.draws[1].blend, BlendMode::Over);
    }

    #[test]
    fn aov_threads_active_layer_into_every_draw() {
        let mut i = input(CompareMode::Composite);
        i.active_layer = 3;
        let p = run(&i);
        assert!(p.draws.iter().all(|d| d.aov == 3));
    }

    #[test]
    fn blink_odd_phase_resolves_to_b_solo() {
        // Blink alternates the effective compare_mode SingleA/SingleB; the resolver
        // sees the already-chosen mode and solos the right slot.
        assert_eq!(
            run(&input(CompareMode::SingleA)).draws[0].input,
            ProgramInput::A
        );
        assert_eq!(
            run(&input(CompareMode::SingleB)).draws[0].input,
            ProgramInput::B
        );
    }

    #[test]
    fn layer_ids_are_stable_and_map_to_slots() {
        let (_stack, ids) = new_ab_stack();
        // The Slot ⇄ LayerId correspondence round-trips and is order-stable.
        assert_eq!(layer_id_for(Slot::A, ids), ids.0);
        assert_eq!(layer_id_for(Slot::B, ids), ids.1);
        assert_eq!(slot_of(ProgramInput::A), Slot::A);
        assert_eq!(slot_of(ProgramInput::B), Slot::B);
        // A fresh stack reuses the same monotonic ids — they are deterministic, so
        // a cache keyed on them survives across rebuilds.
        let (_s2, ids2) = new_ab_stack();
        assert_eq!(ids, ids2);
    }

    #[test]
    fn the_two_draws_resolve_to_the_stable_layer_slots() {
        // composite_at's draws carry the stable ids, and those map back to A/B.
        let (mut stack, ids) = new_ab_stack();
        let _ = resolve(&mut stack, ids, &input(CompareMode::Composite));
        let steps = stack.composite_at(0);
        let ordered: Vec<LayerId> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Draw(d) => Some(d.id),
                Step::Adjust { .. } => None,
            })
            .collect();
        assert_eq!(ordered, vec![ids.0, ids.1], "A below, B above");
    }
}
