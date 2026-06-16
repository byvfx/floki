//! Build script for floki-ocio.
//!
//! With no backend feature (`_native` off) this is a no-op, so the crate builds with no
//! C++ toolchain. Two backends compile the cxx shim against a real OpenColorIO:
//!
//! * `system-ocio` — links an installed OCIO (via `OPENCOLORIO_ROOT` or Homebrew). Dev default.
//! * `vendored-ocio` — statically builds OCIO 2.4.2 from the `vendor/OCIO` submodule via cmake,
//!   for a self-contained distributable binary (end users install nothing).
//!
//! If both are somehow enabled, `vendored-ocio` wins (the point of vendoring is self-containment).

fn main() {
    #[cfg(feature = "_native")]
    native::build();
}

#[cfg(feature = "_native")]
mod native {
    use std::path::PathBuf;

    /// The chosen OCIO backend's compile + link inputs.
    struct Backend {
        /// Primary include dir holding `OpenColorIO/OpenColorIO.h`.
        include: PathBuf,
        /// Additional include dirs (e.g. vendored Imath).
        extra_includes: Vec<PathBuf>,
        /// Library search dirs.
        link_search: Vec<PathBuf>,
        /// `(kind, name)` link directives, in link order. `kind` is `static` or `dylib`.
        link_libs: Vec<(&'static str, String)>,
    }

    pub fn build() {
        // Vendored takes precedence: a self-contained static build should never fall back to a
        // system OCIO even if both features are on (the app's `ocio-vendored` enables `ocio`,
        // which carries `system-ocio`). Selected at compile time so the `cmake` build-dep (only
        // present under `vendored-ocio`) is never referenced by a system-only build.
        #[cfg(feature = "vendored-ocio")]
        let backend = build_vendored_ocio();
        #[cfg(all(feature = "system-ocio", not(feature = "vendored-ocio")))]
        let backend = link_system_ocio();

        // Compile the cxx shim against the chosen OCIO's headers.
        let mut build = cxx_build::bridge("src/ffi.rs");
        build.file("cpp/shim.cpp").std("c++17");
        build.include(&backend.include);
        for inc in &backend.extra_includes {
            build.include(inc);
        }
        build.compile("floki_ocio_shim");

        // Emit link directives (search dirs first, then libs in order).
        for dir in &backend.link_search {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
        for (kind, name) in &backend.link_libs {
            println!("cargo:rustc-link-lib={kind}={name}");
        }

        println!("cargo:rerun-if-changed=src/ffi.rs");
        println!("cargo:rerun-if-changed=cpp/shim.cpp");
        println!("cargo:rerun-if-changed=cpp/shim.h");
        println!("cargo:rerun-if-env-changed=OPENCOLORIO_ROOT");
        println!("cargo:rerun-if-env-changed=IMATH_ROOT");
    }

    // -- system backend -----------------------------------------------------------------

    #[cfg(all(feature = "system-ocio", not(feature = "vendored-ocio")))]
    fn link_system_ocio() -> Backend {
        let (include, lib) = if let Some(root) = std::env::var_os("OPENCOLORIO_ROOT") {
            let root = PathBuf::from(root);
            (root.join("include"), root.join("lib"))
        } else if let Some(prefix) = brew_prefix("opencolorio") {
            (prefix.join("include"), prefix.join("lib"))
        } else {
            panic!(
                "could not locate OpenColorIO; set OPENCOLORIO_ROOT to its install prefix \
                 (containing include/ and lib/), or build with --features vendored-ocio"
            );
        };

        let mut extra_includes = Vec::new();
        if let Some(imath) = locate_imath_include() {
            extra_includes.push(imath);
        }

        Backend {
            include,
            extra_includes,
            link_search: vec![lib],
            link_libs: vec![("dylib", "OpenColorIO".to_string())],
        }
    }

    #[cfg(all(feature = "system-ocio", not(feature = "vendored-ocio")))]
    fn locate_imath_include() -> Option<PathBuf> {
        if let Some(root) = std::env::var_os("IMATH_ROOT") {
            return Some(PathBuf::from(root).join("include"));
        }
        brew_prefix("imath").map(|p| p.join("include"))
    }

    #[cfg(all(feature = "system-ocio", not(feature = "vendored-ocio")))]
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

    // -- vendored backend ---------------------------------------------------------------

    #[cfg(feature = "vendored-ocio")]
    fn build_vendored_ocio() -> Backend {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let src = manifest.join("vendor/OCIO");
        assert!(
            src.join("CMakeLists.txt").exists(),
            "OCIO submodule missing at {} — run `git submodule update --init --recursive`",
            src.display()
        );

        // Statically build OCIO + its ext deps (Imath, yaml-cpp, expat, pystring, minizip-ng).
        // `OCIO_INSTALL_EXT_PACKAGES=ALL` builds and installs those deps into the prefix so a
        // downstream static consumer can link them.
        let mut config = cmake::Config::new(&src);
        config
            .profile("Release")
            .define("BUILD_SHARED_LIBS", "OFF")
            .define("OCIO_BUILD_PYTHON", "OFF")
            .define("OCIO_BUILD_APPS", "OFF")
            .define("OCIO_BUILD_TESTS", "OFF")
            .define("OCIO_BUILD_GPU_TESTS", "OFF")
            .define("OCIO_BUILD_DOCS", "OFF")
            .define("OCIO_BUILD_NUKE", "OFF")
            .define("OCIO_BUILD_OPENFX", "OFF")
            .define("OCIO_INSTALL_EXT_PACKAGES", "ALL")
            .define("OCIO_WARNING_AS_ERROR", "OFF")
            // OCIO's bundled ext deps (expat, etc.) declare `cmake_minimum_required` < 3.5,
            // which CMake 4 rejects. The ext deps build as separate ExternalProject cmake
            // invocations that don't inherit a top-level `-D`, so set the compat shim via the
            // environment (CMake 4 honors `CMAKE_POLICY_VERSION_MINIMUM` from env, and child
            // cmake processes inherit it). Keep the `-D` too for the top-level configure.
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .env("CMAKE_POLICY_VERSION_MINIMUM", "3.5");

        // OCIO's bundled (old) zlib defines `fdopen` -> NULL when `TARGET_OS_MAC` is defined,
        // which every modern macOS SDK sets — clashing with the SDK's real `fdopen` declaration.
        // `-Dfdopen=fdopen` makes zlib's `#ifndef fdopen` guard skip that bad define. Passed via
        // CFLAGS env so it reaches the ext ExternalProject sub-builds (which read CFLAGS).
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            let mut cflags = std::env::var("CFLAGS").unwrap_or_default();
            if !cflags.is_empty() {
                cflags.push(' ');
            }
            cflags.push_str("-Dfdopen=fdopen");
            config.env("CFLAGS", cflags);
        }

        let dst = config.build();

        // Gather installed static archives. OpenColorIO installs to <prefix>/lib, but the ext
        // deps (Imath, yaml-cpp, expat, pystring, minizip-ng, zlib) install into OCIO's separate
        // ext "dist" tree under the build dir, not the main prefix — scan both.
        let mut link_search = Vec::new();
        let mut statics: Vec<String> = Vec::new();
        for d in [
            dst.join("lib"),
            dst.join("lib64"),
            dst.join("build/ext/dist/lib"),
            dst.join("build/ext/dist/lib64"),
        ] {
            if d.is_dir() {
                collect_static_libs(&d, &mut statics);
                link_search.push(d);
            }
        }
        statics.sort();
        statics.dedup();
        assert!(
            statics.iter().any(|n| n.contains("OpenColorIO")),
            "vendored OCIO build installed no OpenColorIO static lib under {}",
            dst.display()
        );

        // Link order matters for static archives on GNU ld: a library must come before the
        // libs it depends on. OpenColorIO is referenced by our shim and references the ext
        // deps, so emit it first, then the rest.
        let mut ordered: Vec<String> = Vec::new();
        if let Some(pos) = statics.iter().position(|n| n == "OpenColorIO") {
            ordered.push(statics.remove(pos));
        }
        ordered.append(&mut statics);

        let mut link_libs: Vec<(&'static str, String)> =
            ordered.into_iter().map(|n| ("static", n)).collect();
        link_libs.extend(runtime_libs());

        Backend {
            include: dst.join("include"),
            // OCIO installs Imath headers under <prefix>/include/Imath; OCIO public headers
            // include them as <Imath/...>, so <prefix>/include already covers it, but add the
            // subdir too for shims that include them unqualified.
            extra_includes: vec![dst.join("include/Imath")],
            link_search,
            link_libs,
        }
    }

    /// Strip platform prefixes/suffixes from static archive filenames into link names.
    /// `libFoo.a` -> `Foo` (unix), `Foo.lib` -> `Foo` (Windows/MSVC).
    #[cfg(feature = "vendored-ocio")]
    fn collect_static_libs(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(stem) = name.strip_prefix("lib").and_then(|s| s.strip_suffix(".a")) {
                out.push(stem.to_string());
            } else if let Some(stem) = name.strip_suffix(".lib") {
                out.push(stem.to_string());
            }
        }
    }

    /// C++ runtime + system libs the static OCIO core needs, linked *after* the OCIO archives.
    #[cfg(feature = "vendored-ocio")]
    fn runtime_libs() -> Vec<(&'static str, String)> {
        let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        let mut v: Vec<(&'static str, String)> = Vec::new();
        match os.as_str() {
            "macos" | "ios" => v.push(("dylib", "c++".to_string())),
            "linux" | "android" => {
                v.push(("dylib", "stdc++".to_string()));
                v.push(("dylib", "m".to_string()));
                v.push(("dylib", "dl".to_string()));
                v.push(("dylib", "pthread".to_string()));
            }
            "windows" if env == "gnu" => v.push(("dylib", "stdc++".to_string())),
            // MSVC links its C++ runtime automatically.
            _ => {}
        }
        v
    }
}
