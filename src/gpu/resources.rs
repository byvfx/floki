//! App-owned GPU core (#54).
//!
//! The single home for the persistent GPU resources (`GpuState`, and — under
//! OCIO — the publisher for `OcioGpuPass` / `OcioTargets`). Fed by
//! [`eframe::egui_wgpu::RenderState`] at startup; the **application is the
//! source of truth**, with egui's `callback_resources` holding `Arc` clones
//! only so the `CallbackTrait` paint callbacks can fetch them.
//!
//! Before this, `GpuState` / `OcioGpuPass` / `OcioTargets` all lived *inside*
//! `egui_wgpu::Renderer::callback_resources` (a typemap) and were fetched back
//! every frame via `render_state.renderer.read().callback_resources.get::<T>()`.
//! That made the GPU core an egui implementation detail — blocking the Qt port
//! (#44) and spawning a class of "remember to invalidate the right typemap key"
//! footguns (e.g. the manual `remove::<OcioTargets>()` in `rebuild_ocio_pass`).
//!
//! Here the app owns `GpuState` directly; the viewer and app read it without
//! touching the renderer lock. The OCIO pass/targets still live in
//! `callback_resources` (the OCIO callback's `prepare` mutates `OcioTargets`
//! through the typemap), but their **lifecycle** is centralized behind
//! [`GpuResources::publish_ocio_pass`] / [`GpuResources::invalidate_ocio_targets`]
//! so call sites don't hand-roll the insert/remove pair.

use eframe::egui_wgpu::RenderState;
use std::sync::Arc;

use crate::gpu::GpuState;

/// App-owned GPU resources. See the module docs.
pub struct GpuResources {
    /// The egui/wgpu render surface. Kept (not just `device`/`queue`) so the
    /// OCIO path can (re)publish `OcioGpuPass` / `OcioTargets` into
    /// `callback_resources` — the OCIO callback's `prepare` mutates
    /// `OcioTargets` through the typemap, so it can't be app-owned the way
    /// `GpuState` is (yet). Once the Qt port (#44) lands, this field is the
    /// seam that becomes a non-egui `wgpu::Surface`.
    pub render_state: RenderState,
    /// Persistent pipeline / sampler / uniform ring buffer / LUT layouts /
    /// colormap + background-gradient textures. `Arc` so the paint callbacks
    /// hold a clone via `callback_resources` (the `CallbackTrait` contract
    /// hands `paint` a `&CallbackResources`, not app state).
    pub gpu_state: Arc<GpuState>,
}

impl GpuResources {
    /// Build the app-owned GPU core from the egui render surface: creates the
    /// `GpuState` and publishes an `Arc<GpuState>` clone into
    /// `callback_resources` for the paint callbacks.
    #[must_use]
    pub fn new(render_state: RenderState) -> Self {
        let gpu_state = Arc::new(GpuState::new(
            &render_state.device,
            &render_state.queue,
            render_state.target_format,
        ));
        // Publish an `Arc` clone for the `CallbackTrait::paint` callbacks
        // (`ExrCallback`, `OcioCallback`), which receive `&CallbackResources`
        // and can't reach app state. Single insert, never removed — the app
        // owns the real `GpuState`; this is just a shared reference.
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(gpu_state.clone());
        Self {
            render_state,
            gpu_state,
        }
    }

    /// `&`-accessor for the egui render surface (device / queue / target_format
    /// / renderer). Call sites that previously held `&RenderState` use this.
    #[inline]
    #[must_use]
    pub const fn render_state(&self) -> &RenderState {
        &self.render_state
    }
}

impl GpuResources {
    /// Publish a (re)built [`OcioGpuPass`] into `callback_resources` (the OCIO
    /// callback reads it from there) **and** invalidate the cached
    /// [`OcioTargets`](crate::gpu::ocio_pass::OcioTargets) so the next OCIO
    /// frame recreates them against the new pass's bind-group layout.
    ///
    /// Centralizes the OCIO reload footgun (#54): without the target
    /// invalidation, a stale `scene_bind_group` from the old layout would be
    /// used with the new pipeline → wgpu validation error. Previously this was
    /// a hand-rolled `insert` + `remove::<OcioTargets>()` pair at the call site
    /// with a scary comment; now it's one named method.
    pub fn publish_ocio_pass(&self, pass: crate::gpu::ocio_pass::OcioGpuPass) {
        let mut renderer = self.render_state.renderer.write();
        renderer.callback_resources.insert(pass);
        renderer
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioTargets>();
    }

    /// Publish a (re)built [`OcioGpuPass`](crate::gpu::ocio_pass::OcioGpuPass)
    /// **built for the `Rgba8Unorm` thumbnail format** into `callback_resources`,
    /// wrapped in [`OcioThumbnailPass`](crate::gpu::ocio_pass::OcioThumbnailPass).
    /// The contact-sheet GPU thumbnail render (#67 Phase 2) reads it back to run
    /// the OCIO display transform offscreen. Rebuilt alongside the main pass.
    pub fn publish_ocio_thumbnail_pass(&self, pass: crate::gpu::ocio_pass::OcioGpuPass) {
        self.render_state
            .renderer
            .write()
            .callback_resources
            .insert(crate::gpu::ocio_pass::OcioThumbnailPass(pass));
    }

    /// Drop the cached
    /// [`OcioThumbnailPass`](crate::gpu::ocio_pass::OcioThumbnailPass) (e.g. when
    /// the OCIO config fails to build a thumbnail pass). The contact sheet then
    /// falls back to the CPU thumbnail path until a pass is published again.
    pub fn clear_ocio_thumbnail_pass(&self) {
        self.render_state
            .renderer
            .write()
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioThumbnailPass>();
    }

    /// Whether an [`OcioThumbnailPass`](crate::gpu::ocio_pass::OcioThumbnailPass)
    /// is currently published. The contact sheet uses this to decide whether the
    /// GPU OCIO thumbnail path is available, or whether to fall back to the CPU
    /// `thumbnails` cache (config still loading / build failed).
    #[must_use]
    pub fn has_ocio_thumbnail_pass(&self) -> bool {
        self.render_state
            .renderer
            .read()
            .callback_resources
            .get::<crate::gpu::ocio_pass::OcioThumbnailPass>()
            .is_some()
    }

    /// Drop the cached [`OcioTargets`](crate::gpu::ocio_pass::OcioTargets) from
    /// `callback_resources` (e.g. when OCIO is disabled or the config is
    /// reloaded without a new pass). The next OCIO frame recreates them on
    /// demand. Centralizes the typemap invalidation (#54).
    pub fn invalidate_ocio_targets(&self) {
        self.render_state
            .renderer
            .write()
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioTargets>();
    }

    /// Drop the [`OcioGpuPass`](crate::gpu::ocio_pass::OcioGpuPass) from
    /// `callback_resources` (e.g. when OCIO is disabled). Also invalidates the
    /// targets, since they reference the pass's bind-group layout.
    ///
    /// `#[allow(dead_code)]`: staged for the OCIO-disable path (currently
    /// disabling OCIO leaves the pass cached but inert, with `ocio_active =
    /// false` gating its use). Wired once the disable flow is revisited.
    #[allow(dead_code)]
    pub fn clear_ocio_pass(&self) {
        let mut renderer = self.render_state.renderer.write();
        renderer
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioGpuPass>();
        renderer
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioTargets>();
        renderer
            .callback_resources
            .remove::<crate::gpu::ocio_pass::OcioThumbnailPass>();
    }
}
