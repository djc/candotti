use std::error::Error;
use std::path::PathBuf;
use std::{env, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    // put memory layout (linker script) in the linker search path
    let memory = match env::var_os("CARGO_FEATURE_S140") {
        Some(_) => "memory-s140.x",
        None => "memory.x",
    };
    fs::copy(memory, out_dir.join("memory.x"))?;

    println!("cargo:rustc-link-search={}", out_dir.display());

    Ok(())
}
