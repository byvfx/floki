//! OCIO display-transform render pass for floki.
//!
//! Turns a `floki_ocio::GpuShaderBundle` (SPIR-V fragment + reflected bindings + LUTs) into a
//! wgpu render pipeline that samples a scene-linear input texture and writes the display-
//! transformed result. This is "pass 2" of the two-pass design; the existing WGSL pipeline
//! ("pass 1") composites + exposes into the offscreen input this pass reads.
//!
//! Binding convention (authored in `floki-ocio`'s transpiler, matched here):
//!   * set 1: binding 0 = scene input texture, binding 1 = scene sampler.
//!   * set 0: binding 2*i = LUT texture i, binding 2*i+1 = its sampler.
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
        let max_group = bundle.bindings.iter().map(|b| b.group).max().unwrap_or(1).max(1);
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
            group_layouts.push(device.create_bind_group_layout(
                &wgpu::BindGroupLayoutDescriptor {
                    label: Some("OCIO bind group layout"),
                    entries: &entries,
                },
            ));
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
    /// into `output_view`.
    pub fn render(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        input_view: &wgpu::TextureView,
        output_view: &wgpu::TextureView,
    ) {
        let set1 = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("OCIO scene bind group"),
            layout: &self.group_layouts[1],
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.scene_sampler),
                },
            ],
        });

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
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.set0_bind_group, &[]);
        rp.set_bind_group(1, &set1, &[]);
        rp.draw(0..3, 0..1);
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

    fn default_request(
        cfg: &floki_ocio::OcioConfig,
    ) -> floki_ocio::DisplayTransformRequest {
        let input_colorspace = cfg
            .scene_linear_colorspace()
            .or_else(|| cfg.color_spaces().into_iter().find(|c| !c.is_data).map(|c| c.name))
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
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            {
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
        pass.render(&device, &mut encoder, &input_view, &output_view);
        queue.submit([encoder.finish()]);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }
}
