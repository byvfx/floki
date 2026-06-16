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
