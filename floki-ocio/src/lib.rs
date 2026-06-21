//! `floki-ocio` — a UI- and graphics-API-agnostic wrapper around OpenColorIO (OCIO v2).
//!
//! The crate exposes three capabilities, all independent of any GUI or GPU runtime:
//!
//! 1. **Config loading + enumeration** ([`OcioConfig`]) — load a config from a file,
//!    a built-in (`ocio://default`), or `$OCIO`, and list its color spaces / displays / views.
//! 2. **CPU processing** ([`CpuProcessor`]) — apply a display transform to a pixel buffer
//!    in place. Useful for thumbnails, batch baking, and resolving texture color spaces.
//! 3. **GPU shader bundles** ([`GpuShaderBundle`]) — an OCIO-generated fragment shader as
//!    SPIR-V (+ optional WGSL) plus its LUT textures and binding reflection, ready for the
//!    consumer to map onto wgpu / its own GPU backend.
//!
//! ## Backends
//!
//! A native OCIO must be linked via one of the cargo features:
//! - `vendored-ocio` — build OCIO from the vendored submodule (cmake).
//! - `system-ocio` — link an externally-provided OCIO.
//!
//! With neither feature, every OCIO call returns [`OcioError::NotCompiled`]; the crate still
//! compiles (pure Rust) so it does not force a C++ toolchain on unrelated builds.

use std::path::Path;

mod error;
pub use error::{OcioError, Result};

// Native backend (cxx shim + transpile). Only compiled when a backend feature is enabled.
#[cfg(feature = "_native")]
mod backend;
#[cfg(feature = "_native")]
mod ffi;
#[cfg(feature = "_native")]
mod transpile;

// ---------------------------------------------------------------------------
// Config source
// ---------------------------------------------------------------------------

/// Where to load an OCIO config from.
#[derive(Debug, Clone)]
pub enum ConfigSource<'a> {
    /// A `.ocio` file on disk.
    File(&'a Path),
    /// A built-in config string, e.g. `"ocio://default"` or `"ocio://studio-config-latest"`.
    BuiltIn(&'a str),
    /// Read the config pointed to by the `$OCIO` environment variable.
    Env,
}

// ---------------------------------------------------------------------------
// Enumeration types (plain data)
// ---------------------------------------------------------------------------

/// A color space declared in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorSpace {
    pub name: String,
    pub family: String,
    /// `true` if OCIO marks this space as raw data (no color transform should apply).
    pub is_data: bool,
}

/// A display device and the views available on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub name: String,
    pub views: Vec<String>,
    pub default_view: String,
}

/// A request to build the transform from a working/input space to a display+view.
#[derive(Debug, Clone)]
pub struct DisplayTransformRequest {
    /// Input color space, e.g. a role like `"scene_linear"` or a name like `"ACEScg"`.
    pub input_colorspace: String,
    /// Display device name, e.g. `"sRGB - Display"`.
    pub display: String,
    /// View name on that display, e.g. `"ACES 1.0 - SDR Video"`.
    pub view: String,
    /// Bake the display transform to a 3D LUT of this edge length (e.g. `33` or `65`), so the
    /// GPU shader becomes a cheap `texture3D` lookup (fronted by a log2 shaper) instead of the
    /// full analytic ACES ALU. `0` (the default) keeps the analytic shader; values `< 2` are
    /// treated as `0`. Baking trades a small, fixed amount of LUT-interpolation error for a
    /// large per-pixel speedup — useful for smooth pan/zoom on weak GPUs.
    pub bake_lut_size: u32,
}

// ---------------------------------------------------------------------------
// GPU shader bundle (graphics-API-agnostic)
// ---------------------------------------------------------------------------

/// Texture sampling dimensionality of an OCIO LUT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexDim {
    D1,
    D2,
    D3,
}

/// Interpolation requested for a LUT's sampler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interp {
    Nearest,
    Linear,
    /// 3D-only; the tetrahedral topology is encoded in the shader, the sampler stays linear.
    Tetrahedral,
}

/// A LUT texture emitted by OCIO, repacked to consumer-friendly layout.
#[derive(Debug, Clone)]
pub struct LutTexture {
    /// GLSL variable name OCIO emitted (matches a [`BindingInfo`]).
    pub name: String,
    /// Sampler name OCIO emitted.
    pub sampler_name: String,
    pub dim: TexDim,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub interpolation: Interp,
    /// Number of channels in the OCIO source data (1 = RED, 3 = RGB). Lets the consumer
    /// pick a texture format; `data_rgba` is already padded to RGBA regardless.
    pub source_channels: u8,
    /// Texel data, **already repacked to interleaved RGBA f32** (alpha = 1.0) so consumers
    /// never deal with OCIO's 3-float-per-texel layout (wgpu has no `Rgb32Float`).
    pub data_rgba: Vec<f32>,
}

/// A dynamic property OCIO exposed as a shader uniform (driven at render time).
#[derive(Debug, Clone)]
pub struct DynamicProp {
    pub kind: DynPropKind,
    /// The uniform name OCIO referenced in the generated shader.
    pub uniform_name: String,
    pub default: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynPropKind {
    Exposure,
    Gamma,
    Contrast,
}

/// What a reflected shader binding points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingKind {
    Texture(TexDim),
    Sampler,
    UniformBuffer,
}

/// A binding discovered by reflecting the compiled SPIR-V. The consumer builds its bind
/// group layout from these rather than hardcoding — guarantees the layout matches the shader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    pub group: u32,
    pub binding: u32,
    pub name: String,
    pub kind: BindingKind,
}

/// Everything a consumer needs to run the OCIO display transform on the GPU, with no
/// dependency on any specific graphics API.
#[derive(Debug, Clone)]
pub struct GpuShaderBundle {
    /// Compiled fragment shader (entry point [`Self::entry_point`]).
    pub spirv: Vec<u32>,
    /// Transpiled WGSL, for inspection or WGSL-only consumers. `None` if not requested/available.
    pub wgsl: Option<String>,
    pub entry_point: String,
    pub textures: Vec<LutTexture>,
    pub dynamic_props: Vec<DynamicProp>,
    pub bindings: Vec<BindingInfo>,
}

// ---------------------------------------------------------------------------
// OcioConfig
// ---------------------------------------------------------------------------

/// A loaded OCIO configuration. Cheap to query; building processors/shaders is the costly part.
pub struct OcioConfig {
    #[cfg(feature = "_native")]
    inner: backend::Config,
    #[cfg(not(feature = "_native"))]
    #[allow(dead_code)]
    _private: (),
}

impl OcioConfig {
    /// Load a config from a file, a built-in name, or `$OCIO`.
    ///
    /// # Errors
    /// Returns [`OcioError::NotCompiled`] when built without an OCIO backend feature, or
    /// [`OcioError::Load`] if the config cannot be found, read, or parsed.
    pub fn load(src: ConfigSource<'_>) -> Result<Self> {
        #[cfg(feature = "_native")]
        {
            backend::load(src).map(|inner| OcioConfig { inner })
        }
        #[cfg(not(feature = "_native"))]
        {
            let _ = src;
            Err(OcioError::NotCompiled)
        }
    }

    /// All color spaces declared in the config.
    #[must_use]
    pub fn color_spaces(&self) -> Vec<ColorSpace> {
        #[cfg(feature = "_native")]
        {
            self.inner.color_spaces()
        }
        #[cfg(not(feature = "_native"))]
        {
            Vec::new()
        }
    }

    /// All displays (each carrying its views and default view).
    #[must_use]
    pub fn displays(&self) -> Vec<Display> {
        #[cfg(feature = "_native")]
        {
            self.inner.displays()
        }
        #[cfg(not(feature = "_native"))]
        {
            Vec::new()
        }
    }

    /// The config's default display name.
    #[must_use]
    pub fn default_display(&self) -> String {
        #[cfg(feature = "_native")]
        {
            self.inner.default_display()
        }
        #[cfg(not(feature = "_native"))]
        {
            String::new()
        }
    }

    /// The color space bound to the `scene_linear` role, if any.
    #[must_use]
    pub fn scene_linear_colorspace(&self) -> Option<String> {
        #[cfg(feature = "_native")]
        {
            self.inner.scene_linear_colorspace()
        }
        #[cfg(not(feature = "_native"))]
        {
            None
        }
    }

    /// Build a GPU shader bundle for the given input→display/view transform.
    ///
    /// # Errors
    /// Returns [`OcioError::NotCompiled`] when built without an OCIO backend feature,
    /// [`OcioError::Transform`] if the transform cannot be built from the config, or
    /// [`OcioError::Transpile`] if the generated shader cannot be translated.
    pub fn build_gpu_shader(&self, req: &DisplayTransformRequest) -> Result<GpuShaderBundle> {
        #[cfg(feature = "_native")]
        {
            self.inner.build_gpu_shader(req)
        }
        #[cfg(not(feature = "_native"))]
        {
            let _ = req;
            Err(OcioError::NotCompiled)
        }
    }

    /// Build a CPU processor for the given input→display/view transform.
    ///
    /// # Errors
    /// Returns [`OcioError::NotCompiled`] when built without an OCIO backend feature, or
    /// [`OcioError::Transform`] if the transform cannot be built from the config.
    pub fn build_cpu_processor(&self, req: &DisplayTransformRequest) -> Result<CpuProcessor> {
        #[cfg(feature = "_native")]
        {
            self.inner
                .build_cpu_processor(req)
                .map(|inner| CpuProcessor { inner })
        }
        #[cfg(not(feature = "_native"))]
        {
            let _ = req;
            Err(OcioError::NotCompiled)
        }
    }
}

// ---------------------------------------------------------------------------
// CpuProcessor
// ---------------------------------------------------------------------------

/// A baked CPU color transform. Applies in place; safe to share and reuse across threads
/// (e.g. rayon over image tiles).
pub struct CpuProcessor {
    #[cfg(feature = "_native")]
    inner: backend::Processor,
    #[cfg(not(feature = "_native"))]
    #[allow(dead_code)]
    _private: (),
}

impl CpuProcessor {
    /// Apply the transform to interleaved RGBA f32 pixels, in place.
    ///
    /// # Errors
    /// Returns [`OcioError::BufferSize`] if `pixels.len()` is not `width * height * 4`, or
    /// [`OcioError::NotCompiled`] when built without an OCIO backend feature.
    pub fn apply_rgba(&self, pixels: &mut [f32], width: usize, height: usize) -> Result<()> {
        let expected = width.saturating_mul(height).saturating_mul(4);
        if pixels.len() != expected {
            return Err(OcioError::BufferSize {
                got: pixels.len(),
                width,
                height,
                channels: 4,
                expected,
            });
        }
        #[cfg(feature = "_native")]
        {
            self.inner.apply_rgba(pixels, width, height)
        }
        #[cfg(not(feature = "_native"))]
        {
            Err(OcioError::NotCompiled)
        }
    }
}
