//! GLSL (from OCIO) -> SPIR-V (shaderc) -> reflect + WGSL (naga).
//!
//! OCIO emits only declarations + helper functions + a `vec4 <fn>(vec4)` entry function (no
//! `#version`/IO/`main`), so we wrap it into a complete fragment shader, compile to SPIR-V,
//! then reflect the bindings (so consumers build bind group layouts from the shader, never
//! hardcoded) and emit WGSL for inspection / WGSL-only consumers.

use naga::valid::{Capabilities, ValidationFlags, Validator};

use crate::ffi::bridge::OcioShaderData;
use crate::{BindingInfo, BindingKind, OcioError, Result, TexDim};

/// Descriptor set used for the scene-input texture/sampler. OCIO's LUTs live in set 0.
pub const SCENE_SET: u32 = 1;
pub const SCENE_TEX_BINDING: u32 = 0;
pub const SCENE_SAMPLER_BINDING: u32 = 1;

/// OCIO emits *combined* samplers (`samplerND`), which WebGPU/naga reject. We split each into
/// a separate texture + sampler using these name suffixes (shared with the texture repacker so
/// reflection names line up with the returned `LutTexture`s).
pub fn tex_var(combined_sampler_name: &str) -> String {
    format!("{combined_sampler_name}_tex")
}
pub fn smp_var(combined_sampler_name: &str) -> String {
    format!("{combined_sampler_name}_smp")
}

pub struct Transpiled {
    pub spirv: Vec<u32>,
    pub wgsl: Option<String>,
    pub bindings: Vec<BindingInfo>,
    pub entry_point: String,
}

pub fn transpile(ocio: &OcioShaderData) -> Result<Transpiled> {
    let glsl = wrap_glsl(ocio);
    let spirv = compile_to_spirv(&glsl)?;
    let (bindings, wgsl) = reflect_and_emit_wgsl(&spirv)?;
    Ok(Transpiled {
        spirv,
        wgsl: Some(wgsl),
        bindings,
        entry_point: "main".to_string(),
    })
}

/// Wrap OCIO's emitted code (GLSL 4.0 target, combined samplers, no layout decorations) into a
/// complete Vulkan-GLSL fragment shader, splitting every combined sampler into a separate
/// texture + sampler with explicit bindings.
fn wrap_glsl(ocio: &OcioShaderData) -> String {
    let mut body = ocio.glsl.clone();
    let mut decls = String::new();

    // Scene input lives in its own descriptor set so it never collides with OCIO's LUTs.
    decls.push_str(&format!(
        "layout(set = {SCENE_SET}, binding = {SCENE_TEX_BINDING}) uniform texture2D ocio_scene_tex;\n"
    ));
    decls.push_str(&format!(
        "layout(set = {SCENE_SET}, binding = {SCENE_SAMPLER_BINDING}) uniform sampler ocio_scene_smp;\n"
    ));

    for (i, t) in ocio.textures.iter().enumerate() {
        let dim_kw = match t.dim {
            1 => "1D",
            2 => "2D",
            _ => "3D",
        };
        let combined = t.sampler_name.as_str();
        let tex = tex_var(combined);
        let smp = smp_var(combined);
        let tex_binding = (i as u32) * 2;
        let smp_binding = (i as u32) * 2 + 1;

        decls.push_str(&format!(
            "layout(set = 0, binding = {tex_binding}) uniform texture{dim_kw} {tex};\n"
        ));
        decls.push_str(&format!(
            "layout(set = 0, binding = {smp_binding}) uniform sampler {smp};\n"
        ));

        // Drop OCIO's combined declaration and rewrite its sampling calls to the split form.
        body = body.replace(&format!("uniform sampler{dim_kw} {combined};"), "");
        body = body.replace(
            &format!("texture({combined},"),
            &format!("texture(sampler{dim_kw}({tex}, {smp}),"),
        );
    }

    format!(
        "#version 460\n\
         layout(location = 0) in vec2 v_uv;\n\
         layout(location = 0) out vec4 o_color;\n\
         {decls}\n{body}\n\
         void main() {{\n    \
         vec4 inColor = texture(sampler2D(ocio_scene_tex, ocio_scene_smp), v_uv);\n    \
         o_color = {func}(inColor);\n\
         }}\n",
        func = ocio.function_name,
    )
}

fn compile_to_spirv(glsl: &str) -> Result<Vec<u32>> {
    let compiler = shaderc::Compiler::new()
        .map_err(|e| OcioError::Transpile(format!("shaderc init failed: {e}")))?;
    let mut options = shaderc::CompileOptions::new()
        .map_err(|e| OcioError::Transpile(format!("shaderc options failed: {e}")))?;
    options.set_target_env(
        shaderc::TargetEnv::Vulkan,
        shaderc::EnvVersion::Vulkan1_2 as u32,
    );
    // Leave SPIR-V unoptimized: glslang's optimizer can emit constructs naga's SPIR-V
    // frontend rejects. wgpu/naga lowers to the native backend anyway.
    options.set_optimization_level(shaderc::OptimizationLevel::Zero);

    let artifact = compiler
        .compile_into_spirv(
            glsl,
            shaderc::ShaderKind::Fragment,
            "ocio_display.frag",
            "main",
            Some(&options),
        )
        .map_err(|e| OcioError::Transpile(format!("GLSL->SPIR-V: {e}")))?;
    Ok(artifact.as_binary().to_vec())
}

fn reflect_and_emit_wgsl(spirv: &[u32]) -> Result<(Vec<BindingInfo>, String)> {
    let options = naga::front::spv::Options::default();
    let module = naga::front::spv::Frontend::new(spirv.iter().copied(), &options)
        .parse()
        .map_err(|e| OcioError::Transpile(format!("SPIR-V parse (naga): {e}")))?;

    let info = Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .map_err(|e| OcioError::Transpile(format!("SPIR-V validation (naga): {e}")))?;

    let mut bindings = Vec::new();
    for (_h, var) in module.global_variables.iter() {
        let Some(rb) = &var.binding else { continue };
        let kind = match &module.types[var.ty].inner {
            naga::TypeInner::Image { dim, .. } => BindingKind::Texture(map_dim(*dim)),
            naga::TypeInner::Sampler { .. } => BindingKind::Sampler,
            _ => BindingKind::UniformBuffer,
        };
        bindings.push(BindingInfo {
            group: rb.group,
            binding: rb.binding,
            name: var.name.clone().unwrap_or_default(),
            kind,
        });
    }
    bindings.sort_by_key(|b| (b.group, b.binding));

    let wgsl =
        naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
            .map_err(|e| OcioError::Transpile(format!("WGSL emit (naga): {e}")))?;

    Ok((bindings, wgsl))
}

fn map_dim(dim: naga::ImageDimension) -> TexDim {
    match dim {
        naga::ImageDimension::D1 => TexDim::D1,
        naga::ImageDimension::D2 => TexDim::D2,
        naga::ImageDimension::D3 => TexDim::D3,
        naga::ImageDimension::Cube => TexDim::D2,
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    fn round_trip(glsl: &str) -> Result<()> {
        let spirv = compile_to_spirv(glsl)?;
        reflect_and_emit_wgsl(&spirv).map(|_| ())
    }

    #[test]
    fn separate_sampler_parses() {
        let glsl = "#version 460\n\
            layout(location=0) out vec4 o;\n\
            layout(set=0, binding=0) uniform texture2D t;\n\
            layout(set=0, binding=1) uniform sampler s;\n\
            void main() { o = texture(sampler2D(t, s), vec2(0.5)); }\n";
        round_trip(glsl).expect("separate texture+sampler should round-trip through naga");
    }

    #[test]
    fn combined_sampler_is_rejected() {
        // Documents why we split OCIO's combined samplers: naga/WebGPU reject combined
        // image-samplers. If this ever starts passing, the split could be simplified.
        let glsl = "#version 460\n\
            layout(location=0) out vec4 o;\n\
            layout(set=0, binding=0) uniform sampler2D c;\n\
            void main() { o = texture(c, vec2(0.5)); }\n";
        assert!(
            round_trip(glsl).is_err(),
            "naga unexpectedly accepted a combined image-sampler"
        );
    }
}
