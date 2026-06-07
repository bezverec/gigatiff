use std::collections::VecDeque;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use eframe::egui;

use crate::core::render::RenderStats;
use crate::core::tiff_info::ImageInfo;

use super::render_queue::PreviewRequest;

const OVERVIEW_MAGIC: &[u8; 8] = b"GTOV0001";
const OVERVIEW_CACHE_LIMIT: u64 = 256 * 1024 * 1024;
const OVERVIEW_MAX_BYTES: u64 = 64 * 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OverviewCacheKey {
    filename: String,
}

pub(crate) struct CachedOverview {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

pub(crate) struct PersistentOverviewCache {
    dir: PathBuf,
    byte_limit: u64,
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

impl PersistentOverviewCache {
    pub(crate) fn new() -> Option<Self> {
        overview_cache_dir().map(|dir| Self {
            dir,
            byte_limit: OVERVIEW_CACHE_LIMIT,
        })
    }

    pub(crate) fn key_for(path: &Path, info: &ImageInfo) -> Option<OverviewCacheKey> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let path_text = platform_cache_path(&canonical);

        let mut hash = Fnv1a64::new();
        hash.write_bytes(path_text.as_bytes());
        hash.write_u64(metadata.len());
        hash.write_u64(modified.as_secs());
        hash.write_u64(modified.subsec_nanos() as u64);
        hash.write_u64(info.width as u64);
        hash.write_u64(info.height as u64);
        hash.write_u64(info.chunk_width as u64);
        hash.write_u64(info.chunk_height as u64);
        hash.write_u64(info.bits_per_sample.as_ref().map_or(0, |bits| bits.len()) as u64);
        if let Some(bits) = &info.bits_per_sample {
            for bit in bits {
                hash.write_u64(*bit as u64);
            }
        }
        hash.write_u64(info.samples_per_pixel.unwrap_or_default() as u64);
        hash.write_u64(info.photometric.unwrap_or_default() as u64);
        hash.write_u64(info.icc_profile.as_ref().map_or(0, |profile| profile.len()) as u64);

        Some(OverviewCacheKey {
            filename: format!("{:016x}.gtov", hash.finish()),
        })
    }

    pub(crate) fn load(&self, key: &OverviewCacheKey) -> Result<Option<CachedOverview>> {
        let path = self.path_for(key);
        if !path.exists() {
            return Ok(None);
        }

        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mut cursor = Cursor::new(bytes.as_slice());
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != OVERVIEW_MAGIC {
            return Ok(None);
        }

        let width = read_u32(&mut cursor)?;
        let height = read_u32(&mut cursor)?;
        let len = read_u64(&mut cursor)?;
        if width == 0 || height == 0 || len > OVERVIEW_MAX_BYTES {
            return Ok(None);
        }
        if len != width as u64 * height as u64 * 4 {
            return Ok(None);
        }

        let mut rgba = vec![0u8; len as usize];
        cursor.read_exact(&mut rgba)?;
        Ok(Some(CachedOverview {
            width,
            height,
            rgba,
        }))
    }

    pub(crate) fn store(
        &self,
        key: &OverviewCacheKey,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<()> {
        if width == 0 || height == 0 || rgba.len() as u64 > OVERVIEW_MAX_BYTES {
            return Ok(());
        }
        if rgba.len() != width as usize * height as usize * 4 {
            return Ok(());
        }

        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(key);
        let mut bytes = Vec::with_capacity(8 + 4 + 4 + 8 + rgba.len());
        bytes.extend_from_slice(OVERVIEW_MAGIC);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&(rgba.len() as u64).to_le_bytes());
        bytes.extend_from_slice(rgba);
        fs::write(path, bytes)?;
        self.prune()?;
        Ok(())
    }

    fn path_for(&self, key: &OverviewCacheKey) -> PathBuf {
        self.dir.join(&key.filename)
    }

    fn prune(&self) -> Result<()> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Ok(());
        };
        let mut files = Vec::new();
        let mut total = 0u64;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("gtov") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let len = metadata.len();
            total = total.saturating_add(len);
            files.push((metadata.modified().unwrap_or(UNIX_EPOCH), len, path));
        }

        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, len, path) in files {
            if total <= self.byte_limit {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }

        Ok(())
    }
}

struct Fnv1a64 {
    value: u64,
}

impl Fnv1a64 {
    fn new() -> Self {
        Self {
            value: 0xcbf29ce484222325,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.value ^= *byte as u64;
            self.value = self.value.wrapping_mul(0x100000001b3);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.value
    }
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut bytes = [0u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn overview_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
            .map(|dir| dir.join("GigaTIFF").join("overview-cache"));
    }

    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(PathBuf::from).map(|dir| {
            dir.join("Library")
                .join("Caches")
                .join("GigaTIFF")
                .join("overview-cache")
        });
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(cache_home) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
            return Some(cache_home.join("gigatiff").join("overview-cache"));
        }
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|dir| dir.join(".cache").join("gigatiff").join("overview-cache"));
    }

    #[allow(unreachable_code)]
    None
}

fn platform_cache_path(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        text.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_overview_cache_round_trips_rgba() {
        let dir = std::env::temp_dir().join(format!(
            "gigatiff-overview-cache-test-{}",
            std::process::id()
        ));
        let cache = PersistentOverviewCache {
            dir,
            byte_limit: 1024 * 1024,
        };
        let key = OverviewCacheKey {
            filename: "roundtrip.gtov".to_string(),
        };
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];

        cache.store(&key, 2, 2, &rgba).expect("store overview");
        let loaded = cache
            .load(&key)
            .expect("load overview")
            .expect("overview exists");

        assert_eq!(loaded.width, 2);
        assert_eq!(loaded.height, 2);
        assert_eq!(loaded.rgba, rgba);
    }
}
