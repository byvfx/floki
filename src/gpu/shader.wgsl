struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Uniforms {
    rect_min: vec2<f32>,
    rect_max: vec2<f32>,
    screen_size: vec2<f32>,
    exposure: f32,
    gamma: f32,
    diff_multiplier: f32,
    channel_mode: u32,
    is_diff_mode: u32,
    srgb: u32,
    enable_lut: u32,
    opacity: f32,
    is_composite: u32,
    blend_mode: u32,
};

@group(0) @binding(0) var tex_a: texture_2d<f32>;
@group(0) @binding(1) var samp_a: sampler;

@group(1) @binding(0) var tex_b: texture_2d<f32>;
@group(1) @binding(1) var samp_b: sampler;

@group(2) @binding(0) var<uniform> uniforms: Uniforms;

@group(3) @binding(0) var lut_tex: texture_3d<f32>;
@group(3) @binding(1) var lut_samp: sampler;

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
    
    if uniforms.is_diff_mode == 1u || uniforms.is_composite == 1u {
        color_b = textureSample(tex_b, samp_b, in.uv);
    }

    var r = color_a.r;
    var g = color_a.g;
    var b = color_a.b;
    var a = color_a.a;

    if uniforms.is_diff_mode == 1u {
        r = abs(r - color_b.r) * uniforms.diff_multiplier;
        g = abs(g - color_b.g) * uniforms.diff_multiplier;
        b = abs(b - color_b.b) * uniforms.diff_multiplier;
        a = 1.0;
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
    
    // Background Checkerboard compositing
    // In screen space we can do a checkerboard
    let screen_pos = mix(uniforms.rect_min, uniforms.rect_max, in.uv);
    let check_x = u32(screen_pos.x / 16.0);
    let check_y = u32(screen_pos.y / 16.0);
    let is_dark = (check_x + check_y) % 2u == 0u;
    let bg_linear = select(0.2, 0.1, is_dark);
    
    let a_clamp = clamp(a, 0.0, 1.0);
    r = r + bg_linear * (1.0 - a_clamp);
    g = g + bg_linear * (1.0 - a_clamp);
    b = b + bg_linear * (1.0 - a_clamp);

    // Gamma
    if uniforms.gamma != 1.0 {
        let inv_gamma = 1.0 / uniforms.gamma;
        r = select(0.0, pow(r, inv_gamma), r > 0.0);
        g = select(0.0, pow(g, inv_gamma), g > 0.0);
        b = select(0.0, pow(b, inv_gamma), b > 0.0);
    }

    // sRGB
    if uniforms.enable_lut == 1u {
        let l_color = textureSample(lut_tex, lut_samp, clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)));
        r = l_color.r;
        g = l_color.g;
        b = l_color.b;
    }

    if uniforms.srgb == 1u {
        r = linear_to_srgb(r);
        g = linear_to_srgb(g);
        b = linear_to_srgb(b);
    }

    return vec4<f32>(r, g, b, uniforms.opacity);
}
