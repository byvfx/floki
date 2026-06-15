//! Build script for floki-ocio.
//!
//! With no backend feature (`_native` off) this is a no-op, so the crate builds with no
//! C++ toolchain. The `system-ocio` path locates an installed OpenColorIO (via
//! `OPENCOLORIO_ROOT` or Homebrew) and compiles the cxx shim against it. The `vendored-ocio`
//! path (cmake-build the submodule) is added when we cut a distributable build.
//! See plans/i-want-to-map-modular-hinton.md.

fn main() {
    #[cfg(feature = "_native")]
    native::build();
}

#[cfg(feature = "_native")]
mod native {
    use std::path::PathBuf;

    pub fn build() {
        let system = std::env::var_os("CARGO_FEATURE_SYSTEM_OCIO").is_some();
        let vendored = std::env::var_os("CARGO_FEATURE_VENDORED_OCIO").is_some();

        let ocio = if system {
            locate_system_ocio()
        } else if vendored {
            panic!(
                "vendored-ocio (cmake build of vendor/OCIO) is not implemented yet; \
                 build with --features system-ocio against an installed OpenColorIO."
            );
        } else {
            unreachable!("_native is only enabled via system-ocio or vendored-ocio");
        };

        let mut build = cxx_build::bridge("src/ffi.rs");
        build.file("cpp/shim.cpp").std("c++17");
        build.include(&ocio.include);
        // Some OCIO headers may transitively need Imath; add it if discoverable.
        if let Some(imath_include) = locate_imath_include() {
            build.include(imath_include);
        }
        build.compile("floki_ocio_shim");

        println!("cargo:rustc-link-search=native={}", ocio.lib.display());
        println!("cargo:rustc-link-lib=dylib=OpenColorIO");

        println!("cargo:rerun-if-changed=src/ffi.rs");
        println!("cargo:rerun-if-changed=cpp/shim.cpp");
        println!("cargo:rerun-if-changed=cpp/shim.h");
        println!("cargo:rerun-if-env-changed=OPENCOLORIO_ROOT");
        println!("cargo:rerun-if-env-changed=IMATH_ROOT");
    }

    struct OcioPaths {
        include: PathBuf,
        lib: PathBuf,
    }

    fn locate_system_ocio() -> OcioPaths {
        if let Some(root) = std::env::var_os("OPENCOLORIO_ROOT") {
            let root = PathBuf::from(root);
            return OcioPaths {
                include: root.join("include"),
                lib: root.join("lib"),
            };
        }
        if let Some(prefix) = brew_prefix("opencolorio") {
            return OcioPaths {
                include: prefix.join("include"),
                lib: prefix.join("lib"),
            };
        }
        panic!(
            "could not locate OpenColorIO; set OPENCOLORIO_ROOT to its install prefix \
             (containing include/ and lib/)"
        );
    }

    fn locate_imath_include() -> Option<PathBuf> {
        if let Some(root) = std::env::var_os("IMATH_ROOT") {
            return Some(PathBuf::from(root).join("include"));
        }
        brew_prefix("imath").map(|p| p.join("include"))
    }

    fn brew_prefix(formula: &str) -> Option<PathBuf> {
        let out = std::process::Command::new("brew")
            .args(["--prefix", formula])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if p.is_empty() {
            None
        } else {
            Some(PathBuf::from(p))
        }
    }
}
