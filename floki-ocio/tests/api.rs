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
    assert!(!cfg.color_spaces().is_empty(), "config must declare color spaces");
    assert!(!cfg.displays().is_empty(), "config must declare displays");
    assert!(!cfg.default_display().is_empty(), "config must have a default display");
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
    proc.apply_rgba(&mut px, 1, 1).expect("apply should succeed");

    // Output must be finite, in display range, and alpha untouched.
    for c in &px[..3] {
        assert!(c.is_finite(), "channel must be finite, got {c}");
        assert!((0.0..=1.0).contains(c), "channel should be in [0,1], got {c}");
    }
    assert_eq!(px[3], 1.0, "alpha must be preserved");
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
        Err(OcioError::BufferSize { expected: 4, got: 3, .. }) => {}
        other => panic!("expected BufferSize error, got {other:?}"),
    }
}
