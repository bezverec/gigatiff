use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum Backend {
    Auto,
    Libtiff,
    Rust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PngCompression {
    None,
    Fastest,
    Fast,
    Balanced,
    High,
}

impl PngCompression {
    pub fn to_png(self) -> png::Compression {
        match self {
            Self::None => png::Compression::NoCompression,
            Self::Fastest => png::Compression::Fastest,
            Self::Fast => png::Compression::Fast,
            Self::Balanced => png::Compression::Balanced,
            Self::High => png::Compression::High,
        }
    }
}
