use bytemuck::{Pod, Zeroable};
use eframe::egui_wgpu::wgpu;
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Uniforms {
    pub rect_min: [f32; 2],
    pub rect_max: [f32; 2],
    pub screen_size: [f32; 2],
    pub exposure: f32,
    pub gamma: f32,
    pub diff_multiplier: f32,
    pub channel_mode: u32,
    pub is_diff_mode: u32,
    pub srgb: u32,
    pub enable_lut: u32,
    pub opacity: f32,
    pub is_composite: u32,
    pub blend_mode: u32,
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
}

impl GpuState {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
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

        // Create a 1x1 black texture for default bind group
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
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

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

        Self {
            pipeline,
            bind_group_layout_tex,
            bind_group_layout_uniform,
            bind_group_layout_lut,
            default_tex_bind_group,
            default_lut_bind_group,
            sampler,
            lut_sampler,
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
            size, 64,
            "Uniforms layout changed — update shader.wgsl to match"
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
