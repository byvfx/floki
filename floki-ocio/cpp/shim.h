// C++ shim exposing a cxx-friendly slice of the OpenColorIO API.
//
// Opaque classes hold OCIO smart pointers; all data returned to Rust is copied into owned
// rust::* containers inside each call (OCIO-owned char*/float* must not escape).
#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>

#include <OpenColorIO/OpenColorIO.h>

namespace OCIO = OCIO_NAMESPACE;

// Forward-declare the opaque types BEFORE the cxx-generated header: it emits
// `using OcioConfig = ::floki_ocio::OcioConfig;` aliases that need these names visible.
namespace floki_ocio {
class OcioConfig;
class OcioCpuProcessor;
} // namespace floki_ocio

// Brings in the shared structs (ColorSpaceInfo, DisplayInfo) and rust::* types.
#include "floki-ocio/src/ffi.rs.h"

namespace floki_ocio {

class OcioConfig {
public:
    explicit OcioConfig(OCIO::ConstConfigRcPtr cfg) : cfg_(std::move(cfg)) {}

    rust::Vec<ColorSpaceInfo> color_spaces() const;
    rust::Vec<DisplayInfo> displays() const;
    rust::String default_display() const;
    rust::String scene_linear_colorspace() const;

    std::unique_ptr<OcioCpuProcessor> build_cpu_processor(
        rust::Str input_cs, rust::Str display, rust::Str view) const;

    OcioShaderData build_gpu_shader(
        rust::Str input_cs, rust::Str display, rust::Str view, std::uint8_t language) const;

private:
    OCIO::ConstConfigRcPtr cfg_;
};

class OcioCpuProcessor {
public:
    explicit OcioCpuProcessor(OCIO::ConstCPUProcessorRcPtr cpu) : cpu_(std::move(cpu)) {}

    void apply_rgba(rust::Slice<float> pixels, std::size_t width, std::size_t height) const;

private:
    OCIO::ConstCPUProcessorRcPtr cpu_;
};

std::unique_ptr<OcioConfig> load_config(std::uint8_t kind, rust::Str value);

} // namespace floki_ocio
