use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;

#[path = "../tools.rs"]
mod tools;

fn main() {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let input = match args.next() {
        Some(v) => PathBuf::from(v),
        None => {
            eprintln!("Usage: cargo run --bin convert_dir -- <input_dir> [output_dir]");
            std::process::exit(2);
        }
    };

    let output = match args.next() {
        Some(v) => PathBuf::from(v),
        None => input.join("converted"),
    };

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));

    tools::run_conversion_task(input, output, tx, cancel);

    for (done, total, msg) in rx {
        println!("[{done}/{total}] {msg}");
    }
}