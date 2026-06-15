//! cxx bridge to the C++ OCIO shim (`cpp/shim.{h,cpp}`).
//!
//! Only compiled under a backend feature. Everything crossing the boundary is either an
//! opaque `UniquePtr` handle or an owned POD shared struct — no OCIO smart pointers or raw
//! pointers leak across. Functions that can throw `OCIO::Exception` return `Result`, which
//! cxx maps from the C++ exception.

#[cxx::bridge(namespace = "floki_ocio")]
pub mod bridge {
    /// Flat description of one color space.
    struct ColorSpaceInfo {
        name: String,
        family: String,
        is_data: bool,
    }

    /// Flat description of one display device and its views.
    struct DisplayInfo {
        name: String,
        views: Vec<String>,
        default_view: String,
    }

    unsafe extern "C++" {
        include!("floki-ocio/cpp/shim.h");

        /// Opaque wrapper around `OCIO::ConstConfigRcPtr`.
        type OcioConfig;
        /// Opaque wrapper around `OCIO::ConstCPUProcessorRcPtr`.
        type OcioCpuProcessor;

        /// Load a config. `kind`: 0 = file path, 1 = built-in name (e.g. "ocio://default"),
        /// 2 = `$OCIO` env (value ignored).
        fn load_config(kind: u8, value: &str) -> Result<UniquePtr<OcioConfig>>;

        fn color_spaces(self: &OcioConfig) -> Vec<ColorSpaceInfo>;
        fn displays(self: &OcioConfig) -> Vec<DisplayInfo>;
        fn default_display(self: &OcioConfig) -> String;
        /// Color space bound to the `scene_linear` role, or empty if none.
        fn scene_linear_colorspace(self: &OcioConfig) -> String;

        fn build_cpu_processor(
            self: &OcioConfig,
            input_cs: &str,
            display: &str,
            view: &str,
        ) -> Result<UniquePtr<OcioCpuProcessor>>;

        /// Apply the transform in place to interleaved RGBA f32 (`pixels.len() == w*h*4`).
        fn apply_rgba(
            self: &OcioCpuProcessor,
            pixels: &mut [f32],
            width: usize,
            height: usize,
        ) -> Result<()>;
    }
}
