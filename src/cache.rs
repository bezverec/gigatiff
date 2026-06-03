use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::render::{PreviewRequest, RenderStats};

pub(crate) struct TileTexture {
    pub(crate) texture: egui::TextureHandle,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source: &'static str,
    pub(crate) stats: RenderStats,
    pub(crate) bytes: usize,
}

pub(crate) struct TileTextureCache {
    pub(crate) entries: VecDeque<(PreviewRequest, Arc<TileTexture>)>,
    pub(crate) byte_limit: usize,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanlineKey {
    pub(crate) path: PathBuf,
    pub(crate) y: u32,
    pub(crate) x: u32,
    pub(crate) width: u32,
    pub(crate) bytes_per_pixel: usize,
}

pub(crate) struct ScanlineCache {
    pub(crate) entries: VecDeque<(ScanlineKey, Arc<Vec<u8>>)>,
    pub(crate) byte_limit: usize,
    pub(crate) bytes: usize,
}

impl TileTextureCache {
    pub(crate) fn new(byte_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            byte_limit,
            bytes: 0,
        }
    }

    pub(crate) fn get(&mut self, request: &PreviewRequest) -> Option<Arc<TileTexture>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_request, _)| cached_request == request)?;
        let entry = self.entries.remove(index)?;
        let tile = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(tile)
    }

    pub(crate) fn contains(&self, request: &PreviewRequest) -> bool {
        self.entries
            .iter()
            .any(|(cached_request, _)| cached_request == request)
    }

    pub(crate) fn insert(&mut self, request: PreviewRequest, tile: Arc<TileTexture>) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_request, _)| cached_request == &request)
        {
            if let Some((_, old)) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }

        self.bytes += tile.bytes;
        self.entries.push_back((request, tile));

        while self.bytes > self.byte_limit && self.entries.len() > 1 {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl ScanlineCache {
    pub(crate) fn new(byte_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            byte_limit,
            bytes: 0,
        }
    }

    pub(crate) fn get(&mut self, key: &ScanlineKey) -> Option<Arc<Vec<u8>>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == key)?;
        let entry = self.entries.remove(index)?;
        let row = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(row)
    }

    pub(crate) fn insert(&mut self, key: ScanlineKey, row: Arc<Vec<u8>>) {
        if row.len() > self.byte_limit {
            return;
        }

        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == &key)
        {
            if let Some((_, old)) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }

        self.bytes += row.len();
        self.entries.push_back((key, row));

        while self.bytes > self.byte_limit {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }
}
