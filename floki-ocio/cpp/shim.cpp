#include "floki-ocio/cpp/shim.h"

#include <string>

namespace floki_ocio {

namespace {

// rust::Str -> null-terminated std::string for OCIO's const char* APIs.
std::string to_std(rust::Str s) { return std::string(s.data(), s.size()); }

// const char* (possibly null) -> rust::String.
rust::String to_rust(const char* s) { return s ? rust::String(s) : rust::String(); }

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

rust::String OcioConfig::default_display() const {
    return to_rust(cfg_->getDefaultDisplay());
}

rust::String OcioConfig::scene_linear_colorspace() const {
    OCIO::ConstColorSpaceRcPtr cs = cfg_->getColorSpace(OCIO::ROLE_SCENE_LINEAR);
    return cs ? to_rust(cs->getName()) : rust::String();
}

std::unique_ptr<OcioCpuProcessor> OcioConfig::build_cpu_processor(
    rust::Str input_cs, rust::Str display, rust::Str view) const {
    OCIO::DisplayViewTransformRcPtr transform = OCIO::DisplayViewTransform::Create();
    transform->setSrc(to_std(input_cs).c_str());
    transform->setDisplay(to_std(display).c_str());
    transform->setView(to_std(view).c_str());

    OCIO::ConstProcessorRcPtr proc = cfg_->getProcessor(transform);
    OCIO::ConstCPUProcessorRcPtr cpu = proc->getDefaultCPUProcessor();
    return std::make_unique<OcioCpuProcessor>(std::move(cpu));
}

namespace {

OCIO::GpuLanguage map_language(std::uint8_t language) {
    switch (language) {
        case 1:
            return OCIO::GPU_LANGUAGE_GLSL_VK_4_6;
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
    rust::Str input_cs, rust::Str display, rust::Str view, std::uint8_t language) const {
    OCIO::DisplayViewTransformRcPtr transform = OCIO::DisplayViewTransform::Create();
    transform->setSrc(to_std(input_cs).c_str());
    transform->setDisplay(to_std(display).c_str());
    transform->setView(to_std(view).c_str());

    OCIO::ConstProcessorRcPtr proc = cfg_->getProcessor(transform);
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
        copy_values(values,
                    static_cast<std::size_t>(edgelen) * edgelen * edgelen * 3,
                    t.data);
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
        const std::uint8_t channels =
            channel == OCIO::GpuShaderDesc::TEXTURE_RGB_CHANNEL ? 3 : 1;
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
        copy_values(values,
                    static_cast<std::size_t>(width) * eff_height * channels,
                    t.data);
        out.textures.push_back(std::move(t));
    }

    return out;
}

void OcioCpuProcessor::apply_rgba(
    rust::Slice<float> pixels, std::size_t width, std::size_t height) const {
    OCIO::PackedImageDesc img(
        pixels.data(),
        static_cast<long>(width),
        static_cast<long>(height),
        4 /* channels: RGBA */);
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
