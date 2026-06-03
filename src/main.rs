mod cache;
mod cli;
mod color;
mod gui;
mod render;
mod tiff_info;

fn main() -> anyhow::Result<()> {
    cli::run()
}
