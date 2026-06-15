//! Native backend: turns the raw cxx bridge into the crate's public types.
//! Only compiled under a backend feature (`vendored-ocio` / `system-ocio`).

use cxx::UniquePtr;

use crate::ffi::bridge;
use crate::{
    ColorSpace, ConfigSource, Display, DisplayTransformRequest, GpuShaderBundle, OcioError, Result,
};

/// Wraps the opaque C++ config handle.
pub struct Config {
    inner: UniquePtr<bridge::OcioConfig>,
}

/// Wraps the opaque C++ CPU processor handle.
pub struct Processor {
    inner: UniquePtr<bridge::OcioCpuProcessor>,
}

pub fn load(src: ConfigSource<'_>) -> Result<Config> {
    let (kind, value) = match src {
        ConfigSource::File(p) => (0u8, p.to_string_lossy().into_owned()),
        ConfigSource::BuiltIn(s) => (1u8, s.to_string()),
        ConfigSource::Env => (2u8, String::new()),
    };
    bridge::load_config(kind, &value)
        .map(|inner| Config { inner })
        .map_err(|e| OcioError::Load(e.what().to_string()))
}

impl Config {
    pub fn color_spaces(&self) -> Vec<ColorSpace> {
        self.inner
            .color_spaces()
            .into_iter()
            .map(|c| ColorSpace {
                name: c.name,
                family: c.family,
                is_data: c.is_data,
            })
            .collect()
    }

    pub fn displays(&self) -> Vec<Display> {
        self.inner
            .displays()
            .into_iter()
            .map(|d| Display {
                name: d.name,
                views: d.views,
                default_view: d.default_view,
            })
            .collect()
    }

    pub fn default_display(&self) -> String {
        self.inner.default_display()
    }

    pub fn scene_linear_colorspace(&self) -> Option<String> {
        let s = self.inner.scene_linear_colorspace();
        if s.is_empty() { None } else { Some(s) }
    }

    pub fn build_cpu_processor(&self, req: &DisplayTransformRequest) -> Result<Processor> {
        self.inner
            .build_cpu_processor(&req.input_colorspace, &req.display, &req.view)
            .map(|inner| Processor { inner })
            .map_err(|e| OcioError::Transform(e.what().to_string()))
    }

    pub fn build_gpu_shader(&self, req: &DisplayTransformRequest) -> Result<GpuShaderBundle> {
        let _ = req;
        // Stage 2: extract GpuShaderDesc, transpile GLSL -> SPIR-V, reflect bindings.
        Err(OcioError::Transpile(
            "GPU shader bundle generation is not implemented yet (Stage 2)".to_string(),
        ))
    }
}

impl Processor {
    pub fn apply_rgba(&self, pixels: &mut [f32], width: usize, height: usize) -> Result<()> {
        self.inner
            .apply_rgba(pixels, width, height)
            .map_err(|e| OcioError::Transform(e.what().to_string()))
    }
}
