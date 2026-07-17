//! OCIO display-transform render pass for floki.
//!
//! Turns a `floki_ocio::GpuShaderBundle` (SPIR-V fragment + reflected bindings + LUTs) into a
//! wgpu render pipeline that samples a scene-linear input texture and writes the display-
//! transformed result. This is "pass 2" of the two-pass design; the existing WGSL pipeline
//! ("pass 1") composites + exposes into the offscreen input this pass reads.
//!
//! Binding convention (authored in `floki-ocio`'s transpiler, matched here):
//!
//! * set 1: binding 0 = scene input texture, binding 1 = scene sampler.
//! * set 0: binding 2*i = LUT texture i, binding 2*i+1 = its sampler.
//!
//! Bind group *layouts* are built from reflection so they always match the shader; the
//! resource *assignment* uses the deterministic binding scheme above.

use eframe::egui_wgpu::wgpu;
use floki_ocio::{BindingKind, GpuShaderBundle, Interp, LutTexture, TexDim};

const FULLSCREEN_VS: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // Map clip space to [0,1] UV with origin at top-left.
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return out;
}
"#;

pub struct OcioGpuPass {
    pipeline: wgpu::RenderPipeline,
    group_layouts: Vec<wgpu::BindGroupLayout>,
    set0_bind_group: wgpu::BindGroup,
    scene_sampler: wgpu::Sampler,
    // Keep LUT resources alive for the lifetime of the pass.
    _lut_textures: Vec<wgpu::Texture>,
    _lut_views: Vec<wgpu::TextureView>,
    _lut_samplers: Vec<wgpu::Sampler>,
}

impl OcioGpuPass {
    pub fn from_bundle(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bundle: &GpuShaderBundle,
        output_format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        // --- Bind group layouts from reflection (so they always match the shader) ---
        let max_group = bundle
            .bindings
            .iter()
            .map(|b| b.group)
            .max()
            .unwrap_or(0)
            .max(1);
        let mut group_layouts = Vec::new();
        for g in 0..=max_group {
            let mut entries: Vec<wgpu::BindGroupLayoutEntry> = bundle
                .bindings
                .iter()
                .filter(|b| b.group == g)
                .map(|b| wgpu::BindGroupLayoutEntry {
                    binding: b.binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: match &b.kind {
                        BindingKind::Texture(dim) => wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: view_dim(*dim),
                            multisampled: false,
                        },
                        BindingKind::Sampler => {
                            wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)
                        }
                        BindingKind::UniformBuffer => wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                    },
                    count: None,
                })
                .collect();
            entries.sort_by_key(|e| e.binding);
            group_layouts.push(
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("OCIO bind group layout"),
                    entries: &entries,
                }),
            );
        }

        // The OCIO convention requires set 1 (scene input texture + sampler);
        // `render` indexes `group_layouts[1]` unconditionally. Assert at
        // construction so a degenerate bundle fails here instead of panicking
        // mid-frame on the first `render` call.
        if group_layouts.len() < 2 {
            return Err(
                "OCIO bundle must have at least 2 bind groups (set 0 = uniforms, set 1 = scene)"
                    .to_string(),
            );
        }

        // --- Pipeline ---
        let layout_refs: Vec<Option<&wgpu::BindGroupLayout>> =
            group_layouts.iter().map(Some).collect();
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("OCIO Pipeline Layout"),
            bind_group_layouts: &layout_refs,
            immediate_size: 0,
        });

        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("OCIO fullscreen VS"),
            source: wgpu::ShaderSource::Wgsl(FULLSCREEN_VS.into()),
        });
        // Use the bundle's WGSL (naga already validated it). eframe's wgpu isn't built with
        // the `spirv` feature, and WGSL keeps this portable for non-wgpu consumers too.
        let wgsl = bundle
            .wgsl
            .as_deref()
            .ok_or_else(|| "GpuShaderBundle has no WGSL output".to_string())?;
        let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("OCIO fragment (WGSL)"),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("OCIO Display Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &vs,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &fs,
                entry_point: Some(bundle.entry_point.as_str()),
                targets: &[Some(wgpu::ColorTargetState {
                    format: output_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // --- LUT textures + set 0 bind group (binding 2*i = tex, 2*i+1 = sampler) ---
        let mut lut_textures = Vec::with_capacity(bundle.textures.len());
        let mut lut_views = Vec::with_capacity(bundle.textures.len());
        let mut lut_samplers = Vec::with_capacity(bundle.textures.len());
        for t in &bundle.textures {
            let (tex, view) = upload_lut(device, queue, t);
            let filter = match t.interpolation {
                Interp::Nearest => wgpu::FilterMode::Nearest,
                _ => wgpu::FilterMode::Linear,
            };
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("OCIO LUT sampler"),
                mag_filter: filter,
                min_filter: filter,
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            });
            lut_textures.push(tex);
            lut_views.push(view);
            lut_samplers.push(sampler);
        }

        let mut set0_entries = Vec::with_capacity(bundle.textures.len() * 2);
        for i in 0..bundle.textures.len() {
            set0_entries.push(wgpu::BindGroupEntry {
                binding: (i as u32) * 2,
                resource: wgpu::BindingResource::TextureView(&lut_views[i]),
            });
            set0_entries.push(wgpu::BindGroupEntry {
                binding: (i as u32) * 2 + 1,
                resource: wgpu::BindingResource::Sampler(&lut_samplers[i]),
            });
        }
        let set0_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO LUT bind group"),
            layout: &group_layouts[0],
            entries: &set0_entries,
        });

        let scene_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("OCIO scene sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Ok(Self {
            pipeline,
            group_layouts,
            set0_bind_group,
            scene_sampler,
            _lut_textures: lut_textures,
            _lut_views: lut_views,
            _lut_samplers: lut_samplers,
        })
    }

    /// Encode the OCIO pass: sample `input_view` (scene-linear) and write the display result
    /// into `output_view`. `scissor` (x, y, w, h in pixels) limits the (expensive) transform
    /// to the visible image region; `None` runs it over the whole target.
    ///
    /// `scene_bind_group` is the cached set-1 bind group (scene view + sampler)
    /// owned by `OcioTargets` — built once by `init_scene_bind_group` and reused
    /// across dirty frames instead of being recreated here each call.
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        output_view: &wgpu::TextureView,
        scene_bind_group: &wgpu::BindGroup,
        scissor: Option<[u32; 4]>,
    ) {
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("OCIO Display Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some([x, y, w, h]) = scissor {
            rp.set_scissor_rect(x, y, w, h);
        }
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.set0_bind_group, &[]);
        rp.set_bind_group(1, scene_bind_group, &[]);
        rp.draw(0..3, 0..1);
    }

    /// Build a set-1 (scene input) bind group binding `scene_view` + this pass's
    /// scene sampler. The viewport path caches one such group on `OcioTargets`
    /// (the scene texture is stable across frames); the contact-sheet thumbnail
    /// render (#67 Phase 2) instead renders a *fresh* scene texture per thumbnail
    /// and builds a throwaway bind group here for the single offscreen pass.
    pub fn create_scene_bind_group(
        &self,
        device: &wgpu::Device,
        scene_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO thumbnail scene bind group"),
            layout: &self.group_layouts[1],
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.scene_sampler),
                },
            ],
        })
    }
}

/// An [`OcioGpuPass`] built for the `Rgba8Unorm` contact-sheet thumbnail target,
/// distinct from the main viewport pass (which targets the swapchain
/// `target_format`). Cached in `callback_resources`, rebuilt alongside the main
/// pass in `App::rebuild_ocio_pass`, and consumed by
/// [`crate::gpu::thumbnail::generate_ocio`] (#67 Phase 2) so contact-sheet
/// thumbnails run the same OCIO display transform offscreen into an
/// egui-registerable texture.
pub struct OcioThumbnailPass(pub OcioGpuPass);

// ---------------------------------------------------------------------------
// Two-pass viewer integration: pass 1 (composite + exposure -> scene-linear
// offscreen) then pass 2 (OCIO display transform), blitted into egui's pass.
// ---------------------------------------------------------------------------

use std::sync::Arc;

use crate::gpu::GpuState;

/// Screen-sized offscreen targets for the OCIO path, plus the blit bind group for `paint`.
/// Recreated when the viewport size changes.
pub struct OcioTargets {
    width: u32,
    height: u32,
    scene_view: wgpu::TextureView,
    display_view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
    blit_uniform_buffer: wgpu::Buffer,
    /// Cached scene-input bind group (set 1) for `OcioGpuPass::render`. The
    /// scene view is stable across dirty frames (only changes on resize, which
    /// recreates `OcioTargets`), so this is built once via
    /// `OcioGpuPass::init_scene_bind_group` and reused — eliminates a
    /// per-dirty-frame `create_bind_group`.
    scene_bind_group: Option<wgpu::BindGroup>,
    /// Second scene-linear target for the layer-stack accumulate ping-pong (#99).
    /// The composite folds bottom→top through the `[scene_view, scene_view_b]` pair;
    /// the parity `start=(N-1)%2` is chosen so the final accumulation always lands
    /// back in `scene_view`, leaving pass 2 + the blit (which read `scene_view`)
    /// untouched. Unused by the single-pass path (single / wipe / side-by-side).
    scene_view_b: wgpu::TextureView,
    /// Pipeline-linear group-1 bind groups binding each scene target as `tex_b`, so
    /// an accumulate draw can read the prior accumulation. `[0]`=`scene_view`,
    /// `[1]`=`scene_view_b`.
    accum_tex_bg: [wgpu::BindGroup; 2],
    /// `render_sig` of the content currently in `display_view`; lets `prepare` skip the
    /// two passes when nothing changed. `None` after (re)creation forces a first render.
    last_render_sig: Option<u64>,
    _scene: wgpu::Texture,
    _scene_b: wgpu::Texture,
    _display: wgpu::Texture,
}

impl Drop for OcioTargets {
    fn drop(&mut self) {
        // Explicitly destroy GPU textures so memory is released in the current
        // submission cycle, not deferred to the next driver GC sweep. On a
        // window-resize drag loop this prevents a memory spike (each resize
        // creates ~83 MB of 4K Rgba16Float + display textures).
        //
        // SAFETY-BY-ORDERING (#148, same force-destroy class as #120): this is
        // sound only while no recorded-but-unsubmitted command buffer still
        // references these textures at drop time. Both drop sites hold that
        // today — `resources.rs` drops in `update()` before this frame's
        // `prepare` records anything, and the resize path in `prepare` replaces
        // the targets before the dirty-render encodes. If a future change can
        // rebuild the OCIO pass after a callback was queued (or run a second
        // callback into the resize branch in one frame), switch this to
        // drop-only like T2 eviction — a destroy of an in-flight texture aborts
        // the submit on Vulkan (see the note at viewer.rs `T2Texture`).
        self._scene.destroy();
        self._scene_b.destroy();
        self._display.destroy();
    }
}

impl OcioTargets {
    /// Build and cache the scene-input bind group (set 1) from the OCIO pass's
    /// group layout + scene sampler. Must be called once after creation (and
    /// after any recreation) before `render` is called. This avoids recreating
    /// the bind group every dirty frame.
    ///
    /// Takes the layout + sampler by reference (wgpu types are cheaply
    /// `Arc`-clonable, but borrowing avoids the clone) — the caller clones them
    /// out of `OcioGpuPass` first to sidestep the `CallbackResources` borrow
    /// conflict (can't hold `&OcioGpuPass` and `&mut OcioTargets` at once).
    fn init_scene_bind_group(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) {
        self.scene_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO scene bind group (cached)"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        }));
    }

    #[allow(clippy::too_many_arguments)] // offscreen targets need the full GPU context
    fn new(
        device: &wgpu::Device,
        blit_layout: &wgpu::BindGroupLayout,
        blit_sampler: &wgpu::Sampler,
        bg_gradient_view: &wgpu::TextureView,
        tex_layout: &wgpu::BindGroupLayout,
        tex_sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
        display_format: wgpu::TextureFormat,
    ) -> Self {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        // Rgba16Float scene targets: half the bandwidth of 32F, ample range for viewing.
        // Must match `GpuState::pipeline_linear`'s color target format in gpu/mod.rs. The
        // second (`scene_b`) is the layer-stack accumulate ping-pong partner (#99).
        let scene_desc = wgpu::TextureDescriptor {
            label: Some("OCIO scene-linear target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let scene = device.create_texture(&scene_desc);
        let scene_b = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("OCIO scene-linear target B (accumulate)"),
            ..scene_desc
        });
        let display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("OCIO display target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: display_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let scene_view = scene.create_view(&wgpu::TextureViewDescriptor::default());
        let scene_view_b = scene_b.create_view(&wgpu::TextureViewDescriptor::default());
        let display_view = display.create_view(&wgpu::TextureViewDescriptor::default());
        // Bind each scene target as `tex_b` for the accumulate ping-pong; nearest,
        // non-filtering sampler (the scene target is sampled 1:1 at screen resolution).
        let accum_tex_bg = [&scene_view, &scene_view_b].map(|view| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("OCIO accumulate tex_b bind group"),
                layout: tex_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(tex_sampler),
                    },
                ],
            })
        });
        let blit_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("OCIO blit uniform buffer"),
            size: std::mem::size_of::<crate::gpu::BlitUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO blit bind group"),
            layout: blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&display_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(blit_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: blit_uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(bg_gradient_view),
                },
            ],
        });
        Self {
            width,
            height,
            scene_view,
            scene_view_b,
            accum_tex_bg,
            display_view,
            blit_bind_group,
            blit_uniform_buffer,
            scene_bind_group: None,
            last_render_sig: None,
            _scene: scene,
            _scene_b: scene_b,
            _display: display,
        }
    }
}

/// One pass-1 draw for the OCIO path: the four bind groups a single `pipeline_linear`
/// draw needs. A frame carries one of these per image (1 for single/wipe/diff/composite,
/// 2 for side-by-side) — all rendered into the one scene-linear offscreen before a single
/// OCIO display pass, so OCIO runs once over the composited frame.
pub struct OcioPass1Draw {
    pub bg_a: Arc<wgpu::BindGroup>,
    pub bg_b: Arc<wgpu::BindGroup>,
    /// Dynamic offset into `GpuState::uniform_buffer` where this draw's
    /// `Uniforms` data was written via `queue.write_buffer`.
    pub uniform_offset: u32,
    pub lut_bg: Arc<wgpu::BindGroup>,
}

/// egui paint callback for the OCIO path. `prepare` runs pass 1 (one `pipeline_linear`
/// draw per `OcioPass1Draw`, all into the shared scene-linear offscreen) + pass 2 (the
/// single `OcioGpuPass` display transform), and `paint` blits the result — compositing the
/// display-space checker and overscan dim from `blit_uniforms`.
pub struct OcioCallback {
    pub draws: Vec<OcioPass1Draw>,
    /// When true, `draws` are a bottom→top layer-stack composite: pass 1 folds them
    /// through the `scene`/`scene_b` ping-pong (each draw its own render pass, reading
    /// the prior accumulation as `tex_b`) instead of the single-pass loop. Set for the
    /// `Composite` arrangement; false for single / wipe / side-by-side.
    pub accumulate: bool,
    pub display_format: wgpu::TextureFormat,
    pub blit_uniforms: crate::gpu::BlitUniforms,
    /// Visible image bounds in egui points (xmin, ymin, xmax, ymax). The OCIO transform is
    /// scissored to this region so it doesn't run over the empty background. `None` = whole
    /// target (e.g. side-by-side, where image content spans the canvas).
    pub scissor_pts: Option<[f32; 4]>,
    /// Hash of everything affecting the OCIO render (uniforms + texture identities + config).
    /// When it matches the last render, the two expensive passes are skipped and `paint` just
    /// re-blits the cached `display_view` — so hover / menu / animation repaints stay cheap.
    pub render_sig: u64,
}

impl eframe::egui_wgpu::CallbackTrait for OcioCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &eframe::egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut eframe::egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let [w, h] = screen_descriptor.size_in_pixels;
        let (w, h) = (w.max(1), h.max(1));

        // (Re)create offscreen targets on first use / resize.
        let need_new = callback_resources
            .get::<OcioTargets>()
            .is_none_or(|t| t.width != w || t.height != h);
        if need_new {
            let (blit_layout, blit_sampler, bg_gradient_view, tex_layout, tex_sampler) = {
                let Some(gpu) = callback_resources.get::<std::sync::Arc<GpuState>>() else {
                    return Vec::new();
                };
                let gpu = gpu.as_ref();
                (
                    gpu.blit_layout.clone(),
                    gpu.blit_sampler.clone(),
                    gpu.bg_gradient_texture
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                    gpu.bind_group_layout_tex.clone(),
                    gpu.sampler.clone(),
                )
            };
            let targets = OcioTargets::new(
                device,
                &blit_layout,
                &blit_sampler,
                &bg_gradient_view,
                &tex_layout,
                &tex_sampler,
                w,
                h,
                self.display_format,
            );
            callback_resources.insert(targets);
        }

        // Per-frame blit params (display window, overscan dim, checker) — written every
        // frame so `paint` (which has no queue) can just bind the existing buffer.
        {
            let Some(targets) = callback_resources.get::<OcioTargets>() else {
                return Vec::new();
            };
            queue.write_buffer(
                &targets.blit_uniform_buffer,
                0,
                bytemuck::bytes_of(&self.blit_uniforms),
            );
        }

        // The OCIO pass may not exist yet (config not loaded); nothing to do then.
        if callback_resources.get::<OcioGpuPass>().is_none() {
            return Vec::new();
        }

        // If OcioTargets was just (re)created, initialize the cached scene
        // bind group now that we know OcioGpuPass exists. This avoids
        // recreating it every dirty frame in `render`.
        //
        // Clone the layout + sampler out of `OcioGpuPass` first (wgpu types are
        // cheaply `Arc`-backed) so we don't hold an immutable borrow of
        // `callback_resources` while taking a mutable one for `OcioTargets`.
        let scene_bg_missing = callback_resources
            .get::<OcioTargets>()
            .is_some_and(|t| t.scene_bind_group.is_none());
        if scene_bg_missing {
            let (layout, sampler) = {
                let Some(ocio) = callback_resources.get::<OcioGpuPass>() else {
                    return Vec::new();
                };
                (ocio.group_layouts[1].clone(), ocio.scene_sampler.clone())
            };
            if let Some(targets) = callback_resources.get_mut::<OcioTargets>() {
                targets.init_scene_bind_group(device, &layout, &sampler);
            }
        }

        // Skip the two passes when nothing affecting the render changed; `paint` re-blits the
        // cached display_view, so hover / menu / animation repaints stay cheap.
        let Some(targets) = callback_resources.get::<OcioTargets>() else {
            return Vec::new();
        };
        let dirty = targets.last_render_sig != Some(self.render_sig);
        if !dirty {
            return Vec::new();
        }

        let cmd = {
            let (Some(gpu), Some(ocio), Some(targets)) = (
                callback_resources
                    .get::<std::sync::Arc<GpuState>>()
                    .map(std::sync::Arc::as_ref),
                callback_resources.get::<OcioGpuPass>(),
                callback_resources.get::<OcioTargets>(),
            ) else {
                return Vec::new();
            };

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("OCIO"),
            });

            // The α=−1 no-image sentinel clear: the blit treats scene_a < 0 as "no image"
            // so the checker only shows under drawn pixels (matching the non-OCIO path).
            let clear = wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: -1.0,
                }),
                store: wgpu::StoreOp::Store,
            };

            if self.accumulate && !self.draws.is_empty() {
                // Pass 1 (layer-stack accumulate, #99): fold the draws bottom→top through
                // the scene ping-pong. Each draw is its own render pass so the prior
                // accumulation can be sampled as `tex_b` (a read-after-write across passes,
                // barriered by wgpu). The bottom draw is `is_composite=0` (a copy over the
                // clear); every draw above binds the prior accumulation as `tex_b` and its
                // uniforms carry `is_composite=1` + `composite_accum=1` (screen-normalized
                // tex_b sampling). Parity `start=(N-1)%2` lands the final accumulation in
                // `scene_view`, so pass 2 + the blit are untouched.
                //
                // Correct while all layers share one image rect (today's A/B compare); a
                // layer smaller than the frame would clear the accumulation outside its rect
                // — differing per-layer rects land with the N-way panel (PR-B).
                let n = self.draws.len();
                let start = (n - 1) % 2;
                let scene_views = [&targets.scene_view, &targets.scene_view_b];
                for (i, d) in self.draws.iter().enumerate() {
                    let dst = (i + start) % 2;
                    let tex_b: &wgpu::BindGroup = if i == 0 {
                        d.bg_b.as_ref() // unused: bottom draw is is_composite=0
                    } else {
                        &targets.accum_tex_bg[(i - 1 + start) % 2]
                    };
                    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("OCIO pass 1 (accumulate)"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: scene_views[dst],
                            resolve_target: None,
                            depth_slice: None,
                            ops: clear,
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    rp.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
                    rp.set_pipeline(&gpu.pipeline_linear);
                    rp.set_bind_group(0, d.bg_a.as_ref(), &[]);
                    rp.set_bind_group(1, tex_b, &[]);
                    rp.set_bind_group(2, &gpu.uniform_bind_group, &[d.uniform_offset]);
                    rp.set_bind_group(3, d.lut_bg.as_ref(), &[]);
                    rp.draw(0..6, 0..1);
                }
            } else {
                // Pass 1 (single-pass): composite + exposure into scene-linear in one pass.
                // Each draw maps its own rect via the vertex shader, so two side-by-side
                // draws land in their sub-rects within the one offscreen.
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("OCIO pass 1 (scene-linear)"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.scene_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: clear,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
                rp.set_pipeline(&gpu.pipeline_linear);
                for d in &self.draws {
                    rp.set_bind_group(0, d.bg_a.as_ref(), &[]);
                    rp.set_bind_group(1, d.bg_b.as_ref(), &[]);
                    rp.set_bind_group(2, &gpu.uniform_bind_group, &[d.uniform_offset]);
                    rp.set_bind_group(3, d.lut_bg.as_ref(), &[]);
                    rp.draw(0..6, 0..1);
                }
            }

            // Pass 2: OCIO display transform, scissored to the visible image region (points ->
            // px, clamped to the target) so the expensive shader skips the empty background.
            let ppp = screen_descriptor.pixels_per_point;
            let scissor = self
                .scissor_pts
                .map(|[x0, y0, x1, y1]| {
                    let cx = ((x0 * ppp).floor().max(0.0) as u32).min(w);
                    let cy = ((y0 * ppp).floor().max(0.0) as u32).min(h);
                    let cw = ((x1 * ppp).ceil() as u32).min(w).saturating_sub(cx);
                    let ch = ((y1 * ppp).ceil() as u32).min(h).saturating_sub(cy);
                    [cx, cy, cw, ch]
                })
                .filter(|[_, _, sw, sh]| *sw > 0 && *sh > 0);
            ocio.render(
                &mut encoder,
                &targets.display_view,
                targets
                    .scene_bind_group
                    .as_ref()
                    .expect("scene_bind_group initialized in prepare()"),
                scissor,
            );

            encoder.finish()
        };

        if let Some(t) = callback_resources.get_mut::<OcioTargets>() {
            t.last_render_sig = Some(self.render_sig);
        }
        vec![cmd]
    }

    fn paint(
        &self,
        info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &eframe::egui_wgpu::CallbackResources,
    ) {
        let Some(gpu) = callback_resources.get::<std::sync::Arc<GpuState>>() else {
            return;
        };
        let gpu = gpu.as_ref();
        let Some(targets) = callback_resources.get::<OcioTargets>() else {
            return;
        };
        // Nothing has rendered into the display texture yet (the OCIO config is
        // still loading): blitting would show the zero-initialized texture —
        // alpha 0.0 reads as "covered, transparent", a transient background
        // flash over the canvas instead of the alpha=-1 no-image sentinel the
        // first real pass 1 clears to (#148).
        if targets.last_render_sig.is_none() {
            return;
        }
        // Override egui's per-primitive viewport to full screen so the screen-aligned display
        // texture maps 1:1; egui's scissor (the callback clip rect) limits what's shown.
        render_pass.set_viewport(
            0.0,
            0.0,
            info.screen_size_px[0] as f32,
            info.screen_size_px[1] as f32,
            0.0,
            1.0,
        );
        render_pass.set_pipeline(&gpu.blit_pipeline);
        render_pass.set_bind_group(0, &targets.blit_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

fn view_dim(dim: TexDim) -> wgpu::TextureViewDimension {
    match dim {
        TexDim::D1 => wgpu::TextureViewDimension::D1,
        TexDim::D2 => wgpu::TextureViewDimension::D2,
        TexDim::D3 => wgpu::TextureViewDimension::D3,
    }
}

fn upload_lut(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    t: &LutTexture,
) -> (wgpu::Texture, wgpu::TextureView) {
    let (dimension, view_dimension, extent) = match t.dim {
        TexDim::D1 => (
            wgpu::TextureDimension::D1,
            wgpu::TextureViewDimension::D1,
            wgpu::Extent3d {
                width: t.width,
                height: 1,
                depth_or_array_layers: 1,
            },
        ),
        TexDim::D2 => (
            wgpu::TextureDimension::D2,
            wgpu::TextureViewDimension::D2,
            wgpu::Extent3d {
                width: t.width,
                height: t.height.max(1),
                depth_or_array_layers: 1,
            },
        ),
        TexDim::D3 => (
            wgpu::TextureDimension::D3,
            wgpu::TextureViewDimension::D3,
            wgpu::Extent3d {
                width: t.width,
                height: t.height.max(1),
                depth_or_array_layers: t.depth.max(1),
            },
        ),
    };

    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("OCIO LUT"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&t.data_rgba),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extent.width * 16), // RGBA32F = 16 bytes/texel
            rows_per_image: Some(extent.height),
        },
        extent,
    );

    let view = tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(view_dimension),
        ..Default::default()
    });
    (tex, view)
}

#[cfg(all(
    test,
    target_os = "macos",
    any(feature = "system-ocio", feature = "vendored")
))]
mod metal_tests {
    use super::*;
    fn default_request(cfg: &floki_ocio::OcioConfig) -> floki_ocio::DisplayTransformRequest {
        let input_colorspace = cfg
            .scene_linear_colorspace()
            .or_else(|| {
                cfg.color_spaces()
                    .into_iter()
                    .find(|c| !c.is_data)
                    .map(|c| c.name)
            })
            .unwrap();
        let display = cfg.default_display();
        let view = cfg
            .displays()
            .into_iter()
            .find(|d| d.name == display)
            .map(|d| d.default_view)
            .unwrap();
        floki_ocio::DisplayTransformRequest {
            input_colorspace,
            display,
            view,
            bake_lut_size: 0,
        }
    }

    // Real-device validation of the highest-risk seam: OCIO SPIR-V -> naga -> MSL pipeline
    // creation + execution on the platform GPU (Metal here).
    #[test]
    fn ocio_pipeline_creates_and_runs_on_device() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = match pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping on-device OCIO test");
                return;
            }
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ocio-test-device"),
            required_features: wgpu::Features::FLOAT32_FILTERABLE,
            ..Default::default()
        }))
        .expect("request_device");

        let cfg = floki_ocio::OcioConfig::load(floki_ocio::ConfigSource::BuiltIn("ocio://default"))
            .expect("load default config");
        let bundle = cfg
            .build_gpu_shader(&default_request(&cfg))
            .expect("build gpu shader bundle");

        let output_format = wgpu::TextureFormat::Rgba8Unorm;
        // Validate GpuState's pipelines too (pipeline_linear + blit) — catches issues like a
        // non-blendable offscreen format that the OCIO pass alone wouldn't exercise.
        let _gpu = GpuState::new(&device, &queue, output_format);
        // This is where naga generates MSL and the driver compiles it — the real de-risk.
        let pass = OcioGpuPass::from_bundle(&device, &queue, &bundle, output_format)
            .expect("OCIO pipeline should create on this device");

        // Scene-linear 18% grey input.
        let input = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-in"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let pixels: Vec<f32> = [0.18f32, 0.18, 0.18, 1.0]
            .iter()
            .cycle()
            .take(4 * 4)
            .copied()
            .collect();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&pixels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(2 * 16),
                rows_per_image: Some(2),
            },
            wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        let input_view = input.create_view(&wgpu::TextureViewDescriptor::default());

        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("display-out"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: output_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let scene_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test scene bg"),
            layout: &pass.group_layouts[1],
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&pass.scene_sampler),
                },
            ],
        });
        pass.render(&mut encoder, &output_view, &scene_bg, None);
        queue.submit([encoder.finish()]);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }

    // Validates the OCIO blit pipeline (new bind-group layout + BLIT_SHADER) compiles and
    // runs on the platform GPU, and that its three behaviors are correct: the negative-alpha
    // sentinel means "no image" (transparent), opaque pixels pass the OCIO display color
    // through, and transparent-but-covered pixels show the display-space checker.
    #[test]
    fn blit_coverage_and_checker_on_device() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = match pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping on-device blit test");
                return;
            }
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("blit-test-device"),
            required_features: wgpu::Features::FLOAT32_FILTERABLE,
            ..Default::default()
        }))
        .expect("request_device");

        let output_format = wgpu::TextureFormat::Rgba8Unorm;
        let gpu = GpuState::new(&device, &queue, output_format);

        // 3x1 scene-linear input: texel0 alpha=-1 (sentinel/no image), texel1 alpha=1
        // (opaque), texel2 alpha=0 (covered but transparent).
        let scene = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene"),
            size: wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let scene_px: Vec<f32> = vec![
            0.0, 0.0, 0.0, -1.0, // texel0: no image
            0.0, 0.0, 0.0, 1.0, // texel1: opaque
            0.0, 0.0, 0.0, 0.0, // texel2: covered, transparent
        ];
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &scene,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&scene_px),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(3 * 16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let scene_view = scene.create_view(&wgpu::TextureViewDescriptor::default());

        // 3x1 "OCIO display" color: texel1 mid-grey (0.5), others black.
        let display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("display"),
            size: wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: output_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let display_px: [u8; 12] = [0, 0, 0, 255, 128, 128, 128, 255, 0, 0, 0, 255];
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &display,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &display_px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(3 * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let display_view = display.create_view(&wgpu::TextureViewDescriptor::default());

        // checker_size=3 so all texels land in the same (dark, 0.1) cell; whole row inside
        // the display window so no overscan dim.
        let bu = crate::gpu::BlitUniforms {
            display_min: [0.0, 0.0],
            display_max: [3.0, 1.0],
            screen_size: [3.0, 1.0],
            overscan_factor: 0.5,
            bg_mode: 0.0, // checkerboard
            bg_checker_size: 3.0,
            bg_grad_angle: 0.0,
            gamma: 1.0,
            _pad_b: 0.0,
            bg_checker_dark: [0.1, 0.1, 0.1, 0.0],
            bg_checker_light: [0.2, 0.2, 0.2, 0.0],
            bg_solid: [0.1, 0.1, 0.1, 0.0],
        };
        let bg_grad_view = gpu
            .bg_gradient_texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit-uniforms"),
            size: std::mem::size_of::<crate::gpu::BlitUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&bu));

        let blit_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit-bg"),
            layout: &gpu.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&display_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&gpu.blit_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: ubuf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&bg_grad_view),
                },
            ],
        });

        let out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit-out"),
            size: wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: output_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());

        // Read-back buffer: bytes_per_row must be 256-aligned.
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit-readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&gpu.blit_pipeline);
            rp.set_bind_group(0, &blit_bg, &[]);
            rp.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &out,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 3,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let data = readback.slice(..).get_mapped_range();
        let px = &data[..12];

        // texel0: sentinel -> fully transparent (nothing drawn).
        assert_eq!(px[3], 0, "sentinel texel should be transparent");
        // texel1: opaque -> OCIO display color (mid-grey) passes through, checker adds nothing.
        assert!(
            (px[4] as i32 - 128).abs() <= 3 && px[7] == 255,
            "opaque texel should pass display color through (got {:?})",
            &px[4..8]
        );
        // texel2: covered but transparent -> display-space checker (dark cell ~0.1).
        assert!(
            (px[8] as i32 - 26).abs() <= 6 && px[11] == 255,
            "transparent covered texel should show the checker (got {:?})",
            &px[8..12]
        );
    }

    // Decode one IEEE-754 binary16 (half) into f32. `pipeline_linear` renders into
    // an Rgba16Float target, so the accumulate readback is half-float; there's no
    // `half` crate in the dep tree. Only the finite range the test exercises needs
    // to be exact — subnormals / inf / nan are handled for completeness.
    fn f16_to_f32(h: u16) -> f32 {
        let sign = if (h >> 15) & 1 == 1 { -1.0 } else { 1.0 };
        let exp = ((h >> 10) & 0x1f) as i32;
        let mant = (h & 0x3ff) as f32;
        let mag = if exp == 0 {
            mant * 2f32.powi(-24) // subnormal: mant * 2^-24
        } else if exp == 0x1f {
            if mant == 0.0 { f32::INFINITY } else { f32::NAN }
        } else {
            (1.0 + mant / 1024.0) * 2f32.powi(exp - 15)
        };
        sign * mag
    }

    // The shader's premultiplied-alpha blend switch (shader.wgsl:214-249),
    // transcribed as the *independent* CPU reference the on-device accumulate test
    // asserts against — no such helper exists elsewhere in the tree (the
    // shader.wgsl:213 comment names a `generate_composite_texture` that doesn't
    // exist). `layer` is `color_a` (the incoming top layer), `accum` is `color_b`
    // (the running accumulation below it). Keep in lockstep with `BlendMode::as_u32`
    // (Over=0, Under=1, Add=2, Multiply=3, Screen=4).
    fn cpu_blend(layer: [f32; 4], accum: [f32; 4], blend: u32) -> [f32; 4] {
        let [ar, ag, ab, aa] = layer;
        let [br, bg, bb, ba] = accum;
        match blend {
            1 => [
                br + ar * (1.0 - ba),
                bg + ag * (1.0 - ba),
                bb + ab * (1.0 - ba),
                ba + aa * (1.0 - ba),
            ], // Under: B over A
            2 => [ar + br, ag + bg, ab + bb, (aa + ba).min(1.0)], // Add
            3 => [ar * br, ag * bg, ab * bb, aa],                 // Multiply (alpha = layer's)
            4 => [
                ar + br - ar * br,
                ag + bg - ag * bg,
                ab + bb - ab * bb,
                aa + ba - aa * ba,
            ], // Screen
            _ => [
                ar + br * (1.0 - aa),
                ag + bg * (1.0 - aa),
                ab + bb * (1.0 - aa),
                aa + ba * (1.0 - aa),
            ], // Over: A over B
        }
    }

    // Neutral scene-linear "accumulate pass" uniforms: every display/exposure knob
    // that would bake into the offscreen is zeroed (srgb=0, gamma=1, enable_lut=0,
    // channel=RGB, skip_checker=1 so the real alpha — not the checker/opacity — is
    // emitted). Exposure is a parameter only so the second assertion can prove the
    // *global* exposure stage is a clean post-composite multiply; the accumulate
    // itself always passes exposure=0.
    #[allow(clippy::too_many_arguments)] // exercises the full accumulate uniform surface
    fn accum_uniforms(
        rect_min: [f32; 2],
        rect_max: [f32; 2],
        screen: [f32; 2],
        is_composite: u32,
        blend: u32,
        composite_accum: u32,
        exposure: f32,
    ) -> crate::gpu::Uniforms {
        crate::gpu::Uniforms {
            rect_min,
            rect_max,
            screen_size: screen,
            wipe_center: [0.0, 0.0],
            display_min: rect_min,
            display_max: rect_max,
            exposure,
            gamma: 1.0,
            diff_multiplier: 1.0,
            opacity: 1.0,
            wipe_angle: 0.0,
            channel_mode: 0,
            is_diff_mode: 0,
            srgb: 0,
            enable_lut: 0,
            is_composite,
            blend_mode: blend,
            is_wipe_mode: 0,
            skip_checker: 1,
            diff_metric: 0,
            diff_floor: 0.0,
            overscan_factor: 1.0,
            lut_domain_min: [0.0, 0.0, 0.0, 0.0],
            lut_domain_max: [1.0, 1.0, 1.0, 1.0],
            bg_checker_dark: [0.1, 0.1, 0.1, 0.0],
            bg_checker_light: [0.2, 0.2, 0.2, 0.0],
            bg_solid: [0.18, 0.18, 0.18, 0.0],
            bg_mode: 0,
            bg_grad_angle: 0.0,
            bg_checker_size: 16.0,
            composite_accum,
        }
    }

    // PR-A.2 (compositing layer stack, #99): the accumulate render seam validated
    // end-to-end on the platform GPU — the plan's highest-risk item (ping-pong
    // blend correctness in scene-linear premultiplied alpha). Builds a real
    // bottom→top layer stack, composites it by ping-ponging through
    // `pipeline_linear` exactly as the layer-stack render will (bottom layer
    // `is_composite=0` over the sentinel clear; each layer above binds the prior
    // accumulation as `tex_b`, itself as `tex_a`, `is_composite=1`,
    // `blend_mode=layer.blend`), reads the Rgba16Float result back, and asserts it
    // against the independent CPU reference above. Semi-transparent layers make the
    // (1 - alpha) coverage terms and operand order (layer over accum) observable.
    //
    // Second assertion: a global exposure applied as a *post-composite* pass is a
    // clean 2^EV multiply on rgb (alpha unchanged). Exposure must live in that
    // global stage, not in the per-layer accumulate — it isn't a global operation
    // once Multiply/Screen are in the stack.
    #[test]
    fn accumulate_composite_on_device() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = match pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping on-device accumulate test");
                return;
            }
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("accumulate-test-device"),
            required_features: wgpu::Features::FLOAT32_FILTERABLE,
            ..Default::default()
        }))
        .expect("request_device");

        // Rgba8Unorm surface format is irrelevant here — we only drive `pipeline_linear`
        // (Rgba16Float offscreen) and reuse GpuState's real bind-group layouts, ring
        // uniform buffer, and default tex/LUT bind groups.
        let gpu = GpuState::new(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);

        // Bottom→top premultiplied scene-linear stack (rgb <= alpha), all
        // semi-transparent. The bottom layer's blend is unused (drawn is_composite=0);
        // the layers above exercise the three blends the plan calls highest-risk.
        use crate::viewer::BlendMode;
        let layers: [([f32; 4], u32); 4] = [
            ([0.20, 0.10, 0.05, 0.50], BlendMode::Over.as_u32()), // base (is_composite=0)
            ([0.30, 0.00, 0.00, 0.60], BlendMode::Over.as_u32()),
            ([0.10, 0.15, 0.10, 0.30], BlendMode::Add.as_u32()),
            ([0.50, 0.40, 0.60, 0.80], BlendMode::Multiply.as_u32()),
        ];

        // Independent CPU reference: bottom is a straight copy, each layer above
        // folds in via cpu_blend(layer, accum, blend).
        let mut cpu = layers[0].0;
        for (rgba, blend) in &layers[1..] {
            cpu = cpu_blend(*rgba, cpu, *blend);
        }

        // 1x1 targets keep readback row-padding trivial: a full-screen quad covers
        // the single pixel, which samples each 1x1 source at its center (nearest).
        let (w, h) = (1u32, 1u32);
        let extent = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };

        // Upload each layer as an exact Rgba32Float source + a tex bind group over
        // the real layout and nearest sampler. Keep every handle alive until submit.
        let mut src_texs = Vec::new();
        let mut src_views = Vec::new();
        let mut src_bgs = Vec::new();
        for (i, (rgba, _)) in layers.iter().enumerate() {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("layer-src"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::bytes_of(rgba),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(16),
                    rows_per_image: Some(1),
                },
                extent,
            );
            src_views.push(tex.create_view(&wgpu::TextureViewDescriptor::default()));
            src_bgs.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer-bg"),
                layout: &gpu.bind_group_layout_tex,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&src_views[i]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                ],
            }));
            src_texs.push(tex);
        }

        // Ping-pong pair: two Rgba16Float scene targets (pipeline_linear's format),
        // each also sampleable as tex_b and copyable for readback.
        let make_scene = |label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let scene_texs = [make_scene("scene0"), make_scene("scene1")];
        let scene_views = [
            scene_texs[0].create_view(&wgpu::TextureViewDescriptor::default()),
            scene_texs[1].create_view(&wgpu::TextureViewDescriptor::default()),
        ];
        let scene_bgs = [0usize, 1].map(|i| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scene-bg"),
                layout: &gpu.bind_group_layout_tex,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&scene_views[i]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                ],
            })
        });

        // Per-draw uniforms into the persistent ring buffer at aligned slot offsets:
        // slots 0..N for the layers, slot N for the global-exposure pass.
        let stride = gpu.uniform_stride;
        // 1x1 target: image fills it, so screen-normalized == image-local uv and the
        // accumulate draws use composite_accum=0 (the screen-normalized path is
        // exercised separately by `accumulate_matches_single_pass_composite_on_device`).
        let full = ([0.0, 0.0], [w as f32, h as f32], [w as f32, h as f32]);
        for (i, (_, blend)) in layers.iter().enumerate() {
            let is_comp = if i == 0 { 0 } else { 1 };
            let u = accum_uniforms(full.0, full.1, full.2, is_comp, *blend, 0, 0.0);
            queue.write_buffer(&gpu.uniform_buffer, i as u64 * stride as u64, bytemuck::bytes_of(&u));
        }
        let exp_slot = layers.len() as u32;
        let ev = 1.0f32;
        let u_exp = accum_uniforms(full.0, full.1, full.2, 0, 0, 0, ev);
        queue.write_buffer(
            &gpu.uniform_buffer,
            exp_slot as u64 * stride as u64,
            bytemuck::bytes_of(&u_exp),
        );

        // Sentinel clear matching production pass 1; each draw is a full-quad REPLACE
        // that covers the single pixel, so the clear value is overwritten under the image.
        let clear = wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: -1.0,
            }),
            store: wgpu::StoreOp::Store,
        };

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        // Accumulate: draw i -> scene[i % 2], reading the prior accumulation from
        // scene[(i + 1) % 2] as tex_b. Separate render passes; wgpu barriers the
        // read-after-write between them.
        for (i, _) in layers.iter().enumerate() {
            let dst = i % 2;
            let src = (i + 1) % 2;
            let tex_b: &wgpu::BindGroup = if i == 0 {
                gpu.default_tex_bind_group.as_ref() // unused when is_composite=0
            } else {
                &scene_bgs[src]
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("accumulate"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene_views[dst],
                    resolve_target: None,
                    depth_slice: None,
                    ops: clear,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
            rp.set_pipeline(&gpu.pipeline_linear);
            rp.set_bind_group(0, &src_bgs[i], &[]);
            rp.set_bind_group(1, tex_b, &[]);
            rp.set_bind_group(2, &gpu.uniform_bind_group, &[i as u32 * stride]);
            rp.set_bind_group(3, gpu.default_lut_bind_group.as_ref(), &[]);
            rp.draw(0..6, 0..1);
        }
        let composite_idx = (layers.len() - 1) % 2;
        let exposed_idx = 1 - composite_idx;

        // Global exposure stage: one pass over the finished composite (is_composite=0,
        // exposure=EV) -> scene[exposed_idx]. Models the post-composite display stage.
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("global-exposure"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene_views[exposed_idx],
                    resolve_target: None,
                    depth_slice: None,
                    ops: clear,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
            rp.set_pipeline(&gpu.pipeline_linear);
            rp.set_bind_group(0, &scene_bgs[composite_idx], &[]); // tex_a = composite
            rp.set_bind_group(1, gpu.default_tex_bind_group.as_ref(), &[]); // tex_b unused
            rp.set_bind_group(2, &gpu.uniform_bind_group, &[exp_slot * stride]);
            rp.set_bind_group(3, gpu.default_lut_bind_group.as_ref(), &[]);
            rp.draw(0..6, 0..1);
        }

        // Read both targets back (bytes_per_row must be 256-aligned).
        let make_readback = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 256,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        };
        let rb_comp = make_readback("rb-composite");
        let rb_exp = make_readback("rb-exposed");
        for (scene_idx, rb) in [(composite_idx, &rb_comp), (exposed_idx, &rb_exp)] {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &scene_texs[scene_idx],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: rb,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(1),
                    },
                },
                extent,
            );
        }
        queue.submit([encoder.finish()]);

        rb_comp.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        rb_exp.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        let decode = |buf: &wgpu::Buffer| -> [f32; 4] {
            let data = buf.slice(..).get_mapped_range();
            let hs: &[u16] = bytemuck::cast_slice(&data[..8]);
            [
                f16_to_f32(hs[0]),
                f16_to_f32(hs[1]),
                f16_to_f32(hs[2]),
                f16_to_f32(hs[3]),
            ]
        };
        let gpu_comp = decode(&rb_comp);
        let gpu_exp = decode(&rb_exp);

        // f16 carries ~3 decimal digits; a handful of chained ops keeps error well
        // under this tolerance.
        let tol = 0.01;
        for c in 0..4 {
            assert!(
                (gpu_comp[c] - cpu[c]).abs() <= tol,
                "composite channel {c}: gpu {gpu_comp:?} vs cpu {cpu:?}"
            );
        }
        let scale = 2f32.powf(ev);
        let expected_exp = [cpu[0] * scale, cpu[1] * scale, cpu[2] * scale, cpu[3]];
        for c in 0..4 {
            assert!(
                (gpu_exp[c] - expected_exp[c]).abs() <= tol * scale,
                "exposed channel {c}: gpu {gpu_exp:?} vs expected {expected_exp:?}"
            );
        }
    }

    // PR-A.3: behavior preservation + the screen-normalized `tex_b` sampling that the
    // live accumulate ping-pong depends on. Two spatially-varying images are composited
    // into an OFF-ORIGIN sub-rect of a screen-sized target two ways — (1) today's
    // single-pass 2-input composite (`is_composite=1, composite_accum=0`, tex_b = the B
    // image), and (2) the ping-pong (bottom B copy → scene_b, then A over scene_b with
    // `composite_accum=1`, tex_b = the screen-sized accumulation) — and asserts they are
    // pixel-identical. This can only hold if the top draw reads the accumulation at the
    // fragment's screen position: sampling it at the image-local uv would read a different
    // (or empty) region of scene_b once the image doesn't fill the viewport, so a wrong
    // uv fails here. Also checks the composited region equals the CPU A-over-B reference
    // (so the test can't pass by both paths being blank).
    #[test]
    fn accumulate_matches_single_pass_composite_on_device() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = match pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping on-device accumulate-parity test");
                return;
            }
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("accumulate-parity-device"),
            required_features: wgpu::Features::FLOAT32_FILTERABLE,
            ..Default::default()
        }))
        .expect("request_device");
        let gpu = GpuState::new(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);

        // 4x2 screen; the image occupies the RIGHT half (off-origin, so screen-normalized
        // and image-local uv diverge). Two 2x2 images with distinct per-texel premultiplied
        // colors so a mis-sampled tex_b reads a wrong value.
        let (sw, sh) = (4u32, 2u32);
        let a_px: [f32; 16] = [
            0.30, 0.10, 0.05, 0.60, 0.20, 0.20, 0.10, 0.50, // row 0: (0,0) (1,0)
            0.10, 0.05, 0.30, 0.55, 0.25, 0.15, 0.15, 0.45, // row 1: (0,1) (1,1)
        ];
        let b_px: [f32; 16] = [
            0.15, 0.20, 0.10, 0.50, 0.05, 0.10, 0.25, 0.40, // row 0
            0.30, 0.05, 0.05, 0.60, 0.10, 0.25, 0.20, 0.55, // row 1
        ];
        let texel = |arr: &[f32; 16], i: usize, j: usize| -> [f32; 4] {
            let o = (j * 2 + i) * 4;
            [arr[o], arr[o + 1], arr[o + 2], arr[o + 3]]
        };

        // 2x2 Rgba32Float image + its tex bind group.
        let mut keep_img = Vec::new(); // hold textures/views alive
        let mut mk_image = |px: &[f32; 16]| -> wgpu::BindGroup {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("img"),
                size: wgpu::Extent3d {
                    width: 2,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::bytes_of(px),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(2 * 16),
                    rows_per_image: Some(2),
                },
                wgpu::Extent3d {
                    width: 2,
                    height: 2,
                    depth_or_array_layers: 1,
                },
            );
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("img-bg"),
                layout: &gpu.bind_group_layout_tex,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                ],
            });
            keep_img.push((tex, view));
            bg
        };
        let a_bg = mk_image(&a_px);
        let b_bg = mk_image(&b_px);

        // Screen-sized Rgba16Float scene targets: ref (single-pass), plus the ping-pong
        // pair (accum result in scene_a, prior accumulation in scene_b).
        let scene_extent = wgpu::Extent3d {
            width: sw,
            height: sh,
            depth_or_array_layers: 1,
        };
        let mk_scene = |label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: scene_extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let scene_ref = mk_scene("scene-ref");
        let scene_a = mk_scene("scene-a");
        let scene_b = mk_scene("scene-b");
        let v_ref = scene_ref.create_view(&wgpu::TextureViewDescriptor::default());
        let v_a = scene_a.create_view(&wgpu::TextureViewDescriptor::default());
        let v_b = scene_b.create_view(&wgpu::TextureViewDescriptor::default());
        let scene_b_texbg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene-b-texb"),
            layout: &gpu.bind_group_layout_tex,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&v_b),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
            ],
        });

        // Image placed at the right half of the screen.
        let rect_min = [(sw / 2) as f32, 0.0];
        let rect_max = [sw as f32, sh as f32];
        let screen = [sw as f32, sh as f32];
        let over = crate::viewer::BlendMode::Over.as_u32();
        // slot 0: single-pass composite (composite_accum=0, tex_b = B image)
        // slot 1: ping-pong bottom (B copy, is_composite=0)
        // slot 2: ping-pong top (A over accumulation, composite_accum=1)
        let stride = gpu.uniform_stride;
        let us = [
            accum_uniforms(rect_min, rect_max, screen, 1, over, 0, 0.0),
            accum_uniforms(rect_min, rect_max, screen, 0, 0, 0, 0.0),
            accum_uniforms(rect_min, rect_max, screen, 1, over, 1, 0.0),
        ];
        for (i, u) in us.iter().enumerate() {
            queue.write_buffer(
                &gpu.uniform_buffer,
                i as u64 * stride as u64,
                bytemuck::bytes_of(u),
            );
        }

        let clear = wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: -1.0,
            }),
            store: wgpu::StoreOp::Store,
        };
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        // (tex_a, tex_b, uniform slot, target) for each of the three draws.
        let passes: [(&wgpu::BindGroup, &wgpu::BindGroup, u32, &wgpu::TextureView); 3] = [
            (&a_bg, &b_bg, 0, &v_ref),          // single-pass reference
            (&b_bg, &b_bg, 1, &v_b),            // ping-pong bottom (tex_b unused)
            (&a_bg, &scene_b_texbg, 2, &v_a),   // ping-pong top (tex_b = accumulation)
        ];
        for (tex_a, tex_b, slot, target) in passes {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("parity"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    depth_slice: None,
                    ops: clear,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_viewport(0.0, 0.0, sw as f32, sh as f32, 0.0, 1.0);
            rp.set_pipeline(&gpu.pipeline_linear);
            rp.set_bind_group(0, tex_a, &[]);
            rp.set_bind_group(1, tex_b, &[]);
            rp.set_bind_group(2, &gpu.uniform_bind_group, &[slot * stride]);
            rp.set_bind_group(3, gpu.default_lut_bind_group.as_ref(), &[]);
            rp.draw(0..6, 0..1);
        }

        // Read scene_ref and scene_a back. Row stride padded to 256; sh=2 rows.
        let rb_bytes = 256u64 * sh as u64;
        let mk_rb = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: rb_bytes,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        };
        let rb_ref = mk_rb("rb-ref");
        let rb_pp = mk_rb("rb-pp");
        for (tex, rb) in [(&scene_ref, &rb_ref), (&scene_a, &rb_pp)] {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: rb,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(sh),
                    },
                },
                scene_extent,
            );
        }
        queue.submit([encoder.finish()]);
        rb_ref.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        rb_pp.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        // pixel (x, y) -> [f32; 4], decoding four halfs at row*256 + x*8.
        let read_px = |buf: &wgpu::Buffer, x: u32, y: u32| -> [f32; 4] {
            let data = buf.slice(..).get_mapped_range();
            let base = (y as usize) * 256 + (x as usize) * 8;
            let hs: &[u16] = bytemuck::cast_slice(&data[base..base + 8]);
            [
                f16_to_f32(hs[0]),
                f16_to_f32(hs[1]),
                f16_to_f32(hs[2]),
                f16_to_f32(hs[3]),
            ]
        };

        let tol = 0.01;
        for y in 0..sh {
            for x in 0..sw {
                let r = read_px(&rb_ref, x, y);
                let p = read_px(&rb_pp, x, y);
                for c in 0..4 {
                    assert!(
                        (r[c] - p[c]).abs() <= tol,
                        "pixel ({x},{y}) chan {c}: single-pass {r:?} vs ping-pong {p:?}"
                    );
                }
                if x >= sw / 2 {
                    // Composited region: must equal the CPU A-over-B reference (and so be
                    // non-sentinel), proving neither path is silently blank.
                    let (i, j) = ((x - sw / 2) as usize, y as usize);
                    let want = cpu_blend(texel(&a_px, i, j), texel(&b_px, i, j), over);
                    for c in 0..4 {
                        assert!(
                            (p[c] - want[c]).abs() <= tol,
                            "pixel ({x},{y}) chan {c}: composite {p:?} vs cpu A-over-B {want:?}"
                        );
                    }
                } else {
                    // Outside the image: both keep the α=−1 no-image sentinel.
                    assert!(r[3] < 0.0 && p[3] < 0.0, "pixel ({x},{y}) should be sentinel");
                }
            }
        }
    }
}
