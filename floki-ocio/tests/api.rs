//! Smoke + behaviour tests for the public API. The no-backend path pins the
//! `NotCompiled` contract; the native path exercises real OCIO against the built-in
//! `ocio://default` config (no vendored config file needed).

use floki_ocio::ConfigSource;

#[test]
#[cfg(not(feature = "_native"))]
fn load_without_backend_reports_not_compiled() {
    match floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default")) {
        Err(floki_ocio::OcioError::NotCompiled) => {}
        Err(other) => panic!("expected NotCompiled, got {other:?}"),
        Ok(_) => panic!("expected NotCompiled without a backend feature"),
    }
}

#[test]
#[cfg(feature = "_native")]
fn builtin_default_config_loads_and_enumerates() {
    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default"))
        .expect("built-in default config should load");
    assert!(
        !cfg.color_spaces().is_empty(),
        "config must declare color spaces"
    );
    assert!(!cfg.displays().is_empty(), "config must declare displays");
    assert!(
        !cfg.default_display().is_empty(),
        "config must have a default display"
    );
}

#[test]
#[cfg(feature = "_native")]
fn cpu_processor_transforms_scene_linear_to_display() {
    use floki_ocio::DisplayTransformRequest;

    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default"))
        .expect("built-in default config should load");

    // Pick a sensible transform from the config itself rather than hardcoding names.
    let input_colorspace = cfg
        .scene_linear_colorspace()
        .or_else(|| {
            cfg.color_spaces()
                .into_iter()
                .find(|c| !c.is_data)
                .map(|c| c.name)
        })
        .expect("config should expose a non-data color space");

    let display = cfg.default_display();
    let view = cfg
        .displays()
        .into_iter()
        .find(|d| d.name == display)
        .map(|d| d.default_view)
        .expect("default display should have a default view");

    let req = DisplayTransformRequest {
        input_colorspace,
        display,
        view,
        bake_lut_size: 0,
    };
    let proc = cfg
        .build_cpu_processor(&req)
        .expect("should build a CPU processor");

    // 18% mid-grey in scene-linear, opaque.
    let mut px = vec![0.18_f32, 0.18, 0.18, 1.0];
    proc.apply_rgba(&mut px, 1, 1)
        .expect("apply should succeed");

    // Output must be finite, in display range, and alpha untouched.
    for c in &px[..3] {
        assert!(c.is_finite(), "channel must be finite, got {c}");
        assert!(
            (0.0..=1.0).contains(c),
            "channel should be in [0,1], got {c}"
        );
    }
    assert_eq!(px[3], 1.0, "alpha must be preserved");
}

#[test]
#[cfg(feature = "_native")]
fn baked_lut_matches_analytic_within_tolerance() {
    // The baked path (log2 shaper + 3D LUT) must reproduce the analytic display transform.
    // We compare the two as CPU processors across scene-linear shadows..highlights; any gross
    // error (wrong shaper direction, channel swap, bad grid indexing) shows up immediately,
    // while the 65^3 LUT's interpolation error on the built-in ACES transform stays small
    // (worst case ~0.02, on saturated highlights; midtones far tighter).
    use floki_ocio::DisplayTransformRequest;

    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default"))
        .expect("built-in default config should load");
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

    let mk = |bake: u32| {
        cfg.build_cpu_processor(&DisplayTransformRequest {
            input_colorspace: input_colorspace.clone(),
            display: display.clone(),
            view: view.clone(),
            bake_lut_size: bake,
        })
        .expect("should build a CPU processor")
    };
    let analytic = mk(0);
    let baked = mk(65);

    // Grey ramp + a few saturated colors spanning ~16 stops of scene-linear.
    let mut samples: Vec<[f32; 4]> = Vec::new();
    for &v in &[0.0, 0.002, 0.018, 0.18, 0.5, 1.0, 4.0, 16.0, 64.0] {
        samples.push([v, v, v, 1.0]);
        samples.push([v, v * 0.5, v * 0.25, 1.0]);
        samples.push([v * 0.25, v, v * 0.5, 1.0]);
    }

    let mut max_diff = 0.0f32;
    let mut worst = ([0.0f32; 4], [0.0f32; 4], [0.0f32; 4]);
    for px in &samples {
        let (mut a, mut b) = (*px, *px);
        analytic.apply_rgba(&mut a, 1, 1).unwrap();
        baked.apply_rgba(&mut b, 1, 1).unwrap();
        for i in 0..3 {
            assert!(b[i].is_finite(), "baked output must be finite, got {b:?}");
            // Compare what actually reaches the screen. Some configs (e.g. the OCIO 2.4 default)
            // emit out-of-gamut negatives for saturated colors; those clamp to the display range
            // identically for both paths, so the visible error is the clamped difference — not
            // the raw out-of-gamut math noise.
            let d = (a[i].clamp(0.0, 1.0) - b[i].clamp(0.0, 1.0)).abs();
            if d > max_diff {
                max_diff = d;
                worst = (*px, a, b);
            }
        }
    }
    assert!(
        max_diff < 0.02,
        "baked 65^3 LUT must match analytic within 0.02, got {max_diff} \
         at input {:?}: analytic {:?} vs baked {:?}",
        worst.0,
        worst.1,
        worst.2
    );
}

#[test]
#[cfg(feature = "_native")]
fn baked_gpu_shader_emits_a_single_3d_lut() {
    // The baked GPU path must reduce the analytic transform to (log2 shaper + one 3D LUT)
    // and still transpile to valid SPIR-V/WGSL — this covers the GPU-shader emission the
    // CPU `baked_lut_matches_analytic_*` test does not.
    use floki_ocio::{DisplayTransformRequest, TexDim};

    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default"))
        .expect("built-in default config should load");
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

    let bundle = cfg
        .build_gpu_shader(&DisplayTransformRequest {
            input_colorspace,
            display,
            view,
            bake_lut_size: 65,
        })
        .expect("baked GPU shader should build + transpile");

    assert!(!bundle.spirv.is_empty(), "baked SPIR-V must be non-empty");
    assert!(
        bundle
            .wgsl
            .as_deref()
            .is_some_and(|w| w.contains("fn main")),
        "baked WGSL should contain a main entry point"
    );

    let luts_3d: Vec<_> = bundle
        .textures
        .iter()
        .filter(|t| t.dim == TexDim::D3)
        .collect();
    assert_eq!(
        luts_3d.len(),
        1,
        "baked transform should emit exactly one 3D LUT, got {:?}",
        bundle.textures.iter().map(|t| t.dim).collect::<Vec<_>>()
    );
    let lut = luts_3d[0];
    assert_eq!((lut.width, lut.height, lut.depth), (65, 65, 65));
    assert_eq!(lut.data_rgba.len(), 65 * 65 * 65 * 4, "65^3 RGBA texels");
}

#[test]
#[cfg(feature = "_native")]
fn build_gpu_shader_produces_spirv_bindings_and_wgsl() {
    use floki_ocio::{BindingKind, DisplayTransformRequest};

    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default"))
        .expect("built-in default config should load");
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

    let bundle = cfg
        .build_gpu_shader(&DisplayTransformRequest {
            input_colorspace,
            display,
            view,
            bake_lut_size: 0,
        })
        .expect("should generate + transpile a GPU shader bundle");

    assert!(!bundle.spirv.is_empty(), "SPIR-V must be non-empty");
    assert_eq!(bundle.entry_point, "main");
    assert!(
        bundle
            .wgsl
            .as_deref()
            .is_some_and(|w| w.contains("fn main")),
        "WGSL output should contain a main entry point"
    );

    // The scene input texture + sampler must be reflected (set 1), plus OCIO's own LUTs.
    let has_scene_tex = bundle
        .bindings
        .iter()
        .any(|b| b.group == 1 && matches!(b.kind, BindingKind::Texture(_)));
    let has_scene_sampler = bundle
        .bindings
        .iter()
        .any(|b| b.group == 1 && b.kind == BindingKind::Sampler);
    assert!(
        has_scene_tex && has_scene_sampler,
        "scene input must be reflected: {:?}",
        bundle.bindings
    );

    // ACES config emits LUT textures; each must be repacked to RGBA (4 floats/texel).
    for t in &bundle.textures {
        let texels = (t.width as usize) * (t.height.max(1) as usize) * (t.depth.max(1) as usize);
        assert_eq!(
            t.data_rgba.len(),
            texels * 4,
            "texture {} must be RGBA-packed",
            t.name
        );
    }
}

#[test]
#[cfg(feature = "_native")]
fn env_config_source_reads_ocio_var() {
    // `$OCIO` accepts built-in URIs too, so this exercises the Env path without a config file.
    // SAFETY: no other test reads/writes the OCIO env var.
    unsafe { std::env::set_var("OCIO", "ocio://default") };
    let result = floki_ocio::OcioConfig::load(ConfigSource::Env);
    unsafe { std::env::remove_var("OCIO") };

    let cfg = result.expect("should load the config named by $OCIO");
    assert!(
        !cfg.displays().is_empty(),
        "config from $OCIO must enumerate displays"
    );
}

#[test]
#[cfg(feature = "_native")]
fn apply_rgba_rejects_wrong_buffer_length() {
    use floki_ocio::{DisplayTransformRequest, OcioError};

    let cfg = floki_ocio::OcioConfig::load(ConfigSource::BuiltIn("ocio://default")).unwrap();
    let input_colorspace = cfg
        .scene_linear_colorspace()
        .unwrap_or_else(|| cfg.color_spaces().into_iter().next().unwrap().name);
    let display = cfg.default_display();
    let view = cfg
        .displays()
        .into_iter()
        .find(|d| d.name == display)
        .map(|d| d.default_view)
        .unwrap();
    let proc = cfg
        .build_cpu_processor(&DisplayTransformRequest {
            input_colorspace,
            display,
            view,
            bake_lut_size: 0,
        })
        .unwrap();

    // 1x1 RGBA needs 4 floats; give it 3.
    let mut px = vec![0.0_f32; 3];
    match proc.apply_rgba(&mut px, 1, 1) {
        Err(OcioError::BufferSize {
            expected: 4,
            got: 3,
            ..
        }) => {}
        other => panic!("expected BufferSize error, got {other:?}"),
    }
}
