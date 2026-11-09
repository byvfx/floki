# `slm-spike` — throwaway measurement rig for #358

Not part of floki. This crate exists to answer one question for
[#358](https://github.com/byvfx/floki/issues/358): *what does running a small language model
on CPU actually cost?* It loads a quantized Qwen3-0.6B through `candle`, prefills a prompt,
decodes some tokens, and prints timings. That is all it does.

It is **deliberately not a workspace member.** Its own `Cargo.toml` carries an empty
`[workspace]` table, so it is a separate workspace root and `cargo build` at the repo root
never sees it. Nothing here touches floki's `Cargo.lock`, and `candle` is not a floki
dependency. `Cargo.lock` is committed so the measurements below stay reproducible.

## Running it

From this directory:

```bash
cargo run --release --locked -- <model.gguf> <tokenizer.json> [prompt_tokens] [n_decode]
```

`cargo run` rather than `cargo build` is deliberate: building leaves the executable in the
target directory without putting it on `PATH`, and that directory is not fixed — a
`build.target-dir` in a developer's own `~/.cargo/config.toml` (a per-user setting, not
anything this repo provides) relocates it. `cargo run` finds it either way, and `--locked`
holds the committed pins.

Weights are not included. The 378 MiB Q4_K_M below is third-party `unsloth/`; Qwen publishes
only a 609 MiB Q8_0 at this size. Fetch them into a directory of their own:

```bash
curl -L -o Qwen3-0.6B-Q4_K_M.gguf https://huggingface.co/unsloth/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q4_K_M.gguf
curl -L -o tokenizer.json https://huggingface.co/Qwen/Qwen3-0.6B/resolve/main/tokenizer.json
```

## What it measured (Windows, CPU, Q4_K_M, 2026-11-08)

| prompt tokens | prefill tok/s | prefill time | decode tok/s |
| --- | --- | --- | --- |
| 95 | 197 | **0.48 s** | 75.0 |
| 277 | 259 | **1.07 s** | 66.2 |
| 1057 | 222 | **4.75 s** | 48.6 |
| 2071 | 162 | **12.76 s** | 34.3 |

The third column is prefill alone. Time to first token adds one argmax over the 151,936-entry
vocabulary, which is sub-millisecond; the binary now reports prefill and that first sample
separately rather than labelling prefill as though it already included it.

Peak memory, sampled live from outside the process:

| prompt tokens | peak working set | peak commit |
| --- | --- | --- |
| ~95 | **1,431 MiB** | 2,302 MiB |
| ~2071 | **2,472 MiB** | 2,927 MiB |

Three conclusions, all recorded in #358:

1. **Prefill is the binding constraint**, and its throughput *degrades* with length rather
   than amortising. A few hundred prompt tokens is a hard budget, so retrieval exists to keep
   the prompt small, not merely relevant.
2. **~940 MB of the footprint is eager KV preallocation** — on CPU each layer builds
   `RawInterleavedKvCache::new(h_kv, d, 4096)`, i.e. `vec![0f32; 4096 * h_kv * 2 * d]`,
   33.5 MiB per layer in f32 × 28 layers, regardless of context actually used. Capping that
   4096 would return most of it.
3. **The pure-Rust CPU path is real**: 84 s build across 118 crates with bare `cargo`, no
   vcvars and no cmake. `tokenizers` must carry `default-features = false`, otherwise
   `esaxx_fast` pulls a C++ compile.

## Not measured

Q8_0 (estimated ~1.66 GB floor by the same arithmetic), Metal, and anything with `mkl` or
`accelerate` enabled — all of which would move the prefill number, which is the one that
matters.
