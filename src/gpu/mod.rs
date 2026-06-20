use bytemuck::{Pod, Zeroable};
use eframe::egui_wgpu::wgpu;
use std::sync::Arc;

#[cfg(feature = "ocio")]
pub mod ocio_pass;

pub mod resources;

pub use resources::GpuResources;

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
    /// Diff visualization controls (only read by the shader when `is_diff_mode`).
    /// `diff_metric` encodes `gradient::DiffMetric` (MaxChannel=0, Luminance=1,
    /// PerChannelRGB=2); `diff_floor` is a noise floor subtracted from the gained
    /// magnitude. Keep in lockstep with `Uniforms` in `shader.wgsl`.
    pub diff_metric: u32,
    pub diff_floor: f32,
    pub _pad2: u32,
    /// `.cube` LUT domain bounds (xyz + pad). The lookup coordinate is remapped
    /// from `[domain_min, domain_max]` to `[0, 1]` before sampling the 3D LUT
    /// texture, so non-unit-domain LUTs (common for HDR/film looks) sample
    /// correctly. Defaults to `[0,0,0,0]` / `[1,1,1,1]` (identity). Keep in
    /// lockstep with `Uniforms.lut_domain_min/max` in `shader.wgsl`.
    pub lut_domain_min: [f32; 4],
    pub lut_domain_max: [f32; 4],
    /// Customizable viewport background (issue #18). Linear-space colours (xyz;
    /// w unused), composited where image alpha < 1 — see `background_color` in
    /// `shader.wgsl`. `bg_mode` encodes `background::BackgroundMode`
    /// (Checkerboard=0, Solid=1, Gradient=2). The gradient ramp itself is a 256×1
    /// LUT (`bg_gradient_texture`), sampled along `bg_grad_angle`. Defaults
    /// reproduce the historical grey checker. Keep in lockstep with `shader.wgsl`.
    pub bg_checker_dark: [f32; 4],
    pub bg_checker_light: [f32; 4],
    pub bg_solid: [f32; 4],
    pub bg_mode: u32,
    pub bg_grad_angle: f32,
    pub bg_checker_size: f32,
    pub _pad3: u32,
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
    pub overscan_factor: f32,
    /// Customizable viewport background (issue #18), composited here in *display*
    /// space (post-OCIO, not colour-managed). `bg_mode`: Checkerboard=0, Solid=1,
    /// Gradient=2 (`background::BackgroundMode::as_u32`, stored as f32 to keep this
    /// an all-f32 / 16-byte-aligned struct). The gradient ramp is the shared
    /// `bg_gradient_texture` (group(0) binding 4 in the blit). Keep in lockstep
    /// with the `BlitUniforms` mirror in `BLIT_SHADER`.
    pub bg_mode: f32,
    pub bg_checker_size: f32,
    pub bg_grad_angle: f32,
    pub _pad_a: f32,
    pub _pad_b: f32,
    pub bg_checker_dark: [f32; 4],
    pub bg_checker_light: [f32; 4],
    pub bg_solid: [f32; 4],
}

pub struct GpuState {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group_layout_tex: wgpu::BindGroupLayout,
    /// Kept on the struct for potential future use; the ring-buffer bind group
    /// (`uniform_bind_group` below) is what the paint callbacks actually use.
    #[allow(dead_code)]
    pub bind_group_layout_uniform: wgpu::BindGroupLayout,
    pub bind_group_layout_lut: wgpu::BindGroupLayout,
    pub default_tex_bind_group: Arc<wgpu::BindGroup>,
    pub default_lut_bind_group: Arc<wgpu::BindGroup>,
    pub sampler: wgpu::Sampler,
    pub lut_sampler: wgpu::Sampler,
    /// Persistent `256x1` RGBA8 diff colormap LUT. Bound into every group(3)
    /// bind group (alongside the 3D look LUT) and updated *in place* via
    /// [`GpuState::write_colormap`] when the active gradient changes — the texture
    /// handle is stable, so the bind groups never need rebuilding. Initialised to
    /// the black-body ramp (the default colormap) so diff renders correctly before
    /// any update.
    pub colormap_texture: wgpu::Texture,
    /// Persistent `256x1` RGBA8 background gradient LUT (issue #18), updated in
    /// place via [`GpuState::write_bg_gradient`]. Shares group(3) and the colormap
    /// sampler. Seeded with the default dark→light grey ramp.
    pub bg_gradient_texture: wgpu::Texture,
    /// Persistent uniform ring buffer (sized `UNIFORM_RING_SLOTS *
    /// uniform_stride`). Per-draw uniform data is written via
    /// `queue.write_buffer` at a dynamic offset, eliminating the per-frame
    /// `create_buffer_init` + `create_bind_group` that previously ran 1–4× per
    /// frame. The bind group is created once and rebound with a dynamic offset.
    pub uniform_buffer: wgpu::Buffer,
    pub uniform_bind_group: wgpu::BindGroup,
    /// Stride per uniform slot in bytes, padded up to the device's
    /// `min_uniform_buffer_offset_alignment` (typically 256). The raw
    /// `Uniforms` struct is 128 bytes, but dynamic offsets must be aligned,
    /// so each slot is padded.
    pub uniform_stride: u32,
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
    overscan_factor: f32,
    bg_mode: f32,
    bg_checker_size: f32,
    bg_grad_angle: f32,
    _pad_a: f32,
    _pad_b: f32,
    bg_checker_dark: vec4<f32>,
    bg_checker_light: vec4<f32>,
    bg_solid: vec4<f32>,
};
@group(0) @binding(0) var t: texture_2d<f32>;       // OCIO display-transformed color
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var scene_t: texture_2d<f32>; // pre-OCIO scene-linear (for alpha/coverage)
@group(0) @binding(3) var<uniform> bu: BlitUniforms;
@group(0) @binding(4) var bg_grad_t: texture_2d<f32>; // shared 256x1 background gradient LUT

// Display-space background colour. Mirrors `background_color` in shader.wgsl and
// `Background::sample_linear` in src/background.rs (kept in lockstep). `screen_pt`
// is in screen pixels (checker tiling); `guv` is normalized across the display
// window (gradient direction).
fn blit_background(screen_pt: vec2<f32>, guv: vec2<f32>) -> vec3<f32> {
    if bu.bg_mode > 1.5 {
        let a = radians(bu.bg_grad_angle);
        let d = vec2<f32>(cos(a), sin(a));
        let pmin = min(d.x, 0.0) + min(d.y, 0.0);
        let pmax = max(d.x, 0.0) + max(d.y, 0.0);
        let p = guv.x * d.x + guv.y * d.y;
        let tt = clamp((p - pmin) / max(pmax - pmin, 1e-4), 0.0, 1.0);
        return textureSampleLevel(bg_grad_t, s, vec2<f32>(tt, 0.5), 0.0).rgb;
    }
    if bu.bg_mode > 0.5 {
        return bu.bg_solid.rgb;
    }
    let size = max(bu.bg_checker_size, 1.0);
    let cx = floor(screen_pt.x / size);
    let cy = floor(screen_pt.y / size);
    let is_dark = (i32(cx) + i32(cy)) % 2 == 0;
    return select(bu.bg_checker_light.rgb, bu.bg_checker_dark.rgb, is_dark);
}

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

    // Background (checker / solid / gradient), composited AFTER OCIO in display
    // space so neutral grey stays neutral (not colour-managed).
    var rgb = disp.rgb;
    {
        let screen_pt = i.uv * bu.screen_size;
        let guv = (screen_pt - bu.display_min) / max(bu.display_max - bu.display_min, vec2<f32>(1.0));
        let bg = blit_background(screen_pt, guv);
        rgb = rgb + bg * (1.0 - a);
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

/// Number of uniform slots in the persistent ring buffer. Up to 4 draws per
/// frame (A/B/diff/composite/side-by-side) — 16 gives ample headroom. At
/// 256-byte stride (worst-case alignment) the buffer is 4 KB.
pub const UNIFORM_RING_SLOTS: u64 = 16;

/// Round `size` up to the next multiple of `align` (must be a power of two).
fn align_to(size: u32, align: u32) -> u32 {
    (size + align - 1) & !(align - 1)
}

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
                        has_dynamic_offset: true,
                        min_binding_size: std::num::NonZeroU64::new(
                            std::mem::size_of::<Uniforms>() as u64,
                        ),
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
                    // 256x1 diff colormap LUT (+ filtering sampler). Shares this
                    // group because we are at the 4-bind-group limit.
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    // 256x1 background gradient LUT (issue #18); reuses the
                    // colormap/LUT filtering sampler at binding 3.
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
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

        // Persistent uniform ring buffer: one buffer + one bind group, reused
        // across all draws via dynamic offsets. Eliminates the per-draw
        // `create_buffer_init` + `create_bind_group` that previously ran 1–4×
        // per frame. Each slot is padded to the device's
        // `min_uniform_buffer_offset_alignment` (typically 256) so dynamic
        // offsets are always valid.
        let align = device.limits().min_uniform_buffer_offset_alignment;
        let uniform_stride = align_to(std::mem::size_of::<Uniforms>() as u32, align);
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Uniform Ring Buffer"),
            size: UNIFORM_RING_SLOTS * uniform_stride as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Uniform Ring Bind Group"),
            layout: &bind_group_layout_uniform,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniform_buffer,
                    offset: 0,
                    // Bind a single Uniforms-sized window; the dynamic offset
                    // passed at `set_bind_group` slides this window across the
                    // ring buffer. Must NOT use `as_entire_binding()` (size =
                    // None) — wgpu requires that offset + bound_size <= buffer
                    // size, and with the whole buffer bound any offset > 0
                    // overruns.
                    size: std::num::NonZeroU64::new(std::mem::size_of::<Uniforms>() as u64),
                }),
            }],
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

        // Persistent 256x1 LUTs (diff colormap + background gradient), each seeded
        // with its default ramp and updated in place via `write_colormap` /
        // `write_bg_gradient`. `lut_sampler` (linear, clamp-to-edge) doubles as
        // their sampler.
        let make_lut_texture = |label: &str| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: crate::gradient::COLORMAP_LUT_SIZE as u32,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let colormap_texture = make_lut_texture("Diff Colormap LUT");
        write_lut_row(
            queue,
            &colormap_texture,
            &crate::gradient::Colormap::BlackBody
                .gradient()
                .bake(crate::gradient::COLORMAP_LUT_SIZE),
        );
        let colormap_view = colormap_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bg_gradient_texture = make_lut_texture("Background Gradient LUT");
        write_lut_row(
            queue,
            &bg_gradient_texture,
            &crate::background::default_gradient().bake(crate::gradient::COLORMAP_LUT_SIZE),
        );
        let bg_gradient_view =
            bg_gradient_texture.create_view(&wgpu::TextureViewDescriptor::default());

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
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&colormap_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&lut_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(&bg_gradient_view),
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
                    // Shared 256x1 background gradient LUT (sampled with the
                    // non-filtering blit sampler at binding 1).
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
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
            colormap_texture,
            bg_gradient_texture,
            uniform_buffer,
            uniform_bind_group,
            uniform_stride,
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
    ) -> (Arc<wgpu::BindGroup>, wgpu::Texture) {
        let (lut_size, lut_bytes) = lut.as_rgba_bytes();
        let size = wgpu::Extent3d {
            width: lut_size,
            height: lut_size,
            depth_or_array_layers: lut_size,
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
            lut_bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(lut_size * 16),
                rows_per_image: Some(lut_size),
            },
            size,
        );

        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let colormap_view = self
            .colormap_texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bg_gradient_view = self
            .bg_gradient_texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let bg = Arc::new(device.create_bind_group(&wgpu::BindGroupDescriptor {
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
                // The shared diff colormap + background gradient LUTs travel with
                // every group(3) bind group; both are updated in place so these
                // views stay valid.
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&colormap_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&bg_gradient_view),
                },
            ],
        }));
        (bg, tex)
    }

    /// Upload a freshly baked diff colormap into the persistent colormap texture.
    /// `rgba` must be `COLORMAP_LUT_SIZE * 4` bytes (the output of
    /// [`crate::gradient::Gradient::bake`]). Cheap (~1 KB) — called only when the
    /// active gradient changes.
    pub fn write_colormap(&self, queue: &wgpu::Queue, rgba: &[u8]) {
        write_lut_row(queue, &self.colormap_texture, rgba);
    }

    /// Upload a freshly baked background gradient into its persistent texture.
    /// Same contract as [`Self::write_colormap`].
    pub fn write_bg_gradient(&self, queue: &wgpu::Queue, rgba: &[u8]) {
        write_lut_row(queue, &self.bg_gradient_texture, rgba);
    }
}

/// Write a baked `COLORMAP_LUT_SIZE × 1` RGBA8 LUT row into `tex`. Shared by the
/// colormap and background-gradient textures (seed + updates).
fn write_lut_row(queue: &wgpu::Queue, tex: &wgpu::Texture, rgba: &[u8]) {
    let width = crate::gradient::COLORMAP_LUT_SIZE as u32;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
}

pub struct ExrCallback {
    pub bg_a: Arc<wgpu::BindGroup>,
    pub bg_b: Arc<wgpu::BindGroup>,
    /// Dynamic offset into `GpuState::uniform_buffer` where this draw's
    /// `Uniforms` data was written via `queue.write_buffer`.
    pub uniform_offset: u32,
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
        // egui's CallbackTrait::paint is infallible, so a panic here would crash
        // the app — bail cleanly if GpuState is somehow absent.
        let Some(gpu_state) = callback_resources.get::<Arc<GpuState>>() else {
            return;
        };
        let gpu_state = gpu_state.as_ref();

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
        render_pass.set_bind_group(2, &gpu_state.uniform_bind_group, &[self.uniform_offset]);
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
            size, 192,
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
            size, 96,
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
            diff_metric: 1,
            diff_floor: 0.05,
            _pad2: 0,
            lut_domain_min: [-0.5, -0.5, -0.5, 0.0],
            lut_domain_max: [1.5, 1.5, 1.5, 0.0],
            bg_checker_dark: [0.1, 0.1, 0.1, 0.0],
            bg_checker_light: [0.2, 0.2, 0.2, 0.0],
            bg_solid: [0.18, 0.18, 0.18, 0.0],
            bg_mode: 2,
            bg_grad_angle: 90.0,
            bg_checker_size: 16.0,
            _pad3: 0,
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
