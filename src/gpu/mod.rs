use bytemuck::{Pod, Zeroable};
use eframe::egui_wgpu::wgpu;
use std::sync::Arc;

pub mod ocio_pass;

pub mod resources;
pub mod thumbnail;

pub use resources::{GpuResources, TexBuildCtx};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Uniforms {
    // 8-byte aligned fields first (required by WGSL vec2<f32>)
    pub rect_min: [f32; 2],
    pub rect_max: [f32; 2],
    pub screen_size: [f32; 2],
    pub wipe_center: [f32; 2],
    /// Display-window bounds in screen points (#146). Fragments outside this
    /// rect blend at `overscan_factor` instead of `opacity`, so the data-window
    /// overscan dims in the same draw — replacing the old two-draw scheme
    /// (whole image at dim opacity + display window redrawn at full).
    pub display_min: [f32; 2],
    pub display_max: [f32; 2],
    /// The screen-space quad an **accumulate fold** rasterizes (#257), as opposed
    /// to `rect_min`/`rect_max`, which stay the layer's own rect and define the uv
    /// mapping. A fold must cover everything the accumulation below it occupies —
    /// its own pass clears the whole target, so any pixel it skips is lost — but it
    /// need not cover more, so this is the running union of the layer rects, not
    /// the screen. Ignored unless `composite_accum == 1`; set equal to
    /// `rect_min`/`rect_max` on every other draw, which makes the vertex math
    /// identical to a plain placed quad. Keep in lockstep with `shader.wgsl`.
    pub fold_min: [f32; 2],
    pub fold_max: [f32; 2],
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
    /// Blend factor for fragments outside `display_min..display_max` (#146):
    /// `1.0` = no dim (thumbnails, side-by-side, OCIO pass 1 — the blit dims
    /// there), `0.0` = hidden, in between = the overscan dim.
    pub overscan_factor: f32,
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
    /// When 1, the shader treats `tex_b` as the **screen-sized** scene
    /// accumulation of the layer-stack ping-pong (#99) and samples it at the
    /// fragment's screen position rather than the image-local `in.uv`. 0 for
    /// single-pass composite / wipe / diff (where `tex_b` is an image texture).
    /// Formerly `_pad3` alignment filler — same slot, so the layout is unchanged.
    /// Keep in lockstep with `Uniforms.composite_accum` in `shader.wgsl`.
    pub composite_accum: u32,
}

/// Uniforms for the OCIO blit pass: composites the transparency checkerboard in
/// display space (after the OCIO transform, so neutral grey stays neutral) and
/// applies the overscan dim factor outside the display window. All rects/sizes are
/// in egui *points* (the same unit as `Uniforms.screen_size` / `rect_min`), so the
/// 16-point checker and the display-window boundary match the non-OCIO path on HiDPI.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BlitUniforms {
    /// The region the display stage covers: the display window **unioned with every
    /// layer rect** (#254/#257). Normalizes the background gradient, so it must span
    /// everything drawn or the gradient clips to the canvas layer.
    pub display_min: [f32; 2],
    pub display_max: [f32; 2],
    /// The display window alone — the format boundary the overscan dim gates on
    /// (#251). Separate from `display_*` because that pair grew to mean "everything
    /// covered": with one pair serving both, the dim's `inside` test spanned every
    /// layer rect and so was true everywhere, silently dimming nothing. Set both to
    /// the same rect to disable the dim.
    pub dim_min: [f32; 2],
    pub dim_max: [f32; 2],
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
    /// User gamma applied to the OCIO-transformed image in *display* space, after
    /// the view transform and before the (un-managed) background composite (#93).
    /// `1.0` = no-op. In the non-OCIO path gamma lives in `shader.wgsl`; under OCIO
    /// the display chain is OCIO's, so the user gamma is re-applied here.
    pub gamma: f32,
    pub _pad_b: f32,
    pub bg_checker_dark: [f32; 4],
    pub bg_checker_light: [f32; 4],
    pub bg_solid: [f32; 4],
}

pub struct GpuState {
    pub pipeline: wgpu::RenderPipeline,
    /// Same shader/layout as `pipeline` but targets an `Rgba8Unorm` offscreen
    /// texture with `blend: None` (REPLACE) — one opaque quad into a fresh
    /// target. Drives the GPU contact-sheet thumbnail render (#67): with
    /// `srgb=1, skip_checker=0, opacity=1.0` it emits the sRGB-encoded,
    /// checker-composited, opaque bytes egui displays directly.
    pub thumbnail_pipeline: wgpu::RenderPipeline,
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
    /// Stride per uniform slot in bytes: the raw `Uniforms` struct size padded
    /// up to the device's `min_uniform_buffer_offset_alignment` (typically
    /// 256), since dynamic offsets must be aligned.
    pub uniform_stride: u32,
    /// Same shader/layout as `pipeline` but renders into an `Rgba32Float` offscreen target
    /// (the OCIO "pass 1" scene-linear buffer). Drive it with `srgb=0, gamma=1, enable_lut=0`
    /// so it emits linear color for the OCIO display transform.
    pub pipeline_linear: wgpu::RenderPipeline,
    /// Blits the OCIO display texture into egui's render pass (OCIO "paint").
    pub blit_pipeline: wgpu::RenderPipeline,
    pub blit_layout: wgpu::BindGroupLayout,
    pub blit_sampler: wgpu::Sampler,
    /// OCIO-off display encode (#99 render-unify): scene-linear accumulate → sRGB
    /// display. Input `bind_group_layout_tex` (scene view + sampler), output
    /// `target_format`. Slots in where OCIO pass 2 goes so the N-layer composite
    /// renders without OCIO (see [`DISPLAY_ENCODE_SHADER`]).
    pub display_encode_pipeline: wgpu::RenderPipeline,
}

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
    dim_min: vec2<f32>,
    dim_max: vec2<f32>,
    screen_size: vec2<f32>,
    overscan_factor: f32,
    bg_mode: f32,
    bg_checker_size: f32,
    bg_grad_angle: f32,
    gamma: f32,
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

// TPDF output dither — kept in lockstep with `shader.wgsl` so the OCIO display
// path breaks up dark-gradient banding the same way the non-OCIO path does.
fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}
fn tpdf_dither(p: vec2<f32>, chan: f32) -> f32 {
    let q = p + vec2<f32>(chan * 37.0, chan * 17.0);
    return hash12(q) - hash12(q + vec2<f32>(11.3, 7.7));
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

    // User gamma (#93): under OCIO the display chain is OCIO's, so the gamma
    // control is re-applied here in display space, on the image only (the
    // background stays neutral, composited below). 1.0 is a no-op.
    var rgb = disp.rgb;
    if bu.gamma != 1.0 {
        rgb = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / bu.gamma));
    }

    // Background (checker / solid / gradient), composited AFTER OCIO in display
    // space so neutral grey stays neutral (not colour-managed).
    {
        let screen_pt = i.uv * bu.screen_size;
        let guv = (screen_pt - bu.display_min) / max(bu.display_max - bu.display_min, vec2<f32>(1.0));
        let bg = blit_background(screen_pt, guv);
        rgb = rgb + bg * (1.0 - a);
    }

    // Overscan dim: multiply by the dim factor where the pixel is outside the display
    // window (data-window overscan region). Gates on `dim_*`, the display window
    // alone — `display_*` is the whole covered region and would always test inside.
    let screen_pt2 = i.uv * bu.screen_size;
    let inside = screen_pt2.x >= bu.dim_min.x && screen_pt2.x <= bu.dim_max.x
              && screen_pt2.y >= bu.dim_min.y && screen_pt2.y <= bu.dim_max.y;
    let dim = select(bu.overscan_factor, 1.0, inside);
    rgb = rgb * dim;

    // Dither before the 8-bit output quantization (this blit writes the final
    // OCIO display color to the 8-bit surface), keyed on the framebuffer pixel.
    let dp = i.pos.xy;
    rgb = rgb + vec3<f32>(
        tpdf_dither(dp, 0.0),
        tpdf_dither(dp, 1.0),
        tpdf_dither(dp, 2.0),
    ) / 255.0;

    return vec4<f32>(rgb, 1.0);
}
"#;

/// Display-encode pass (#99 render-unify): the **OCIO-off** twin of OCIO pass 2.
/// Samples the scene-linear accumulate and applies the default display encode
/// (linear → sRGB), so the N-layer composite can render with OCIO *off* — the
/// existing OCIO blit then garnishes the result (background / user-gamma / dither)
/// identically to the OCIO path. Exposure is already baked into the accumulate's
/// top layer (PR-A.4); user gamma is the blit's job. Input is `bind_group_layout_tex`
/// (texture + sampler); output matches the display target (`target_format`).
const DISPLAY_ENCODE_SHADER: &str = r#"
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
@group(0) @binding(0) var scene_t: texture_2d<f32>;
@group(0) @binding(1) var scene_s: sampler;
fn lin_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 { return l * 12.92; }
    return 1.055 * pow(l, 1.0 / 2.4) - 0.055;
}
@fragment
fn fs_main(i: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(scene_t, scene_s, i.uv);
    // Carry alpha through unchanged (incl. the <0 "no image" sentinel the blit uses).
    let rgb = vec3<f32>(
        lin_to_srgb(max(c.r, 0.0)),
        lin_to_srgb(max(c.g, 0.0)),
        lin_to_srgb(max(c.b, 0.0)),
    );
    return vec4<f32>(rgb, c.a);
}
"#;

/// Number of uniform slots in the persistent ring buffer. Up to 4 draws per
/// frame (A/B/diff/composite/side-by-side) — 16 gives ample headroom. At
/// 256-byte stride (worst-case alignment) the buffer is 4 KB.
pub const UNIFORM_RING_SLOTS: u64 = 16;

/// Ring slot reserved for offscreen generators (contact-sheet thumbnails).
/// They write + submit immediately, while the viewport's draws are deferred to
/// the egui render pass — sharing slot 0 was safe only while the sheet and the
/// canvas were mutually exclusive; any future same-frame combination would
/// silently draw the viewport with thumbnail uniforms (#148). The viewport's
/// per-frame allocator uses slots `0..UNIFORM_RING_OFFSCREEN_SLOT`.
pub const UNIFORM_RING_OFFSCREEN_SLOT: u64 = UNIFORM_RING_SLOTS - 1;

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

        // GPU contact-sheet thumbnail pipeline (#67): identical to `pipeline` but
        // renders one opaque quad (`blend: None` = REPLACE) into a fresh
        // `Rgba8Unorm` target (egui's required format for `register_native_texture`).
        let thumbnail_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Exr Thumbnail Pipeline"),
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
                    format: wgpu::TextureFormat::Rgba8Unorm,
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
                // f32 ramp (#157): the LUT stores gradient values at full float
                // precision, so dark ramps don't carry 8-bit LUT-side banding.
                // The layout binds it filterable (FLOAT32_FILTERABLE, enabled).
                format: wgpu::TextureFormat::Rgba32Float,
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

        // OCIO-off display encode (#99 render-unify): scene-linear → sRGB, output
        // to the display target. Reuses `bind_group_layout_tex` for its scene input.
        let display_encode_pipeline = {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Display Encode Shader"),
                source: wgpu::ShaderSource::Wgsl(DISPLAY_ENCODE_SHADER.into()),
            });
            let de_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Display Encode Layout"),
                bind_group_layouts: &[Some(&bind_group_layout_tex)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Display Encode Pipeline"),
                layout: Some(&de_layout),
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
            })
        };

        Self {
            pipeline,
            thumbnail_pipeline,
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
            pipeline_linear,
            blit_pipeline,
            blit_layout,
            blit_sampler,
            display_encode_pipeline,
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
    /// `rgba` must be `COLORMAP_LUT_SIZE * 4` f32s (the output of
    /// [`crate::gradient::Gradient::bake`]). Cheap (~4 KB) — called only when the
    /// active gradient changes.
    pub fn write_colormap(&self, queue: &wgpu::Queue, rgba: &[f32]) {
        write_lut_row(queue, &self.colormap_texture, rgba);
    }

    /// Upload a freshly baked background gradient into its persistent texture.
    /// Same contract as [`Self::write_colormap`].
    pub fn write_bg_gradient(&self, queue: &wgpu::Queue, rgba: &[f32]) {
        write_lut_row(queue, &self.bg_gradient_texture, rgba);
    }
}

/// Write a baked `COLORMAP_LUT_SIZE × 1` `Rgba32Float` LUT row into `tex`. Shared
/// by the colormap and background-gradient textures (seed + updates).
fn write_lut_row(queue: &wgpu::Queue, tex: &wgpu::Texture, rgba: &[f32]) {
    let width = crate::gradient::COLORMAP_LUT_SIZE as u32;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(rgba),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4 * 4),
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

/// One GPU adapter as the startup preflight sees it (#247).
///
/// Plain strings rather than the wgpu types so the decision below is pure and
/// testable: enumerating adapters needs a live instance, deciding what to say about
/// them does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterSummary {
    pub name: String,
    pub backend: String,
    pub device_type: String,
    /// Whether this adapter offers `FLOAT32_FILTERABLE`, which `GpuState` requires
    /// to linearly sample the f32 3D LUT texture.
    pub float32_filterable: bool,
}

/// `None` if floki can run on one of `adapters`; otherwise the message to show the
/// user before exiting (#247).
///
/// The requirement itself isn't negotiable — the f32 3D LUT is how every image gets
/// colour-managed, so there is no reduced mode to fall back to, and the CPU path in
/// `viewer.rs` can't run the OCIO ping-pong or the comp composite. What was
/// negotiable is *failing legibly*: `required_features` at device creation made
/// `request_device` return `Err` and the app never opened a window, so the report
/// came back as "it doesn't open" with nothing attached.
///
/// The message names the adapters actually found, because the common causes are all
/// diagnosable from that list: a laptop on its integrated adapter rather than the
/// discrete one, or a remote session exposing only a software rasterizer — and
/// reviewing over RDP/VDI is a normal VFX workflow, not an edge case. Pure.
#[must_use]
pub fn gpu_preflight_error(adapters: &[AdapterSummary]) -> Option<String> {
    if adapters.iter().any(|a| a.float32_filterable) {
        return None;
    }
    if adapters.is_empty() {
        return Some(
            "Floki could not find a GPU.\n\n\
             The system reported no graphics adapter at all. Floki renders entirely \
             on the GPU, so it cannot start without one.\n\n\
             If the WGPU_BACKEND environment variable is set, unset it and try \
             again: it restricts which adapters Floki can see, and a value naming a \
             backend this machine does not have leaves none at all.\n\n\
             Otherwise this is usually a remote session (RDP or VDI) that exposes no \
             usable adapter, or a graphics driver that failed to load. Running on \
             the machine directly, or reinstalling the driver, is normally the fix."
                .to_string(),
        );
    }
    let found = adapters
        .iter()
        .map(|a| format!("  - {} ({}, {})", a.name, a.backend, a.device_type))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Floki needs a GPU feature this machine's graphics adapter does not \
         provide.\n\n\
         Missing: FLOAT32_FILTERABLE (linear sampling of 32-bit float textures). \
         Floki uses it for the colour-management LUT, which every displayed image \
         goes through, so there is no reduced mode to fall back to.\n\n\
         Adapters found:\n{found}\n\n\
         If this machine also has a discrete GPU, launching Floki on it usually \
         works (Windows: Settings > System > Display > Graphics; otherwise the GPU \
         vendor's control panel). A remote session often exposes only a software \
         adapter, which will not.\n\n\
         If the WGPU_BACKEND environment variable is set, unset it — it restricts \
         which adapters Floki can see, and may be hiding one that would work. If it \
         is not set and you believe the GPU does support this, try forcing a backend \
         with it (vulkan, dx12, or metal)."
    ))
}

/// A GPU device for an on-device test, or `None` when this machine can't give one —
/// in which case the caller returns and the test is a no-op. **Every on-device test
/// goes through this**; see TESTING.md.
///
/// There are two ways to come up short and both must skip rather than fail:
///
/// 1. No adapter at all — a headless runner with no Vulkan/Metal/DX.
/// 2. An adapter that *exists* but lacks `FLOAT32_FILTERABLE`, which [`GpuState`]
///    requires for the f32 3D LUT. This is the one that bites: `Test & Lint` runs on
///    ubuntu-latest, which has llvmpipe in software, so an adapter-only guard passes
///    the first check and then panics in `request_device`.
///
/// Condition 2 was reported in #195 and reached CI in #259, where five hand-rolled
/// copies of this guard were all missing it. Living in `gpu` rather than in one test
/// module is the point (#266): `ocio_pass::metal_tests`, `ocio_pass::device_tests` and
/// `thumbnail` all need it, and four copies is how the condition went missing from
/// every copy at once.
#[cfg(test)]
pub(crate) fn test_device(label: &'static str) -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping {label}");
                return None;
            }
        };
    match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some(label),
        required_features: wgpu::Features::FLOAT32_FILTERABLE,
        ..Default::default()
    })) {
        Ok(dq) => Some(dq),
        Err(e) => {
            // Don't assert the cause: a missing FLOAT32_FILTERABLE is the expected
            // reason and the one worth naming, but `request_device` fails for others
            // too, and a message that states the wrong cause is worse than none when
            // you are reading a CI log trying to work out why a test went quiet.
            eprintln!(
                "request_device failed ({e:?}) — this test needs FLOAT32_FILTERABLE; \
                 skipping {label}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, backend: &str, kind: &str, f32f: bool) -> AdapterSummary {
        AdapterSummary {
            name: name.to_string(),
            backend: backend.to_string(),
            device_type: kind.to_string(),
            float32_filterable: f32f,
        }
    }

    /// One capable adapter is enough, even when it isn't the only one — a laptop
    /// reporting both an integrated and a discrete GPU is the normal case, and
    /// refusing to start because *an* adapter falls short would ground machines that
    /// run floki perfectly well.
    #[test]
    fn preflight_passes_when_any_adapter_can_run_floki() {
        assert!(
            gpu_preflight_error(&[adapter(
                "NVIDIA GeForce RTX 4090",
                "Vulkan",
                "DiscreteGpu",
                true
            )])
            .is_none()
        );
        assert!(
            gpu_preflight_error(&[
                adapter("Intel UHD Graphics 620", "Vulkan", "IntegratedGpu", false),
                adapter("NVIDIA RTX A2000", "Dx12", "DiscreteGpu", true),
            ])
            .is_none(),
            "the discrete adapter qualifies, so the integrated one falling short is \
             not a reason to refuse"
        );
    }

    /// The whole point of #247 is the message, so assert it carries what a person
    /// needs: the missing feature by name, every adapter that *was* found, and the
    /// two things that actually fix it.
    #[test]
    fn preflight_names_the_missing_feature_and_the_adapters_it_found() {
        let msg = gpu_preflight_error(&[
            adapter("Intel UHD Graphics 620", "Vulkan", "IntegratedGpu", false),
            adapter("Microsoft Basic Render Driver", "Dx12", "Cpu", false),
        ])
        .expect("no adapter qualifies");

        assert!(msg.contains("FLOAT32_FILTERABLE"), "{msg}");
        // Both adapters, so a bug report pasting this says what the machine has.
        assert!(msg.contains("Intel UHD Graphics 620"), "{msg}");
        assert!(msg.contains("Microsoft Basic Render Driver"), "{msg}");
        assert!(msg.contains("Vulkan") && msg.contains("Dx12"), "{msg}");
        // And the two routes out: the discrete GPU, or a forced backend.
        assert!(msg.contains("discrete"), "{msg}");
        assert!(msg.contains("WGPU_BACKEND"), "{msg}");
    }

    /// No adapters at all is a different failure with a different cause, so it gets
    /// its own message: listing "adapters found:" and then nothing would read as a
    /// bug in the message rather than as the diagnosis it is.
    #[test]
    fn preflight_distinguishes_no_adapter_from_an_unsuitable_one() {
        let none = gpu_preflight_error(&[]).expect("no adapters at all is a failure");
        assert!(none.contains("could not find a GPU"), "{none}");
        assert!(!none.contains("Adapters found"), "{none}");
        // The likely causes, which are not the same as the unsuitable-adapter ones.
        assert!(none.contains("RDP") || none.contains("remote"), "{none}");
        assert!(none.contains("driver"), "{none}");
        // `WGPU_BACKEND` filtering every adapter out reaches this same branch, and
        // it is the one cause the user can fix in a second. Verifying #247 hit
        // exactly that — a backend value with no native adapters — and the message
        // then blamed a remote session and a broken driver, neither of which was
        // true. Both branches name it now.
        assert!(none.contains("WGPU_BACKEND"), "{none}");
    }

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
            size, 224,
            "Uniforms layout changed — update shader.wgsl to match"
        );
    }

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
            size, 112,
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
            display_min: [1.0, 2.0],
            display_max: [3.0, 4.0],
            fold_min: [-2.0, -3.0],
            fold_max: [9.0, 8.0],
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
            overscan_factor: 1.0,
            lut_domain_min: [-0.5, -0.5, -0.5, 0.0],
            lut_domain_max: [1.5, 1.5, 1.5, 0.0],
            bg_checker_dark: [0.1, 0.1, 0.1, 0.0],
            bg_checker_light: [0.2, 0.2, 0.2, 0.0],
            bg_solid: [0.18, 0.18, 0.18, 0.0],
            bg_mode: 2,
            bg_grad_angle: 90.0,
            bg_checker_size: 16.0,
            composite_accum: 0,
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
