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
            .unwrap_or(1)
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
    pub fn render(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        input_view: &wgpu::TextureView,
        output_view: &wgpu::TextureView,
        scissor: Option<[u32; 4]>,
    ) {
        // Use the cached scene bind group if available; otherwise build it
        // (and cache via interior mutability — the cache is populated on the
        // first dirty frame after OcioTargets creation).
        let cached = self
            .scene_bind_group
            .as_ref()
            .expect("scene_bind_group must be initialized via set_scene_bind_group before render");

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
        rp.set_bind_group(1, cached, &[]);
        rp.draw(0..3, 0..1);
    }
}

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
    /// recreates `OcioTargets`), so this is built once in `set_scene_bind_group`
    /// and reused — eliminates a per-dirty-frame `create_bind_group`.
    scene_bind_group: Option<wgpu::BindGroup>,
    /// `render_sig` of the content currently in `display_view`; lets `prepare` skip the
    /// two passes when nothing changed. `None` after (re)creation forces a first render.
    last_render_sig: Option<u64>,
    _scene: wgpu::Texture,
    _display: wgpu::Texture,
}

impl Drop for OcioTargets {
    fn drop(&mut self) {
        // Explicitly destroy GPU textures so memory is released in the current
        // submission cycle, not deferred to the next driver GC sweep. On a
        // window-resize drag loop this prevents a memory spike (each resize
        // creates ~83 MB of 4K Rgba16Float + display textures).
        self._scene.destroy();
        self._display.destroy();
    }

    /// Build and cache the scene-input bind group (set 1) on `targets`. Uses
    /// `self.group_layouts[1]` + `self.scene_sampler` + `targets.scene_view`.
    /// Called once after `OcioTargets` creation so `render` doesn't recreate
    /// the bind group every dirty frame.
    pub fn init_scene_bind_group(&self, device: &wgpu::Device, targets: &mut OcioTargets) {
        targets.scene_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO scene bind group (cached)"),
            layout: &self.group_layouts[1],
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&targets.scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.scene_sampler),
                },
            ],
        }));
    }
}

impl OcioTargets {
    fn new(
        device: &wgpu::Device,
        blit_layout: &wgpu::BindGroupLayout,
        blit_sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
        display_format: wgpu::TextureFormat,
    ) -> Self {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let scene = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("OCIO scene-linear target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Rgba16Float: half the bandwidth of 32F, ample range for viewing. Must match
            // `GpuState::pipeline_linear`'s color target format in gpu/mod.rs.
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
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
        let display_view = display.create_view(&wgpu::TextureViewDescriptor::default());
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
            ],
        });
        Self {
            width,
            height,
            scene_view,
            display_view,
            blit_bind_group,
            blit_uniform_buffer,
            scene_bind_group: None,
            last_render_sig: None,
            _scene: scene,
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
            let (blit_layout, blit_sampler) = {
                let gpu = callback_resources.get::<GpuState>().unwrap();
                (gpu.blit_layout.clone(), gpu.blit_sampler.clone())
            };
            let targets = OcioTargets::new(
                device,
                &blit_layout,
                &blit_sampler,
                w,
                h,
                self.display_format,
            );
            callback_resources.insert(targets);
        }

        // Per-frame blit params (display window, overscan dim, checker) — written every
        // frame so `paint` (which has no queue) can just bind the existing buffer.
        {
            let targets = callback_resources.get::<OcioTargets>().unwrap();
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
        if callback_resources
            .get::<OcioTargets>()
            .unwrap()
            .scene_bind_group
            .is_none()
        {
            let ocio = callback_resources.get::<OcioGpuPass>().unwrap();
            let targets = callback_resources.get_mut::<OcioTargets>().unwrap();
            ocio.init_scene_bind_group(device, targets);
        }

        // Skip the two passes when nothing affecting the render changed; `paint` re-blits the
        // cached display_view, so hover / menu / animation repaints stay cheap.
        let dirty = callback_resources
            .get::<OcioTargets>()
            .unwrap()
            .last_render_sig
            != Some(self.render_sig);
        if !dirty {
            return Vec::new();
        }

        let cmd = {
            let gpu = callback_resources.get::<GpuState>().unwrap();
            let ocio = callback_resources.get::<OcioGpuPass>().unwrap();
            let targets = callback_resources.get::<OcioTargets>().unwrap();

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("OCIO"),
            });

            // Pass 1: composite + exposure into scene-linear (uniforms set srgb=0/gamma=1/lut=0/
            // skip_checker=1). Each draw maps its own rect via the vertex shader, so two
            // side-by-side draws land in their sub-rects within the one offscreen. Alpha is
            // cleared to a negative sentinel: the blit treats scene_a < 0 as "no image" so the
            // checker only shows under drawn pixels (matching the non-OCIO path).
            {
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("OCIO pass 1 (scene-linear)"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.scene_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: -1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
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
                device,
                &mut encoder,
                &targets.scene_view,
                &targets.display_view,
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
        let Some(gpu) = callback_resources.get::<GpuState>() else {
            return;
        };
        let Some(targets) = callback_resources.get::<OcioTargets>() else {
            return;
        };
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

#[cfg(all(test, feature = "ocio"))]
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
        pass.render(&device, &mut encoder, &input_view, &output_view, None);
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
            checker_dark: 0.1,
            checker_light: 0.2,
            checker_size: 3.0,
            checker_enabled: 1.0,
            overscan_factor: 0.5,
            _pad0: 0.0,
        };
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
}
