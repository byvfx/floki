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
    // Display-window bounds in screen points (#146). Fragments outside blend at
    // `overscan_factor` instead of `opacity` — single-draw overscan dim. Keep in
    // lockstep with `Uniforms` in src/gpu/mod.rs.
    display_min: vec2<f32>,
    display_max: vec2<f32>,
    // The screen-space quad an accumulate fold rasterizes (#257). `rect_min`/
    // `rect_max` stay the layer's own rect and define the uv mapping; this is the
    // running union of the layer rects, which is everything the accumulation below
    // occupies. Ignored unless composite_accum == 1, and set equal to rect_min/max
    // on every other draw. Keep in lockstep with `Uniforms` in src/gpu/mod.rs.
    fold_min: vec2<f32>,
    fold_max: vec2<f32>,
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
    // Blend factor outside the display window (#146): 1.0 = no dim (thumbnails,
    // side-by-side, OCIO pass 1 — the blit dims there), 0.0 = hidden.
    overscan_factor: f32,
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
    // When 1, `tex_b` is the SCREEN-SIZED scene accumulation of the layer-stack
    // ping-pong (#99), not an image texture, so it is sampled at the fragment's
    // screen position instead of the image-local `in.uv`. 0 for single-pass
    // composite / wipe / diff (tex_b is an image). Keep in lockstep with
    // `Uniforms.composite_accum` in src/gpu/mod.rs.
    composite_accum: u32,
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

    // An accumulate fold (#257) rasterizes `fold_min..fold_max` — the running union
    // of the layer rects — rather than just this layer's rect. Each fold is its own
    // render pass over a full-target `Clear`, so a quad covering only the layer
    // would leave the sentinel everywhere else and the composite would end up
    // clipped to the topmost layer's rect, erasing the layers below it. Covering
    // the union instead lets the fragment stage pass the prior accumulation through
    // untouched outside this layer, so the whole stack survives.
    //
    // The union, not the screen: outside it nothing has been drawn, so rasterizing
    // there would discard exactly what it computed. That matters because this path
    // is shared with the A/B composite under OCIO, where a full-screen fold would
    // enlarge the rasterized area of an existing draw every frame. With the union
    // the quad is unchanged whenever the layers share a rect, which is every case
    // until per-layer placement (#254) lands.
    //
    // Every other draw sets `fold_*` equal to `rect_*`, so this is one `select`,
    // not a second geometry path.
    let is_accum = uniforms.composite_accum == 1u;

    // Map 0..1 to rect_min..rect_max (or to the fold quad for an accumulate fold)
    let screen_pos = select(
        mix(uniforms.rect_min, uniforms.rect_max, pos),
        mix(uniforms.fold_min, uniforms.fold_max, pos),
        is_accum,
    );

    // Map screen_pos to clip space (-1..1)
    let clip_x = (screen_pos.x / uniforms.screen_size.x) * 2.0 - 1.0;
    let clip_y = 1.0 - (screen_pos.y / uniforms.screen_size.y) * 2.0;

    // `uv` stays image-local in both cases — the layer's rect is where uv is 0..1,
    // so a full-screen fold reads outside that range exactly where it misses the
    // layer. The mapping is affine in screen space, so interpolating it across the
    // larger quad is exact. Non-accumulate draws take `pos` verbatim rather than
    // the algebraically-equal unmix, to keep that path bit-for-bit unchanged.
    let span = max(uniforms.rect_max - uniforms.rect_min, vec2<f32>(1e-6, 1e-6));

    var out: VertexOutput;
    out.position = vec4<f32>(clip_x, clip_y, 0.0, 1.0);
    out.uv = select(pos, (screen_pos - uniforms.rect_min) / span, is_accum);
    return out;
}

fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 {
        return l * 12.92;
    } else {
        return 1.055 * pow(l, 1.0 / 2.4) - 0.055;
    }
}

// Hash a screen point to a uniform value in [0, 1) (Dave Hoskins' `hash12` —
// no `sin`, so it's stable across GPUs unlike the classic `fract(sin(...))`).
fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Triangular-PDF (TPDF) dither in (-1, 1) LSB — the difference of two
// independent uniforms. Added before the 8-bit output quantization, it turns
// hard gradient bands into imperceptible noise (the standard fix for banding on
// a smooth ramp written to an 8-bit target). Keyed on the framebuffer pixel and
// a per-channel offset so the three channels get independent noise.
fn tpdf_dither(p: vec2<f32>, chan: f32) -> f32 {
    let q = p + vec2<f32>(chan * 37.0, chan * 17.0);
    return hash12(q) - hash12(q + vec2<f32>(11.3, 7.7));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Single-draw overscan dim (#146): fragments outside the display window
    // blend at `overscan_factor` instead of `opacity`, replacing the old
    // two-draw scheme (whole image at dim opacity + display window redrawn at
    // full). `mix(rect_min, rect_max, uv)` is this fragment's screen point.
    let frag_pt = mix(uniforms.rect_min, uniforms.rect_max, in.uv);
    let od_inside = frag_pt.x >= uniforms.display_min.x && frag_pt.x <= uniforms.display_max.x
                 && frag_pt.y >= uniforms.display_min.y && frag_pt.y <= uniforms.display_max.y;
    let eff_opacity = select(uniforms.overscan_factor, uniforms.opacity, od_inside);

    var color_a = textureSample(tex_a, samp_a, in.uv);
    var color_b = vec4<f32>(0.0);

    if uniforms.is_diff_mode == 1u || uniforms.is_composite == 1u || uniforms.is_wipe_mode == 1u {
        // In the layer-stack accumulate pass (composite_accum==1) tex_b is the
        // screen-sized scene accumulation, so sample it at the fragment's screen
        // position — `in.uv` is image-local (0..1 across the image rect) and only
        // coincides with screen space when the image fills the viewport. Single-pass
        // composite / wipe / diff keep image-local uv (tex_b is an image texture).
        let b_uv = select(
            in.uv,
            in.position.xy / vec2<f32>(textureDimensions(tex_b)),
            uniforms.composite_accum == 1u,
        );
        color_b = textureSample(tex_b, samp_b, b_uv);
    }

    // The prior accumulation exactly as pass 1 left it, kept before the sentinel
    // is normalized below — the pass-through at the end of this function must
    // re-emit it verbatim, α<0 included, or "no image" would decay into "black
    // image" and the background would stop showing through.
    let accum_prev = color_b;

    // Pass 1 clears to α=−1, the "no image here" sentinel the blit keys off. When
    // folding, treat it as fully transparent black: a layer extending past the
    // accumulation beneath it must composite over the background, and a negative
    // alpha would otherwise drive the blend to a bogus coverage (an aa=0.5 layer
    // over α=−1 lands on α=0 — fully background, when half the layer should show).
    if uniforms.composite_accum == 1u && color_b.a < 0.0 {
        color_b = vec4<f32>(0.0);
    }

    // Per-layer opacity for the Layers-panel accumulate composite (#99 PR-B.4):
    // premultiply this layer's color by its opacity, so the subsequent blend fades
    // the layer's contribution (Over/Add: exact; Multiply/Screen: a reasonable
    // premultiplied fade). Gated to the OCIO accumulate context (skip_checker==1)
    // where the comp layers live — a no-op for a single OCIO image (opacity==1) and
    // for diff (which is skip_checker==0 and returns above). The overscan dim does
    // NOT interact: under OCIO it is applied post-transform in the blit, not here.
    if uniforms.skip_checker == 1u {
        color_a = color_a * uniforms.opacity;
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
            return vec4<f32>(mr, mg, mb, eff_opacity);
        }
        var d = max(dr, max(dg, db));
        if uniforms.diff_metric == 1u {
            // Rec.709 luminance-weighted magnitude.
            d = abs(0.2126 * (r - color_b.r) + 0.7152 * (g - color_b.g) + 0.0722 * (b - color_b.b));
        }
        let m = clamp((d * gain - nfloor) / denom, 0.0, 1.0);
        let heat = textureSample(colormap_tex, colormap_samp, vec2<f32>(m, 0.5)).rgb;
        return vec4<f32>(heat, eff_opacity);
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

    // Dither before the 8-bit output quantization to break up gradient banding
    // (worst in dark background ramps). Only on the non-OCIO path: here fs_main
    // writes the final display-encoded color to the 8-bit target, so this is the
    // quantization point. Under OCIO (skip_checker==1) this shader emits a
    // scene-linear intermediate — dithering there would inject huge relative
    // noise — so the OCIO blit owns its own output dither.
    if uniforms.skip_checker == 0u {
        let p = in.position.xy;
        r = r + tpdf_dither(p, 0.0) / 255.0;
        g = g + tpdf_dither(p, 1.0) / 255.0;
        b = b + tpdf_dither(p, 2.0) / 255.0;
    }

    // Under OCIO (skip_checker==1) emit the real image alpha so the display-space
    // checker + overscan dim (in the blit pass) have a coverage/alpha signal. The
    // opacity/overscan dim is applied post-OCIO in that case, not here.
    let out_a = select(eff_opacity, a, uniforms.skip_checker == 1u);
    var result = vec4<f32>(r, g, b, out_a);

    // #257: outside this layer's rect an accumulate fold contributes nothing, so
    // re-emit the accumulation below it verbatim. Written as a `select` on the
    // final value rather than an early `return`: the condition is per-fragment,
    // and a non-uniform return would put every later `textureSample` (LUT,
    // colormap) in non-uniform control flow, which WGSL rejects.
    //
    // These fragments therefore run the whole function before discarding it: the
    // `tex_a` fetch above, the blend, and the view ops — not just ALU. Three things
    // keep that acceptable. The fold quad is the union of the layer rects, not the
    // screen, so this region is only ever the gap between one layer and the stack's
    // extent (empty whenever the layers share a rect). Those `tex_a` fetches are
    // outside 0..1 and clamp to the same edge texels, so they stay cache-resident.
    // And the display chain is already neutralized here (`srgb=0`, `gamma=1`,
    // `enable_lut=0`; see `DrawCtx::draw`), so the tail is genuinely just ALU.
    if uniforms.composite_accum == 1u {
        let outside = any(in.uv < vec2<f32>(0.0)) || any(in.uv > vec2<f32>(1.0));
        result = select(result, accum_prev, outside);
    }
    return result;
}
