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
cargo run --release --locked -- <model.gguf> <tokenizer.json> [options]

  --prompt <text>   ask this, instead of running the synthetic benchmark
  --ground <file>   prepend this file as context, instructing the model to use only it
  --tokens <n>      synthetic prompt of about n tokens (benchmark mode, default 256)
  --decode <n>      maximum tokens to generate (default 64)
  --think           let Qwen3 open a <think> block (suppressed by default)
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

## What the grounded Q&A showed

Two trials with `--ground` pointed at the README's 19-line **Comparison** section — 550 prompt
tokens, ~1.9 s to first token:

**In-context question — usable.** *"How do I compare the current layer against a different
version?"* produced a correct answer, naming the Compare picker in the toolbar and that it
compares the current layer against another layer in the stack. It did conflate the mode picker
with the `vs:` layer picker, and padded with "a comprehensive comparison of their properties",
but nothing was invented. The premise behind #358's retrieval plan holds: the grounding does
the work, not the model.

**Out-of-context question — fabricated.** *"What keyboard shortcut starts and stops
playback?"* is absent from that section, and the system prompt instructed it to say it did not
know when the context lacks the answer. It answered **"`Ctrl + P` ... and `Ctrl + Shift + P`"**.
The real answer is `Space` (`src/app.rs`). That is not merely wrong, it is *plausible* — and
`Ctrl+P` is what #357 proposes for the command palette, so a TD would try it, get nothing, and
stop trusting the panel.

The design consequence, and the main thing this spike bought: **the model cannot be trusted to
decide whether it knows.** #358's acceptance criterion "an unanswerable question returns I
don't know" has to be a **code path, not a prompt instruction**. If retrieval finds no registry
entry or doc chunk above a threshold, the panel must refuse and show the registry rows without
consulting the model at all. One trial each, so treat the magnitude as indicative — but the
direction is not in doubt, since a model this size follows negative instructions poorly by
nature.

## Not measured

Q8_0 (estimated ~1.66 GB floor by the same arithmetic), Metal, and anything with `mkl` or
`accelerate` enabled — all of which would move the prefill number, the one that matters.

Also untested: whether a sterner refusal instruction, few-shot examples, or a logprob
threshold would make refusal reliable enough to lean on. The architectural fix above does not
depend on the answer.
