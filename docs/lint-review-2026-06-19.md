# Lint & Convention Review — 2026-06-19

Automated tooling pass over the whole workspace, focused on **maintainability** and
**Rust / C++ convention adherence**. Companion to the manual
[`audit-2026-06.md`](./audit-2026-06.md) — that one is the prioritized issue list; this one
is the raw tooling output and the read on it. No code was written; this is review only.

## Tooling & baseline

| Tool | Scope | Result |
|---|---|---|
| `cargo clippy` (default, all-targets, all-features) | whole workspace | **0 warnings, 0 errors** ✅ |
| `cargo fmt --check` | whole workspace | **clean** ✅ |
| `cargo clippy` (`-W pedantic -W nursery`) | whole workspace | **610 unique warnings** |
| `clang-format` | `floki-ocio/cpp/shim.{cpp,h}` | no `.clang-format` config present |

Toolchain: `rustc 1.96.0`, `clippy` + `rustfmt` installed. No `clang-tidy` / `cppcheck`
available locally (see C++ section).

The default lint set being clean is a strong signal — the codebase already passes the bar
that most CI gates on. The pedantic/nursery pass is opt-in strictness; treat it as a
prioritization list, not a defect list.

## Where the debt concentrates

Top files by unique pedantic findings:

| file | unique warnings | lines |
|---|---:|---:|
| `src/viewer.rs` | 296 | 4,037 |
| `src/app.rs` | 79 | 2,173 |
| `src/gradient.rs` | 50 | 321 |
| `src/tools.rs` | 38 | 509 |
| `src/gpu/ocio_pass.rs` | 28 | 1,126 |
| `src/background.rs` | 15 | 194 |
| `src/gpu/mod.rs` | 15 | 1,038 |

`viewer.rs` is the clear hotspot — nearly half of all pedantic findings, and the largest
file by ~2×.

## Rust — maintainability findings worth acting on

### 1. Oversized functions (`clippy::too_many_lines`, 15 unique)

The single biggest maintainability issue. Notable offenders:

- `src/app.rs:895` — `ExrApp::ui` is **962 lines**. The eframe update entry point has
  accreted all per-frame orchestration. **#1 refactor target.**
  > **✅ DONE (2026-06).** `ExrApp::ui` is now ~22 lines (`app.rs:983`), delegating to
  > `draw_*` helpers (`draw_menu_bar`, `draw_side_panel`, `draw_central_canvas`, …). The lone
  > remaining oversized fn is `GpuState::new` (`gpu/mod.rs:246-751`, ~506 lines) —
  > tracked in [#66](https://github.com/byvfx/floki/issues/66).
- `src/gpu/mod.rs:242` — `GpuState::new` is **445 lines** (pipeline / shader / layout
  construction).
- `src/gpu/ocio_pass.rs:881` — **224 lines**; `src/viewer.rs:1624` — **230 lines**;
  `src/viewer.rs:1921` — **180**.
- Several more in the 100–147 line range across `viewer.rs`, `gpu/ocio_pass.rs`,
  `color/cube.rs`.

`app.rs::ui` and `gpu/mod.rs::new` in particular are doing many distinct jobs inline;
extracting sub-functions (per-panel UI, per-stage GPU setup) would help testability and
readability the most. Note: the earlier manual audit flagged `ExrViewer::ui`
(`viewer.rs:579`, ~1244 lines) as [#26](https://github.com/byvfx/floki/issues/26) — this is
a *different* giant `ui` (`ExrApp::ui`), so both app- and viewer-level update entry points
are oversized.

### 2. Primitive obsession / bool-heavy structs (`clippy::struct_excessive_bools`, 2)

- `src/app.rs:57` `ExrApp`
- `src/viewer.rs:232` `ExrViewer`

Both already group some related state into sub-structs (`diff_colormap` / `diff_metric` /
`diff_floor`, `background`, …), which is good. The remaining loose bools are candidates for
small typed wrappers or enums (e.g. window-open flags → a `Windows { background: bool,
gradient: bool, … }` struct, or a bool-set). Low urgency, but it pays off as features
accrete.

### 3. Ownership / borrow correctness signals

- **`needless_pass_by_ref_mut` (1)** — `src/viewer.rs:1339`
  `annotation_text_popup(&mut self, …)` takes `&mut self` but doesn't mutate. Signature
  lie; should be `&self`.
- **`unused_self` (2)** — `src/viewer.rs:1133` (`gradient_preview_bar`) and `:2827`
  (`generate_gpu_texture`) don't use `self`. Either they belong on a different type / are
  free functions, or the design intends them as part of the type's API — worth a conscious
  decision (and a doc comment or `#[allow]` if intentional).
- **`significant_drop_tightening` (2)** — `src/viewer.rs:2397` and `:2831`: a temporary with
  a significant `Drop` (read guard / GPU handle) is held longer than necessary. The 2397
  spot already has a scoped block to end the borrow early, but clippy thinks it can be
  tighter — worth a look; holding a read guard across GPU callback setup can serialize
  readers.
- **`redundant_clone` (7)** and **`needless_pass_by_value` (7)** — minor allocation / clone
  avoidable. `src/tools.rs:31-34` takes several args by value without consuming them
  (should be refs).

### 4. API / documentation hygiene (cheap wins, improves library ergonomics)

- **`must_use_candidate` (8)** — public constructors / getters in `floki-ocio/src/lib.rs`
  (199, 211, 223, 235), `app.rs:289`, `exr_loader.rs:113/123`, `tools.rs:25` return values
  that callers silently dropping would be a bug. Adding `#[must_use]` is low-risk and
  high-signal, especially for the `floki-ocio` public API.
- **`missing_errors_doc` (5)** — public `Result`-returning fns in `floki-ocio/src/lib.rs`
  (186, 247, 260, 291) and `exr_loader.rs:41` lack a `# Errors` section. Since `floki-ocio`
  is a published-style crate boundary, these matter most there.
- **`doc_markdown` (4)** / **`too_long_first_doc_paragraph` (2)** / **`doc_link_code` (1)**
  — doc polish.

### 5. Readability micro-issues (bulk of the count, low individual value)

- **`cast_*` family (~220 unique combined)**: `cast_possible_truncation` (92),
  `cast_sign_loss` (69), `cast_precision_loss` (46), `cast_possible_wrap` (6),
  `cast_lossless` (7). Concentrated in `viewer.rs`. Most are `usize→u32`/`f32`,
  `f32→u8`, `f64→f32` in pixel-packing and histogram paths. These are genuinely worth
  auditing *once* for correctness (truncation in image dims / pixel index math can be a
  real bug), but many are intentional and just want `as` → `try_from().unwrap_or(…)` or an
  explicit comment. Recommend a focused pass rather than blanket-allowing.
- **`float_cmp` (16)** — exact `==` on floats. Several are at `viewer.rs` max-channel
  detection (`max - v[0] == 0.0` style) and gamma checks. In color math these are often
  *intentional* (comparing a value known to be exactly the max), so blanket-applying
  epsilon compares would be wrong. Worth case-by-case review with a comment or `#[allow]`
  on the intentional ones.
- **`suboptimal_flops` (67)** — `a*b+c` → `mul_add`. In hot pixel loops this is both faster
  and more accurate on most targets. Good batch-fix candidate, but verify it doesn't change
  reference-output tests.
- **`uninlined_format_args` (45)** / **`format_push_string` (6)** —
  `format!("{}", x)` → `format!("{x}")`, and `s.push_str(&format!(..))` →
  `write!(s, ..)` or `format_args!`. Pure style, zero behavior risk. `cargo clippy --fix`
  handles these automatically.
- **`use_self` (72)** — repeating the type name inside `impl` blocks (e.g.
  `ExrViewer::new` returning `ExrViewer` → `Self`). Mechanical, safe, big count. Also
  auto-fixable.
- **`unreadable_literal` (5)** — long numeric literals wanting `_` separators
  (e.g. `render_math.rs:29`).
- **`similar_names` (8)** / **`many_single_char_names` (5)** — `r`/`g`/`b`/`a` channel vars
  in viewer are expected in color code; likely intentional. `items_after_statements` (3)
  and `useless_let_if_seq` (5) are mild style nits.

### 6. Smaller correctness-adjacent nits

- `manual_let_else` (3): `exr_loader.rs:684`, `ocio_pass.rs:767/884` — `let x = match … {
  Err => return, _ => … }` patterns read better as `let …else`.
- `inconsistent_struct_constructor` (1) at `viewer.rs:2793` — field order in a constructor
  differs from the struct definition; minor risk of silent field-swap bugs on future edits.
- `case_sensitive_file_extension_comparisons` (1) — file-ext check that won't match `.EXR`;
  likely a real UX bug for users on case-sensitive filesystems or Windows. Worth checking.
- `while_float` (1) — a `while` loop on a float condition; can loop forever under rounding.

## C++ review (`floki-ocio/cpp/shim.{cpp,h}`)

Conventionally this is in good shape:

- ✅ RAII throughout: OCIO smart pointers (`ConstConfigRcPtr`, …) held by value in opaque
  classes; `std::unique_ptr` for factory returns; `std::move` on transfers.
- ✅ Correct FFI ownership discipline — the header comment states and the code honors
  "OCIO-owned char*/float* must not escape"; everything is copied into owned `rust::Vec` /
  `rust::String` before crossing back.
- ✅ Anonymous namespace for internal linkage of `to_std` / `to_rust` / `map_language`
  helpers.
- ✅ Null-safety on C-string returns from OCIO (`if (!name) continue;`,
  `s ? rust::String(s) : rust::String()`).
- ✅ Forward declarations ordered before the cxx-generated `ffi.rs.h` include, with a clear
  comment explaining why (the `using` aliases). That's a real subtle dependency and it's
  documented — nice.
- ✅ `explicit` single-arg constructors; `const` methods where appropriate.

Issues / gaps:

- **No `.clang-format` config.** `clang-format` (LLVM defaults) reports ~380 / 57 diff
  lines, but that's purely a 2-space-LLVM-vs-your-4-space-style mismatch — not real
  problems. **Recommendation:** commit a `.clang-format` (the code reads as Google-ish:
  4-space, `const char*`, pointer-left, braced on same line for short fns). Without it,
  format checks are meaningless and contributors will drift.
- **No static analyzer run.** `clang-tidy` and `cppcheck` aren't installed here. The shim
  is small (~220 lines) and hand-audited clean, but a one-time
  `clang-tidy -checks=bugprone-*,cert-*,performance-*,readability-*` pass would be
  worthwhile given the manual pointer / null handling. Note: `build.rs` drives the C++
  build via cxxbridge / cmake, so wiring clang-tidy needs the `compile_commands.json`
  (cmake can emit it).
- Minor: `map_language` has a `case 0: default:` fallthrough with an OCIO-version `#if`
  guard — correct, but the intent (2.4 fallback) is buried; a one-line comment on the
  `case 0` arm would help future readers.

## Suggested priority (by maintainability ROI)

1. ~~**Break up `ExrApp::ui` (962 lines)**~~ (done — see item 1 above) and **`GpuState::new`**
   (now ~506 lines, [#66](https://github.com/byvfx/floki/issues/66)). Highest payoff.
2. **Audit the `cast_*` truncations in `viewer.rs` pixel / dim math** — potential real
   bugs, not just lint.
3. **Add `#[must_use]` + `# Errors` docs to the `floki-ocio` public API** — cheap, raises
   the crate's quality bar at its boundary.
4. **Fix the signature lies**: `needless_pass_by_ref_mut` (viewer:1339), `unused_self`
   (viewer:1133, 2827), `inconsistent_struct_constructor` (viewer:2793),
   `case_sensitive_file_extension_comparisons`.
5. **Mechanical auto-fix batch** (`cargo clippy --fix`): `uninlined_format_args`,
   `use_self`, `redundant_clone`, `format_push_string` — ~130 warnings, zero behavior
   risk, shrinks the noise floor so the signal stands out.
6. **`mul_add` pass** (`suboptimal_flops`) in hot loops — perf + accuracy, but gate behind
   the existing reference tests.
7. **Add a `.clang-format`** and run `clang-tidy` once over the shim.

## Raw counts (unique, for reference)

By lint (top 20):

| lint | count |
|---|---:|
| `cast_possible_truncation` | 92 |
| `use_self` | 72 |
| `cast_sign_loss` | 69 |
| `suboptimal_flops` | 67 |
| `cast_precision_loss` | 46 |
| `uninlined_format_args` | 45 |
| `missing_const_for_fn` | 17 |
| `float_cmp` | 16 |
| `too_many_lines` | 15 |
| `map_unwrap_or` | 13 |
| `option_if_let_else` | 9 |
| `must_use_candidate` | 8 |
| `similar_names` | 8 |
| `default_trait_access` | 8 |
| `needless_pass_by_value` | 7 |
| `redundant_clone` | 7 |
| `cast_lossless` | 7 |
| `format_push_string` | 6 |
| `cast_possible_wrap` | 6 |
| `unnecessary_debug_formatging` | 6 |

All 47 distinct lints with counts and `file:line` locations are in the full JSON output;
re-generate with:

```sh
cargo clippy --all-targets --all-features --message-format=json \
  -- -W clippy::pedantic -W clippy::nursery -A clippy::cargo > clippy.json
```
