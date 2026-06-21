//! Native backend: turns the raw cxx bridge into the crate's public types.
//! Only compiled under a backend feature (`vendored-ocio` / `system-ocio`).

use cxx::UniquePtr;

use crate::ffi::bridge;
use crate::{
    ColorSpace, ConfigSource, Display, DisplayTransformRequest, GpuShaderBundle, Interp,
    LutTexture, OcioError, Result, TexDim,
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
            .build_cpu_processor(
                &req.input_colorspace,
                &req.display,
                &req.view,
                req.bake_lut_size,
            )
            .map(|inner| Processor { inner })
            .map_err(|e| OcioError::Transform(e.what().to_string()))
    }

    pub fn build_gpu_shader(&self, req: &DisplayTransformRequest) -> Result<GpuShaderBundle> {
        // language 0 = GLSL 4.0. We split its combined samplers into separate texture+sampler
        // (the 4.0 target declares samplers without layout prefixes, so it patches cleanly).
        let data = self
            .inner
            .build_gpu_shader(
                &req.input_colorspace,
                &req.display,
                &req.view,
                0,
                req.bake_lut_size,
            )
            .map_err(|e| OcioError::Transform(e.what().to_string()))?;

        let transpiled = crate::transpile::transpile(&data)?;
        let textures = data.textures.iter().map(repack_texture).collect();

        Ok(GpuShaderBundle {
            spirv: transpiled.spirv,
            wgsl: transpiled.wgsl,
            entry_point: transpiled.entry_point,
            textures,
            // floki applies exposure in its own pass; OCIO dynamic properties are not wired
            // yet (none for the built-in display transforms). See plan, Stage 2 notes.
            dynamic_props: Vec::new(),
            bindings: transpiled.bindings,
        })
    }
}

/// Repack OCIO's raw 1- or 3-channel LUT data to interleaved RGBA f32 (alpha = 1.0); wgpu
/// has no `Rgb32Float`. A 1-channel (RED) LUT goes into R (the OCIO shader samples `.r`).
fn repack_texture(t: &bridge::OcioTexture) -> LutTexture {
    let texels = (t.width as usize) * (t.height.max(1) as usize) * (t.depth.max(1) as usize);
    let ch = t.channels as usize;
    let mut data_rgba = Vec::with_capacity(texels * 4);
    for i in 0..texels {
        let base = i * ch;
        let (r, g, b) = if ch >= 3 {
            (t.data[base], t.data[base + 1], t.data[base + 2])
        } else {
            // 1-/2-channel LUT: replicate nothing — put the single channel in R and
            // leave G/B at 0. `unwrap_or(0.0)` only guards a malformed texture whose
            // declared size exceeds its data; 0.0 (black) is the safe neutral there.
            (t.data.get(base).copied().unwrap_or(0.0), 0.0, 0.0)
        };
        data_rgba.extend_from_slice(&[r, g, b, 1.0]);
    }

    LutTexture {
        // Names match the split texture/sampler the transpiler emits, so consumers can map
        // each LUT to its reflected bindings by name.
        name: crate::transpile::tex_var(&t.sampler_name),
        sampler_name: crate::transpile::smp_var(&t.sampler_name),
        dim: match t.dim {
            3 => TexDim::D3,
            2 => TexDim::D2,
            _ => TexDim::D1,
        },
        width: t.width,
        height: t.height,
        depth: t.depth,
        interpolation: match t.interpolation {
            1 => Interp::Nearest,
            3 => Interp::Tetrahedral,
            _ => Interp::Linear,
        },
        source_channels: t.channels,
        data_rgba,
    }
}

impl Processor {
    pub fn apply_rgba(&self, pixels: &mut [f32], width: usize, height: usize) -> Result<()> {
        self.inner
            .apply_rgba(pixels, width, height)
            .map_err(|e| OcioError::Transform(e.what().to_string()))
    }
}

#[cfg(test)]
mod dump_tests {
    use super::*;

    // Inspection helper (not an assertion): dumps the OCIO-generated GLSL + texture summary
    // so we can design the transpile wrapper. Run with:
    //   cargo test -p floki-ocio --features system-ocio dump_glsl -- --nocapture --ignored
    #[test]
    #[ignore]
    fn dump_glsl() {
        for cfg_name in ["ocio://default", "ocio://studio-config-latest"] {
            let cfg = match load(ConfigSource::BuiltIn(cfg_name)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("\n### {cfg_name}: load failed: {e}\n");
                    continue;
                }
            };
            let input = cfg
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

            for (lang, label) in [(0u8, "GLSL_4_0"), (1u8, "GLSL_VK_4_6")] {
                let data = match crate::ffi::bridge::load_config(1, cfg_name)
                    .and_then(|c| c.build_gpu_shader(&input, &display, &view, lang, 0))
                {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("\n### {cfg_name} [{label}]: {}\n", e.what());
                        continue;
                    }
                };
                eprintln!(
                    "\n========== {cfg_name} | {input} -> {display}/{view} | {label} ==========",
                );
                eprintln!("-- {} texture(s):", data.textures.len());
                for t in &data.textures {
                    eprintln!(
                        "   {} (sampler {}): dim={} {}x{}x{} ch={} interp={} data_len={}",
                        t.name,
                        t.sampler_name,
                        t.dim,
                        t.width,
                        t.height,
                        t.depth,
                        t.channels,
                        t.interpolation,
                        t.data.len()
                    );
                }
                eprintln!("-- GLSL ({} bytes):\n{}", data.glsl.len(), data.glsl);
            }
        }
    }
}
