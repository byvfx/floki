# #33 — Single-part decode gap on Apple Silicon: workstation validation (agent handoff)

> **Read this first, then do the work below.** This is a handoff from a session
> that ran the #33 decode benchmarks on an **Apple A18/A19 "MacBook Neo"**
> (phone-class ARM, 8 GB). Your job is to re-run the same controlled benchmark on
> **this x86 workstation** and decide whether the measured gap is *ARM-specific*
> (fixable with a NEON tier) or a *general* fork inefficiency.

## TL;DR — the one question to answer

On the Neo, floki's **single-part** EXR decode is **1.8–2.9× slower than OpenEXR**
across *every* codec (worst on PIZ and DWA). Multi-part decode is fine (floki is
competitive-to-faster — cross-part rayon parallelism).

**Validate on x86:** does the gap close on this workstation?
- **DWA closes → ~1.0×** but PIZ/ZIP stay ~2× ⇒ the DWA gap is the missing **ARM
  NEON IDCT** (fix = add a NEON tier to `byvfx/exrs`); PIZ/ZIP is a separate,
  general gap.
- **Everything closes** ⇒ the whole thing is Apple-Silicon-only (fork simply isn't
  tuned for ARM) — decide whether a Mac-review tool cares.
- **Nothing closes** ⇒ the fork's per-part decode is just slower than OpenEXR
  regardless of arch (then #33's only real lever stays "decode fewer parts").

> ⚠️ **Trust the RATIO, not the absolute ms.** The Neo inflates every number. The
> floki-vs-OpenEXR ratio on the *same machine + same pixels* cancels the hardware
> and isolates the decoder implementation. Report ratios.

## Context (what floki is / what #33 is)

- floki = Rust GPU EXR **review player**. Decodes EXR via a patched fork of the
  `exrs` crate: `byvfx/exrs`, branch `miniz-inflate-1.74.1` (see the repo root
  `Cargo.toml` `[patch.crates-io]`). The patch swaps zune-inflate → miniz_oxide
  (panic-safety) and rebased onto upstream v1.74.1 (which added DWA decode).
- #33 = "speed up `ExrData::load`". Full write-up + the earlier verdict:
  **`docs/perf-roadmap.md`** section *"1. Faster first-pass decode — #33"*.
- **Established already (do not re-litigate):** no full OpenEXR rewrite — on
  multi-part dailies the fork wins or ties, and a rewrite costs the miniz
  panic-fix + a C++ dep + 3-platform static build. This investigation is only
  about the **single-part gap on Apple Silicon**.

## Root cause found so far (partly confirmed in source)

**DWA:** the fork's inverse-DCT is **x86-only SIMD + scalar fallback, no NEON.**
In the exrs checkout (`~/.cargo/git/checkouts/exrs-*/*/src/compression/dwa/idct.rs`)
the dispatch `dct_inverse_8x8_batch` is:

```rust
pub fn dct_inverse_8x8_batch<'a>(blocks: ...) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    { if let Some(v3) = V3::try_new() { /* AVX2 */ return }
      if let Some(v1) = V1::try_new() { /* SSE2 */ return } }
    dct_inverse_8x8_scalar(data)   // ← what ALL Apple Silicon runs
}
```

So on x86 you get AVX2; on arm64 you get scalar. **`pulp` 0.22 ships an aarch64
backend** (`~/.cargo/registry/src/*/pulp-0.22.3/src/aarch64.rs`), so a NEON tier
is addable by mirroring the V1/V3 tiers.

**PIZ** has no DCT, yet it's the *worst* codec on the Neo (2.9×) — so there is a
**second, broader** single-part gap (inflate + f16/f32 channel unpack) that the DWA
fix would not touch. The x86 numbers you collect will tell us if PIZ/ZIP is
also ARM-scalar or general.

## Apple Silicon baseline (A18/A19) — reproduce this table on x86

Same 4608×3164×3-channel plate, 5 compressions (identical pixels), warm cache,
best/median, all cores. `load` = full `ExrData::load`.

| Codec | floki `load` | OpenEXR ref | ratio (floki ÷ OpenEXR) |
|-------|-------------:|------------:|:------------------------|
| ZIP   |  65 ms |  28 ms | **2.3×** |
| ZIPS  |  81 ms |  46 ms | **1.8×** |
| DWAA  | 100 ms |  40 ms | **2.5×** |
| DWAB  | 109 ms |  46 ms | **2.4×** |
| PIZ   | 148 ms |  51 ms | **2.9×** |

(For reference, on the same Neo, `load_beauty`/`load_proxy` — the playback paths —
stay ~fast and are not the concern here.)

---

## RESULTS — x86 workstation (Windows 11, 2026-07-15)

**Machine:** Intel Core i9-13980HX (24C/32T, AVX2), Windows 11, all cores.
**Plate:** beauty part (subimage 0) of the `redSea` multi-part plate, **3225×2215
half RGBA (4ch), single-part**, re-encoded to 5 identical-pixel codec variants with
`oiiotool`. (Neo used a 4608×3164×3ch plate; different pixel count — the *ratio*
still cancels the machine, but don't compare absolute ms across the two tables.)
**floki:** criterion `/load` median (warm, sample_size 10). **OpenEXR:** `exrbench`
all-parts, best of 3 runs (each internally best-of-3), `setGlobalThreadCount(32)`.

| Codec | floki `/load` (median) | OpenEXR (best) | **x86 ratio** | Neo ratio |
|-------|-----------------------:|---------------:|:-------------:|:---------:|
| ZIP   | 33.0 ms | 24.2 ms | **1.36×** | 2.3× |
| ZIPS  | 38.4 ms | 36.4 ms | **1.05×** | 1.8× |
| DWAA  | 43.5 ms | 29.9 ms | **1.46×** | 2.5× |
| DWAB  | 62.6 ms | 39.5 ms | **1.59×** | 2.4× |
| PIZ   | 38.5 ms | 31.0 ms | **1.24×** | 2.9× |

(floki `/load` ≈ `/load_beauty` here, as expected for a single-part file. DWA
correctness re-confirmed: `exr_loader::tests::loads_dwaa_and_dwab_compressed_exrs`
passes, and `p_dwaa.exr` opens correctly.)

### Verdict: **outcome 2 — the gap is overwhelmingly Apple-Silicon-specific.**

*Everything* closed on x86. The Neo's 1.8–2.9× spread collapses to 1.05–1.59×;
ZIPS essentially ties OpenEXR (1.05×). This is **not** outcome 1 (DWA→1.0× while
PIZ/ZIP stay ~2×) — PIZ, the *worst* codec on the Neo (2.9×), closes to 1.24×, so
its penalty was almost entirely the ARM **scalar** inflate/wavelet/unpack paths, not
anything DCT-related.

Two things the x86 numbers *do* isolate:
- **The ARM regression is broad, not DWA-only.** All five codecs regress ~1.8–2.9×
  on Apple Silicon. A NEON DCT tier touches only DWA; it cannot fix the ZIP/ZIPS/PIZ
  ARM slowdowns (those have no DCT).
- **DWA carries a real, arch-independent ~1.5× gap.** DWAA/DWAB are the *worst*
  residual on x86 (1.46× / 1.59×) even with AVX2 — so the fork's DWA decode trails
  OpenEXR's by ~1.5× on *both* arches, independent of NEON.

**Implication for the NEON-tier decision:** a NEON IDCT would help ARM DWA, but at
best pulls ARM DWA down toward this x86 ~1.5× floor — not to parity — and does
nothing for the (larger, broader) ARM regression in the non-DWA codecs. So the
doc's original "just add NEON DWA" framing is only a *partial* ARM win. The bigger
ARM lever is the general scalar decode paths (inflate + f16/f32 channel unpack);
the ~1.5× cross-arch DWA inefficiency is a separate, smaller optimization worth a
look on both arches. Net: single-part decode on x86 is already competitive
(1.0–1.6×) — no urgent action; #33's ARM story is "broad scalar regression," and
the NEON tier alone is not the fix it was hypothesized to be.

---

## Windows (x86) procedure — how these numbers were produced

The doc's macOS/Linux steps adapted for this Windows box. Tooling was already on the
network share one level above the repo (`G:\__projects\_programming`): `oiiotool` +
`exrinfo` from a USD build, and OpenEXR 3.4 + Imath + OpenColorIO 2.4 from a vcpkg
tree. No installs were needed.

**Tool locations**
- `oiiotool.exe`, `exrinfo.exe`: `G:\__projects\_programming\usd_25_11\bin` — invoke
  with **both** `usd_25_11\bin` *and* `usd_25_11\lib` on `PATH` (its boost/OIIO DLLs
  live in `lib`; without it you get `0xC0000135` DLL-not-found).
- OpenEXR 3.4 / Imath / OpenColorIO dev libs: `G:\__projects\_programming\vcpkg\installed\x64-windows`
  (`include`, `lib`, `bin`).
- MSVC 2022: init via `…\VC\Auxiliary\Build\vcvars64.bat`.

**0. Toolchain gotcha.** The pulled tree needs **rustc ≥ 1.92** (egui 0.34.3); this
box had 1.90 — `rustup update stable` (→ 1.97). Building floki's bench also compiles
`floki-ocio`, which pulls **shaderc** (built from source → needs `cmake` + `ninja` +
`python` + `git` on PATH; VS bundles cmake/ninja, Anaconda supplies python) and
requires an **OpenColorIO** backend. Point `system-ocio` at vcpkg instead of a
Homebrew/`OPENCOLORIO_ROOT` install:
```
set OPENCOLORIO_ROOT=G:\__projects\_programming\vcpkg\installed\x64-windows
set IMATH_ROOT=%OPENCOLORIO_ROOT%
```
and keep `%VCPKG%\bin` on PATH at *run* time so the bench exe finds `OpenColorIO_2_4.dll`.
(All of the above is scripted in a single `.bat` that calls `vcvars64` then prepends
cmake/ninja/python/git/vcpkg-bin to PATH before `cargo bench`.)

**1–2. Single-part plate + identical-pixel variants** (PowerShell; `$USD\bin;$USD\lib` on PATH):
```powershell
$OIIO = "G:\__projects\_programming\usd_25_11\bin\oiiotool.exe"
$SRC  = "assets\perf\TPLS2_206_206-0370_render_v006.redSea_bty.1078.exr"  # multi-part
$DW   = "$env:TEMP\dwatest"; New-Item -ItemType Directory -Force $DW | Out-Null
& $OIIO $SRC --subimage 0 --compression zips -o "$DW\_base.exr"           # -> single-part beauty
foreach ($c in 'zip','zips','dwaa','dwab','piz') { & $OIIO "$DW\_base.exr" --compression $c -o "$DW\p_$c.exr" }
Remove-Item "$DW\_base.exr"                                               # bench only the 5 variants
& "G:\__projects\_programming\usd_25_11\bin\exrinfo.exe" "$DW\p_dwaa.exr" # sanity: 1 part, dwaa
```

**3. floki decode** (inside the MSVC/OCIO-env .bat):
```
set FLOKI_PERF_FIXTURES=%TEMP%\dwatest
cargo bench --bench exr_load -- exr_load/local
```

**4. OpenEXR C++ reference** — same `exrbench.cpp` as below, built with **MSVC** and
vcpkg OpenEXR 3.4 (swap the clang/Homebrew line for):
```
cl /std:c++17 /O2 /EHsc exrbench.cpp ^
   /I "%VCPKG%\include" /I "%VCPKG%\include\OpenEXR" /I "%VCPKG%\include\Imath" ^
   /Fe:exrbench.exe /link /LIBPATH:"%VCPKG%\lib" OpenEXR-3_4.lib Imath-3_2.lib Iex-3_4.lib IlmThread-3_4.lib
set "PATH=%VCPKG%\bin;%PATH%"            & rem runtime DLLs
for %%c in (zip zips dwaa dwab piz) do .\exrbench.exe "%TEMP%\dwatest\p_%%c.exr"
```

---

## Do this on the workstation

### 0. Prereqs
- Build floki here (from this branch). `cargo build` (dev = `system-ocio`).
- OpenEXR + Imath dev libs (for the reference bench) and `oiiotool` (OpenImageIO)
  for generating codec variants. On Linux: `apt install openexr libopenexr-dev
  openimageio-tools` (or the distro equivalent); adjust the include/lib paths in
  the build line below.

### 1. Pick a real 4K-ish single-part plate
Any single-part beauty plate works. Note its native compression with
`exrinfo file.exr` (or `exrheader`). Put it at `PLATE=/path/to/plate.exr`.

### 2. Generate identical-pixel codec variants (controlled comparison)
```sh
DW=/tmp/dwatest; mkdir -p "$DW"
oiiotool "$PLATE" --compression zip  -o "$DW/p_zip.exr"
oiiotool "$PLATE" --compression zips -o "$DW/p_zips.exr"
oiiotool "$PLATE" --compression dwaa -o "$DW/p_dwaa.exr"
oiiotool "$PLATE" --compression dwab -o "$DW/p_dwab.exr"
oiiotool "$PLATE" --compression piz  -o "$DW/p_piz.exr"
```

### 3. floki decode times
The `exr_load` bench (this branch) benches `load` / `load_beauty` / `load_proxy`
per file in `$FLOKI_PERF_FIXTURES` (default `assets/perf/`):
```sh
FLOKI_PERF_FIXTURES="$DW" cargo bench --bench exr_load -- exr_load/local
```
Record the **`/load`** median per codec.

### 4. OpenEXR reference (eager decode, same threading, read-loop only)
Save as `exrbench.cpp`, build, run on each variant. It times *only* the pixel-read
loop (file open excluded), all parts, `setGlobalThreadCount(cores)` — apples to
apples with floki's `load`.

```cpp
// exrbench.cpp — OpenEXR eager-decode reference for floki #33.
#include <ImfMultiPartInputFile.h>
#include <ImfInputPart.h>
#include <ImfChannelList.h>
#include <ImfFrameBuffer.h>
#include <ImfThreading.h>
#include <ImathBox.h>
#include <chrono>
#include <cstdio>
#include <thread>
#include <vector>
using namespace Imf; using namespace Imath;
static double decode(const char* path, int limit, int reps) {
    double best = 1e18;
    for (int r = 0; r < reps; r++) {
        MultiPartInputFile file(path);
        int nparts = file.parts();
        int last = (limit > 0 && limit < nparts) ? limit : nparts;
        auto t0 = std::chrono::steady_clock::now();
        for (int p = 0; p < last; p++) {
            InputPart in(file, p);
            Box2i dw = in.header().dataWindow();
            int w = dw.max.x - dw.min.x + 1, h = dw.max.y - dw.min.y + 1;
            const ChannelList& chans = in.header().channels();
            FrameBuffer fb; std::vector<std::vector<char>> bufs;
            for (auto it = chans.begin(); it != chans.end(); ++it) {
                PixelType pt = it.channel().type;
                size_t px = (pt == HALF) ? 2 : 4;
                bufs.emplace_back((size_t)w * h * px);
                char* base = bufs.back().data();
                char* origin = base - ((size_t)dw.min.x + (size_t)dw.min.y * w) * px;
                fb.insert(it.name(), Slice(pt, origin, px, (size_t)w * px));
            }
            in.setFrameBuffer(fb);
            in.readPixels(dw.min.y, dw.max.y);
        }
        auto t1 = std::chrono::steady_clock::now();
        double ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
        if (ms < best) best = ms;
    }
    return best;
}
int main(int argc, char** argv) {
    if (argc < 2) { fprintf(stderr, "usage: exrbench <file.exr>\n"); return 1; }
    setGlobalThreadCount((int)std::thread::hardware_concurrency());
    decode(argv[1], 1, 1);                 // warm
    double all = decode(argv[1], 0, 3);    // all parts  -> vs load
    double beauty = decode(argv[1], 1, 3); // part 0     -> vs load_beauty
    printf("%8.1f ms all-parts  %8.1f ms beauty  %s\n", all, beauty, argv[1]);
    return 0;
}
```

Build (macOS/Homebrew example — swap paths on Linux):
```sh
OEXR=/opt/homebrew/opt/openexr; IMATH=/opt/homebrew/opt/imath
clang++ -std=c++17 -O2 exrbench.cpp -o exrbench \
  -I$OEXR/include -I$OEXR/include/OpenEXR -I$IMATH/include -I$IMATH/include/Imath \
  -L$OEXR/lib -L$IMATH/lib -lOpenEXR -lImath
for f in "$DW"/p_*.exr; do ./exrbench "$f"; done   # (set DYLD/LD_LIBRARY_PATH if needed)
```

### 5. Report
For each codec: `floki /load` vs `OpenEXR all-parts`, and the ratio. Compare the
ratios to the Neo table above. **Also verify DWA correctness** — floki must open
DWAA/DWAB and the pixels must match (the fork's decode is already tested, but sanity
check a variant opens in the app).

## The fix (only if x86 shows DWA closing to ~1.0×)

> **Status (2026-07-15, x86 run above):** precondition **not** cleanly met — DWA did
> not close to ~1.0× on x86 (it's the *worst* residual at ~1.5×), and PIZ/ZIP did
> *not* stay ~2× (they closed too). So a NEON tier is at best a partial ARM win, not
> the fix. Kept below for when/if it's revisited (measure on the Mac first).

Add an **aarch64 NEON tier** to `byvfx/exrs` `src/compression/dwa/idct.rs`,
mirroring the x86 `V1`/`V3` tiers, using pulp's aarch64 NEON token; gate the new
block `#[cfg(target_arch = "aarch64")]` and add it to the `dct_inverse_8x8_batch`
dispatch above the scalar fallback. Then re-bench **on the Mac** to measure how much
DWA recovers. This is a fork PR (`byvfx/exrs`), then repoint floki's Cargo patch.
Watch: PIZ/ZIP won't be helped by this — if they're also ARM-scalar, that's a
separate optimization (or accept it, since it's single-part-only and floki's
playback paths use beauty/proxy anyway).

## Guardrails
- Don't rewrite floki's decoder to OpenEXR bindings — that decision is settled
  (see `docs/perf-roadmap.md` #33).
- Multi-part decode is already good — don't optimize it.
- Keep any fork change on a branch; measure before/after on **both** an Apple
  Silicon Mac and this x86 box (the whole point is the arch split).
