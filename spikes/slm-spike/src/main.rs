// Throwaway spike for floki #358: load a quantized Qwen3, measure prefill and decode.
// Deliberately outside the floki workspace so nothing touches its lock file.
use anyhow::{Context, Result};
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen3::ModelWeights as Qwen3;
use std::time::Instant;
use tokenizers::Tokenizer;

const MIB: f64 = 1024.0 * 1024.0;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        anyhow::bail!("usage: slm-spike <model.gguf> <tokenizer.json> [prompt_tokens] [n_decode]");
    }
    let (model_path, tok_path) = (&a[1], &a[2]);
    let want_prompt: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(256);
    let n_decode: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(64);

    let device = Device::Cpu;

    // ---- load ----
    let t0 = Instant::now();
    let mut file = std::fs::File::open(model_path).with_context(|| format!("open {model_path}"))?;
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

    // ---- tokenizer + a prompt of roughly the requested length ----
    let tokenizer = Tokenizer::from_file(tok_path).map_err(anyhow::Error::msg)?;
    let chunk = "The byte-budgeted ring keeps decoded frames resident in RAM within a live memory budget, so scrub-backs and loops stay instant. ";
    let mut body = String::new();
    while tokenizer
        .encode(body.as_str(), false)
        .map_err(anyhow::Error::msg)?
        .get_ids()
        .len()
        < want_prompt
    {
        body.push_str(chunk);
    }
    let prompt =
        format!("<|im_start|>user\n{body}\nSummarise that in one sentence.<|im_end|>\n<|im_start|>assistant\n");
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
    let mut next = lp.sample(&logits)?;

    // ---- decode ----
    let t2 = Instant::now();
    let mut out = vec![next];
    for i in 0..n_decode {
        let input = Tensor::new(&[next], &device)?.unsqueeze(0)?;
        let logits = model.forward(&input, tokens.len() + i)?.squeeze(0)?;
        next = lp.sample(&logits)?;
        out.push(next);
    }
    let dec = t2.elapsed().as_secs_f64();
    println!(
        "decode      : {:.3} s for {} tok   => {:.1} tok/s",
        dec,
        n_decode,
        n_decode as f64 / dec
    );
    println!("first-token : {:.3} s  (prefill + 1 sample)", prefill);
    println!(
        "text        : {}",
        tokenizer
            .decode(&out, true)
            .unwrap_or_default()
            .replace('\n', " ")
    );
    Ok(())
}
