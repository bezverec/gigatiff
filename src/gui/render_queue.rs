use std::path::PathBuf;

use anyhow::Result;

use crate::core::render::{PreviewBitmap, Rect};
use crate::core::tiff_info::ImageInfo;
use crate::options::Backend;

use super::cache::OverviewCacheKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PreviewRequest {
    pub(crate) path: PathBuf,
    pub(crate) rect: Rect,
    pub(crate) max_output: u32,
    pub(crate) backend: Backend,
}

#[derive(Debug)]
pub(crate) struct RenderJob {
    pub(crate) request: PreviewRequest,
    pub(crate) info: ImageInfo,
    pub(crate) max_chunk_mb: usize,
    pub(crate) generation: u64,
    pub(crate) kind: RenderJobKind,
}

#[derive(Debug)]
pub(crate) struct RenderResult {
    pub(crate) request: PreviewRequest,
    pub(crate) generation: u64,
    pub(crate) kind: RenderJobKind,
    pub(crate) result: Result<PreviewBitmap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderJobKind {
    Tile,
    Overview(OverviewCacheKey),
}
