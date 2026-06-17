use bytemuck::{Pod, Zeroable};
use eframe::egui_wgpu::wgpu;
use std::sync::Arc;

#[cfg(feature = "ocio")]
pub mod ocio_pass;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Uniforms {
    // 8-byte aligned fields first (required by WGSL vec2<f32>)
    pub rect_min: [f32; 2],
    pub rect_max: [f32; 2],
    pub screen_size: [f32; 2],
    pub wipe_center: [f32; 2],
    // 4-byte aligned fields
    pub exposure: f32,
    pub gamma: f32,
    pub diff_multiplier: f32,
    pub opacity: f32,
    pub wipe_angle: f32,
    pub channel_mode: u32,
    pub is_diff_mode: u32,
    pub srgb: u32,
    pub enable_lut: u32,
    pub is_composite: u32,
    pub blend_mode: u32,
    pub is_wipe_mode: u32,
    /// When 1, pass 1 skips the checkerboard composite and emits the real image
    /// alpha (the OCIO path composites the checker in display space afterwards).
    /// Keep in lockstep with `Uniforms.skip_checker` in `shader.wgsl`.
    pub skip_checker: u32,
    pub _pad0: u32,
    pub _pad1: u32,
    pub _pad2: u32,
    /// `.cube` LUT domain bounds (xyz + pad). The lookup coordinate is remapped
    /// from `[domain_min, domain_max]` to `[0, 1]` before sampling the 3D LUT
    /// texture, so non-unit-domain LUTs (common for HDR/film looks) sample
    /// correctly. Defaults to `[0,0,0,0]` / `[1,1,1,1]` (identity). Keep in
    /// lockstep with `Uniforms.lut_domain_min/max` in `shader.wgsl`.
    pub lut_domain_min: [f32; 4],
    pub lut_domain_max: [f32; 4],
}

/// Uniforms for the OCIO blit pass: composites the transparency checkerboard in
/// display space (after the OCIO transform, so neutral grey stays neutral) and
/// applies the overscan dim factor outside the display window. All rects/sizes are
/// in egui *points* (the same unit as `Uniforms.screen_size` / `rect_min`), so the
/// 16-point checker and the display-window boundary match the non-OCIO path on HiDPI.
#[cfg(feature = "ocio")]
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BlitUniforms {
    pub display_min: [f32; 2],
    pub display_max: [f32; 2],
    pub screen_size: [f32; 2],
    pub checker_dark: f32,
    pub checker_light: f32,
    pub checker_size: f32,
    pub checker_enabled: f32,
    pub overscan_factor: f32,
    pub _pad0: f32,
}

pub struct GpuState {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group_layout_tex: wgpu::BindGroupLayout,
    pub bind_group_layout_uniform: wgpu::BindGroupLayout,
    pub bind_group_layout_lut: wgpu::BindGroupLayout,
    pub default_tex_bind_group: Arc<wgpu::BindGroup>,
    pub default_lut_bind_group: Arc<wgpu::BindGroup>,
    pub sampler: wgpu::Sampler,
    pub lut_sampler: wgpu::Sampler,
    /// Same shader/layout as `pipeline` but renders into an `Rgba32Float` offscreen target
    /// (the OCIO "pass 1" scene-linear buffer). Drive it with `srgb=0, gamma=1, enable_lut=0`
    /// so it emits linear color for the OCIO display transform.
    #[cfg(feature = "ocio")]
    pub pipeline_linear: wgpu::RenderPipeline,
    /// Blits the OCIO display texture into egui's render pass (OCIO "paint").
    #[cfg(feature = "ocio")]
    pub blit_pipeline: wgpu::RenderPipeline,
    #[cfg(feature = "ocio")]
    pub blit_layout: wgpu::BindGroupLayout,
    #[cfg(feature = "ocio")]
    pub blit_sampler: wgpu::Sampler,
}

#[cfg(feature = "ocio")]
const BLIT_SHADER: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    var c = array<vec2<f32>, 3>(vec2<f32>(-1.0,-1.0), vec2<f32>(3.0,-1.0), vec2<f32>(-1.0,3.0));
    let xy = c[vi];
    var o: VOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}
struct BlitUniforms {
    display_min: vec2<f32>,
    display_max: vec2<f32>,
    screen_size: vec2<f32>,
    checker_dark: f32,
    checker_light: f32,
    checker_size: f32,
    checker_enabled: f32,
    overscan_factor: f32,
    _pad0: f32,
};
@group(0) @binding(0) var t: texture_2d<f32>;       // OCIO display-transformed color
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var scene_t: texture_2d<f32>; // pre-OCIO scene-linear (for alpha/coverage)
@group(0) @binding(3) var<uniform> bu: BlitUniforms;
@fragment
fn fs_main(i: VOut) -> @location(0) vec4<f32> {
    // Pass 1 clears the scene target's alpha to a negative sentinel; the image quad(s)
    // write alpha in [0,1]. So scene_a < 0 means "no image here" -> show nothing (the
    // egui panel background), matching the non-OCIO path where the checker only appears
    // under the image.
    let scene_a = textureSample(scene_t, s, i.uv).a;
    if scene_a < 0.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    let disp = textureSample(t, s, i.uv);
    let a = clamp(scene_a, 0.0, 1.0);

    // Display-space checkerboard (16-point cells), composited AFTER OCIO so the grey
    // checker is not color-managed.
    var rgb = disp.rgb;
    if bu.checker_enabled > 0.5 {
        let screen_pt = i.uv * bu.screen_size;
        let cx = u32(screen_pt.x / bu.checker_size);
        let cy = u32(screen_pt.y / bu.checker_size);
        let is_dark = (cx + cy) % 2u == 0u;
        let bg = select(bu.checker_light, bu.checker_dark, is_dark);
        rgb = rgb + vec3<f32>(bg) * (1.0 - a);
    }

    // Overscan dim: multiply by the dim factor where the pixel is outside the display
    // window (data-window overscan region).
    let screen_pt2 = i.uv * bu.screen_size;
    let inside = screen_pt2.x >= bu.display_min.x && screen_pt2.x <= bu.display_max.x
              && screen_pt2.y >= bu.display_min.y && screen_pt2.y <= bu.display_max.y;
    let dim = select(bu.overscan_factor, 1.0, inside);
    rgb = rgb * dim;

    return vec4<f32>(rgb, 1.0);
}
"#;

impl GpuState {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Exr Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout_tex =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Texture Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            });

        let bind_group_layout_uniform =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Uniform Bind Group Layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group_layout_lut =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("LUT Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D3,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Exr Pipeline Layout"),
            bind_group_layouts: &[
                Some(&bind_group_layout_tex),     // tex_a
                Some(&bind_group_layout_tex),     // tex_b
                Some(&bind_group_layout_uniform), // uniforms
                Some(&bind_group_layout_lut),     // lut
            ],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Exr Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let lut_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let default_lut_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Default LUT"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Explicitly zero the 1x1x1 LUT texel. wgpu does not guarantee
        // zero-initialization on all backends (Vulkan leaves texture memory
        // undefined); sampling garbage RGBA32Float (possibly NaN/Inf) into the
        // exposure/LUT chain would silently corrupt the output when the LUT is
        // disabled but the bind group is still bound.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &default_lut_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8; 16],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let default_lut_view = default_lut_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let default_lut_bind_group =
            Arc::new(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Default LUT Bind Group"),
                layout: &bind_group_layout_lut,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&default_lut_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&lut_sampler),
                    },
                ],
            }));

        // Create a 1x1 black texture for default bind group. COPY_DST is needed
        // to explicitly zero the texel — wgpu does not guarantee zero-initialization
        // on all backends (Vulkan leaves it undefined), and sampling garbage
        // RGBA32Float (possibly NaN/Inf) when image B is unset would corrupt output.
        let default_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Default Texture"),
            size: wgpu::Extent3d {
                width: 1,
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
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &default_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8; 16],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        let default_view = default_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let default_tex_bind_group =
            Arc::new(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Default Texture Bind Group"),
                layout: &bind_group_layout_tex,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&default_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            }));

        #[cfg(feature = "ocio")]
        let pipeline_linear = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Exr Linear Offscreen Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    // Rgba16Float scene-linear offscreen: half the bandwidth of 32F for the
                    // OCIO pass (write here, sampled by the display transform) and ample range
                    // for viewing (half-float reaches 65504). Not blendable, but pass 1 is a
                    // single full-quad draw into a cleared target, so no blending is needed.
                    // Must match the OCIO scene target format in ocio_pass.rs.
                    format: wgpu::TextureFormat::Rgba16Float,
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

        #[cfg(feature = "ocio")]
        let (blit_pipeline, blit_layout, blit_sampler) = {
            let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Blit Shader"),
                source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
            });
            let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Blit Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                    // Pre-OCIO scene-linear texture, sampled only for its alpha (coverage
                    // + the post-OCIO checker composite).
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
            let blit_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Blit Pipeline Layout"),
                bind_group_layouts: &[Some(&blit_layout)],
                immediate_size: 0,
            });
            let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Blit Pipeline"),
                layout: Some(&blit_pl_layout),
                vertex: wgpu::VertexState {
                    module: &blit_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &blit_shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
            let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("Blit Sampler"),
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            });
            (blit_pipeline, blit_layout, blit_sampler)
        };

        Self {
            pipeline,
            bind_group_layout_tex,
            bind_group_layout_uniform,
            bind_group_layout_lut,
            default_tex_bind_group,
            default_lut_bind_group,
            sampler,
            lut_sampler,
            #[cfg(feature = "ocio")]
            pipeline_linear,
            #[cfg(feature = "ocio")]
            blit_pipeline,
            #[cfg(feature = "ocio")]
            blit_layout,
            #[cfg(feature = "ocio")]
            blit_sampler,
        }
    }

    pub fn create_lut_bind_group(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lut: &crate::color::cube::CubeLut,
    ) -> Arc<wgpu::BindGroup> {
        let size = wgpu::Extent3d {
            width: lut.size as u32,
            height: lut.size as u32,
            depth_or_array_layers: lut.size as u32,
        };

        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("LUT Texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
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
            bytemuck::cast_slice(&lut.data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(lut.size as u32 * 16),
                rows_per_image: Some(lut.size as u32),
            },
            size,
        );

        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        Arc::new(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("LUT Bind Group"),
            layout: &self.bind_group_layout_lut,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.lut_sampler),
                },
            ],
        }))
    }
}

pub struct ExrCallback {
    pub bg_a: Arc<wgpu::BindGroup>,
    pub bg_b: Arc<wgpu::BindGroup>,
    pub uniform_bg: wgpu::BindGroup,
    pub lut_bg: Arc<wgpu::BindGroup>,
}

impl eframe::egui_wgpu::CallbackTrait for ExrCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen_descriptor: &eframe::egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut eframe::egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        Vec::new()
    }

    fn paint(
        &self,
        info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &eframe::egui_wgpu::CallbackResources,
    ) {
        let gpu_state = callback_resources.get::<GpuState>().unwrap();

        // egui_wgpu sets the viewport to the primitive's bounding box, which squishes our quad!
        // We override it to the full physical screen so our shader's screen-space math works perfectly.
        render_pass.set_viewport(
            0.0,
            0.0,
            info.screen_size_px[0] as f32,
            info.screen_size_px[1] as f32,
            0.0,
            1.0,
        );

        render_pass.set_pipeline(&gpu_state.pipeline);
        render_pass.set_bind_group(0, self.bg_a.as_ref(), &[]);
        render_pass.set_bind_group(1, self.bg_b.as_ref(), &[]);
        render_pass.set_bind_group(2, &self.uniform_bg, &[]);
        render_pass.set_bind_group(3, self.lut_bg.as_ref(), &[]);
        render_pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniforms_size_is_16_byte_aligned() {
        // WGSL uniform buffers require the struct size to be a multiple of 16
        // bytes; the explicit `pad*` fields exist solely to guarantee that.
        // Keep this in lockstep with the uniform struct in `shader.wgsl`.
        let size = std::mem::size_of::<Uniforms>();
        assert_eq!(
            size % 16,
            0,
            "Uniforms size ({size}) must be a multiple of 16"
        );
        assert_eq!(
            size, 128,
            "Uniforms layout changed — update shader.wgsl to match"
        );
    }

    #[cfg(feature = "ocio")]
    #[test]
    fn blit_uniforms_size_is_16_byte_aligned() {
        // The OCIO blit uniform buffer must be a multiple of 16 bytes (WGSL uniform rule).
        // Keep in lockstep with the `BlitUniforms` struct in `BLIT_SHADER`.
        let size = std::mem::size_of::<BlitUniforms>();
        assert_eq!(
            size % 16,
            0,
            "BlitUniforms size ({size}) must be a multiple of 16"
        );
        assert_eq!(
            size, 48,
            "BlitUniforms layout changed — update BLIT_SHADER to match"
        );
    }

    #[test]
    fn uniforms_round_trip_through_bytes() {
        // Proves the `Pod`/`Zeroable` derives are sound: what we upload is what
        // the shader receives, byte for byte.
        let u = Uniforms {
            rect_min: [1.0, 2.0],
            rect_max: [3.0, 4.0],
            screen_size: [800.0, 600.0],
            exposure: 1.5,
            gamma: 2.2,
            diff_multiplier: 4.0,
            channel_mode: 3,
            is_diff_mode: 1,
            srgb: 1,
            enable_lut: 0,
            opacity: 0.5,
            is_composite: 1,
            blend_mode: 2,
            is_wipe_mode: 1,
            wipe_center: [0.5, 0.5],
            wipe_angle: 0.0,
            skip_checker: 1,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
            lut_domain_min: [-0.5, -0.5, -0.5, 0.0],
            lut_domain_max: [1.5, 1.5, 1.5, 0.0],
        };
        let bytes = bytemuck::bytes_of(&u);
        assert_eq!(bytes.len(), std::mem::size_of::<Uniforms>());

        let back: &Uniforms = bytemuck::from_bytes(bytes);
        assert_eq!(back.exposure, 1.5);
        assert_eq!(back.gamma, 2.2);
        assert_eq!(back.diff_multiplier, 4.0);
        assert_eq!(back.channel_mode, 3);
        assert_eq!(back.is_diff_mode, 1);
        assert_eq!(back.srgb, 1);
        assert_eq!(back.enable_lut, 0);
        assert_eq!(back.opacity, 0.5);
        assert_eq!(back.is_composite, 1);
        assert_eq!(back.blend_mode, 2);
        assert_eq!(back.screen_size, [800.0, 600.0]);
        assert_eq!(back.skip_checker, 1);
        assert_eq!(back.lut_domain_min, [-0.5, -0.5, -0.5, 0.0]);
        assert_eq!(back.lut_domain_max, [1.5, 1.5, 1.5, 0.0]);
    }

    #[test]
    fn channel_mode_encoding_matches_shader_contract() {
        // The single source of truth (`ChannelMode::as_u32`) must keep emitting
        // the values the shader's `channel_mode` switch expects.
        use crate::viewer::ChannelMode;
        assert_eq!(ChannelMode::RGB.as_u32(), 0);
        assert_eq!(ChannelMode::R.as_u32(), 1);
        assert_eq!(ChannelMode::G.as_u32(), 2);
        assert_eq!(ChannelMode::B.as_u32(), 3);
        assert_eq!(ChannelMode::A.as_u32(), 4);
    }

    #[test]
    fn blend_mode_encoding_matches_shader_contract() {
        // The single source of truth (`BlendMode::as_u32`) must keep emitting the
        // values the shader's `blend_mode` switch expects.
        use crate::viewer::BlendMode;
        assert_eq!(BlendMode::Over.as_u32(), 0);
        assert_eq!(BlendMode::Under.as_u32(), 1);
        assert_eq!(BlendMode::Add.as_u32(), 2);
        assert_eq!(BlendMode::Multiply.as_u32(), 3);
        assert_eq!(BlendMode::Screen.as_u32(), 4);
    }
}
