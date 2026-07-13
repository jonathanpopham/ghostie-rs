#![forbid(unsafe_code)]
//! Thin binary shim: collect args, call the library, exit with its code.
//! All logic (and all testing) lives in the `ghostie` library crate.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(ghostie::cli::run(&args));
}
