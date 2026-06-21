#include "floki-ocio/cpp/shim.h"

#include <string>

namespace floki_ocio {

namespace {

// rust::Str -> null-terminated std::string for OCIO's const char* APIs.
std::string to_std(rust::Str s) { return std::string(s.data(), s.size()); }

// const char* (possibly null) -> rust::String.
rust::String to_rust(const char* s) { return s ? rust::String(s) : rust::String(); }

// Log2-shaper stop range for baked-LUT allocation: maps HDR scene-linear into [0,1] for the
// 3D LUT domain. Concentrated on the range a display transform actually resolves — below
// ~2^-10 is display-black and above ~2^8 clips to display-white, so spending grid samples
// outside this band just coarsens the midtones/highlights where accuracy matters. Values
// outside clamp to the LUT edge (black / white), which is the correct display behaviour.
constexpr float kShaperMinStop = -10.0f;
constexpr float kShaperMaxStop = 8.0f;

// Build the (input -> display/view) DisplayViewTransform.
OCIO::DisplayViewTransformRcPtr make_display_view(rust::Str input_cs, rust::Str display,
                                                  rust::Str view) {
    OCIO::DisplayViewTransformRcPtr t = OCIO::DisplayViewTransform::Create();
    t->setSrc(to_std(input_cs).c_str());
    t->setDisplay(to_std(display).c_str());
    t->setView(to_std(view).c_str());
    return t;
}

// Bake the display transform to a [log2-shaper -> 3D LUT] GroupTransform. The LUT is filled
// by evaluating [shaper(inverse) -> display] on the grid, so at runtime shaper(forward) then
// the LUT reproduces the display transform by construction (the shaper cancels) — trading the
// analytic ACES ALU for one texture lookup. `lut_size` is the grid edge length (e.g. 33/65).
OCIO::GroupTransformRcPtr make_baked_display_transform(const OCIO::ConstConfigRcPtr& cfg,
                                                       rust::Str input_cs, rust::Str display,
                                                       rust::Str view, std::uint32_t lut_size) {
    const float vars[2] = {kShaperMinStop, kShaperMaxStop};

    // Forward shaper applied at runtime: scene-linear -> shaped [0,1].
    OCIO::AllocationTransformRcPtr shaper = OCIO::AllocationTransform::Create();
    shaper->setAllocation(OCIO::ALLOCATION_LG2);
    shaper->setVars(2, vars);

    // Fill pipeline: shaped [0,1] -> (inverse shaper) -> scene-linear -> display.
    OCIO::AllocationTransformRcPtr shaper_inv = OCIO::AllocationTransform::Create();
    shaper_inv->setAllocation(OCIO::ALLOCATION_LG2);
    shaper_inv->setVars(2, vars);
    shaper_inv->setDirection(OCIO::TRANSFORM_DIR_INVERSE);

    OCIO::GroupTransformRcPtr fill = OCIO::GroupTransform::Create();
    fill->appendTransform(shaper_inv);
    fill->appendTransform(make_display_view(input_cs, display, view));
    OCIO::ConstCPUProcessorRcPtr fill_cpu = cfg->getProcessor(fill)->getDefaultCPUProcessor();

    // Evaluate the fill pipeline at each grid point to populate the 3D LUT.
    OCIO::Lut3DTransformRcPtr lut = OCIO::Lut3DTransform::Create();
    lut->setGridSize(lut_size);
    // Tetrahedral interpolation tracks the curved ACES tone mapping far better than trilinear
    // at the same grid size; the GPU consumer already supports it.
    lut->setInterpolation(OCIO::INTERP_TETRAHEDRAL);
    const float denom = static_cast<float>(lut_size - 1);
    for (std::uint32_t ir = 0; ir < lut_size; ++ir) {
        for (std::uint32_t ig = 0; ig < lut_size; ++ig) {
            for (std::uint32_t ib = 0; ib < lut_size; ++ib) {
                float rgb[3] = {
                    static_cast<float>(ir) / denom,
                    static_cast<float>(ig) / denom,
                    static_cast<float>(ib) / denom,
                };
                fill_cpu->applyRGB(rgb);
                lut->setValue(ir, ig, ib, rgb[0], rgb[1], rgb[2]);
            }
        }
    }

    OCIO::GroupTransformRcPtr group = OCIO::GroupTransform::Create();
    group->appendTransform(shaper);
    group->appendTransform(lut);
    return group;
}

// Processor for either the analytic display transform (lut_size < 2) or the baked LUT.
OCIO::ConstProcessorRcPtr make_display_processor(const OCIO::ConstConfigRcPtr& cfg,
                                                 rust::Str input_cs, rust::Str display,
                                                 rust::Str view, std::uint32_t lut_size) {
    if (lut_size >= 2) {
        return cfg->getProcessor(
            make_baked_display_transform(cfg, input_cs, display, view, lut_size));
    }
    return cfg->getProcessor(make_display_view(input_cs, display, view));
}

} // namespace

rust::Vec<ColorSpaceInfo> OcioConfig::color_spaces() const {
    rust::Vec<ColorSpaceInfo> out;
    const int n = cfg_->getNumColorSpaces();
    for (int i = 0; i < n; ++i) {
        const char* name = cfg_->getColorSpaceNameByIndex(i);
        if (!name) {
            continue;
        }
        OCIO::ConstColorSpaceRcPtr cs = cfg_->getColorSpace(name);
        ColorSpaceInfo info;
        info.name = to_rust(name);
        info.family = cs ? to_rust(cs->getFamily()) : rust::String();
        info.is_data = cs ? cs->isData() : false;
        out.push_back(std::move(info));
    }
    return out;
}

rust::Vec<DisplayInfo> OcioConfig::displays() const {
    rust::Vec<DisplayInfo> out;
    const int n = cfg_->getNumDisplays();
    for (int i = 0; i < n; ++i) {
        const char* disp = cfg_->getDisplay(i);
        if (!disp) {
            continue;
        }
        DisplayInfo info;
        info.name = to_rust(disp);
        info.default_view = to_rust(cfg_->getDefaultView(disp));
        const int nv = cfg_->getNumViews(disp);
        for (int j = 0; j < nv; ++j) {
            const char* view = cfg_->getView(disp, j);
            if (view) {
                info.views.push_back(to_rust(view));
            }
        }
        out.push_back(std::move(info));
    }
    return out;
}

rust::String OcioConfig::default_display() const { return to_rust(cfg_->getDefaultDisplay()); }

rust::String OcioConfig::scene_linear_colorspace() const {
    OCIO::ConstColorSpaceRcPtr cs = cfg_->getColorSpace(OCIO::ROLE_SCENE_LINEAR);
    return cs ? to_rust(cs->getName()) : rust::String();
}

std::unique_ptr<OcioCpuProcessor> OcioConfig::build_cpu_processor(
    rust::Str input_cs, rust::Str display, rust::Str view, std::uint32_t lut_size) const {
    OCIO::ConstProcessorRcPtr proc =
        make_display_processor(cfg_, input_cs, display, view, lut_size);
    OCIO::ConstCPUProcessorRcPtr cpu = proc->getDefaultCPUProcessor();
    return std::make_unique<OcioCpuProcessor>(std::move(cpu));
}

namespace {

OCIO::GpuLanguage map_language(std::uint8_t language) {
    switch (language) {
#if OCIO_VERSION_HEX >= 0x02050000
        // Vulkan-flavored GLSL (separate texture/sampler objects); OCIO 2.5+ only. On 2.4.x
        // this case falls through to GLSL_4_0 (combined samplers, split in transpile.rs).
        case 1:
            return OCIO::GPU_LANGUAGE_GLSL_VK_4_6;
#endif
        case 0:
        default:
            return OCIO::GPU_LANGUAGE_GLSL_4_0;
    }
}

void copy_values(const float* values, std::size_t count, rust::Vec<float>& out) {
    out.reserve(count);
    for (std::size_t k = 0; k < count; ++k) {
        out.push_back(values[k]);
    }
}

} // namespace

OcioShaderData OcioConfig::build_gpu_shader(
    rust::Str input_cs, rust::Str display, rust::Str view, std::uint8_t language,
    std::uint32_t lut_size) const {
    OCIO::ConstProcessorRcPtr proc =
        make_display_processor(cfg_, input_cs, display, view, lut_size);
    OCIO::ConstGPUProcessorRcPtr gpu = proc->getDefaultGPUProcessor();

    OCIO::GpuShaderDescRcPtr desc = OCIO::GpuShaderDesc::CreateShaderDesc();
    desc->setLanguage(map_language(language));
    desc->setFunctionName("OCIODisplay");
    desc->setResourcePrefix("ocio_");
    gpu->extractGpuShaderInfo(desc);

    OcioShaderData out;
    out.glsl = to_rust(desc->getShaderText());
    out.function_name = rust::String("OCIODisplay");

    // 3D LUTs (RGB, edgelen^3 texels).
    const unsigned n3 = desc->getNum3DTextures();
    for (unsigned i = 0; i < n3; ++i) {
        const char* tex_name = nullptr;
        const char* samp_name = nullptr;
        unsigned edgelen = 0;
        OCIO::Interpolation interp = OCIO::INTERP_DEFAULT;
        desc->get3DTexture(i, tex_name, samp_name, edgelen, interp);

        const float* values = nullptr;
        desc->get3DTextureValues(i, values);

        OcioTexture t;
        t.name = to_rust(tex_name);
        t.sampler_name = to_rust(samp_name);
        t.dim = 3;
        t.width = edgelen;
        t.height = edgelen;
        t.depth = edgelen;
        t.channels = 3;
        t.interpolation = static_cast<std::uint8_t>(interp);
        copy_values(values, static_cast<std::size_t>(edgelen) * edgelen * edgelen * 3, t.data);
        out.textures.push_back(std::move(t));
    }

    // 1D / 2D LUTs.
    const unsigned n = desc->getNumTextures();
    for (unsigned i = 0; i < n; ++i) {
        const char* tex_name = nullptr;
        const char* samp_name = nullptr;
        unsigned width = 0;
        unsigned height = 0;
        OCIO::GpuShaderDesc::TextureType channel = OCIO::GpuShaderDesc::TEXTURE_RGB_CHANNEL;
        OCIO::GpuShaderDesc::TextureDimensions dims = OCIO::GpuShaderDesc::TEXTURE_1D;
        OCIO::Interpolation interp = OCIO::INTERP_DEFAULT;
        desc->getTexture(i, tex_name, samp_name, width, height, channel, dims, interp);

        const float* values = nullptr;
        desc->getTextureValues(i, values);

        const bool is_2d = dims == OCIO::GpuShaderDesc::TEXTURE_2D;
        const std::uint8_t channels = channel == OCIO::GpuShaderDesc::TEXTURE_RGB_CHANNEL ? 3 : 1;
        const unsigned eff_height = is_2d ? height : 1;

        OcioTexture t;
        t.name = to_rust(tex_name);
        t.sampler_name = to_rust(samp_name);
        t.dim = is_2d ? 2 : 1;
        t.width = width;
        t.height = eff_height;
        t.depth = 1;
        t.channels = channels;
        t.interpolation = static_cast<std::uint8_t>(interp);
        copy_values(values, static_cast<std::size_t>(width) * eff_height * channels, t.data);
        out.textures.push_back(std::move(t));
    }

    return out;
}

void OcioCpuProcessor::apply_rgba(
    rust::Slice<float> pixels, std::size_t width, std::size_t height) const {
    OCIO::PackedImageDesc img(
        pixels.data(), static_cast<long>(width), static_cast<long>(height), 4 /* channels: RGBA */);
    cpu_->apply(img);
}

std::unique_ptr<OcioConfig> load_config(std::uint8_t kind, rust::Str value) {
    OCIO::ConstConfigRcPtr cfg;
    switch (kind) {
        case 0: // file
            cfg = OCIO::Config::CreateFromFile(to_std(value).c_str());
            break;
        case 1: { // built-in, e.g. "ocio://default" or "default"
            std::string name = to_std(value);
            const std::string scheme = "ocio://";
            if (name.rfind(scheme, 0) == 0) {
                name = name.substr(scheme.size());
            }
            cfg = OCIO::Config::CreateFromBuiltinConfig(name.c_str());
            break;
        }
        case 2: // $OCIO
        default:
            cfg = OCIO::Config::CreateFromEnv();
            break;
    }
    return std::make_unique<OcioConfig>(std::move(cfg));
}

} // namespace floki_ocio
