struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Uniforms {
    // 8-byte aligned fields first (required by WGSL vec2<f32>)
    rect_min: vec2<f32>,
    rect_max: vec2<f32>,
    screen_size: vec2<f32>,
    wipe_center: vec2<f32>,
    // 4-byte aligned fields
    exposure: f32,
    gamma: f32,
    diff_multiplier: f32,
    opacity: f32,
    wipe_angle: f32,
    channel_mode: u32,
    is_diff_mode: u32,
    srgb: u32,
    enable_lut: u32,
    is_composite: u32,
    blend_mode: u32,
    is_wipe_mode: u32,
    // When 1, skip the background checkerboard composite and emit the real image
    // alpha (instead of `opacity`). Used by the OCIO "pass 1" so the checker can be
    // composited in display space *after* the OCIO transform. Keep in lockstep with
    // `Uniforms.skip_checker` in src/gpu/mod.rs.
    skip_checker: u32,
    // Diff visualization controls (only read when is_diff_mode == 1). Source of
    // truth for `diff_metric`: `DiffMetric::as_u32` in src/gradient.rs
    // (MaxChannel=0, Luminance=1, PerChannelRGB=2). `diff_floor` is a noise floor
    // subtracted from the gained magnitude. Keep in lockstep with src/gpu/mod.rs.
    diff_metric: u32,
    diff_floor: f32,
    _pad2: u32,
    // .cube LUT domain bounds (xyz + pad). The lookup coordinate is remapped from
    // [domain_min, domain_max] to [0, 1] before sampling the 3D LUT texture, so
    // non-unit-domain LUTs (HDR/film looks) sample correctly. Defaults to identity.
    // Keep in lockstep with `Uniforms.lut_domain_min/max` in src/gpu/mod.rs.
    lut_domain_min: vec4<f32>,
    lut_domain_max: vec4<f32>,
    // Customizable viewport background (issue #18). Linear-space colours (xyz),
    // composited where image alpha < 1. `bg_mode`: Checkerboard=0, Solid=1,
    // Gradient=2 (source of truth: `background::BackgroundMode::as_u32`). The
    // gradient ramp is `bg_gradient_tex`, sampled along `bg_grad_angle`. Keep in
    // lockstep with `Uniforms` in src/gpu/mod.rs.
    bg_checker_dark: vec4<f32>,
    bg_checker_light: vec4<f32>,
    bg_solid: vec4<f32>,
    bg_mode: u32,
    bg_grad_angle: f32,
    bg_checker_size: f32,
    _pad3: u32,
};

@group(0) @binding(0) var tex_a: texture_2d<f32>;
@group(0) @binding(1) var samp_a: sampler;

@group(1) @binding(0) var tex_b: texture_2d<f32>;
@group(1) @binding(1) var samp_b: sampler;

@group(2) @binding(0) var<uniform> uniforms: Uniforms;

@group(3) @binding(0) var lut_tex: texture_3d<f32>;
@group(3) @binding(1) var lut_samp: sampler;
// 256x1 diff colormap LUT (display-space false colour). Shares group(3) with the
// 3D look LUT because we are already at the 4-bind-group limit. Updated in place
// via `queue.write_texture` when the active gradient changes (see src/gpu/mod.rs).
@group(3) @binding(2) var colormap_tex: texture_2d<f32>;
@group(3) @binding(3) var colormap_samp: sampler;
// 256x1 background gradient LUT (issue #18), updated in place like the colormap.
@group(3) @binding(4) var bg_gradient_tex: texture_2d<f32>;

// Linear background colour at screen pixel `screen_pos` (for the checker) /
// normalized `uv` (for the gradient). Keep in lockstep with `Background` in
// src/background.rs and the blit shader in src/gpu/mod.rs.
fn background_color(screen_pos: vec2<f32>, uv: vec2<f32>) -> vec3<f32> {
    if uniforms.bg_mode == 1u {
        return uniforms.bg_solid.rgb;
    }
    if uniforms.bg_mode == 2u {
        let a = radians(uniforms.bg_grad_angle);
        let d = vec2<f32>(cos(a), sin(a));
        let pmin = min(d.x, 0.0) + min(d.y, 0.0);
        let pmax = max(d.x, 0.0) + max(d.y, 0.0);
        let p = uv.x * d.x + uv.y * d.y;
        let t = clamp((p - pmin) / max(pmax - pmin, 1e-4), 0.0, 1.0);
        return textureSampleLevel(bg_gradient_tex, colormap_samp, vec2<f32>(t, 0.5), 0.0).rgb;
    }
    // Checkerboard.
    let size = max(uniforms.bg_checker_size, 1.0);
    let cx = floor(screen_pos.x / size);
    let cy = floor(screen_pos.y / size);
    let is_dark = (i32(cx) + i32(cy)) % 2 == 0;
    return select(uniforms.bg_checker_light.rgb, uniforms.bg_checker_dark.rgb, is_dark);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0)
    );

    let pos = positions[vertex_index];
    
    // Map 0..1 to rect_min..rect_max
    let screen_pos = mix(uniforms.rect_min, uniforms.rect_max, pos);
    
    // Map screen_pos to clip space (-1..1)
    let clip_x = (screen_pos.x / uniforms.screen_size.x) * 2.0 - 1.0;
    let clip_y = 1.0 - (screen_pos.y / uniforms.screen_size.y) * 2.0;

    var out: VertexOutput;
    out.position = vec4<f32>(clip_x, clip_y, 0.0, 1.0);
    out.uv = vec2<f32>(pos.x, pos.y);
    return out;
}

fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 {
        return l * 12.92;
    } else {
        return 1.055 * pow(l, 1.0 / 2.4) - 0.055;
    }
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    var color_a = textureSample(tex_a, samp_a, in.uv);
    var color_b = vec4<f32>(0.0);
    
    if uniforms.is_diff_mode == 1u || uniforms.is_composite == 1u || uniforms.is_wipe_mode == 1u {
        color_b = textureSample(tex_b, samp_b, in.uv);
    }

    var r = color_a.r;
    var g = color_a.g;
    var b = color_a.b;
    var a = color_a.a;

    if uniforms.is_diff_mode == 1u {
        // VFX-style diff: the per-pixel difference reduced to a magnitude per
        // `diff_metric`, gained by `diff_multiplier`, noise-floored by `diff_floor`,
        // and mapped through the `colormap_tex` ramp. This is a false-color
        // visualization, emitted directly in display space and NOT color-managed —
        // the viewer routes diff through this pipeline even under OCIO. Keep the
        // metric/floor math in lockstep with `generate_diff_texture` in src/viewer.rs.
        let dr = abs(r - color_b.r);
        let dg = abs(g - color_b.g);
        let db = abs(b - color_b.b);
        let gain = uniforms.diff_multiplier;
        let nfloor = uniforms.diff_floor;
        let denom = max(1.0 - nfloor, 1e-3);
        if uniforms.diff_metric == 2u {
            // Per-channel RGB: show each channel's gained |Δ| directly (no colormap).
            let mr = clamp((dr * gain - nfloor) / denom, 0.0, 1.0);
            let mg = clamp((dg * gain - nfloor) / denom, 0.0, 1.0);
            let mb = clamp((db * gain - nfloor) / denom, 0.0, 1.0);
            return vec4<f32>(mr, mg, mb, uniforms.opacity);
        }
        var d = max(dr, max(dg, db));
        if uniforms.diff_metric == 1u {
            // Rec.709 luminance-weighted magnitude.
            d = abs(0.2126 * (r - color_b.r) + 0.7152 * (g - color_b.g) + 0.0722 * (b - color_b.b));
        }
        let m = clamp((d * gain - nfloor) / denom, 0.0, 1.0);
        let heat = textureSample(colormap_tex, colormap_samp, vec2<f32>(m, 0.5)).rgb;
        return vec4<f32>(heat, uniforms.opacity);
    }

    // Premultiplied-alpha compositing. Keep the `blend_mode` switch in lockstep
    // with `BlendMode::as_u32` in src/viewer.rs (Over=0, Under=1, Add=2,
    // Multiply=3, Screen=4) and the CPU `generate_composite_texture`.
    if uniforms.is_composite == 1u {
        let aa = color_a.a;
        let ba = color_b.a;
        switch uniforms.blend_mode {
            case 1u: { // Under: B over A
                r = color_b.r + color_a.r * (1.0 - ba);
                g = color_b.g + color_a.g * (1.0 - ba);
                b = color_b.b + color_a.b * (1.0 - ba);
                a = ba + aa * (1.0 - ba);
            }
            case 2u: { // Add
                r = color_a.r + color_b.r;
                g = color_a.g + color_b.g;
                b = color_a.b + color_b.b;
                a = min(aa + ba, 1.0);
            }
            case 3u: { // Multiply
                r = color_a.r * color_b.r;
                g = color_a.g * color_b.g;
                b = color_a.b * color_b.b;
                a = aa;
            }
            case 4u: { // Screen
                r = color_a.r + color_b.r - color_a.r * color_b.r;
                g = color_a.g + color_b.g - color_a.g * color_b.g;
                b = color_a.b + color_b.b - color_a.b * color_b.b;
                a = aa + ba - aa * ba;
            }
            default: { // 0u Over: A over B
                r = color_a.r + color_b.r * (1.0 - aa);
                g = color_a.g + color_b.g * (1.0 - aa);
                b = color_a.b + color_b.b * (1.0 - aa);
                a = aa + ba * (1.0 - aa);
            }
        }
    }

    // Wipe mode: use dot product to determine which side of the line we are on.
    // Write r/g/b/a directly — they were already copied from color_a above, so
    // reassigning color_a here would have no effect on the output.
    if uniforms.is_wipe_mode == 1u {
        // Work in screen-pixel space so the split lines up with the on-screen wipe
        // line at every angle. UV space is normalized 0..1 per-axis, so on a
        // non-square image it distorts the angle; scaling by the rect size
        // (rect_max - rect_min, in pixels) removes that distortion.
        let rect_size = uniforms.rect_max - uniforms.rect_min;
        let to_pixel = (in.uv - uniforms.wipe_center) * rect_size;
        let normal = vec2<f32>(cos(uniforms.wipe_angle), sin(uniforms.wipe_angle));
        let dist = dot(to_pixel, normal);
        if dist >= 0.0 {
            r = color_b.r;
            g = color_b.g;
            b = color_b.b;
            a = color_b.a;
        }
    }

    // Channel mode
    // 0: RGB, 1: R, 2: G, 3: B, 4: A
    // Source of truth for this encoding: `ChannelMode::as_u32` in src/viewer.rs.
    // Keep these branches in lockstep with that mapping.
    if uniforms.channel_mode == 1u {
        g = r; b = r; a = 1.0;
    } else if uniforms.channel_mode == 2u {
        r = g; b = g; a = 1.0;
    } else if uniforms.channel_mode == 3u {
        r = b; g = b; a = 1.0;
    } else if uniforms.channel_mode == 4u {
        r = a; g = a; b = a; a = 1.0;
    }

    // Exposure
    let exp_mult = exp2(uniforms.exposure);
    r *= exp_mult;
    g *= exp_mult;
    b *= exp_mult;
    
    // Background compositing (checkerboard / solid / gradient — see `background_color`).
    // Composited in scene-linear space, then tone-mapped with the image below.
    // Skipped under OCIO (skip_checker==1): the background is composited in display
    // space after the OCIO transform (in the blit pass) so neutral grey stays neutral.
    if uniforms.skip_checker == 0u {
        let screen_pos = mix(uniforms.rect_min, uniforms.rect_max, in.uv);
        let bg = background_color(screen_pos, in.uv);

        let a_clamp = clamp(a, 0.0, 1.0);
        r = r + bg.r * (1.0 - a_clamp);
        g = g + bg.g * (1.0 - a_clamp);
        b = b + bg.b * (1.0 - a_clamp);
    }

    // Display transform chain: gamma → LUT → sRGB.
    //
    // This order treats the .cube LUT as a "look" LUT applied in display space
    // (after gamma adjustment but before sRGB encoding), which matches how most
    // DCC tools (Nuke, Resolve) apply .cube LUTs for creative grading. The LUT
    // input is clamped to its authored domain (see domain remap below) so HDR
    // values above 1.0 are mapped, not discarded.
    //
    // If both enable_lut and srgb are on, the chain is: linear → gamma → LUT → sRGB.
    // A pure display LUT (which includes its own display curve) would typically
    // be used with srgb=0 to avoid double-applying a display curve.

    // Gamma
    if uniforms.gamma != 1.0 {
        let inv_gamma = 1.0 / uniforms.gamma;
        r = pow(max(r, 0.0), inv_gamma);
        g = pow(max(g, 0.0), inv_gamma);
        b = pow(max(b, 0.0), inv_gamma);
    }

    // LUT
    if uniforms.enable_lut == 1u {
        // Remap the display-space RGB from the LUT's authored domain to [0,1]
        // texture coordinates. A unit-domain LUT (the common case) has
        // domain_min=0, domain_max=1 and the remap is identity. HDR/film LUTs
        // authored with e.g. DOMAIN_MIN -0.5 / DOMAIN_MAX 1.5 would otherwise
        // have their input clamped to [0,1] and sample the wrong texels.
        let dmin = uniforms.lut_domain_min.xyz;
        let dmax = uniforms.lut_domain_max.xyz;
        let lut_uv = clamp((vec3<f32>(r, g, b) - dmin) / (dmax - dmin), vec3<f32>(0.0), vec3<f32>(1.0));
        let l_color = textureSample(lut_tex, lut_samp, lut_uv);
        r = l_color.r;
        g = l_color.g;
        b = l_color.b;
    }

    if uniforms.srgb == 1u {
        r = linear_to_srgb(r);
        g = linear_to_srgb(g);
        b = linear_to_srgb(b);
    }

    // Under OCIO (skip_checker==1) emit the real image alpha so the display-space
    // checker + overscan dim (in the blit pass) have a coverage/alpha signal. The
    // `opacity` dim is applied post-OCIO in that case, not here.
    let out_a = select(uniforms.opacity, a, uniforms.skip_checker == 1u);
    return vec4<f32>(r, g, b, out_a);
}
