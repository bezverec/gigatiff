use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, ensure};
use clap::Parser;
use gigatiff_core::{grok_ffi, render::Rect};

/// Measure fresh Grok decoders without HTTP, resampling or output encoding.
#[derive(Parser)]
struct Args {
    file: PathBuf,
    #[arg(long, default_value_t = 0)]
    x: u32,
    #[arg(long, default_value_t = 0)]
    y: u32,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
    #[arg(long)]
    out_width: u32,
    #[arg(long)]
    out_height: u32,
    #[arg(long, default_value_t = 5)]
    iterations: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.width > 0 && args.height > 0 && args.out_width > 0 && args.out_height > 0,
        "source and output dimensions must be positive"
    );
    ensure!(args.iterations > 0, "iterations must be positive");
    let rect = Rect {
        x: args.x,
        y: args.y,
        width: args.width,
        height: args.height,
    };

    // Exclude process-global initialization and the first file read from samples.
    black_box(grok_ffi::render_region(
        &args.file,
        rect,
        args.out_width,
        args.out_height,
    )?);
    println!("iteration,width,height,decode_ms,convert_ms,wall_ms");
    for iteration in 0..args.iterations {
        let start = Instant::now();
        let bitmap = grok_ffi::render_region(&args.file, rect, args.out_width, args.out_height)?;
        let elapsed = start.elapsed();
        println!(
            "{iteration},{},{},{:.6},{:.6},{:.6}",
            bitmap.width,
            bitmap.height,
            bitmap.decode.as_secs_f64() * 1000.0,
            bitmap.convert.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1000.0
        );
        black_box(bitmap);
    }
    Ok(())
}
