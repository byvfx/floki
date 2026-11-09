// Throwaway spike for floki #358: load a quantized Qwen3 on CPU and either run a synthetic
// prefill/decode benchmark, or ask it a real question with floki's own docs as context.
// Deliberately outside the floki workspace so nothing touches its lock file.
use anyhow::{Context, Result};
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen3::ModelWeights as Qwen3;
use std::time::Instant;
use tokenizers::Tokenizer;

const MIB: f64 = 1024.0 * 1024.0;

/// Filler for benchmark mode. Content is irrelevant; only its token count matters.
const FILLER: &str = "The byte-budgeted ring keeps decoded frames resident in RAM within a live memory budget, so scrub-backs and loops stay instant. ";

const GROUNDING: &str = "You answer questions about floki, a GPU-accelerated viewer for \
multi-layer OpenEXR files. Use only the context below. If it does not contain the answer, \
say you do not know.";

const USAGE: &str = "\
usage: slm-spike <model.gguf> <tokenizer.json> [options]

  --prompt <text>   ask this, instead of running the synthetic benchmark
  --ground <file>   prepend this file as context, instructing the model to use only it
  --tokens <n>      synthetic prompt of about n tokens (benchmark mode, default 256)
  --decode <n>      maximum tokens to generate (default 64)
  --think           let Qwen3 open a <think> block (suppressed by default)
";

struct Args {
    model: String,
    tokenizer: String,
    prompt: Option<String>,
    ground: Option<String>,
    tokens: usize,
    decode: usize,
    think: bool,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let model = it.next().with_context(|| USAGE)?;
    let tokenizer = it.next().with_context(|| USAGE)?;
    let mut args = Args {
        model,
        tokenizer,
        prompt: None,
        ground: None,
        tokens: 256,
        decode: 64,
        think: false,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--prompt" => args.prompt = Some(it.next().context("--prompt needs text")?),
            "--ground" => args.ground = Some(it.next().context("--ground needs a path")?),
            "--tokens" => args.tokens = it.next().context("--tokens needs a number")?.parse()?,
            "--decode" => args.decode = it.next().context("--decode needs a number")?.parse()?,
            "--think" => args.think = true,
            other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(args)
}

/// Build a Qwen3 chat prompt. Without `--think` this emits the already-closed think block
/// that the official template produces for `enable_thinking=false`, so the model cannot open
/// one — a soft `/no_think` request is only advisory and a 0.6B does not reliably obey it.
fn build_prompt(args: &Args, tok: &Tokenizer) -> Result<String> {
    let user = match &args.prompt {
        Some(p) => p.clone(),
        None => {
            let mut body = String::new();
            while tok
                .encode(body.as_str(), false)
                .map_err(anyhow::Error::msg)?
                .get_ids()
                .len()
                < args.tokens
            {
                body.push_str(FILLER);
            }
            format!("{body}\nSummarise that in one sentence.")
        }
    };

    let mut s = String::new();
    if let Some(path) = &args.ground {
        let ctx = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        s.push_str("<|im_start|>system\n");
        s.push_str(GROUNDING);
        s.push_str("\n\n");
        s.push_str(ctx.trim());
        s.push_str("<|im_end|>\n");
    }
    s.push_str("<|im_start|>user\n");
    s.push_str(&user);
    s.push_str("<|im_end|>\n<|im_start|>assistant\n");
    if !args.think {
        s.push_str("<think>\n\n</think>\n\n");
    }
    Ok(s)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let device = Device::Cpu;

    // ---- load ----
    let t0 = Instant::now();
    let mut file =
        std::fs::File::open(&args.model).with_context(|| format!("open {}", args.model))?;
    let content = gguf_file::Content::read(&mut file).map_err(anyhow::Error::msg)?;
    let n_tensors = content.tensor_infos.len();
    let weight_bytes: usize = content
        .tensor_infos
        .values()
        .map(|t| t.shape.elem_count() * t.ggml_dtype.type_size() / t.ggml_dtype.block_size())
        .sum();
    let mut model = Qwen3::from_gguf(content, &mut file, &device)?;
    println!(
        "load        : {:.2} s   {} tensors   weights {:.1} MiB",
        t0.elapsed().as_secs_f64(),
        n_tensors,
        weight_bytes as f64 / MIB
    );

    let tokenizer = Tokenizer::from_file(&args.tokenizer).map_err(anyhow::Error::msg)?;
    let eos = tokenizer.get_vocab(true).get("<|im_end|>").copied();
    let prompt = build_prompt(&args, &tokenizer)?;
    let tokens: Vec<u32> = tokenizer
        .encode(prompt.as_str(), true)
        .map_err(anyhow::Error::msg)?
        .get_ids()
        .to_vec();

    // ---- prefill (whole prompt in one forward) ----
    let t1 = Instant::now();
    let input = Tensor::new(tokens.as_slice(), &device)?.unsqueeze(0)?;
    let logits = model.forward(&input, 0)?.squeeze(0)?;
    let prefill = t1.elapsed().as_secs_f64();
    println!(
        "prefill     : {:.3} s for {} tok   => {:.0} tok/s",
        prefill,
        tokens.len(),
        tokens.len() as f64 / prefill
    );

    let mut lp = LogitsProcessor::from_sampling(42, Sampling::ArgMax);
    // Timed on its own so `first-token` below is prefill + this sample rather than prefill
    // reported twice, and so the prefill println above is excluded from it.
    let t_sample = Instant::now();
    let mut next = lp.sample(&logits)?;
    let first_sample = t_sample.elapsed().as_secs_f64();

    // ---- decode ----
    let t2 = Instant::now();
    let mut out = vec![next];
    let mut generated = 1usize;
    for i in 0..args.decode {
        if Some(next) == eos {
            break;
        }
        let input = Tensor::new(&[next], &device)?.unsqueeze(0)?;
        let logits = model.forward(&input, tokens.len() + i)?.squeeze(0)?;
        next = lp.sample(&logits)?;
        out.push(next);
        generated += 1;
    }
    let dec = t2.elapsed().as_secs_f64();
    println!(
        "decode      : {:.3} s for {} tok   => {:.1} tok/s",
        dec,
        generated,
        generated as f64 / dec
    );
    println!(
        "first-token : {:.3} s  (prefill {:.3} + first sample {:.4})",
        prefill + first_sample,
        prefill,
        first_sample
    );
    println!(
        "answer      : {}",
        tokenizer.decode(&out, true).unwrap_or_default().trim()
    );
    Ok(())
}
