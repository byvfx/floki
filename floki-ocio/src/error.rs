use thiserror::Error;

/// Errors surfaced by the floki-ocio API.
#[derive(Error, Debug)]
pub enum OcioError {
    /// The crate was built without an OCIO backend feature
    /// (`vendored-ocio` or `system-ocio`), so no native OCIO is linked.
    #[error(
        "floki-ocio was compiled without an OCIO backend; \
         enable the `vendored-ocio` or `system-ocio` feature"
    )]
    NotCompiled,

    /// Failed to load or parse an OCIO config.
    #[error("failed to load OCIO config: {0}")]
    Load(String),

    /// Failed to build a processor / transform from the config.
    #[error("failed to build OCIO transform: {0}")]
    Transform(String),

    /// Failed to transpile the OCIO-generated GLSL to SPIR-V/WGSL.
    #[error("failed to transpile OCIO shader: {0}")]
    Transpile(String),

    /// A buffer handed to a CPU processor had the wrong length for its dimensions.
    #[error("pixel buffer length {got} does not match {width}x{height}x{channels} (expected {expected})")]
    BufferSize {
        got: usize,
        width: usize,
        height: usize,
        channels: usize,
        expected: usize,
    },
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, OcioError>;
