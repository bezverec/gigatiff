use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use crate::gui::run_gui;
use crate::render::{clamp_rect, ms, render_preview, save_png};
use crate::tiff_info::{load_info, print_info};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "GigaTIFF: a memory-conscious TIFF/BigTIFF viewer"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// TIFF/BigTIFF opened directly in the GUI when no subcommand is used.
    path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print metadata without decoding the full image.
    Info { path: PathBuf },
    /// Open a small desktop viewer.
    Gui { path: Option<PathBuf> },
    /// Decode only the chunks intersecting a source rectangle and save a PNG preview.
    Preview {
        path: PathBuf,

        #[arg(long, default_value_t = 0)]
        x: u32,

        #[arg(long, default_value_t = 0)]
        y: u32,

        #[arg(long, default_value_t = 1024)]
        width: u32,

        #[arg(long, default_value_t = 1024)]
        height: u32,

        #[arg(long, default_value_t = 1024)]
        max_output: u32,

        #[arg(long, default_value = "preview.png")]
        out: PathBuf,

        /// Maximum decoded chunk allocation in MiB.
        #[arg(long, default_value_t = 256)]
        max_chunk_mb: usize,

        /// Pixel backend used for rendering.
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,

        /// PNG compression used for preview export.
        #[arg(long, value_enum, default_value_t = PngCompression::Fast)]
        png_compression: PngCompression,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub(crate) enum Backend {
    Auto,
    Libtiff,
    Rust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum PngCompression {
    None,
    Fastest,
    Fast,
    Balanced,
    High,
}

impl PngCompression {
    pub(crate) fn to_png(self) -> png::Compression {
        match self {
            Self::None => png::Compression::NoCompression,
            Self::Fastest => png::Compression::Fastest,
            Self::Fast => png::Compression::Fast,
            Self::Balanced => png::Compression::Balanced,
            Self::High => png::Compression::High,
        }
    }
}

pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Info { path }) => print_info(&path),
        Some(Command::Gui { path }) => run_gui(path),
        Some(Command::Preview {
            path,
            x,
            y,
            width,
            height,
            max_output,
            out,
            max_chunk_mb,
            backend,
            png_compression,
        }) => write_preview(
            &path,
            x,
            y,
            width,
            height,
            max_output,
            &out,
            max_chunk_mb,
            backend,
            png_compression,
        ),
        None => run_gui(cli.path),
    }
}

fn write_preview(
    path: &Path,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    max_output: u32,
    out: &Path,
    max_chunk_mb: usize,
    backend: Backend,
    png_compression: PngCompression,
) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("preview width and height must be greater than zero");
    }
    if max_output == 0 {
        bail!("max-output must be greater than zero");
    }

    let info = load_info(path)?;
    let rect = clamp_rect(x, y, width, height, info.width, info.height)?;
    let preview = render_preview(
        path,
        &info,
        rect,
        max_output,
        max_chunk_mb,
        backend,
        None,
        None,
    )?;

    let save_start = Instant::now();
    save_png(
        out,
        preview.width,
        preview.height,
        &preview.rgba,
        png_compression,
    )?;
    let save_time = save_start.elapsed();
    println!(
        "wrote {} ({} x {}, {}, decoded {} chunk(s), {}, png {:?} {:.1} ms)",
        out.display(),
        preview.width,
        preview.height,
        preview.source,
        preview.decoded_chunks,
        preview.stats.short_label(),
        png_compression,
        ms(save_time)
    );
    Ok(())
}
